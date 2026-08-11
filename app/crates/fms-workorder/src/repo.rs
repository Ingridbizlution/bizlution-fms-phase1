//! 工單的資料存取。
//!
//! 刻意的分工：
//!   * **狀態變更完全不在這裡實作**。`transition` 只是呼叫
//!     `fms.transition_work_order()`；合法性、稽核列、outbox 事件都由該函式
//!     在同一個交易內完成。應用層若自己 UPDATE status，004 的
//!     `trg_enforce_wo_transition` 觸發器會擋下來 —— 那個觸發器的存在
//!     就是在宣告「函式是唯一入口」。
//!   * `wo_no` 由 `fms.next_document_no()` 產生，不在應用層拼字串：
//!     那個函式在 sequence 列上序列化，保證同租戶內無間隙。
//!   * `status_category` 由 `work_order_statuses` JOIN 帶出，不在應用層
//!     用 match 推導 —— 狀態的分類是資料。

use uuid::Uuid;

use fms_shared::{Problem, TenantTx};

/// `WorkOrder` 的一列。扁平承載，嵌套物件在 handler 組裝 ——
/// `query_as!` 無法直接產生嵌套結構。
pub struct WorkOrderRow {
    pub id: Uuid,
    pub wo_no: String,
    pub facility_id: Uuid,
    pub work_order_type: String,
    pub source: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub status_category: String,
    pub priority: String,
    pub asset_id: Option<Uuid>,
    pub asset_code: Option<String>,
    pub asset_name: Option<String>,
    pub spatial_node_id: Option<Uuid>,
    pub node_name: Option<String>,
    pub node_path: Option<String>,
    pub service_item_id: Option<Uuid>,
    pub reservation_id: Option<Uuid>,
    pub alarm_id: Option<Uuid>,
    pub requester_id: Option<Uuid>,
    pub requester_name: Option<String>,
    pub assignee_id: Option<Uuid>,
    pub assignee_name: Option<String>,
    pub team_id: Option<Uuid>,
    pub payload: serde_json::Value,
    pub scheduled_start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub scheduled_end_at: Option<chrono::DateTime<chrono::Utc>>,
    pub actual_start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub actual_end_at: Option<chrono::DateTime<chrono::Utc>>,
    pub response_due_at: Option<chrono::DateTime<chrono::Utc>>,
    pub resolution_due_at: Option<chrono::DateTime<chrono::Utc>>,
    pub sla_state: String,
    pub labor_minutes: i32,
    pub total_cost: Option<f64>,
    pub satisfaction_score: Option<i16>,
    pub resolution_notes: Option<String>,
    pub version: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl WorkOrderRow {
    /// 該列在指定排序欄位下的游標鍵。必須與 `list` 的 ORDER BY 一致。
    pub fn cursor_key(&self, sort_column: &str) -> (String, Uuid) {
        let key = match sort_column {
            "wo_no" => self.wo_no.clone(),
            "priority" => self.priority.clone(),
            _ => self.created_at.to_rfc3339(),
        };
        (key, self.id)
    }

    /// `required_fields` 提到的欄位，在**目前這一列**上是否已有值。
    ///
    /// 為什麼需要這個：`required_fields` 的語意是「這個動作發生時該欄位必須有值」，
    /// 不是「必須出現在請求 body 裡」。平台預設把 `title` 列為 SUBMIT 的必填，
    /// 而 `title` 是建立時就寫好的 NOT NULL 欄位、`WorkOrderTransitionRequest`
    /// 裡根本沒有這個欄位 —— 只檢查 body 會讓 SUBMIT 永遠無法執行。
    pub fn has_value_for(&self, field: &str) -> bool {
        match field {
            "title" => !self.title.is_empty(),
            "assignee_id" => self.assignee_id.is_some(),
            "team_id" => self.team_id.is_some(),
            "scheduled_start_at" => self.scheduled_start_at.is_some(),
            "resolution_notes" => self.resolution_notes.is_some(),
            "description" => self.description.is_some(),
            // `reason` 沒有對應的既有欄位（函式會把它寫進 cancelled_reason
            // 與稽核列），因此只能來自 body。其餘未知名稱同理。
            _ => false,
        }
    }
}

// SELECT 主體在 list 與 fetch 中各一份：`query_as!` 的第一個參數必須是
// 字串字面值。與 fms-asset 相同的取捨，理由見該模組的說明。

/// 依 id 集合取工單。private：兩個條件都不給會回全表。
async fn fetch(tx: &mut TenantTx, ids: &[Uuid]) -> Result<Vec<WorkOrderRow>, Problem> {
    sqlx::query_as!(
        WorkOrderRow,
        r#"
        SELECT w.id,
               w.wo_no::text            AS "wo_no!",
               w.facility_id,
               w.work_order_type,
               w.source,
               w.title::text            AS "title!",
               w.description,
               w.status,
               s.category               AS "status_category!",
               w.priority,
               w.asset_id,
               a.asset_code::text       AS "asset_code",
               a.name::text             AS "asset_name",
               w.spatial_node_id,
               sn.name::text            AS "node_name",
               sn.node_path::text       AS "node_path",
               w.service_item_id,
               w.reservation_id,
               w.alarm_id,
               w.requester_id,
               ru.display_name::text    AS "requester_name",
               w.assignee_id,
               au.display_name::text    AS "assignee_name",
               w.team_id,
               w.payload                AS "payload!",
               w.scheduled_start_at,
               w.scheduled_end_at,
               w.actual_start_at,
               w.actual_end_at,
               w.response_due_at,
               w.resolution_due_at,
               w.sla_state,
               w.labor_minutes,
               -- 契約有 total_cost，schema 只有三個分項且沒有生成欄位，
               -- 因此在查詢裡加總。放在 SQL 而非 Rust，是為了讓
               -- 日後要用它排序或過濾時不需要改兩個地方。
               (w.labor_cost + w.parts_cost + w.other_cost)::float8 AS "total_cost",
               w.satisfaction_score,
               w.resolution_notes,
               w.version,
               w.created_at,
               w.updated_at,
               w.completed_at
        FROM fms.work_orders w
        JOIN fms.work_order_statuses s ON s.code = w.status
        LEFT JOIN fms.assets a        ON a.id  = w.asset_id
        LEFT JOIN fms.spatial_nodes sn ON sn.id = w.spatial_node_id
        LEFT JOIN fms.users ru        ON ru.id = w.requester_id
        LEFT JOIN fms.users au        ON au.id = w.assignee_id
        WHERE w.deleted_at IS NULL AND w.id = ANY($1)
        ORDER BY w.created_at DESC
        "#,
        ids
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

pub async fn get(tx: &mut TenantTx, id: Uuid) -> Result<Option<WorkOrderRow>, Problem> {
    Ok(fetch(tx, &[id]).await?.pop())
}

/// 資產的未結工單（供 `GET /assets/{id}?include=open_work_orders`）。
pub async fn open_for_asset(tx: &mut TenantTx, asset_id: Uuid) -> Result<Vec<Uuid>, Problem> {
    sqlx::query_scalar!(
        r#"SELECT w.id FROM fms.work_orders w
            WHERE w.asset_id = $1 AND w.deleted_at IS NULL
              AND w.status NOT IN ('CLOSED','CANCELLED','REJECTED')
            ORDER BY w.created_at DESC"#,
        asset_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 依 id 集合取工單，供關聯展開使用。
pub async fn by_ids(tx: &mut TenantTx, ids: &[Uuid]) -> Result<Vec<WorkOrderRow>, Problem> {
    fetch(tx, ids).await
}

/// `list` 的過濾條件。欄位數已超過 clippy 的參數上限，包成結構比
/// `#[allow(too_many_arguments)]` 誠實 —— 12 個位置參數的呼叫端不可讀。
pub struct ListFilter<'a> {
    pub facility_id: Option<Uuid>,
    pub work_order_type: Option<&'a str>,
    /// 已切開的多值 `status`（契約是逗號分隔字串）。
    pub statuses: Option<Vec<String>>,
    pub status_category: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub assignee_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub source: Option<&'a str>,
    pub sla_state: Option<&'a str>,
    /// `mine=true` 時傳入目前使用者 id；否則 None。
    pub mine_user_id: Option<Uuid>,
    pub created_from: Option<chrono::DateTime<chrono::Utc>>,
    pub created_to: Option<chrono::DateTime<chrono::Utc>>,
}

/// 列出工單。動態排序的手法與 `fms-asset::repo::list` 相同，理由見該處。
pub async fn list(
    tx: &mut TenantTx,
    f: &ListFilter<'_>,
    cursor: Option<&fms_shared::Cursor>,
    sort: &fms_shared::SortSpec,
    limit: i64,
) -> Result<Vec<WorkOrderRow>, Problem> {
    let (cursor_id, cursor_ts, cursor_text) = match cursor {
        None => (None, None, None),
        Some(c) if c.sort_column == "created_at" => {
            (Some(c.uuid_id()?), Some(c.as_timestamp()?), None)
        }
        Some(c) => (Some(c.uuid_id()?), None, Some(c.key.clone())),
    };

    sqlx::query_as!(
        WorkOrderRow,
        r#"
        SELECT w.id,
               w.wo_no::text            AS "wo_no!",
               w.facility_id,
               w.work_order_type,
               w.source,
               w.title::text            AS "title!",
               w.description,
               w.status,
               s.category               AS "status_category!",
               w.priority,
               w.asset_id,
               a.asset_code::text       AS "asset_code",
               a.name::text             AS "asset_name",
               w.spatial_node_id,
               sn.name::text            AS "node_name",
               sn.node_path::text       AS "node_path",
               w.service_item_id,
               w.reservation_id,
               w.alarm_id,
               w.requester_id,
               ru.display_name::text    AS "requester_name",
               w.assignee_id,
               au.display_name::text    AS "assignee_name",
               w.team_id,
               w.payload                AS "payload!",
               w.scheduled_start_at,
               w.scheduled_end_at,
               w.actual_start_at,
               w.actual_end_at,
               w.response_due_at,
               w.resolution_due_at,
               w.sla_state,
               w.labor_minutes,
               (w.labor_cost + w.parts_cost + w.other_cost)::float8 AS "total_cost",
               w.satisfaction_score,
               w.resolution_notes,
               w.version,
               w.created_at,
               w.updated_at,
               w.completed_at
        FROM fms.work_orders w
        JOIN fms.work_order_statuses s ON s.code = w.status
        LEFT JOIN fms.assets a        ON a.id  = w.asset_id
        LEFT JOIN fms.spatial_nodes sn ON sn.id = w.spatial_node_id
        LEFT JOIN fms.users ru        ON ru.id = w.requester_id
        LEFT JOIN fms.users au        ON au.id = w.assignee_id
        WHERE w.deleted_at IS NULL
          AND ($1::uuid   IS NULL OR w.facility_id = $1)
          AND ($2::text   IS NULL OR w.work_order_type = $2)
          AND ($3::text[] IS NULL OR w.status = ANY($3))
          AND ($4::text   IS NULL OR s.category = $4)
          AND ($5::text   IS NULL OR w.priority = $5)
          AND ($6::uuid   IS NULL OR w.assignee_id = $6)
          AND ($7::uuid   IS NULL OR w.team_id = $7)
          AND ($8::uuid   IS NULL OR w.asset_id = $8)
          AND ($9::uuid   IS NULL OR w.spatial_node_id = $9)
          AND ($10::text  IS NULL OR w.source = $10)
          AND ($11::text  IS NULL OR w.sla_state = $11)
          -- mine：契約定義為「負責人或申請人」，兩者皆算
          AND ($12::uuid  IS NULL OR w.assignee_id = $12 OR w.requester_id = $12)
          AND ($13::timestamptz IS NULL OR w.created_at >= $13)
          AND ($14::timestamptz IS NULL OR w.created_at <= $14)
          AND ($15::uuid IS NULL OR CASE
                WHEN $18::text = 'created_at' AND $19::bool
                  THEN (w.created_at, w.id) < ($16::timestamptz, $15::uuid)
                WHEN $18::text = 'created_at' AND NOT $19::bool
                  THEN (w.created_at, w.id) > ($16::timestamptz, $15::uuid)
                WHEN $18::text = 'wo_no' AND $19::bool
                  THEN (w.wo_no::text, w.id) < ($17::text, $15::uuid)
                WHEN $18::text = 'wo_no' AND NOT $19::bool
                  THEN (w.wo_no::text, w.id) > ($17::text, $15::uuid)
                WHEN $18::text = 'priority' AND $19::bool
                  THEN (w.priority, w.id) < ($17::text, $15::uuid)
                WHEN $18::text = 'priority' AND NOT $19::bool
                  THEN (w.priority, w.id) > ($17::text, $15::uuid)
              END)
        ORDER BY
          (CASE WHEN $18::text = 'created_at' AND $19::bool THEN w.created_at END) DESC,
          (CASE WHEN $18::text = 'created_at' AND NOT $19::bool THEN w.created_at END) ASC,
          (CASE WHEN $18::text = 'wo_no' AND $19::bool THEN w.wo_no::text END) DESC,
          (CASE WHEN $18::text = 'wo_no' AND NOT $19::bool THEN w.wo_no::text END) ASC,
          (CASE WHEN $18::text = 'priority' AND $19::bool THEN w.priority END) DESC,
          (CASE WHEN $18::text = 'priority' AND NOT $19::bool THEN w.priority END) ASC,
          (CASE WHEN $19::bool THEN w.id END) DESC,
          (CASE WHEN NOT $19::bool THEN w.id END) ASC
        LIMIT $20
        "#,
        f.facility_id,
        f.work_order_type,
        f.statuses.as_deref(),
        f.status_category,
        f.priority,
        f.assignee_id,
        f.team_id,
        f.asset_id,
        f.spatial_node_id,
        f.source,
        f.sla_state,
        f.mine_user_id,
        f.created_from,
        f.created_to,
        cursor_id,
        cursor_ts,
        cursor_text,
        sort.column,
        sort.desc,
        limit + 1
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

pub struct NewWorkOrder<'a> {
    pub facility_id: Uuid,
    pub work_order_type: &'a str,
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub asset_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub service_item_id: Option<Uuid>,
    pub reservation_id: Option<Uuid>,
    pub priority: Option<&'a str>,
    pub requested_start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub payload: Option<&'a serde_json::Value>,
    pub team_id: Option<Uuid>,
    pub assignee_id: Option<Uuid>,
    /// `DRAFT` 或 `SUBMITTED`。刻意由 handler 決定並傳入字面值，
    /// 而不是傳 bool 讓 SQL 做 CASE —— 狀態碼是契約詞彙，該看得見。
    pub status: &'a str,
    /// `work_orders.source`。報表要區分反應性與計畫性維護的比例，
    /// 因此 provenance 不能用一個寫死的預設值 ——
    /// REST 建立是 `API`，PM 產生器是 `PM_PLAN`，IoT 告警是 `IOT_ALARM`。
    pub source: &'a str,
    pub maintenance_plan_id: Option<Uuid>,
    pub maintenance_occurrence_id: Option<Uuid>,
}

/// 建立工單。
///
/// `requester_id` 一律是目前使用者：契約沒有讓客戶端指定申請人的欄位，
/// 而「誰報修」是稽核資訊，不該由請求方自稱。
pub async fn create(tx: &mut TenantTx, new: NewWorkOrder<'_>) -> Result<Uuid, Problem> {
    let ctx = tx.context();
    let (tenant_id, user_id) = (ctx.tenant_id, ctx.user_id);
    sqlx::query_scalar!(
        r#"
        INSERT INTO fms.work_orders
          (tenant_id, facility_id, wo_no, work_order_type, source, title, description,
           asset_id, spatial_node_id, service_item_id, reservation_id,
           requester_id, assignee_id, team_id, priority, status,
           requested_start_at, payload, created_by,
           maintenance_plan_id, maintenance_occurrence_id)
        VALUES
          ($1, $2, fms.next_document_no($1, 'WORK_ORDER', 'WO'), $3, $17, $4, $5,
           $6, $7, $8, $9,
           $10, $11, $12, coalesce($13, 'MEDIUM'), $14,
           $15, coalesce($16, '{}'::jsonb), $10,
           $18, $19)
        RETURNING id
        "#,
        tenant_id,
        new.facility_id,
        new.work_order_type,
        new.title,
        new.description,
        new.asset_id,
        new.spatial_node_id,
        new.service_item_id,
        new.reservation_id,
        user_id,
        new.assignee_id,
        new.team_id,
        new.priority,
        new.status,
        new.requested_start_at,
        new.payload,
        new.source,
        new.maintenance_plan_id,
        new.maintenance_occurrence_id,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 局部更新非狀態欄位。
pub async fn update(
    tx: &mut TenantTx,
    id: Uuid,
    u: &crate::dto::WorkOrderUpdate,
) -> Result<(), Problem> {
    sqlx::query!(
        r#"
        UPDATE fms.work_orders SET
          title              = coalesce($2, title),
          description        = coalesce($3, description),
          priority           = coalesce($4, priority),
          team_id            = coalesce($5, team_id),
          scheduled_start_at = coalesce($6, scheduled_start_at),
          scheduled_end_at   = coalesce($7, scheduled_end_at),
          payload            = coalesce($8, payload),
          is_chargeback      = coalesce($9, is_chargeback),
          chargeback_org_id  = coalesce($10, chargeback_org_id)
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id,
        u.title,
        u.description,
        u.priority,
        u.team_id,
        u.scheduled_start_at,
        u.scheduled_end_at,
        u.payload,
        u.is_chargeback,
        u.chargeback_org_id,
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 狀態機規則的一列（只取應用層需要的欄位）。
pub struct TransitionRule {
    pub to_status: String,
    pub required_permission: Option<String>,
    pub required_fields: Vec<String>,
    pub side_effects: serde_json::Value,
    pub label_zh: Option<String>,
}

/// 找出「目前狀態 + 這個動作」對應的規則。
///
/// 選取順序與 `fms.transition_work_order` 內部**必須一致**
/// （`tenant_id NULLS LAST, work_order_type NULLS LAST`）：
/// 應用層依這條規則檢查權限與必填欄位，資料庫依它決定目標狀態，
/// 兩邊選到不同列就會出現「檢查了 A、執行了 B」。
pub async fn matched_rule(
    tx: &mut TenantTx,
    work_order_id: Uuid,
    action: &str,
) -> Result<Option<TransitionRule>, Problem> {
    sqlx::query_as!(
        TransitionRule,
        r#"
        SELECT t.to_status       AS "to_status!",
               t.required_permission,
               t.required_fields AS "required_fields!",
               t.side_effects    AS "side_effects!",
               act.label_zh::text AS "label_zh"
        FROM fms.work_orders w
        JOIN fms.work_order_transitions_allowed t
          ON t.is_active
         AND (t.tenant_id IS NULL OR t.tenant_id = w.tenant_id)
         AND (t.work_order_type IS NULL OR t.work_order_type = w.work_order_type)
         AND t.from_status = w.status
         AND t.action = $2
        LEFT JOIN fms.work_order_actions act ON act.code = t.action
        WHERE w.id = $1
        ORDER BY t.tenant_id NULLS LAST, t.work_order_type NULLS LAST
        LIMIT 1
        "#,
        work_order_id,
        action
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 目前狀態下所有可執行的動作（不含權限判定）。
pub struct AvailableAction {
    pub action: String,
    pub to_status: String,
    pub label_zh: Option<String>,
    pub required_fields: Vec<String>,
    pub required_permission: Option<String>,
}

/// 列出目前狀態的所有動作。
///
/// `DISTINCT ON (t.action)` 搭配同樣的優先順序：同一個動作若同時有平台預設列
/// 與租戶覆寫列，只能出現一次，而且必須是實際會被套用的那一列。
pub async fn available_actions(
    tx: &mut TenantTx,
    work_order_id: Uuid,
) -> Result<Vec<AvailableAction>, Problem> {
    sqlx::query_as!(
        AvailableAction,
        r#"
        SELECT DISTINCT ON (t.action)
               t.action          AS "action!",
               t.to_status       AS "to_status!",
               act.label_zh::text AS "label_zh",
               t.required_fields AS "required_fields!",
               t.required_permission
        FROM fms.work_orders w
        JOIN fms.work_order_transitions_allowed t
          ON t.is_active
         AND (t.tenant_id IS NULL OR t.tenant_id = w.tenant_id)
         AND (t.work_order_type IS NULL OR t.work_order_type = w.work_order_type)
         AND t.from_status = w.status
        LEFT JOIN fms.work_order_actions act ON act.code = t.action
        WHERE w.id = $1
        ORDER BY t.action, t.tenant_id NULLS LAST, t.work_order_type NULLS LAST
        "#,
        work_order_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 在轉換**之前**把 body 帶來的欄位寫進工單。
///
/// 順序不可反轉：`required_fields` 的語意是「轉換發生時該欄位必須有值」，
/// 而 `fms.transition_work_order` 讀的是資料列。先寫欄位再轉換，
/// `set_actual_end` 之類的副作用才會作用在正確的資料上。
///
/// 這些欄位的寫入不含 `status`，因此不會踩到 `trg_enforce_wo_transition`。
pub async fn apply_transition_fields(
    tx: &mut TenantTx,
    id: Uuid,
    req: &crate::dto::TransitionRequest,
) -> Result<(), Problem> {
    sqlx::query!(
        r#"
        UPDATE fms.work_orders SET
          assignee_id        = coalesce($2, assignee_id),
          team_id            = coalesce($3, team_id),
          scheduled_start_at = coalesce($4, scheduled_start_at),
          resolution_notes   = coalesce($5, resolution_notes),
          close_code         = coalesce($6, close_code),
          root_cause         = coalesce($7, root_cause)
          -- labor_minutes 刻意不在這裡寫：它現在由 work_order_labor 的
          -- 明細列 rollup 而來（見 recompute_costs）。兩邊都寫會讓
          -- 「總量」與「明細之和」在重試後對不上。
          , labor_minutes = labor_minutes
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id,
        req.assignee_id,
        req.team_id,
        req.scheduled_start_at,
        req.resolution_notes,
        req.close_code,
        req.root_cause,
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 執行狀態轉換。回傳轉換後的狀態。
///
/// 只回 `status` 而非整列：函式的回傳型別是 `fms.work_orders`（複合型別），
/// `query_as!` 無法直接展開它。呼叫端接著用 `get` 重讀完整列 ——
/// 同一個交易內，因此讀到的必然是轉換後的狀態。
pub async fn transition(
    tx: &mut TenantTx,
    id: Uuid,
    action: &str,
    reason: Option<&str>,
    metadata: &serde_json::Value,
) -> Result<String, Problem> {
    let actor = tx.context().user_id;
    let status = sqlx::query_scalar!(
        r#"SELECT (fms.transition_work_order($1, $2, $3, $4, $5)).status AS "status!""#,
        id,
        action,
        actor,
        reason,
        metadata
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(status)
}

/// `side_effects` 的 `increment_reopen`。
///
/// 為什麼在應用層做：`fms.transition_work_order` 只實作了 `emit`、
/// `set_responded`、`set_actual_start`、`set_actual_end` 四個 key，
/// 其餘七個宣告了卻沒有任何執行者。004 的欄位註解寫的是
/// 「Declarative effects executed by the service layer」，
/// 因此服務層才是宣告的歸屬地。與轉換同一個交易，所以仍然是原子的。
pub async fn increment_reopen(tx: &mut TenantTx, id: Uuid) -> Result<(), Problem> {
    sqlx::query!(
        "UPDATE fms.work_orders SET reopened_count = reopened_count + 1 WHERE id = $1",
        id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// `side_effects` 的 `release_assignee`：取消時清掉負責人。
///
/// 不清會讓「我的工單」列表一直帶著已取消的項目，
/// 而 `idx_wo_assignee_open` 的部分索引條件已經把終態排除，
/// 顯示層與索引層對「未結」的定義就此不一致。
pub async fn release_assignee(tx: &mut TenantTx, id: Uuid) -> Result<(), Problem> {
    sqlx::query!(
        "UPDATE fms.work_orders SET assignee_id = NULL WHERE id = $1",
        id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 狀態轉換稽核紀錄（`include=transitions`）。
pub struct TransitionLogRow {
    pub from_status: Option<String>,
    pub action: String,
    pub to_status: String,
    pub actor_name: Option<String>,
    pub reason: Option<String>,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
}

pub async fn transition_log(tx: &mut TenantTx, id: Uuid) -> Result<Vec<TransitionLogRow>, Problem> {
    sqlx::query_as!(
        TransitionLogRow,
        r#"
        SELECT t.from_status::text AS "from_status",
               t.action::text      AS "action!",
               t.to_status::text   AS "to_status!",
               u.display_name::text AS "actor_name",
               t.reason::text      AS "reason",
               t.occurred_at
        FROM fms.work_order_transitions t
        LEFT JOIN fms.users u ON u.id = t.actor_user_id
        WHERE t.work_order_id = $1
        ORDER BY t.occurred_at, t.id
        "#,
        id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 服務項目的 `form_schema`，供 `payload` 驗證。
/// 回傳 `None` 表示服務項目不存在（或不在本租戶可見範圍內）。
pub async fn service_item_form_schema(
    tx: &mut TenantTx,
    id: Uuid,
) -> Result<Option<serde_json::Value>, Problem> {
    sqlx::query_scalar!(
        r#"SELECT form_schema AS "form_schema!" FROM fms.service_items
            WHERE id = $1 AND deleted_at IS NULL"#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

// =============================================================================
// 工單子資源：檢查表與留言
// =============================================================================

/// `WorkOrderTask` 的一列。
pub struct TaskRow {
    pub id: Uuid,
    pub seq: i16,
    pub title: String,
    pub input_type: String,
    pub unit: Option<String>,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    pub options: Option<serde_json::Value>,
    pub is_required: bool,
    pub result_value: Option<serde_json::Value>,
    pub is_pass: Option<bool>,
    pub notes: Option<String>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// 由保養範本的 `checklist` 展開成工單的檢查項目。
///
/// # 為什麼在 SQL 裡用 `jsonb_to_recordset` 而不是在 Rust 迴圈
///
/// 一次寫入、不需要中間往返，而且範本的 JSON 欄位名（`input_type`、
/// `min_value`…）刻意與 `work_order_tasks` 的欄位同名 ——
/// 009 的種子就是照這個對應寫的，因此展開是純粹的形狀轉換，
/// 沒有應用層需要決定的事。
///
/// `ON CONFLICT DO NOTHING` 對應 `uq_wo_tasks_seq`：重跑不會重複展開。
/// 這與產生器的冪等是同一個手法。
pub async fn expand_template_checklist(
    tx: &mut TenantTx,
    work_order_id: Uuid,
    template_id: Uuid,
) -> Result<u64, Problem> {
    let tenant_id = tx.context().tenant_id;
    let done = sqlx::query!(
        r#"
        INSERT INTO fms.work_order_tasks
          (tenant_id, work_order_id, seq, title, input_type, unit,
           min_value, max_value, options, is_required)
        SELECT $1, $2, c.seq, c.title,
               coalesce(c.input_type, 'CHECKBOX'), c.unit,
               c.min_value, c.max_value, c.options,
               coalesce(c.is_required, false)
        FROM fms.maintenance_templates t
        CROSS JOIN LATERAL jsonb_to_recordset(t.checklist) AS c(
          seq smallint, title varchar, input_type text, unit varchar,
          min_value numeric, max_value numeric, options jsonb, is_required boolean
        )
        WHERE t.id = $3
        ON CONFLICT (work_order_id, seq) DO NOTHING
        "#,
        tenant_id,
        work_order_id,
        template_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(done.rows_affected())
}

pub async fn tasks(tx: &mut TenantTx, work_order_id: Uuid) -> Result<Vec<TaskRow>, Problem> {
    sqlx::query_as!(
        TaskRow,
        r#"SELECT id, seq, title::text AS "title!", input_type,
                  unit::text AS "unit", min_value::float8 AS "min_value",
                  max_value::float8 AS "max_value", options,
                  is_required, result_value, is_pass, notes, completed_at
             FROM fms.work_order_tasks
            WHERE work_order_id = $1
            ORDER BY seq"#,
        work_order_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 單一檢查項目。**同時**以工單 id 過濾，而不是只用 task id ——
/// 契約的路徑是 `/work-orders/{woId}/tasks/{taskId}`，
/// 若只用 taskId 查，帶著別的工單 id 也會成功，路徑就變成謊言。
pub async fn task(
    tx: &mut TenantTx,
    work_order_id: Uuid,
    task_id: Uuid,
) -> Result<Option<TaskRow>, Problem> {
    sqlx::query_as!(
        TaskRow,
        r#"SELECT id, seq, title::text AS "title!", input_type,
                  unit::text AS "unit", min_value::float8 AS "min_value",
                  max_value::float8 AS "max_value", options,
                  is_required, result_value, is_pass, notes, completed_at
             FROM fms.work_order_tasks
            WHERE work_order_id = $1 AND id = $2"#,
        work_order_id,
        task_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 回填檢查結果。
///
/// `completed_at` 與 `completed_by` 在**有填入結果時**才設定：
/// 只改備註不算完成該項目。
pub async fn update_task(
    tx: &mut TenantTx,
    task_id: Uuid,
    result_value: Option<&serde_json::Value>,
    is_pass: Option<bool>,
    notes: Option<&str>,
) -> Result<(), Problem> {
    let user_id = tx.context().user_id;
    sqlx::query!(
        r#"
        UPDATE fms.work_order_tasks SET
          result_value = coalesce($2, result_value),
          is_pass      = coalesce($3, is_pass),
          notes        = coalesce($4, notes),
          completed_by = CASE WHEN $2 IS NOT NULL THEN $5 ELSE completed_by END,
          completed_at = CASE WHEN $2 IS NOT NULL
                              THEN clock_timestamp() ELSE completed_at END
        WHERE id = $1
        "#,
        task_id,
        result_value,
        is_pass,
        notes,
        user_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// `WorkOrderDetail.comments[]` 的一列。
pub struct CommentRow {
    pub id: Uuid,
    pub author_name: Option<String>,
    pub visibility: String,
    pub body: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn comments(tx: &mut TenantTx, work_order_id: Uuid) -> Result<Vec<CommentRow>, Problem> {
    sqlx::query_as!(
        CommentRow,
        r#"SELECT c.id, u.display_name::text AS "author_name",
                  c.visibility, c.body AS "body!", c.created_at
             FROM fms.work_order_comments c
             LEFT JOIN fms.users u ON u.id = c.author_id
            WHERE c.work_order_id = $1
            ORDER BY c.created_at, c.id"#,
        work_order_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

pub async fn add_comment(
    tx: &mut TenantTx,
    work_order_id: Uuid,
    visibility: &str,
    body: &str,
) -> Result<Uuid, Problem> {
    let ctx = tx.context();
    sqlx::query_scalar!(
        r#"INSERT INTO fms.work_order_comments
             (tenant_id, work_order_id, author_id, visibility, body)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id"#,
        ctx.tenant_id,
        work_order_id,
        ctx.user_id,
        visibility,
        body
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

// =============================================================================
// 工單子資源：工時與料件
// =============================================================================

/// `WorkOrderDetail.parts[]` 的一列。
pub struct UsedPartRow {
    pub part_code: String,
    pub name: String,
    pub quantity_used: f64,
    pub total_cost: Option<f64>,
}

pub async fn used_parts(
    tx: &mut TenantTx,
    work_order_id: Uuid,
) -> Result<Vec<UsedPartRow>, Problem> {
    sqlx::query_as!(
        UsedPartRow,
        r#"SELECT p.part_code::text AS "part_code!",
                  p.name::text      AS "name!",
                  wp.quantity_used::float8 AS "quantity_used!",
                  wp.total_cost::float8    AS "total_cost"
             FROM fms.work_order_parts wp
             JOIN fms.parts p ON p.id = wp.part_id
            WHERE wp.work_order_id = $1
            ORDER BY p.part_code"#,
        work_order_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 工時明細的一列。
pub struct LaborRow {
    pub user_name: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub minutes: Option<i32>,
    pub cost: Option<f64>,
    pub is_overtime: bool,
}

pub async fn labor(tx: &mut TenantTx, work_order_id: Uuid) -> Result<Vec<LaborRow>, Problem> {
    sqlx::query_as!(
        LaborRow,
        r#"SELECT u.display_name::text AS "user_name",
                  l.started_at, l.ended_at, l.minutes,
                  l.cost::float8 AS "cost",
                  l.is_overtime
             FROM fms.work_order_labor l
             LEFT JOIN fms.users u ON u.id = l.user_id
            WHERE l.work_order_id = $1
            ORDER BY l.started_at"#,
        work_order_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 記錄一筆工時。
///
/// # 為什麼 `hourly_rate` 與 `cost` 留 NULL
///
/// **全 schema 沒有任何費率來源**：`hourly_rate` 只出現在
/// `work_order_labor` 自己身上，沒有使用者費率表、沒有技能費率表，
/// 契約的 `WorkOrderTransitionRequest` 也沒有 rate 欄位。
/// 因此工時的**分鐘數**可以記，**成本**無法計算 ——
/// 隨便填一個費率會產生看起來精確而實際上憑空的成本數字，
/// 那比留 NULL 糟得多。已記入 docs/WBS-rebaseline.md 4.1m。
///
/// `started_at` 由 `actual_start_at` 回推：契約只給了 `labor_minutes`
/// （一個總量），沒有起訖時刻。以工單的實際開始時間為起點是最接近事實的
/// 推導，而 `ck_labor_range` 要求 `ended_at >= started_at`。
pub async fn record_labor(
    tx: &mut TenantTx,
    work_order_id: Uuid,
    minutes: i32,
) -> Result<(), Problem> {
    let ctx = tx.context();
    sqlx::query!(
        r#"
        INSERT INTO fms.work_order_labor
          (tenant_id, work_order_id, user_id, started_at, ended_at, minutes)
        SELECT $1, $2, $3,
               coalesce(w.actual_start_at, clock_timestamp())
                 - ($4::int * interval '1 minute'),
               coalesce(w.actual_start_at, clock_timestamp()),
               $4
        FROM fms.work_orders w
        WHERE w.id = $2
        "#,
        ctx.tenant_id,
        work_order_id,
        ctx.user_id,
        minutes
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 記錄一筆料件用量，並在該場域有庫存時原子性扣帳。
///
/// # 為什麼用「條件式 UPDATE 看影響列數」而不是先查再扣
///
/// 先查庫存再扣是 check-then-act：兩張工單同時領最後一片濾網，兩者都會
/// 讀到「還有 1」。條件寫進 `WHERE quantity_on_hand >= $qty` 之後，
/// 由資料庫仲裁，落敗者的影響列數是 0。
///
/// `ck_part_stock_nonneg` 是最後一道網，但**不該讓它成為錯誤路徑**：
/// 它會拋 23514，而那個 SQLSTATE 在本專案已有兩種語意
/// （配額、狀態機），再加一種只會讓錯誤映射更難維護。
///
/// 回傳 `false` 表示「該場域有這個料件的庫存，但不足」。
/// 完全沒有庫存列則回 `true` 並不連結庫存 —— 那是真實情境
/// （廠商當場帶料、緊急採購），拒絕它會讓系統無法記錄真正發生的事。
pub async fn record_part_usage(
    tx: &mut TenantTx,
    work_order_id: Uuid,
    facility_id: Uuid,
    part_id: Uuid,
    quantity: f64,
) -> Result<bool, Problem> {
    let ctx = tx.context();

    // 先試著扣帳。命中就拿到 stock id 以供連結。
    let stock_id: Option<Uuid> = sqlx::query_scalar!(
        r#"
        UPDATE fms.part_stock
           SET quantity_on_hand = quantity_on_hand - $3::float8::numeric,
               updated_at = clock_timestamp()
         WHERE part_id = $1 AND facility_id = $2
           AND quantity_on_hand >= $3::float8::numeric
        RETURNING id
        "#,
        part_id,
        facility_id,
        quantity
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;

    if stock_id.is_none() {
        // 區分「沒有庫存列」與「有但不足」——前者可以繼續，後者是衝突。
        let tracked: Option<bool> = sqlx::query_scalar!(
            r#"SELECT true FROM fms.part_stock WHERE part_id = $1 AND facility_id = $2"#,
            part_id,
            facility_id
        )
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?
        .flatten();
        if tracked.is_some() {
            return Ok(false);
        }
    }

    sqlx::query!(
        r#"
        INSERT INTO fms.work_order_parts
          (tenant_id, work_order_id, part_id, quantity_planned, quantity_used,
           unit_cost, total_cost, issued_from_stock_id, issued_at, issued_by)
        SELECT $1, $2, $3, 0, $4::float8::numeric,
               p.unit_cost,
               -- 成本在領用時**快照**：料件單價日後調整不該改寫已完成工單的成本。
               p.unit_cost * $4::float8::numeric,
               $5, clock_timestamp(), $6
        FROM fms.parts p
        WHERE p.id = $3
        "#,
        ctx.tenant_id,
        work_order_id,
        part_id,
        quantity,
        stock_id,
        ctx.user_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;

    Ok(true)
}

/// 由明細列重算工單的成本欄位。
///
/// 重算而非累加：累加在重試或部分失敗後會漂移，而明細列是唯一真實來源。
/// `labor_cost` 仍會是 0 —— 沒有費率來源（見 [`record_labor`]）。
pub async fn recompute_costs(tx: &mut TenantTx, work_order_id: Uuid) -> Result<(), Problem> {
    sqlx::query!(
        r#"
        UPDATE fms.work_orders w SET
          parts_cost = coalesce((SELECT sum(wp.total_cost)
                                   FROM fms.work_order_parts wp
                                  WHERE wp.work_order_id = w.id), 0),
          labor_cost = coalesce((SELECT sum(l.cost)
                                   FROM fms.work_order_labor l
                                  WHERE l.work_order_id = w.id), 0),
          labor_minutes = coalesce((SELECT sum(l.minutes)::int
                                      FROM fms.work_order_labor l
                                     WHERE l.work_order_id = w.id), w.labor_minutes)
        WHERE w.id = $1
        "#,
        work_order_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// `side_effects` 的 `request_satisfaction`：結案時請申請人評分。
///
/// 邏輯在 SQL（067 的 `fms.request_satisfaction`）而不是這裡：它要讀範本、
/// 算變數、判斷有沒有邀請過，而那三件事都在資料庫裡。搬到 Rust 會變成
/// 「範本渲染有兩份實作」—— 059 的證照提醒已經在 SQL 做過同一件事。
///
/// 回傳建立的通知筆數。**0 不是錯誤**，有三種原因（無範本／工單沒有申請人／
/// 已經邀請過），呼叫端據此決定要不要記一筆。
pub async fn request_satisfaction(tx: &mut TenantTx, id: Uuid) -> Result<i32, Problem> {
    sqlx::query_scalar!("SELECT fms.request_satisfaction($1)", id)
        .fetch_one(tx.conn())
        .await
        .map_err(Problem::from)
        .map(|n| n.unwrap_or(0))
}

/// 鎖住那一列，供樂觀鎖的前置讀取使用。
///
/// **必須在讀出 `version` 之前呼叫。** 少了它，兩個並發的 PATCH 會讀到同一個
/// 版本、都通過 `check_version`、都寫入 —— 見該函式的說明。
///
/// 不做 not-found 判斷：呼叫端緊接著的 `get()` 會處理。列不存在時
/// `FOR UPDATE` 什麼也不鎖，那是正確的行為。
///
/// 刻意用不帶 JOIN 的最小查詢：`get()` 帶 JOIN，而對它加 `FOR UPDATE` 會連
/// `users` 那些列一起鎖住 —— 過度加鎖，而且會製造與其他路徑的死鎖機會。
pub async fn lock(tx: &mut TenantTx, id: Uuid) -> Result<(), Problem> {
    sqlx::query("SELECT 1 FROM fms.work_orders WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?;
    Ok(())
}
