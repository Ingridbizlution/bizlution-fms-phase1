//! 維護計畫與排程占位（occurrence）的資料存取。

use uuid::Uuid;

use fms_shared::{Problem, TenantTx};

/// `MaintenancePlan` 的一列，外加展開排程需要的欄位。
pub struct PlanRow {
    pub id: Uuid,
    pub facility_id: Uuid,
    /// 場域時區，展開 RRULE 用。
    pub facility_timezone: String,
    pub code: String,
    pub name: String,
    pub template_id: Uuid,
    pub template_name: String,
    pub target_type: String,
    pub target_id: Uuid,
    pub target_label: Option<String>,
    pub trigger_type: String,
    pub rrule: Option<String>,
    pub meter_code: Option<String>,
    pub meter_threshold: Option<f64>,
    pub generate_lead_days: i16,
    /// 063 新增：完工容許窗，由管理者定義。
    pub completion_grace_days: i16,
    pub priority: String,
    pub assigned_team_id: Option<Uuid>,
    pub next_due_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_generated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// SELECT 主體在 list 與 get 中各一份（`query_as!` 需要字面值）。

/// 依 id 取計畫。
pub async fn get(tx: &mut TenantTx, id: Uuid) -> Result<Option<PlanRow>, Problem> {
    sqlx::query_as!(
        PlanRow,
        r#"
        SELECT p.id                  AS "id!",
               p.facility_id         AS "facility_id!",
               f.timezone::text      AS "facility_timezone!",
               p.code::text          AS "code!",
               p.name::text          AS "name!",
               p.template_id         AS "template_id!",
               t.name::text          AS "template_name!",
               CASE WHEN p.asset_id IS NOT NULL        THEN 'ASSET'
                    WHEN p.spatial_node_id IS NOT NULL THEN 'SPATIAL_NODE'
                    ELSE 'CATEGORY' END AS "target_type!",
               coalesce(p.asset_id, p.spatial_node_id, p.category_id) AS "target_id!",
               CASE WHEN p.asset_id IS NOT NULL
                      THEN (SELECT a.name::text FROM fms.assets a WHERE a.id = p.asset_id)
                    WHEN p.spatial_node_id IS NOT NULL
                      THEN (SELECT n.name::text FROM fms.spatial_nodes n
                             WHERE n.id = p.spatial_node_id)
                    ELSE (SELECT c.name::text FROM fms.asset_categories c
                           WHERE c.id = p.category_id) END AS "target_label",
               p.trigger_type        AS "trigger_type!",
               p.rrule,
               p.meter_code::text    AS "meter_code",
               p.meter_threshold::float8 AS "meter_threshold",
               p.generate_lead_days  AS "generate_lead_days!",
               p.completion_grace_days AS "completion_grace_days!",
               p.priority            AS "priority!",
               p.assigned_team_id,
               p.next_due_at,
               p.last_generated_at,
               p.is_active           AS "is_active!",
               p.created_at          AS "created_at!"
        FROM fms.maintenance_plans p
        JOIN fms.maintenance_templates t ON t.id = p.template_id
        JOIN fms.facilities f ON f.id = p.facility_id
        WHERE p.id = $1
        "#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 列出計畫。契約的過濾條件：`facility_id`、`trigger_type`、`due_before`。
/// 排序固定 `code`（契約無 `sort`），游標記下欄位。
pub async fn list(
    tx: &mut TenantTx,
    facility_id: Option<Uuid>,
    trigger_type: Option<&str>,
    due_before: Option<chrono::DateTime<chrono::Utc>>,
    cursor: Option<&fms_shared::Cursor>,
    limit: i64,
) -> Result<Vec<PlanRow>, Problem> {
    let (cursor_key, cursor_id) = match cursor {
        Some(c) => (Some(c.key.clone()), Some(c.uuid_id()?)),
        None => (None, None),
    };

    sqlx::query_as!(
        PlanRow,
        r#"
        SELECT p.id                  AS "id!",
               p.facility_id         AS "facility_id!",
               f.timezone::text      AS "facility_timezone!",
               p.code::text          AS "code!",
               p.name::text          AS "name!",
               p.template_id         AS "template_id!",
               t.name::text          AS "template_name!",
               CASE WHEN p.asset_id IS NOT NULL        THEN 'ASSET'
                    WHEN p.spatial_node_id IS NOT NULL THEN 'SPATIAL_NODE'
                    ELSE 'CATEGORY' END AS "target_type!",
               coalesce(p.asset_id, p.spatial_node_id, p.category_id) AS "target_id!",
               CASE WHEN p.asset_id IS NOT NULL
                      THEN (SELECT a.name::text FROM fms.assets a WHERE a.id = p.asset_id)
                    WHEN p.spatial_node_id IS NOT NULL
                      THEN (SELECT n.name::text FROM fms.spatial_nodes n
                             WHERE n.id = p.spatial_node_id)
                    ELSE (SELECT c.name::text FROM fms.asset_categories c
                           WHERE c.id = p.category_id) END AS "target_label",
               p.trigger_type        AS "trigger_type!",
               p.rrule,
               p.meter_code::text    AS "meter_code",
               p.meter_threshold::float8 AS "meter_threshold",
               p.generate_lead_days  AS "generate_lead_days!",
               p.completion_grace_days AS "completion_grace_days!",
               p.priority            AS "priority!",
               p.assigned_team_id,
               p.next_due_at,
               p.last_generated_at,
               p.is_active           AS "is_active!",
               p.created_at          AS "created_at!"
        FROM fms.maintenance_plans p
        JOIN fms.maintenance_templates t ON t.id = p.template_id
        JOIN fms.facilities f ON f.id = p.facility_id
        WHERE ($1::uuid IS NULL OR p.facility_id = $1)
          AND ($2::text IS NULL OR p.trigger_type = $2)
          AND ($3::timestamptz IS NULL
               OR (p.next_due_at IS NOT NULL AND p.next_due_at <= $3))
          AND ($4::text IS NULL OR (p.code::text, p.id) > ($4::text, $5::uuid))
        ORDER BY p.code, p.id
        LIMIT $6
        "#,
        facility_id,
        trigger_type,
        due_before,
        cursor_key,
        cursor_id,
        limit + 1
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

pub struct NewPlan<'a> {
    pub facility_id: Uuid,
    pub template_id: Uuid,
    pub code: &'a str,
    pub name: &'a str,
    pub asset_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub category_id: Option<Uuid>,
    pub trigger_type: &'a str,
    pub rrule: Option<&'a str>,
    pub meter_code: Option<&'a str>,
    pub meter_threshold: Option<f64>,
    pub generate_lead_days: Option<i32>,
    pub completion_grace_days: Option<i32>,
    pub priority: Option<&'a str>,
    pub assigned_team_id: Option<Uuid>,
    pub sla_policy_id: Option<Uuid>,
    /// 首次到期時刻。CALENDAR 型由 RRULE 展開的第一個時刻填入 ——
    /// 沒有它，產生器不知道何時該動。
    pub next_due_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn create(tx: &mut TenantTx, new: NewPlan<'_>) -> Result<Uuid, Problem> {
    let tenant_id = tx.context().tenant_id;
    sqlx::query_scalar!(
        r#"
        INSERT INTO fms.maintenance_plans
          (tenant_id, facility_id, template_id, code, name,
           asset_id, spatial_node_id, category_id, trigger_type, rrule,
           meter_code, meter_threshold, generate_lead_days,
           completion_grace_days, priority,
           assigned_team_id, sla_policy_id, next_due_at)
        VALUES
          ($1, $2, $3, $4, $5,
           $6, $7, $8, $9, $10,
           $11, $12::float8::numeric, coalesce($13::int::smallint, 7),
           coalesce($14::int::smallint, 0), coalesce($15, 'MEDIUM'),
           $16, $17, $18)
        RETURNING id
        "#,
        tenant_id,
        new.facility_id,
        new.template_id,
        new.code,
        new.name,
        new.asset_id,
        new.spatial_node_id,
        new.category_id,
        new.trigger_type,
        new.rrule,
        new.meter_code,
        new.meter_threshold,
        new.generate_lead_days,
        new.completion_grace_days,
        new.priority,
        new.assigned_team_id,
        new.sla_policy_id,
        new.next_due_at,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 計畫瞄準的設備清單。
///
/// 三種瞄準模式與 `fms-asset` 的 `maintenance_plans` 展開、
/// 以及計量門檻判定使用**同一套規則**：單一設備、空間子樹、或分類。
/// 三處若不一致，就會出現「詳情說這個計畫涵蓋本設備，產生器卻沒開單」。
pub async fn target_assets(tx: &mut TenantTx, plan_id: Uuid) -> Result<Vec<Uuid>, Problem> {
    sqlx::query_scalar!(
        r#"
        SELECT a.id
        FROM fms.maintenance_plans p
        JOIN fms.assets a
          ON a.deleted_at IS NULL
         AND a.facility_id = p.facility_id
         AND ( p.asset_id = a.id
               OR ( p.spatial_node_id IS NOT NULL
                    AND EXISTS (
                      SELECT 1 FROM fms.spatial_nodes sn, fms.spatial_nodes root
                       WHERE sn.id = a.spatial_node_id
                         AND root.id = p.spatial_node_id
                         AND sn.node_path OPERATOR(public.<@) root.node_path) )
               OR p.category_id = a.category_id )
        WHERE p.id = $1
        ORDER BY a.asset_code
        "#,
        plan_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 排程占位的一列。
pub struct OccurrenceRow {
    pub id: Uuid,
    pub asset_id: Option<Uuid>,
    pub scheduled_for: chrono::DateTime<chrono::Utc>,
    pub status: String,
    pub work_order_id: Option<Uuid>,
}

/// 建立排程占位。
///
/// 回傳 `None` 表示已經存在（`uq_maintenance_occurrences` 撞了）——
/// 這正是**冪等**的來源：產生器重跑、事件重放、兩個 worker 同時跑，
/// 都不會產生第二張工單。這個唯一索引是 004 就有的，
/// 產生器只要順著它走，就不需要自己做去重。
pub async fn claim_occurrence(
    tx: &mut TenantTx,
    plan_id: Uuid,
    asset_id: Option<Uuid>,
    scheduled_for: chrono::DateTime<chrono::Utc>,
) -> Result<Option<Uuid>, Problem> {
    let tenant_id = tx.context().tenant_id;
    sqlx::query_scalar!(
        r#"
        INSERT INTO fms.maintenance_occurrences
          (tenant_id, plan_id, asset_id, scheduled_for, status)
        VALUES ($1, $2, $3, $4, 'PLANNED')
        ON CONFLICT (plan_id, coalesce(asset_id, '00000000-0000-0000-0000-000000000000'::uuid),
                     scheduled_for)
        DO NOTHING
        RETURNING id
        "#,
        tenant_id,
        plan_id,
        asset_id,
        scheduled_for
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 把占位標記為已產生，並記下工單。
pub async fn mark_generated(
    tx: &mut TenantTx,
    occurrence_id: Uuid,
    work_order_id: Uuid,
) -> Result<(), Problem> {
    sqlx::query!(
        r#"UPDATE fms.maintenance_occurrences
              SET status = 'GENERATED',
                  work_order_id = $2,
                  generated_at = clock_timestamp()
            WHERE id = $1"#,
        occurrence_id,
        work_order_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 推進計畫的下一次到期時刻與最後產生時間。
pub async fn advance_plan(
    tx: &mut TenantTx,
    plan_id: Uuid,
    next_due_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), Problem> {
    sqlx::query!(
        r#"UPDATE fms.maintenance_plans
              SET next_due_at = $2,
                  last_generated_at = clock_timestamp()
            WHERE id = $1"#,
        plan_id,
        next_due_at
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 到期（含前置天數）且仍啟用的 CALENDAR／HYBRID 計畫。
///
/// `generate_lead_days` 是「提前幾天開單」，因此判定式是
/// `next_due_at <= now + lead_days`，而不是 `next_due_at <= now` ——
/// 後者會讓保養永遠在到期當天才被排上，違背整個前置天數欄位的用意。
pub async fn plans_due(tx: &mut TenantTx, batch: i64) -> Result<Vec<Uuid>, Problem> {
    sqlx::query_scalar!(
        r#"
        SELECT p.id
        FROM fms.maintenance_plans p
        WHERE p.is_active
          AND p.trigger_type IN ('CALENDAR', 'HYBRID')
          AND p.rrule IS NOT NULL
          AND p.next_due_at IS NOT NULL
          AND p.next_due_at <= clock_timestamp()
                               + (p.generate_lead_days::int * interval '1 day')
        ORDER BY p.next_due_at
        LIMIT $1
        "#,
        batch
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 某個計畫在指定時刻的占位（供測試與冪等檢查）。
pub async fn occurrences_of(
    tx: &mut TenantTx,
    plan_id: Uuid,
) -> Result<Vec<OccurrenceRow>, Problem> {
    sqlx::query_as!(
        OccurrenceRow,
        r#"SELECT id, asset_id, scheduled_for, status, work_order_id
             FROM fms.maintenance_occurrences
            WHERE plan_id = $1
            ORDER BY scheduled_for, id"#,
        plan_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 由 `category_code` 解析 `category_id`（建立計畫時契約給的是 code）。
pub async fn resolve_category(tx: &mut TenantTx, code: &str) -> Result<Option<Uuid>, Problem> {
    sqlx::query_scalar!("SELECT id FROM fms.asset_categories WHERE code = $1", code)
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)
}

/// 場域時區（建立計畫時要展開 RRULE 算首次到期）。
pub async fn facility_timezone(
    tx: &mut TenantTx,
    facility_id: Uuid,
) -> Result<Option<String>, Problem> {
    sqlx::query_scalar!(
        r#"SELECT timezone::text AS "timezone!" FROM fms.facilities
            WHERE id = $1 AND deleted_at IS NULL"#,
        facility_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 計畫瞄準的設備（含代碼），供 preview-schedule 顯示。
///
/// 與 [`target_assets`] 必須選出同一批設備 —— preview 顯示的張數
/// 就是產生器會開的張數，兩者不同就違背 preview 的用途。
pub async fn asset_codes_for(
    tx: &mut TenantTx,
    plan_id: Uuid,
) -> Result<Vec<(Uuid, String)>, Problem> {
    let rows = sqlx::query!(
        r#"
        SELECT a.id, a.asset_code::text AS "asset_code!"
        FROM fms.maintenance_plans p
        JOIN fms.assets a
          ON a.deleted_at IS NULL
         AND a.facility_id = p.facility_id
         AND ( p.asset_id = a.id
               OR ( p.spatial_node_id IS NOT NULL
                    AND EXISTS (
                      SELECT 1 FROM fms.spatial_nodes sn, fms.spatial_nodes root
                       WHERE sn.id = a.spatial_node_id
                         AND root.id = p.spatial_node_id
                         AND sn.node_path OPERATOR(public.<@) root.node_path) )
               OR p.category_id = a.category_id )
        WHERE p.id = $1
        ORDER BY a.asset_code
        "#,
        plan_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(rows.into_iter().map(|r| (r.id, r.asset_code)).collect())
}
