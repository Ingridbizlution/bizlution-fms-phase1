//! 工單模組（規格書 L2 的 work_order 模組；WBS S4）。
//!
//! 本模組的核心不是 CRUD，而是**狀態機**：狀態變更只能經由
//! `POST /work-orders/{id}/transitions`，且判定完全委派給資料庫的
//! `fms.transition_work_order()` 與 `work_order_transitions_allowed` 表。
//! 應用層不複製一份狀態圖 —— 複製就會漂移，而 004 的觸發器
//! `trg_enforce_wo_transition` 連直接 UPDATE 都會擋，兩份規則不一致時
//! 應用層會先放行、資料庫再拒絕，錯誤訊息卻來自最底層。
pub mod dto;
pub mod handlers;
pub mod holiday;
pub mod repo;
pub mod satisfaction;
pub mod sla_policy;
pub mod tail;
