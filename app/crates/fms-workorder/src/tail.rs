//! Work Orders 補完的八支端點（狀態字典、狀態機、批次轉換、團隊、備品）。
//!
//! # `state_machine` 要說出「宣告了」與「真的會做」的差別
//!
//! 008 的 `work_order_transitions_allowed.side_effects` 宣告了七個 key，而
//! `apply_side_effects` 只執行三個（`EXECUTED_SIDE_EFFECTS`）。其餘四個
//! （`notify`、`compute_sla`、`update_asset_status`、`release_reservation_step`）
//! 是惰性的宣告。
//!
//! 這支端點的用途是「供前端繪流程圖」，所以那個差別不能被抹掉：一份把惰性宣告
//! 畫成實際行為的流程圖，會讓看圖的人以為結案時系統會自動改設備狀態。
//! 回應因此把每個 side effect 標上 `executed: true/false`，並在 `meta` 給出
//! 那份權威清單。
//!
//! # `part_stock.available` 是一個要說出前提的數字
//!
//! `quantity_reserved` 的寫入者數量是 **0**（量過：整個 `app/crates` 與
//! `sql/` 除了 004 的建表之外沒有任何地方寫它）。所以
//! `available = on_hand - reserved` 今天恆等於 `on_hand`。
//!
//! 那個欄位仍然回傳，但 `meta.reserved_is_never_written` 說出它是死的 ——
//! 少了這句，一個看到 `available` 的人會以為預留機制已經在運作，
//! 而在領料撞到短缺時才發現不是。
//!
//! # 批次轉換是「部分成功」，而每一筆的結果都要回
//!
//! `POST /work-orders:bulk-transition` 對 50 張工單做同一個動作，其中三張
//! 狀態不對。整批回滾會讓另外 47 張白做；整批回 200 而不說哪三張失敗，
//! 使用者會以為全部成功。所以每一筆一個 savepoint，回傳逐筆結果 + 三個計數
//! （`succeeded`／`failed`／`skipped`），與 `assets:bulk-import` 同一個形狀。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, FieldError, Problem};

use crate::handlers::{WorkOrderState, EXECUTED_SIDE_EFFECTS};

// =============================================================================
// GET /work-order-statuses
// =============================================================================

/// `GET /work-order-statuses`
///
/// 只需要登入 —— 這是字典，而客戶端要靠它把狀態碼翻成人看得懂的字。
/// 不擋權限的理由與 `GET /permissions` 相同：知道有哪些狀態不洩漏任何資料。
pub async fn statuses(
    State(state): State<WorkOrderState>,
    caller: Caller,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let rows = sqlx::query!(
        r#"SELECT code::text AS "code!", name_zh::text AS "name_zh!",
                  name_en::text AS "name_en!", category, is_terminal, display_order
             FROM fms.work_order_statuses
            ORDER BY display_order, code"#
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "code": r.code,
                "name_zh": r.name_zh,
                "name_en": r.name_en,
                "category": r.category,
                // 終態的定義來自這一欄，而不是硬編一份狀態清單 ——
                // 報表（`work_orders_open`）與這裡讀的是同一個欄位。
                "is_terminal": r.is_terminal,
                "display_order": r.display_order,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "terminal_source": "work_order_statuses.is_terminal",
            "count": rows.len(),
        },
    })))
}

// =============================================================================
// GET /work-order-state-machine
// =============================================================================

#[derive(Debug, Deserialize)]
pub struct StateMachineQuery {
    /// 只回這個工單型別適用的轉換（含 `work_order_type IS NULL` 的通用規則）。
    pub work_order_type: Option<String>,
}

/// `GET /work-order-state-machine`
///
/// 需要 `work_order:read`。這是 008 的 `work_order_transitions_allowed`
/// **第一次對外可讀** —— 在此之前只有 `apply_side_effects` 讀它。
///
/// 每個 side effect 都標上 `executed`，見模組檔頭。
pub async fn state_machine(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Query(q): Query<StateMachineQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "work_order:read", None, None).await?;

    // 租戶自訂的規則優先於平台預設（`tenant_id IS NULL`）—— 與 041／059 的
    // 範本解析同一個慣例。這裡兩者都回並標上 `is_tenant_override`，
    // 因為畫流程圖的人需要知道哪幾條是這個客戶特有的。
    let rows = sqlx::query!(
        r#"SELECT t.id, t.tenant_id IS NOT NULL AS "is_tenant_override!",
                  t.work_order_type::text AS work_order_type,
                  t.from_status::text AS "from_status!",
                  t.action::text AS "action!",
                  t.to_status::text AS "to_status!",
                  t.required_permission::text AS required_permission,
                  t.required_fields AS "required_fields!",
                  t.side_effects AS "side_effects!",
                  fs.name_zh::text AS "from_name!",
                  ts.name_zh::text AS "to_name!",
                  ts.is_terminal AS "to_is_terminal!"
             FROM fms.work_order_transitions_allowed t
             JOIN fms.work_order_statuses fs ON fs.code = t.from_status
             JOIN fms.work_order_statuses ts ON ts.code = t.to_status
            WHERE t.is_active
              AND ($1::text IS NULL
                   OR t.work_order_type IS NULL
                   OR t.work_order_type = $1::text)
            ORDER BY fs.display_order, t.action"#,
        q.work_order_type,
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    let mut declared_but_inert: Vec<String> = Vec::new();
    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            // side_effects 的每個 key 標上「這個系統真的會做嗎」。
            let effects: Vec<serde_json::Value> = r
                .side_effects
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| {
                            let executed = EXECUTED_SIDE_EFFECTS.contains(&k.as_str());
                            if !executed && !declared_but_inert.contains(k) {
                                declared_but_inert.push(k.clone());
                            }
                            serde_json::json!({ "key": k, "value": v, "executed": executed })
                        })
                        .collect()
                })
                .unwrap_or_default();
            serde_json::json!({
                "id": r.id,
                "work_order_type": r.work_order_type,
                "is_tenant_override": r.is_tenant_override,
                "from_status": r.from_status,
                "from_status_name": r.from_name,
                "action": r.action,
                "to_status": r.to_status,
                "to_status_name": r.to_name,
                "to_is_terminal": r.to_is_terminal,
                "required_permission": r.required_permission,
                "required_fields": r.required_fields,
                "side_effects": effects,
            })
        })
        .collect();
    declared_but_inert.sort();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "work_order_type": q.work_order_type,
            "transition_count": rows.len(),
            // **權威清單**：這三個是 `apply_side_effects` 真的會執行的。
            "executed_side_effects": EXECUTED_SIDE_EFFECTS,
            // 規則宣告了但系統不會做的。畫流程圖時這些不該被畫成行為 ——
            // 一份把它們畫成實際行為的圖，會讓人以為結案時設備狀態會自動改。
            "declared_but_inert": declared_but_inert,
            "checks_are_applied_by": "應用層（004 的 required_permission／required_fields 兩欄在資料庫端是惰性的）",
        },
    })))
}

// =============================================================================
// POST /work-orders:bulk-transition
// =============================================================================

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkTransitionRequest {
    pub work_order_ids: Vec<Uuid>,
    pub action: String,
    /// 套用到每一筆的欄位（例如 `assignee_id`）。
    #[serde(default)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct BulkOutcome {
    work_order_id: Uuid,
    ok: bool,
    /// 成功時的新狀態。
    #[serde(skip_serializing_if = "Option::is_none")]
    to_status: Option<String>,
    /// 失敗時的原因 —— **每一筆各自的原因**，不是一個整批的錯誤。
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
}

/// 一批最多幾筆。
///
/// 200 不是猜的：每一筆是一次狀態機轉換加上副作用（可能發通知），而整批在同一個
/// 交易裡。超過這個量的請求該分批送 —— 一個吃下 5000 筆的端點會讓一個
/// 打錯的請求鎖住 `work_orders` 好幾秒。
const BULK_MAX: usize = 200;

/// `POST /work-orders:bulk-transition`
///
/// 需要 `work_order:assign`（契約），**而每一筆仍然要通過那一條轉換自己的
/// `required_permission`** —— 批次不是繞過個別權限的後門。
pub async fn bulk_transition(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Json(req): Json<BulkTransitionRequest>,
) -> Result<Json<serde_json::Value>, Problem> {
    if req.work_order_ids.is_empty() {
        return Err(Problem::validation(
            "`work_order_ids` 不得為空 —— 空批次不會有任何效果",
        ));
    }
    if req.work_order_ids.len() > BULK_MAX {
        return Err(
            Problem::validation(format!("一批最多 {BULK_MAX} 筆")).with_errors(vec![FieldError {
                pointer: "/work_order_ids".to_string(),
                code: "TOO_MANY".to_string(),
                message: format!("送了 {} 筆", req.work_order_ids.len()),
            }]),
        );
    }
    if req.action.trim().is_empty() {
        return Err(Problem::validation("`action` 不得為空"));
    }
    // 重複的 id：第二次一定會失敗（狀態已經變了），而那個失敗看起來像一個
    // 真的錯誤。先擋掉比讓使用者對著一個假的失敗查半天好。
    let mut seen = std::collections::HashSet::new();
    if let Some(dup) = req.work_order_ids.iter().find(|id| !seen.insert(**id)) {
        return Err(
            Problem::validation("`work_order_ids` 有重複").with_errors(vec![FieldError {
                pointer: "/work_order_ids".to_string(),
                code: "DUPLICATE".to_string(),
                message: format!("{dup} 出現多次；第二次一定會因為狀態已變而失敗"),
            }]),
        );
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "work_order:assign", None, None).await?;

    let mut outcomes: Vec<BulkOutcome> = Vec::with_capacity(req.work_order_ids.len());
    for id in &req.work_order_ids {
        // **每一筆一個 savepoint。** 一筆失敗只回捲那一筆 ——
        // 整批回滾會讓其餘白做，而那正是批次端點存在的理由被抵銷。
        // 與 `assets:bulk-import` 同一個做法。
        sqlx::query("SAVEPOINT bulk_item")
            .execute(tx.conn())
            .await
            .map_err(Problem::from)?;

        let raw = serde_json::Value::Object(req.fields.clone());
        match crate::handlers::transition_one(&mut tx, *id, &req.action, &raw).await {
            Ok(to_status) => {
                sqlx::query("RELEASE SAVEPOINT bulk_item")
                    .execute(tx.conn())
                    .await
                    .map_err(Problem::from)?;
                outcomes.push(BulkOutcome {
                    work_order_id: *id,
                    ok: true,
                    to_status: Some(to_status),
                    error: None,
                    error_code: None,
                });
            }
            Err(p) => {
                sqlx::query("ROLLBACK TO SAVEPOINT bulk_item")
                    .execute(tx.conn())
                    .await
                    .map_err(Problem::from)?;
                outcomes.push(BulkOutcome {
                    work_order_id: *id,
                    ok: false,
                    to_status: None,
                    error: Some(
                        p.detail
                            .clone()
                            .unwrap_or_else(|| p.code.as_str().to_string()),
                    ),
                    error_code: Some(p.code.as_str().to_string()),
                });
            }
        }
    }
    tx.commit().await?;

    let succeeded = outcomes.iter().filter(|o| o.ok).count();
    let failed = outcomes.len() - succeeded;

    Ok(Json(serde_json::json!({
        "data": outcomes,
        "meta": {
            "action": req.action,
            "requested": req.work_order_ids.len(),
            // **三個數字分開。** 只回「成功 47 筆」會讓那三筆失敗的消失，
            // 而使用者會以為整批都好了。
            "succeeded": succeeded,
            "failed": failed,
            "partial_success": succeeded > 0 && failed > 0,
            "per_item_permission_still_checked": true,
        },
    })))
}

// =============================================================================
// GET /teams、/teams/{id}/workload、/teams/{id}/shifts、POST shifts
// =============================================================================

#[derive(Debug, Deserialize)]
pub struct TeamQuery {
    pub facility_id: Option<Uuid>,
    pub include_inactive: Option<bool>,
}

/// `GET /teams`
pub async fn list_teams(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Query(q): Query<TeamQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let include_inactive = q.include_inactive.unwrap_or(false);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "team:read", q.facility_id, None).await?;

    let rows = sqlx::query!(
        r#"SELECT t.id, t.facility_id, t.code::text AS "code!", t.name::text AS "name!",
                  t.team_type, t.vendor_name::text AS vendor_name, t.lead_user_id,
                  t.dispatch_rule AS "dispatch_rule!",
                  t.contact_email::text AS contact_email,
                  t.contact_phone::text AS contact_phone, t.is_active,
                  coalesce(
                    (SELECT jsonb_agg(jsonb_build_object(
                              'user_id', m.user_id,
                              'display_name', u.display_name,
                              'role_in_team', m.role_in_team,
                              'joined_at', m.joined_at)
                            ORDER BY m.role_in_team, u.display_name)
                       FROM fms.team_members m
                       JOIN fms.users u ON u.id = m.user_id
                      WHERE m.team_id = t.id),
                    '[]'::jsonb) AS "members!"
             FROM fms.teams t
            WHERE ($1::uuid IS NULL OR t.facility_id = $1::uuid OR t.facility_id IS NULL)
              AND ($2 OR t.is_active)
            ORDER BY t.name, t.code"#,
        q.facility_id,
        include_inactive,
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
                // NULL = 全租戶的團隊（004 的註解）。與 service_items 同一個慣例。
                "facility_id": r.facility_id,
                "code": r.code,
                "name": r.name,
                "team_type": r.team_type,
                "vendor_name": r.vendor_name,
                "lead_user_id": r.lead_user_id,
                // `dispatch_rule` 的 strategy 目前**沒有任何讀者** ——
                // 派工是手動指定的（`POST /work-orders/{id}/transitions` 的
                // ASSIGN 帶 assignee_id）。原樣回傳，並在 meta 說出這件事。
                "dispatch_rule": r.dispatch_rule,
                "contact_email": r.contact_email,
                "contact_phone": r.contact_phone,
                "is_active": r.is_active,
                "members": r.members,
            })
        })
        .collect();

    let with_members = rows
        .iter()
        .filter(|r| r.members.as_array().is_some_and(|a| !a.is_empty()))
        .count();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "count": rows.len(),
            "include_inactive": include_inactive,
            // 沒有成員的團隊派不了工。這個數字讓那件事看得見 ——
            // 一個空團隊在畫面上與有人的團隊長得一樣。
            "teams_with_members": with_members,
            "teams_without_members": rows.len() - with_members,
            // `dispatch_rule.strategy` 宣告了 ROUND_ROBIN／LEAST_LOADED／
            // SKILL_MATCH，而**沒有任何程式碼讀它**：派工目前是手動指定
            // assignee_id。原樣回傳但不要當成行為。
            "dispatch_rule_is_not_yet_applied": true,
        },
    })))
}

/// `GET /teams/{teamId}/workload`
///
/// 派工決策用：每個成員手上有幾張未結工單，以及依 SLA 狀態的分佈。
///
/// **分母是「未結」而不是「全部」** —— 一個做過 500 張工單的老手與一個手上壓著
/// 20 張的人，在派工時要看的是後者那個數字。
pub async fn team_workload(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(team_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let team = sqlx::query!(
        r#"SELECT facility_id, name::text AS "name!" FROM fms.teams WHERE id = $1"#,
        team_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("找不到這個團隊"))?;
    require_permission(&mut tx, "team:read", team.facility_id, None).await?;

    let rows = sqlx::query!(
        r#"SELECT m.user_id, u.display_name::text AS "display_name!",
                  m.role_in_team,
                  count(w.id) FILTER (WHERE st.is_terminal IS NOT TRUE) AS "open!",
                  count(w.id) FILTER (WHERE st.is_terminal IS NOT TRUE
                                        AND w.sla_state = 'BREACHED') AS "breached!",
                  count(w.id) FILTER (WHERE st.is_terminal IS NOT TRUE
                                        AND w.resolution_due_at < clock_timestamp())
                    AS "overdue!",
                  count(w.id) FILTER (WHERE st.is_terminal IS NOT TRUE
                                        AND w.priority IN ('URGENT','CRITICAL'))
                    AS "urgent!"
             FROM fms.team_members m
             JOIN fms.users u ON u.id = m.user_id
             LEFT JOIN fms.work_orders w
                    ON w.assignee_id = m.user_id AND w.deleted_at IS NULL
             LEFT JOIN fms.work_order_statuses st ON st.code = w.status
            WHERE m.team_id = $1
            GROUP BY m.user_id, u.display_name, m.role_in_team
            ORDER BY 4 DESC, u.display_name"#,
        team_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;

    // 團隊本身被指派的（`work_orders.team_id`）但還沒指到人的：那些是
    // 派工佇列，而它們不屬於任何成員的負載。分開回報 —— 混進成員的數字
    // 會讓「還沒有人接」看不見。
    let unassigned = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!"
             FROM fms.work_orders w
             LEFT JOIN fms.work_order_statuses st ON st.code = w.status
            WHERE w.team_id = $1 AND w.assignee_id IS NULL
              AND w.deleted_at IS NULL AND st.is_terminal IS NOT TRUE"#,
        team_id
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "user_id": r.user_id,
                "display_name": r.display_name,
                "role_in_team": r.role_in_team,
                "open_work_orders": r.open,
                "sla_breached": r.breached,
                "overdue": r.overdue,
                "urgent_or_critical": r.urgent,
            })
        })
        .collect();

    let total_open: i64 = rows.iter().map(|r| r.open).sum();
    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "team_id": team_id,
            "team_name": team.name,
            "member_count": rows.len(),
            "denominator": "未結工單（work_order_statuses.is_terminal IS NOT TRUE）",
            "total_open_across_members": total_open,
            // **還沒指到人的**。混進成員的數字會讓「派工佇列還有東西」看不見。
            "unassigned_in_team_queue": unassigned,
        },
    })))
}

#[derive(Debug, Deserialize)]
pub struct ShiftQuery {
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}

/// `GET /teams/{teamId}/shifts`
pub async fn team_shifts(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(team_id): Path<Uuid>,
    Query(q): Query<ShiftQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let from = q.from.unwrap_or_else(chrono::Utc::now);
    let to = q.to.unwrap_or_else(|| from + chrono::Duration::days(14));
    if from >= to {
        return Err(Problem::validation("`from` 必須早於 `to`"));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let team = sqlx::query!("SELECT facility_id FROM fms.teams WHERE id = $1", team_id)
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?
        .ok_or_else(|| Problem::not_found("找不到這個團隊"))?;
    require_permission(&mut tx, "team:read", team.facility_id, None).await?;

    let rows = sqlx::query!(
        r#"SELECT s.id, s.user_id, u.display_name::text AS "display_name!",
                  s.shift_start, s.shift_end, s.shift_type
             FROM fms.team_shifts s
             JOIN fms.users u ON u.id = s.user_id
            WHERE s.team_id = $1
              -- 重疊而不是包含：一個橫跨查詢區間邊界的班次仍然與那段時間相關。
              AND s.shift_range && tstzrange($2, $3, '[)')
            ORDER BY s.shift_start, u.display_name"#,
        team_id,
        from,
        to,
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
                "user_id": r.user_id,
                "display_name": r.display_name,
                "shift_start": r.shift_start,
                "shift_end": r.shift_end,
                "shift_type": r.shift_type,
            })
        })
        .collect();
    let on_leave = rows.iter().filter(|r| r.shift_type == "LEAVE").count();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "from": from, "to": to,
            "count": rows.len(),
            // 請假也是一筆班次（`shift_type = 'LEAVE'`）。派工的人要看得出
            // 「有排班」與「排的是休假」的差別 —— 兩者在清單裡長得一樣。
            "leave_entries": on_leave,
            "window_semantics": "與查詢區間**重疊**即回傳（不是被包含）",
        },
    })))
}

const SHIFT_TYPES: &[&str] = &["REGULAR", "ON_CALL", "OVERTIME", "LEAVE"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateShiftRequest {
    pub user_id: Uuid,
    pub shift_start: chrono::DateTime<chrono::Utc>,
    pub shift_end: chrono::DateTime<chrono::Utc>,
    pub shift_type: Option<String>,
}

/// `POST /teams/{teamId}/shifts`
///
/// # 同型別的重疊是錯誤，不同型別的不是
///
/// 004 沒有給 `team_shifts` 排除約束，所以重疊完全不受限。而「重疊算不算錯」
/// 取決於型別：
///
///   * 同一個人同一段時間有兩筆 `REGULAR` → **重複資料**，一定是錯的。
///   * `LEAVE` 蓋在 `REGULAR` 上 → 那正是請假的記法。
///   * `ON_CALL` 蓋在 `REGULAR` 上 → 值班與正常班同時存在是常見的。
///
/// 所以只擋同型別重疊，而跨型別的重疊在回應裡回報出來（`overlaps_other_types`）
/// —— 讓排班的人看得到，而不是替他決定那是不是錯的。
pub async fn create_shift(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(team_id): Path<Uuid>,
    Json(req): Json<CreateShiftRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let shift_type = req
        .shift_type
        .clone()
        .unwrap_or_else(|| "REGULAR".to_string());
    if !SHIFT_TYPES.contains(&shift_type.as_str()) {
        return Err(Problem::validation(format!(
            "`shift_type` 必須是 {SHIFT_TYPES:?} 之一"
        )));
    }
    if req.shift_end <= req.shift_start {
        return Err(
            Problem::validation("`shift_end` 必須晚於 `shift_start`").with_errors(vec![
                FieldError {
                    pointer: "/shift_end".to_string(),
                    code: "RANGE".to_string(),
                    message: "004 的 ck_team_shifts_range 也會擋，但這裡先擋以回 422".to_string(),
                },
            ]),
        );
    }
    // 超過 24 小時的班次幾乎一定是打錯日期。擋下來的理由與 `days` 上限相同：
    // 一個 3 個月的「班次」會讓每一次班表查詢都把它撈出來。
    if req.shift_end - req.shift_start > chrono::Duration::hours(24) {
        return Err(Problem::validation(
            "單一班次不得超過 24 小時 —— 跨多天請分成多筆（那也是班表查詢的單位）",
        ));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let team = sqlx::query!("SELECT facility_id FROM fms.teams WHERE id = $1", team_id)
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?
        .ok_or_else(|| Problem::not_found("找不到這個團隊"))?;
    require_permission(&mut tx, "team:write", team.facility_id, None).await?;

    // 排班的人必須是團隊成員 —— 給一個不在團隊裡的人排班，那筆班表在
    // `GET /teams/{id}/shifts` 會出現，但在 workload 裡不會（那支走
    // team_members），於是兩支端點對同一個人的說法不一致。
    let is_member = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM fms.team_members
                           WHERE team_id = $1 AND user_id = $2) AS "e!""#,
        team_id,
        req.user_id,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    if !is_member {
        return Err(
            Problem::validation("這個使用者不是這個團隊的成員").with_errors(vec![FieldError {
                pointer: "/user_id".to_string(),
                code: "NOT_A_MEMBER".to_string(),
                message: "非成員的班次會出現在班表卻不出現在 workload —— \
                          兩支端點對同一個人的說法會不一致"
                    .to_string(),
            }]),
        );
    }

    let overlaps = sqlx::query!(
        r#"SELECT count(*) FILTER (WHERE s.shift_type = $4) AS "same_type!",
                  count(*) FILTER (WHERE s.shift_type <> $4) AS "other_types!"
             FROM fms.team_shifts s
            WHERE s.team_id = $1 AND s.user_id = $2
              AND s.shift_range && tstzrange($3, $5, '[)')"#,
        team_id,
        req.user_id,
        req.shift_start,
        shift_type,
        req.shift_end,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;

    if overlaps.same_type > 0 {
        return Err(
            Problem::new(fms_shared::ProblemCode::Conflict).with_detail(format!(
                "這個人在那段時間已經有 {} 筆 `{shift_type}` 班次 —— \
             同型別重疊是重複資料。（跨型別的重疊是允許的：LEAVE 蓋在 REGULAR \
             上正是請假的記法，ON_CALL 蓋在 REGULAR 上也很常見。）",
                overlaps.same_type
            )),
        );
    }

    let id = sqlx::query_scalar!(
        r#"INSERT INTO fms.team_shifts
             (tenant_id, team_id, user_id, shift_start, shift_end, shift_type)
           VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5)
           RETURNING id"#,
        team_id,
        req.user_id,
        req.shift_start,
        req.shift_end,
        shift_type,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "data": {
                "id": id,
                "team_id": team_id,
                "user_id": req.user_id,
                "shift_start": req.shift_start,
                "shift_end": req.shift_end,
                "shift_type": shift_type,
            },
            "meta": {
                // 跨型別的重疊**不擋但回報** —— 讓排班的人看得到，
                // 而不是替他決定那是不是錯的。
                "overlaps_other_types": overlaps.other_types,
                "same_type_overlap_is_rejected": true,
            },
        })),
    ))
}

// =============================================================================
// GET /parts、GET /part-stock
// =============================================================================

#[derive(Debug, Deserialize)]
pub struct PartQuery {
    pub category_id: Option<Uuid>,
    pub include_inactive: Option<bool>,
    pub q: Option<String>,
}

/// `GET /parts`
pub async fn list_parts(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Query(q): Query<PartQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let include_inactive = q.include_inactive.unwrap_or(false);
    let search = q.q.as_deref().map(|s| format!("%{}%", s.to_lowercase()));

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "part:read", None, None).await?;

    let rows = sqlx::query!(
        r#"SELECT p.id, p.part_code::text AS "part_code!", p.name::text AS "name!",
                  p.category_id, p.unit::text AS "unit!",
                  p.unit_cost::float8 AS unit_cost, p.currency::text AS currency,
                  p.manufacturer::text AS manufacturer,
                  p.manufacturer_part_no::text AS manufacturer_part_no,
                  p.is_consumable, p.is_active,
                  coalesce((SELECT sum(s.quantity_on_hand)::float8
                              FROM fms.part_stock s WHERE s.part_id = p.id), 0)
                    AS "total_on_hand!"
             FROM fms.parts p
            WHERE ($1 OR p.is_active)
              AND ($2::uuid IS NULL OR p.category_id = $2::uuid)
              AND ($3::text IS NULL
                   OR lower(p.part_code::text) LIKE $3::text
                   OR lower(p.name::text) LIKE $3::text)
            ORDER BY p.part_code"#,
        include_inactive,
        q.category_id,
        search,
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
                "part_code": r.part_code,
                "name": r.name,
                "category_id": r.category_id,
                "unit": r.unit,
                "unit_cost": r.unit_cost,
                "currency": r.currency,
                "manufacturer": r.manufacturer,
                "manufacturer_part_no": r.manufacturer_part_no,
                "is_consumable": r.is_consumable,
                "is_active": r.is_active,
                // 跨場域的總量。要看某個場域的請用 `/part-stock`。
                "total_on_hand": r.total_on_hand,
            })
        })
        .collect();

    let no_cost = rows.iter().filter(|r| r.unit_cost.is_none()).count();
    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "count": rows.len(),
            // 沒有單價的備品領用之後算不出成本 —— `report_service_volume` 的
            // `parts_cost` 會少算，而那筆帳單會安靜地偏低。與那支報表的
            // `work_orders_without_rate` 同一條規則：讓「不知道」看得見。
            "parts_without_unit_cost": no_cost,
        },
    })))
}

#[derive(Debug, Deserialize)]
pub struct StockQuery {
    pub facility_id: Option<Uuid>,
    pub part_id: Option<Uuid>,
    /// 只回低於再訂購點的。
    pub below_reorder_point: Option<bool>,
}

/// `GET /part-stock`
///
/// **`available` 有一個要說出的前提**：`quantity_reserved` 的寫入者數量是 0
/// （量過），所以 `available` 今天恆等於 `quantity_on_hand`。那個欄位仍然回傳，
/// 但 `meta.reserved_is_never_written` 說出它是死的 —— 少了這句，看到
/// `available` 的人會以為預留機制在運作，而在領料撞到短缺時才發現不是。
pub async fn list_part_stock(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Query(q): Query<StockQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let below_only = q.below_reorder_point.unwrap_or(false);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "part:read", q.facility_id, None).await?;

    let rows = sqlx::query!(
        r#"SELECT s.id, s.part_id, p.part_code::text AS "part_code!",
                  p.name::text AS "part_name!", p.unit::text AS "unit!",
                  s.facility_id, s.storage_node_id,
                  n.name::text AS storage_node_name,
                  s.quantity_on_hand::float8 AS "on_hand!",
                  s.quantity_reserved::float8 AS "reserved!",
                  (s.quantity_on_hand - s.quantity_reserved)::float8 AS "available!",
                  s.reorder_point::float8 AS reorder_point,
                  s.reorder_quantity::float8 AS reorder_quantity,
                  (s.reorder_point IS NOT NULL
                   AND s.quantity_on_hand <= s.reorder_point) AS "needs_reorder!"
             FROM fms.part_stock s
             JOIN fms.parts p ON p.id = s.part_id
             LEFT JOIN fms.spatial_nodes n ON n.id = s.storage_node_id
            WHERE ($1::uuid IS NULL OR s.facility_id = $1::uuid)
              AND ($2::uuid IS NULL OR s.part_id = $2::uuid)
              AND (NOT $3 OR (s.reorder_point IS NOT NULL
                              AND s.quantity_on_hand <= s.reorder_point))
            ORDER BY p.part_code, s.facility_id"#,
        q.facility_id,
        q.part_id,
        below_only,
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
                "part_id": r.part_id,
                "part_code": r.part_code,
                "part_name": r.part_name,
                "unit": r.unit,
                "facility_id": r.facility_id,
                "storage_node_id": r.storage_node_id,
                "storage_node_name": r.storage_node_name,
                "quantity_on_hand": r.on_hand,
                "quantity_reserved": r.reserved,
                "available": r.available,
                "reorder_point": r.reorder_point,
                "reorder_quantity": r.reorder_quantity,
                "needs_reorder": r.needs_reorder,
            })
        })
        .collect();

    let needs = rows.iter().filter(|r| r.needs_reorder).count();
    let no_reorder_point = rows.iter().filter(|r| r.reorder_point.is_none()).count();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "count": rows.len(),
            "below_reorder_point": needs,
            // **沒有設再訂購點的列永遠不會出現在補貨清單裡。**
            // 那不是「庫存充足」，是「沒有人設過門檻」——
            // 與報表的分母規則同一條：讓「不知道」看得見。
            "rows_without_reorder_point": no_reorder_point,
            // `quantity_reserved` 沒有任何寫入者（量過），所以 `available`
            // 恆等於 `quantity_on_hand`。**不要把它當成預留機制在運作的證據。**
            "reserved_is_never_written": true,
            "available_formula": "quantity_on_hand - quantity_reserved（而 reserved 目前恆為 0）",
        },
    })))
}
