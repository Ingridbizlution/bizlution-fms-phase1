//! `GET /reports/sla-compliance`

use axum::extract::{Query, State};
use axum::Json;
use sqlx::PgPool;

use fms_shared::{begin_tenant_tx, require_permission, Caller, FieldError, Problem};

use crate::dto::*;
use crate::repo;

#[derive(Clone)]
pub struct ReportState {
    pub pool: PgPool,
}

/// 達成率。分母為 0 時回 `None` 而不是 0 或 100。
///
/// 這一行是整支端點最容易出錯的地方：`0/0` 在達成率的語境下最自然的兩個
/// 錯誤答案分別是「0%（很糟）」與「100%（完美）」，而正確答案是
/// **「這個分組沒有可判定的工單」**。回 0 會讓一個空分組看起來像災難，
/// 回 100 會讓它看起來像完美 —— 兩者都會被拿去做決定。
fn pct(met: i64, total: i64) -> Option<f64> {
    if total == 0 {
        return None;
    }
    Some(((met as f64 / total as f64) * 1000.0).round() / 10.0)
}

fn to_dto(r: repo::SlaComplianceRow) -> SlaComplianceRow {
    SlaComplianceRow {
        response_compliance_pct: pct(r.response_met, r.response_total),
        resolution_compliance_pct: pct(r.resolution_met, r.resolution_total),
        // 034 對沒有名稱的分組回 '(未指派)'，但 group_label 在 SQL 裡是
        // `coalesce(max(glabel), ...)`，型別上仍可為 NULL。
        group_label: r.group_label.unwrap_or_else(|| "(未指派)".to_string()),
        group_key: r.group_key,
        response_total: r.response_total,
        response_met: r.response_met,
        response_breached: r.response_breached,
        avg_response_minutes: r.avg_response_minutes,
        resolution_total: r.resolution_total,
        resolution_met: r.resolution_met,
        resolution_breached: r.resolution_breached,
        avg_resolution_minutes: r.avg_resolution_minutes,
        avg_waiting_minutes: r.avg_waiting_minutes,
        reopened: r.reopened,
        excluded_no_policy: r.excluded_no_policy,
        excluded_in_flight: r.excluded_in_flight,
        excluded_abandoned: r.excluded_abandoned,
        excluded_business_hours: r.excluded_business_hours,
        substituted_business_hours: r.substituted_business_hours,
        excluded_pm_response: r.excluded_pm_response,
    }
}

/// 白名單檢查。未知值回 422 而不是讓它進到 SQL ——
/// 034 的 `CASE p_group_by` 沒有 ELSE，未知值會靜默地把所有工單併成
/// 一個 `group_key = NULL` 的分組，也就是一個看起來合理的錯誤答案。
fn check_enum(field: &str, value: &str, allowed: &[&str]) -> Result<(), Problem> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(
        Problem::validation(format!("`{field}` 必須是 {} 之一", allowed.join("／"))).with_errors(
            vec![FieldError {
                pointer: format!("/{field}"),
                code: "ENUM".to_string(),
                message: format!("`{value}` 不是合法的 {field}"),
            }],
        ),
    )
}

/// `GET /reports/sla-compliance`
///
/// 權限是 `report:read`，**範圍不指定場域** —— 這支端點本來就是跨場域彙總。
/// 範圍收斂由 RLS 完成：034 的函式是 `SECURITY INVOKER`，因此場域範圍的
/// 使用者只會算到自己看得見的工單。在應用層再過濾一次會是同一條規則的
/// 第二份實作，而兩份實作最後總會分歧。
pub async fn sla_compliance(
    State(state): State<ReportState>,
    caller: Caller,
    Query(q): Query<SlaComplianceQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let group_by = q.group_by.unwrap_or_else(|| "facility".to_string());
    let strictness = q.strictness.unwrap_or_else(|| "strict".to_string());
    check_enum("group_by", &group_by, &GROUP_BY)?;
    check_enum("strictness", &strictness, &STRICTNESS)?;

    // `from > to` 會讓 034 回空集合 —— 而空集合與「這段期間真的沒有工單」
    // 長得一模一樣。一個把日期填反的請求應該得到錯誤，不是一份看起來
    // 合理的空報表。
    if q.from > q.to {
        return Err(
            Problem::validation("`from` 不得晚於 `to`").with_errors(vec![FieldError {
                pointer: "/from".to_string(),
                code: "RANGE".to_string(),
                message: format!("from={} 晚於 to={}", q.from, q.to),
            }]),
        );
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "report:read", None, None).await?;
    let rows = repo::sla_compliance(&mut tx, &group_by, q.from, q.to, &strictness).await?;
    tx.commit().await?;

    let data: Vec<SlaComplianceRow> = rows.into_iter().map(to_dto).collect();
    Ok(Json(serde_json::json!({
        "data": data,
        "meta": SlaComplianceMeta {
            group_by,
            from: q.from,
            to: q.to,
            strictness,
            minutes_basis: MINUTES_BASIS_WALLCLOCK,
        },
    })))
}

// -----------------------------------------------------------------------------
// GET /reports/pm-compliance
// -----------------------------------------------------------------------------

const PM_GROUP_BY: [&str; 3] = ["facility", "plan", "none"];

#[derive(Debug, serde::Deserialize)]
pub struct PmComplianceQuery {
    pub group_by: Option<String>,
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    /// 覆寫每個計畫的容許窗做情境分析。**不寫入任何東西。**
    pub grace_days: Option<i32>,
}

/// `GET /reports/pm-compliance`
///
/// # 三個分母是分開的，而那是這支端點最重要的性質
///
/// 主分母（`scheduled_total`）只含**已經有結果**的期次：準時、逾時、漏做。
/// 兩類被排除的各自具名回傳：
///
///   * `excluded_in_window` —— 還在容許窗內、尚無結果。它們還有機會，
///     算進分母會讓「這個月才剛開始」看起來像執行不力。
///   * `excluded_skipped` —— 被跳過的，附 `skip_reasons` 分佈。
///
/// 為什麼一定要並列而不能挑一個：把 skip 算進分母，「全部跳過」得到 0%
/// （看起來像糟糕的執行）；完全不計算，「全部跳過」得到 100%（看起來完美）。
/// **兩者都是謊**，所以兩個數字必須一起出現。ADR-12 已經替 SLA 定過同形的規則。
///
/// # 容許窗來自計畫，而 `meta` 會說出用的是哪一個
///
/// 每個計畫的 `completion_grace_days`（063，管理者定義）。`grace_days` 參數
/// 可以覆寫來做情境分析，而 `meta.grace_source` 會回報實際用的是哪一個 ——
/// 少了它，兩份不同前提算出來的報表看起來一模一樣。
pub async fn pm_compliance(
    State(state): State<ReportState>,
    caller: Caller,
    Query(q): Query<PmComplianceQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let group_by = q.group_by.unwrap_or_else(|| "facility".to_string());
    check_enum("group_by", &group_by, &PM_GROUP_BY)?;
    if let Some(g) = q.grace_days {
        if !(0..=365).contains(&g) {
            return Err(Problem::validation("grace_days 必須是 0 到 365"));
        }
    }
    // 與 sla-compliance 同一個判斷：日期填反該得到錯誤，
    // 不是一份看起來合理的空報表。
    if q.from > q.to {
        return Err(
            Problem::validation("`from` 不得晚於 `to`").with_errors(vec![FieldError {
                pointer: "/from".to_string(),
                code: "RANGE".to_string(),
                message: format!("from={} 晚於 to={}", q.from, q.to),
            }]),
        );
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 範圍不指定場域：這支端點本來就是跨場域彙總，收斂由 RLS 完成
    // （063 的函式是 SECURITY INVOKER，而 062 讓 occurrences 跟著計畫收斂）。
    require_permission(&mut tx, "report:read", None, None).await?;
    let rows = repo::pm_compliance(&mut tx, &group_by, q.from, q.to, q.grace_days).await?;
    tx.commit().await?;

    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            // 比率在這裡算而不是在 SQL 裡：分母為 0 時要回 null 而不是 0.0，
            // 而「沒有任何期次」與「全部沒做」是完全不同的兩件事。
            let rate = if r.scheduled_total > 0 {
                Some(r.completed_on_time as f64 / r.scheduled_total as f64)
            } else {
                None
            };
            serde_json::json!({
                "group_key": r.group_key,
                "group_label": r.group_label,
                "scheduled_total": r.scheduled_total,
                "completed_on_time": r.completed_on_time,
                "completed_late": r.completed_late,
                "missed": r.missed,
                "excluded_in_window": r.excluded_in_window,
                "excluded_skipped": r.excluded_skipped,
                "skip_reasons": r.skip_reasons,
                "avg_days_late": r.avg_days_late,
                // null = 這段期間沒有任何已結果的期次，**不是 0%**。
                "on_time_rate": rate,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "group_by": group_by,
            "from": q.from,
            "to": q.to,
            // 說出容許窗的來源。兩份不同前提的報表必須看得出差別。
            "grace_source": if q.grace_days.is_some() {
                "query_override"
            } else {
                "maintenance_plans.completion_grace_days"
            },
            "grace_days_override": q.grace_days,
            // MISSED 是算出來的，不是資料庫裡的狀態 —— 見 063 檔頭。
            "missed_is_derived": true,
        },
    })))
}

// =============================================================================
// 065 的四支報表端點
// =============================================================================
//
// 四支都是薄 handler：驗白名單、驗日期順序、呼叫 repo、把 `meta` 填上
// 「這個數字是怎麼算的」。分母的定義全部在 SQL 函式裡 ——
// 應用層再算一次會變成第二份實作。
//
// 共同的 `meta` 規則：**任何有前提的數字都要說出前提**。
// SLA 報表用 `strictness`、PM 用 `grace_source`，這四支各有自己的
// （`subtree`、`history_since`、`hours_basis`、`without_rate`）。

/// 共用的日期區間檢查。
///
/// `from > to` 會讓所有這些函式回空集合 —— 而空集合與「這段期間真的沒有資料」
/// 長得一模一樣。一個把日期填反的請求該得到錯誤，不是一份看起來合理的空報表。
fn check_range(from: chrono::NaiveDate, to: chrono::NaiveDate) -> Result<(), Problem> {
    if from > to {
        return Err(
            Problem::validation("`from` 不得晚於 `to`").with_errors(vec![FieldError {
                pointer: "/from".to_string(),
                code: "RANGE".to_string(),
                message: format!("from={from} 晚於 to={to}"),
            }]),
        );
    }
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
pub struct GroupRollupQuery {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    /// 只算這個組織的子樹（含自己）。省略時回整棵樹。
    pub subtree_of: Option<uuid::Uuid>,
}

/// `GET /reports/group-rollup`
///
/// 每一列是**子樹**的總和（ltree `<@`），不是直屬設施 ——
/// 逐層爬會讓中間層漏掉孫節點的設施，於是集團那一列比底下各分公司的總和還小。
///
/// 因此各列**會重疊**：父組織的數字包含子組織的。`meta.rows_are_cumulative`
/// 把那件事說出來 —— 前端把各列相加會得到重複計算的總數。
pub async fn group_rollup(
    State(state): State<ReportState>,
    caller: Caller,
    Query(q): Query<GroupRollupQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    check_range(q.from, q.to)?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "report:read", None, None).await?;
    let rows = repo::group_rollup(&mut tx, q.from, q.to, q.subtree_of).await?;
    tx.commit().await?;

    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "org_id": r.org_id, "org_name": r.org_name,
                "org_path": r.org_path, "depth": r.depth,
                "facility_count": r.facility_count,
                "work_orders_total": r.work_orders_total,
                "work_orders_open": r.work_orders_open,
                "work_orders_overdue": r.work_orders_overdue,
                "pm_scheduled": r.pm_scheduled,
                "pm_on_time": r.pm_on_time,
                // 分母為 0 時 null 而不是 0 —— 與 pm-compliance 同一條規則。
                "pm_on_time_rate": if r.pm_scheduled > 0 {
                    Some(r.pm_on_time as f64 / r.pm_scheduled as f64)
                } else { None },
                "total_cost": r.total_cost,
                "chargeback_cost": r.chargeback_cost,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "from": q.from, "to": q.to,
            "subtree_of": q.subtree_of,
            // **各列會重疊**：父組織含子組織。相加會重複計算。
            "rows_are_cumulative": true,
            "subtree_basis": "organizations.org_path (ltree)",
        },
    })))
}

#[derive(Debug, serde::Deserialize)]
pub struct AssetReliabilityQuery {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    pub facility_id: Option<uuid::Uuid>,
    pub limit: Option<i32>,
}

/// `GET /reports/asset-reliability`
///
/// # `mtbf_hours` 為 null 有兩種原因，而它們不同
///
/// 區間內只有一次故障（算不出間隔），或完全沒有故障。前者是「還不知道」，
/// 後者是「很可靠」。`failure_count` 把兩者分開 —— 只看 MTBF 為 null
/// 會把它們混成同一件事。
///
/// # `history_since` 必須看
///
/// `asset_status_history` 的第一列是 migration 064 之後才出現的。
/// 那個時刻**晚於** `from` 時，這個 MTBF 涵蓋的區間比你要求的短，
/// 而 `meta.history_covers_full_range` 直接回答那件事。
pub async fn asset_reliability(
    State(state): State<ReportState>,
    caller: Caller,
    Query(q): Query<AssetReliabilityQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    check_range(q.from, q.to)?;
    let limit = q.limit.unwrap_or(50).clamp(1, 500);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "report:read", None, None).await?;
    let rows = repo::asset_reliability(&mut tx, q.from, q.to, q.facility_id, limit).await?;
    tx.commit().await?;

    // 最早的歷程時刻。晚於 `from` 就代表整份報表涵蓋不到要求的區間。
    let earliest = rows.iter().filter_map(|r| r.history_since).min();
    let covers_full = earliest.is_some_and(|t| t.date_naive() <= q.from);

    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "asset_id": r.asset_id, "asset_code": r.asset_code,
                "asset_name": r.asset_name, "facility_id": r.facility_id,
                "criticality": r.criticality,
                "failure_count": r.failure_count,
                "corrective_orders": r.corrective_orders,
                "mttr_hours": r.mttr_hours,
                // null + failure_count == 1 → 還不知道；
                // null + failure_count == 0 → 沒故障過。見 handler 檔頭。
                "mtbf_hours": r.mtbf_hours,
                "downtime_hours": r.downtime_hours,
                "repair_cost": r.repair_cost,
                "history_since": r.history_since,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "from": q.from, "to": q.to, "limit": limit,
            "mtbf_source": "asset_status_history (migration 064)",
            "mttr_source": "work_orders (CORRECTIVE)",
            "earliest_history_at": earliest,
            // false = 這份報表的 MTBF 涵蓋不到你要求的整個區間。
            "history_covers_full_range": covers_full,
        },
    })))
}

#[derive(Debug, serde::Deserialize)]
pub struct SpaceUtilizationQuery {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    pub facility_id: Option<uuid::Uuid>,
}

/// `GET /reports/space-utilization`
///
/// 兩個分母都不是「所有預約」：
///
///   * 使用率的分母是**可預約時數**，來源在 `hours_basis` 裡。
///     `assumed_24h` 代表那個資源與它的場域都沒設營運時間 ——
///     那一列的百分比不可與有設定的比較。
///   * no-show 的分母**只含需要報到的預約**，`checkin_required` 是那個數字，
///     而且讀的是 `reservations.requires_check_in`（預約自己那一份）——
///     與產生 `NO_SHOW` 的那支背景作業同一欄。
pub async fn space_utilization(
    State(state): State<ReportState>,
    caller: Caller,
    Query(q): Query<SpaceUtilizationQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    check_range(q.from, q.to)?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "report:read", None, None).await?;
    let rows = repo::space_utilization(&mut tx, q.from, q.to, q.facility_id).await?;
    tx.commit().await?;

    let assumed = rows
        .iter()
        .filter(|r| r.hours_basis == "assumed_24h")
        .count();
    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "resource_id": r.resource_id, "resource_name": r.resource_name,
                "resource_type": r.resource_type, "facility_id": r.facility_id,
                "capacity": r.capacity,
                "reservations_total": r.reservations_total,
                "booked_hours": r.booked_hours,
                "available_hours": r.available_hours,
                "utilization_rate": r.utilization_rate,
                "hours_basis": r.hours_basis,
                "checkin_required": r.checkin_required,
                "no_shows": r.no_shows,
                "no_show_rate": r.no_show_rate,
                "cancelled": r.cancelled,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "from": q.from, "to": q.to,
            // 有幾列的分母是猜的。> 0 時整份報表的可比性受限。
            "resources_with_assumed_hours": assumed,
            "no_show_denominator": "reservations.requires_check_in (same column no_show.rs sweeps)",
        },
    })))
}

const SERVICE_VOLUME_GROUP_BY: [&str; 3] = ["service_item", "facility", "org"];

#[derive(Debug, serde::Deserialize)]
pub struct ServiceVolumeQuery {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    pub group_by: Option<String>,
}

/// `GET /reports/service-volume`
///
/// 三種成本分開回傳，因為它們的爭議點不同（工時費率、料件單價、外包發票）
/// —— 一個總和無法對帳。
///
/// **`work_orders_without_rate` 是這支端點最重要的數字。** 它是「有工時但工時
/// 成本為 0」的張數，也就是**費率未知**的工單。那不是免費 —— 把它當 0 加總
/// 會讓 chargeback 帳單安靜地偏低，而收到帳單的人不會知道少算了什麼。
pub async fn service_volume(
    State(state): State<ReportState>,
    caller: Caller,
    Query(q): Query<ServiceVolumeQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    check_range(q.from, q.to)?;
    let group_by = q.group_by.unwrap_or_else(|| "service_item".to_string());
    check_enum("group_by", &group_by, &SERVICE_VOLUME_GROUP_BY)?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "report:read", None, None).await?;
    let rows = repo::service_volume(&mut tx, q.from, q.to, &group_by).await?;
    tx.commit().await?;

    let without_rate: i64 = rows.iter().map(|r| r.work_orders_without_rate).sum();
    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "group_key": r.group_key, "group_label": r.group_label,
                "requests": r.requests, "completed": r.completed,
                "labor_minutes": r.labor_minutes,
                "labor_cost": r.labor_cost,
                "parts_cost": r.parts_cost,
                "other_cost": r.other_cost,
                "total_cost": r.labor_cost + r.parts_cost + r.other_cost,
                "chargeback_requests": r.chargeback_requests,
                "chargeback_cost": r.chargeback_cost,
                "work_orders_without_rate": r.work_orders_without_rate,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "from": q.from, "to": q.to, "group_by": group_by,
            // 整份報表有幾張工單的工時成本未知。> 0 時金額是**下限**，不是實際值。
            "work_orders_without_rate": without_rate,
            "cost_is_lower_bound": without_rate > 0,
        },
    })))
}
