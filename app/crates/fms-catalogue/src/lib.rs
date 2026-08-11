//! 軟性服務型錄（ENDPOINTS.md §6 Service Catalogue）。
//!
//! # 為什麼是獨立 crate 而不是塞進預約或工單
//!
//! `fms.service_items` 有兩個消費者，而**兩者都不擁有它**：
//!   * 預約的附加服務（`POST /reservations` 的 `services[]`）
//!   * 獨立申請的 SERVICE 類工單（`POST /work-orders` 的 `service_item_id`）
//!
//! 放進其中任一個，另一個就得跨模組讀別人的資料。這與 `fms-attachment`
//! 是同一個形狀（附件掛在工單／預約／設備／BIM 上，四者都不擁有它），
//! 而這個專案對那個形狀的既有答案就是獨立 crate —— 因此這裡不是新的抽象，
//! 是沿用既有結構。
//!
//! 五支端點：型錄瀏覽（`handlers`）與管理面＋可用時段（`admin`）。
pub mod admin;
pub mod dto;
pub mod handlers;
pub mod repo;
