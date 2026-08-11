//! Assets 補完六支：分類樹、型錄寫入、相容性、維修履歷、狀態歷程、讀數時序。
//!
//! # 三支是「表在、沒有人讀」
//!
//! `asset_status_history`（0 列 0 寫入者 0 讀者）、`asset_meter_readings`
//! （有寫入者但沒有讀取端點）、`attribute_definitions`（0 列 0 讀者，
//! 這一輪不做 —— 見下）。
//!
//! `asset_status_history` 的寫入者由 migration 064 補上（觸發器）。少了它，
//! `GET /assets/{id}/status-history` 會是一支永遠回空清單的端點 ——
//! 而它看起來會像「這台設備從來沒有故障過」。
//!
//! # `compatibility` 檢查的是「宣告對不對得上現實」
//!
//! `asset_models.spare_part_codes` 與 `supported_protocols` 都是**無外鍵的
//! 字串陣列**。所以「這台機器用 XYZ 濾網」可以安靜地指向不存在的料件，
//! 而技師是在要叫料的時候才發現。
//!
//! 量過示範資料：4 個型號裡 **2 個宣告了不存在的備品代碼**
//! （`DPH-100K` 與 `SP4K-15C` 各宣告 2 個、只有 1 個對得上）。
//!
//! 所以這支端點不是「查一張相容性表」（沒有那張表），而是**把宣告拿去對現實**。
//! 那與 `alarm_rules` 的 `notify_role_codes`、`maintenance_templates` 的
//! `required_skill_codes` 是同一個問題的第三次出現。
//!
//! # `attribute_definitions` 刻意不做
//!
//! 它是「動態欄位定義」，0 列 0 讀者，而 `assets.attributes` 是自由 jsonb。
//! 做 GET/POST 兩支端點但**沒有任何地方拿定義去驗 `attributes`**，
//! 等於再造一次「宣告了沒有人讀」—— 那需要先決定誰驗、在哪一層驗。

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

// -----------------------------------------------------------------------------
// GET /asset-categories
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CategoryDto {
    pub id: Uuid,
    /// null = 平台分類（所有租戶共用）。
    pub tenant_id: Option<Uuid>,
    pub parent_id: Option<Uuid>,
    pub code: String,
    pub name: String,
    /// ltree 路徑。前端要畫樹的話這比逐層查父節點便宜得多。
    pub category_path: String,
    /// 由 `category_path` 算出來的深度（根 = 0）。
    pub depth: i32,
    pub domain: String,
    pub default_criticality: String,
    pub is_active: bool,
    /// 這個分類（含子分類）底下有幾台設備。0 代表建了沒人用。
    pub asset_count: i64,
}

#[derive(Debug, Deserialize)]
pub struct CategoryQuery {
    pub domain: Option<String>,
    pub is_active: Option<bool>,
    /// 只回這個節點的子樹（含自己）。用 ltree 的 `<@`。
    pub subtree_of: Option<Uuid>,
}

/// `GET /asset-categories`
///
/// 回扁平清單 + `category_path`／`depth`，而不是嵌套的 JSON 樹。
///
/// 理由與 `/facilities/{id}/spatial-nodes` 的 `view=flat|tree` 同一個：
/// 扁平加路徑讓前端自己決定要畫樹還是清單，而嵌套結構在分頁與過濾之後
/// 會殘缺（父節點被過濾掉時子節點就沒有掛的地方）。
///
/// `asset_count` 用 ltree 的 `<@` 算**含子分類**的數量 —— 「空調」底下的設備
/// 應該計入「空調」，否則中間層永遠是 0 而看起來像沒人用。
pub async fn list_categories(
    State(state): State<AssetState>,
    caller: Caller,
    Query(q): Query<CategoryQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 分類沒有場域維度（它是一份目錄），所以範圍用 None。
    require_permission(&mut tx, "asset:read", None, None).await?;

    let rows: Vec<CategoryDto> = sqlx::query_as(
        "SELECT c.id, c.tenant_id, c.parent_id,
                c.code::text AS code, c.name::text AS name,
                c.category_path::text AS category_path,
                (nlevel(c.category_path) - 1)::int AS depth,
                c.domain, c.default_criticality, c.is_active,
                -- 含子分類：中間層若只算自己會永遠是 0，看起來像沒人用。
                (SELECT count(*) FROM fms.assets a
                   JOIN fms.asset_categories c2 ON c2.id = a.category_id
                  WHERE c2.category_path <@ c.category_path) AS asset_count
           FROM fms.asset_categories c
          WHERE ($1::text IS NULL OR c.domain = upper($1::text))
            AND ($2::bool IS NULL OR c.is_active = $2::bool)
            AND ($3::uuid IS NULL OR c.category_path <@
                 (SELECT p.category_path FROM fms.asset_categories p WHERE p.id = $3::uuid))
          ORDER BY c.category_path",
    )
    .bind(q.domain.as_deref())
    .bind(q.is_active)
    .bind(q.subtree_of)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

// -----------------------------------------------------------------------------
// POST /asset-models
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ModelCreate {
    pub category_id: Option<Uuid>,
    pub manufacturer: Option<String>,
    pub model_no: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub specifications: Option<serde_json::Value>,
    pub supported_protocols: Option<Vec<String>>,
    pub power_rating_w: Option<i32>,
    pub expected_life_months: Option<i32>,
    pub mtbf_hours: Option<i32>,
    pub spare_part_codes: Option<Vec<String>>,
    pub documentation_urls: Option<serde_json::Value>,
    pub is_active: Option<bool>,
}

const PROTOCOLS: [&str; 6] = ["MQTT", "HTTP", "MODBUS_TCP", "BACNET_IP", "OPC_UA", "SNMP"];

/// `POST /asset-models`
///
/// # `spare_part_codes` 會被驗證存在
///
/// 那一欄是無外鍵的字串陣列，而它的用途是「這台機器要叫哪些料」。
/// 指向不存在的料件代碼等於沒有那個宣告 —— 而技師是在要叫料時才發現。
///
/// 量過示範資料：4 個型號裡 2 個有這個問題。所以這裡在建立時就擋掉，
/// 與 `alarm_rules` 的 `notify_role_codes`、`maintenance_templates` 的
/// `required_skill_codes` 同一個判斷。
///
/// **既有的那 2 筆不動** —— 它們是 seed 的資料，而 `GET /{id}/compatibility`
/// 正是用來把它們找出來的。在這裡回溯驗證會讓那支端點沒有東西可報。
pub async fn create_model(
    State(state): State<AssetState>,
    caller: Caller,
    Json(body): Json<ModelCreate>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let category_id = body
        .category_id
        .ok_or_else(|| Problem::validation("category_id 為必填"))?;
    let manufacturer = required(&body.manufacturer, "manufacturer")?;
    let model_no = required(&body.model_no, "model_no")?;
    let name = required(&body.name, "name")?;

    if let Some(ps) = body.supported_protocols.as_deref() {
        for p in ps {
            if !PROTOCOLS.contains(&p.to_uppercase().as_str()) {
                return Err(Problem::validation(format!(
                    "supported_protocols 的「{p}」不認得，必須是 {} 其中之一",
                    PROTOCOLS.join("／")
                )));
            }
        }
    }
    for (v, field) in [
        (body.power_rating_w, "power_rating_w"),
        (body.expected_life_months, "expected_life_months"),
        (body.mtbf_hours, "mtbf_hours"),
    ] {
        if let Some(n) = v {
            if n <= 0 {
                return Err(Problem::validation(format!("{field} 必須大於 0")));
            }
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "asset_model:write", None, None).await?;

    // 指向不存在料件的備品清單，等於沒有那個清單。
    if let Some(codes) = body.spare_part_codes.as_deref() {
        let unknown: Vec<String> = sqlx::query_scalar(
            "SELECT c FROM unnest($1::text[]) AS c
              WHERE NOT EXISTS (SELECT 1 FROM fms.parts p
                                 WHERE upper(p.part_code) = upper(c))",
        )
        .bind(codes)
        .fetch_all(tx.conn())
        .await?;
        if !unknown.is_empty() {
            return Err(Problem::validation(format!(
                "spare_part_codes 裡這些料件不存在：{} —— \
                 留著它們的話，技師會在要叫料的時候才發現",
                unknown.join("、")
            )));
        }
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.asset_models
           (tenant_id, category_id, manufacturer, model_no, name, description,
            specifications, supported_protocols, power_rating_w,
            expected_life_months, mtbf_hours, spare_part_codes,
            documentation_urls, is_active)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5,
                 coalesce($6, '{}'::jsonb),
                 coalesce($7::text[], '{}'::text[]),
                 $8, $9, $10,
                 coalesce($11::text[], '{}'::text[]),
                 coalesce($12, '{}'::jsonb),
                 coalesce($13, true))
         RETURNING id",
    )
    .bind(category_id)
    .bind(manufacturer)
    .bind(model_no)
    .bind(name)
    .bind(body.description.as_deref())
    .bind(body.specifications.as_ref())
    .bind(
        body.supported_protocols
            .as_ref()
            .map(|v| v.iter().map(|s| s.to_uppercase()).collect::<Vec<_>>()),
    )
    .bind(body.power_rating_w)
    .bind(body.expected_life_months)
    .bind(body.mtbf_hours)
    .bind(body.spare_part_codes.as_deref())
    .bind(body.documentation_urls.as_ref())
    .bind(body.is_active)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

// -----------------------------------------------------------------------------
// GET /asset-models/{modelId}/compatibility
// -----------------------------------------------------------------------------

/// `GET /asset-models/{modelId}/compatibility`
///
/// **沒有相容性表。** 這支端點做的是把型錄上的兩個字串陣列拿去對現實：
///
///   * `spare_part_codes` → `parts.part_code` 存不存在
///   * `supported_protocols` → 這個租戶的閘道實際講哪些協定
///
/// 兩者都是無外鍵的宣告，所以都可能指向不存在的東西。
/// 量過示範資料：4 個型號裡 2 個的備品代碼對不上。
pub async fn model_compatibility(
    State(state): State<AssetState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "asset_model:read", None, None).await?;

    let model: Option<(String, String, Vec<String>, Vec<String>)> = sqlx::query_as(
        "SELECT model_no::text, name::text, spare_part_codes, supported_protocols
           FROM fms.asset_models WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(tx.conn())
    .await?;
    let (model_no, name, spare_codes, protocols) =
        model.ok_or_else(|| Problem::not_found("找不到這個型號（或它不在你的範圍內）"))?;

    // 備品代碼：分成對得上與對不上。回兩份清單而不是一個數字 ——
    // 「哪一個對不上」才是可行動的資訊。
    let parts: Vec<(String, bool, Option<Uuid>)> = sqlx::query_as(
        "SELECT c, EXISTS (SELECT 1 FROM fms.parts p WHERE upper(p.part_code) = upper(c)),
                (SELECT p.id FROM fms.parts p WHERE upper(p.part_code) = upper(c) LIMIT 1)
           FROM unnest($1::text[]) AS c
          ORDER BY c",
    )
    .bind(&spare_codes)
    .fetch_all(tx.conn())
    .await?;

    // 協定：對照這個租戶的閘道實際講什麼。**空的閘道清單不代表不相容** ——
    // 那代表還沒有裝任何閘道，所以那種情況回 `gateways_configured: false`
    // 而不是把每個協定標成不支援。
    let gateway_protocols: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT protocol FROM fms.iot_gateways ORDER BY 1")
            .fetch_all(tx.conn())
            .await?;
    tx.commit().await?;

    let missing: Vec<&String> = parts
        .iter()
        .filter(|(_, ok, _)| !ok)
        .map(|(c, _, _)| c)
        .collect();
    let protocol_rows: Vec<serde_json::Value> = protocols
        .iter()
        .map(|p| {
            serde_json::json!({
                "protocol": p,
                // 沒有任何閘道時這裡是 null，不是 false —— 見上面的註解。
                "gateway_available": if gateway_protocols.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Bool(gateway_protocols.contains(p))
                },
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "model": { "id": id, "model_no": model_no, "name": name },
        "spare_parts": parts.iter().map(|(c, ok, pid)| serde_json::json!({
            "part_code": c, "exists": ok, "part_id": pid,
        })).collect::<Vec<_>>(),
        "protocols": protocol_rows,
        "meta": {
            // 這兩個數字是這支端點的重點：宣告了卻對不上的東西。
            "spare_parts_declared": spare_codes.len(),
            "spare_parts_missing": missing.len(),
            "gateways_configured": !gateway_protocols.is_empty(),
            // 兩者都對得上才算完整。null 的協定不計入不完整
            // —— 還沒裝閘道不是型號的問題。
            "complete": missing.is_empty(),
        },
    })))
}

// -----------------------------------------------------------------------------
// GET /assets/{assetId}/work-orders
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AssetWorkOrderDto {
    pub id: Uuid,
    pub wo_no: String,
    pub work_order_type: String,
    pub source: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub labor_minutes: i32,
    pub labor_cost: Option<f64>,
    pub parts_cost: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `GET /assets/{assetId}/work-orders`
///
/// 維修履歷。依 `created_at` 遞減 —— 問這個問題的人要看的是「最近修過什麼」。
///
/// 成本欄位一併回傳，因為「這台機器今年花了多少」是履歷最常被問的衍生問題，
/// 而它們已經在 `work_orders` 上（由 `recompute_costs` rollup）。
pub async fn asset_work_orders(
    State(state): State<AssetState>,
    caller: Caller,
    Path(asset_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "work_order:read", None, None).await?;

    // 先確認設備看得到 —— 少了這一步，範圍外的設備會回「空的履歷」，
    // 而那與「這台機器沒修過」無法區分。
    let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM fms.assets WHERE id = $1")
        .bind(asset_id)
        .fetch_optional(tx.conn())
        .await?;
    if exists.is_none() {
        return Err(Problem::not_found("找不到這台設備（或它不在你的範圍內）"));
    }

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "created_at".to_string(),
        desc: true,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.as_timestamp()?), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<AssetWorkOrderDto> = sqlx::query_as(
        "SELECT w.id, w.wo_no::text AS wo_no, w.work_order_type, w.source,
                w.title::text AS title, w.status, w.priority,
                w.created_at, w.completed_at, w.labor_minutes,
                w.labor_cost::float8 AS labor_cost,
                w.parts_cost::float8 AS parts_cost
           FROM fms.work_orders w
          WHERE w.asset_id = $1
            AND w.deleted_at IS NULL
            AND ($2::timestamptz IS NULL OR w.created_at >= $2::timestamptz)
            AND ($3::timestamptz IS NULL OR w.created_at < $3::timestamptz)
            AND ($4::timestamptz IS NULL
                 OR (w.created_at, w.id) < ($4::timestamptz, $5::uuid))
          ORDER BY w.created_at DESC, w.id DESC
          LIMIT $6",
    )
    .bind(asset_id)
    .bind(q.from)
    .bind(q.to)
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.created_at.to_rfc3339(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

// -----------------------------------------------------------------------------
// GET /assets/{assetId}/status-history
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct StatusChangeDto {
    pub id: i64,
    pub from_status: Option<String>,
    pub to_status: String,
    pub reason: Option<String>,
    pub work_order_id: Option<Uuid>,
    pub work_order_no: Option<String>,
    /// null = **系統改的**（計量規則、背景工作），不是缺漏。
    /// 見 migration 064 的檔頭。
    pub changed_by: Option<Uuid>,
    pub changed_by_name: Option<String>,
    pub changed_at: chrono::DateTime<chrono::Utc>,
}

/// `GET /assets/{assetId}/status-history`
///
/// **這張表在 migration 064 之前 0 列 0 寫入者。** 照契約做而不補寫入者的話，
/// 這支端點會永遠回空清單 —— 而它看起來會像「這台設備從來沒有故障過」。
///
/// `changed_by` 為 null 代表系統依規則自動改的（例如 030 的計量規則把設備
/// 降為 DEGRADED）。那不是缺漏，而是與「某個人手動改的」不同的事實。
pub async fn asset_status_history(
    State(state): State<AssetState>,
    caller: Caller,
    Path(asset_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "asset:read", None, None).await?;

    let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM fms.assets WHERE id = $1")
        .bind(asset_id)
        .fetch_optional(tx.conn())
        .await?;
    if exists.is_none() {
        return Err(Problem::not_found("找不到這台設備（或它不在你的範圍內）"));
    }

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "changed_at".to_string(),
        desc: true,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.as_timestamp()?), Some(c.bigint_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<StatusChangeDto> = sqlx::query_as(
        "SELECT h.id, h.from_status::text AS from_status, h.to_status::text AS to_status,
                h.reason::text AS reason, h.work_order_id, wo.wo_no::text AS work_order_no,
                h.changed_by, u.display_name::text AS changed_by_name, h.changed_at
           FROM fms.asset_status_history h
           LEFT JOIN fms.work_orders wo ON wo.id = h.work_order_id
           LEFT JOIN fms.users u ON u.id = h.changed_by
          WHERE h.asset_id = $1
            AND ($2::timestamptz IS NULL OR h.changed_at >= $2::timestamptz)
            AND ($3::timestamptz IS NULL OR h.changed_at < $3::timestamptz)
            AND ($4::timestamptz IS NULL
                 OR (h.changed_at, h.id) < ($4::timestamptz, $5::bigint))
          ORDER BY h.changed_at DESC, h.id DESC
          LIMIT $6",
    )
    .bind(asset_id)
    .bind(q.from)
    .bind(q.to)
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.changed_at.to_rfc3339(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

// -----------------------------------------------------------------------------
// GET /assets/{assetId}/meters/{meterCode}/readings
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ReadingDto {
    pub id: i64,
    pub reading_at: chrono::DateTime<chrono::Utc>,
    pub value: f64,
    /// MANUAL／IOT／IMPORT／ESTIMATED。`ESTIMATED` 的值不該拿去算合約用的
    /// 用量，所以來源必須回傳。
    pub source: String,
    pub recorded_by: Option<Uuid>,
    /// 與前一筆的差值。計量的意義通常在增量（用電量、運轉時數），
    /// 而讀數本身是累計值。
    pub delta: Option<f64>,
}

/// `GET /assets/{assetId}/meters/{meterCode}/readings`
///
/// `POST` 那一半早就實作了（`record_reading`），而**讀取端沒有** ——
/// 也就是說讀數寫得進去、讀不回來。與遙測那次同一個形狀。
///
/// `delta` 用視窗函式算：計量表是累計值（電度、運轉時數），
/// 而人們要的是「這段期間用了多少」。在應用層算會需要把整段序列拉回來，
/// 而分頁之後那個差值會在頁邊界斷掉。
pub async fn meter_readings(
    State(state): State<AssetState>,
    caller: Caller,
    Path((asset_id, meter_code)): Path<(Uuid, String)>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "meter:read", None, None).await?;

    // 計量表必須屬於這台設備 —— 只用 meter_code 查的話，
    // 帶著別台設備的 id 也會成功，路徑就變成謊言。
    let meter_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT m.id FROM fms.asset_meters m
          WHERE m.asset_id = $1 AND upper(m.meter_code) = upper($2)",
    )
    .bind(asset_id)
    .bind(&meter_code)
    .fetch_optional(tx.conn())
    .await?;
    let meter_id = meter_id
        .ok_or_else(|| Problem::not_found("找不到這台設備的這個計量表（或設備不在你的範圍內）"))?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "reading_at".to_string(),
        desc: true,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.as_timestamp()?), Some(c.bigint_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<ReadingDto> = sqlx::query_as(
        "SELECT r.id, r.reading_at, r.value::float8 AS value, r.source, r.recorded_by,
                -- 與**時間上**前一筆的差。視窗函式在分頁之前算，
                -- 所以頁邊界的那一筆也有值。
                (r.value - lag(r.value) OVER (ORDER BY r.reading_at))::float8 AS delta
           FROM fms.asset_meter_readings r
          WHERE r.asset_meter_id = $1
            AND ($2::timestamptz IS NULL OR r.reading_at >= $2::timestamptz)
            AND ($3::timestamptz IS NULL OR r.reading_at < $3::timestamptz)
            AND ($4::timestamptz IS NULL
                 OR (r.reading_at, r.id) < ($4::timestamptz, $5::bigint))
          ORDER BY r.reading_at DESC, r.id DESC
          LIMIT $6",
    )
    .bind(meter_id)
    .bind(q.from)
    .bind(q.to)
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.reading_at.to_rfc3339(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
        "meta": { "meter_code": meter_code, "asset_meter_id": meter_id },
    })))
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
        Some("uq_asset_models") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail("這個製造商 + 型號已經在型錄裡了"),
        Some("asset_models_category_id_fkey") => Problem::not_found("找不到這個設備分類"),
        _ => Problem::from(err),
    }
}
