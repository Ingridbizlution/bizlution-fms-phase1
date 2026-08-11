//! 維護計畫端點（WBS 5.x）。契約中的三支：
//! `GET/POST /maintenance-plans`、
//! `GET /maintenance-plans/{planId}/preview-schedule`。

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, concurrency, page, require_permission, Caller, Cursor, PageMeta,
    Problem, SortSpec,
};

use crate::dto::*;
use crate::repo;
use crate::schedule::{self, PlanSchedule};

#[derive(Clone)]
pub struct MaintenanceState {
    pub pool: PgPool,
}

const ENDPOINT: &str = "POST /maintenance-plans";

/// `maintenance_plans.trigger_type` 的 CHECK 允許值。
const TRIGGER_TYPES: &[&str] = &["CALENDAR", "METER", "CONDITION", "HYBRID"];
const PRIORITIES: &[&str] = &["LOW", "MEDIUM", "HIGH", "URGENT", "CRITICAL"];

/// preview 的上界，對齊契約（default 12、maximum 100）。
const PREVIEW_DEFAULT: u16 = 12;
const PREVIEW_MAX: u16 = 100;

fn to_dto(p: repo::PlanRow) -> PlanDto {
    PlanDto {
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
        completion_grace_days: p.completion_grace_days,
        priority: p.priority,
        assigned_team_id: p.assigned_team_id,
        next_due_at: p.next_due_at,
        is_active: p.is_active,
    }
}

/// `GET /maintenance-plans`
pub async fn list(
    State(state): State<MaintenanceState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    if let Some(t) = q.trigger_type.as_deref() {
        if !TRIGGER_TYPES.contains(&t) {
            return Err(Problem::validation(format!(
                "invalid trigger_type `{t}`; allowed: {TRIGGER_TYPES:?}"
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "maintenance_plan:read", q.facility_id, None).await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "code".to_string(),
        desc: false,
    };
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };

    let rows = repo::list(
        &mut tx,
        q.facility_id,
        q.trigger_type.as_deref(),
        q.due_before,
        cursor.as_ref(),
        limit,
    )
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.code.clone(), r.id));
    let data: Vec<PlanDto> = paged.data.into_iter().map(to_dto).collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

/// `POST /maintenance-plans`
///
/// CALENDAR／HYBRID 型計畫在建立時就展開 RRULE 的第一個時刻並寫入
/// `next_due_at`。理由是產生器完全以 `next_due_at` 驅動：
/// 若建立時留空，計畫會靜靜地永遠不產生任何工單，
/// 而使用者看不出哪裡不對。
pub async fn create(
    State(state): State<MaintenanceState>,
    caller: Caller,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let w: PlanCreate = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid MaintenancePlanCreate: {e}")))?;

    // 契約的 required: [facility_id, template_id, code, name, trigger_type]
    let facility_id = w
        .facility_id
        .ok_or_else(|| Problem::validation("facility_id is required"))?;
    let template_id = w
        .template_id
        .ok_or_else(|| Problem::validation("template_id is required"))?;
    let code = w
        .code
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Problem::validation("code is required"))?;
    let name = w
        .name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Problem::validation("name is required"))?;
    let trigger_type = w
        .trigger_type
        .as_deref()
        .ok_or_else(|| Problem::validation("trigger_type is required"))?;

    if !TRIGGER_TYPES.contains(&trigger_type) {
        return Err(Problem::validation(format!(
            "invalid trigger_type `{trigger_type}`; allowed: {TRIGGER_TYPES:?}"
        )));
    }
    if let Some(p) = w.priority.as_deref() {
        if !PRIORITIES.contains(&p) {
            return Err(Problem::validation(format!(
                "invalid priority `{p}`; allowed: {PRIORITIES:?}"
            )));
        }
    }

    // ck_plan_trigger：CALENDAR 要 rrule、METER 要 meter_code + threshold。
    // 先擋才不會把約束違反變成 500。
    match trigger_type {
        "CALENDAR" if w.rrule.is_none() => {
            return Err(Problem::validation("rrule is required for CALENDAR plans"))
        }
        "METER" if w.meter_code.is_none() || w.meter_threshold.is_none() => {
            return Err(Problem::validation(
                "meter_code and meter_threshold are required for METER plans",
            ))
        }
        _ => {}
    }

    let idem_key = concurrency::key_from(&headers)?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // 冪等**登記**在最前面；**回放**必須等授權跑完
    // （見 docs/security-review-open-items.md 第 1 項與 PendingReplay）。
    let pending = match idem_key.as_deref() {
        Some(key) => concurrency::begin(&mut tx, key, ENDPOINT, &raw)
            .await?
            .pending(),
        None => None,
    };

    let auth =
        require_permission(&mut tx, "maintenance_plan:write", Some(facility_id), None).await?;

    if let Some(replay) = pending {
        let (code, body) = replay.release(auth);
        tx.commit().await?;
        return Ok((code, Json(body)));
    }

    // ck_plan_target：三種瞄準模式恰好一種。
    let category_id = match w.category_code.as_deref() {
        Some(c) => Some(
            repo::resolve_category(&mut tx, c)
                .await?
                .ok_or_else(|| Problem::validation(format!("unknown category_code: {c}")))?,
        ),
        None => None,
    };
    let targets = [
        w.asset_id.is_some(),
        w.spatial_node_id.is_some(),
        category_id.is_some(),
    ]
    .iter()
    .filter(|t| **t)
    .count();
    if targets != 1 {
        return Err(Problem::validation(
            "exactly one of asset_id, spatial_node_id or category_code must be supplied",
        ));
    }

    // CALENDAR／HYBRID：算出首次到期時刻。
    let next_due_at = match (trigger_type, w.rrule.as_deref()) {
        ("CALENDAR" | "HYBRID", Some(rrule)) => {
            let timezone = repo::facility_timezone(&mut tx, facility_id)
                .await?
                .ok_or_else(|| Problem::validation("unknown facility_id"))?;
            let times = schedule::expand(
                &PlanSchedule {
                    rrule,
                    // 起點是「現在」：計畫從建立之後才開始適用，
                    // 用更早的起點會讓第一批工單一被建立就已逾期。
                    dtstart: chrono::Utc::now(),
                    timezone: &timezone,
                },
                2,
                None,
            )?;
            let first = times.into_iter().next().ok_or_else(|| {
                // 例如 `UNTIL` 已過期的規則：語法對但展開為空。
                // 這是使用者可修正的輸入問題，因此 422。
                Problem::validation(
                    "the supplied rrule expands to no future occurrence; \
                     check UNTIL/COUNT",
                )
            })?;
            Some(first)
        }
        _ => None,
    };

    let id = repo::create(
        &mut tx,
        repo::NewPlan {
            facility_id,
            template_id,
            code,
            name,
            asset_id: w.asset_id,
            spatial_node_id: w.spatial_node_id,
            category_id,
            trigger_type,
            rrule: w.rrule.as_deref(),
            meter_code: w.meter_code.as_deref(),
            meter_threshold: w.meter_threshold,
            generate_lead_days: w.generate_lead_days,
            completion_grace_days: w.completion_grace_days,
            priority: w.priority.as_deref(),
            assigned_team_id: w.assigned_team_id,
            sla_policy_id: w.sla_policy_id,
            next_due_at,
        },
    )
    .await?;

    let created = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("plan vanished after insert")))?;
    let body = serde_json::to_value(to_dto(created)).map_err(Problem::internal)?;

    if let Some(key) = idem_key.as_deref() {
        concurrency::complete(&mut tx, key, ENDPOINT, 201, &body).await?;
    }
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(body)))
}

/// `GET /maintenance-plans/{planId}/preview-schedule`
///
/// 契約的用途是「讓管理員在啟用計畫前確認 RRULE 展開結果，
/// 避免產生大量錯誤工單」。因此它**必須**用與產生器同一份展開邏輯
/// （`schedule::expand`）與同一份瞄準規則（`repo::target_assets`）——
/// 各算一次的話，preview 通過而產生器出錯正是它要防的事。
///
/// 回傳是「時刻 × 設備」的笛卡兒積，因為那正是產生器會開的工單張數：
/// 瞄準 4 樓 8 台空調、預覽 12 期，就是 96 張。管理員需要看到這個數字。
pub async fn preview_schedule(
    State(state): State<MaintenanceState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Query(q): Query<PreviewQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let limit = q.limit.unwrap_or(PREVIEW_DEFAULT).clamp(1, PREVIEW_MAX);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let plan = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("maintenance plan not found"))?;
    require_permission(
        &mut tx,
        "maintenance_plan:read",
        Some(plan.facility_id),
        None,
    )
    .await?;

    let Some(rrule) = plan.rrule.as_deref() else {
        // 非日曆型計畫沒有排程可展開。回空陣列而不是 422：
        // 「這個計畫不是靠日曆觸發的」是正常狀態，不是請求錯誤。
        tx.commit().await?;
        return Ok(Json(serde_json::json!({ "data": [] })));
    };

    let until = q
        .until
        .map(|d| {
            // `until` 是日期，涵蓋當天整日，因此取當日 23:59:59。
            schedule::local_to_utc(
                d.and_hms_opt(23, 59, 59)
                    .ok_or_else(|| Problem::validation("invalid until date"))?,
                &plan.facility_timezone,
            )
        })
        .transpose()?;

    let times = schedule::expand(
        &PlanSchedule {
            rrule,
            dtstart: plan.next_due_at.unwrap_or(plan.created_at),
            timezone: &plan.facility_timezone,
        },
        limit,
        until,
    )?;
    let assets = repo::asset_codes_for(&mut tx, id).await?;
    tx.commit().await?;

    let data: Vec<PreviewItemDto> = times
        .iter()
        .flat_map(|t| {
            assets.iter().map(move |a| PreviewItemDto {
                scheduled_for: *t,
                asset_id: a.0,
                asset_code: a.1.clone(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "data": data })))
}

// -----------------------------------------------------------------------------
// PATCH /maintenance-plans/{planId}
// -----------------------------------------------------------------------------

/// 可更新的欄位。**`facility_id`、`template_id`、`trigger_type` 不可改** ——
/// 那三者變更會讓既有的占位與工單失去意義（占位的唯一鍵含 plan_id，
/// 而觸發型別決定 `next_due_at` 的意義）。要換就建新計畫並停用舊的。
#[derive(Debug, serde::Deserialize)]
pub struct PlanPatch {
    pub name: Option<String>,
    pub rrule: Option<String>,
    pub meter_code: Option<String>,
    pub meter_threshold: Option<f64>,
    pub generate_lead_days: Option<i32>,
    /// 063 新增。改它會**立刻改變歷史的合規判定** —— 見 handler 註解。
    pub completion_grace_days: Option<i32>,
    pub priority: Option<String>,
    pub assigned_team_id: Option<Uuid>,
    pub sla_policy_id: Option<Uuid>,
    pub is_active: Option<bool>,
}

/// `PATCH /maintenance-plans/{planId}`
///
/// # 改 `completion_grace_days` 會改變**過去**的數字
///
/// 合規報表是即時算的（`report_pm_compliance` 讀計畫當下的容許窗），
/// 所以把容許窗從 0 改成 7 之後，上個月的準時率會跟著上升。
///
/// 那是刻意的，而且是兩害相權的結果：另一種做法是把容許窗快照到每一筆
/// occurrence 上，讓歷史凍結。但那會讓「我們決定月保養容許 7 天」
/// 這個政策變更**無法回溯套用**，於是管理者改完設定看不到任何效果，
/// 只能等新的期次累積 —— 而他改設定的目的通常正是想重新評估現況。
///
/// 回應的 `meta` 與報表的 `meta` 都會回報實際使用的容許窗，
/// 所以那個數字始終是可解釋的。真正需要凍結的稽核用途應該匯出報表快照
/// （`POST /reports/{reportCode}:export`，尚未實作）。
pub async fn patch_plan(
    State(state): State<MaintenanceState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<PlanPatch>,
) -> Result<Json<serde_json::Value>, Problem> {
    if let Some(g) = body.completion_grace_days {
        if !(0..=365).contains(&g) {
            return Err(Problem::validation("completion_grace_days 必須是 0 到 365"));
        }
    }
    if let Some(d) = body.generate_lead_days {
        if !(0..=365).contains(&d) {
            return Err(Problem::validation("generate_lead_days 必須是 0 到 365"));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let plan = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這個保養計畫（或它不在你的範圍內）"))?;
    require_permission(
        &mut tx,
        "maintenance_plan:write",
        Some(plan.facility_id),
        None,
    )
    .await?;

    // 改 rrule 時一併重算 `next_due_at`：留著舊值會讓計畫按舊排程再產生一次，
    // 而使用者以為自己已經改掉了。與 `create` 的理由相同（見那支的檔頭）。
    let recompute_due = body.rrule.is_some();

    sqlx::query(
        "UPDATE fms.maintenance_plans SET
            name = coalesce($2, name),
            rrule = coalesce($3, rrule),
            meter_code = coalesce($4, meter_code),
            meter_threshold = coalesce($5::float8::numeric, meter_threshold),
            generate_lead_days = coalesce($6::int::smallint, generate_lead_days),
            completion_grace_days = coalesce($7::int::smallint, completion_grace_days),
            priority = coalesce($8, priority),
            assigned_team_id = coalesce($9, assigned_team_id),
            sla_policy_id = coalesce($10, sla_policy_id),
            is_active = coalesce($11, is_active),
            updated_at = clock_timestamp()
          WHERE id = $1",
    )
    .bind(id)
    .bind(body.name.as_deref())
    .bind(body.rrule.as_deref())
    .bind(body.meter_code.as_deref())
    .bind(body.meter_threshold)
    .bind(body.generate_lead_days)
    .bind(body.completion_grace_days)
    .bind(body.priority.as_deref())
    .bind(body.assigned_team_id)
    .bind(body.sla_policy_id)
    .bind(body.is_active)
    .execute(tx.conn())
    .await?;

    if recompute_due {
        let fresh = repo::get(&mut tx, id)
            .await?
            .ok_or_else(|| Problem::not_found("找不到這個保養計畫"))?;
        if let Some(rrule) = fresh.rrule.as_deref() {
            let next = schedule::expand(
                &PlanSchedule {
                    rrule,
                    // 起點是「現在」：改了排程之後才開始適用，
                    // 與 `create` 的理由相同。
                    dtstart: chrono::Utc::now(),
                    timezone: &fresh.facility_timezone,
                },
                1,
                None,
            )
            .map_err(|e| Problem::validation(format!("rrule 無效：{e}")))?
            .into_iter()
            .next();
            sqlx::query("UPDATE fms.maintenance_plans SET next_due_at = $2 WHERE id = $1")
                .bind(id)
                .bind(next)
                .execute(tx.conn())
                .await?;
        }
    }

    let row = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這個保養計畫"))?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": to_dto(row) })))
}

// -----------------------------------------------------------------------------
// POST /maintenance-plans/{planId}/generate-now
// -----------------------------------------------------------------------------

/// `POST /maintenance-plans/{planId}/generate-now`
///
/// 立刻產生下一期的工單，不等排程掃描。
///
/// **與掃描走同一條路徑**（`generator::generate_for`）—— 不是另寫一份。
/// 各寫一份的話，手動產生的工單會與自動產生的在某個細節上不同
/// （占位、SLA、瞄準的設備清單），而那種差異只在事後對帳時才看得出來。
///
/// 冪等由占位的唯一鍵保證（`(plan, asset, scheduled_for)`）：
/// 連按兩次不會產生兩張工單，第二次的 `created` 會是 0。
pub async fn generate_now(
    State(state): State<MaintenanceState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let plan = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這個保養計畫（或它不在你的範圍內）"))?;
    require_permission(
        &mut tx,
        "maintenance_plan:write",
        Some(plan.facility_id),
        None,
    )
    .await?;

    if !plan.is_active {
        return Err(Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail("這個計畫已停用 —— 停用的計畫不該產生工單"));
    }

    // 用計畫自己的 `next_due_at` 當排定時刻，而不是「現在」。
    //
    // 理由是合規判定：occurrence 的 `scheduled_for` 是準時與否的基準。
    // 若手動產生時填「現在」，那一期就永遠是準時的 —— 一個逾期三週才
    // 想起來要做的保養，按下這個按鈕就會變成準時，而合規率是拿去談合約的。
    let scheduled_for = plan.next_due_at.ok_or_else(|| {
        Problem::new(fms_shared::ProblemCode::Conflict).with_detail(
            "這個計畫沒有 next_due_at（日曆序列已走完，或不是日曆／混合型）—— \
             沒有排定時刻就沒有準時與否可判定",
        )
    })?;

    let generated = crate::generator::generate_for(&mut tx, &plan, scheduled_for).await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        // `Generated` 回的是 id 清單而不是計數 —— 回傳 id 而不只是數字，
        // 因為呼叫端按下按鈕之後通常要直接跳到那張工單。
        "created": generated.work_order_ids.len(),
        "work_order_ids": generated.work_order_ids,
        "occurrence_ids": generated.occurrence_ids,
        "skipped": generated.skipped,
        "scheduled_for": scheduled_for,
        "meta": {
            // 說出用的是計畫的排定時刻，不是「現在」—— 見 handler 註解。
            "scheduled_for_source": "plan.next_due_at",
        },
    })))
}
