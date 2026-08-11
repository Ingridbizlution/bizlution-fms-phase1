//! 形狀對齊 `openapi.yaml` 的 `Reservation` / `ReservationCreate` / `ReservationUpdate`。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `Reservation.organizer`
#[derive(Debug, Serialize)]
pub struct OrganizerDto {
    pub id: Uuid,
    pub display_name: String,
}

/// `Reservation`
#[derive(Debug, Serialize)]
pub struct ReservationDto {
    pub id: Uuid,
    pub reservation_no: String,
    pub facility_id: Uuid,
    pub resource_id: Uuid,
    pub resource_name: String,
    pub resource_type: String,
    pub title: Option<String>,
    pub purpose: Option<String>,
    pub party_size: i32,
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
    pub status: String,
    /// **可為 null**：私人預約對非本人／非 `reservation:view_private`
    /// 持有者遮罩主辦人。
    ///
    /// 契約因此把它改成 nullable。為什麼不是「保留 id、只遮 display_name」：
    /// 那樣仍然可以拿 id 去 `GET /users/{id}` 換回姓名 —— 一個看起來有遮罩、
    /// 實際上只多一次請求的遮罩比沒有遮罩更糟，因為它讓人以為問題解決了。
    pub organizer: Option<OrganizerDto>,
    pub approval_required: bool,
    pub requires_check_in: bool,
    pub checked_in_at: Option<chrono::DateTime<chrono::Utc>>,
    pub auto_release_at: Option<chrono::DateTime<chrono::Utc>>,
    pub recurrence_group_id: Option<Uuid>,
    pub created_via: String,
    pub version: i32,
    /// 011 的私人預約旗標。**遮罩與否都回傳它** —— 客戶端要靠它決定
    /// 渲染「已預約」還是標題。
    ///
    /// 單一旗標就夠：`is_private && title == null` 是「被遮罩」，
    /// `is_private && title != null` 是「我有權看」。再加一個 `masked`
    /// 旗標不會多給客戶端任何它算不出來的資訊。
    pub is_private: bool,
}

/// `ReservationCreate.services[]`
///
/// 契約：「附加的軟性服務；建立成功後由事件驅動產生對應工單」。
/// 產生工單的 fan-out worker 尚未實作，因此本切片只負責**正確登記**：
/// 驗證 `service_items` 宣告的每一條規則、算出服務班表、寫入
/// `fms.reservation_services`。工單由 `reservation.confirmed` 事件的
/// 訂閱者日後補上（005 的觸發器已經在發那個事件）。
#[derive(Debug, Deserialize)]
pub struct ServiceRequest {
    pub service_item_id: Uuid,
    /// 契約是 `number`，資料庫是 `numeric(12,2)`。沿用本專案既有做法
    /// （工單的 `total_cost`）以 `float8` 進出，避免為兩位小數引入
    /// 一個新的 decimal 依賴。
    #[serde(default = "default_quantity")]
    pub quantity: f64,
    pub payload: Option<serde_json::Value>,
    pub notes: Option<String>,
}

fn default_quantity() -> f64 {
    1.0
}

/// `ReservationCreate.participants[]`。契約允許的角色只有 `ATTENDEE`／
/// `OPTIONAL`——`ORGANIZER`／`RESOURCE_OWNER` 是伺服端語意（建立者自動是
/// organizer），不開放由呼叫端指定。
#[derive(Debug, Deserialize)]
pub struct ParticipantRequest {
    pub user_id: Option<Uuid>,
    pub external_email: Option<String>,
    #[serde(default = "default_participant_role")]
    pub role: String,
}

fn default_participant_role() -> String {
    "ATTENDEE".to_string()
}

/// `ReservationCreate`。本切片支援核心欄位、`hold_token`、`services`、
/// `recurrence_rule` 與 `participants`。
#[derive(Debug, Deserialize)]
pub struct CreateReservation {
    pub resource_id: Uuid,
    pub title: Option<String>,
    pub purpose: Option<String>,
    #[serde(default = "default_party_size")]
    pub party_size: i32,
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
    /// 兩階段預約的第二階段。契約：「若先取得佔位則帶入，伺服端會消耗該 hold」。
    ///
    /// 選用 —— 沒帶就是單階段建立。帶了但不可用（過期／已消耗／不是自己的／
    /// 範圍不涵蓋）一律 409，見 `repo::consume_hold`。
    pub hold_token: Option<String>,
    /// 附加的軟性服務。空陣列與未帶等價。
    #[serde(default)]
    pub services: Vec<ServiceRequest>,
    /// RFC 5545 RRULE。契約：「伺服端展開為多筆預約並回傳 recurrence_group_id」。
    ///
    /// `start_at`／`end_at` 是**第一次**的時段；展開後每一筆沿用同樣的時長。
    /// 展開視窗上界來自資源的 `advance_booking_days`。
    pub recurrence_rule: Option<String>,
    /// 私人預約（011）。未帶等於 `false`。
    ///
    /// **不需要額外權限** —— 隱私是主辦人對自己會議的選擇，而建立者就是主辦人。
    /// 有週期規則時整個系列一起套用（旗標在 INSERT 上，展開的每一筆都帶著它）。
    pub is_private: Option<bool>,
    /// 與會者。空陣列與未帶等價。每筆需要 `user_id` 或 `external_email`
    /// 其中一個（DB 端 `ck_participant_identity` 也會擋，這裡先擋一次給出
    /// 更清楚的 422，見 handlers 的驗證）。
    #[serde(default)]
    pub participants: Vec<ParticipantRequest>,
}

/// `ReservationDetail.participants[]`
#[derive(Debug, Serialize)]
pub struct ParticipantDto {
    pub user_id: Option<Uuid>,
    pub display_name: Option<String>,
    pub external_email: Option<String>,
    pub role: String,
    pub response: String,
}

/// `ReservationDetail.services[]`
#[derive(Debug, Serialize)]
pub struct ReservationServiceDto {
    pub id: Uuid,
    pub service_item_id: Uuid,
    pub service_name: String,
    pub quantity: f64,
    pub payload: serde_json::Value,
    pub service_start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: String,
    /// fan-out worker 尚未實作，因此目前恆為 `None`。契約把它標為可空，
    /// 所以這是符合契約的狀態，不是缺欄位。
    pub work_order: Option<serde_json::Value>,
}

fn default_party_size() -> i32 {
    1
}

/// `ReservationUpdate`
#[derive(Debug, Deserialize)]
pub struct UpdateReservation {
    pub title: Option<String>,
    pub purpose: Option<String>,
    pub party_size: Option<i32>,
    pub start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub end_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 私人預約旗標（011）。
    ///
    /// **改它需要的條件比改其他欄位嚴格。** PATCH 對非主辦人只要求
    /// `reservation:update`，而把 `is_private` 從 `true` 改成 `false`
    /// 的效果就是**揭露內容** —— 那會讓一個沒有 `reservation:view_private`
    /// 的人只要有 update 權限就繞過整個遮罩。
    ///
    /// 因此判定重用遮罩自己的條件（見 handlers 的 `update`）：
    /// **看得到遮罩後內容的人，不能改這個旗標。**
    pub is_private: Option<bool>,
    /// 週期預約的編輯範圍：`THIS`（預設）／`THIS_AND_FOLLOWING`／`ALL`。
    ///
    /// 只對 `title`／`purpose`／`party_size`／`is_private` 生效——非 `THIS`
    /// 時若同時帶 `start_at`／`end_at` 會回 422（見 handlers 的 `update`：
    /// 時段是每一次各自的，「整個系列一起改時間」沒有單一定義的語意，
    /// 不像 `is_private` 那種整系列共用同一個值的旗標）。
    #[serde(default = "default_apply_scope")]
    pub apply_scope: String,
}

fn default_apply_scope() -> String {
    "THIS".to_string()
}

/// `GET /reservations` 的查詢參數
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub resource_id: Option<Uuid>,
    pub organizer_id: Option<Uuid>,
    #[serde(default)]
    pub mine: bool,
    pub status: Option<String>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `ResourceAvailability.rules`
#[derive(Debug, Serialize)]
pub struct ResourceRulesDto {
    pub min_duration_minutes: i32,
    pub max_duration_minutes: i32,
    pub slot_granularity_minutes: i32,
    pub requires_approval: bool,
    pub advance_booking_days: i32,
}

/// `ResourceAvailability.busy[]`
#[derive(Debug, Serialize)]
pub struct BusyBlockDto {
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    pub reason: Option<String>,
}

/// `ResourceAvailability.free_slots[]`
#[derive(Debug, Serialize)]
pub struct FreeSlotDto {
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
}

/// `ResourceAvailability`
#[derive(Debug, Serialize)]
pub struct ResourceAvailabilityDto {
    pub resource_id: Uuid,
    pub resource_type: String,
    pub display_name: String,
    pub capacity: i32,
    pub opening_hours: serde_json::Value,
    pub rules: ResourceRulesDto,
    pub busy: Vec<BusyBlockDto>,
    pub free_slots: Vec<FreeSlotDto>,
}

/// `GET /facilities/{facilityId}/availability` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct AvailabilityQuery {
    /// 逗號分隔；省略則回該設施所有可預約資源。
    pub resource_ids: Option<String>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub slot_minutes: Option<i32>,
    pub min_capacity: Option<i32>,
}

/// `POST /reservations/holds` 的請求。
#[derive(Debug, Deserialize)]
pub struct HoldCreate {
    pub resource_id: Option<Uuid>,
    pub start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub end_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ttl_seconds: Option<i32>,
}

/// 佔位成功的回應。
#[derive(Debug, Serialize)]
pub struct HoldDto {
    pub hold_token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub resource_id: Uuid,
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
}

/// `POST /reservations/{id}/check-in` 的請求。
#[derive(Debug, Deserialize)]
pub struct CheckInRequest {
    pub method: Option<String>,
}

/// `DELETE /reservations/{id}` 的請求（選填原因）。
#[derive(Debug, Deserialize)]
pub struct CancelRequest {
    pub reason: Option<String>,
}

/// `POST /reservations/{id}/reject` 的請求。原因必填。
#[derive(Debug, Deserialize)]
pub struct RejectRequest {
    pub reason: Option<String>,
}

/// 即時佔用地圖的一列。
#[derive(Debug, Serialize)]
pub struct OccupancyDto {
    pub resource_id: Uuid,
    pub display_name: String,
    pub resource_type: String,
    pub capacity: i32,
    /// `FREE` / `OCCUPIED`（已報到）/ `RESERVED`（已訂未報到）/ `HELD`（佔位中）
    pub state: String,
    pub reservation_id: Option<Uuid>,
    pub title: Option<String>,
    pub organizer_name: Option<String>,
    pub start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub end_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 私人預約：`title` 與 `organizer_name` 已被遮罩成 `null`。
    /// `state` 與時段照舊 —— 那正是 011 說「只看得到『已預約』與時段」的意思。
    pub is_private: bool,
}
