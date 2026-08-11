//! 形狀對齊 `openapi.yaml` 的 `WorkOrder` / `WorkOrderCreate` /
//! `WorkOrderUpdate` / `WorkOrderTransitionRequest`。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `WorkOrder.asset`、`WorkOrder.requester`、`WorkOrder.assignee` 這類
/// 「嵌一個小物件而非只給 id」的欄位。
///
/// 契約刻意這樣設計：工單列表要顯示「4F 空調箱 / 王技師」，
/// 若只回 id，前端每列都要再查兩次。這是把 N+1 從前端移到一次 JOIN。
#[derive(Debug, Serialize)]
pub struct AssetRefDto {
    pub id: Uuid,
    pub asset_code: String,
    pub name: String,
}

/// `WorkOrder.location`
#[derive(Debug, Serialize)]
pub struct LocationDto {
    pub spatial_node_id: Uuid,
    pub name: String,
    pub node_path: Option<String>,
}

/// `WorkOrder.requester` / `WorkOrder.assignee`
#[derive(Debug, Serialize)]
pub struct UserRefDto {
    pub id: Uuid,
    pub display_name: String,
}

/// `WorkOrder`
#[derive(Debug, Serialize)]
pub struct WorkOrderDto {
    pub id: Uuid,
    pub wo_no: String,
    pub facility_id: Uuid,
    pub work_order_type: String,
    pub source: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    /// 由 `work_order_statuses.category` 帶出，不是應用層推導的 ——
    /// 狀態的分類是資料，改一個狀態的歸屬不該需要改程式。
    pub status_category: String,
    pub priority: String,
    pub asset: Option<AssetRefDto>,
    pub location: Option<LocationDto>,
    pub service_item_id: Option<Uuid>,
    pub reservation_id: Option<Uuid>,
    pub alarm_id: Option<Uuid>,
    pub requester: Option<UserRefDto>,
    pub assignee: Option<UserRefDto>,
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
    pub version: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// `WorkOrderCreate`
#[derive(Debug, Deserialize)]
pub struct WorkOrderCreate {
    pub facility_id: Option<Uuid>,
    pub work_order_type: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub asset_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub service_item_id: Option<Uuid>,
    pub reservation_id: Option<Uuid>,
    pub priority: Option<String>,
    pub requested_start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub payload: Option<serde_json::Value>,
    pub team_id: Option<Uuid>,
    pub assignee_id: Option<Uuid>,
    /// `true` 時建立為 `DRAFT`，否則 `SUBMITTED`（契約預設 false）。
    #[serde(default)]
    pub as_draft: bool,
}

/// `WorkOrderUpdate`。刻意沒有 `status` 欄位 ——
/// 契約明訂狀態變更一律走 transitions，避免繞過狀態機。
#[derive(Debug, Deserialize)]
pub struct WorkOrderUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub priority: Option<String>,
    pub team_id: Option<Uuid>,
    pub scheduled_start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub scheduled_end_at: Option<chrono::DateTime<chrono::Utc>>,
    pub payload: Option<serde_json::Value>,
    pub is_chargeback: Option<bool>,
    pub chargeback_org_id: Option<Uuid>,
}

/// `WorkOrderTransitionRequest`
///
/// 除 `action` 外的欄位都是「某些動作的必填欄位」。哪些是必填**不寫在這裡**，
/// 而是由 `work_order_transitions_allowed.required_fields` 決定 ——
/// 那是資料，租戶可以覆寫；寫在 Rust 裡就固定了。
#[derive(Debug, Deserialize)]
pub struct TransitionRequest {
    pub action: Option<String>,
    pub assignee_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub scheduled_start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reason: Option<String>,
    pub resolution_notes: Option<String>,
    pub close_code: Option<String>,
    pub root_cause: Option<String>,
    pub labor_minutes: Option<i32>,
    /// 契約的 `parts_used`。`quantity` 是實際領用量。
    pub parts_used: Option<Vec<PartUsage>>,
    pub metadata: Option<serde_json::Value>,
}

/// `WorkOrderTransitionRequest.parts_used[]`
#[derive(Debug, Deserialize)]
pub struct PartUsage {
    pub part_id: Option<Uuid>,
    pub quantity: Option<f64>,
}

/// `available-actions` 的一項。
#[derive(Debug, Serialize)]
pub struct AvailableActionDto {
    pub action: String,
    pub to_status: String,
    /// 來自 015 的 `work_order_actions` catalog。缺列時為 `null` ——
    /// 標籤是顯示用資料，缺了不該讓動作變得不可執行。
    pub label_zh: Option<String>,
    pub required_fields: Vec<String>,
    /// 目前使用者是否具備 `required_permission`。
    /// 動作仍然列出來（前端要顯示成 disabled 而不是整顆消失，
    /// 否則使用者不知道「有這個動作但我沒權限」）。
    pub permitted: bool,
}

/// `GET /work-orders` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub work_order_type: Option<String>,
    /// 逗號分隔多值（契約如此定義）。
    pub status: Option<String>,
    pub status_category: Option<String>,
    pub priority: Option<String>,
    pub assignee_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub source: Option<String>,
    pub sla_state: Option<String>,
    /// 只回與目前使用者相關（負責人或申請人）的工單。
    pub mine: Option<bool>,
    pub created_from: Option<chrono::DateTime<chrono::Utc>>,
    pub created_to: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
    pub fields: Option<String>,
    pub sort: Option<String>,
}

/// `GET /work-orders/{workOrderId}` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct GetQuery {
    pub include: Option<String>,
}

/// `WorkOrderDetail.transitions[]`
#[derive(Debug, Serialize)]
pub struct TransitionLogDto {
    pub from_status: Option<String>,
    pub action: String,
    pub to_status: String,
    pub actor_name: Option<String>,
    pub reason: Option<String>,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
}

/// `WorkOrderTask`
#[derive(Debug, Serialize)]
pub struct TaskDto {
    pub id: Uuid,
    pub seq: i16,
    pub title: String,
    pub input_type: String,
    pub unit: Option<String>,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    pub is_required: bool,
    pub result_value: Option<serde_json::Value>,
    pub is_pass: Option<bool>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// `PATCH /work-orders/{workOrderId}/tasks/{taskId}` 的請求。
///
/// `result_value` 在契約中是無型別的（`{}`）：實際型別由該項目的
/// `input_type` 決定，因此驗證在執行期做，見 handler 的 `validate_result`。
#[derive(Debug, Deserialize)]
pub struct TaskUpdate {
    pub result_value: Option<serde_json::Value>,
    pub is_pass: Option<bool>,
    pub notes: Option<String>,
}

/// `WorkOrderDetail.comments[]`
#[derive(Debug, Serialize)]
pub struct CommentDto {
    pub id: Uuid,
    pub author_name: Option<String>,
    pub visibility: String,
    pub body: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// `WorkOrderDetail.parts[]`
#[derive(Debug, Serialize)]
pub struct UsedPartDto {
    pub part_code: String,
    pub name: String,
    pub quantity_used: f64,
    pub total_cost: Option<f64>,
}

/// `WorkOrderDetail.labor[]`
#[derive(Debug, Serialize)]
pub struct LaborDto {
    pub user_name: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub minutes: Option<i32>,
    /// 目前一律為 null：全 schema 沒有費率來源（見 repo::record_labor）。
    pub cost: Option<f64>,
    pub is_overtime: bool,
}

/// 新增留言的請求。
#[derive(Debug, Deserialize)]
pub struct CommentCreate {
    pub body: Option<String>,
    pub visibility: Option<String>,
}
