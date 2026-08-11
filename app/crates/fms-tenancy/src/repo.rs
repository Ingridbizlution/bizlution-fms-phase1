//! 組織／場域／空間節點的資料存取。
//!
//! 兩處刻意的分工：
//!   * **不寫 `org_path`／`node_path`／`depth`**：由 001／003 的觸發器
//!     從 `parent_id + code` 推導，且在搬移時重算整棵子樹。
//!   * `ltree` 以 `::text` 對外，因為 sqlx 沒有 ltree 的原生型別映射。

use uuid::Uuid;

use fms_shared::{Problem, TenantTx};

pub struct OrganizationRow {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub code: String,
    pub name: String,
    pub org_type: String,
    pub org_path: String,
    pub depth: i32,
    pub cost_center: Option<String>,
    pub facility_count: i64,
    pub status: String,
}

/// 列出組織。排序固定 `org_path`：那讓父節點必然排在子節點之前，
/// 前端可以一次線性掃過就組出樹，不必先全部收集再排序。
pub async fn list_orgs(
    tx: &mut TenantTx,
    cursor: Option<&fms_shared::Cursor>,
    limit: i64,
) -> Result<Vec<OrganizationRow>, Problem> {
    let (key, id) = match cursor {
        Some(c) => (Some(c.key.clone()), Some(c.uuid_id()?)),
        None => (None, None),
    };
    sqlx::query_as!(
        OrganizationRow,
        r#"
        SELECT o.id, o.parent_id, o.code::text AS "code!", o.name::text AS "name!",
               o.org_type, o.org_path::text AS "org_path!",
               -- depth 不是欄位：組織樹的深度由 ltree 的層數導出。
               (nlevel(o.org_path) - 1)::int AS "depth!",
               o.cost_center::text AS "cost_center",
               (SELECT count(*) FROM fms.facilities f
                 WHERE f.org_id = o.id AND f.deleted_at IS NULL) AS "facility_count!",
               o.status
        FROM fms.organizations o
        WHERE o.deleted_at IS NULL
          AND ($1::text IS NULL OR (o.org_path::text, o.id) > ($1::text, $2::uuid))
        ORDER BY o.org_path, o.id
        LIMIT $3
        "#,
        key,
        id,
        limit + 1
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

pub async fn get_org(tx: &mut TenantTx, id: Uuid) -> Result<Option<OrganizationRow>, Problem> {
    sqlx::query_as!(
        OrganizationRow,
        r#"
        SELECT o.id, o.parent_id, o.code::text AS "code!", o.name::text AS "name!",
               o.org_type, o.org_path::text AS "org_path!",
               (nlevel(o.org_path) - 1)::int AS "depth!",
               o.cost_center::text AS "cost_center",
               (SELECT count(*) FROM fms.facilities f
                 WHERE f.org_id = o.id AND f.deleted_at IS NULL) AS "facility_count!",
               o.status
        FROM fms.organizations o
        WHERE o.id = $1 AND o.deleted_at IS NULL
        "#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

pub struct NewOrg<'a> {
    pub parent_id: Option<Uuid>,
    pub code: &'a str,
    pub name: &'a str,
    pub org_type: &'a str,
    pub cost_center: Option<&'a str>,
    pub manager_user_id: Option<Uuid>,
    pub attributes: Option<&'a serde_json::Value>,
}

/// 建立組織。
///
/// `org_path` 不在 INSERT 欄位裡 —— 它是 NOT NULL，但 `trg_organization_path`
/// 是 BEFORE INSERT，因此觸發器會在約束檢查前填好。這是刻意依賴觸發器，
/// 不是遺漏。
pub async fn create_org(tx: &mut TenantTx, new: NewOrg<'_>) -> Result<Uuid, Problem> {
    let tenant_id = tx.context().tenant_id;
    sqlx::query_scalar!(
        r#"
        INSERT INTO fms.organizations
          (tenant_id, parent_id, code, name, org_type, cost_center,
           manager_user_id, attributes)
        VALUES ($1, $2, $3, $4, $5, $6, $7, coalesce($8, '{}'::jsonb))
        RETURNING id
        "#,
        tenant_id,
        new.parent_id,
        new.code,
        new.name,
        new.org_type,
        new.cost_center,
        new.manager_user_id,
        new.attributes,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

pub struct FacilityRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub code: String,
    pub name: String,
    pub facility_type: String,
    pub address_line1: Option<String>,
    pub city: Option<String>,
    pub country_code: String,
    pub timezone: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub gross_area_sqm: Option<f64>,
    pub operating_hours: serde_json::Value,
    pub status: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// SELECT 主體在 list 與 get 各一份（`query_as!` 需要字面值）。

pub async fn list_facilities(
    tx: &mut TenantTx,
    org_id: Option<Uuid>,
    status: Option<&str>,
    cursor: Option<&fms_shared::Cursor>,
    limit: i64,
) -> Result<Vec<FacilityRow>, Problem> {
    let (key, id) = match cursor {
        Some(c) => (Some(c.key.clone()), Some(c.uuid_id()?)),
        None => (None, None),
    };
    sqlx::query_as!(
        FacilityRow,
        r#"
        SELECT f.id, f.org_id, f.code::text AS "code!", f.name::text AS "name!",
               f.facility_type, f.address_line1, f.city::text AS "city",
               f.country_code::text AS "country_code!", f.timezone::text AS "timezone!",
               f.latitude::float8 AS "latitude", f.longitude::float8 AS "longitude",
               f.gross_area_sqm::float8 AS "gross_area_sqm",
               f.operating_hours AS "operating_hours!", f.status, f.updated_at
        FROM fms.facilities f
        WHERE f.deleted_at IS NULL
          AND ($1::uuid IS NULL OR f.org_id = $1)
          AND ($2::text IS NULL OR f.status = $2)
          AND ($3::text IS NULL OR (f.code::text, f.id) > ($3::text, $4::uuid))
        ORDER BY f.code, f.id
        LIMIT $5
        "#,
        org_id,
        status,
        key,
        id,
        limit + 1
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

pub async fn get_facility(tx: &mut TenantTx, id: Uuid) -> Result<Option<FacilityRow>, Problem> {
    sqlx::query_as!(
        FacilityRow,
        r#"
        SELECT f.id, f.org_id, f.code::text AS "code!", f.name::text AS "name!",
               f.facility_type, f.address_line1, f.city::text AS "city",
               f.country_code::text AS "country_code!", f.timezone::text AS "timezone!",
               f.latitude::float8 AS "latitude", f.longitude::float8 AS "longitude",
               f.gross_area_sqm::float8 AS "gross_area_sqm",
               f.operating_hours AS "operating_hours!", f.status, f.updated_at
        FROM fms.facilities f
        WHERE f.id = $1 AND f.deleted_at IS NULL
        "#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 建立場域。
///
/// # 為什麼 id 由應用層產生、且不用 `RETURNING`
///
/// `facilities` 有 RESTRICTIVE 的 `facility_scope` 政策，判定式是
/// `facility_in_scope(id)`。PostgreSQL 會對 `INSERT ... RETURNING` 的
/// 回傳列**套用 SELECT 側政策**，而剛建立的場域不在交易開始時取的
/// `app.facility_ids` 快照裡 —— 於是 `RETURNING` 會失敗，
/// 錯誤訊息還是誤導人的「new row violates row-level security policy」。
///
/// 因此改為應用層產生 uuid、INSERT 不帶 RETURNING，
/// 由呼叫端先 `refresh_facility_scope` 再讀回。
pub async fn create_facility(
    tx: &mut TenantTx,
    id: Uuid,
    org_id: Uuid,
    w: &crate::dto::FacilityWrite,
    code: &str,
    name: &str,
) -> Result<(), Problem> {
    let tenant_id = tx.context().tenant_id;
    sqlx::query!(
        r#"
        INSERT INTO fms.facilities
          (id, tenant_id, org_id, code, name, facility_type, address_line1, city,
           country_code, timezone, gross_area_sqm, operating_hours, attributes)
        VALUES ($13, $1, $2, $3, $4, coalesce($5, 'OFFICE'), $6, $7,
                coalesce($8, 'TW'), coalesce($9, 'Asia/Taipei'),
                $10::float8::numeric, coalesce($11, '{}'::jsonb),
                coalesce($12, '{}'::jsonb))
        "#,
        tenant_id,
        org_id,
        code,
        name,
        w.facility_type,
        w.address_line1,
        w.city,
        w.country_code,
        w.timezone,
        w.gross_area_sqm,
        w.operating_hours,
        w.attributes,
        id,
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

pub async fn update_facility(
    tx: &mut TenantTx,
    id: Uuid,
    w: &crate::dto::FacilityWrite,
) -> Result<(), Problem> {
    sqlx::query!(
        r#"
        UPDATE fms.facilities SET
          org_id         = coalesce($2, org_id),
          code           = coalesce($3, code),
          name           = coalesce($4, name),
          facility_type  = coalesce($5, facility_type),
          address_line1  = coalesce($6, address_line1),
          city           = coalesce($7, city),
          country_code   = coalesce($8, country_code),
          timezone       = coalesce($9, timezone),
          gross_area_sqm = coalesce($10::float8::numeric, gross_area_sqm),
          operating_hours = coalesce($11, operating_hours),
          attributes     = coalesce($12, attributes)
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id,
        w.org_id,
        w.code,
        w.name,
        w.facility_type,
        w.address_line1,
        w.city,
        w.country_code,
        w.timezone,
        w.gross_area_sqm,
        w.operating_hours,
        w.attributes,
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

pub struct SpatialNodeRow {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub node_type_code: String,
    pub code: String,
    pub name: String,
    pub node_path: String,
    pub depth: i16,
    pub floor_level: Option<i32>,
    pub floor_label: Option<String>,
    pub area_sqm: Option<f64>,
    pub capacity: i32,
    pub is_bookable: bool,
    pub status: String,
    pub health_score: Option<f64>,
    pub utilization_pct: Option<f64>,
    pub bim_element_id: Option<String>,
    pub asset_count: i64,
    pub open_work_order_count: i64,
}

/// 列出空間節點。
///
/// `include_asset_counts` 決定是否算兩個子查詢的計數。預設不算：
/// 側欄逐層瀏覽不需要它，而每個節點兩個 count 在深樹上是實際成本。
/// 不算時回 0 而非 null —— 契約宣告 `asset_count` 為 integer 非 nullable。
#[allow(clippy::too_many_arguments)]
pub async fn list_nodes(
    tx: &mut TenantTx,
    facility_id: Uuid,
    parent_id: Option<Uuid>,
    subtree_of: Option<Uuid>,
    node_type_code: Option<&str>,
    floor_level: Option<i32>,
    bookable_only: bool,
    with_counts: bool,
    cursor: Option<&fms_shared::Cursor>,
    limit: i64,
) -> Result<Vec<SpatialNodeRow>, Problem> {
    let (key, id) = match cursor {
        Some(c) => (Some(c.key.clone()), Some(c.uuid_id()?)),
        None => (None, None),
    };
    sqlx::query_as!(
        SpatialNodeRow,
        r#"
        SELECT n.id, n.facility_id, n.parent_id,
               n.node_type_code::text AS "node_type_code!",
               n.code::text AS "code!", n.name::text AS "name!",
               n.node_path::text AS "node_path!", n.depth,
               n.floor_level, n.floor_label::text AS "floor_label",
               n.area_sqm::float8 AS "area_sqm", n.capacity, n.is_bookable, n.status,
               n.health_score::float8 AS "health_score",
               n.utilization_pct::float8 AS "utilization_pct",
               n.bim_element_id::text AS "bim_element_id",
               CASE WHEN $7::bool THEN
                 (SELECT count(*) FROM fms.assets a
                   WHERE a.spatial_node_id = n.id AND a.deleted_at IS NULL)
               ELSE 0 END AS "asset_count!",
               CASE WHEN $7::bool THEN
                 (SELECT count(*) FROM fms.work_orders w
                   WHERE w.spatial_node_id = n.id AND w.deleted_at IS NULL
                     AND w.status NOT IN ('CLOSED','CANCELLED','REJECTED'))
               ELSE 0 END AS "open_work_order_count!"
        FROM fms.spatial_nodes n
        WHERE n.deleted_at IS NULL
          AND n.facility_id = $1
          AND ($2::uuid IS NULL OR n.parent_id = $2)
          AND ($3::uuid IS NULL OR n.node_path OPERATOR(public.<@)
                 (SELECT r.node_path FROM fms.spatial_nodes r WHERE r.id = $3))
          AND ($4::text IS NULL OR n.node_type_code = $4)
          AND ($5::int IS NULL OR n.floor_level = $5)
          AND (NOT $6::bool OR n.is_bookable)
          AND ($8::text IS NULL OR (n.node_path::text, n.id) > ($8::text, $9::uuid))
        ORDER BY n.node_path, n.id
        LIMIT $10
        "#,
        facility_id,
        parent_id,
        subtree_of,
        node_type_code,
        floor_level,
        bookable_only,
        with_counts,
        key,
        id,
        limit + 1
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

pub async fn get_node(tx: &mut TenantTx, id: Uuid) -> Result<Option<SpatialNodeRow>, Problem> {
    sqlx::query_as!(
        SpatialNodeRow,
        r#"
        SELECT n.id, n.facility_id, n.parent_id,
               n.node_type_code::text AS "node_type_code!",
               n.code::text AS "code!", n.name::text AS "name!",
               n.node_path::text AS "node_path!", n.depth,
               n.floor_level, n.floor_label::text AS "floor_label",
               n.area_sqm::float8 AS "area_sqm", n.capacity, n.is_bookable, n.status,
               n.health_score::float8 AS "health_score",
               n.utilization_pct::float8 AS "utilization_pct",
               n.bim_element_id::text AS "bim_element_id",
               0::bigint AS "asset_count!", 0::bigint AS "open_work_order_count!"
        FROM fms.spatial_nodes n
        WHERE n.id = $1 AND n.deleted_at IS NULL
        "#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 建立空間節點。`node_path` 與 `depth` 都不在欄位清單裡 ——
/// `trg_spatial_node_path` 是 BEFORE INSERT，會在 NOT NULL 檢查前填好。
pub async fn create_node(
    tx: &mut TenantTx,
    facility_id: Uuid,
    w: &crate::dto::SpatialNodeCreate,
    node_type_code: &str,
    code: &str,
    name: &str,
) -> Result<Uuid, Problem> {
    let tenant_id = tx.context().tenant_id;
    sqlx::query_scalar!(
        r#"
        INSERT INTO fms.spatial_nodes
          (tenant_id, facility_id, parent_id, node_type_code, code, name,
           floor_level, floor_label, area_sqm, capacity, is_bookable,
           bim_model_id, bim_element_id, geometry, attributes)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::float8::numeric,
                coalesce($10, 0), coalesce($11, false), $12, $13,
                coalesce($14, '{}'::jsonb), coalesce($15, '{}'::jsonb))
        RETURNING id
        "#,
        tenant_id,
        facility_id,
        w.parent_id,
        node_type_code,
        code,
        name,
        w.floor_level,
        w.floor_label,
        w.area_sqm,
        w.capacity,
        w.is_bookable,
        w.bim_model_id,
        w.bim_element_id,
        w.geometry,
        w.attributes,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 搬移節點（改 `parent_id`）。
///
/// 只改 `parent_id` 一個欄位，路徑重算完全交給 `trg_spatial_node_path`
/// —— 它會用 `subpath()` 把整棵子樹重新掛好。應用層若自己算，
/// 就會有第二份實作，而這正是最容易寫錯的一類 SQL。
pub async fn move_node(
    tx: &mut TenantTx,
    id: Uuid,
    new_parent: Option<Uuid>,
) -> Result<u64, Problem> {
    let done = sqlx::query!(
        "UPDATE fms.spatial_nodes SET parent_id = $2 WHERE id = $1 AND deleted_at IS NULL",
        id,
        new_parent
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(done.rows_affected())
}

/// 節點型別是否存在（`spatial_node_types` 是 catalog，含平台預設列）。
pub async fn node_type_exists(tx: &mut TenantTx, code: &str) -> Result<bool, Problem> {
    Ok(sqlx::query_scalar!(
        "SELECT true FROM fms.spatial_node_types WHERE code = $1",
        code
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .flatten()
    .unwrap_or(false))
}
