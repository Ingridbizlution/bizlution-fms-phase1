//! 形狀對齊 `openapi.yaml` 的 `Asset` / `AssetCreate`。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `Asset`
///
/// 四個欄位不是 `fms.assets` 的直接欄位，需由關聯取得：
///   * `category_code` —— assets 存的是 `category_id`，契約要 code
///   * `spatial_node_path` —— 來自 `spatial_nodes.node_path`（ltree）
///   * `open_work_order_count` / `active_alarm_count` —— 子查詢
#[derive(Debug, Serialize)]
pub struct AssetDto {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub spatial_node_id: Option<Uuid>,
    pub spatial_node_path: Option<String>,
    pub asset_code: String,
    pub name: String,
    pub serial_no: Option<String>,
    pub category_code: String,
    pub asset_model_id: Option<Uuid>,
    pub parent_asset_id: Option<Uuid>,
    pub criticality: String,
    pub status: String,
    pub install_date: Option<chrono::NaiveDate>,
    pub warranty_end_date: Option<chrono::NaiveDate>,
    pub health_score: Option<f64>,
    pub last_telemetry_at: Option<chrono::DateTime<chrono::Utc>>,
    pub open_work_order_count: i64,
    pub active_alarm_count: i64,
    pub specifications: serde_json::Value,
    pub attributes: serde_json::Value,
    pub version: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// `AssetCreate`。也用於 PATCH（契約的 update 沿用同一個 schema）。
#[derive(Debug, Deserialize)]
pub struct AssetWrite {
    pub facility_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub parent_asset_id: Option<Uuid>,
    pub asset_model_id: Option<Uuid>,
    /// 建立時必填；PATCH 時選填
    pub category_code: Option<String>,
    pub asset_code: Option<String>,
    pub name: Option<String>,
    pub serial_no: Option<String>,
    pub criticality: Option<String>,
    pub status: Option<String>,
    pub install_date: Option<chrono::NaiveDate>,
    pub warranty_end_date: Option<chrono::NaiveDate>,
    pub purchase_cost: Option<f64>,
    pub currency: Option<String>,
    pub custodian_user_id: Option<Uuid>,
    pub specifications: Option<serde_json::Value>,
    pub attributes: Option<serde_json::Value>,
}

/// `AssetDetail.relations[]`
#[derive(Debug, Serialize)]
pub struct RelationDto {
    pub relation_type: String,
    pub direction: String,
    pub impact_level: String,
    pub asset: AssetDto,
}

/// `AssetDetail.meters[]`
#[derive(Debug, Serialize)]
pub struct MeterDto {
    pub meter_code: String,
    pub name: String,
    pub unit: String,
    pub last_value: Option<f64>,
    pub last_read_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// `MaintenancePlan.target`
#[derive(Debug, Serialize)]
pub struct PlanTargetDto {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Uuid,
    pub label: Option<String>,
}

/// `MaintenancePlan`
#[derive(Debug, Serialize)]
pub struct PlanDto {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub code: String,
    pub name: String,
    pub template_id: Uuid,
    pub template_name: String,
    pub target: PlanTargetDto,
    pub trigger_type: String,
    pub rrule: Option<String>,
    pub meter_code: Option<String>,
    pub meter_threshold: Option<f64>,
    pub generate_lead_days: i16,
    pub priority: String,
    pub assigned_team_id: Option<Uuid>,
    pub next_due_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: bool,
}

/// `DependencyGraph.nodes[]`
#[derive(Debug, Serialize)]
pub struct GraphNodeDto {
    pub id: Uuid,
    pub asset_code: String,
    pub name: String,
    pub category_code: String,
    pub status: String,
    pub criticality: String,
}

/// `DependencyGraph.edges[]`
///
/// `from`／`to` 是契約的欄位名（不是 `from_asset_id`）；`from` 在 Rust 裡
/// 不是關鍵字，但為了與契約一字不差仍用 `serde(rename)` 明寫。
#[derive(Debug, Serialize)]
pub struct GraphEdgeDto {
    #[serde(rename = "from")]
    pub from_asset_id: Uuid,
    #[serde(rename = "to")]
    pub to_asset_id: Uuid,
    pub relation_type: String,
    pub impact_level: String,
}

/// `DependencyGraph`
#[derive(Debug, Serialize)]
pub struct DependencyGraphDto {
    pub nodes: Vec<GraphNodeDto>,
    pub edges: Vec<GraphEdgeDto>,
}

/// `GET /assets/{assetId}` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct GetQuery {
    pub include: Option<String>,
}

/// `GET /assets/{assetId}/dependency-graph` 的查詢參數。
///
/// 型別刻意是 `Option<i32>` 與 `Option<String>` 而非帶預設值的具體型別：
/// 界線檢查要回 422 並說明合法範圍，讓 serde 在反序列化階段就失敗
/// 只會得到一個沒有領域訊息的錯誤。
#[derive(Debug, Deserialize)]
pub struct GraphQuery {
    pub depth: Option<i32>,
    pub direction: Option<String>,
}

/// `GET /assets` 的查詢參數。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub subtree_of_node: Option<Uuid>,
    pub category_code: Option<String>,
    pub status: Option<String>,
    pub criticality: Option<String>,
    pub has_open_work_order: Option<bool>,
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
    pub fields: Option<String>,
    pub sort: Option<String>,
}

/// `AssetModel`
#[derive(Debug, Serialize)]
pub struct AssetModelDto {
    pub id: Uuid,
    pub is_platform: bool,
    pub category_code: String,
    pub manufacturer: String,
    pub model_no: String,
    pub name: String,
    pub specifications: serde_json::Value,
    pub supported_protocols: Vec<String>,
    pub expected_life_months: Option<i32>,
}

/// `GET /asset-models` 的查詢參數。契約沒有 `sort` 與 `fields`。
#[derive(Debug, Deserialize)]
pub struct ModelQuery {
    pub category_code: Option<String>,
    pub manufacturer: Option<String>,
    pub scope: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `POST /assets/{assetId}/meters/{meterCode}/readings` 的請求。
#[derive(Debug, Deserialize)]
pub struct ReadingWrite {
    pub value: Option<f64>,
    pub reading_at: Option<chrono::DateTime<chrono::Utc>>,
    pub source: Option<String>,
}

/// 讀數登錄的回應。
#[derive(Debug, Serialize)]
pub struct ReadingResultDto {
    pub meter_code: String,
    pub last_value: f64,
    /// 因為這筆讀數而到達門檻的計量型保養計畫。
    /// **不是**已產生的工單 —— 產單是 PM 產生器的職責。
    pub triggered_maintenance_plan_ids: Vec<Uuid>,
}
