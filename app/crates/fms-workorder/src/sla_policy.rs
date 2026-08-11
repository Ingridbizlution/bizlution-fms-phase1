//! SLA 政策的維護（`/sla-policies`）。
//!
//! # 為什麼這支端點是必要的
//!
//! 032 之後，SLA policy 決定每一張工單的目標時刻，而目標時刻決定
//! `GET /reports/sla-compliance` 上那個要拿去談合約的百分比。在這支端點
//! 存在之前，維護 policy 的唯一方式是寫 migration —— 也就是把合約數字
//! 寫死在程式碼裡。
//!
//! 直接後果是種子只覆蓋 `CRITICAL`／`HIGH`／`MEDIUM`，於是 `LOW` 與
//! `URGENT` 的工單一律 `NOT_APPLICABLE`：不進分母、不被掃描、不會升級。
//! 補那兩筆不該是再寫一個 migration，而是讓管理者自己定義。
//!
//! # 為什麼放在 fms-workorder
//!
//! `sla_policies` 只有一個領域消費者（工單的目標時刻），報表是它的下游。
//! 這與 `fms-catalogue` 的處境不同 —— 那裡的 `service_items` 有兩個消費者
//! 而兩者都不擁有它。這裡有明確的擁有者。
//!
//! # 兩個範圍規則
//!
//! `sla_policy:write` 宣告 `FACILITY`（032 的解析順序刻意讓場域專屬的
//! policy 優先，若要求 TENANT 那條設計就沒有人走得到）。但**租戶通用的
//! policy（`facility_id IS NULL`）影響每一個場域**，因此那一類額外要求
//! TENANT 範圍。搬移 policy 的場域則要求對**新舊兩端**都有權限。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, require_permission, require_tenant_scoped_permission, Caller, FieldError,
    Problem, TenantTx,
};

#[derive(Clone)]
pub struct SlaPolicyState {
    pub pool: PgPool,
}

const PRIORITIES: [&str; 5] = ["LOW", "MEDIUM", "HIGH", "URGENT", "CRITICAL"];

// =============================================================================
// DTO
// =============================================================================

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SlaPolicyDto {
    pub id: Uuid,
    pub facility_id: Option<Uuid>,
    pub code: String,
    pub name: String,
    pub applies_to_priority: Option<String>,
    pub response_minutes: i32,
    pub resolution_minutes: i32,
    pub business_hours_only: bool,
    pub escalation_rules: serde_json::Value,
    pub is_active: bool,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    #[serde(default)]
    pub include_inactive: bool,
}

#[derive(Debug, Deserialize)]
pub struct SlaPolicyCreate {
    pub facility_id: Option<Uuid>,
    pub code: String,
    pub name: String,
    pub applies_to_priority: Option<String>,
    pub response_minutes: i32,
    pub resolution_minutes: i32,
    #[serde(default = "default_true")]
    pub business_hours_only: bool,
    #[serde(default = "empty_array")]
    pub escalation_rules: serde_json::Value,
}

fn default_true() -> bool {
    true
}
fn empty_array() -> serde_json::Value {
    serde_json::json!([])
}

/// PATCH 的主體。
///
/// `facility_id` 與 `applies_to_priority` 用 `Option<Option<T>>`：對這兩個
/// 欄位而言 **「沒有提供」與「明確設為 null」是不同的意思**，而後者是範圍上的
/// 放大 —— `facility_id: null` 把一個場域政策變成影響全租戶的政策。
/// 用單層 `Option` 會讓那兩件事在型別上無法區分，於是權限檢查也就分不出來。
#[derive(Debug, Deserialize)]
pub struct SlaPolicyUpdate {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub facility_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "double_option")]
    pub applies_to_priority: Option<Option<String>>,
    pub response_minutes: Option<i32>,
    pub resolution_minutes: Option<i32>,
    pub business_hours_only: Option<bool>,
    pub escalation_rules: Option<serde_json::Value>,
    pub is_active: Option<bool>,
}

fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

// =============================================================================
// 驗證與錯誤轉譯
// =============================================================================

fn check_priority(value: Option<&str>) -> Result<(), Problem> {
    match value {
        None => Ok(()),
        Some(p) if PRIORITIES.contains(&p) => Ok(()),
        Some(p) => Err(Problem::validation(format!(
            "`applies_to_priority` 必須是 {} 之一",
            PRIORITIES.join("／")
        ))
        .with_errors(vec![FieldError {
            pointer: "/applies_to_priority".to_string(),
            code: "ENUM".to_string(),
            message: format!("`{p}` 不是合法的優先度"),
        }])),
    }
}

fn check_minutes(field: &str, value: i32) -> Result<(), Problem> {
    if value >= 1 {
        return Ok(());
    }
    Err(
        Problem::validation(format!("`{field}` 必須至少 1 分鐘")).with_errors(vec![FieldError {
            pointer: format!("/{field}"),
            code: "MINIMUM".to_string(),
            message: format!("{value} 不是有效的分鐘數"),
        }]),
    )
}

/// 把資料庫的約束違反轉成契約形狀的錯誤。
///
/// 三個約束各對應一個管理者會真的犯的錯，而**預設的映射對兩個是錯的**：
///
/// * `ck_sla_escalation_rules`（036）是 `23514`，而 `fms-shared` 的通用映射
///   對不認識的 `23514` 會落到 `Problem::internal` → **500**。一個打錯字的
///   `at_pct` 不該是伺服器錯誤。
/// * 兩個唯一索引都是 `23505` → 通用映射給的是「a conflicting record already
///   exists」，而管理者需要知道是**代碼重複**還是**該 (場域, 優先度) 已經有
///   一個生效的政策**。後者是 037 加的，而它防的正是「第二個政策靜默沒有
///   作用」——如果錯誤訊息不說清楚，那個保護就只是換一種方式讓人困惑。
///
/// 用約束名稱當接點，因此規則本身只有一份（在資料庫裡）。
fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        Some("ck_sla_escalation_rules") => Problem::validation(
            "`escalation_rules` 必須是物件陣列，每個物件帶一個 0 到 100 之間的數值 `at_pct`",
        )
        .with_errors(vec![FieldError {
            pointer: "/escalation_rules".to_string(),
            code: "SHAPE".to_string(),
            message: "缺少 at_pct、非數值、或超出 (0,100] 的規則永遠不會生效".to_string(),
        }]),
        Some("uq_sla_policies_code") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail("該代碼已被使用（同一租戶內不分大小寫）"),
        Some("uq_sla_policies_scope") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail(
                "該 (場域, 優先度) 已經有一個生效的政策。\
                 同組只能有一個，否則哪一個生效會取決於代碼字典序 —— \
                 請先停用舊的（is_active: false）",
            ),
        _ => Problem::from(err),
    }
}

// =============================================================================
// 範圍檢查
// =============================================================================

/// 對某個 policy 範圍的寫入權限。
///
/// `None`（租戶通用）要求 TENANT 範圍：那一類 policy 會套用到每一個場域，
/// 而 026 之後 `require_permission(.., None, None)` 只檢查「在任何範圍持有」
/// —— 對一個場域管理員來說那會通過，而他不該能改全租戶的合約條款。
async fn require_scope(tx: &mut TenantTx, facility_id: Option<Uuid>) -> Result<(), Problem> {
    match facility_id {
        Some(fid) => require_permission(tx, "sla_policy:write", Some(fid), None)
            .await
            .map(|_| ()),
        None => require_tenant_scoped_permission(tx, "sla_policy:write")
            .await
            .map(|_| ()),
    }
}

// =============================================================================
// Handlers
// =============================================================================

const COLUMNS: &str = "id, facility_id, code, name, applies_to_priority,
                       response_minutes, resolution_minutes, business_hours_only,
                       escalation_rules, is_active";

/// `GET /sla-policies`
///
/// 不分頁：policy 的數量級是「每個場域每個優先度一筆」，而且客戶端需要全部
/// 才能顯示「哪些優先度還沒有政策」—— 那正是這支端點最重要的用途
/// （目前 `LOW` 與 `URGENT` 就是空的）。
pub async fn list(
    State(state): State<SlaPolicyState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "sla_policy:read", q.facility_id, None).await?;

    // 場域範圍的使用者由 RLS 自動收斂（`sla_policies` 有 facility_scope 政策，
    // 而 `facility_in_scope(NULL)` 為真，因此租戶通用的政策對所有人可見 ——
    // 那是必要的，否則同一張工單的 SLA 目標會取決於是誰開的單）。
    let rows: Vec<SlaPolicyDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM fms.sla_policies
          WHERE ($1::uuid IS NULL OR facility_id = $1::uuid)
            AND ($2::bool OR is_active)
          ORDER BY facility_id NULLS FIRST, applies_to_priority NULLS FIRST, code"
    ))
    .bind(q.facility_id)
    .bind(q.include_inactive)
    .fetch_all(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": rows })))
}

/// `POST /sla-policies`
pub async fn create(
    State(state): State<SlaPolicyState>,
    caller: Caller,
    Json(body): Json<SlaPolicyCreate>,
) -> Result<(StatusCode, Json<SlaPolicyDto>), Problem> {
    check_priority(body.applies_to_priority.as_deref())?;
    check_minutes("response_minutes", body.response_minutes)?;
    check_minutes("resolution_minutes", body.resolution_minutes)?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_scope(&mut tx, body.facility_id).await?;

    let row: SlaPolicyDto = sqlx::query_as(&format!(
        "INSERT INTO fms.sla_policies
           (tenant_id, facility_id, code, name, applies_to_priority,
            response_minutes, resolution_minutes, business_hours_only, escalation_rules)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING {COLUMNS}"
    ))
    .bind(body.facility_id)
    .bind(&body.code)
    .bind(&body.name)
    .bind(&body.applies_to_priority)
    .bind(body.response_minutes)
    .bind(body.resolution_minutes)
    .bind(body.business_hours_only)
    .bind(&body.escalation_rules)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `PATCH /sla-policies/{slaPolicyId}`
///
/// **改分鐘數不影響已經開立的工單** —— `response_due_at`／`resolution_due_at`
/// 在開單時就算成絕對時刻（ADR-12 決定 F 的快照）。這不是實作上的疏漏，
/// 是刻意的：合約報表不能因為今天調了政策而回溯改變。
pub async fn update(
    State(state): State<SlaPolicyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SlaPolicyUpdate>,
) -> Result<Json<SlaPolicyDto>, Problem> {
    if let Some(p) = &body.applies_to_priority {
        check_priority(p.as_deref())?;
    }
    if let Some(m) = body.response_minutes {
        check_minutes("response_minutes", m)?;
    }
    if let Some(m) = body.resolution_minutes {
        check_minutes("resolution_minutes", m)?;
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let current: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT facility_id FROM fms.sla_policies WHERE id = $1")
            .bind(id)
            .fetch_optional(tx.conn())
            .await?;
    let current = current.ok_or_else(|| Problem::not_found("SLA policy not found"))?;

    // 舊範圍要有權限 —— 否則就是改別人的合約條款。
    require_scope(&mut tx, current).await?;
    // 若要搬到別的範圍，新範圍也要有權限。少了這一步，一個場域管理員可以
    // 把自己場域的政策改成 `facility_id: null`，於是它套用到全租戶 ——
    // 那是用一次 PATCH 完成的權限放大。
    if let Some(target) = body.facility_id {
        if target != current {
            require_scope(&mut tx, target).await?;
        }
    }

    let row: SlaPolicyDto = sqlx::query_as(&format!(
        "UPDATE fms.sla_policies SET
           name                = coalesce($2, name),
           facility_id         = CASE WHEN $3::bool THEN $4::uuid ELSE facility_id END,
           applies_to_priority = CASE WHEN $5::bool THEN $6::text ELSE applies_to_priority END,
           response_minutes    = coalesce($7, response_minutes),
           resolution_minutes  = coalesce($8, resolution_minutes),
           business_hours_only = coalesce($9, business_hours_only),
           escalation_rules    = coalesce($10, escalation_rules),
           is_active           = coalesce($11, is_active),
           updated_at          = clock_timestamp()
         WHERE id = $1
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(&body.name)
    // `facility_id` 與 `applies_to_priority` 不能用 coalesce：null 是有意義的值
    // （租戶通用／所有優先度）。因此用一個布林旗標表達「這個欄位有被提供」。
    .bind(body.facility_id.is_some())
    .bind(body.facility_id.flatten())
    .bind(body.applies_to_priority.is_some())
    .bind(body.applies_to_priority.clone().flatten())
    .bind(body.response_minutes)
    .bind(body.resolution_minutes)
    .bind(body.business_hours_only)
    .bind(&body.escalation_rules)
    .bind(body.is_active)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok(Json(row))
}
