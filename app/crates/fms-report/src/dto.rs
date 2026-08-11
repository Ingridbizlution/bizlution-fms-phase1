//! `GET /reports/sla-compliance` 的請求與回應形狀。

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct SlaComplianceQuery {
    pub group_by: Option<String>,
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    pub strictness: Option<String>,
}

/// 契約 enum 的白名單。
///
/// 用切片而不是 Rust enum：這五個值直接是 034 的 `CASE p_group_by` 分支，
/// 而那個 CASE **沒有 ELSE** —— 未知值不會報錯，只會讓 `group_key` 整欄
/// 變成 NULL，也就是「一個叫做『全部』的分組」。那種靜默的錯誤答案比 400
/// 糟得多，因此在進入 SQL 之前就擋掉。
pub const GROUP_BY: [&str; 5] = ["facility", "org", "team", "service_item", "priority"];
pub const STRICTNESS: [&str; 2] = ["strict", "operational"];

#[derive(Debug, Serialize)]
pub struct SlaComplianceMeta {
    pub group_by: String,
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    pub strictness: String,
    /// 下面每一列的兩個平均值是**用什麼時間單位**算的。
    ///
    /// `WALLCLOCK` = 牆鐘時間，包含夜間與週末。
    ///
    /// 這個欄位存在是因為那兩個數字很容易被誤讀：038 之後 MEDIUM 的期限是
    /// 用營業時間算的，於是一張週五晚上開、週一上午修好的工單，
    /// **達成率是達成，而平均解決時間是 2296 分鐘** —— 看起來像系統很慢，
    /// 實際上那包含了兩個晚上和一個週末。
    ///
    /// 達成率（`*_compliance_pct`）不受影響：它比的是絕對時刻，而期限本身
    /// 已經是營業時間意義下算好的（ADR-12 決定 C）。
    ///
    /// 放在 `meta` 而不是每一列：它描述的是**整份回應的計算方式**，
    /// 不是某個分組的性質。日後若真的算了營業分鐘，這個欄位會需要下沉到
    /// 每一列（因為一個分組可能同時有 24/7 與營業時間的政策）——
    /// 那時候會有人認真想過那件事，而不是繼承一個沉默的預設。
    pub minutes_basis: &'static str,
}

/// 目前唯一的值。`fms.report_sla_compliance` 的兩個 `avg(...)` 都是對
/// `completed_at - created_at` 這種絕對時刻差做的。
///
/// **改動那兩個 avg 的單位時必須一起改這裡** ——
/// `the_averages_are_wallclock_and_labelled_as_such` 會擋住忘記的情況。
pub const MINUTES_BASIS_WALLCLOCK: &str = "WALLCLOCK";

#[derive(Debug, Serialize)]
pub struct SlaComplianceRow {
    /// 未指派時為 `null`（例如沒有團隊的工單）。
    pub group_key: Option<String>,
    pub group_label: String,

    pub response_total: i64,
    pub response_met: i64,
    pub response_breached: i64,
    pub response_compliance_pct: Option<f64>,
    pub avg_response_minutes: Option<f64>,

    pub resolution_total: i64,
    pub resolution_met: i64,
    pub resolution_breached: i64,
    pub resolution_compliance_pct: Option<f64>,
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
