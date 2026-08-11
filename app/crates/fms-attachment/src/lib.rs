//! 附件模組（WBS S5）。
//!
//! # 契約原本沒有任何附件端點
//!
//! `openapi.yaml` 定義了 `Attachment` schema，並在
//! `WorkOrderDetail.attachments` 與 `WorkOrderCreate.attachment_ids` 引用它，
//! 但**沒有任何端點能產生附件**。也就是說契約要求客戶端提供
//! `attachment_ids`，卻沒給它取得 id 的方法 —— 那個欄位在契約自身的範圍內
//! 是不可用的。
//!
//! 本模組補上最小的三支端點（建立、取得、刪除），並同步更新
//! `openapi.yaml`。這是**新增契約面**而不是重新解讀既有條文，
//! 因此在 docs/WBS-rebaseline.md 4.1j 單獨記錄，以便審閱。
pub mod dto;
pub mod handlers;
pub mod repo;
