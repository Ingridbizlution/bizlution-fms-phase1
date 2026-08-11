//! 組織與空間節點的單筆讀寫刪（契約 §2 的其餘六支）。
//!
//! # 刪除一定是軟刪除，而理由是量出來的
//!
//! 兩張表都有 `deleted_at`，而**硬刪除會靜默地毀掉別的東西**。
//! 量了一下誰參照它們（`pg_constraint.confdeltype`）：
//!
//! `spatial_nodes` 被 12 個地方參照，其中 **4 個是 CASCADE**：
//! `bookable_resources`、`desk_assignments`、`maintenance_plans`、
//! `visitor_access_grants`。硬刪一個房間會連帶刪掉它的可預約資源與保養計畫，
//! 而回應只會是一個 204。
//!
//! `organizations` 被 7 個地方參照，`desk_assignments.org_id` 是 CASCADE。
//!
//! 所以這兩支 DELETE 設 `deleted_at`。既有的 `list`／`get` 都已經帶
//! `deleted_at IS NULL`（量過），所以軟刪除真的會讓那一列消失。
//!
//! # 有阻擋物就回 409，並把數字說出來
//!
//! 「刪不掉」有好幾種原因，而它們要做的事完全不同：先搬走子節點、先停用可預約
//! 資源、先結掉工單。只回一個 409 不帶內容的話，呼叫者只能一個一個猜。
//!
//! 子節點是硬性阻擋：一棵樹如果中間少一層，所有帶 `deleted_at IS NULL` 的
//! 讀取都會看到斷開的兩段，而 ltree 的路徑仍然寫著原來的祖先 —— 那比拒絕更難
//! 收拾。
//!
//! # PATCH 可以搬移，因為觸發器會重編整棵子樹
//!
//! 001／003 的觸發器在 `parent_id` 或 `code` 變動時，會用一個 UPDATE 把整個
//! 子樹的路徑重編。**而 migration 069 才補上了循環守衛** —— 在那之前，把一個
//! 節點搬到自己的後代底下不會報錯，只會讓兩者互為祖先而 `<@` 從此回錯的答案。
//! 那個守衛在觸發器裡，所以這裡不需要（也不該）再實作一次；handler 只把
//! `TREE_CYCLE` 翻成 422。

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, FieldError, Problem, ProblemCode};

use crate::dto::{OrganizationDto, SpatialNodeDto};
use crate::handlers::TenancyState;
use crate::repo;

// =============================================================================
// organizations/{id}
// =============================================================================

/// `GET /organizations/{organizationId}`
pub async fn get_org(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<OrganizationDto>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "organization:read", None, None).await?;
    let row = repo::get_org(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這個組織"))?;
    tx.commit().await?;
    Ok(Json(crate::handlers::org_to_dto(row)))
}

/// 可改的欄位。
///
/// **`code` 與 `parent_id` 都可以改** —— 觸發器會重編整棵子樹的路徑，而 069
/// 擋掉了會造成循環的搬移。`org_path`／`depth` 不可改：那兩個是觸發器算出來的，
/// 手動指定會讓它們與 `parent_id` 不一致。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchOrgRequest {
    pub code: Option<String>,
    pub name: Option<String>,
    pub org_type: Option<String>,
    pub status: Option<String>,
    /// 外層 `None` = 沒提供（不動）；內層 `None` = 設為 NULL（升為根節點）。
    #[serde(default, with = "double_option")]
    pub parent_id: Option<Option<Uuid>>,
    #[serde(default, with = "double_option")]
    pub cost_center: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    pub manager_user_id: Option<Option<Uuid>>,
}

pub(crate) mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::deserialize(d).map(Some)
    }
}

fn split<T>(v: Option<Option<T>>) -> (bool, Option<T>) {
    match v {
        None => (false, None),
        Some(inner) => (true, inner),
    }
}

const ORG_TYPES: &[&str] = &[
    "GROUP",
    "COMPANY",
    "DIVISION",
    "DEPARTMENT",
    "TEAM",
    "COST_CENTER",
];
const ORG_STATUSES: &[&str] = &["ACTIVE", "INACTIVE"];

/// `PATCH /organizations/{organizationId}`
pub async fn patch_org(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Json<serde_json::Value>,
) -> Result<Json<OrganizationDto>, Problem> {
    let obj = body
        .0
        .as_object()
        .ok_or_else(|| Problem::validation("請求體必須是一個 JSON 物件"))?;
    if obj.is_empty() {
        return Err(Problem::validation("沒有要更新的欄位"));
    }
    // `org_path`／`depth` 是觸發器算出來的。手動指定會讓它們與 `parent_id`
    // 不一致，而之後每一次子樹查詢都以那個不一致為基準。
    for f in ["org_path", "depth", "id", "tenant_id"] {
        if obj.contains_key(f) {
            return Err(
                Problem::validation(format!("`{f}` 不可指定")).with_errors(vec![FieldError {
                    pointer: format!("/{f}"),
                    code: "DERIVED".to_string(),
                    message: if f == "org_path" || f == "depth" {
                        "由觸發器從 parent_id 與 code 算出。手動指定會讓它與樹的\
                         實際結構不一致，而子樹查詢會以那個不一致為基準"
                            .to_string()
                    } else {
                        "識別欄位不可變更".to_string()
                    },
                }]),
            );
        }
    }

    let req: PatchOrgRequest = serde_json::from_value(body.0.clone()).map_err(|e| {
        Problem::validation(format!("不認識的欄位或型別不符：{e}")).with_errors(vec![FieldError {
            pointer: "/".to_string(),
            code: "UNKNOWN_FIELD".to_string(),
            message: e.to_string(),
        }])
    })?;

    if let Some(t) = &req.org_type {
        if !ORG_TYPES.contains(&t.as_str()) {
            return Err(Problem::validation(format!(
                "`org_type` 必須是 {ORG_TYPES:?} 之一"
            )));
        }
    }
    if let Some(s) = &req.status {
        if !ORG_STATUSES.contains(&s.as_str()) {
            return Err(Problem::validation(format!(
                "`status` 必須是 {ORG_STATUSES:?} 之一"
            )));
        }
    }
    if let Some(c) = &req.code {
        if c.trim().is_empty() || c.len() > 50 {
            return Err(Problem::validation("`code` 長度必須是 1–50"));
        }
    }

    let (parent_given, parent) = split(req.parent_id);
    let (cc_given, cc) = split(req.cost_center);
    let (mgr_given, mgr) = split(req.manager_user_id);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "organization:write", None, None).await?;

    let updated = sqlx::query_scalar!(
        r#"UPDATE fms.organizations SET
             code            = coalesce($2, code),
             name            = coalesce($3, name),
             org_type        = coalesce($4, org_type),
             status          = coalesce($5, status),
             parent_id       = CASE WHEN $6 THEN $7 ELSE parent_id END,
             cost_center     = CASE WHEN $8 THEN $9 ELSE cost_center END,
             manager_user_id = CASE WHEN $10 THEN $11 ELSE manager_user_id END,
             updated_at = clock_timestamp()
           WHERE id = $1 AND deleted_at IS NULL
           RETURNING id"#,
        id,
        req.code,
        req.name,
        req.org_type,
        req.status,
        parent_given,
        parent,
        cc_given,
        cc,
        mgr_given,
        mgr,
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(map_tree_violation)?;

    if updated.is_none() {
        return Err(Problem::not_found("找不到這個組織"));
    }

    let row = repo::get_org(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("organization vanished")))?;
    tx.commit().await?;
    Ok(Json(crate::handlers::org_to_dto(row)))
}

/// `DELETE /organizations/{organizationId}` —— 軟刪除。
///
/// 有子組織或設施時回 409 並把數字說出來：那兩件事要做的處理完全不同
/// （先搬子組織／先把設施移到別的組織）。
pub async fn delete_org(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "organization:write", None, None).await?;

    let counts = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM fms.organizations c
                    WHERE c.parent_id = $1 AND c.deleted_at IS NULL) AS "children!",
                  (SELECT count(*) FROM fms.facilities f
                    WHERE f.org_id = $1 AND f.deleted_at IS NULL) AS "facilities!",
                  (SELECT count(*) FROM fms.users u
                    WHERE u.primary_org_id = $1) AS "users!",
                  EXISTS (SELECT 1 FROM fms.organizations o
                           WHERE o.id = $1 AND o.deleted_at IS NULL) AS "exists!""#,
        id
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;

    if !counts.exists {
        return Err(Problem::not_found("找不到這個組織"));
    }
    if counts.children > 0 || counts.facilities > 0 {
        return Err(Problem::new(ProblemCode::Conflict)
            .with_detail(format!(
                "還有 {} 個子組織與 {} 個設施掛在這個組織上。\
                 子組織要先搬走（樹中間少一層會讓子樹查詢看到斷開的兩段），\
                 設施要先移到別的組織",
                counts.children, counts.facilities
            ))
            .with_errors(vec![
                FieldError {
                    pointer: "/child_organizations".to_string(),
                    code: "HAS_CHILDREN".to_string(),
                    message: counts.children.to_string(),
                },
                FieldError {
                    pointer: "/facilities".to_string(),
                    code: "HAS_FACILITIES".to_string(),
                    message: counts.facilities.to_string(),
                },
            ]));
    }

    sqlx::query!(
        "UPDATE fms.organizations SET deleted_at = clock_timestamp(),
                updated_at = clock_timestamp()
          WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": { "id": id, "deleted": true },
        "meta": {
            "soft_delete": true,
            // 軟刪除之後還有什麼指著它。`users.primary_org_id` 的外鍵是
            // SET NULL，所以硬刪會靜默地把那些人的所屬組織清掉。
            "users_still_referencing": counts.users,
            "why_soft": "desk_assignments.org_id 是 CASCADE、users.primary_org_id 是 SET NULL —— 硬刪會靜默地改動別的資料",
        },
    })))
}

// =============================================================================
// spatial-nodes/{id}
// =============================================================================

/// `GET /spatial-nodes/{nodeId}`
pub async fn get_node(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<SpatialNodeDto>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 場域從那一列讀出來 —— 節點的路徑上沒有場域，而權限是 FACILITY 範圍。
    // RLS 已經把不可見的列排除，所以查不到就是 404。
    let row = repo::get_node(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這個空間節點"))?;
    require_permission(&mut tx, "spatial_node:read", Some(row.facility_id), None).await?;
    tx.commit().await?;
    Ok(Json(crate::handlers::node_to_dto(row)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchNodeRequest {
    pub code: Option<String>,
    pub name: Option<String>,
    pub node_type_code: Option<String>,
    pub capacity: Option<i32>,
    pub is_bookable: Option<bool>,
    pub is_active: Option<bool>,
    #[serde(default, with = "double_option")]
    pub parent_id: Option<Option<Uuid>>,
    #[serde(default, with = "double_option")]
    pub floor_level: Option<Option<i32>>,
    #[serde(default, with = "double_option")]
    pub floor_label: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    pub area_sqm: Option<Option<f64>>,
    #[serde(default, with = "double_option")]
    pub bim_element_id: Option<Option<String>>,
}

/// `PATCH /spatial-nodes/{nodeId}`
///
/// **`facility_id` 不可變更。** 節點的路徑是相對於場域根節點的，而 003 的
/// re-path 觸發器用 `WHERE facility_id = NEW.facility_id` 找子樹 —— 換場域會讓
/// 子節點留在舊場域而父節點跑到新場域，兩邊的路徑都不再一致。
pub async fn patch_node(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Json<serde_json::Value>,
) -> Result<Json<SpatialNodeDto>, Problem> {
    let obj = body
        .0
        .as_object()
        .ok_or_else(|| Problem::validation("請求體必須是一個 JSON 物件"))?;
    if obj.is_empty() {
        return Err(Problem::validation("沒有要更新的欄位"));
    }
    for f in ["node_path", "depth", "facility_id", "id", "tenant_id"] {
        if obj.contains_key(f) {
            return Err(
                Problem::validation(format!("`{f}` 不可指定")).with_errors(vec![FieldError {
                    pointer: format!("/{f}"),
                    code: "DERIVED".to_string(),
                    message: match f {
                        "node_path" | "depth" => "由觸發器從 parent_id 與 code 算出".to_string(),
                        "facility_id" => "節點的路徑是相對於場域根節點的；\
                             換場域會讓子節點留在舊場域而父節點跑到新場域"
                            .to_string(),
                        _ => "識別欄位不可變更".to_string(),
                    },
                }]),
            );
        }
    }

    let req: PatchNodeRequest = serde_json::from_value(body.0.clone()).map_err(|e| {
        Problem::validation(format!("不認識的欄位或型別不符：{e}")).with_errors(vec![FieldError {
            pointer: "/".to_string(),
            code: "UNKNOWN_FIELD".to_string(),
            message: e.to_string(),
        }])
    })?;

    if req.capacity.is_some_and(|v| v < 0) {
        return Err(Problem::validation("`capacity` 不得為負"));
    }
    if let Some(c) = &req.code {
        if c.trim().is_empty() || c.len() > 60 {
            return Err(Problem::validation("`code` 長度必須是 1–60"));
        }
    }

    let (parent_given, parent) = split(req.parent_id);
    let (fl_given, fl) = split(req.floor_level);
    let (fla_given, fla) = split(req.floor_label);
    let (area_given, area) = split(req.area_sqm);
    let (bim_given, bim) = split(req.bim_element_id);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let existing = repo::get_node(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這個空間節點"))?;
    require_permission(
        &mut tx,
        "spatial_node:write",
        Some(existing.facility_id),
        None,
    )
    .await?;

    // 型別碼必須存在 —— 那一欄沒有外鍵（003 用的是 varchar + 目錄表），
    // 所以打錯字會寫進去而沒有人擋。既有的 `create_node` 也做同一個檢查。
    if let Some(t) = &req.node_type_code {
        if !repo::node_type_exists(&mut tx, t).await? {
            return Err(
                Problem::validation(format!("`node_type_code` `{t}` 不存在")).with_errors(vec![
                    FieldError {
                        pointer: "/node_type_code".to_string(),
                        code: "NOT_FOUND".to_string(),
                        message: "見 GET /spatial-node-types".to_string(),
                    },
                ]),
            );
        }
    }

    sqlx::query!(
        r#"UPDATE fms.spatial_nodes SET
             code           = coalesce($2, code),
             name           = coalesce($3, name),
             node_type_code = coalesce($4, node_type_code),
             capacity       = coalesce($5, capacity),
             is_bookable    = coalesce($6, is_bookable),
             is_active      = coalesce($7, is_active),
             parent_id      = CASE WHEN $8 THEN $9 ELSE parent_id END,
             floor_level    = CASE WHEN $10 THEN $11 ELSE floor_level END,
             floor_label    = CASE WHEN $12 THEN $13 ELSE floor_label END,
             area_sqm       = CASE WHEN $14 THEN $15::float8::numeric ELSE area_sqm END,
             bim_element_id = CASE WHEN $16 THEN $17 ELSE bim_element_id END,
             updated_at = clock_timestamp()
           WHERE id = $1 AND deleted_at IS NULL"#,
        id,
        req.code,
        req.name,
        req.node_type_code,
        req.capacity,
        req.is_bookable,
        req.is_active,
        parent_given,
        parent,
        fl_given,
        fl,
        fla_given,
        fla,
        area_given,
        area,
        bim_given,
        bim,
    )
    .execute(tx.conn())
    .await
    .map_err(map_tree_violation)?;

    let row = repo::get_node(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("spatial node vanished")))?;
    tx.commit().await?;
    Ok(Json(crate::handlers::node_to_dto(row)))
}

/// `DELETE /spatial-nodes/{nodeId}` —— 軟刪除。
///
/// 子節點、未結工單、啟用中的可預約資源都是阻擋物，而三者要做的處理完全不同。
/// 只回一個 409 不帶內容的話，呼叫者只能一個一個猜。
pub async fn delete_node(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let existing = repo::get_node(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這個空間節點"))?;
    require_permission(
        &mut tx,
        "spatial_node:write",
        Some(existing.facility_id),
        None,
    )
    .await?;

    let counts = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM fms.spatial_nodes c
                    WHERE c.parent_id = $1 AND c.deleted_at IS NULL) AS "children!",
                  (SELECT count(*) FROM fms.work_orders w
                    LEFT JOIN fms.work_order_statuses st ON st.code = w.status
                    WHERE w.spatial_node_id = $1 AND w.deleted_at IS NULL
                      AND st.is_terminal IS NOT TRUE) AS "open_work_orders!",
                  (SELECT count(*) FROM fms.bookable_resources br
                    WHERE br.spatial_node_id = $1 AND br.is_bookable) AS "bookable!",
                  (SELECT count(*) FROM fms.assets a
                    WHERE a.spatial_node_id = $1 AND a.deleted_at IS NULL) AS "assets!",
                  (SELECT count(*) FROM fms.maintenance_plans mp
                    WHERE mp.spatial_node_id = $1) AS "maintenance_plans!""#,
        id
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;

    if counts.children > 0 || counts.open_work_orders > 0 || counts.bookable > 0 {
        return Err(Problem::new(ProblemCode::Conflict)
            .with_detail(format!(
                "還有 {} 個子節點、{} 張未結工單、{} 個啟用中的可預約資源。\
                 子節點要先搬走或刪掉，工單要先結案，可預約資源要先停用",
                counts.children, counts.open_work_orders, counts.bookable
            ))
            .with_errors(vec![
                FieldError {
                    pointer: "/children".to_string(),
                    code: "HAS_CHILDREN".to_string(),
                    message: counts.children.to_string(),
                },
                FieldError {
                    pointer: "/open_work_orders".to_string(),
                    code: "HAS_OPEN_WORK_ORDERS".to_string(),
                    message: counts.open_work_orders.to_string(),
                },
                FieldError {
                    pointer: "/bookable_resources".to_string(),
                    code: "HAS_BOOKABLE_RESOURCE".to_string(),
                    message: counts.bookable.to_string(),
                },
            ]));
    }

    sqlx::query!(
        "UPDATE fms.spatial_nodes SET deleted_at = clock_timestamp(),
                is_active = false, updated_at = clock_timestamp()
          WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": { "id": id, "deleted": true },
        "meta": {
            "soft_delete": true,
            // 這些還指著它。**軟刪除的重點就是它們沒有被連帶毀掉** ——
            // `maintenance_plans.spatial_node_id` 的外鍵是 CASCADE，
            // 硬刪會讓保養計畫連同它的排程一起消失，而回應只會是一個 204。
            "assets_still_referencing": counts.assets,
            "maintenance_plans_still_referencing": counts.maintenance_plans,
            "why_soft": "bookable_resources／desk_assignments／maintenance_plans／visitor_access_grants 四個外鍵都是 CASCADE",
        },
    })))
}

/// 把 069 的循環守衛（`HINT = TREE_CYCLE`）翻成 422，其餘照原樣。
///
/// 那個守衛在觸發器裡而不是這裡 —— 理由見 069 的檔頭：re-path 本身就是觸發器
/// 做的，任何寫入者都會經過它。handler 只負責讓錯誤訊息對客戶端有意義。
fn map_tree_violation(e: sqlx::Error) -> Problem {
    if let Some(db) = e.as_database_error() {
        let msg = db.message().to_string();
        if msg.contains("cycle") {
            return Problem::validation(
                "不能把節點搬到它自己的後代底下 —— 那會讓兩者互為祖先，\
                 而所有子樹查詢（含集團彙總報表）從此回錯的答案",
            )
            .with_errors(vec![FieldError {
                pointer: "/parent_id".to_string(),
                code: "TREE_CYCLE".to_string(),
                message: msg,
            }]);
        }
        if msg.contains("its own parent") {
            return Problem::validation("不能把節點設成自己的父節點").with_errors(vec![
                FieldError {
                    pointer: "/parent_id".to_string(),
                    code: "SELF_PARENT".to_string(),
                    message: msg,
                },
            ]);
        }
        if msg.contains("not found") {
            return Problem::validation("指定的父節點不存在").with_errors(vec![FieldError {
                pointer: "/parent_id".to_string(),
                code: "NOT_FOUND".to_string(),
                message: msg,
            }]);
        }
    }
    Problem::from(e)
}
