//! 報表（ENDPOINTS.md §11 Reporting）。
//!
//! # 為什麼是獨立 crate
//!
//! 契約有八支報表端點，而它們**跨領域**：`sla-compliance` 讀工單，
//! `space-utilization` 讀預約，`asset-reliability` 讀設備，
//! `group-rollup` 跨組織彙總全部。放進任何一個領域 crate，其餘的報表就得
//! 跨模組讀別人的資料 —— 與 `fms-catalogue`／`fms-attachment` 是同一個形狀，
//! 而這個專案對那個形狀的既有答案就是獨立 crate。
//!
//! # 為什麼這一層很薄
//!
//! 計算全在 SQL 函式裡（migration 034，ADR-09 紀律 2）。這裡只做三件事：
//! 驗參數、呼叫函式、把結果轉成契約的形狀。
//!
//! 那不只是風格：函式是 `SECURITY INVOKER`，因此 RLS 會自動把場域範圍的
//! 使用者限制在他看得見的工單上。若把彙總搬到 Rust，範圍過濾就得在應用層
//! 再寫一份 —— 同一條規則的第二份實作。
//!
//! 匯出（`export`）是唯一不走這個形狀的：它不算數字，只把某一支報表的
//! 結果轉成檔案，而算數字的還是同一個 SQL 函式。
pub mod dashboard;
pub mod dto;
pub mod export;
pub mod handlers;
pub mod repo;
