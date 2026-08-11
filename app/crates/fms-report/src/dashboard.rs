//! 設施儀表板（`/reports/facility-dashboard`）。
//!
//! # SLA 的數字**不是這裡算的**
//!
//! `sla.compliance_pct` 與 `work_orders.avg_resolution_minutes` 來自
//! `repo::sla_compliance()` —— 也就是 `/reports/sla-compliance` 呼叫的
//! **同一支** 034 函式。
//!
//! ADR-12 定義了量測規則（回應與解決分開的分母、營業時間的排除與替代、
//! PM 工單不計入回應）。自己在這裡再算一遍必然與報表漂移，而那時
//! **沒有人知道哪一個是對的** —— 而儀表板與報表對不上是最難查的那種問題，
//! 因為兩邊看起來都「有在算」。
//!
//! 這是這個 codebase 反覆出現的教訓：一條判定散成兩份手抄本就會漂移
//! （見 `sql/053` 的檔頭）。
//!
//! # 兩個百分比的定義寫在契約裡
//!
//! `space.utilization_pct` 與 `maintenance.pm_compliance_pct` 沒有唯一正確的
//! 定義。**一個沒有標明定義的百分比比不給更糟** —— 看的人會用自己的假設
//! 解讀它。完整定義在 `openapi.yaml` 的 `getFacilityDashboard` 說明裡；
//! 這裡的註解只記關鍵取捨。
//!
//! # `null` 與 `0` 是不同的答案
//!
//! `null` = 沒有資料可以算（期間內沒有納入 SLA 的工單）。
//! `0` = 算出來是 0。混用會讓前端把「沒資料」畫成一條貼底的線，
//! 而那看起來像系統壞了。

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, Problem};

use crate::handlers::ReportState;
use crate::repo;

#[derive(Debug, Deserialize)]
pub struct DashboardQuery {
    pub facility_id: Option<Uuid>,
    pub period: Option<String>,
}

const PERIODS: [&str; 5] = ["today", "7d", "30d", "mtd", "qtd"];

/// SLA 的口徑。**與 `/reports/sla-compliance` 的預設相同**，否則同一個場域
/// 在兩個畫面上會有兩個達成率。
///
/// 這個常數同時餵給函式呼叫與回應的 `meta.sla_source`，因此**兩者不可能
/// 不一致** —— 而 `facility_dashboard_slice.rs` 的 `b_` 釘住 meta 裡的值。
///
/// 那一格原本只比對「兩支端點的數字相同」，而突變測試證明那不夠：
/// 把口徑改成 `loose` 之後 5 格全過，因為示範資料裡 strict 與 loose 算出來
/// 一樣（沒有跨營業時間的工單）。**數字相同不代表口徑相同。**
const SLA_STRICTNESS: &str = "strict";

/// 期間換算成日期範圍。**在 SQL 裡用 `current_date`** 而不是應用伺服器的
/// 時鐘：兩者時區不同時，跨日那幾個小時會給出不同的期間。
fn period_sql(period: &str) -> &'static str {
    match period {
        "today" => "current_date",
        "7d" => "current_date - interval '6 days'",
        "30d" => "current_date - interval '29 days'",
        "mtd" => "date_trunc('month', current_date)",
        "qtd" => "date_trunc('quarter', current_date)",
        _ => "current_date - interval '29 days'",
    }
}

/// `GET /reports/facility-dashboard`
pub async fn facility_dashboard(
    State(state): State<ReportState>,
    caller: Caller,
    Query(q): Query<DashboardQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let facility_id = q
        .facility_id
        .ok_or_else(|| Problem::validation("facility_id 為必填"))?;
    let period = q.period.unwrap_or_else(|| "30d".to_string());
    if !PERIODS.contains(&period.as_str()) {
        return Err(Problem::validation(format!(
            "period 必須是 {} 其中之一",
            PERIODS.join("／")
        )));
    }
    let from_expr = period_sql(&period);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 範圍就是這個場域 —— 儀表板是場域級的畫面，場域管理員要看得到自己的。
    require_permission(&mut tx, "report:read", Some(facility_id), None).await?;

    // 場域必須存在且可見。少了這一步，一個看不到的（或不存在的）場域會回
    // 一整組 0，而那看起來像「這個場域很閒」而不是「你問錯了」。
    let facility: Option<(Uuid, String)> =
        sqlx::query_as("SELECT id, name::text FROM fms.facilities WHERE id = $1")
            .bind(facility_id)
            .fetch_optional(tx.conn())
            .await?;
    let (fid, fname) =
        facility.ok_or_else(|| Problem::not_found("找不到這個場域（或它不在你的範圍內）"))?;

    // --- 工單 ---------------------------------------------------------------
    // `open` 的定義：還沒 COMPLETED／CLOSED／CANCELLED。
    // `overdue`：解決期限已過而還沒完工 —— 用 `resolution_due_at` 而不是
    // `sla_state`，因為後者由 worker 掃描更新，可能落後幾秒；儀表板要的是
    // 「現在」。兩者的差異是刻意的，`sla.breached` 才用 `sla_state`。
    let wo: (i64, i64, i64, serde_json::Value, serde_json::Value) = sqlx::query_as(&format!(
        "WITH scoped AS (
           SELECT * FROM fms.work_orders
            WHERE facility_id = $1 AND deleted_at IS NULL)
         SELECT
           (SELECT count(*) FROM scoped
             WHERE status NOT IN ('COMPLETED','CLOSED','CANCELLED')),
           (SELECT count(*) FROM scoped
             WHERE status NOT IN ('COMPLETED','CLOSED','CANCELLED')
               AND resolution_due_at IS NOT NULL
               AND resolution_due_at < clock_timestamp()),
           (SELECT count(*) FROM scoped
             WHERE completed_at >= {from_expr} AND completed_at IS NOT NULL),
           (SELECT coalesce(jsonb_object_agg(status, n), '{{}}'::jsonb)
              FROM (SELECT status, count(*) AS n FROM scoped GROUP BY status) s),
           (SELECT coalesce(jsonb_object_agg(source, n), '{{}}'::jsonb)
              FROM (SELECT source, count(*) AS n FROM scoped GROUP BY source) s)"
    ))
    .bind(fid)
    .fetch_one(tx.conn())
    .await?;

    // --- SLA：**複用報表的函式** -------------------------------------------
    // `strictness = 'strict'` 與報表的預設一致，否則同一個場域在兩個畫面上
    // 會有兩個達成率。
    let from_date: chrono::NaiveDate = sqlx::query_scalar(&format!("SELECT ({from_expr})::date"))
        .fetch_one(tx.conn())
        .await?;
    let today: chrono::NaiveDate = sqlx::query_scalar("SELECT current_date")
        .fetch_one(tx.conn())
        .await?;
    let sla_rows =
        repo::sla_compliance(&mut tx, "facility", from_date, today, SLA_STRICTNESS).await?;
    let row = sla_rows
        .iter()
        .find(|r| r.group_key.as_deref() == Some(&fid.to_string()));

    // 分母是 0 時回 null 而不是 0 —— 「期間內沒有納入 SLA 的工單」
    // 與「達成率 0%」是完全不同的事實。
    let compliance_pct = row.and_then(|r| {
        (r.resolution_total > 0)
            .then(|| (r.resolution_met as f64) * 100.0 / (r.resolution_total as f64))
    });
    let avg_resolution_minutes = row.and_then(|r| r.avg_resolution_minutes);

    // `at_risk` 與 `breached` 用 `sla_state`（worker 維護的狀態），
    // 而不是即時比對期限 —— 那兩個數字對應的是「掃描器認定的狀態」，
    // 與 `/reports/sla-compliance` 的口徑一致。
    let (at_risk, breached): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE sla_state = 'AT_RISK'),
                count(*) FILTER (WHERE sla_state IN ('RESPONSE_BREACHED','RESOLUTION_BREACHED'))
           FROM fms.work_orders
          WHERE facility_id = $1 AND deleted_at IS NULL
            AND status NOT IN ('COMPLETED','CLOSED','CANCELLED')",
    )
    .bind(fid)
    .fetch_one(tx.conn())
    .await?;

    // --- 資產 ---------------------------------------------------------------
    let assets: (i64, i64, i64, i64, Option<f64>) = sqlx::query_as(
        "SELECT count(*),
                count(*) FILTER (WHERE status = 'DOWN'),
                count(*) FILTER (WHERE status = 'DEGRADED'),
                count(*) FILTER (WHERE warranty_end_date IS NOT NULL
                                   AND warranty_end_date
                                       BETWEEN current_date AND current_date + 90),
                avg(health_score)::float8
           FROM fms.assets
          WHERE facility_id = $1 AND deleted_at IS NULL
            AND status <> 'DECOMMISSIONED'",
    )
    .bind(fid)
    .fetch_one(tx.conn())
    .await?;

    // --- 維護 ---------------------------------------------------------------
    // `maintenance_occurrences` **沒有 facility_id** —— 場域要從
    // `maintenance_plans.facility_id` 取。走 plan 而不是 asset：
    // 場域級的計畫（沒有 asset_id）若只 join assets 就會整批漏掉。
    //
    // `pm_compliance_pct` 的分母刻意**不含 SKIPPED**：那是有人刻意跳過
    // 並留下 skip_reason 的，算成未達成會懲罰正確的決定。
    let maint: (i64, Option<f64>, i64) = sqlx::query_as(&format!(
        "WITH occ AS (
           SELECT o.* FROM fms.maintenance_occurrences o
             JOIN fms.maintenance_plans p ON p.id = o.plan_id
            WHERE p.facility_id = $1)
         SELECT
           (SELECT count(*) FROM occ
             WHERE status IN ('PLANNED','GENERATED')
               AND scheduled_for BETWEEN current_date AND current_date + 30),
           (SELECT CASE WHEN count(*) FILTER (WHERE status IN ('COMPLETED','MISSED')) = 0
                        THEN NULL
                        ELSE count(*) FILTER (WHERE status = 'COMPLETED') * 100.0
                             / count(*) FILTER (WHERE status IN ('COMPLETED','MISSED'))
                   END::float8
              FROM occ WHERE scheduled_for >= {from_expr}),
           (SELECT count(*) FROM occ
             WHERE status IN ('PLANNED','GENERATED') AND scheduled_for < current_date)"
    ))
    .bind(fid)
    .fetch_one(tx.conn())
    .await?;

    // --- 告警 ---------------------------------------------------------------
    // `unlinked_to_work_order` 與 `GET /alarms?unlinked_only=true` 是同一個
    // 述詞 —— 那是「IoT 與工單的串接缺口」，儀表板上該有一個數字。
    let alarms: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'ACTIVE'),
                count(*) FILTER (WHERE status = 'ACTIVE' AND severity = 'CRITICAL'),
                count(*) FILTER (WHERE status = 'ACTIVE' AND work_order_id IS NULL)
           FROM fms.alarms WHERE facility_id = $1",
    )
    .bind(fid)
    .fetch_one(tx.conn())
    .await?;

    // --- 空間 ---------------------------------------------------------------
    // 定義見契約。分母用期間總分鐘數而不是營業時間 —— 後者會讓同一個數字
    // 在不同場域之間不可比。
    let space: (i64, Option<f64>, Option<f64>) = sqlx::query_as(&format!(
        "WITH nodes AS (
           SELECT count(*) AS n FROM fms.spatial_nodes
            WHERE facility_id = $1 AND is_bookable),
         rsv AS (
           SELECT * FROM fms.reservations
            WHERE facility_id = $1 AND start_at >= {from_expr}),
         win AS (
           SELECT extract(epoch FROM (clock_timestamp() - ({from_expr})::timestamptz)) / 60
                  AS minutes)
         SELECT
           (SELECT n FROM nodes),
           (SELECT CASE WHEN (SELECT n FROM nodes) = 0
                          OR (SELECT minutes FROM win) <= 0 THEN NULL
                        ELSE least(100.0, coalesce(sum(
                               extract(epoch FROM (r.end_at - r.start_at)) / 60), 0)
                             * 100.0
                             / ((SELECT n FROM nodes) * (SELECT minutes FROM win)))
                   END::float8
              FROM rsv r
             WHERE r.status IN ('CONFIRMED','CHECKED_IN','COMPLETED')),
           (SELECT CASE WHEN count(*) = 0 THEN NULL
                        ELSE count(*) FILTER (WHERE status = 'NO_SHOW') * 100.0 / count(*)
                   END::float8
              FROM rsv
             WHERE status IN ('CONFIRMED','CHECKED_IN','COMPLETED','NO_SHOW'))"
    ))
    .bind(fid)
    .fetch_one(tx.conn())
    .await?;

    // --- 裝置 ---------------------------------------------------------------
    // `offline` 用 `status`（由掃描維護），`stale_over_24h` 用 `last_seen_at`
    // —— 兩者刻意不同：一個裝置可能狀態還是 ONLINE 但已經 30 小時沒回報，
    // 而那正是「掃描器沒跟上」的訊號，該看得見。
    let devices: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*),
                count(*) FILTER (WHERE status = 'OFFLINE'),
                count(*) FILTER (WHERE last_seen_at IS NULL
                                   OR last_seen_at < clock_timestamp() - interval '24 hours')
           FROM fms.devices WHERE facility_id = $1 AND deleted_at IS NULL",
    )
    .bind(fid)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "facility": { "id": fid, "name": fname },
        "work_orders": {
            "open": wo.0,
            "overdue": wo.1,
            "completed_in_period": wo.2,
            "by_status": wo.3,
            "by_source": wo.4,
            "avg_resolution_minutes": avg_resolution_minutes,
        },
        "sla": {
            "compliance_pct": compliance_pct,
            "at_risk": at_risk,
            "breached": breached,
        },
        "assets": {
            "total": assets.0,
            "down": assets.1,
            "degraded": assets.2,
            "warranty_expiring_90d": assets.3,
            "avg_health_score": assets.4,
        },
        "maintenance": {
            "pm_due_30d": maint.0,
            "pm_compliance_pct": maint.1,
            "overdue_occurrences": maint.2,
        },
        "alarms": {
            "active": alarms.0,
            "critical": alarms.1,
            "unlinked_to_work_order": alarms.2,
        },
        "space": {
            "bookable_resources": space.0,
            "utilization_pct": space.1,
            "no_show_pct": space.2,
        },
        "devices": {
            "total": devices.0,
            "offline": devices.1,
            "stale_over_24h": devices.2,
        },
        "meta": {
            "period": period,
            // 指回契約，因為兩個百分比的定義不是自明的。
            "definitions": "見 openapi.yaml 的 getFacilityDashboard 說明",
            // SLA 的口徑：說出來，否則沒有人知道這個數字與報表是不是同一個。
            // 由 SLA_STRICTNESS 產生，不是手寫的字串 —— 改了常數就會改這裡，
            // 而測試釘住這裡。
            "sla_source": format!(
                "fms.report_sla_compliance(group_by=facility, strictness={SLA_STRICTNESS})"
            ),
        },
    })))
}
