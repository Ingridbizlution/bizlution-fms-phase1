//! 2.5D 樓層平面圖的設備標點（`/spatial-nodes/{id}/floor-plan-markers`、
//! `/floor-plan-markers/{id}`）。
//!
//! # 平面圖影像本身不在這裡
//!
//! 影像走既有的 `POST /attachments`（`entity_type=SPATIAL_NODE`,
//! `entity_id=<floor_node_id>`, `purpose=FLOOR_PLAN_IMAGE`）——樓層本身就是
//! 一列 `node_type_code = 'FLOOR'` 的 `spatial_nodes`（見 migration 003），
//! attachments 已經有直傳上傳／S3 儲存／presigned 下載 URL 全套機制，這裡
//! 不重造。這個模組只管「設備在那張圖上的哪個比例位置」——這才是
//! attachments 沒地方放的東西（見 migration 086 檔頭）。
//!
//! # `floor_node_id` 必須是 FLOOR 節點，在這裡驗證，不是 DB CHECK
//!
//! `spatial_node_types` 是租戶可擴充的型別目錄（003），資料庫層的靜態
//! CHECK 驗證不到它。跟 077 對 `directory_role_mappings.scope_type` 的處理
//! 同一個決定：由 handler 查一次目標節點的 `node_type_code`。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, Problem, TenantTx};

use crate::handlers::TenancyState;

const COLUMNS: &str = "m.id, m.floor_node_id, m.entity_type, m.entity_id,
                       m.x_ratio::float8 AS x_ratio, m.y_ratio::float8 AS y_ratio,
                       m.z_offset::float8 AS z_offset,
                       CASE m.entity_type
                         WHEN 'ASSET' THEN a.name
                         WHEN 'DEVICE' THEN d.name
                         WHEN 'SPATIAL_NODE' THEN n.name
                       END::text AS entity_label,
                       CASE m.entity_type
                         WHEN 'ASSET' THEN a.status
                         WHEN 'DEVICE' THEN d.status
                         WHEN 'SPATIAL_NODE' THEN n.status
                       END::text AS entity_status,
                       m.created_at";

const FROM: &str = "FROM fms.floor_plan_markers m
                    LEFT JOIN fms.assets a        ON a.id = m.entity_id AND m.entity_type = 'ASSET'
                    LEFT JOIN fms.devices d        ON d.id = m.entity_id AND m.entity_type = 'DEVICE'
                    LEFT JOIN fms.spatial_nodes n  ON n.id = m.entity_id AND m.entity_type = 'SPATIAL_NODE'";

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct MarkerDto {
    pub id: Uuid,
    pub floor_node_id: Uuid,
    pub entity_type: String,
    pub entity_id: Uuid,
    pub x_ratio: f64,
    pub y_ratio: f64,
    pub z_offset: f64,
    pub entity_label: Option<String>,
    pub entity_status: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct MarkerCreate {
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    pub x_ratio: Option<f64>,
    pub y_ratio: Option<f64>,
    pub z_offset: Option<f64>,
}

/// 查一次目標節點是不是 FLOOR，順便把 `facility_id` 帶回來做權限檢查——
/// 兩件事都得查這一列，合成一次查詢。
async fn require_floor_node(tx: &mut TenantTx, floor_node_id: Uuid) -> Result<Uuid, Problem> {
    let row = sqlx::query!(
        r#"SELECT facility_id, node_type_code::text AS "node_type_code!"
             FROM fms.spatial_nodes WHERE id = $1 AND deleted_at IS NULL"#,
        floor_node_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("找不到這個空間節點"))?;

    if row.node_type_code != "FLOOR" {
        return Err(Problem::validation(format!(
            "{floor_node_id} 不是樓層節點（node_type_code = {}），\
             平面圖標點只能掛在 FLOOR 節點上",
            row.node_type_code
        )));
    }
    Ok(row.facility_id)
}

/// 確認 `entity_id` 真的存在，訊息說得出「哪個表、哪個 id」——不是留給
/// insert 失敗時的一個不知所云的 FK 錯誤（這張表刻意沒有 FK，因為指向的表
/// 依 `entity_type` 而不同，見 migration 086 檔頭）。
async fn require_entity_exists(
    tx: &mut TenantTx,
    entity_type: &str,
    entity_id: Uuid,
) -> Result<(), Problem> {
    let exists: bool = match entity_type {
        "ASSET" => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM fms.assets WHERE id = $1 AND deleted_at IS NULL)",
        ),
        "DEVICE" => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM fms.devices WHERE id = $1 AND deleted_at IS NULL)",
        ),
        "SPATIAL_NODE" => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM fms.spatial_nodes WHERE id = $1 AND deleted_at IS NULL)",
        ),
        _ => unreachable!("entity_type 已在呼叫端驗證過"),
    }
    .bind(entity_id)
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;

    if exists {
        Ok(())
    } else {
        Err(Problem::validation(format!(
            "找不到 entity_id={entity_id}（entity_type={entity_type}，或已被刪除）"
        )))
    }
}

/// `GET /spatial-nodes/{floorNodeId}/floor-plan-markers`
pub async fn list(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(floor_node_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let facility_id = require_floor_node(&mut tx, floor_node_id).await?;
    require_permission(&mut tx, "spatial_node:read", Some(facility_id), None).await?;

    let rows: Vec<MarkerDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} {FROM} WHERE m.floor_node_id = $1 ORDER BY m.created_at"
    ))
    .bind(floor_node_id)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

/// `POST /spatial-nodes/{floorNodeId}/floor-plan-markers`
pub async fn create(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(floor_node_id): Path<Uuid>,
    Json(body): Json<MarkerCreate>,
) -> Result<(StatusCode, Json<MarkerDto>), Problem> {
    let entity_type = body
        .entity_type
        .as_deref()
        .filter(|s| matches!(*s, "ASSET" | "DEVICE" | "SPATIAL_NODE"))
        .ok_or_else(|| Problem::validation("entity_type 必須是 ASSET／DEVICE／SPATIAL_NODE"))?;
    let entity_id = body
        .entity_id
        .ok_or_else(|| Problem::validation("entity_id 為必填"))?;
    let x_ratio = body
        .x_ratio
        .filter(|v| (0.0..=1.0).contains(v))
        .ok_or_else(|| Problem::validation("x_ratio 為必填，且必須在 0 到 1 之間"))?;
    let y_ratio = body
        .y_ratio
        .filter(|v| (0.0..=1.0).contains(v))
        .ok_or_else(|| Problem::validation("y_ratio 為必填，且必須在 0 到 1 之間"))?;
    let z_offset = body.z_offset.unwrap_or(0.0);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let facility_id = require_floor_node(&mut tx, floor_node_id).await?;
    require_permission(&mut tx, "spatial_node:write", Some(facility_id), None).await?;
    require_entity_exists(&mut tx, entity_type, entity_id).await?;

    let row: MarkerDto = sqlx::query_as(&format!(
        "WITH ins AS (
           INSERT INTO fms.floor_plan_markers
             (tenant_id, floor_node_id, entity_type, entity_id, x_ratio, y_ratio, z_offset, created_by)
           VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7)
           RETURNING *)
         SELECT {COLUMNS} FROM ins m
           LEFT JOIN fms.assets a        ON a.id = m.entity_id AND m.entity_type = 'ASSET'
           LEFT JOIN fms.devices d        ON d.id = m.entity_id AND m.entity_type = 'DEVICE'
           LEFT JOIN fms.spatial_nodes n  ON n.id = m.entity_id AND m.entity_type = 'SPATIAL_NODE'"
    ))
    .bind(floor_node_id)
    .bind(entity_type)
    .bind(entity_id)
    .bind(x_ratio)
    .bind(y_ratio)
    .bind(z_offset)
    .bind(caller.user_id)
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `DELETE /floor-plan-markers/{id}`
pub async fn delete(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let floor_node_id: Uuid =
        sqlx::query_scalar("SELECT floor_node_id FROM fms.floor_plan_markers WHERE id = $1")
            .bind(id)
            .fetch_optional(tx.conn())
            .await?
            .ok_or_else(|| Problem::not_found("找不到這個標點"))?;

    let facility_id = require_floor_node(&mut tx, floor_node_id).await?;
    require_permission(&mut tx, "spatial_node:write", Some(facility_id), None).await?;

    sqlx::query("DELETE FROM fms.floor_plan_markers WHERE id = $1")
        .bind(id)
        .execute(tx.conn())
        .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "deleted": id })))
}
