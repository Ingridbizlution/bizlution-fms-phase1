//! 動態欄位定義（`/attribute-definitions`）與設備依賴關係（`/assets/{id}/relations`）。
//!
//! # 動態欄位：**API 層驗證，不用資料庫觸發器**
//!
//! 這是明確的設計決定。`attribute_definitions.validation_schema` 是一份
//! JSON Schema，而 `assets.attributes` 的寫入路徑在 handler 裡拿它驗。
//!
//! 為什麼不用 CHECK 或觸發器：schema 可以隨時被管理者改，而資料庫層的約束
//! 只能驗「當下」那一版 —— 一次設定變更會讓**歷史資料變成無法儲存的東西**
//! （任何 UPDATE 都會撞上新約束，即使那次更新與 attributes 無關）。
//!
//! 代價要說清楚：**既有的 `attributes` 不會被回溯驗證**。要找出不符合現行
//! 定義的舊資料需要另一支稽核端點，那不在這一輪。這與 `asset_models` 的
//! `spare_part_codes` 是同一個判斷 —— 新的擋住，舊的留著並且看得見。
//!
//! 驗證器沿用 `fms_shared::form_schema`，那支已經有兩個消費者
//! （SERVICE 工單的 payload、預約的附加服務）。**不新寫一套** ——
//! 它的檔頭寫著「兩份驗證遲早會出現同樣的 payload 在一處被接受、在另一處
//! 被拒」，而那個理由對第三個消費者一樣成立。
//!
//! # 依賴關係：**建立前做循環偵測**
//!
//! `/assets/{id}/dependency-graph` 已經在讀 `asset_relations`。若允許建出
//! 循環，那支圖會無限展開。
//!
//! `ck_asset_relations_distinct` 擋掉了自我參照（A → A），但擋不掉
//! A → B → A。所以這裡在寫入前用遞迴 CTE 檢查：**新關係的終點能不能走回
//! 起點**。走得回去就是循環，拒絕。
//!
//! 深度上限（[`MAX_DEPENDENCY_DEPTH`]）同時是兩件事的界線：遞迴 CTE 的
//! 停止條件，與「這條鏈太深了」的錯誤訊息。兩者共用一個常數 ——
//! 分開寫會讓錯誤訊息說一個數字而實際擋在另一個。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, Problem};

use crate::handlers::AssetState;

/// 依賴鏈的深度上限。
///
/// 這個數字同時是遞迴 CTE 的停止條件與錯誤訊息裡的那個數字 ——
/// 分開寫會讓訊息說一個值而實際擋在另一個。
///
/// 32 是這樣來的：真實的設施依賴鏈是「市電 → 主配電 → 分配電 → UPS →
/// 機櫃 → 設備」那種量級（個位數）。32 遠高於任何合理的鏈，所以撞到它
/// 幾乎一定表示資料有問題，而不是鏈真的那麼長。
pub const MAX_DEPENDENCY_DEPTH: i32 = 32;

const RELATION_TYPES: [&str; 5] = [
    "DEPENDS_ON",
    "FEEDS",
    "BACKUP_OF",
    "CONTROLS",
    "REDUNDANT_WITH",
];
const IMPACT_LEVELS: [&str; 4] = ["LOW", "MEDIUM", "HIGH", "CRITICAL"];
const TARGET_ENTITIES: [&str; 4] = ["FACILITY", "SPATIAL_NODE", "ASSET", "ASSET_MODEL"];
const ATTR_DATA_TYPES: [&str; 7] = [
    "STRING", "TEXT", "NUMBER", "INTEGER", "BOOLEAN", "DATE", "ENUM",
];

// -----------------------------------------------------------------------------
// GET /attribute-definitions
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AttributeDefinitionDto {
    pub id: Uuid,
    /// null = 平台定義（所有租戶共用）。
    pub tenant_id: Option<Uuid>,
    pub target_entity: String,
    /// 縮小到某個型別（例如只有某個設備分類才有這一欄）。
    pub applies_to_type: Option<String>,
    pub attribute_key: String,
    pub label: String,
    pub data_type: String,
    pub is_required: bool,
    pub is_searchable: bool,
    pub default_value: Option<serde_json::Value>,
    /// JSON Schema。`POST/PATCH /assets` 拿它驗 `attributes` ——
    /// **在 API 層，不是資料庫觸發器**（見模組檔頭）。
    pub validation_schema: serde_json::Value,
    pub ui_hints: serde_json::Value,
    pub display_order: i32,
    pub is_active: bool,
}

#[derive(Debug, Deserialize)]
pub struct DefinitionQuery {
    pub target_entity: Option<String>,
    pub applies_to_type: Option<String>,
    pub is_active: Option<bool>,
}

/// `GET /attribute-definitions`
///
/// 前端動態表單用。依 `display_order` 排序 —— 那一欄存在就是為了讓管理者
/// 決定欄位在表單上的順序，忽略它會讓那個設定沒有效果。
pub async fn list_definitions(
    State(state): State<AssetState>,
    caller: Caller,
    Query(q): Query<DefinitionQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    if let Some(e) = q.target_entity.as_deref() {
        if !TARGET_ENTITIES.contains(&e.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "target_entity 必須是 {} 其中之一",
                TARGET_ENTITIES.join("／")
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "asset:read", None, None).await?;

    let rows: Vec<AttributeDefinitionDto> = sqlx::query_as(
        "SELECT id, tenant_id, target_entity,
                applies_to_type::text AS applies_to_type,
                attribute_key::text AS attribute_key, label::text AS label,
                data_type, is_required, is_searchable, default_value,
                validation_schema, ui_hints, display_order, is_active
           FROM fms.attribute_definitions
          WHERE ($1::text IS NULL OR target_entity = upper($1::text))
            AND ($2::text IS NULL OR applies_to_type = $2::text)
            AND ($3::bool IS NULL OR is_active = $3::bool)
          ORDER BY target_entity, display_order, attribute_key",
    )
    .bind(q.target_entity.as_deref())
    .bind(q.applies_to_type.as_deref())
    .bind(q.is_active)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "items": rows,
        "meta": {
            // 說出驗證發生在哪一層。前端需要知道「送錯會在哪裡被擋」，
            // 而運維需要知道改了定義之後舊資料會不會壞（不會）。
            "validated_at": "api",
            "existing_values_revalidated": false,
        },
    })))
}

// -----------------------------------------------------------------------------
// POST /attribute-definitions
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DefinitionCreate {
    pub target_entity: Option<String>,
    pub applies_to_type: Option<String>,
    pub attribute_key: Option<String>,
    pub label: Option<String>,
    pub data_type: Option<String>,
    pub is_required: Option<bool>,
    pub is_searchable: Option<bool>,
    pub default_value: Option<serde_json::Value>,
    pub validation_schema: Option<serde_json::Value>,
    pub ui_hints: Option<serde_json::Value>,
    pub display_order: Option<i32>,
    pub is_active: Option<bool>,
}

/// `POST /attribute-definitions`
///
/// # `validation_schema` 本身會被驗證是合法的 JSON Schema
///
/// 存進去之後它會被用來擋別人的請求。一份壞掉的 schema 會讓
/// `POST /assets` 回 500（見 `form_schema::validate_named` 的判斷：
/// schema 壞是設定問題，不是客戶端的錯）。
///
/// 所以在**這裡**編譯一次 —— 建立設定的人才是能修它的人。
/// 讓它在使用時才爆，會讓一個完全正確的 `POST /assets` 得到 500。
///
/// # `default_value` 也要符合自己的 schema
///
/// 一個不符合自己 schema 的預設值是保證會失敗的組合：套用預設值之後
/// 立刻驗不過。這種矛盾要在建立時擋掉。
pub async fn create_definition(
    State(state): State<AssetState>,
    caller: Caller,
    Json(body): Json<DefinitionCreate>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let target_entity = body
        .target_entity
        .as_deref()
        .map(str::to_uppercase)
        .unwrap_or_else(|| "ASSET".to_string());
    if !TARGET_ENTITIES.contains(&target_entity.as_str()) {
        return Err(Problem::validation(format!(
            "target_entity 必須是 {} 其中之一",
            TARGET_ENTITIES.join("／")
        )));
    }
    let attribute_key = required(&body.attribute_key, "attribute_key")?;
    let label = required(&body.label, "label")?;
    let data_type = body
        .data_type
        .as_deref()
        .map(str::to_uppercase)
        .unwrap_or_else(|| "STRING".to_string());
    if !ATTR_DATA_TYPES.contains(&data_type.as_str()) {
        return Err(Problem::validation(format!(
            "data_type 必須是 {} 其中之一",
            ATTR_DATA_TYPES.join("／")
        )));
    }
    // `attribute_key` 會變成 `attributes` 裡的 JSON key，所以限制它的字元 ——
    // 帶點或引號的 key 會讓 JSON pointer 形式的錯誤路徑無法解析。
    if !attribute_key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(Problem::validation(
            "attribute_key 只能用英數與底線 —— 它會變成 JSON 的 key，\
             帶點或引號會讓錯誤路徑無法解析",
        ));
    }

    let schema = body
        .validation_schema
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    // 在這裡編譯一次。見 handler 檔頭：讓它在使用時才爆，會讓一個完全正確的
    // `POST /assets` 得到 500，而修它的人不在那條路徑上。
    jsonschema::validator_for(&schema).map_err(|e| {
        Problem::validation(format!(
            "validation_schema 不是合法的 JSON Schema：{e} —— \
             存進去之後它會用來擋別人的請求，壞掉的 schema 會讓那些請求收到 500"
        ))
    })?;

    // 預設值必須符合自己的 schema，否則套用預設值之後立刻驗不過。
    if let Some(dv) = body.default_value.as_ref() {
        fms_shared::form_schema::validate_named(
            &schema,
            dv,
            "/default_value",
            "這個定義自己的 validation_schema",
        )
        .map_err(|p| {
            p.with_detail(
                "default_value 不符合自己的 validation_schema —— \
                 那是保證會失敗的組合：套用預設值之後立刻驗不過",
            )
        })?;
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "tenant:update", None, None).await?;

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.attribute_definitions
           (tenant_id, target_entity, applies_to_type, attribute_key, label,
            data_type, is_required, is_searchable, default_value,
            validation_schema, ui_hints, display_order, is_active)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5,
                 coalesce($6, false), coalesce($7, false), $8,
                 $9, coalesce($10, '{}'::jsonb), coalesce($11, 100),
                 coalesce($12, true))
         RETURNING id",
    )
    .bind(&target_entity)
    .bind(body.applies_to_type.as_deref())
    .bind(attribute_key)
    .bind(label)
    .bind(&data_type)
    .bind(body.is_required)
    .bind(body.is_searchable)
    .bind(body.default_value.as_ref())
    .bind(&schema)
    .bind(body.ui_hints.as_ref())
    .bind(body.display_order)
    .bind(body.is_active)
    .fetch_one(tx.conn())
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.constraint().is_some_and(|c| c.contains("attribute")) => {
            Problem::new(fms_shared::ProblemCode::Conflict)
                .with_detail("這個 target_entity 已經有同名的 attribute_key 了")
        }
        _ => Problem::from(e),
    })?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

// -----------------------------------------------------------------------------
// POST /assets/{assetId}/relations
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RelationCreate {
    pub to_asset_id: Option<Uuid>,
    pub relation_type: Option<String>,
    pub impact_level: Option<String>,
    pub notes: Option<String>,
}

/// `POST /assets/{assetId}/relations`
///
/// # 循環偵測是必要的，不是防禦性程式碼
///
/// `/assets/{id}/dependency-graph` 已經在讀 `asset_relations`。
/// `ck_asset_relations_distinct` 擋掉了 A → A，但擋不掉 A → B → A ——
/// 而那會讓那支圖無限展開。
///
/// 所以寫入前用遞迴 CTE 問一件事：**從新關係的終點出發，走得回起點嗎**。
/// 走得回去就是循環。
///
/// 深度也一併擋：遞迴 CTE 的停止條件與錯誤訊息用同一個常數
/// （[`MAX_DEPENDENCY_DEPTH`]）—— 分開寫會讓訊息說一個數字而實際擋在另一個。
pub async fn create_relation(
    State(state): State<AssetState>,
    caller: Caller,
    Path(from_asset_id): Path<Uuid>,
    Json(body): Json<RelationCreate>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let to_asset_id = body
        .to_asset_id
        .ok_or_else(|| Problem::validation("to_asset_id 為必填"))?;
    if to_asset_id == from_asset_id {
        return Err(Problem::validation(
            "一台設備不能依賴自己（`ck_asset_relations_distinct`）",
        ));
    }
    let relation_type = body
        .relation_type
        .as_deref()
        .map(str::to_uppercase)
        .unwrap_or_else(|| "DEPENDS_ON".to_string());
    if !RELATION_TYPES.contains(&relation_type.as_str()) {
        return Err(Problem::validation(format!(
            "relation_type 必須是 {} 其中之一",
            RELATION_TYPES.join("／")
        )));
    }
    if let Some(l) = body.impact_level.as_deref() {
        if !IMPACT_LEVELS.contains(&l.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "impact_level 必須是 {} 其中之一",
                IMPACT_LEVELS.join("／")
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "asset:write", None, None).await?;

    // 兩端都必須看得到。少了這一步，範圍外的設備會以外鍵違反回 500，
    // 而那看起來像系統壞了而不是「你看不到那台設備」。
    let visible: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.assets WHERE id = ANY(ARRAY[$1::uuid, $2::uuid])",
    )
    .bind(from_asset_id)
    .bind(to_asset_id)
    .fetch_one(tx.conn())
    .await?;
    if visible < 2 {
        return Err(Problem::not_found("兩端的設備都必須存在且在你的範圍內"));
    }

    // **循環偵測。** 從 to 出發沿著既有關係走，看得不看得到 from。
    //
    // `depth <= MAX` 是停止條件而不只是限制：沒有它，一條**既有的**循環
    // （若資料庫裡已經有）會讓這個查詢自己無限跑。
    let cycle: Option<i32> = sqlx::query_scalar(
        "WITH RECURSIVE walk(asset_id, depth) AS (
           SELECT $2::uuid, 1
           UNION ALL
           SELECT r.to_asset_id, w.depth + 1
             FROM walk w
             JOIN fms.asset_relations r ON r.from_asset_id = w.asset_id
            WHERE w.depth <= $3::int
         )
         SELECT min(depth) FROM walk WHERE asset_id = $1::uuid",
    )
    .bind(from_asset_id)
    .bind(to_asset_id)
    .bind(MAX_DEPENDENCY_DEPTH)
    .fetch_one(tx.conn())
    .await?;
    if let Some(d) = cycle {
        return Err(
            Problem::new(fms_shared::ProblemCode::Conflict).with_detail(format!(
                "這條關係會形成循環（從終點走 {d} 步就回到起點）—— \
             /assets/{{id}}/dependency-graph 會因此無限展開"
            )),
        );
    }

    // 深度：新關係之後，從起點往下走的最深鏈有多長。
    let depth: Option<i32> = sqlx::query_scalar(
        "WITH RECURSIVE walk(asset_id, depth) AS (
           SELECT $1::uuid, 0
           UNION ALL
           SELECT r.to_asset_id, w.depth + 1
             FROM walk w
             JOIN fms.asset_relations r ON r.from_asset_id = w.asset_id
            WHERE w.depth <= $2::int
         )
         SELECT max(depth) FROM walk",
    )
    .bind(to_asset_id)
    .bind(MAX_DEPENDENCY_DEPTH)
    .fetch_one(tx.conn())
    .await?;
    // +1 是這條新關係本身。
    let new_depth = depth.unwrap_or(0) + 1;
    if new_depth > MAX_DEPENDENCY_DEPTH {
        return Err(
            Problem::new(fms_shared::ProblemCode::Conflict).with_detail(format!(
                "這條關係會讓依賴鏈深達 {new_depth} 層，上限是 {MAX_DEPENDENCY_DEPTH} —— \
             真實的設施依賴鏈是個位數，撞到這個上限幾乎一定表示資料有問題"
            )),
        );
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.asset_relations
           (tenant_id, from_asset_id, to_asset_id, relation_type, impact_level, notes)
         VALUES (fms.current_tenant_id(), $1, $2, $3, coalesce(upper($4), 'MEDIUM'), $5)
         RETURNING id",
    )
    .bind(from_asset_id)
    .bind(to_asset_id)
    .bind(&relation_type)
    .bind(body.impact_level.as_deref())
    .bind(body.notes.as_deref())
    .fetch_one(tx.conn())
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.constraint().is_some_and(|c| c.starts_with("uq_")) => {
            Problem::new(fms_shared::ProblemCode::Conflict)
                .with_detail("這兩台設備之間已經有同型別的關係了")
        }
        _ => Problem::from(e),
    })?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": id,
            "from_asset_id": from_asset_id,
            "to_asset_id": to_asset_id,
            "relation_type": relation_type,
            "meta": {
                // 回報這條關係之後的鏈深，讓管理者看得出自己離上限多遠。
                "chain_depth": new_depth,
                "max_depth": MAX_DEPENDENCY_DEPTH,
            },
        })),
    ))
}

/// `DELETE /asset-relations/{relationId}`
///
/// 硬刪除。依賴關係是**當前**的拓樸描述，不是歷史事件 ——
/// 軟刪除會讓 `dependency-graph` 必須每次過濾 `deleted_at`，
/// 而漏一次就會顯示已經拆掉的依賴。
pub async fn delete_relation(
    State(state): State<AssetState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "asset:write", None, None).await?;

    let deleted: Option<Uuid> =
        sqlx::query_scalar("DELETE FROM fms.asset_relations WHERE id = $1 RETURNING id")
            .bind(id)
            .fetch_optional(tx.conn())
            .await?;
    if deleted.is_none() {
        return Err(Problem::not_found("找不到這條關係（或它不在你的範圍內）"));
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn required<'a>(v: &'a Option<String>, field: &str) -> Result<&'a str, Problem> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation(format!("{field} 為必填")))
}

// -----------------------------------------------------------------------------
// POST /assets:bulk-import
// -----------------------------------------------------------------------------

/// 一次匯入的上限。與 `/telemetry:batch-ingest` 的 1000 是同一個理由：
/// 在這裡擋而不是讓資料庫慢慢吃 —— 一批十萬筆會佔住連線幾分鐘，
/// 而呼叫端只會看到逾時。
const MAX_IMPORT_ROWS: usize = 500;

#[derive(Debug, Deserialize)]
pub struct BulkImportRequest {
    /// **預設 true。** 匯入是不可逆的批次寫入，所以預設值該是安全的那一邊
    /// —— 想真的寫入必須明說 `dry_run: false`。
    pub dry_run: Option<bool>,
    pub rows: Vec<ImportRow>,
}

#[derive(Debug, Deserialize)]
pub struct ImportRow {
    pub asset_code: Option<String>,
    pub name: Option<String>,
    pub facility_id: Option<Uuid>,
    pub category_code: Option<String>,
    pub spatial_node_code: Option<String>,
    pub model_id: Option<Uuid>,
    pub status: Option<String>,
    pub criticality: Option<String>,
    pub attributes: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct RowOutcome {
    pub index: usize,
    pub asset_code: Option<String>,
    /// `CREATED`／`WOULD_CREATE`／`REJECTED`。
    ///
    /// 試跑用 `WOULD_CREATE` 而不是 `CREATED`：呼叫端把回應存下來當紀錄時，
    /// 那兩個字的差別就是「這件事發生了」與「這件事會發生」。
    pub outcome: String,
    pub asset_id: Option<Uuid>,
    pub error_code: Option<String>,
    pub error: Option<String>,
}

/// `POST /assets:bulk-import`
///
/// # `dry_run` 走**完全相同**的路徑，只是最後回捲
///
/// 預覽與真跑各寫一份驗證的話，預覽通過而真跑失敗正是它要防的事。
/// 所以這裡的做法是：照真的寫入做（含唯一鍵衝突、外鍵、JSON Schema），
/// 然後在 `dry_run` 時 **rollback 整個交易**。
///
/// 那讓預覽的結果與真跑逐列相同 —— 包括**批次內部的重複**
/// （同一批裡兩列用同樣的 `asset_code`），而那種衝突是逐列驗證抓不到的：
/// 它只在第二列真的寫進去時才出現。
///
/// # 逐列 savepoint
///
/// 與 `/telemetry:batch-ingest` 同一個理由：一個交易裡某一筆 SQL 失敗會讓
/// **整個交易**進入 aborted 狀態，後面每一筆都會拿到
/// `current transaction is aborted`。
///
/// 少了 savepoint，「500 列裡有 3 列分類代碼打錯」會變成 497 列好資料一起
/// 被丟掉，而呼叫端只看到一個 500。
pub async fn bulk_import(
    State(state): State<AssetState>,
    caller: Caller,
    Json(req): Json<BulkImportRequest>,
) -> Result<Json<serde_json::Value>, Problem> {
    // 預設 true —— 見 `BulkImportRequest::dry_run` 的註解。
    let dry_run = req.dry_run.unwrap_or(true);

    if req.rows.is_empty() {
        return Err(Problem::validation("rows 不能是空的"));
    }
    if req.rows.len() > MAX_IMPORT_ROWS {
        return Err(Problem::validation(format!(
            "單次上限 {MAX_IMPORT_ROWS} 列，這一批有 {} 列",
            req.rows.len()
        )));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 範圍用 None：一批可以跨場域，而每一列的場域由 `facility_id` 決定
    // —— 看不到的場域那一列會被 RLS 擋成 REJECTED。
    require_permission(&mut tx, "asset:write", None, None).await?;

    // 動態欄位定義一次讀出來。每列各查一次的話，500 列就是 500 次查詢，
    // 而定義在一次匯入中不會變。
    let definitions: Vec<(String, serde_json::Value, bool)> = sqlx::query_as(
        "SELECT attribute_key::text, validation_schema, is_required
           FROM fms.attribute_definitions
          WHERE target_entity = 'ASSET' AND is_active",
    )
    .fetch_all(tx.conn())
    .await?;

    let mut outcomes: Vec<RowOutcome> = Vec::with_capacity(req.rows.len());
    let mut created = 0usize;

    for (i, row) in req.rows.iter().enumerate() {
        // **每一列一個 savepoint。** 見 handler 檔頭。
        sqlx::query("SAVEPOINT row").execute(tx.conn()).await?;

        match import_one(&mut tx, row, &definitions).await {
            Ok(id) => {
                sqlx::query("RELEASE SAVEPOINT row")
                    .execute(tx.conn())
                    .await?;
                created += 1;
                outcomes.push(RowOutcome {
                    index: i,
                    asset_code: row.asset_code.clone(),
                    outcome: if dry_run { "WOULD_CREATE" } else { "CREATED" }.to_string(),
                    asset_id: Some(id),
                    error_code: None,
                    error: None,
                });
            }
            Err((code, msg)) => {
                sqlx::query("ROLLBACK TO SAVEPOINT row")
                    .execute(tx.conn())
                    .await?;
                outcomes.push(RowOutcome {
                    index: i,
                    asset_code: row.asset_code.clone(),
                    outcome: "REJECTED".to_string(),
                    asset_id: None,
                    error_code: Some(code),
                    error: Some(msg),
                });
            }
        }
    }

    // 試跑：回捲整個交易。**不 commit。**
    if dry_run {
        drop(tx);
    } else {
        tx.commit().await?;
    }

    Ok(Json(serde_json::json!({
        "dry_run": dry_run,
        "total": req.rows.len(),
        "accepted": created,
        "rejected": req.rows.len() - created,
        "rows": outcomes,
        "meta": {
            // 試跑時 `asset_id` 是**回捲掉的** id，不能拿去用。
            // 不說清楚的話，呼叫端可能把它存起來然後查不到。
            "ids_are_provisional": dry_run,
            "limit": MAX_IMPORT_ROWS,
        },
    })))
}

/// 匯入一列。回傳 `(error_code, message)` 讓呼叫端能自動分類：
/// 「分類代碼打錯」要修檔案，「編號重複」可能可以忽略。
async fn import_one(
    tx: &mut fms_shared::TenantTx,
    row: &ImportRow,
    definitions: &[(String, serde_json::Value, bool)],
) -> Result<Uuid, (String, String)> {
    let asset_code = row
        .asset_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ("MISSING_ASSET_CODE".to_string(), "asset_code 為必填".into()))?;
    let name = row
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ("MISSING_NAME".to_string(), "name 為必填".into()))?;
    let facility_id = row
        .facility_id
        .ok_or_else(|| ("MISSING_FACILITY".to_string(), "facility_id 為必填".into()))?;
    let category_code = row
        .category_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            (
                "MISSING_CATEGORY".to_string(),
                "category_code 為必填（`assets.category_id` 是 NOT NULL）".into(),
            )
        })?;

    // 動態欄位：拿 `attribute_definitions` 的 JSON Schema 驗。
    // **在這一層驗，不是資料庫觸發器** —— 見模組檔頭。
    let attributes = row
        .attributes
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    for (key, schema, is_required) in definitions {
        match attributes.get(key) {
            Some(value) => {
                if let Err(p) = fms_shared::form_schema::validate_named(
                    schema,
                    value,
                    &format!("/attributes/{key}"),
                    &format!("attribute_definitions.{key} 的 validation_schema"),
                ) {
                    // `errors` 帶著逐欄位的路徑與訊息，比 `detail` 具體 ——
                    // 匯入是逐列回報，呼叫端要的是「哪一個欄位錯了」。
                    let detail = if p.errors.is_empty() {
                        p.detail.unwrap_or_else(|| "不符合定義".to_string())
                    } else {
                        p.errors
                            .iter()
                            .map(|e| format!("{}: {}", e.pointer, e.message))
                            .collect::<Vec<_>>()
                            .join("; ")
                    };
                    return Err((
                        "ATTRIBUTE_SCHEMA_VIOLATION".to_string(),
                        format!("attributes.{key} {detail}"),
                    ));
                }
            }
            None if *is_required => {
                return Err((
                    "MISSING_REQUIRED_ATTRIBUTE".to_string(),
                    format!("必填的動態欄位 {key} 沒有值"),
                ))
            }
            None => {}
        }
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.assets
           (tenant_id, facility_id, category_id, spatial_node_id, asset_model_id,
            asset_code, name, status, criticality, attributes)
         SELECT fms.current_tenant_id(), $1,
                (SELECT c.id FROM fms.asset_categories c
                  WHERE upper(c.code) = upper($2) LIMIT 1),
                CASE WHEN $3::text IS NULL THEN
                       (SELECT n.id FROM fms.spatial_nodes n
                         WHERE n.facility_id = $1 LIMIT 1)
                     ELSE (SELECT n.id FROM fms.spatial_nodes n
                            WHERE n.facility_id = $1
                              AND upper(n.code) = upper($3::text) LIMIT 1)
                END,
                $4, $5, $6,
                coalesce(upper($7), 'OPERATIONAL'),
                coalesce(upper($8), 'MEDIUM'),
                $9
         RETURNING id",
    )
    .bind(facility_id)
    .bind(category_code)
    .bind(row.spatial_node_code.as_deref())
    .bind(row.model_id)
    .bind(asset_code)
    .bind(name)
    .bind(row.status.as_deref())
    .bind(row.criticality.as_deref())
    .bind(&attributes)
    .fetch_one(tx.conn())
    .await
    .map_err(|e| classify(&e, category_code))?;

    Ok(id)
}

/// 把資料庫錯誤翻成可自動分類的代碼。
///
/// `category_id` 為 NULL 的 not-null 違反其實是「分類代碼查不到」——
/// 直接回那個 SQLSTATE 會讓使用者以為自己漏填了 category_id，
/// 而他填了、只是打錯字。
fn classify(e: &sqlx::Error, category_code: &str) -> (String, String) {
    let (constraint, code) = match e {
        sqlx::Error::Database(db) => (
            db.constraint().map(str::to_string),
            db.code().map(|c| c.to_string()),
        ),
        _ => (None, None),
    };
    match (constraint.as_deref(), code.as_deref()) {
        (Some("uq_assets_tenant_code"), _) => (
            "DUPLICATE_ASSET_CODE".to_string(),
            "這個 asset_code 已經存在（不分大小寫）—— 同一批裡重複也會撞到這裡".into(),
        ),
        (Some("assets_facility_id_fkey"), _) => (
            "FACILITY_NOT_FOUND".to_string(),
            "找不到這個場域（或它不在你的範圍內）".into(),
        ),
        (Some("assets_asset_model_id_fkey"), _) => {
            ("MODEL_NOT_FOUND".to_string(), "找不到這個型號".into())
        }
        (Some("assets_status_check"), _) => (
            "BAD_STATUS".to_string(),
            "status 不在允許值裡（PLANNED／IN_STORAGE／INSTALLING／OPERATIONAL／\
             DEGRADED／DOWN／UNDER_MAINTENANCE／DECOMMISSIONED）"
                .into(),
        ),
        (Some("assets_criticality_check"), _) => (
            "BAD_CRITICALITY".to_string(),
            "criticality 必須是 LOW／MEDIUM／HIGH／CRITICAL".into(),
        ),
        // 23502 = not_null_violation。category_id 是 NOT NULL，而它來自子查詢
        // —— 查不到就是 NULL。見函式檔頭。
        (_, Some("23502")) => (
            "CATEGORY_NOT_FOUND".to_string(),
            format!("查不到分類代碼「{category_code}」—— 檢查拼字，或先建立那個分類"),
        ),
        _ => ("DB_ERROR".to_string(), e.to_string()),
    }
}

// -----------------------------------------------------------------------------
// 給 `POST /assets` 用的驗證入口
// -----------------------------------------------------------------------------

/// 拿 `attribute_definitions` 驗一筆 `assets.attributes`。
///
/// **這是 `attribute_definitions` 的第一個讀者。** 在它之前那張表是
/// 0 列 0 讀者 —— 一份宣告了沒有人讀的定義，正是這個 repo 反覆出現的缺陷。
///
/// 沒有任何啟用的定義時直接回 Ok：那代表這個租戶還沒有設定動態欄位，
/// 而不是「所有 attributes 都非法」。
///
/// 未在定義裡的 key **不拒絕**。理由：`assets.attributes` 從 003 就是自由
/// jsonb，既有資料可能有任何 key。拒絕未知 key 會讓一次「新增定義」變成
/// 一次資料遷移 —— 而那不是新增一個欄位該有的代價。
/// 要收緊到白名單需要先有一支稽核端點找出現有的 key，那不在這一輪。
pub async fn validate_asset_attributes(
    tx: &mut fms_shared::TenantTx,
    attributes: Option<&serde_json::Value>,
) -> Result<(), Problem> {
    let Some(attributes) = attributes else {
        // 沒帶 attributes：只要沒有必填的定義就通過。
        return check_required_present(tx, &serde_json::json!({})).await;
    };
    if !attributes.is_object() {
        return Err(Problem::validation("attributes 必須是一個物件"));
    }

    let definitions: Vec<(String, serde_json::Value, bool)> = sqlx::query_as(
        "SELECT attribute_key::text, validation_schema, is_required
           FROM fms.attribute_definitions
          WHERE target_entity = 'ASSET' AND is_active",
    )
    .fetch_all(tx.conn())
    .await?;

    let mut errors: Vec<fms_shared::FieldError> = Vec::new();
    for (key, schema, is_required) in &definitions {
        match attributes.get(key) {
            Some(value) => {
                if let Err(p) = fms_shared::form_schema::validate_named(
                    schema,
                    value,
                    &format!("/attributes/{key}"),
                    &format!("attribute_definitions.{key} 的 validation_schema"),
                ) {
                    // schema 本身壞掉是 500（設定問題），不該被降級成 422。
                    if p.errors.is_empty() {
                        return Err(p);
                    }
                    errors.extend(p.errors);
                }
            }
            None if *is_required => errors.push(fms_shared::FieldError {
                pointer: format!("/attributes/{key}"),
                code: "REQUIRED".to_string(),
                message: format!("必填的動態欄位 {key} 沒有值"),
            }),
            None => {}
        }
    }

    if errors.is_empty() {
        return Ok(());
    }
    Err(Problem::validation("attributes 不符合這個租戶的動態欄位定義").with_errors(errors))
}

/// 只檢查必填 —— `attributes` 完全沒帶時用這一條。
async fn check_required_present(
    tx: &mut fms_shared::TenantTx,
    attributes: &serde_json::Value,
) -> Result<(), Problem> {
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT attribute_key::text
           FROM fms.attribute_definitions
          WHERE target_entity = 'ASSET' AND is_active AND is_required
            AND NOT ($1::jsonb ? attribute_key::text)
          ORDER BY attribute_key",
    )
    .bind(attributes)
    .fetch_all(tx.conn())
    .await?;
    if missing.is_empty() {
        return Ok(());
    }
    Err(Problem::validation("缺少必填的動態欄位").with_errors(
        missing
            .into_iter()
            .map(|k| fms_shared::FieldError {
                pointer: format!("/attributes/{k}"),
                code: "REQUIRED".to_string(),
                message: format!("必填的動態欄位 {k} 沒有值"),
            })
            .collect(),
    ))
}
