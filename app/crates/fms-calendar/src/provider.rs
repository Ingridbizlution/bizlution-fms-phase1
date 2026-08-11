//! `CalendarProvider`：一個外部日曆平台的統一介面（ADR-14 決定 G）。
//!
//! 呼叫端（watchdog、推出消費者）不需要知道底下是 Microsoft Graph 還是
//! Google Calendar API——這也是 Google 之後只需要「補一個 trait 實作」，
//! 不必重新設計呼叫端的原因。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use fms_shared::Problem;
use uuid::Uuid;

/// 外部平台上的一個房間／資源日曆（ADR-14 決定 J 的比對來源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalResource {
    /// Microsoft 的房間信箱 UPN，或 Google 的資源 email。
    pub external_id: String,
    pub display_name: String,
}

/// 從外部日曆拉回來的一個事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalEvent {
    pub external_event_id: String,
    pub subject: Option<String>,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    /// delta 查詢回報「這個事件被刪除了」時是 true——呼叫端要把對應的
    /// 合成預約標記取消，不是插入新列。
    pub is_deleted: bool,
    /// 我們自己打的標記（ADR-14 決定 I 的防迴圈機制）。`Some` 代表這是
    /// FMS 推出去的事件的回音，呼叫端應該略過，不當成新的外部事件 import；
    /// 若時間與 FMS 這邊不一致，代表使用者直接在外部日曆改了時間，
    /// 該進 `calendar_sync_conflicts`，不是靜默覆蓋。
    pub fms_reservation_id: Option<Uuid>,
}

/// 一次增量拉取的結果。
#[derive(Debug, Clone)]
pub struct DeltaPage {
    pub events: Vec<ExternalEvent>,
    /// 下一次同步要帶的游標，存進 `calendar_resource_mappings.delta_token`。
    /// 兩家平台的格式不同（Graph 是完整的 deltaLink URL，Google 是
    /// `syncToken` 字串），因此是不透明字串，呼叫端不應該解讀內容。
    pub next_delta_token: String,
}

/// 要推去外部日曆的最小事件內容。刻意不是 `fms_reservation::Reservation`
/// 本身——這個 crate 不依賴 `fms-reservation`，呼叫端只傳這裡定義的欄位。
#[derive(Debug, Clone)]
pub struct OutboundEvent {
    pub reservation_id: Uuid,
    pub subject: String,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
}

#[async_trait::async_trait]
pub trait CalendarProvider: Send + Sync {
    /// 列出這個租戶在該平台上可用的房間／資源日曆。
    async fn list_resources(&self) -> Result<Vec<ExternalResource>, Problem>;

    /// 增量拉取一個資源日曆自 `since` 之後的變化。`since = None` 代表
    /// 初始同步（全量），由 provider 實作決定用什麼查詢達成。
    async fn fetch_events_delta(
        &self,
        external_resource_id: &str,
        since: Option<&str>,
    ) -> Result<DeltaPage, Problem>;

    /// 建立一個外部事件並打上 `fms_reservation_id` 標記，回傳
    /// `external_event_id`（寫回 `reservations.external_event_id`）。
    async fn create_event(
        &self,
        external_resource_id: &str,
        event: &OutboundEvent,
    ) -> Result<String, Problem>;

    /// 取消／刪除一個外部事件。
    async fn cancel_event(
        &self,
        external_resource_id: &str,
        external_event_id: &str,
    ) -> Result<(), Problem>;
}

/// `build()` 建不出 provider 時的原因——呼叫端（watchdog、推出消費者）
/// 各自決定要怎麼計數／記錄，這裡只回報事實。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderBuildIssue {
    /// `provider = 'MS365'` 但整合列缺 `ms_tenant_id`。
    MissingMsTenantId,
    /// 這個 provider 還沒有實作（ADR-14 決定 K：目前只有 Microsoft）。
    Unsupported(String),
}

/// 依 `calendar_integrations.provider` 建構對應的 `CalendarProvider`。
/// watchdog（拉入）與推出消費者共用同一個分派邏輯，不各自維護一份。
pub fn build_provider(
    provider: &str,
    ms_tenant_id: Option<&str>,
    ms_app_client_id: &str,
    ms_app_client_secret: &str,
    ms_bases_override: Option<&(String, String)>,
) -> Result<Arc<dyn CalendarProvider>, ProviderBuildIssue> {
    match provider {
        "MS365" => {
            let ms_tenant_id = ms_tenant_id.ok_or(ProviderBuildIssue::MissingMsTenantId)?;
            let mut p = crate::microsoft::MicrosoftGraphProvider::new(
                ms_tenant_id.to_string(),
                ms_app_client_id.to_string(),
                ms_app_client_secret.to_string(),
            );
            if let Some((token_base, graph_base)) = ms_bases_override {
                p = p.with_bases(token_base.clone(), graph_base.clone());
            }
            Ok(Arc::new(p))
        }
        other => Err(ProviderBuildIssue::Unsupported(other.to_string())),
    }
}
