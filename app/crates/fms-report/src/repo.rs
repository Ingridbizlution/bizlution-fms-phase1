//! 呼叫 034 的 `fms.report_sla_compliance`。

use fms_shared::TenantTx;

/// 一列彙總結果，欄位順序與 034 的 `RETURNS TABLE` 一致。
///
/// `numeric` 欄位在這裡取成 `f64`：它們是平均分鐘數，用來看的，
/// 不參與後續計算，因此浮點的精度損失沒有影響。
/// （金額不會這樣做 —— 那些欄位在別處是 `rust_decimal`。）
#[derive(Debug, sqlx::FromRow)]
pub struct SlaComplianceRow {
    pub group_key: Option<String>,
    pub group_label: Option<String>,
    pub response_total: i64,
    pub response_met: i64,
    pub response_breached: i64,
    pub avg_response_minutes: Option<f64>,
    pub resolution_total: i64,
    pub resolution_met: i64,
    pub resolution_breached: i64,
    pub avg_resolution_minutes: Option<f64>,
    pub avg_waiting_minutes: Option<f64>,
    pub reopened: i64,
    pub excluded_no_policy: i64,
    pub excluded_in_flight: i64,
    pub excluded_abandoned: i64,
    pub excluded_business_hours: i64,
    pub substituted_business_hours: i64,
    pub excluded_pm_response: i64,
}

/// `group_by` 與 `strictness` 已由 handler 依契約的 enum 驗過白名單。
///
/// 仍然用綁定參數而不是字串拼接：白名單是**當下**的正確性，
/// 而綁定是結構上的 —— 日後有人加了一個 group_by 卻忘了更新白名單時,
/// 這裡不會變成注入點。
pub async fn sla_compliance(
    tx: &mut TenantTx,
    group_by: &str,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    strictness: &str,
) -> Result<Vec<SlaComplianceRow>, sqlx::Error> {
    sqlx::query_as::<_, SlaComplianceRow>("SELECT * FROM fms.report_sla_compliance($1, $2, $3, $4)")
        .bind(group_by)
        .bind(from)
        .bind(to)
        .bind(strictness)
        .fetch_all(tx.conn())
        .await
}

/// 一列 PM 合規結果，欄位順序與 063 的 `RETURNS TABLE` 一致。
#[derive(Debug, sqlx::FromRow)]
pub struct PmComplianceRow {
    pub group_key: Option<String>,
    pub group_label: Option<String>,
    pub scheduled_total: i64,
    pub completed_on_time: i64,
    pub completed_late: i64,
    pub missed: i64,
    pub excluded_in_window: i64,
    pub excluded_skipped: i64,
    pub skip_reasons: serde_json::Value,
    pub avg_days_late: Option<f64>,
}

/// `group_by` 已由 handler 依白名單驗過；仍然用綁定參數，理由同 `sla_compliance`。
///
/// `grace_override` 為 `None` 時每個計畫用自己的 `completion_grace_days`
/// （063 新增，管理者定義）。給值是做情境分析用的 ——
/// 「若一律容許 3 天，合規率會是多少」。
pub async fn pm_compliance(
    tx: &mut TenantTx,
    group_by: &str,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    grace_override: Option<i32>,
) -> Result<Vec<PmComplianceRow>, sqlx::Error> {
    sqlx::query_as::<_, PmComplianceRow>("SELECT * FROM fms.report_pm_compliance($1, $2, $3, $4)")
        .bind(group_by)
        .bind(from)
        .bind(to)
        .bind(grace_override)
        .fetch_all(tx.conn())
        .await
}

// =============================================================================
// 065 的四支報表
// =============================================================================
//
// 每一支都是薄包裝：`SELECT * FROM fms.report_*($1, …)`。分母的定義、排除項、
// 「哪個數字為 0 時該回 NULL」全部在 SQL 函式裡 —— 應用層再算一次會變成
// 同一套語意的第二份實作，而那正是 061 抽述詞時處理過的問題。

#[derive(Debug, sqlx::FromRow)]
pub struct GroupRollupRow {
    pub org_id: uuid::Uuid,
    pub org_name: String,
    pub org_path: String,
    pub depth: i32,
    pub facility_count: i64,
    pub work_orders_total: i64,
    pub work_orders_open: i64,
    pub work_orders_overdue: i64,
    pub pm_scheduled: i64,
    pub pm_on_time: i64,
    pub total_cost: f64,
    pub chargeback_cost: f64,
}

pub async fn group_rollup(
    tx: &mut TenantTx,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    subtree_of: Option<uuid::Uuid>,
) -> Result<Vec<GroupRollupRow>, sqlx::Error> {
    sqlx::query_as::<_, GroupRollupRow>("SELECT * FROM fms.report_group_rollup($1, $2, $3)")
        .bind(from)
        .bind(to)
        .bind(subtree_of)
        .fetch_all(tx.conn())
        .await
}

#[derive(Debug, sqlx::FromRow)]
pub struct AssetReliabilityRow {
    pub asset_id: uuid::Uuid,
    pub asset_code: String,
    pub asset_name: String,
    pub facility_id: uuid::Uuid,
    pub criticality: String,
    pub failure_count: i64,
    pub corrective_orders: i64,
    /// 修復花多久（來自工單）。
    pub mttr_hours: Option<f64>,
    /// 兩次故障之間撐多久。**一次故障是 None 而不是 0** ——
    /// 0 會看起來像設備一直在壞。
    pub mtbf_hours: Option<f64>,
    pub downtime_hours: f64,
    pub repair_cost: f64,
    /// 這台設備的歷程從什麼時候開始有。早於查詢起點時 MTBF 才涵蓋完整區間。
    pub history_since: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn asset_reliability(
    tx: &mut TenantTx,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    facility_id: Option<uuid::Uuid>,
    limit: i32,
) -> Result<Vec<AssetReliabilityRow>, sqlx::Error> {
    sqlx::query_as::<_, AssetReliabilityRow>(
        "SELECT * FROM fms.report_asset_reliability($1, $2, $3, $4)",
    )
    .bind(from)
    .bind(to)
    .bind(facility_id)
    .bind(limit)
    .fetch_all(tx.conn())
    .await
}

#[derive(Debug, sqlx::FromRow)]
pub struct SpaceUtilizationRow {
    pub resource_id: uuid::Uuid,
    pub resource_name: String,
    pub resource_type: String,
    pub facility_id: uuid::Uuid,
    pub capacity: Option<i32>,
    pub reservations_total: i64,
    pub booked_hours: f64,
    pub available_hours: f64,
    pub utilization_rate: Option<f64>,
    /// 時數基準：`resource.opening_hours`／`facility.operating_hours`／
    /// `assumed_24h`。**必須回傳** —— 兩個不同基準算出來的百分比不可比。
    pub hours_basis: String,
    /// no-show 的分母：只含 `requires_check_in` 的預約。
    pub checkin_required: i64,
    pub no_shows: i64,
    /// 分母為 0 時是 None ——「沒有任何需要報到的預約」與「都報到了」不同。
    pub no_show_rate: Option<f64>,
    pub cancelled: i64,
}

pub async fn space_utilization(
    tx: &mut TenantTx,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    facility_id: Option<uuid::Uuid>,
) -> Result<Vec<SpaceUtilizationRow>, sqlx::Error> {
    sqlx::query_as::<_, SpaceUtilizationRow>(
        "SELECT * FROM fms.report_space_utilization($1, $2, $3)",
    )
    .bind(from)
    .bind(to)
    .bind(facility_id)
    .fetch_all(tx.conn())
    .await
}

#[derive(Debug, sqlx::FromRow)]
pub struct ServiceVolumeRow {
    pub group_key: Option<String>,
    pub group_label: Option<String>,
    pub requests: i64,
    pub completed: i64,
    pub labor_minutes: i64,
    pub labor_cost: f64,
    pub parts_cost: f64,
    pub other_cost: f64,
    pub chargeback_requests: i64,
    pub chargeback_cost: f64,
    /// 有工時但工時成本為 0 的張數 —— **費率未知，不是免費**。
    pub work_orders_without_rate: i64,
}

pub async fn service_volume(
    tx: &mut TenantTx,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    group_by: &str,
) -> Result<Vec<ServiceVolumeRow>, sqlx::Error> {
    sqlx::query_as::<_, ServiceVolumeRow>("SELECT * FROM fms.report_service_volume($1, $2, $3)")
        .bind(from)
        .bind(to)
        .bind(group_by)
        .fetch_all(tx.conn())
        .await
}
