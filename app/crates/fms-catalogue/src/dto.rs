//! 形狀對齊 `openapi.yaml` 的 `ServiceItem`，不多不少。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `GET /facilities/{facilityId}/service-items` 的查詢參數。
///
/// 三個過濾條件都來自契約。`attachable_to_reservation` 與 `standalone_only`
/// 刻意保持獨立而非合併成一個 enum：契約就是兩個布林，而它們在資料庫裡
/// 也是兩個獨立欄位（一個服務可以同時兩者皆可）。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub category: Option<String>,
    pub attachable_to_reservation: Option<bool>,
    pub standalone_only: Option<bool>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `ServiceItem.sla`
#[derive(Debug, Serialize)]
pub struct SlaDto {
    pub response_minutes: i32,
    pub resolution_minutes: i32,
}

/// `ServiceItem`
#[derive(Debug, Serialize)]
pub struct ServiceItemDto {
    pub id: Uuid,
    /// `null` 代表適用所有場域。
    pub facility_id: Option<Uuid>,
    pub category: String,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub lead_time_minutes: i32,
    pub default_duration_minutes: i32,
    pub relative_offset_minutes: i32,
    pub is_attachable_to_reservation: bool,
    pub is_standalone_requestable: bool,
    pub requires_approval: bool,
    pub chargeable: bool,
    pub unit_price: Option<f64>,
    pub currency: Option<String>,
    pub unit_label: Option<String>,
    pub max_quantity: Option<i32>,
    /// 前端據此渲染動態表單，並在送出前先驗一次；伺服端仍會再驗
    /// （`fms_shared::form_schema`）。
    pub form_schema: serde_json::Value,
    /// `sla_policies` 未設定時為 `None`。契約把它列為物件而非必填，
    /// 因此不出現是合法的 —— 刻意不填 0，那會被讀成「零分鐘內必須回應」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sla: Option<SlaDto>,
    pub icon: Option<String>,
    pub display_order: i32,
}
