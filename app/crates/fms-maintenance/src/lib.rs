//! 預防性維護（PM）模組：計畫、排程展開、產生器（WBS 5.x）。
//!
//! 三個端點（`GET/POST /maintenance-plans`、
//! `GET /maintenance-plans/{planId}/preview-schedule`）與**產生器**共用
//! 同一份排程展開邏輯（[`schedule`]）。這件事是刻意的：契約說
//! preview-schedule 的用途是「讓管理員在啟用計畫前確認 RRULE 展開結果，
//! 避免產生大量錯誤工單」—— 若 preview 與產生器各算一次，
//! preview 就失去了它唯一的價值。
pub mod dto;
pub mod generator;
pub mod handlers;
pub mod occurrences;
pub mod pm_worker;
pub mod relay_handler;
pub mod repo;
pub mod templates;
/// RRULE 展開已搬到 `fms_shared::schedule`：RFC 5545 是規格而非保養領域的概念，
/// 而預約的週期展開要用的是同一份實作（見 fms-reservation）。
pub use fms_shared::schedule;
