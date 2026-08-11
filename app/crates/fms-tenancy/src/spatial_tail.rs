//! Spatial & BIM 補完的六支端點。
//!
//! # `bulk-import` 的真正難處是「父節點用 code 指定」
//!
//! 一份樓層平面的匯入檔長這樣：
//!
//! ```json
//! [{"code":"FL03","parent_code":"BLDG_A","node_type_code":"FLOOR", ...},
//!  {"code":"BLDG_A","node_type_code":"BUILDING", ...}]
//! ```
//!
//! 注意 `FL03` 引用了**還沒出現**的 `BLDG_A`。要求呼叫者自己做拓撲排序是把
//! 問題丟回去 —— 匯入檔通常是從別的系統匯出的，順序不由他決定。
//!
//! 所以這裡**多趟解析**：每一趟建立所有父節點已知的列，直到沒有進展為止。
//! 剩下的就是真的解不開（父節點不存在，或形成循環），逐筆回報原因。
//!
//! 那也讓 069 的循環守衛在這裡有第二個用處：一份含循環的匯入檔會停在
//! 「沒有進展」，而不是寫出一棵壞掉的樹。
//!
//! # `mappings` 是三個欄位的第一個寫入者
//!
//! `bim_models.mapped_node_count`、`mapped_asset_count`、`unresolved_elements`
//! 到目前為止**只有讀者**（量過）。這支端點寫它們：
//!
//!   * 把 `bim_element_id` 寫到目標的節點或設備上
//!   * 從 `unresolved_elements` 移除那個元件
//!   * 重算兩個計數
//!
//! 三件事必須一起做。少了第二件，同一個元件會永遠留在「待補正」清單裡；
//! 少了第三件，計數與實際對應數分歧，而畫面上顯示的「已對應 12 個」是假的。

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_reservation::repo::occupancy as reservation_occupancy;
use fms_shared::{begin_tenant_tx, require_permission, Caller, FieldError, Problem};

use crate::handlers::TenancyState;

// =============================================================================
// GET /spatial-node-types
// =============================================================================

/// `GET /spatial-node-types`
///
/// 需要 `spatial_node:read`（不指定場域 —— 型別目錄是租戶層級的）。
///
/// 平台預設（`tenant_id IS NULL`）與租戶自訂一起回，並標上 `is_platform`。
/// `spatial_nodes.node_type_code` **沒有外鍵**，所以這支端點是客戶端唯一能
/// 知道「哪些值是合法的」的地方 —— 少了它，前端只能硬編一份會過期的清單。
pub async fn list_node_types(
    State(state): State<TenancyState>,
    caller: Caller,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "spatial_node:read", None, None).await?;

    let rows = sqlx::query!(
        r#"SELECT id, tenant_id IS NULL AS "is_platform!",
                  code::text AS "code!", name::text AS "name!",
                  level_hint, is_bookable, is_leaf_default,
                  allowed_child_codes AS "allowed_child_codes!",
                  icon::text AS icon
             FROM fms.spatial_node_types
            ORDER BY level_hint, code"#
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "code": r.code,
                "name": r.name,
                // 這一層在樹裡的建議深度（SITE=0、BUILDING=1、…）。
                "level_hint": r.level_hint,
                "is_bookable": r.is_bookable,
                "is_leaf_default": r.is_leaf_default,
                // **允許的子型別。** 這一欄目前沒有任何強制執行者 ——
                // `POST /facilities/{id}/spatial-nodes` 不檢查它，
                // 所以把 ROOM 掛在 DESK 底下是寫得進去的。原樣回傳讓前端
                // 可以自己擋，但那不是後端的保證。
                "allowed_child_codes": r.allowed_child_codes,
                "icon": r.icon,
                // 平台預設不可改；租戶自訂的才是這個客戶加的。
                "is_platform": r.is_platform,
            })
        })
        .collect();
    let tenant_own = rows.iter().filter(|r| !r.is_platform).count();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "count": rows.len(),
            "tenant_defined": tenant_own,
            // `spatial_nodes.node_type_code` 沒有外鍵，所以這份清單是客戶端
            // 唯一的合法值來源 —— 打錯字只有應用層擋得住。
            "no_foreign_key_on_node_type_code": true,
            // `allowed_child_codes` 沒有執行者：後端不會擋「ROOM 掛在 DESK
            // 底下」。前端可以照它擋，但不要當成後端的保證。
            "allowed_child_codes_is_advisory_only": true,
        },
    })))
}

// =============================================================================
// POST /facilities/{facilityId}/spatial-nodes:bulk-import
// =============================================================================

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkNodeRow {
    pub code: String,
    pub name: String,
    pub node_type_code: String,
    /// 父節點的 **code**（不是 uuid）—— 匯入檔是從別的系統匯出的，
    /// 裡面不會有我們的 uuid。`null`／省略 = 這個場域的根節點。
    #[serde(default)]
    pub parent_code: Option<String>,
    #[serde(default)]
    pub floor_level: Option<i32>,
    #[serde(default)]
    pub floor_label: Option<String>,
    #[serde(default)]
    pub area_sqm: Option<f64>,
    #[serde(default)]
    pub capacity: Option<i32>,
    #[serde(default)]
    pub is_bookable: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkNodeImportRequest {
    pub rows: Vec<BulkNodeRow>,
    /// 預設 **true** —— 與 `assets:bulk-import` 同一個判斷：匯入是破壞性的，
    /// 而預設不寫入讓呼叫者可以先看結果。要真的寫入必須明確送 `false`。
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Serialize)]
struct BulkNodeOutcome {
    code: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
}

const BULK_NODE_MAX: usize = 1000;

/// `POST /facilities/{facilityId}/spatial-nodes:bulk-import`
///
/// 多趟解析，見模組檔頭。回傳逐筆結果 + 四個計數。
pub async fn bulk_import_nodes(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Json(req): Json<BulkNodeImportRequest>,
) -> Result<Json<serde_json::Value>, Problem> {
    let dry_run = req.dry_run.unwrap_or(true);
    if req.rows.is_empty() {
        return Err(Problem::validation("`rows` 不得為空"));
    }
    if req.rows.len() > BULK_NODE_MAX {
        return Err(Problem::validation(format!("一次最多 {BULK_NODE_MAX} 列")));
    }
    // 檔案內部的 code 重複：兩列同 code 的話第二列一定撞唯一索引，而那個失敗
    // 看起來像「這個 code 已經存在於資料庫」。先在檔案層面擋掉，訊息才準確。
    {
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = req
            .rows
            .iter()
            .find(|r| !seen.insert(r.code.to_lowercase()))
        {
            return Err(
                Problem::validation("`rows` 內部有重複的 `code`").with_errors(vec![FieldError {
                    pointer: "/rows".to_string(),
                    code: "DUPLICATE_IN_FILE".to_string(),
                    message: format!(
                        "`{}` 出現多次 —— 那與「資料庫裡已經有這個 code」是不同的錯誤",
                        dup.code
                    ),
                }]),
            );
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "spatial_node:write", Some(facility_id), None).await?;

    // 型別碼先全部驗一遍（沒有外鍵，見 `list_node_types`）。整批驗而不是逐列，
    // 因為一份匯入檔裡打錯的型別通常是同一個 —— 一次講完比讓他來 20 次好。
    let mut bad_types: Vec<String> = Vec::new();
    for t in req
        .rows
        .iter()
        .map(|r| r.node_type_code.clone())
        .collect::<std::collections::BTreeSet<_>>()
    {
        if !crate::repo::node_type_exists(&mut tx, &t).await? {
            bad_types.push(t);
        }
    }
    if !bad_types.is_empty() {
        return Err(
            Problem::validation("有不存在的 `node_type_code`").with_errors(
                bad_types
                    .iter()
                    .map(|t| FieldError {
                        pointer: "/rows/*/node_type_code".to_string(),
                        code: "NOT_FOUND".to_string(),
                        message: format!("`{t}` 不在 GET /spatial-node-types 裡"),
                    })
                    .collect(),
            ),
        );
    }

    // 這個場域既有的 code → id，讓匯入檔可以掛在既有的樹上。
    let existing = sqlx::query!(
        r#"SELECT code::text AS "code!", id FROM fms.spatial_nodes
            WHERE facility_id = $1 AND deleted_at IS NULL"#,
        facility_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    let mut known: std::collections::HashMap<String, Uuid> = existing
        .into_iter()
        .map(|r| (r.code.to_lowercase(), r.id))
        .collect();

    let mut outcomes: Vec<BulkNodeOutcome> = Vec::with_capacity(req.rows.len());
    let mut pending: Vec<&BulkNodeRow> = req.rows.iter().collect();
    let mut created = 0usize;

    // **多趟。** 每一趟建立所有父節點已知的列；沒有進展就停。
    loop {
        let before = pending.len();
        let mut still: Vec<&BulkNodeRow> = Vec::new();

        for row in pending.into_iter() {
            let parent_id = match &row.parent_code {
                None => None,
                Some(pc) => match known.get(&pc.to_lowercase()) {
                    Some(id) => Some(*id),
                    // 父節點還不知道 —— 可能在後面幾列，下一趟再試。
                    None => {
                        still.push(row);
                        continue;
                    }
                },
            };

            sqlx::query("SAVEPOINT bulk_node")
                .execute(tx.conn())
                .await
                .map_err(Problem::from)?;

            let inserted = sqlx::query!(
                r#"INSERT INTO fms.spatial_nodes
                     (tenant_id, facility_id, parent_id, node_type_code, code, name,
                      floor_level, floor_label, area_sqm, capacity, is_bookable)
                   VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5,
                           $6, $7, $8::float8::numeric, coalesce($9, 0),
                           coalesce($10, false))
                   RETURNING id, node_path::text AS "node_path!""#,
                facility_id,
                parent_id,
                row.node_type_code,
                row.code,
                row.name,
                row.floor_level,
                row.floor_label,
                row.area_sqm,
                row.capacity,
                row.is_bookable,
            )
            .fetch_one(tx.conn())
            .await;

            match inserted {
                Ok(r) => {
                    known.insert(row.code.to_lowercase(), r.id);
                    created += 1;
                    // dry-run 也要真的 INSERT 一次，否則路徑與唯一性都驗不到；
                    // 只是最後整批回捲。與 `assets:bulk-import` 同一個做法。
                    sqlx::query("RELEASE SAVEPOINT bulk_node")
                        .execute(tx.conn())
                        .await
                        .map_err(Problem::from)?;
                    outcomes.push(BulkNodeOutcome {
                        code: row.code.clone(),
                        status: if dry_run { "WOULD_CREATE" } else { "CREATED" }.to_string(),
                        id: Some(r.id),
                        node_path: Some(r.node_path),
                        error: None,
                        error_code: None,
                    });
                }
                Err(e) => {
                    sqlx::query("ROLLBACK TO SAVEPOINT bulk_node")
                        .execute(tx.conn())
                        .await
                        .map_err(Problem::from)?;
                    let (code, msg) = classify_node_error(&e);
                    outcomes.push(BulkNodeOutcome {
                        code: row.code.clone(),
                        status: "FAILED".to_string(),
                        id: None,
                        node_path: None,
                        error: Some(msg),
                        error_code: Some(code),
                    });
                }
            }
        }

        pending = still;
        if pending.is_empty() || pending.len() == before {
            // 沒有進展：剩下的父節點都解不開（不存在，或這幾列互相引用成環）。
            break;
        }
    }

    // 解不開的逐筆回報，並說出是哪一種。
    let unresolved = pending.len();
    for row in pending {
        outcomes.push(BulkNodeOutcome {
            code: row.code.clone(),
            status: "UNRESOLVED_PARENT".to_string(),
            id: None,
            node_path: None,
            error: Some(format!(
                "父節點 `{}` 既不在這個場域裡也不在這份匯入檔可解的部分 —— \
                 它可能不存在，或這幾列互相引用形成了環",
                row.parent_code.clone().unwrap_or_default()
            )),
            error_code: Some("UNRESOLVED_PARENT".to_string()),
        });
    }

    let failed = outcomes.iter().filter(|o| o.status == "FAILED").count();

    if dry_run {
        // 整批回捲。**dry-run 仍然跑完全部的 INSERT**，所以路徑、唯一性、
        // 型別、循環守衛全都真的驗過了 —— 一份 dry-run 通過的檔案不會在
        // 真跑時才炸。
        return Ok(Json(serde_json::json!({
            "data": outcomes,
            "meta": bulk_meta(dry_run, req.rows.len(), created, failed, unresolved),
        })));
    }
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": outcomes,
        "meta": bulk_meta(dry_run, req.rows.len(), created, failed, unresolved),
    })))
}

fn bulk_meta(
    dry_run: bool,
    requested: usize,
    created: usize,
    failed: usize,
    unresolved: usize,
) -> serde_json::Value {
    serde_json::json!({
        "dry_run": dry_run,
        "requested": requested,
        // 四個數字分開。只回「成功 N 筆」會讓「父節點解不開」與「撞唯一索引」
        // 混成同一件事，而那兩者要做的處理完全不同。
        "created": created,
        "failed": failed,
        "unresolved_parent": unresolved,
        "parents_may_appear_after_children": true,
        "dry_run_validates_everything": "dry-run 也真的 INSERT 過再回捲，所以路徑、唯一性、型別與循環守衛都驗過了",
    })
}

fn classify_node_error(e: &sqlx::Error) -> (String, String) {
    let db = e.as_database_error();
    let constraint = db.and_then(|d| d.constraint()).unwrap_or_default();
    let msg = db.map(|d| d.message().to_string()).unwrap_or_default();
    if constraint.contains("uq_spatial_nodes") || msg.contains("duplicate key") {
        return (
            "DUPLICATE_CODE".to_string(),
            "這個場域裡已經有相同 code 的節點".to_string(),
        );
    }
    if msg.contains("cycle") {
        return (
            "TREE_CYCLE".to_string(),
            "這一列會讓樹形成循環（migration 069 的守衛）".to_string(),
        );
    }
    ("DB_ERROR".to_string(), msg)
}

// =============================================================================
// GET /bim-models/{id}
// =============================================================================

/// `GET /bim-models/{bimModelId}`
///
/// 需要 `bim_model:read`。詳情 + 解析報告。
///
/// **`unresolved_elements` 是空陣列有兩種意思**（既有的 `bim.rs` 已經記過這件
/// 事）：`PARSED` 之後的空陣列代表「全部都對應好了」，而 `UPLOADED` 的空陣列
/// 代表「還在排隊等解析」。解析由獨立的 `bim-worker` 服務每 30 秒輪詢處理，
/// 沒有推播通道 —— 呼叫端要輪詢本端點。`status_explanation` 把那個區別講出來。
pub async fn get_bim_model(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let m = sqlx::query!(
        r#"SELECT b.id, b.facility_id, b.name::text AS "name!",
                  b.source_format, b.version_label::text AS version_label,
                  b.storage_bucket::text AS "storage_bucket!",
                  b.storage_key, b.viewer_urn,
                  b.discipline::text AS discipline, b.status,
                  b.element_count, b.mapped_node_count, b.mapped_asset_count,
                  jsonb_array_length(b.unresolved_elements) AS "unresolved_count!",
                  b.parse_report AS "parse_report!", b.parsed_at,
                  b.uploaded_by, u.display_name::text AS uploaded_by_name,
                  b.created_at, b.updated_at
             FROM fms.bim_models b
             LEFT JOIN fms.users u ON u.id = b.uploaded_by
            WHERE b.id = $1"#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("找不到這個 BIM 模型"))?;
    require_permission(&mut tx, "bim_model:read", Some(m.facility_id), None).await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": {
            "id": m.id,
            "facility_id": m.facility_id,
            "name": m.name,
            "source_format": m.source_format,
            "version_label": m.version_label,
            "storage_bucket": m.storage_bucket,
            "storage_key": m.storage_key,
            "viewer_urn": m.viewer_urn,
            "discipline": m.discipline,
            "status": m.status,
            "element_count": m.element_count,
            "mapped_node_count": m.mapped_node_count,
            "mapped_asset_count": m.mapped_asset_count,
            "unresolved_count": m.unresolved_count,
            "parse_report": m.parse_report,
            "parsed_at": m.parsed_at,
            "uploaded_by": m.uploaded_by,
            "uploaded_by_name": m.uploaded_by_name,
            "created_at": m.created_at,
            "updated_at": m.updated_at,
        },
        "meta": {
            // 與 `bim.rs` 的 `unresolved_elements` 端點共用同一份說明 ——
            // 兩支端點對同一個狀態的解釋不該不一樣。
            "status_explanation": crate::bim::parsing_note(&m.status),
            // `UPLOADED` 是排隊中，不是終點站：`element_count` 與
            // `parse_report` 會是 0 與 `{}`，而那不是「模型是空的」——
            // bim-worker 還沒輪到它。
            "awaiting_parse": m.status == "UPLOADED",
        },
    })))
}

// =============================================================================
// POST /bim-models/{id}/mappings
// =============================================================================

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRow {
    pub bim_element_id: String,
    /// `SPATIAL_NODE` 或 `ASSET`。
    pub target_type: String,
    pub target_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMappingsRequest {
    pub mappings: Vec<MappingRow>,
}

/// `POST /bim-models/{bimModelId}/mappings`
///
/// 需要 `bim_model:write`。這是 `mapped_node_count`／`mapped_asset_count`／
/// `unresolved_elements` 的**第一個寫入者**（在此之前那三個欄位只有讀者）。
///
/// 三件事一起做：寫 `bim_element_id`、從 `unresolved_elements` 移除、重算計數。
/// 少了第二件，同一個元件會永遠留在待補正清單裡；少了第三件，畫面上的
/// 「已對應 12 個」是假的。
pub async fn create_mappings(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(model_id): Path<Uuid>,
    Json(req): Json<CreateMappingsRequest>,
) -> Result<Json<serde_json::Value>, Problem> {
    if req.mappings.is_empty() {
        return Err(Problem::validation("`mappings` 不得為空"));
    }
    if req.mappings.len() > 500 {
        return Err(Problem::validation("一次最多 500 筆對應"));
    }
    for (i, m) in req.mappings.iter().enumerate() {
        if !["SPATIAL_NODE", "ASSET"].contains(&m.target_type.as_str()) {
            return Err(
                Problem::validation("`target_type` 必須是 SPATIAL_NODE 或 ASSET").with_errors(
                    vec![FieldError {
                        pointer: format!("/mappings/{i}/target_type"),
                        code: "ENUM".to_string(),
                        message: format!("`{}` 不合", m.target_type),
                    }],
                ),
            );
        }
        if m.bim_element_id.trim().is_empty() || m.bim_element_id.len() > 120 {
            return Err(Problem::validation(format!(
                "`mappings/{i}/bim_element_id` 長度必須是 1–120"
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let model = sqlx::query!(
        "SELECT facility_id, status FROM fms.bim_models WHERE id = $1",
        model_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("找不到這個 BIM 模型"))?;
    require_permission(&mut tx, "bim_model:write", Some(model.facility_id), None).await?;

    let mut applied = 0usize;
    let mut outcomes: Vec<serde_json::Value> = Vec::with_capacity(req.mappings.len());

    for m in &req.mappings {
        // 目標必須在**同一個場域**。跨場域的對應會讓 floor-view 把別棟樓的
        // 設備畫進這一層 —— 而那個錯誤在畫面上看起來只是「位置怪怪的」。
        let ok = if m.target_type == "SPATIAL_NODE" {
            sqlx::query_scalar!(
                r#"UPDATE fms.spatial_nodes
                      SET bim_model_id = $1, bim_element_id = $2,
                          updated_at = clock_timestamp()
                    WHERE id = $3 AND facility_id = $4 AND deleted_at IS NULL
                  RETURNING true AS "ok!""#,
                model_id,
                m.bim_element_id,
                m.target_id,
                model.facility_id,
            )
            .fetch_optional(tx.conn())
            .await
            .map_err(Problem::from)?
        } else {
            sqlx::query_scalar!(
                r#"UPDATE fms.assets
                      SET bim_element_id = $1, updated_at = clock_timestamp()
                    WHERE id = $2 AND facility_id = $3 AND deleted_at IS NULL
                  RETURNING true AS "ok!""#,
                m.bim_element_id,
                m.target_id,
                model.facility_id,
            )
            .fetch_optional(tx.conn())
            .await
            .map_err(Problem::from)?
        };

        if ok.is_some() {
            applied += 1;
            outcomes.push(serde_json::json!({
                "bim_element_id": m.bim_element_id,
                "target_type": m.target_type,
                "target_id": m.target_id,
                "ok": true,
            }));
        } else {
            outcomes.push(serde_json::json!({
                "bim_element_id": m.bim_element_id,
                "target_type": m.target_type,
                "target_id": m.target_id,
                "ok": false,
                "error_code": "TARGET_NOT_IN_FACILITY",
                "error": "目標不存在、已軟刪除，或不屬於這個模型的場域 —— \
                          跨場域的對應會讓 floor-view 把別棟樓的設備畫進這一層",
            }));
        }
    }

    // 從 `unresolved_elements` 移除已對應的，並重算兩個計數。
    // **三件事在同一個交易裡**，所以不可能出現「對應好了但還在待補正清單」。
    let mapped_ids: Vec<String> = req
        .mappings
        .iter()
        .map(|m| m.bim_element_id.clone())
        .collect();
    let after = sqlx::query!(
        r#"UPDATE fms.bim_models b SET
             unresolved_elements = coalesce((
               SELECT jsonb_agg(e)
                 FROM jsonb_array_elements(b.unresolved_elements) e
                WHERE coalesce(e ->> 'bim_element_id', e #>> '{}') <> ALL($2::text[])
             ), '[]'::jsonb),
             mapped_node_count = (
               SELECT count(*) FROM fms.spatial_nodes n
                WHERE n.bim_model_id = b.id AND n.bim_element_id IS NOT NULL
                  AND n.deleted_at IS NULL),
             mapped_asset_count = (
               SELECT count(*) FROM fms.assets a
                WHERE a.facility_id = b.facility_id AND a.bim_element_id IS NOT NULL
                  AND a.deleted_at IS NULL),
             updated_at = clock_timestamp()
           WHERE b.id = $1
           RETURNING mapped_node_count, mapped_asset_count,
                     jsonb_array_length(unresolved_elements) AS "unresolved_count!""#,
        model_id,
        &mapped_ids,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": outcomes,
        "meta": {
            "requested": req.mappings.len(),
            "applied": applied,
            "rejected": req.mappings.len() - applied,
            // 三個欄位一起更新 —— 它們在這支端點之前只有讀者。
            "mapped_node_count": after.mapped_node_count,
            "mapped_asset_count": after.mapped_asset_count,
            "unresolved_count": after.unresolved_count,
            "counts_recomputed_from_rows": "計數是重算的（不是遞增），所以它與實際對應數不可能分歧",
        },
    })))
}

// =============================================================================
// GET /facilities/{facilityId}/floor-view
// =============================================================================

#[derive(Debug, Deserialize)]
pub struct FloorViewQuery {
    /// 只回這一層（`floor_level`）。省略時回全部樓層。
    pub floor_level: Option<i32>,
}

/// `GET /facilities/{facilityId}/floor-view`
///
/// 需要 `spatial_node:read`。逐層的節點 + 設備數 + 未結告警 + 幾何 +
/// 即時佔用狀態 + 設備連線狀態。
///
/// **`geometry` 為 `{}` 的節點畫不出來。** 那不是「這個房間沒有形狀」，
/// 是沒有人匯入過幾何 —— Phase 1 沒有 BIM 解析器，所以那是常態。
/// `meta.nodes_without_geometry` 讓那個數字看得見，否則前端只會畫出一張
/// 缺了一半房間的圖而不知道為什麼。
///
/// # 為什麼佔用狀態沒有另外檢查 `reservation:read`
///
/// 與 `asset_count`／`open_work_orders`／`active_alarms` 同一個判斷：這支
/// 端點是「單一請求回傳整層樓概覽」，聚合數字不逐一檢查各自領域的權限
/// （與 `fms-report` 的 facility-dashboard 同一個先例）。只回聚合後的
/// `occupancy_state`（FREE／OCCUPIED／RESERVED／HELD），**不回** `reservation:read`
/// 才會回的 `title`／`organizer_name` —— 私人預約的遮罩問題因此不存在，
/// 因為這裡本來就不打算回那些欄位。
///
/// # 為什麼設備連線用 `fms.device_connectivity()`
///
/// 不是自己重算：那支函式（migration 081）已經是 `fms-asset` 的
/// 單一真實來源，這裡是第三個消費者，複製一份判定式只會製造漂移。
///
/// # 為什麼 `occupancy_end_at`／`occupancy_start_at` 不需要額外的權限檢查
///
/// 時間本身從來不是 `occupancy` 端點遮罩的對象——遮罩只作用在
/// `title`／`organizer_name`（誰訂的），`start_at`／`end_at` 對私人預約
/// 一樣照回，見 `fms-reservation` 的 `occupancy` handler。這裡跟著回時間，
/// 不會比既有的 `occupancy` 端點洩漏更多。
///
/// # 為什麼多一個 `worst_alarm_rank`
///
/// `worst_alarm_severity` 是 `max(severity)` 的字串，字典序不是嚴重度序
/// （`CRITICAL` 字母排序在 `WARNING` 前面，嚴重度卻相反）——前端要用它
/// 排序或決定顏色深淺，必須先查 `meta.alarm_severity_order` 才能用，
/// 多一趟轉換。`worst_alarm_rank` 直接給排好序的數字（`array_position` 是
/// Postgres 的 1-based 陣列索引，所以是 1=INFO…5=CRITICAL，沒有未結告警時
/// 是 `null`），前端可以直接拿來排序或做顏色漸層，不用查表。舊欄位保留，
/// 不是每個消費者都需要排序。
pub async fn floor_view(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Query(q): Query<FloorViewQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "spatial_node:read", Some(facility_id), None).await?;

    let rows = sqlx::query!(
        r#"SELECT n.id, n.parent_id, n.code::text AS "code!", n.name::text AS "name!",
                  n.node_type_code::text AS "node_type_code!",
                  n.node_path::text AS "node_path!", n.depth,
                  n.floor_level, n.floor_label::text AS floor_label,
                  n.area_sqm::float8 AS area_sqm, n.capacity, n.is_bookable,
                  n.geometry AS "geometry!", n.bim_element_id::text AS bim_element_id,
                  (SELECT count(*) FROM fms.assets a
                    WHERE a.spatial_node_id = n.id AND a.deleted_at IS NULL)
                    AS "asset_count!",
                  (SELECT count(*) FROM fms.work_orders w
                     LEFT JOIN fms.work_order_statuses st ON st.code = w.status
                    WHERE w.spatial_node_id = n.id AND w.deleted_at IS NULL
                      AND st.is_terminal IS NOT TRUE) AS "open_work_orders!",
                  (SELECT count(*) FROM fms.alarms al
                    WHERE al.spatial_node_id = n.id
                      AND al.status IN ('ACTIVE','ACKNOWLEDGED')) AS "active_alarms!",
                  (SELECT max(al.severity) FROM fms.alarms al
                    WHERE al.spatial_node_id = n.id
                      AND al.status IN ('ACTIVE','ACKNOWLEDGED')) AS worst_alarm_severity,
                  (SELECT array_position(
                            ARRAY['INFO','WARNING','MINOR','MAJOR','CRITICAL']::text[],
                            max(al.severity))
                     FROM fms.alarms al
                    WHERE al.spatial_node_id = n.id
                      AND al.status IN ('ACTIVE','ACKNOWLEDGED')) AS worst_alarm_rank,
                  (SELECT count(*) FROM fms.devices d
                    WHERE d.spatial_node_id = n.id
                       OR d.asset_id IN (SELECT id FROM fms.assets
                                          WHERE spatial_node_id = n.id AND deleted_at IS NULL))
                    AS "device_count!",
                  (SELECT count(*) FROM fms.devices d
                    WHERE (d.spatial_node_id = n.id
                           OR d.asset_id IN (SELECT id FROM fms.assets
                                              WHERE spatial_node_id = n.id AND deleted_at IS NULL))
                      AND fms.device_connectivity(d.status, d.last_seen_at,
                                                   d.offline_alarm_after_seconds) = 'OFFLINE')
                    AS "devices_offline_count!"
             FROM fms.spatial_nodes n
            WHERE n.facility_id = $1 AND n.deleted_at IS NULL AND n.is_active
              AND ($2::int IS NULL OR n.floor_level = $2::int)
            ORDER BY n.floor_level NULLS FIRST, n.node_path"#,
        facility_id,
        q.floor_level,
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;

    // 即時佔用狀態：重用 occupancy 既有的 repo 函式，不重寫一份
    // FREE/OCCUPIED/RESERVED/HELD 的判定 —— 那條判定式（含 hold 的時間窗、
    // CHECKED_IN 優先於一般 CONFIRMED）已經在 reservation 模組驗過一次，
    // 複製只會漂移。`resource_id` 對 SPATIAL_NODE 型別的可預約資源就是
    // 節點 id 本身。
    let occupancy_rows = reservation_occupancy(&mut tx, facility_id).await?;
    tx.commit().await?;

    let occupancy_by_node: std::collections::HashMap<Uuid, &fms_reservation::repo::OccupancyRow> =
        occupancy_rows.iter().map(|r| (r.resource_id, r)).collect();

    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let occ = occupancy_by_node.get(&r.id);
            serde_json::json!({
                "id": r.id,
                "parent_id": r.parent_id,
                "code": r.code,
                "name": r.name,
                "node_type_code": r.node_type_code,
                "node_path": r.node_path,
                "depth": r.depth,
                "floor_level": r.floor_level,
                "floor_label": r.floor_label,
                "area_sqm": r.area_sqm,
                "capacity": r.capacity,
                "is_bookable": r.is_bookable,
                // `{}` = 沒有幾何，畫不出來。見函式檔頭。
                "geometry": r.geometry,
                "bim_element_id": r.bim_element_id,
                "asset_count": r.asset_count,
                "open_work_orders": r.open_work_orders,
                "active_alarms": r.active_alarms,
                // 這一層最嚴重的告警等級 —— 字串，字典序。保留給只需要顯示
                // 文字的消費者；要排序或決定顏色深淺請用 `worst_alarm_rank`。
                "worst_alarm_severity": r.worst_alarm_severity,
                // 已經是排好序的數字（1=INFO…5=CRITICAL），沒有未結告警是
                // null。不用查 `meta.alarm_severity_order` 就能直接排序。
                "worst_alarm_rank": r.worst_alarm_rank,
                // 不是可預約資源的節點（走廊、樓層本身）回 null，
                // 不是 "FREE" —— null 代表「這個問題對它沒有意義」，
                // FREE 會被誤讀成「可以訂」。
                "occupancy_state": occ.map(|o| o.state.as_str()),
                // 時間不是遮罩對象（見函式檔頭），私人預約一樣照回。
                "occupancy_start_at": occ.and_then(|o| o.start_at),
                "occupancy_end_at": occ.and_then(|o| o.end_at),
                "device_count": r.device_count,
                "devices_offline_count": r.devices_offline_count,
            })
        })
        .collect();

    let without_geometry = rows
        .iter()
        .filter(|r| r.geometry.as_object().is_some_and(|o| o.is_empty()))
        .count();
    let floors: std::collections::BTreeSet<i32> =
        rows.iter().filter_map(|r| r.floor_level).collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "facility_id": facility_id,
            "floor_level": q.floor_level,
            "node_count": rows.len(),
            "floors": floors.iter().collect::<Vec<_>>(),
            // **畫不出來的節點數。** 那不是「沒有形狀」，是沒有人匯入幾何 ——
            // BIM 模型還在排隊等 bim-worker 解析，或這個節點本來就是手動建立、
            // 從沒掛過幾何。少了這個數字，前端只會畫出一張缺了一半房間的圖
            // 而不知道為什麼。
            "nodes_without_geometry": without_geometry,
            "geometry_comes_from": "BIM 解析（bim-worker 非同步處理，見 GET .../bim-models/{id} 的 status）或手動匯入",
            // `max(severity)` 是字典序，前端要照這個順序排嚴重度。
            "alarm_severity_order": ["INFO", "WARNING", "MINOR", "MAJOR", "CRITICAL"],
        },
    })))
}
