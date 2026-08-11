//! 形狀對齊 `openapi.yaml` 的 `MaintenancePlan` / `MaintenancePlanCreate`。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `MaintenancePlan.target`
#[derive(Debug, Serialize)]
pub struct PlanTargetDto {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Uuid,
    pub label: Option<String>,
}

/// `MaintenancePlan`
#[derive(Debug, Serialize)]
pub struct PlanDto {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub code: String,
    pub name: String,
    pub template_id: Uuid,
    pub template_name: String,
    pub target: PlanTargetDto,
    pub trigger_type: String,
    pub rrule: Option<String>,
    pub meter_code: Option<String>,
    pub meter_threshold: Option<f64>,
    pub generate_lead_days: i16,
    /// 完工容許窗（063）。由管理者定義：法定年檢 0、月保養 7。
    /// 合規報表的準時判定用它。
    pub completion_grace_days: i16,
    pub priority: String,
    pub assigned_team_id: Option<Uuid>,
    pub next_due_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: bool,
}

/// `MaintenancePlanCreate`
#[derive(Debug, Deserialize)]
pub struct PlanCreate {
    pub facility_id: Option<Uuid>,
    pub template_id: Option<Uuid>,
    pub code: Option<String>,
    pub name: Option<String>,
    pub asset_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub category_code: Option<String>,
    pub trigger_type: Option<String>,
    pub rrule: Option<String>,
    pub meter_code: Option<String>,
    pub meter_threshold: Option<f64>,
    pub generate_lead_days: Option<i32>,
    pub completion_grace_days: Option<i32>,
    pub priority: Option<String>,
    pub assigned_team_id: Option<Uuid>,
    pub sla_policy_id: Option<Uuid>,
}

/// `GET /maintenance-plans` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub trigger_type: Option<String>,
    pub due_before: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `GET /maintenance-plans/{planId}/preview-schedule` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    pub until: Option<chrono::NaiveDate>,
    pub limit: Option<u16>,
}

/// preview-schedule 的一項。
#[derive(Debug, Serialize)]
pub struct PreviewItemDto {
    pub scheduled_for: chrono::DateTime<chrono::Utc>,
    pub asset_id: Uuid,
    pub asset_code: String,
}
