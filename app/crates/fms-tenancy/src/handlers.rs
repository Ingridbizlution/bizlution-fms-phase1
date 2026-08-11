//! 組織／場域／空間節點端點。契約中的八支：
//! `GET/POST /organizations`、`GET/POST /facilities`、
//! `GET/PATCH /facilities/{facilityId}`、
//! `GET/POST /facilities/{facilityId}/spatial-nodes`。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_permission, require_tenant_scoped_permission,
    Caller, Cursor, PageMeta, Problem, SortSpec,
};

use crate::dto::*;
use crate::repo;

#[derive(Clone)]
pub struct TenancyState {
    pub pool: PgPool,
}

const ORG_TYPES: &[&str] = &[
    "GROUP",
    "COMPANY",
    "BUSINESS_UNIT",
    "REGION",
    "DEPARTMENT",
    "TEAM",
];

/// `ltree` 的標籤只允許 `[A-Za-z0-9_]`。觸發器會把其他字元換成 `_`，
/// 也就是說 `R-401` 與 `R_401` 會產生**相同的路徑標籤**。
///
/// 因此在應用層先擋：否則兩個看起來不同的 code 會撞上
/// `uq_spatial_nodes_path`（`facility_id, node_path` 唯一），
/// 而錯誤訊息會指向一個使用者沒打過的字串。
fn validate_ltree_label(code: &str, field: &str) -> Result<(), Problem> {
    if code.is_empty() {
        return Err(Problem::validation(format!("{field} must not be empty")));
    }
    if !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Problem::validation(format!(
            "{field} `{code}` may only contain letters, digits and underscore: \
             the ltree path label is derived from it, and other characters are \
             replaced with `_`, which would silently collide with a different code"
        )));
    }
    Ok(())
}

pub(crate) fn org_to_dto(o: repo::OrganizationRow) -> OrganizationDto {
    OrganizationDto {
        id: o.id,
        parent_id: o.parent_id,
        code: o.code,
        name: o.name,
        org_type: o.org_type,
        org_path: o.org_path,
        depth: o.depth,
        cost_center: o.cost_center,
        facility_count: o.facility_count,
        status: o.status,
    }
}

/// `GET /organizations`
pub async fn list_orgs(
    State(state): State<TenancyState>,
    caller: Caller,
    Query(q): Query<OrgQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "organization:read", None, None).await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "org_path".to_string(),
        desc: false,
    };
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };
    let rows = repo::list_orgs(&mut tx, cursor.as_ref(), limit).await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.org_path.clone(), r.id));
    let data: Vec<OrganizationDto> = paged.data.into_iter().map(org_to_dto).collect();
    Ok(Json(serde_json::json!({
        "data": data,
        "page": PageMeta { next_cursor: paged.page.next_cursor, limit: paged.page.limit,
                           total_estimate: None },
    })))
}

/// `POST /organizations`
pub async fn create_org(
    State(state): State<TenancyState>,
    caller: Caller,
    Json(w): Json<OrganizationCreate>,
) -> Result<(StatusCode, Json<OrganizationDto>), Problem> {
    let code = w
        .code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("code is required"))?;
    let name = w
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("name is required"))?;
    let org_type = w
        .org_type
        .as_deref()
        .ok_or_else(|| Problem::validation("org_type is required"))?;
    if !ORG_TYPES.contains(&org_type) {
        return Err(Problem::validation(format!(
            "invalid org_type `{org_type}`; allowed: {ORG_TYPES:?}"
        )));
    }
    validate_ltree_label(code, "code")?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 授權範圍以 **parent_id** 判定，而不是 `None`。
    //
    // 026 把 `organization:write` 宣告成 ORG，讓組織經理能在自己的子樹內建子組織。
    // 但「ORG 範圍」還必須被限制在**哪一棵**子樹，否則組織經理可以建立自己範圍外
    // 的組織 —— 那是逃出自己的範圍，比原本的缺口更難察覺。
    //
    // 這不需要新程式碼：016 的 `user_permission_codes` 的 ORG 分支比對的是
    // `o_target.org_path <@ o_scope.org_path`，把 parent 當成 `org_id` 傳進去，
    // 述詞就正好回答「你的授權範圍涵蓋這個位置嗎」。範圍判定因此仍然只有一份
    // （ADR-09 紀律 2），不會在 handler 裡長出第二套 ltree 邏輯。
    //
    // 建立**根組織**（`parent_id` 為 None）走另一支判定，不能只是傳 None：
    // `require_permission(.., None, None)` 會落到 `user_permission_codes_anywhere`
    // ——「在任一範圍持有」—— 那正好跳過我們要的範圍檢查。根組織不落在任何
    // ORG 子樹內，因此正確語意是「必須是 TENANT 範圍的授權」。
    //
    // 一個刻意接受的不對稱：parent 不存在時，TENANT 範圍的呼叫者會走到下面的
    // 422「parent 不存在」，ORG 範圍的呼叫者在這裡就得到 403。後者其實更精確
    // —— 他對一個不存在的位置確實沒有授權 —— 且不洩漏該 id 是否存在。
    match w.parent_id {
        Some(parent) => require_permission(&mut tx, "organization:write", None, Some(parent)).await,
        None => require_tenant_scoped_permission(&mut tx, "organization:write").await,
    }?;

    // parent 必須存在且同租戶。RLS 已保證同租戶，這裡只查存在性 ——
    // 觸發器雖然也會擋（RAISE 23503），但那個路徑回的是 500。
    if let Some(parent) = w.parent_id {
        if repo::get_org(&mut tx, parent).await?.is_none() {
            return Err(Problem::validation(format!(
                "parent organization {parent} does not exist"
            )));
        }
    }

    let id = repo::create_org(
        &mut tx,
        repo::NewOrg {
            parent_id: w.parent_id,
            code,
            name,
            org_type,
            cost_center: w.cost_center.as_deref(),
            manager_user_id: w.manager_user_id,
            attributes: w.attributes.as_ref(),
        },
    )
    .await?;
    let created = repo::get_org(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("organization vanished")))?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(org_to_dto(created))))
}

/// 契約的 `Facility.version`，但 `fms.facilities` 沒有 `version` 欄位。
///
/// 以 `updated_at` 的秒級 epoch 代替，讓契約欄位有一個**單調遞增**的值。
/// 刻意**不**把它接到 `If-Match`：那會宣稱有樂觀鎖而實際上沒有 ——
/// 同一秒內的兩次更新會拿到相同的版本號，衝突就偵測不到。
/// 真正的解法是給 `facilities` 加 `version` 欄位與 `trg_bump_version`
/// （assets／work_orders 都有），那是一次 migration，見 4.1r。
fn facility_version(updated_at: chrono::DateTime<chrono::Utc>) -> i64 {
    updated_at.timestamp()
}

fn facility_to_dto(f: repo::FacilityRow) -> FacilityDto {
    FacilityDto {
        id: f.id,
        org_id: f.org_id,
        code: f.code,
        name: f.name,
        facility_type: f.facility_type,
        address_line1: f.address_line1,
        city: f.city,
        country_code: f.country_code,
        timezone: f.timezone,
        latitude: f.latitude,
        longitude: f.longitude,
        gross_area_sqm: f.gross_area_sqm,
        operating_hours: f.operating_hours,
        status: f.status,
        version: facility_version(f.updated_at),
    }
}

/// `GET /facilities`
pub async fn list_facilities(
    State(state): State<TenancyState>,
    caller: Caller,
    Query(q): Query<FacilityQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "facility:read", None, None).await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "code".to_string(),
        desc: false,
    };
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };
    let rows = repo::list_facilities(
        &mut tx,
        q.org_id,
        q.status.as_deref(),
        cursor.as_ref(),
        limit,
    )
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.code.clone(), r.id));
    let data: Vec<FacilityDto> = paged.data.into_iter().map(facility_to_dto).collect();
    Ok(Json(serde_json::json!({
        "data": data,
        "page": PageMeta { next_cursor: paged.page.next_cursor, limit: paged.page.limit,
                           total_estimate: None },
    })))
}

/// `GET /facilities/{facilityId}`
pub async fn get_facility(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<FacilityDto>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let row = repo::get_facility(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("facility not found"))?;
    require_permission(&mut tx, "facility:read", Some(id), None).await?;
    tx.commit().await?;
    Ok(Json(facility_to_dto(row)))
}

/// `POST /facilities`
pub async fn create_facility(
    State(state): State<TenancyState>,
    caller: Caller,
    Json(w): Json<FacilityWrite>,
) -> Result<(StatusCode, Json<FacilityDto>), Problem> {
    let org_id = w
        .org_id
        .ok_or_else(|| Problem::validation("org_id is required"))?;
    let code = w
        .code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("code is required"))?
        .to_string();
    let name = w
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("name is required"))?
        .to_string();
    if let Some(c) = w.country_code.as_deref() {
        if c.len() != 2 {
            return Err(Problem::validation(
                "country_code must be exactly 2 characters (ISO 3166-1 alpha-2)",
            ));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // `facility:create`（027 起，宣告 ORG），範圍以 **org_id** 判定。
    //
    // 建立場域是租戶／組織級動作：新場域沒有父場域，「在哪個場域裡建立一個場域」
    // 不成立。027 之前這裡用的是 facility:write（宣告 FACILITY）配 `None`，
    // 也就是「在任一範圍持有即可」—— 場域範圍的角色因此通得過權限檢查。
    //
    // 當時之所以還是回 403，是 007 的 facility_scope 政策的副產品：新場域的 id
    // 不在可見快照裡，下面 `get_facility` 讀不回來就回滾。行為對，理由不對；
    // 任何人日後調整那條政策，保護就無聲失效。現在 403 由權限判定給出，
    // RLS 退回它該扮演的第二道防線。
    //
    // 傳 `org_id` 的作用與 create_org 相同：ORG 範圍的授權只能在自己的組織
    // 子樹內建立場域（見 create_org 的說明）。
    require_permission(&mut tx, "facility:create", None, Some(org_id)).await?;

    // 組織必須存在。外鍵也會擋，但那條路徑回 500。
    if repo::get_org(&mut tx, org_id).await?.is_none() {
        return Err(Problem::validation(format!(
            "organization {org_id} does not exist"
        )));
    }

    let id = Uuid::new_v4();
    repo::create_facility(&mut tx, id, org_id, &w, &code, &name).await?;

    // 新場域不在交易開始時取的可見場域快照裡，因此必須重算，
    // 否則連讀回自己剛建立的那一列都會被 RLS 擋掉。
    fms_shared::refresh_facility_scope(&mut tx).await?;

    let created = match repo::get_facility(&mut tx, id).await? {
        Some(row) => row,
        None => {
            // 重算後仍看不到 —— 代表建立者的角色範圍不涵蓋這個新場域。
            // 此時**不提交**：「建立了一個自己看不到的東西」是比失敗更糟的結果，
            // 而回滾讓狀態保持乾淨、訊息也可行動。
            //
            // 027 之後這裡是**第二道防線**而非主要守衛：場域範圍的角色已經
            // 在上面的 `facility:create` 判定就被擋掉了。仍然留著，因為兩邊
            // 的範圍展開雖然用同一條 ltree 述詞，有效期間的判定卻不完全相同
            // （`user_accessible_facilities` 只看 valid_until，
            //  `v_user_effective_permissions` 同時看 valid_from），
            // 而「悄悄建立一列自己看不到的資料」不該有任何一條路徑能達成。
            return Err(Problem::permission_denied(
                "the facility was not created: your role scope does not cover it. \
                 Creating a facility requires a TENANT- or ORG-scoped role; \
                 ask an administrator to widen your scope or to create it for you",
            ));
        }
    };
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(facility_to_dto(created))))
}

/// `PATCH /facilities/{facilityId}`
///
/// 契約列了 `If-Match` 於其他端點，但**這一支沒有** —— 與
/// `fms.facilities` 沒有 `version` 欄位一致。因此不檢查樂觀鎖，
/// 也不假裝有（見 `facility_version`）。
pub async fn update_facility(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(w): Json<FacilityWrite>,
) -> Result<Json<FacilityDto>, Problem> {
    if let Some(c) = w.country_code.as_deref() {
        if c.len() != 2 {
            return Err(Problem::validation(
                "country_code must be exactly 2 characters (ISO 3166-1 alpha-2)",
            ));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    if repo::get_facility(&mut tx, id).await?.is_none() {
        return Err(Problem::not_found("facility not found"));
    }
    require_permission(&mut tx, "facility:update", Some(id), None).await?;

    if let Some(org_id) = w.org_id {
        if repo::get_org(&mut tx, org_id).await?.is_none() {
            return Err(Problem::validation(format!(
                "organization {org_id} does not exist"
            )));
        }
    }

    repo::update_facility(&mut tx, id, &w).await?;
    let updated = repo::get_facility(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("facility not found"))?;
    tx.commit().await?;
    Ok(Json(facility_to_dto(updated)))
}

pub(crate) fn node_to_dto(n: repo::SpatialNodeRow) -> SpatialNodeDto {
    SpatialNodeDto {
        id: n.id,
        facility_id: n.facility_id,
        parent_id: n.parent_id,
        node_type_code: n.node_type_code,
        code: n.code,
        name: n.name,
        node_path: n.node_path,
        depth: n.depth,
        floor_level: n.floor_level,
        floor_label: n.floor_label,
        area_sqm: n.area_sqm,
        capacity: n.capacity,
        is_bookable: n.is_bookable,
        status: n.status,
        health_score: n.health_score,
        utilization_pct: n.utilization_pct,
        bim_element_id: n.bim_element_id,
        asset_count: n.asset_count,
        open_work_order_count: n.open_work_order_count,
        children: None,
    }
}

/// 把扁平清單依 `parent_id` 組成樹（契約的 `view=tree`）。
///
/// # 為什麼在應用層組樹而不是遞迴查詢
///
/// 查詢已經回了一整棵（依 `node_path` 排序的）子樹，組樹是純粹的形狀轉換。
/// 用 `WITH RECURSIVE` 再走一次只是重複資料庫已經做完的事。
///
/// 「父節點不在結果集裡」的節點會被提升為根 —— 那在 `parent_id` 或
/// `subtree_of` 過濾下是**正常**的（子樹的根本身的父節點在集合外），
/// 靜默丟掉它們會讓 tree 視圖無故少一整塊。
fn build_tree(rows: Vec<repo::SpatialNodeRow>) -> Vec<SpatialNodeDto> {
    use std::collections::HashMap;

    let ids: std::collections::HashSet<Uuid> = rows.iter().map(|r| r.id).collect();
    let mut children_of: HashMap<Uuid, Vec<SpatialNodeDto>> = HashMap::new();
    let mut roots: Vec<SpatialNodeDto> = Vec::new();

    // 由深到淺處理：`node_path` 排序保證父在子之前，反向走就保證
    // 處理某個節點時它的所有子節點都已經組好了。
    for row in rows.into_iter().rev() {
        let id = row.id;
        let parent = row.parent_id;
        let mut dto = node_to_dto(row);
        dto.children = Some(children_of.remove(&id).unwrap_or_default());

        match parent {
            Some(p) if ids.contains(&p) => children_of.entry(p).or_default().insert(0, dto),
            _ => roots.insert(0, dto),
        }
    }
    roots
}

const NODE_VIEWS: &[&str] = &["flat", "tree"];

/// `GET /facilities/{facilityId}/spatial-nodes`
pub async fn list_nodes(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Query(q): Query<NodeQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let view = q.view.unwrap_or_else(|| "flat".to_string());
    if !NODE_VIEWS.contains(&view.as_str()) {
        return Err(Problem::validation(format!(
            "invalid view `{view}`; allowed: {NODE_VIEWS:?}"
        )));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "spatial_node:read", Some(facility_id), None).await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "node_path".to_string(),
        desc: false,
    };
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };
    let rows = repo::list_nodes(
        &mut tx,
        facility_id,
        q.parent_id,
        q.subtree_of,
        q.node_type_code.as_deref(),
        q.floor_level,
        q.bookable_only.unwrap_or(false),
        q.include_asset_counts.unwrap_or(false),
        cursor.as_ref(),
        limit,
    )
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.node_path.clone(), r.id));
    let next_cursor = paged.page.next_cursor;
    let data = if view == "tree" {
        // tree 視圖仍然分頁：一頁之內組樹。跨頁的樹要由客戶端自己接，
        // 而 `node_path` 讓那件事是可行的（父的路徑是子的前綴）。
        serde_json::to_value(build_tree(paged.data)).map_err(Problem::internal)?
    } else {
        serde_json::to_value(paged.data.into_iter().map(node_to_dto).collect::<Vec<_>>())
            .map_err(Problem::internal)?
    };

    Ok(Json(serde_json::json!({
        "data": data,
        "page": PageMeta { next_cursor, limit, total_estimate: None },
    })))
}

/// `POST /facilities/{facilityId}/spatial-nodes`
///
/// 路徑與深度都不由本層寫入：`trg_spatial_node_path` 從
/// `parent_id + code` 推導，改 parent 時還會重算整棵子樹。
pub async fn create_node(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Json(w): Json<SpatialNodeCreate>,
) -> Result<(StatusCode, Json<SpatialNodeDto>), Problem> {
    let node_type_code = w
        .node_type_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("node_type_code is required"))?
        .to_string();
    let code = w
        .code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("code is required"))?
        .to_string();
    let name = w
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("name is required"))?
        .to_string();
    validate_ltree_label(&code, "code")?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    if repo::get_facility(&mut tx, facility_id).await?.is_none() {
        return Err(Problem::not_found("facility not found"));
    }
    require_permission(&mut tx, "spatial_node:write", Some(facility_id), None).await?;

    if !repo::node_type_exists(&mut tx, &node_type_code).await? {
        return Err(Problem::validation(format!(
            "unknown node_type_code: {node_type_code}"
        )));
    }

    // 父節點必須存在**且在同一個場域**：跨場域的父子關係會讓
    // `node_path` 的唯一索引（facility_id, node_path）失去意義，
    // 而觸發器不檢查這一點。
    if let Some(parent) = w.parent_id {
        let p = repo::get_node(&mut tx, parent)
            .await?
            .ok_or_else(|| Problem::validation(format!("parent node {parent} does not exist")))?;
        if p.facility_id != facility_id {
            return Err(Problem::validation(
                "parent node belongs to a different facility",
            ));
        }
    }

    let id = repo::create_node(&mut tx, facility_id, &w, &node_type_code, &code, &name).await?;
    let created = repo::get_node(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("spatial node vanished")))?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(node_to_dto(created))))
}
