//! 組織、場域、空間節點（WBS Tenancy／Spatial）。
//!
//! # 為什麼這一層先於其他功能
//!
//! 資產、工單、預約、保養計畫全都以 `facility_id` 與 `spatial_node_id`
//! 為必要輸入。在本模組之前，那些 id **只能來自種子資料** ——
//! 也就是說沒有 SQL 存取權的人無法開通一個租戶，系統無法端到端示範。
//!
//! # 兩棵樹的路徑都由資料庫維護
//!
//! `organizations.org_path` 與 `spatial_nodes.node_path` 都是 `ltree`，
//! 各有一個觸發器由 `parent_id + code` 推導路徑與深度，**且在改變 parent
//! 時重算整棵子樹**（`trg_organization_path`、`trg_spatial_node_path`）。
//!
//! 因此本模組**不計算路徑**：只寫 `parent_id` 與 `code`，路徑欄位交給觸發器。
//! 在應用層算一份就是製造第二個真實來源，而子樹搬移的路徑重算正是最容易
//! 寫錯的一類 SQL。
pub mod bim;
pub mod dto;
pub mod floor_plan_markers;
pub mod handlers;
pub mod repo;
pub mod spatial_tail;
pub mod tail;
pub mod tenant;
