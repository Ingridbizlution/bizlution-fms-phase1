//! 形狀對齊 `openapi.yaml` 的 `Organization`／`Facility`／`SpatialNode`。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `Organization`
#[derive(Debug, Serialize)]
pub struct OrganizationDto {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub code: String,
    pub name: String,
    pub org_type: String,
    /// `ltree` 以文字形式對外。由觸發器維護，不由應用層計算。
    pub org_path: String,
    pub depth: i32,
    pub cost_center: Option<String>,
    pub facility_count: i64,
    pub status: String,
}

/// `OrganizationCreate`
#[derive(Debug, Deserialize)]
pub struct OrganizationCreate {
    pub parent_id: Option<Uuid>,
    pub code: Option<String>,
    pub name: Option<String>,
    pub org_type: Option<String>,
    pub cost_center: Option<String>,
    pub manager_user_id: Option<Uuid>,
    pub attributes: Option<serde_json::Value>,
}

/// `Facility`
#[derive(Debug, Serialize)]
pub struct FacilityDto {
    pub id: Uuid,
    pub org_id: Uuid,
    pub code: String,
    pub name: String,
    pub facility_type: String,
    pub address_line1: Option<String>,
    pub city: Option<String>,
    pub country_code: String,
    pub timezone: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub gross_area_sqm: Option<f64>,
    pub operating_hours: serde_json::Value,
    pub status: String,
    /// 契約宣告了 `version`，但 `fms.facilities` **沒有這個欄位**
    /// （不像 assets／work_orders 有 `trg_bump_version`）。
    /// 因此樂觀鎖無從實作，這裡回 `updated_at` 的秒級 epoch 作為版本標記 ——
    /// 見 handlers 的說明與 docs/WBS-rebaseline.md 4.1r。
    pub version: i64,
}

/// `FacilityCreate`。PATCH 沿用同一個 schema（契約如此）。
#[derive(Debug, Deserialize)]
pub struct FacilityWrite {
    pub org_id: Option<Uuid>,
    pub code: Option<String>,
    pub name: Option<String>,
    pub facility_type: Option<String>,
    pub address_line1: Option<String>,
    pub city: Option<String>,
    pub country_code: Option<String>,
    pub timezone: Option<String>,
    pub gross_area_sqm: Option<f64>,
    pub operating_hours: Option<serde_json::Value>,
    pub attributes: Option<serde_json::Value>,
}

/// `SpatialNode`
#[derive(Debug, Serialize)]
pub struct SpatialNodeDto {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub node_type_code: String,
    pub code: String,
    pub name: String,
    pub node_path: String,
    pub depth: i16,
    pub floor_level: Option<i32>,
    pub floor_label: Option<String>,
    pub area_sqm: Option<f64>,
    pub capacity: i32,
    pub is_bookable: bool,
    pub status: String,
    pub health_score: Option<f64>,
    pub utilization_pct: Option<f64>,
    pub bim_element_id: Option<String>,
    pub asset_count: i64,
    pub open_work_order_count: i64,
    /// 僅在 `view=tree` 時出現。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<SpatialNodeDto>>,
}

/// `SpatialNodeCreate`。也用於 PATCH 式的搬移（改 `parent_id`）。
#[derive(Debug, Deserialize)]
pub struct SpatialNodeCreate {
    pub parent_id: Option<Uuid>,
    pub node_type_code: Option<String>,
    pub code: Option<String>,
    pub name: Option<String>,
    pub floor_level: Option<i32>,
    pub floor_label: Option<String>,
    pub area_sqm: Option<f64>,
    pub capacity: Option<i32>,
    pub is_bookable: Option<bool>,
    pub bim_model_id: Option<Uuid>,
    pub bim_element_id: Option<String>,
    pub geometry: Option<serde_json::Value>,
    pub attributes: Option<serde_json::Value>,
}

/// `GET /organizations` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct OrgQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `GET /facilities` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct FacilityQuery {
    pub org_id: Option<Uuid>,
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `GET /facilities/{facilityId}/spatial-nodes` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct NodeQuery {
    pub view: Option<String>,
    pub parent_id: Option<Uuid>,
    pub subtree_of: Option<Uuid>,
    pub node_type_code: Option<String>,
    pub floor_level: Option<i32>,
    pub bookable_only: Option<bool>,
    pub include_asset_counts: Option<bool>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}
