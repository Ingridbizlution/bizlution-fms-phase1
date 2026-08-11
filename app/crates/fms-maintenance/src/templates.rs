//! 保養範本（`/maintenance-templates`）。
//!
//! # 契約列了這兩支，而那張表只有 seed 寫過
//!
//! `fms.maintenance_templates` 從 004 就存在，seed 009 放了 2 筆
//! （平台範本 0 筆）。`POST /maintenance-plans` 可以引用 `template_id`，
//! 但**沒有任何端點建立範本** —— 也就是說一份新的保養程序要上線，
//! 得有人連進資料庫。
//!
//! # `required_skill_codes` 在此之前零讀者
//!
//! 那一欄的意思是「做這件保養需要這些證照」。seed 寫過它，
//! 而**整個 codebase 沒有任何地方讀它** —— 包括派工。
//!
//! 這裡不打算補上「派工時檢查技師的證照有沒有過期」（那是另一件事，
//! 需要動工單的指派路徑），但**建立範本時會驗那些代碼存在** ——
//! 與 `alarm_rules` 的 `notify_role_codes` 同一個判斷：
//! 一個指向不存在技能的必要條件，等於沒有那個條件。
//!
//! 誠實記下缺口：**驗了代碼存在，沒有驗執行者真的持有它**。
//! 059 已經讓證照到期看得見，那條鏈的最後一步（指派時擋下）還沒接。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, Problem};

use crate::handlers::MaintenanceState;

const MAINTENANCE_TYPES: [&str; 6] = [
    "PREVENTIVE",
    "INSPECTION",
    "CALIBRATION",
    "DEEP_CLEAN",
    "STATUTORY",
    "PREDICTIVE",
];

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TemplateDto {
    pub id: Uuid,
    /// null = 平台範本（所有租戶共用）。目前 0 筆 —— 記在這裡是因為
    /// 「平台範本」這個概念存在但還沒有內容。
    pub tenant_id: Option<Uuid>,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub applies_to_category_id: Option<Uuid>,
    pub applies_to_model_id: Option<Uuid>,
    pub maintenance_type: String,
    pub checklist: serde_json::Value,
    pub estimated_minutes: i32,
    pub required_skill_codes: Vec<String>,
    pub required_part_codes: Vec<String>,
    pub safety_notes: Option<String>,
    pub requires_permit: bool,
    pub requires_shutdown: bool,
    pub is_active: bool,
    /// 有幾個計畫在用這份範本。0 代表建了沒人用 ——
    /// 那不是錯誤，但值得看得見。
    pub plan_count: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub maintenance_type: Option<String>,
    pub is_active: Option<bool>,
    pub applies_to_category_id: Option<Uuid>,
    /// 只回傳沒有任何計畫在用的範本。
    pub unused_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct TemplateCreate {
    pub code: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub applies_to_category_id: Option<Uuid>,
    pub applies_to_model_id: Option<Uuid>,
    pub maintenance_type: Option<String>,
    pub checklist: Option<serde_json::Value>,
    pub estimated_minutes: Option<i32>,
    pub required_skill_codes: Option<Vec<String>>,
    pub required_part_codes: Option<Vec<String>>,
    pub safety_notes: Option<String>,
    pub requires_permit: Option<bool>,
    pub requires_shutdown: Option<bool>,
    pub is_active: Option<bool>,
}

const COLUMNS: &str = "t.id, t.tenant_id, t.code::text AS code, t.name::text AS name,
                       t.description, t.applies_to_category_id, t.applies_to_model_id,
                       t.maintenance_type, t.checklist, t.estimated_minutes,
                       t.required_skill_codes, t.required_part_codes,
                       t.safety_notes, t.requires_permit, t.requires_shutdown,
                       t.is_active,
                       (SELECT count(*) FROM fms.maintenance_plans mp
                         WHERE mp.template_id = t.id) AS plan_count,
                       t.created_at";

/// `GET /maintenance-templates`
pub async fn list(
    State(state): State<MaintenanceState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    if let Some(t) = q.maintenance_type.as_deref() {
        if !MAINTENANCE_TYPES.contains(&t.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "maintenance_type 必須是 {} 其中之一",
                MAINTENANCE_TYPES.join("／")
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 範本沒有場域維度（它是一份程序），所以範圍用 None ——
    // 場域管理員也該查得到。
    require_permission(&mut tx, "maintenance_plan:read", None, None).await?;

    let rows: Vec<TemplateDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS}
           FROM fms.maintenance_templates t
          WHERE ($1::text IS NULL OR t.maintenance_type = upper($1::text))
            AND ($2::bool IS NULL OR t.is_active = $2::bool)
            AND ($3::uuid IS NULL OR t.applies_to_category_id = $3::uuid)
            AND (NOT $4::bool OR NOT EXISTS (
                  SELECT 1 FROM fms.maintenance_plans mp WHERE mp.template_id = t.id))
          ORDER BY t.tenant_id NULLS FIRST, t.code"
    ))
    .bind(q.maintenance_type.as_deref())
    .bind(q.is_active)
    .bind(q.applies_to_category_id)
    .bind(q.unused_only.unwrap_or(false))
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

/// `POST /maintenance-templates`
pub async fn create(
    State(state): State<MaintenanceState>,
    caller: Caller,
    Json(body): Json<TemplateCreate>,
) -> Result<(StatusCode, Json<TemplateDto>), Problem> {
    let code = required(&body.code, "code")?;
    let name = required(&body.name, "name")?;
    let mtype = body
        .maintenance_type
        .as_deref()
        .map(str::to_uppercase)
        .unwrap_or_else(|| "PREVENTIVE".to_string());
    if !MAINTENANCE_TYPES.contains(&mtype.as_str()) {
        return Err(Problem::validation(format!(
            "maintenance_type 必須是 {} 其中之一",
            MAINTENANCE_TYPES.join("／")
        )));
    }
    // 檢查清單是這份範本的**內容**。空的範本產出的工單沒有任何檢查項，
    // 而那張工單被簽掉時沒有人知道實際做了什麼。
    let checklist = body
        .checklist
        .clone()
        .unwrap_or_else(|| serde_json::json!([]));
    let items = checklist
        .as_array()
        .ok_or_else(|| Problem::validation("checklist 必須是陣列"))?;
    if items.is_empty() {
        return Err(Problem::validation(
            "checklist 不能是空的 —— 沒有檢查項的範本產出的工單，\
             被簽掉時沒有人知道實際做了什麼",
        ));
    }
    if let Some(m) = body.estimated_minutes {
        if !(1..=10_080).contains(&m) {
            return Err(Problem::validation(
                "estimated_minutes 必須是 1 到 10080（一週）",
            ));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "maintenance_template:write", None, None).await?;

    // 指向不存在技能的必要條件，等於沒有那個條件。
    // 與 `alarm_rules` 的 `notify_role_codes` 同一個判斷。
    if let Some(codes) = body.required_skill_codes.as_deref() {
        let unknown: Vec<String> = sqlx::query_scalar(
            "SELECT c FROM unnest($1::text[]) AS c
              WHERE NOT EXISTS (SELECT 1 FROM fms.skills s WHERE upper(s.code) = upper(c))",
        )
        .bind(codes)
        .fetch_all(tx.conn())
        .await?;
        if !unknown.is_empty() {
            return Err(Problem::validation(format!(
                "required_skill_codes 裡這些技能不存在：{} —— \
                 留著它們等於宣告了一個沒有人會檢查的必要條件",
                unknown.join("、")
            )));
        }
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.maintenance_templates
           (tenant_id, code, name, description, applies_to_category_id, applies_to_model_id,
            maintenance_type, checklist, estimated_minutes,
            required_skill_codes, required_part_codes,
            safety_notes, requires_permit, requires_shutdown, is_active)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7,
                 coalesce($8, 60),
                 coalesce($9::text[], '{}'::text[]), coalesce($10::text[], '{}'::text[]),
                 $11, coalesce($12, false), coalesce($13, false), coalesce($14, true))
         RETURNING id",
    )
    .bind(code)
    .bind(name)
    .bind(body.description.as_deref())
    .bind(body.applies_to_category_id)
    .bind(body.applies_to_model_id)
    .bind(&mtype)
    .bind(&checklist)
    .bind(body.estimated_minutes)
    .bind(body.required_skill_codes.as_deref())
    .bind(body.required_part_codes.as_deref())
    .bind(body.safety_notes.as_deref())
    .bind(body.requires_permit)
    .bind(body.requires_shutdown)
    .bind(body.is_active)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;

    let row: TemplateDto = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM fms.maintenance_templates t WHERE t.id = $1"
    ))
    .bind(id)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

fn required<'a>(v: &'a Option<String>, field: &str) -> Result<&'a str, Problem> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation(format!("{field} 為必填")))
}

fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        Some("uq_maintenance_templates_code") => {
            Problem::new(fms_shared::ProblemCode::Conflict).with_detail("這個 code 已被使用")
        }
        Some("maintenance_templates_applies_to_category_id_fkey") => {
            Problem::not_found("找不到這個設備分類")
        }
        Some("maintenance_templates_applies_to_model_id_fkey") => {
            Problem::not_found("找不到這個設備型號")
        }
        _ => Problem::from(err),
    }
}
