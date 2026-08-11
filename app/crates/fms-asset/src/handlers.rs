//! 資產端點（WBS 4.6、4.7）。契約中的六支：
//! `GET/POST /assets`、`GET/PATCH/DELETE /assets/{assetId}`、
//! `GET /assets/{assetId}/dependency-graph`。
//!
//! `sort`（單欄，`-` 前綴為降冪）與 `fields`（稀疏欄位集合）都已實作。
//! 排序欄位白名單見 `ASSET_SORTABLE`；游標會記下排序欄位，
//! 客戶端改了 `sort` 卻沿用舊 `cursor` 會得到 400 而非語意錯亂的一頁。

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, concurrency, fields, include, page, require_permission, Caller,
    Cursor, PageMeta, Problem, ProblemCode, SortSpec,
};

use crate::dto::*;
use crate::repo;

#[derive(Clone)]
pub struct AssetState {
    pub pool: PgPool,
}

const ENDPOINT: &str = "POST /assets";

/// `fields` 允許的欄位 = 契約 `Asset` schema 宣告的全部欄位。
const ASSET_FIELDS: &[&str] = &[
    "id",
    "facility_id",
    "spatial_node_id",
    "spatial_node_path",
    "asset_code",
    "name",
    "serial_no",
    "category_code",
    "asset_model_id",
    "parent_asset_id",
    "criticality",
    "status",
    "install_date",
    "warranty_end_date",
    "health_score",
    "last_telemetry_at",
    "open_work_order_count",
    "active_alarm_count",
    "specifications",
    "attributes",
    "version",
    "created_at",
    "updated_at",
];

fn to_dto(r: repo::AssetRow) -> AssetDto {
    AssetDto {
        id: r.id,
        facility_id: r.facility_id,
        spatial_node_id: r.spatial_node_id,
        spatial_node_path: r.spatial_node_path,
        asset_code: r.asset_code,
        name: r.name,
        serial_no: r.serial_no,
        category_code: r.category_code,
        asset_model_id: r.asset_model_id,
        parent_asset_id: r.parent_asset_id,
        criticality: r.criticality,
        status: r.status,
        install_date: r.install_date,
        warranty_end_date: r.warranty_end_date,
        health_score: r.health_score,
        last_telemetry_at: r.last_telemetry_at,
        open_work_order_count: r.open_work_order_count,
        active_alarm_count: r.active_alarm_count,
        specifications: r.specifications,
        attributes: r.attributes,
        version: r.version,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

/// `assets_status_check` 與 `assets_criticality_check` 允許的值。
///
/// 在應用層再擋一次，理由是 `query!` 巨集**不驗證 CHECK 約束的字串值** ——
/// schema 刻意採 `TEXT + CHECK`（避免加值時的 exclusive lock），代價就是
/// 非法值只會在執行期被資料庫拒絕。若不先擋，客戶端傳
/// `status: "ACTIVE"`（一個看似合理但不在清單內的值）會得到 500，
/// 而正確答案是 422。
const ASSET_STATUS: &[&str] = &[
    "PLANNED",
    "IN_STORAGE",
    "INSTALLING",
    "OPERATIONAL",
    "DEGRADED",
    "DOWN",
    "UNDER_MAINTENANCE",
    "DECOMMISSIONED",
];
const ASSET_CRITICALITY: &[&str] = &["LOW", "MEDIUM", "HIGH", "CRITICAL"];

fn validate_enums(w: &AssetWrite) -> Result<(), Problem> {
    if let Some(s) = w.status.as_deref() {
        if !ASSET_STATUS.contains(&s) {
            return Err(Problem::validation(format!(
                "invalid status `{s}`; allowed: {ASSET_STATUS:?}"
            )));
        }
    }
    if let Some(c) = w.criticality.as_deref() {
        if !ASSET_CRITICALITY.contains(&c) {
            return Err(Problem::validation(format!(
                "invalid criticality `{c}`; allowed: {ASSET_CRITICALITY:?}"
            )));
        }
    }
    Ok(())
}

/// 可排序欄位。與 `repo::list` 的 ORDER BY 分支必須一致 ——
/// 白名單多列一個而 SQL 沒對應分支，該排序會靜默退化成預設順序。
const ASSET_SORTABLE: &[&str] = &["created_at", "asset_code", "name"];

/// `GET /assets`
pub async fn list(
    State(state): State<AssetState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let sort = SortSpec::parse(q.sort.as_deref(), ASSET_SORTABLE, "created_at", true)?;
    let projection = fields::parse(q.fields.as_deref(), ASSET_FIELDS)?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "asset:read", q.facility_id, None).await?;

    let limit = clamp_limit(q.limit);
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };

    let rows = repo::list(
        &mut tx,
        q.facility_id,
        q.spatial_node_id,
        q.subtree_of_node,
        q.category_code.as_deref(),
        q.status.as_deref(),
        q.criticality.as_deref(),
        q.has_open_work_order,
        q.q.as_deref(),
        cursor.as_ref(),
        &sort,
        limit,
    )
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, col| r.cursor_key(col));
    let data: Vec<serde_json::Value> = paged
        .data
        .into_iter()
        .map(|r| serde_json::to_value(to_dto(r)).unwrap_or(serde_json::Value::Null))
        .collect();

    Ok(Json(serde_json::json!({
        "data": fields::project_all(data, &projection),
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

/// `include` 目前真能提供的關聯。
const ASSET_INCLUDES: &[&str] = &[
    "children",
    "relations",
    "meters",
    "maintenance_plans",
    "open_work_orders",
];

/// 契約的 `include` 說明列出、但伺服器尚未提供的關聯。
///
/// 目前是空的：`open_work_orders` 已隨工單模組（S4）上線。保留這個機制
/// 而不是刪掉，因為後續模組還會再用到 —— 回 422 並附原因，
/// 比接受後回傳空陣列誠實：空陣列是「查過了，沒有」的斷言。
const ASSET_INCLUDES_DEFERRED: &[(&str, &str)] = &[];

/// `GET /assets/{assetId}`
///
/// 回應是契約的 `AssetDetail`：基底 `Asset` 欄位，加上 `include` 要求的關聯。
/// 未要求的關聯**不出現**（而非出現為空陣列）—— 契約中它們都是選用欄位，
/// 空陣列會被讀成「查過了，沒有」。
///
/// 每個關聯各一次查詢，都在同一個交易內，因此展開出來的內容彼此一致。
pub async fn get(
    State(state): State<AssetState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Query(q): Query<GetQuery>,
) -> Result<(HeaderMap, Json<serde_json::Value>), Problem> {
    let includes = include::parse(
        q.include.as_deref(),
        ASSET_INCLUDES,
        ASSET_INCLUDES_DEFERRED,
    )?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let row = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("asset not found"))?;
    require_permission(&mut tx, "asset:read", Some(row.facility_id), None).await?;

    let version = row.version;
    let mut body = serde_json::to_value(to_dto(row)).map_err(Problem::internal)?;

    if !includes.is_empty() {
        // 上面已確認 to_dto 的輸出是物件，因此這個分支必然成立。
        let Some(obj) = body.as_object_mut() else {
            return Err(Problem::internal(std::io::Error::other(
                "asset serialised to a non-object",
            )));
        };

        if includes.has("children") {
            let kids: Vec<AssetDto> = repo::children(&mut tx, id)
                .await?
                .into_iter()
                .map(to_dto)
                .collect();
            obj.insert(
                "children".into(),
                serde_json::to_value(kids).map_err(Problem::internal)?,
            );
        }

        if includes.has("relations") {
            let edges = repo::relations(&mut tx, id).await?;
            let ids: Vec<Uuid> = edges.iter().map(|e| e.other_asset_id).collect();
            // 一次取回所有對方設備，而不是每條邊查一次。
            let mut by_id: std::collections::HashMap<Uuid, AssetDto> = repo::by_ids(&mut tx, &ids)
                .await?
                .into_iter()
                .map(|r| (r.id, to_dto(r)))
                .collect();

            // 對方設備可能已被軟刪除（`fetch` 會濾掉），此時整條邊都省略 ——
            // 契約的 relations[].asset 不可為 null。
            let relations: Vec<RelationDto> = edges
                .into_iter()
                .filter_map(|e| {
                    by_id.remove(&e.other_asset_id).map(|asset| RelationDto {
                        relation_type: e.relation_type,
                        direction: e.direction,
                        impact_level: e.impact_level,
                        asset,
                    })
                })
                .collect();
            obj.insert(
                "relations".into(),
                serde_json::to_value(relations).map_err(Problem::internal)?,
            );
        }

        if includes.has("meters") {
            let meters: Vec<MeterDto> = repo::meters(&mut tx, id)
                .await?
                .into_iter()
                .map(|m| MeterDto {
                    meter_code: m.meter_code,
                    name: m.name,
                    unit: m.unit,
                    last_value: m.last_value,
                    last_read_at: m.last_read_at,
                })
                .collect();
            obj.insert(
                "meters".into(),
                serde_json::to_value(meters).map_err(Problem::internal)?,
            );
        }

        if includes.has("open_work_orders") {
            // 委派給工單模組，讓「未結」的定義只有一份（見該函式的說明）。
            let wos = fms_workorder::handlers::open_work_orders_for_asset(&mut tx, id).await?;
            obj.insert(
                "open_work_orders".into(),
                serde_json::to_value(wos).map_err(Problem::internal)?,
            );
        }

        if includes.has("maintenance_plans") {
            let plans: Vec<PlanDto> = repo::maintenance_plans(&mut tx, id)
                .await?
                .into_iter()
                .map(|p| PlanDto {
                    id: p.id,
                    facility_id: p.facility_id,
                    code: p.code,
                    name: p.name,
                    template_id: p.template_id,
                    template_name: p.template_name,
                    target: PlanTargetDto {
                        kind: p.target_type,
                        id: p.target_id,
                        label: p.target_label,
                    },
                    trigger_type: p.trigger_type,
                    rrule: p.rrule,
                    meter_code: p.meter_code,
                    meter_threshold: p.meter_threshold,
                    generate_lead_days: p.generate_lead_days,
                    priority: p.priority,
                    assigned_team_id: p.assigned_team_id,
                    next_due_at: p.next_due_at,
                    is_active: p.is_active,
                })
                .collect();
            obj.insert(
                "maintenance_plans".into(),
                serde_json::to_value(plans).map_err(Problem::internal)?,
            );
        }
    }

    tx.commit().await?;

    let mut headers = HeaderMap::new();
    // ETag 只反映 `assets.version`，刻意不涵蓋展開出來的關聯：
    // `If-Match` 保護的是 PATCH /assets/{id} 這一筆的並行更新，
    // 把子設備或讀表的變動也算進去只會造成無關的 412。
    headers.insert(
        axum::http::header::ETAG,
        format!("\"{version}\"")
            .parse()
            .map_err(|_| Problem::internal(std::io::Error::other("bad etag")))?,
    );
    Ok((headers, Json(body)))
}

/// `depth` 的界線，對齊契約：min 1、max 5、default 2。
const GRAPH_DEPTH_DEFAULT: i32 = 2;
const GRAPH_DEPTH_MAX: i32 = 5;
const GRAPH_DIRECTIONS: &[&str] = &["upstream", "downstream", "both"];

/// `GET /assets/{assetId}/dependency-graph`
///
/// 影響分析用：規格的例子是「UPS 停機會影響哪些設備」。
///
/// `direction` 的語意由 `relation_type` 決定，不是由儲存的 from／to 決定
/// （見 `repo::relations` 的說明表）。回傳的 `edges` 保持儲存方向，
/// 因為要與 `relation_type` 一起讀才正確。
///
/// `depth` 的上界不只是照抄契約：遞迴走訪的節點數可能隨深度指數成長，
/// 上界是保護資料庫，因此超界回 422 而非默默夾到 5。
pub async fn dependency_graph(
    State(state): State<AssetState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<DependencyGraphDto>, Problem> {
    let depth = q.depth.unwrap_or(GRAPH_DEPTH_DEFAULT);
    if !(1..=GRAPH_DEPTH_MAX).contains(&depth) {
        return Err(Problem::validation(format!(
            "`depth` must be between 1 and {GRAPH_DEPTH_MAX}, got {depth}"
        )));
    }
    let direction = q.direction.unwrap_or_else(|| "both".to_string());
    if !GRAPH_DIRECTIONS.contains(&direction.as_str()) {
        return Err(Problem::validation(format!(
            "invalid `direction` `{direction}`; allowed: {GRAPH_DIRECTIONS:?}"
        )));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let root = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("asset not found"))?;
    require_permission(&mut tx, "asset:read", Some(root.facility_id), None).await?;

    let nodes = repo::graph_nodes(&mut tx, id, depth, &direction).await?;
    let ids: Vec<Uuid> = nodes.iter().map(|n| n.id).collect();
    let edges = repo::graph_edges(&mut tx, &ids).await?;
    tx.commit().await?;

    Ok(Json(DependencyGraphDto {
        nodes: nodes
            .into_iter()
            .map(|n| GraphNodeDto {
                id: n.id,
                asset_code: n.asset_code,
                name: n.name,
                category_code: n.category_code,
                status: n.status,
                criticality: n.criticality,
            })
            .collect(),
        edges: edges
            .into_iter()
            .map(|e| GraphEdgeDto {
                from_asset_id: e.from_asset_id,
                to_asset_id: e.to_asset_id,
                relation_type: e.relation_type,
                impact_level: e.impact_level,
            })
            .collect(),
    }))
}

/// `POST /assets`
pub async fn create(
    State(state): State<AssetState>,
    caller: Caller,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let w: AssetWrite = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid AssetCreate: {e}")))?;

    // 契約的 required: [facility_id, asset_code, name, category_code]
    let facility_id = w
        .facility_id
        .ok_or_else(|| Problem::validation("facility_id is required"))?;
    let asset_code = w
        .asset_code
        .as_deref()
        .ok_or_else(|| Problem::validation("asset_code is required"))?;
    let name = w
        .name
        .as_deref()
        .ok_or_else(|| Problem::validation("name is required"))?;
    let category_code = w
        .category_code
        .as_deref()
        .ok_or_else(|| Problem::validation("category_code is required"))?;
    validate_enums(&w)?;

    let idem_key = concurrency::key_from(&headers)?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // 冪等**登記**在最前面；**回放**必須等授權跑完 ——
    // 先前回放走在 require_permission 之前，命中鍵的請求完全不經授權
    // （見 docs/security-review-open-items.md 第 1 項）。
    let pending = match idem_key.as_deref() {
        Some(key) => concurrency::begin(&mut tx, key, ENDPOINT, &raw)
            .await?
            .pending(),
        None => None,
    };

    let auth = require_permission(&mut tx, "asset:write", Some(facility_id), None).await?;

    if let Some(replay) = pending {
        let (code, body) = replay.release(auth);
        tx.commit().await?;
        return Ok((code, Json(body)));
    }

    let category_id = repo::resolve_category(&mut tx, category_code)
        .await?
        .ok_or_else(|| Problem::validation(format!("unknown category_code: {category_code}")))?;

    // **動態欄位在這裡驗**，不是資料庫觸發器。
    //
    // `attribute_definitions` 在此之前是 0 列 0 讀者 —— 一份「宣告了沒有人讀」
    // 的定義。這一行讓它有了讀者。
    //
    // 為什麼在 API 層：schema 可以隨時被管理者改，而資料庫約束只能驗當下那一版
    // —— 一次設定變更會讓歷史資料變成無法儲存的東西（任何 UPDATE 都會撞上
    // 新約束，即使那次更新與 attributes 無關）。代價是既有值不被回溯驗證，
    // 那是刻意的取捨，記在 `attributes.rs` 的模組檔頭。
    crate::attributes::validate_asset_attributes(&mut tx, w.attributes.as_ref()).await?;

    let id = repo::create(
        &mut tx,
        repo::NewAsset {
            facility_id,
            category_id,
            asset_code,
            name,
            spatial_node_id: w.spatial_node_id,
            parent_asset_id: w.parent_asset_id,
            asset_model_id: w.asset_model_id,
            serial_no: w.serial_no.as_deref(),
            criticality: w.criticality.as_deref(),
            status: w.status.as_deref(),
            install_date: w.install_date,
            warranty_end_date: w.warranty_end_date,
            custodian_user_id: w.custodian_user_id,
            specifications: w.specifications.as_ref(),
            attributes: w.attributes.as_ref(),
        },
    )
    .await?;

    let created = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("asset vanished after insert")))?;
    let body = serde_json::to_value(to_dto(created)).map_err(Problem::internal)?;

    if let Some(key) = idem_key.as_deref() {
        concurrency::complete(&mut tx, key, ENDPOINT, 201, &body).await?;
    }
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(body)))
}

/// `PATCH /assets/{assetId}`；`If-Match` 必填。
pub async fn update(
    State(state): State<AssetState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(w): Json<AssetWrite>,
) -> Result<Json<AssetDto>, Problem> {
    let expected_version = concurrency::required_if_match(&headers)?;
    validate_enums(&w)?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // **先鎖，再讀。** 讀出來的 `version` 要用來比對，而沒有鎖的讀取會讓
    // 兩個並發的 PATCH 讀到同一個版本、都通過比對、都寫入（lost update）。
    // 見 `concurrency::check_version` 的說明與 `concurrency_correctness_slice.rs` 的 `d_`。
    repo::lock(&mut tx, id).await?;
    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("asset not found"))?;
    require_permission(&mut tx, "asset:write", Some(current.facility_id), None).await?;
    concurrency::check_version(expected_version, current.version)?;

    let category_id = match w.category_code.as_deref() {
        Some(code) => Some(
            repo::resolve_category(&mut tx, code)
                .await?
                .ok_or_else(|| Problem::validation(format!("unknown category_code: {code}")))?,
        ),
        None => None,
    };

    repo::update(&mut tx, id, category_id, &w).await?;
    let updated = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("asset not found"))?;
    tx.commit().await?;

    Ok(Json(to_dto(updated)))
}

/// `DELETE /assets/{assetId}` —— 軟刪除（報廢）。
///
/// 契約在「被參照」時回 409。這裡把兩種參照視為阻擋：
/// 尚有子設備（刪掉會讓子樹失去父節點），以及尚有未結工單。
pub async fn delete(
    State(state): State<AssetState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("asset not found"))?;
    require_permission(&mut tx, "asset:delete", Some(current.facility_id), None).await?;

    let blockers = repo::delete_blockers(&mut tx, id).await?;
    if blockers.children > 0 || blockers.open_work_orders > 0 {
        return Err(Problem::new(ProblemCode::Conflict).with_detail(format!(
            "asset is still referenced: {} child asset(s), {} open work order(s)",
            blockers.children, blockers.open_work_orders
        )));
    }

    repo::soft_delete(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// =============================================================================
// WBS 4.8：設備型錄
// =============================================================================

/// 型錄游標的兩段鍵之間的分隔字元。
///
/// 用 US（unit separator, U+001F）而不是 `|`：型號字串裡出現 `|` 是可能的，
/// 出現控制字元則不是。`Cursor` 的外層格式已經用掉 `|`，這裡需要另一個。
const MODEL_KEY_SEP: char = '\u{1f}';

const MODEL_SCOPES: &[&str] = &["all", "platform", "tenant"];

/// `GET /asset-models`
///
/// 型錄同時含平台共用（`tenant_id IS NULL`）與租戶自建的列，
/// 對外以 `is_platform` 區分。`scope` 讓客戶端只取其中一種。
pub async fn list_models(
    State(state): State<AssetState>,
    caller: Caller,
    Query(q): Query<ModelQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let scope = q.scope.unwrap_or_else(|| "all".to_string());
    if !MODEL_SCOPES.contains(&scope.as_str()) {
        return Err(Problem::validation(format!(
            "invalid scope `{scope}`; allowed: {MODEL_SCOPES:?}"
        )));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 型錄是租戶層的參考資料，不繫於單一場域，因此不帶 facility 範圍。
    require_permission(&mut tx, "asset_model:read", None, None).await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "manufacturer".to_string(),
        desc: false,
    };
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };

    let rows = repo::list_models(
        &mut tx,
        q.category_code.as_deref(),
        q.manufacturer.as_deref(),
        &scope,
        cursor.as_ref(),
        limit,
    )
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| {
        (
            format!("{}{MODEL_KEY_SEP}{}", r.manufacturer, r.model_no),
            r.id,
        )
    });
    let data: Vec<AssetModelDto> = paged
        .data
        .into_iter()
        .map(|r| AssetModelDto {
            id: r.id,
            is_platform: r.is_platform,
            category_code: r.category_code,
            manufacturer: r.manufacturer,
            model_no: r.model_no,
            name: r.name,
            specifications: r.specifications,
            supported_protocols: r.supported_protocols,
            expected_life_months: r.expected_life_months,
        })
        .collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

// =============================================================================
// WBS 4.9：計量讀數
// =============================================================================

const READING_ENDPOINT: &str = "POST /assets/{assetId}/meters/{meterCode}/readings";
/// 契約允許的 `source`。`IOT` 不在其中：那條路徑是 `fms.ingest_telemetry`，
/// 不該能從人工登錄端點假冒。
const READING_SOURCES: &[&str] = &["MANUAL", "IMPORT", "ESTIMATED"];

// `next_last_value` 已移除 —— 讀數推進規則現在只有一份，在
// `fms.next_meter_value()`（migration 030），由 `repo::next_meter_value` 呼叫。
//
// 先前這裡有一份**正確**的實作，而 `fms.ingest_telemetry` 有一份**錯誤**的
// （一律 `last_value = value`，對 DELTA 型讀表把增量寫成總量）。後果比
// 「有一個 bug」更糟：同一支讀表，人工登錄與 IoT 上報會推進出不同的
// `last_value`，而 PM 的門檻觸發讀的正是它 —— 保養會不會被觸發，
// 取決於讀數是誰送進來的。

/// `POST /assets/{assetId}/meters/{meterCode}/readings`
///
/// 歷史與當前值分開處理（見 `repo::record_reading`），
/// 門檻判定的規則見 `repo::plans_crossing_threshold`。
///
/// 本端點**不產生工單**：回傳的是「哪些保養計畫到期了」，
/// 並發出 outbox 事件讓 PM 產生器接手。契約的欄位名
/// （`triggered_maintenance_plan_ids`）本來就是這個語意。
pub async fn record_reading(
    State(state): State<AssetState>,
    caller: Caller,
    Path((asset_id, meter_code)): Path<(Uuid, String)>,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let w: ReadingWrite = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid reading: {e}")))?;
    let value = w
        .value
        .ok_or_else(|| Problem::validation("value is required"))?;
    if !value.is_finite() {
        return Err(Problem::validation("value must be a finite number"));
    }
    let reading_at = w
        .reading_at
        .ok_or_else(|| Problem::validation("reading_at is required"))?;
    let source = w.source.unwrap_or_else(|| "MANUAL".to_string());
    if !READING_SOURCES.contains(&source.as_str()) {
        return Err(Problem::validation(format!(
            "invalid source `{source}`; allowed: {READING_SOURCES:?}"
        )));
    }

    let idem_key = concurrency::key_from(&headers)?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let pending = match idem_key.as_deref() {
        Some(key) => concurrency::begin(&mut tx, key, READING_ENDPOINT, &raw)
            .await?
            .pending(),
        None => None,
    };

    // 回放也要先解析資產並通過授權：資產若已刪除，重試得到 404 而非回放。
    // 刻意讓回放與首次執行走同一道門（見 fms-shared 的 PendingReplay）。
    let asset = repo::get(&mut tx, asset_id)
        .await?
        .ok_or_else(|| Problem::not_found("asset not found"))?;
    let auth = require_permission(&mut tx, "meter:write", Some(asset.facility_id), None).await?;

    if let Some(replay) = pending {
        let (code, body) = replay.release(auth);
        tx.commit().await?;
        return Ok((code, Json(body)));
    }

    let meter = repo::meter_state(&mut tx, asset_id, &meter_code)
        .await?
        .ok_or_else(|| Problem::not_found(format!("asset has no active meter `{meter_code}`")))?;

    let old_value = meter.last_value;
    // 推進規則委派給 `fms.next_meter_value()`（030）：IoT 上報走的是同一支，
    // 因此兩條路徑不可能對同一支讀表算出不同的值。錯誤（負增量、累計倒退）
    // 由函式以 `METER_VALUE_INVALID` 標記拋出，`Problem::from` 轉譯成 422。
    let new_last_value = repo::next_meter_value(&mut tx, meter.id, value).await?;
    let last_value =
        repo::record_reading(&mut tx, &meter, value, new_last_value, reading_at, &source).await?;

    // 門檻判定用實際生效的當前值：遲到的讀數不會推進 last_value，
    // 也就不該觸發保養 —— 否則補登三個月前的讀數會產生一張今天的工單。
    let triggered = if last_value == new_last_value {
        repo::plans_crossing_threshold(
            &mut tx,
            asset_id,
            &meter.meter_code,
            &meter.reading_type,
            old_value,
            new_last_value,
        )
        .await?
    } else {
        Vec::new()
    };

    if !triggered.is_empty() {
        repo::emit_threshold_event(
            &mut tx,
            asset_id,
            &meter.meter_code,
            last_value,
            reading_at,
            &triggered,
        )
        .await?;
    }

    let body = serde_json::to_value(ReadingResultDto {
        meter_code: meter.meter_code,
        last_value,
        triggered_maintenance_plan_ids: triggered,
    })
    .map_err(Problem::internal)?;

    if let Some(key) = idem_key.as_deref() {
        concurrency::complete(&mut tx, key, READING_ENDPOINT, 201, &body).await?;
    }
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(body)))
}
