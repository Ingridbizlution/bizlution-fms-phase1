//! 服務項目的管理面與可用時段（契約 §6 的其餘四支）。
//!
//! # `availability` 從這裡起有第一個讀者
//!
//! 004 建了那個欄位並在註解裡寫了形狀，但這個 crate 從來沒有讀過它 ——
//! 量過：`dto.rs`／`handlers.rs`／`lib.rs`／`repo.rs` 四個檔案裡一次都沒有
//! 出現。所以 `GET /service-items/{id}/availability` 是它的第一個消費者，
//! 而 migration 068 順勢給了它形狀約束（打錯字的 `blackout_date` 會被擋 ——
//! 少一個 s 在寬鬆的版本下會讓所有停止服務日靜默失效）。
//!
//! # 停用是軟刪除，而且要說出後果
//!
//! `DELETE /service-items/{id}` 設 `deleted_at`。**不硬刪**：`work_orders`
//! 存著 `service_item_id`，硬刪會讓既有工單指向一個不存在的列（外鍵是
//! `ON DELETE SET NULL`，所以那些工單會失去它們是什麼服務的紀錄）。
//!
//! 而回應要說出**還有幾張未結工單引用它** —— 停用一個還有 20 張進行中工單的
//! 服務是合法的（不再接受新申請），但那 20 張的處理方式與零張完全不同。
//! 只回 204 的話那個數字沒有人看得到。
//!
//! # `lead_time_minutes` 與可用時段是兩件事
//!
//! 前者是「最晚要提前多久申請」，後者是「哪幾個小時營業」。混在一起會讓一個
//! lead time 48 小時的服務看起來像「明天不營業」。`/availability` 兩者都回，
//! 並算出 `earliest_requestable_at` —— 那是客戶端真正需要的那個時刻。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, FieldError, Problem};

use crate::handlers::CatalogueState;

/// `service_items.category` 的 CHECK 允許值。在應用層再擋一次的理由與
/// 其他模組相同：`query!` 不驗 CHECK 的字串值，不先擋就會把客戶端的錯字
/// 變成 500。
const CATEGORIES: &[&str] = &[
    "CLEANING",
    "CATERING",
    "IT_SUPPORT",
    "ROOM_SETUP",
    "SECURITY",
    "MOVING",
    "WASTE",
    "LANDSCAPING",
    "AV_SUPPORT",
    "RECEPTION",
    "OTHER",
];

// =============================================================================
// POST /facilities/{facilityId}/service-items
// =============================================================================

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateServiceItemRequest {
    pub category: String,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub lead_time_minutes: Option<i32>,
    pub default_duration_minutes: Option<i32>,
    pub relative_offset_minutes: Option<i32>,
    pub is_attachable_to_reservation: Option<bool>,
    pub is_standalone_requestable: Option<bool>,
    pub requires_approval: Option<bool>,
    pub approver_role_code: Option<String>,
    pub chargeable: Option<bool>,
    pub unit_price: Option<f64>,
    pub currency: Option<String>,
    pub unit_label: Option<String>,
    pub max_quantity: Option<i32>,
    pub form_schema: Option<serde_json::Value>,
    pub availability: Option<serde_json::Value>,
    pub icon: Option<String>,
    pub display_order: Option<i32>,
    /// `true` = 全場域適用（`facility_id` 存 NULL）。路徑上的場域仍然是
    /// 權限判定的依據 —— 建立一個全租戶生效的項目需要**某一個**場域的
    /// `service_item:write`，這是既有權限模型的邊界，記在契約裡。
    pub applies_to_all_facilities: Option<bool>,
}

/// 建立與更新共用的驗證。
///
/// `chargeable` 與價格三欄的關係是這裡唯一有實質內容的規則：
/// **可收費但沒有單價**會產出一張金額不明的帳單，而症狀出現在對帳的時候。
fn validate_pricing(
    chargeable: Option<bool>,
    unit_price: Option<f64>,
    currency: Option<&str>,
) -> Result<(), Problem> {
    if chargeable == Some(true) {
        if unit_price.is_none() {
            return Err(
                Problem::validation("`chargeable` 為 true 時必須有 `unit_price`").with_errors(
                    vec![FieldError {
                        pointer: "/unit_price".to_string(),
                        code: "REQUIRED".to_string(),
                        message: "可收費但沒有單價會產出一張金額不明的帳單".to_string(),
                    }],
                ),
            );
        }
        if currency.is_none() {
            return Err(Problem::validation(
                "`chargeable` 為 true 時必須有 `currency` —— \
                 一個沒有幣別的金額在跨國租戶上是無法對帳的",
            ));
        }
    }
    if let Some(p) = unit_price {
        if p < 0.0 {
            return Err(Problem::validation("`unit_price` 不得為負"));
        }
    }
    if let Some(c) = currency {
        if c.len() != 3 || !c.chars().all(|ch| ch.is_ascii_uppercase()) {
            return Err(Problem::validation(
                "`currency` 必須是三個大寫字母的 ISO 4217 代碼",
            ));
        }
    }
    Ok(())
}

fn validate_durations(
    lead: Option<i32>,
    duration: Option<i32>,
    max_qty: Option<i32>,
) -> Result<(), Problem> {
    if lead.is_some_and(|v| v < 0) {
        return Err(Problem::validation("`lead_time_minutes` 不得為負"));
    }
    if duration.is_some_and(|v| v < 1) {
        return Err(Problem::validation("`default_duration_minutes` 必須 >= 1"));
    }
    if max_qty.is_some_and(|v| v < 1) {
        return Err(Problem::validation(
            "`max_quantity` 必須 >= 1（不限數量請不要送這個欄位）",
        ));
    }
    Ok(())
}

/// `POST /facilities/{facilityId}/service-items`
pub async fn create(
    State(state): State<CatalogueState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Json(req): Json<CreateServiceItemRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    if !CATEGORIES.contains(&req.category.as_str()) {
        return Err(
            Problem::validation(format!("`category` 必須是 {CATEGORIES:?} 之一")).with_errors(
                vec![FieldError {
                    pointer: "/category".to_string(),
                    code: "ENUM".to_string(),
                    message: format!("`{}` 不是合法的類別", req.category),
                }],
            ),
        );
    }
    if req.code.trim().is_empty() || req.code.len() > 50 {
        return Err(Problem::validation("`code` 長度必須是 1–50"));
    }
    if req.name.trim().is_empty() || req.name.chars().count() > 200 {
        return Err(Problem::validation("`name` 長度必須是 1–200"));
    }
    // 兩個都 false 等於建立一個**無法被申請的服務項目**。那不是錯誤設定，
    // 而是一個沒有用的列 —— 擋下來比讓管理者以後才發現好。
    if req.is_attachable_to_reservation == Some(false)
        && req.is_standalone_requestable == Some(false)
    {
        return Err(Problem::validation(
            "`is_attachable_to_reservation` 與 `is_standalone_requestable` \
             不能同時為 false —— 那樣建出來的服務項目沒有任何入口可以申請",
        ));
    }
    validate_pricing(req.chargeable, req.unit_price, req.currency.as_deref())?;
    validate_durations(
        req.lead_time_minutes,
        req.default_duration_minutes,
        req.max_quantity,
    )?;

    let all_facilities = req.applies_to_all_facilities.unwrap_or(false);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "service_item:write", Some(facility_id), None).await?;

    let row: ServiceItemAdminDto = sqlx::query_as!(
        ServiceItemAdminDto,
        r#"INSERT INTO fms.service_items
             (tenant_id, facility_id, category, code, name, description,
              lead_time_minutes, default_duration_minutes, relative_offset_minutes,
              is_attachable_to_reservation, is_standalone_requestable,
              requires_approval, approver_role_code, chargeable, unit_price,
              currency, unit_label, max_quantity, form_schema, availability,
              icon, display_order)
           VALUES (fms.current_tenant_id(),
                   CASE WHEN $1 THEN NULL ELSE $2::uuid END,
                   $3, $4, $5, $6,
                   coalesce($7, 0), coalesce($8, 30), coalesce($9, 0),
                   coalesce($10, true), coalesce($11, true),
                   coalesce($12, false), $13, coalesce($14, false), $15::float8::numeric,
                   $16, $17, $18,
                   coalesce($19, '{"type":"object","properties":{}}'::jsonb),
                   coalesce($20, '{}'::jsonb),
                   $21, coalesce($22, 100))
           RETURNING id, facility_id, category, code::text AS "code!",
                     name::text AS "name!", description,
                     lead_time_minutes, default_duration_minutes,
                     relative_offset_minutes, is_attachable_to_reservation,
                     is_standalone_requestable, requires_approval,
                     approver_role_code::text AS approver_role_code,
                     chargeable, unit_price::float8 AS "unit_price",
                     currency::text AS currency,
                     unit_label::text AS unit_label, max_quantity,
                     form_schema, availability, icon::text AS icon,
                     display_order, is_active, deleted_at"#,
        all_facilities,
        facility_id,
        req.category,
        req.code,
        req.name,
        req.description,
        req.lead_time_minutes,
        req.default_duration_minutes,
        req.relative_offset_minutes,
        req.is_attachable_to_reservation,
        req.is_standalone_requestable,
        req.requires_approval,
        req.approver_role_code,
        req.chargeable,
        req.unit_price,
        req.currency,
        req.unit_label,
        req.max_quantity,
        req.form_schema,
        req.availability,
        req.icon,
        req.display_order,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(map_service_item_violation)?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "data": row,
            "meta": {
                "applies_to_all_facilities": all_facilities,
                // 全場域項目由**某一個**場域的權限建立 —— 記在這裡讓讀者
                // 知道那是既有權限模型的邊界，不是漏檢查。
                "authorized_via_facility": facility_id,
            },
        })),
    ))
}

// =============================================================================
// PATCH /service-items/{id}
// =============================================================================

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchServiceItemRequest {
    pub category: Option<String>,
    pub name: Option<String>,
    pub lead_time_minutes: Option<i32>,
    pub default_duration_minutes: Option<i32>,
    pub relative_offset_minutes: Option<i32>,
    pub is_attachable_to_reservation: Option<bool>,
    pub is_standalone_requestable: Option<bool>,
    pub requires_approval: Option<bool>,
    pub chargeable: Option<bool>,
    pub form_schema: Option<serde_json::Value>,
    pub availability: Option<serde_json::Value>,
    pub display_order: Option<i32>,
    pub is_active: Option<bool>,
    #[serde(default, with = "double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    pub approver_role_code: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    pub unit_price: Option<Option<f64>>,
    #[serde(default, with = "double_option")]
    pub currency: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    pub unit_label: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    pub max_quantity: Option<Option<i32>>,
    #[serde(default, with = "double_option")]
    pub icon: Option<Option<String>>,
}

pub(crate) mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::deserialize(d).map(Some)
    }
}

fn split<T>(v: Option<Option<T>>) -> (bool, Option<T>) {
    match v {
        None => (false, None),
        Some(inner) => (true, inner),
    }
}

/// `PATCH /service-items/{serviceItemId}`
///
/// **`code` 與 `facility_id` 不可變更。** `code` 是 `uq_service_items_code`
/// 的一部分，而客戶端可能已經在自己的設定裡引用它；`facility_id` 改了會讓
/// 既有工單所屬的服務項目突然屬於另一個場域。
pub async fn patch(
    State(state): State<CatalogueState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, Problem> {
    let obj = body
        .0
        .as_object()
        .ok_or_else(|| Problem::validation("請求體必須是一個 JSON 物件"))?;
    if obj.is_empty() {
        return Err(Problem::validation("沒有要更新的欄位"));
    }
    for f in ["code", "facility_id", "id", "tenant_id"] {
        if obj.contains_key(f) {
            return Err(
                Problem::validation(format!("`{f}` 不可變更")).with_errors(vec![FieldError {
                    pointer: format!("/{f}"),
                    code: "IMMUTABLE".to_string(),
                    message: match f {
                        "code" => "code 是唯一索引的一部分，而客戶端可能已經引用它。\
                                   要換代碼請停用舊項目並建立新的"
                            .to_string(),
                        "facility_id" => {
                            "改場域會讓既有工單所屬的服務項目突然屬於另一個場域".to_string()
                        }
                        _ => "識別欄位不可變更".to_string(),
                    },
                }]),
            );
        }
    }

    let req: PatchServiceItemRequest = serde_json::from_value(body.0.clone()).map_err(|e| {
        Problem::validation(format!("不認識的欄位或型別不符：{e}")).with_errors(vec![FieldError {
            pointer: "/".to_string(),
            code: "UNKNOWN_FIELD".to_string(),
            message: e.to_string(),
        }])
    })?;

    if let Some(c) = &req.category {
        if !CATEGORIES.contains(&c.as_str()) {
            return Err(Problem::validation(format!(
                "`category` 必須是 {CATEGORIES:?} 之一"
            )));
        }
    }
    validate_durations(
        req.lead_time_minutes,
        req.default_duration_minutes,
        req.max_quantity.flatten(),
    )?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let existing = sqlx::query!(
        r#"SELECT facility_id, chargeable, unit_price::float8 AS "unit_price",
                  currency::text AS currency,
                  is_attachable_to_reservation, is_standalone_requestable
             FROM fms.service_items
            WHERE id = $1 AND deleted_at IS NULL"#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("找不到這個服務項目"))?;

    // 全場域項目（`facility_id IS NULL`）沒有場域可以指定。與 create 同一個
    // 邊界：改它需要 TENANT 範圍的權限，而權限目錄裡 `service_item:write`
    // 是 FACILITY 範圍 —— 所以這裡用「不指定場域」的檢查（RLS 已經把不可見
    // 的列排除）。記在契約裡。
    require_permission(&mut tx, "service_item:write", existing.facility_id, None).await?;

    // **價格規則要拿合併後的值來驗，不是只看送來的欄位。**
    // 只送 `chargeable: true` 而資料庫裡沒有單價，會建出一個「可收費但金額
    // 不明」的項目 —— 那正是 create 擋掉的情況，PATCH 不該有一個後門。
    let (price_given, price) = split(req.unit_price);
    let (currency_given, currency) = split(req.currency);
    let merged_chargeable = req.chargeable.or(Some(existing.chargeable));
    let merged_price = if price_given {
        price
    } else {
        existing.unit_price
    };
    let merged_currency = if currency_given {
        currency.clone()
    } else {
        existing.currency.clone()
    };
    validate_pricing(merged_chargeable, merged_price, merged_currency.as_deref())?;

    // 同理：兩個入口旗標的合併值不能都是 false。
    let merged_attach = req
        .is_attachable_to_reservation
        .unwrap_or(existing.is_attachable_to_reservation);
    let merged_standalone = req
        .is_standalone_requestable
        .unwrap_or(existing.is_standalone_requestable);
    if !merged_attach && !merged_standalone {
        return Err(Problem::validation(
            "合併後兩個申請入口都是 false —— 這個服務項目會變成沒有任何入口可以申請",
        ));
    }

    let (desc_given, desc) = split(req.description);
    let (arc_given, arc) = split(req.approver_role_code);
    let (label_given, label) = split(req.unit_label);
    let (qty_given, qty) = split(req.max_quantity);
    let (icon_given, icon) = split(req.icon);

    let row: ServiceItemAdminDto = sqlx::query_as!(
        ServiceItemAdminDto,
        r#"UPDATE fms.service_items SET
             category                     = coalesce($2, category),
             name                         = coalesce($3, name),
             lead_time_minutes            = coalesce($4, lead_time_minutes),
             default_duration_minutes     = coalesce($5, default_duration_minutes),
             relative_offset_minutes      = coalesce($6, relative_offset_minutes),
             is_attachable_to_reservation = coalesce($7, is_attachable_to_reservation),
             is_standalone_requestable    = coalesce($8, is_standalone_requestable),
             requires_approval            = coalesce($9, requires_approval),
             chargeable                   = coalesce($10, chargeable),
             form_schema                  = coalesce($11, form_schema),
             availability                 = coalesce($12, availability),
             display_order                = coalesce($13, display_order),
             is_active                    = coalesce($14, is_active),
             description        = CASE WHEN $15 THEN $16 ELSE description END,
             approver_role_code = CASE WHEN $17 THEN $18 ELSE approver_role_code END,
             unit_price         = CASE WHEN $19 THEN $20::float8::numeric ELSE unit_price END,
             currency           = CASE WHEN $21 THEN $22 ELSE currency END,
             unit_label         = CASE WHEN $23 THEN $24 ELSE unit_label END,
             max_quantity       = CASE WHEN $25 THEN $26 ELSE max_quantity END,
             icon               = CASE WHEN $27 THEN $28 ELSE icon END,
             updated_at = clock_timestamp()
           WHERE id = $1 AND deleted_at IS NULL
           RETURNING id, facility_id, category, code::text AS "code!",
                     name::text AS "name!", description,
                     lead_time_minutes, default_duration_minutes,
                     relative_offset_minutes, is_attachable_to_reservation,
                     is_standalone_requestable, requires_approval,
                     approver_role_code::text AS approver_role_code,
                     chargeable, unit_price::float8 AS "unit_price",
                     currency::text AS currency,
                     unit_label::text AS unit_label, max_quantity,
                     form_schema, availability, icon::text AS icon,
                     display_order, is_active, deleted_at"#,
        id,
        req.category,
        req.name,
        req.lead_time_minutes,
        req.default_duration_minutes,
        req.relative_offset_minutes,
        req.is_attachable_to_reservation,
        req.is_standalone_requestable,
        req.requires_approval,
        req.chargeable,
        req.form_schema,
        req.availability,
        req.display_order,
        req.is_active,
        desc_given,
        desc,
        arc_given,
        arc,
        price_given,
        price,
        currency_given,
        currency,
        label_given,
        label,
        qty_given,
        qty,
        icon_given,
        icon,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(map_service_item_violation)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": row })))
}

// =============================================================================
// DELETE /service-items/{id}
// =============================================================================

/// `DELETE /service-items/{serviceItemId}` —— 停用（軟刪除）。
///
/// 回應帶**還有幾張未結工單引用它**。停用一個還有 20 張進行中工單的服務是
/// 合法的（不再接受新申請），但那 20 張的處理方式與零張完全不同 ——
/// 只回 204 的話那個數字沒有人看得到。
pub async fn delete(
    State(state): State<CatalogueState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let facility_id: Option<Option<Uuid>> = sqlx::query_scalar!(
        "SELECT facility_id FROM fms.service_items WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;
    let facility_id = facility_id.ok_or_else(|| Problem::not_found("找不到這個服務項目"))?;
    require_permission(&mut tx, "service_item:write", facility_id, None).await?;

    let row = sqlx::query!(
        r#"WITH open_wo AS (
             SELECT count(*) AS n
               FROM fms.work_orders w
               LEFT JOIN fms.work_order_statuses st ON st.code = w.status
              WHERE w.service_item_id = $1
                AND w.deleted_at IS NULL
                AND st.is_terminal IS NOT TRUE
           ), softened AS (
             UPDATE fms.service_items
                SET deleted_at = clock_timestamp(), is_active = false,
                    updated_at = clock_timestamp()
              WHERE id = $1 AND deleted_at IS NULL
             RETURNING id
           )
           SELECT (SELECT n FROM open_wo) AS "open_work_orders!",
                  EXISTS (SELECT 1 FROM softened) AS "deleted!""#,
        id
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": {
            "id": id,
            "deleted": row.deleted,
            // **不再接受新申請，但既有工單不受影響。** 這個數字讓管理者知道
            // 停用之後還有多少事要處理 —— 而 204 不帶任何資訊。
            "open_work_orders": row.open_work_orders,
        },
        "meta": {
            "soft_delete": true,
            "why": "work_orders 存著 service_item_id；硬刪會讓既有工單失去它們是什麼服務的紀錄",
        },
    })))
}

// =============================================================================
// GET /service-items/{id}/availability
// =============================================================================

#[derive(Debug, Deserialize)]
pub struct AvailabilityQuery {
    pub from: Option<chrono::NaiveDate>,
    pub days: Option<i64>,
}

/// `GET /service-items/{serviceItemId}/availability`
///
/// 回傳未來 N 天（預設 7、上限 31）每一天的可用時段。
///
/// 解析在 SQL（068 的 `fms.service_item_windows`）：那組規則有三層
/// （停止服務日 → 服務自己的星期表 → 場域營運時間），而兩份實作最後總會分歧。
///
/// `basis` 讓「空陣列」的三種原因分得開 —— 今天停止服務、這個星期不提供、
/// 這個服務沒設定而且沒有場域可退。三者對使用者的意義完全不同。
///
/// `lead_time_minutes` 與時段是兩件事，所以兩者都回，並算出
/// `earliest_requestable_at` —— 那才是客戶端真正需要的那個時刻。
pub async fn availability(
    State(state): State<CatalogueState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Query(q): Query<AvailabilityQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let days = q.days.unwrap_or(7);
    if !(1..=31).contains(&days) {
        return Err(
            Problem::validation("`days` 必須是 1 到 31").with_errors(vec![FieldError {
                pointer: "/days".to_string(),
                code: "RANGE".to_string(),
                message: format!("{days} 超出範圍"),
            }]),
        );
    }
    let from = q.from.unwrap_or_else(|| chrono::Utc::now().date_naive());

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let item = sqlx::query!(
        r#"SELECT facility_id, name::text AS "name!", lead_time_minutes,
                  default_duration_minutes, is_active
             FROM fms.service_items
            WHERE id = $1 AND deleted_at IS NULL"#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("找不到這個服務項目"))?;
    require_permission(&mut tx, "service_item:read", item.facility_id, None).await?;

    let rows = sqlx::query!(
        r#"SELECT d::date AS "day!",
                  fms.service_item_windows($1, d::date) AS "resolved!"
             FROM generate_series($2::date, $2::date + ($3::int - 1), interval '1 day') d"#,
        id,
        from,
        days as i32,
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let windows = r
                .resolved
                .get("windows")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(vec![]));
            serde_json::json!({
                "date": r.day,
                "windows": windows,
                // 空陣列的三種原因分得開，見函式檔頭。
                "basis": r.resolved.get("basis"),
                "is_blackout": r.resolved.get("is_blackout"),
            })
        })
        .collect();

    let open_days = data
        .iter()
        .filter(|d| d["windows"].as_array().is_some_and(|w| !w.is_empty()))
        .count();

    Ok(Json(serde_json::json!({
        "data": data,
        "meta": {
            "service_item_id": id,
            "name": item.name,
            "is_active": item.is_active,
            // **時段與提前量是兩件事。** 混在一起會讓一個 lead time 48 小時的
            // 服務看起來像「明天不營業」。
            "lead_time_minutes": item.lead_time_minutes,
            "default_duration_minutes": item.default_duration_minutes,
            // 客戶端真正需要的那個時刻：現在 + 提前量。時段告訴他哪幾個小時
            // 開放，這個告訴他最早能訂到什麼時候。
            "earliest_requestable_at": chrono::Utc::now()
                + chrono::Duration::minutes(item.lead_time_minutes as i64),
            "days": days,
            "from": from,
            // 這 N 天裡有幾天真的開放。0 天是有意義的答案（例如整週都在
            // blackout），而它與「查詢區間太短」不同 —— 所以兩個數字都回。
            "open_days": open_days,
        },
    })))
}

// =============================================================================
// 共用
// =============================================================================

/// `GET`／`PATCH` 回傳的完整形狀（含管理面才看得到的欄位）。
///
/// 比 `ServiceItemDto` 多的是 `availability`、`is_active`、`deleted_at`、
/// `approver_role_code`：那四個是設定，型錄瀏覽用不到，而管理畫面需要。
#[derive(Debug, Serialize)]
pub struct ServiceItemAdminDto {
    pub id: Uuid,
    pub facility_id: Option<Uuid>,
    pub category: String,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub lead_time_minutes: i32,
    pub default_duration_minutes: i32,
    pub relative_offset_minutes: i32,
    pub is_attachable_to_reservation: bool,
    pub is_standalone_requestable: bool,
    pub requires_approval: bool,
    pub approver_role_code: Option<String>,
    pub chargeable: bool,
    pub unit_price: Option<f64>,
    pub currency: Option<String>,
    pub unit_label: Option<String>,
    pub max_quantity: Option<i32>,
    pub form_schema: serde_json::Value,
    pub availability: serde_json::Value,
    pub icon: Option<String>,
    pub display_order: i32,
    pub is_active: bool,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// 把資料庫的約束違反翻譯成 422。
fn map_service_item_violation(e: sqlx::Error) -> Problem {
    let db = e.as_database_error();
    match db.and_then(|d| d.constraint()) {
        Some("uq_service_items_code") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail(
                "同一個場域（或全場域範圍）內已經有相同 `code` 的服務項目 —— \
                 code 是唯一的，而停用的項目不佔用它（唯一索引有 \
                 `WHERE deleted_at IS NULL`）",
            ),
        Some("ck_service_items_availability") => Problem::validation(
            "`availability` 的形狀不合：鍵只能是 mon…sun 或 `blackout_dates`，\
             時段是 [[\"07:00\",\"20:00\"], …]，停止服務日是 YYYY-MM-DD 的陣列。\
             **打錯字的鍵會被拒絕**（例如 `blackout_date` 少一個 s）—— \
             放行它會讓那些停止服務日一天都沒有生效",
        )
        .with_errors(vec![FieldError {
            pointer: "/availability".to_string(),
            code: "SHAPE".to_string(),
            message: "形狀由 fms.service_availability_is_valid 驗（migration 068）".to_string(),
        }]),
        Some(c) if c.starts_with("service_items_category") => {
            Problem::validation(format!("`category` 必須是 {CATEGORIES:?} 之一"))
        }
        _ => Problem::from(e),
    }
}
