//! 站內通知收件匣（ENDPOINTS.md §10）。
//!
//! # 為什麼是獨立 crate
//!
//! 通知是跨領域的：工單狀態機、告警、預約審核、配額都會產生它。
//! 與 `fms-catalogue`／`fms-attachment`／`fms-report` 同一個形狀 ——
//! 放進任何一個領域 crate，其餘的就得跨模組寫別人的資料。
//!
//! # RLS 不夠
//!
//! `fms.notifications` 只有 `tenant_isolation` 政策，**沒有按收件人的過濾**。
//! 因此收件匣必須自己加 `recipient_user_id = 呼叫者`。少了那個條件，
//! 每個人都會看到同租戶所有人的通知 —— 而 RLS 不會攔下來。
//!
//! # 只回 `IN_APP`
//!
//! 收件匣就是站內通知。`EMAIL`／`PUSH` 的列是**傳輸紀錄**，不是收件匣項目
//! （而且目前沒有傳輸層，它們會停在 `QUEUED` —— 見 migration 041 檔頭）。
pub mod handlers;
pub mod templates;
pub mod webhooks;
