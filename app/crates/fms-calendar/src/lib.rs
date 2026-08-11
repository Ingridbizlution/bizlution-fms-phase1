//! 多租戶日曆聯邦（Microsoft 365／Google Workspace 資源同步）。見
//! `docs/adr/ADR-14-calendar-federation.md`（ADR-14 決定 K：獨立 crate，
//! 不塞進 `fms-identity`／`fms-reservation`）。
//!
//! * `provider` —— `CalendarProvider` trait，跟外部平台講話的統一介面。
//! * `microsoft` —— Graph 的實作（app-only client-credentials）。
//! * `watchdog` —— 拉入側的排程輪詢（`fms-jobs` 掛成第 14 個背景迴圈，
//!   跟 `fms_identity::directory_sync_watchdog` 同一個組裝方式）。
//! * `push` —— 推出側：掛在既有的 outbox relay 上（`fms_worker::EventHandler`），
//!   跟 `fms_maintenance::relay_handler::MaintenanceEventHandler` 同一個形狀。
//!
//! `watchdog`／`push` 都會碰 Postgres——與 `directory_sync_watchdog`／
//! `MaintenanceEventHandler` 放在各自的領域 crate（而不是 `fms-jobs`）同一個
//! 判斷：排程與事件處理邏輯屬於它所服務的那個領域，`fms-jobs` 只負責把各
//! 領域的迴圈組裝起來。
//!
//! # 為什麼留在 Rust，不比照 BIM 去 Python
//!
//! BIM 去 Python 是因為 IFC 解析需要 IfcOpenShell，Rust 沒有對等生態
//! （ADR-09）。Microsoft Graph／Google Calendar 都是普通的 REST+JSON+OAuth2，
//! `reqwest` 完全夠用，沒有離開 Rust 生態的理由（ADR-14 決定 K）。

pub mod handlers;
pub mod microsoft;
pub mod provider;
pub mod push;
pub mod watchdog;

pub use provider::{
    build_provider, CalendarProvider, DeltaPage, ExternalEvent, ExternalResource, OutboundEvent,
    ProviderBuildIssue,
};
pub use push::CalendarPushHandler;
pub use watchdog::{CalendarSyncWatchdog, CalendarSyncWatchdogConfig, Sweep};
