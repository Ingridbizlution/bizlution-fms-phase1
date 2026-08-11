//! 預約與空間排程模組（規格書 L2 的 reservation 模組）。
//!
//! 這個切片的價值在於它同時帶出四個橫切關注點（cursor 分頁、Idempotency-Key、
//! If-Match 樂觀鎖、409 衝突映射含 40P01 重試語意），以及兩支資料庫函式
//! （`check_resource_availability`、`next_document_no`）的接法。
pub mod blackouts;
pub mod dto;
pub mod handlers;
pub mod no_show;
pub mod reminder;
pub mod repo;
pub mod tail;
