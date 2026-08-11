//! 裝置與通訊點（`/devices`）。
//!
//! # 在這幾支之前，裝置只能由 seed 建立
//!
//! `fms.devices` 與 `fms.telemetry_points` 從 006 就存在，而**唯一的寫入者是
//! seed 009**。`POST /telemetry:batch-ingest` 靠 `device_code + point_code`
//! 找點位 —— 也就是說一台新裝置上線之前，得有人手寫 SQL 把它建進去。
//!
//! # `status` 這一欄會說謊，所以連線狀態是算出來的
//!
//! `ingest_telemetry()` 會把 `last_seen_at` 往前推，並把 `OFFLINE`／`UNKNOWN`
//! 翻成 `ONLINE`。**但沒有任何東西把它翻回 `OFFLINE`** —— 那需要一個掃描型的
//! `DEVICE_OFFLINE` 規則，而那是 057 明確標記為未做的東西。
//!
//! 結果是：一台裝置在第一筆讀數之後，`status` 永遠是 `ONLINE`，
//! 即使它上個月就死了。ENDPOINTS.md 寫這支端點回傳「連線狀態」——
//! 照那一欄回答就是說謊。
//!
//! 所以 `connectivity` 由 `last_seen_at` 與**資料庫的現在**算出來，
//! 與 `skills` 模組的 `status` 同一個判斷：那種沒有人會主動更新的事實
//! 不能存成欄位。
//!
//! # 門檻用 `offline_alarm_after_seconds`，不自己發明倍數
//!
//! 「多久沒回報算離線」是每台裝置不同的：溫度感測器一分鐘一筆，
//! 電錶可能一小時一筆。006 已經有 `offline_alarm_after_seconds`
//! （每台裝置一個值，預設 900），而它在此之前**零讀者**。
//!
//! 用「心跳間隔 × 2」之類的倍數就是在管理者已經設好的值旁邊
//! 再擺一個能蓋掉它的東西。
//!
//! 006 為這件事建的 `idx_devices_stale`（部分索引，
//! `last_seen_at WHERE status <> 'DISABLED' AND deleted_at IS NULL`）
//! 在此之前也沒有讀者 —— `offline_only=true` 是它的第一個。
//!
//! `DISABLED` 與 `MAINTENANCE` 直接透傳：那兩個是行政狀態
//! （「這台我們故意關掉」），不該被「它沒在回報」蓋掉 ——
//! 反而正是因為關掉了才沒回報。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_permission, Caller, Cursor, PageMeta, Problem,
    SortSpec,
};

use crate::handlers::AssetState;

const DEVICE_TYPES: [&str; 8] = [
    "SENSOR",
    "METER",
    "CONTROLLER",
    "ACCESS_PANEL",
    "CAMERA",
    "OCCUPANCY",
    "ENVIRONMENT",
    "GATEWAY",
];
const DEVICE_STATUSES: [&str; 6] = [
    "ONLINE",
    "OFFLINE",
    "FAULT",
    "MAINTENANCE",
    "UNKNOWN",
    "DISABLED",
];

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct DeviceDto {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub facility_name: String,
    pub gateway_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub asset_code: Option<String>,
    pub spatial_node_id: Option<Uuid>,
    pub location_name: Option<String>,
    pub device_code: String,
    pub name: String,
    pub device_type: String,
    pub address: Option<String>,
    pub heartbeat_interval_seconds: i32,
    /// 管理者設定的離線門檻。放進回應是因為 `connectivity` 是用它算的 ——
    /// 少了它，前端無法解釋「為什麼這台算離線」。
    pub offline_alarm_after_seconds: i32,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    /// **儲存的**狀態。見模組檔頭：第一筆讀數之後它不會再變回 OFFLINE，
    /// 所以它是「最後已知的行政狀態」而不是現在的連線狀態。
    pub status: String,
    /// **算出來的**連線狀態：`ONLINE`／`OFFLINE`／`NEVER_SEEN`，
    /// 或行政狀態（`DISABLED`／`MAINTENANCE`）透傳。
    pub connectivity: String,
    /// 距上次回報幾秒。`last_seen_at` 為 NULL 時是 NULL ——
    /// 「從未回報」與「剛剛回報」不能混成同一個數字。
    pub seconds_since_seen: Option<i64>,
    pub point_count: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PointDto {
    pub id: Uuid,
    pub point_code: String,
    pub name: String,
    pub data_type: String,
    pub unit: Option<String>,
    pub scale_factor: f64,
    pub offset_value: f64,
    pub valid_min: Option<f64>,
    pub valid_max: Option<f64>,
    /// 這個點位是否餵給某個設備計量 —— 有值代表計量型 PM 會跟著這個點位走。
    pub asset_meter_id: Option<Uuid>,
    pub is_active: bool,
    /// 最新值與時刻（來自 `telemetry_latest`）。沒有讀數過的點位是 NULL。
    pub last_observed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_value_num: Option<f64>,
    pub last_value_bool: Option<bool>,
    pub last_value_text: Option<String>,
    pub last_quality: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub device_type: Option<String>,
    /// 逗號分隔。過濾的是**儲存的** `status`，不是算出來的 `connectivity`
    /// —— 要問後者用 `offline_only`。
    pub status: Option<String>,
    pub gateway_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    /// 只回傳算出來是離線的（含 `NEVER_SEEN`）。這是「哪些裝置沒在回報」
    /// 那個問題的唯一正確問法 —— 用 `status=OFFLINE` 問會得到空清單，
    /// 因為沒有人把 `status` 寫回 OFFLINE。
    pub offline_only: Option<bool>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeviceCreate {
    pub facility_id: Option<Uuid>,
    pub device_code: Option<String>,
    pub name: Option<String>,
    pub device_type: Option<String>,
    pub gateway_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub address: Option<String>,
    pub heartbeat_interval_seconds: Option<i32>,
    pub offline_alarm_after_seconds: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct DeviceUpdate {
    pub name: Option<String>,
    pub device_type: Option<String>,
    pub status: Option<String>,
    pub gateway_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub spatial_node_id: Option<Uuid>,
    pub address: Option<String>,
    pub heartbeat_interval_seconds: Option<i32>,
    pub offline_alarm_after_seconds: Option<i32>,
}

/// `connectivity` 與 `seconds_since_seen` 的 SQL。
///
/// `connectivity` 呼叫 `fms.device_connectivity()`（migration 081）而不是
/// 自己重算：那支函式現在是唯一的真實來源，`floor-view` 也呼叫它——
/// 三個地方（這裡的 SELECT、下面 `list()` 的 `offline_only` 篩選、
/// floor-view）曾經各自抄一份判定式，081 把它們收斂成一個。
///
/// `seconds_since_seen` 沒有收斂的必要：只有這一個消費者，且邏輯是
/// 一行的 epoch 減法，寫在 SQL 而不是 Rust的理由不變——基準必須是
/// **資料庫的現在**，應用伺服器與資料庫時鐘不同步時，Rust 端算出來的
/// 「幾秒前」會漂移，而那種偏差只在部署環境才出現，本機測不到。
const CONNECTIVITY: &str = "
  fms.device_connectivity(d.status, d.last_seen_at, d.offline_alarm_after_seconds)
    AS connectivity,
  CASE WHEN d.last_seen_at IS NULL THEN NULL
       ELSE floor(extract(epoch FROM now() - d.last_seen_at))::bigint
  END AS seconds_since_seen";

const COLUMNS: &str = "d.id, d.facility_id, f.name::text AS facility_name,
                       d.gateway_id, d.asset_id, a.asset_code::text AS asset_code,
                       d.spatial_node_id, sn.name::text AS location_name,
                       d.device_code::text AS device_code, d.name::text AS name,
                       d.device_type, d.address::text AS address,
                       d.heartbeat_interval_seconds, d.offline_alarm_after_seconds,
                       d.last_seen_at, d.status,
                       (SELECT count(*) FROM fms.telemetry_points p
                         WHERE p.device_id = d.id) AS point_count,
                       d.created_at";

const FROM: &str = "FROM fms.devices d
                    JOIN fms.facilities f ON f.id = d.facility_id
                    LEFT JOIN fms.assets a ON a.id = d.asset_id
                    LEFT JOIN fms.spatial_nodes sn ON sn.id = d.spatial_node_id";

/// `GET /devices`
pub async fn list(
    State(state): State<AssetState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    if let Some(t) = q.device_type.as_deref() {
        if !DEVICE_TYPES.contains(&t.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "device_type 必須是 {} 其中之一",
                DEVICE_TYPES.join("／")
            )));
        }
    }
    let statuses = parse_statuses(q.status.as_deref())?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "device:read", q.facility_id, None).await?;

    let limit = clamp_limit(q.limit);
    // 依 device_code 排序而不是時間：裝置清單是一份**盤點**，
    // 人們照編號找它，而不是問「最近註冊了什麼」。
    let sort = SortSpec {
        column: "device_code".to_string(),
        desc: false,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.key.clone()), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<DeviceDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS}, {CONNECTIVITY} {FROM}
          WHERE d.deleted_at IS NULL
            AND ($1::uuid IS NULL OR d.facility_id = $1::uuid)
            AND ($2::text IS NULL OR d.device_type = upper($2::text))
            AND ($3::text[] IS NULL OR d.status = ANY($3::text[]))
            AND ($4::uuid IS NULL OR d.gateway_id = $4::uuid)
            AND ($5::uuid IS NULL OR d.asset_id = $5::uuid)
            AND (NOT $6::bool
                 OR fms.device_connectivity(d.status, d.last_seen_at, d.offline_alarm_after_seconds)
                    IN ('OFFLINE', 'NEVER_SEEN'))
            AND ($7::text IS NULL OR (d.device_code, d.id) > ($7::text, $8::uuid))
          ORDER BY d.device_code, d.id
          LIMIT $9"
    ))
    .bind(q.facility_id)
    .bind(q.device_type.as_deref())
    .bind(statuses.as_deref())
    .bind(q.gateway_id)
    .bind(q.asset_id)
    .bind(q.offline_only.unwrap_or(false))
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.device_code.clone(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

/// `POST /devices`
pub async fn create(
    State(state): State<AssetState>,
    caller: Caller,
    Json(body): Json<DeviceCreate>,
) -> Result<(StatusCode, Json<DeviceDto>), Problem> {
    let facility_id = body
        .facility_id
        .ok_or_else(|| Problem::validation("facility_id 為必填"))?;
    let code = required(&body.device_code, "device_code")?;
    let name = required(&body.name, "name")?;
    let device_type = required(&body.device_type, "device_type")?.to_uppercase();
    if !DEVICE_TYPES.contains(&device_type.as_str()) {
        return Err(Problem::validation(format!(
            "device_type 必須是 {} 其中之一",
            DEVICE_TYPES.join("／")
        )));
    }
    // 006 的 `ck_device_target`：至少要綁一個目標。在這裡先擋是為了給出
    // **說得出理由**的訊息 —— 讓 CHECK 擋會變成 500，而且訊息是約束名稱。
    if body.asset_id.is_none() && body.spatial_node_id.is_none() {
        return Err(Problem::validation(
            "asset_id 與 spatial_node_id 至少要有一個 —— \
             一台不知道在監測什麼的裝置，它的讀數無法歸屬到任何地方",
        ));
    }
    check_seconds(
        body.heartbeat_interval_seconds,
        "heartbeat_interval_seconds",
    )?;
    check_seconds(
        body.offline_alarm_after_seconds,
        "offline_alarm_after_seconds",
    )?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "device:write", Some(facility_id), None).await?;

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.devices
           (tenant_id, facility_id, gateway_id, asset_id, spatial_node_id,
            device_code, name, device_type, address,
            heartbeat_interval_seconds, offline_alarm_after_seconds)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7, $8,
                 coalesce($9, 300), coalesce($10, 900))
         RETURNING id",
    )
    .bind(facility_id)
    .bind(body.gateway_id)
    .bind(body.asset_id)
    .bind(body.spatial_node_id)
    .bind(code)
    .bind(name)
    .bind(&device_type)
    .bind(body.address.as_deref())
    .bind(body.heartbeat_interval_seconds)
    .bind(body.offline_alarm_after_seconds)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;

    let row = fetch_one(&mut tx, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `PATCH /devices/{deviceId}`
pub async fn update(
    State(state): State<AssetState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<DeviceUpdate>,
) -> Result<Json<DeviceDto>, Problem> {
    if let Some(t) = body.device_type.as_deref() {
        if !DEVICE_TYPES.contains(&t.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "device_type 必須是 {} 其中之一",
                DEVICE_TYPES.join("／")
            )));
        }
    }
    if let Some(s) = body.status.as_deref() {
        if !DEVICE_STATUSES.contains(&s.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "status 必須是 {} 其中之一",
                DEVICE_STATUSES.join("／")
            )));
        }
    }
    check_seconds(
        body.heartbeat_interval_seconds,
        "heartbeat_interval_seconds",
    )?;
    check_seconds(
        body.offline_alarm_after_seconds,
        "offline_alarm_after_seconds",
    )?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 範圍用 None：裝置搬場域不是這支端點做的事（`facility_id` 不可改），
    // 而目標裝置的場域由 RLS 決定 —— 不在範圍內就是 404。
    require_permission(&mut tx, "device:write", None, None).await?;

    // `coalesce` 逐欄套用：PATCH 的語意是「沒給的不動」。
    //
    // 這也是為什麼 `asset_id`／`spatial_node_id` 沒有辦法透過這支端點清成
    // NULL —— `coalesce(NULL, 舊值)` 會保留舊值。那是刻意的：
    // `ck_device_target` 要求至少留一個，而「清掉哪一個」需要兩欄一起判斷。
    // 要解綁就換綁到另一個目標。
    let updated: Option<Uuid> = sqlx::query_scalar(
        "UPDATE fms.devices SET
            name = coalesce($2, name),
            device_type = coalesce(upper($3), device_type),
            status = coalesce(upper($4), status),
            gateway_id = coalesce($5, gateway_id),
            asset_id = coalesce($6, asset_id),
            spatial_node_id = coalesce($7, spatial_node_id),
            address = coalesce($8, address),
            heartbeat_interval_seconds = coalesce($9, heartbeat_interval_seconds),
            offline_alarm_after_seconds = coalesce($10, offline_alarm_after_seconds),
            updated_at = clock_timestamp()
          WHERE id = $1 AND deleted_at IS NULL
          RETURNING id",
    )
    .bind(id)
    .bind(body.name.as_deref())
    .bind(body.device_type.as_deref())
    .bind(body.status.as_deref())
    .bind(body.gateway_id)
    .bind(body.asset_id)
    .bind(body.spatial_node_id)
    .bind(body.address.as_deref())
    .bind(body.heartbeat_interval_seconds)
    .bind(body.offline_alarm_after_seconds)
    .fetch_optional(tx.conn())
    .await
    .map_err(translate)?;

    if updated.is_none() {
        return Err(Problem::not_found("找不到這台裝置（或它不在你的範圍內）"));
    }

    let row = fetch_one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(row))
}

/// `GET /devices/{deviceId}/points`
pub async fn points(
    State(state): State<AssetState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "device:read", None, None).await?;

    // 先確認裝置看得到 —— 少了這一步，一台看不到的裝置會回「空的點位清單」，
    // 而那與「這台裝置沒有點位」無法區分。
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM fms.devices WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(tx.conn())
            .await?;
    if exists.is_none() {
        return Err(Problem::not_found("找不到這台裝置（或它不在你的範圍內）"));
    }

    let rows: Vec<PointDto> = sqlx::query_as(
        "SELECT p.id, p.point_code::text AS point_code, p.name::text AS name,
                p.data_type, p.unit::text AS unit,
                p.scale_factor::float8 AS scale_factor,
                p.offset_value::float8 AS offset_value,
                p.valid_min::float8 AS valid_min, p.valid_max::float8 AS valid_max,
                p.asset_meter_id, p.is_active,
                l.observed_at AS last_observed_at,
                l.value_num::float8 AS last_value_num,
                l.value_bool AS last_value_bool,
                l.value_text::text AS last_value_text,
                l.quality::text AS last_quality
           FROM fms.telemetry_points p
           LEFT JOIN fms.telemetry_latest l ON l.telemetry_point_id = p.id
          WHERE p.device_id = $1
          ORDER BY p.point_code",
    )
    .bind(id)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

async fn fetch_one(tx: &mut fms_shared::TenantTx, id: Uuid) -> Result<DeviceDto, Problem> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS}, {CONNECTIVITY} {FROM} WHERE d.id = $1"
    ))
    .bind(id)
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

fn parse_statuses(raw: Option<&str>) -> Result<Option<Vec<String>>, Problem> {
    let Some(s) = raw else { return Ok(None) };
    let v: Vec<String> = s
        .split(',')
        .map(|x| x.trim().to_uppercase())
        .filter(|x| !x.is_empty())
        .collect();
    for s in &v {
        if !DEVICE_STATUSES.contains(&s.as_str()) {
            return Err(Problem::validation(format!(
                "status 必須是 {} 其中之一（可逗號分隔）",
                DEVICE_STATUSES.join("／")
            )));
        }
    }
    Ok(Some(v))
}

fn check_seconds(v: Option<i32>, field: &str) -> Result<(), Problem> {
    if let Some(n) = v {
        // 上限一天：比這更長的「離線門檻」等於關掉離線判定，
        // 而那件事該用 status = DISABLED 表達，不是用一個很大的數字。
        if !(1..=86_400).contains(&n) {
            return Err(Problem::validation(format!(
                "{field} 必須是 1 到 86400 秒（一天）"
            )));
        }
    }
    Ok(())
}

fn required<'a>(v: &'a Option<String>, field: &str) -> Result<&'a str, Problem> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation(format!("{field} 為必填")))
}

fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        // 索引是 `(tenant_id, lower(device_code)) WHERE deleted_at IS NULL`
        // —— 不分大小寫，而且軟刪除的裝置不佔用編號。
        Some("uq_devices_code") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail("這個 device_code 已被使用（不分大小寫）"),
        Some("ck_device_target") => Problem::validation(
            "asset_id 與 spatial_node_id 至少要有一個 —— \
             一台不知道在監測什麼的裝置，它的讀數無法歸屬到任何地方",
        ),
        Some("devices_facility_id_fkey") => {
            Problem::not_found("找不到這個場域（或它不在你的範圍內）")
        }
        Some("devices_asset_id_fkey") => Problem::not_found("找不到這個設備"),
        Some("devices_spatial_node_id_fkey") => Problem::not_found("找不到這個空間節點"),
        Some("devices_gateway_id_fkey") => Problem::not_found("找不到這個閘道"),
        _ => Problem::from(err),
    }
}
