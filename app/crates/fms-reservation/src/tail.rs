//! Reservations 補完的五支端點。
//!
//! # 三支各讓一個「宣告了沒人寫」的東西第一次有寫入者
//!
//! | 端點 | 讓誰第一次被寫入 |
//! |---|---|
//! | `DELETE /reservations/holds/{token}` | `reservation_holds.status = 'RELEASED'`（005 的 CHECK 列了它，0 個寫入者） |
//! | `POST /reservations/{id}/check-out` | `reservations.checked_out_at`（0 個寫入者） |
//! | `DELETE /reservation-series/{group}` | 系列取消 —— `idx_reservations_recurrence` 的第一個寫入路徑 |
//!
//! # check-out 為什麼**不**改 `end_at`
//!
//! 契約寫的是「提前離場並釋放時段」，而最直覺的做法是把 `end_at` 縮到現在。
//! 那會壞掉兩件事：
//!
//!   1. **它改寫了約定。** 使用者訂的是兩小時，資料庫接著說他訂了一小時。
//!   2. `report_space_utilization` 的分子是 `sum(end_at - start_at)`，
//!      也就是**已預約時數**。縮短 `end_at` 會讓那個數字悄悄變成「實際使用
//!      時數」—— 兩個意思都合理，但報表的檔頭說的是前者，而沒有人會發現
//!      定義換了。
//!
//! 不需要改也做得到「釋放時段」：005 的排除約束只在
//! `status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN')` 時生效
//! （`excl_reservations_no_overlap` 的 WHERE）。所以 `status = 'COMPLETED'`
//! 就讓那個時段可以被別人訂 —— 時間欄位一個都不用動。
//! `c_` 那一格證明這件事：check-out 之後同一個重疊時段訂得起來。
//!
//! # 釋放佔位的三種狀態要分得開
//!
//! | 佔位當前狀態 | 回應 | 理由 |
//! |---|---|---|
//! | `ACTIVE` | 204 | 正常釋放 |
//! | `EXPIRED`／`RELEASED` | 204 | **幂等** —— 呼叫者要的是「這個時段不再被我佔著」，而那已經成立 |
//! | `CONSUMED` | 409 | 那個佔位已經變成一筆預約；回 204 等於謊稱時段空了 |
//!
//! 把 CONSUMED 也回 204 是最容易寫的版本，而它的後果是客戶端以為釋放成功、
//! 接著訂同一個時段、拿到一個排除約束的衝突錯誤 —— 而錯誤裡不會提到那筆
//! 已經存在的預約。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, permission_codes, require_permission, Caller, FieldError, Problem, ProblemCode,
};

use crate::handlers::ReservationState;

// =============================================================================
// GET /facilities/{facilityId}/bookable-resources
// =============================================================================

#[derive(Debug, Serialize)]
pub struct BookableResourceDto {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub resource_type: String,
    /// `SPATIAL_NODE` 時是節點、`ASSET` 時是設備。005 的 `ck_bookable_target`
    /// 保證恰好有一個非 NULL，所以這裡合成一個欄位比回兩個各半空的欄位誠實。
    pub resource_id: Uuid,
    pub display_name: Option<String>,
    pub is_bookable: bool,
    // ---- 預約規則（契約說的「可預約資源與規則」）----
    pub requires_approval: bool,
    pub approver_role_code: Option<String>,
    pub requires_check_in: bool,
    pub auto_release_minutes: Option<i32>,
    pub min_duration_minutes: i32,
    pub max_duration_minutes: i32,
    pub slot_granularity_minutes: i32,
    pub buffer_before_minutes: i32,
    pub buffer_after_minutes: i32,
    pub advance_booking_days: i32,
    pub min_notice_minutes: i32,
    pub max_active_per_user: Option<i32>,
    pub capacity: i32,
    pub opening_hours: serde_json::Value,
    pub attributes: serde_json::Value,
}

/// `GET /facilities/{facilityId}/bookable-resources`
///
/// 需要 `reservation:read`（**指定場域** —— 這是場域層級的資料，
/// 而權限本身是 FACILITY 範圍的）。
///
/// 預設只回 `is_bookable` 的資源：一個關掉的資源出現在預約畫面上只會讓人
/// 白填一次表單。要看全部帶 `include_unbookable=true` —— 管理設定的畫面需要它。
pub async fn list_bookable_resources(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<ListBookableQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let include_unbookable = q.include_unbookable.unwrap_or(false);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "reservation:read", Some(facility_id), None).await?;

    let rows: Vec<BookableResourceDto> = sqlx::query_as!(
        BookableResourceDto,
        r#"SELECT br.id, br.facility_id, br.resource_type,
                  coalesce(br.spatial_node_id, br.asset_id) AS "resource_id!",
                  br.display_name::text AS display_name,
                  br.is_bookable, br.requires_approval,
                  br.approver_role_code::text AS approver_role_code,
                  br.requires_check_in, br.auto_release_minutes,
                  br.min_duration_minutes, br.max_duration_minutes,
                  br.slot_granularity_minutes, br.buffer_before_minutes,
                  br.buffer_after_minutes, br.advance_booking_days,
                  br.min_notice_minutes, br.max_active_per_user, br.capacity,
                  br.opening_hours, br.attributes
             FROM fms.bookable_resources br
            WHERE br.facility_id = $1
              AND ($2 OR br.is_bookable)
            ORDER BY br.display_name, br.id"#,
        facility_id,
        include_unbookable,
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    let unbookable = rows.iter().filter(|r| !r.is_bookable).count();
    Ok(Json(serde_json::json!({
        "data": rows,
        "meta": {
            "include_unbookable": include_unbookable,
            // 有幾個是關掉的。預設不含，所以這個數字只有帶了旗標才會 > 0。
            "unbookable_count": unbookable,
            // `opening_hours` 為 `{}` 代表沿用場域的營運時間 —— 那是
            // `report_space_utilization` 的 `hours_basis` 會回報
            // `facility.operating_hours` 的那些資源。形狀見 migration 038。
            "opening_hours_shape": "{\"mon\": [[\"08:00\",\"21:00\"]], …}；{} = 沿用場域",
        },
    })))
}

#[derive(Debug, Deserialize)]
pub struct ListBookableQuery {
    pub include_unbookable: Option<bool>,
}

// =============================================================================
// PATCH /bookable-resources/{id}
// =============================================================================

/// 只有預約規則可改。
///
/// **`resource_type`／`spatial_node_id`／`asset_id` 不在裡面**：改那三個等於
/// 把這個資源指向另一個實體，而既有的預約會跟著指過去（`reservations`
/// 存的是 `bookable_resource_id`）。要換標的就建一個新資源、把舊的
/// `is_bookable = false`。與 `/tenant` 那次同一個判斷：分不清「不能改」與
/// 「沒有這個欄位」會讓使用者問錯問題，所以未知欄位一律拒絕。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchBookableRequest {
    pub display_name: Option<String>,
    pub is_bookable: Option<bool>,
    pub requires_approval: Option<bool>,
    pub requires_check_in: Option<bool>,
    pub min_duration_minutes: Option<i32>,
    pub max_duration_minutes: Option<i32>,
    pub slot_granularity_minutes: Option<i32>,
    pub buffer_before_minutes: Option<i32>,
    pub buffer_after_minutes: Option<i32>,
    pub advance_booking_days: Option<i32>,
    pub min_notice_minutes: Option<i32>,
    pub capacity: Option<i32>,
    pub opening_hours: Option<serde_json::Value>,
    pub attributes: Option<serde_json::Value>,
    #[serde(default, with = "crate::tail::double_option")]
    pub auto_release_minutes: Option<Option<i32>>,
    #[serde(default, with = "crate::tail::double_option")]
    pub approver_role_code: Option<Option<String>>,
    #[serde(default, with = "crate::tail::double_option")]
    pub max_active_per_user: Option<Option<i32>>,
}

/// `Option<Option<T>>` 的 serde 支援：外層區分「有沒有這個鍵」，
/// 內層是值本身。`auto_release_minutes` 想清空（＝不自動釋放）要送 `null`，
/// 而 serde 分不出「沒提供」與「提供 null」。
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

/// `PATCH /bookable-resources/{resourceId}`
pub async fn patch_bookable_resource(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Json<serde_json::Value>,
) -> Result<Json<BookableResourceDto>, Problem> {
    let obj = body
        .0
        .as_object()
        .ok_or_else(|| Problem::validation("請求體必須是一個 JSON 物件"))?;
    if obj.is_empty() {
        return Err(Problem::validation(
            "沒有要更新的欄位 —— 空的 PATCH 不會有任何效果",
        ));
    }
    // 換標的的三個欄位單獨給訊息，理由見 `PatchBookableRequest`。
    for f in [
        "resource_type",
        "spatial_node_id",
        "asset_id",
        "facility_id",
    ] {
        if obj.contains_key(f) {
            return Err(
                Problem::validation(format!("`{f}` 不可變更")).with_errors(vec![FieldError {
                    pointer: format!("/{f}"),
                    code: "IMMUTABLE".to_string(),
                    message: format!(
                        "改 `{f}` 等於把這個資源指向另一個實體，而既有的預約會跟著指過去。\
                         要換標的請建立新資源並把舊的 is_bookable 設為 false"
                    ),
                }]),
            );
        }
    }

    let req: PatchBookableRequest = serde_json::from_value(body.0.clone()).map_err(|e| {
        Problem::validation(format!("不認識的欄位或型別不符：{e}")).with_errors(vec![FieldError {
            pointer: "/".to_string(),
            code: "UNKNOWN_FIELD".to_string(),
            message: e.to_string(),
        }])
    })?;

    // 正數檢查在應用層先做：005 沒有給這些欄位 CHECK，所以一個 `-30` 會被
    // 寫進去，然後在算時段的時候變成一個沒有人看得懂的結果。
    for (name, v) in [
        ("min_duration_minutes", req.min_duration_minutes),
        ("max_duration_minutes", req.max_duration_minutes),
        ("slot_granularity_minutes", req.slot_granularity_minutes),
        ("capacity", req.capacity),
    ] {
        if let Some(v) = v {
            if v < 1 {
                return Err(
                    Problem::validation(format!("`{name}` 必須 >= 1")).with_errors(vec![
                        FieldError {
                            pointer: format!("/{name}"),
                            code: "RANGE".to_string(),
                            message: format!("{v} 不合"),
                        },
                    ]),
                );
            }
        }
    }
    for (name, v) in [
        ("buffer_before_minutes", req.buffer_before_minutes),
        ("buffer_after_minutes", req.buffer_after_minutes),
        ("advance_booking_days", req.advance_booking_days),
        ("min_notice_minutes", req.min_notice_minutes),
    ] {
        if let Some(v) = v {
            if v < 0 {
                return Err(Problem::validation(format!("`{name}` 不得為負")));
            }
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 場域從那一列讀出來，所以權限檢查要先查一次。RLS 已經把不可見的列排除，
    // 因此查不到就是 404。
    let facility_id: Option<Uuid> = sqlx::query_scalar!(
        "SELECT facility_id FROM fms.bookable_resources WHERE id = $1",
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;
    let facility_id = facility_id.ok_or_else(|| Problem::not_found("找不到這個可預約資源"))?;
    require_permission(&mut tx, "bookable_resource:write", Some(facility_id), None).await?;

    let (arm_given, arm) = split(req.auto_release_minutes);
    let (arc_given, arc) = split(req.approver_role_code);
    let (mapu_given, mapu) = split(req.max_active_per_user);

    let row: BookableResourceDto = sqlx::query_as!(
        BookableResourceDto,
        r#"UPDATE fms.bookable_resources SET
             display_name             = coalesce($2, display_name),
             is_bookable              = coalesce($3, is_bookable),
             requires_approval        = coalesce($4, requires_approval),
             requires_check_in        = coalesce($5, requires_check_in),
             min_duration_minutes     = coalesce($6, min_duration_minutes),
             max_duration_minutes     = coalesce($7, max_duration_minutes),
             slot_granularity_minutes = coalesce($8, slot_granularity_minutes),
             buffer_before_minutes    = coalesce($9, buffer_before_minutes),
             buffer_after_minutes     = coalesce($10, buffer_after_minutes),
             advance_booking_days     = coalesce($11, advance_booking_days),
             min_notice_minutes       = coalesce($12, min_notice_minutes),
             capacity                 = coalesce($13, capacity),
             opening_hours            = coalesce($14, opening_hours),
             attributes               = coalesce($15, attributes),
             auto_release_minutes = CASE WHEN $16 THEN $17 ELSE auto_release_minutes END,
             approver_role_code   = CASE WHEN $18 THEN $19 ELSE approver_role_code END,
             max_active_per_user  = CASE WHEN $20 THEN $21 ELSE max_active_per_user END,
             updated_at = clock_timestamp()
           WHERE id = $1
           RETURNING id, facility_id, resource_type,
                     coalesce(spatial_node_id, asset_id) AS "resource_id!",
                     display_name::text AS display_name,
                     is_bookable, requires_approval,
                     approver_role_code::text AS approver_role_code,
                     requires_check_in, auto_release_minutes,
                     min_duration_minutes, max_duration_minutes,
                     slot_granularity_minutes, buffer_before_minutes,
                     buffer_after_minutes, advance_booking_days,
                     min_notice_minutes, max_active_per_user, capacity,
                     opening_hours, attributes"#,
        id,
        req.display_name.as_deref(),
        req.is_bookable,
        req.requires_approval,
        req.requires_check_in,
        req.min_duration_minutes,
        req.max_duration_minutes,
        req.slot_granularity_minutes,
        req.buffer_before_minutes,
        req.buffer_after_minutes,
        req.advance_booking_days,
        req.min_notice_minutes,
        req.capacity,
        req.opening_hours,
        req.attributes,
        arm_given,
        arm,
        arc_given,
        arc.as_deref(),
        mapu_given,
        mapu,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(map_bookable_violation)?;
    tx.commit().await?;

    Ok(Json(row))
}

fn split<T>(v: Option<Option<T>>) -> (bool, Option<T>) {
    match v {
        None => (false, None),
        Some(inner) => (true, inner),
    }
}

/// 把資料庫的約束違反翻譯成 422。
///
/// 兩個會踩到的：`ck_bookable_duration`（max >= min）與
/// `ck_bookable_opening_hours`（065 加的形狀約束）。兩者都是使用者送錯值，
/// 不是伺服器壞了 —— 500 會讓客戶端重試一個永遠不會成功的請求。
fn map_bookable_violation(e: sqlx::Error) -> Problem {
    match e.as_database_error().and_then(|d| d.constraint()) {
        Some("ck_bookable_duration") => Problem::validation(
            "`max_duration_minutes` 必須 >= `min_duration_minutes` —— \
             只送其中一個時，另一個仍是資料庫裡的現值",
        )
        .with_errors(vec![FieldError {
            pointer: "/max_duration_minutes".to_string(),
            code: "RANGE".to_string(),
            message: "與現有的 min_duration_minutes 衝突".to_string(),
        }]),
        Some("ck_bookable_opening_hours") => Problem::validation(
            "`opening_hours` 的形狀不合：星期鍵（mon…sun）→ [[\"08:00\",\"21:00\"], …]，\
             結束必須晚於開始（見 migration 038）",
        )
        .with_errors(vec![FieldError {
            pointer: "/opening_hours".to_string(),
            code: "SHAPE".to_string(),
            message: "形狀由 fms.operating_hours_are_valid 驗".to_string(),
        }]),
        _ => Problem::from(e),
    }
}

// =============================================================================
// GET /amenities、GET/PUT /bookable-resources/{id}/amenities
// =============================================================================
//
// `fms.amenities`／`fms.resource_amenities`（011）建好之後一直沒有 API
// 接上——前端只能借用 `bookable_resources.attributes` 這個自由格式欄位塞
// `{"amenities": [...]}`，鍵名跟形狀全靠約定，換個團隊接手就可能對不起來。
// 011 的表註解說得很明白：故意做成 join table 而非 `text[]`，就是為了讓
// 設施篩選、i18n（`name_en`）與「投影機故障→暫不可用」的連動能一致處理，
// 這裡把它接上，不重新發明一套。
//
// RLS 已經在 011 自己的動態套用區塊裡處理過（`catalog_tables := ARRAY
// ['amenities']`），跟 007 對 `spatial_node_types` 的做法是同一套雙政策，
// 不需要再補一支 migration。

#[derive(Debug, Serialize)]
pub struct AmenityDto {
    pub id: Uuid,
    pub is_platform: bool,
    pub code: String,
    pub name: String,
    pub name_en: Option<String>,
    pub category: String,
    pub icon: Option<String>,
}

/// `GET /amenities`
///
/// 附屬設備目錄（平台預設 + 租戶自訂），給前端建立篩選器／勾選清單用。
/// 跟 `spatial_node_types`（003）同一個模式：平台列（`tenant_id IS NULL`）
/// 與租戶自訂一起回，標上 `is_platform`。
///
/// 權限刻意比 `GET .../bookable-resources`（`reservation:read`）寬：
/// REQUESTER 只有 `reservation:create`／`read_own`／`update`，沒有
/// `reservation:read`（見 008 的種子）——但一個要訂會議室的人正需要先看到
/// 「有哪些設備可以篩選」，擋住目錄對他們沒有保護到任何東西，只會讓
/// 訂房畫面的篩選器空白。因此這裡接受任一個「跟預約沾得上邊」的權限碼。
pub async fn list_amenities(
    State(state): State<ReservationState>,
    caller: Caller,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let codes = permission_codes(&mut tx, None, None).await?;
    let allowed = [
        "reservation:read",
        "reservation:read_own",
        "reservation:create",
    ];
    if !allowed.iter().any(|c| codes.contains(*c)) {
        return Err(Problem::permission_denied(format!(
            "missing permission: one of {allowed:?}"
        )));
    }

    let rows: Vec<AmenityDto> = sqlx::query_as!(
        AmenityDto,
        r#"SELECT id, tenant_id IS NULL AS "is_platform!",
                  code::text AS "code!", name::text AS "name!",
                  name_en::text AS name_en, category::text AS "category!",
                  icon::text AS icon
             FROM fms.amenities
            WHERE is_active
            ORDER BY display_order, code"#
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": rows })))
}

#[derive(Debug, Serialize)]
pub struct ResourceAmenityDto {
    pub amenity_id: Uuid,
    pub code: String,
    pub name: String,
    pub name_en: Option<String>,
    pub category: String,
    pub icon: Option<String>,
    pub quantity: i16,
    pub is_operational: bool,
    pub note: Option<String>,
}

async fn require_bookable_resource_facility(
    tx: &mut fms_shared::TenantTx,
    resource_id: Uuid,
) -> Result<Uuid, Problem> {
    sqlx::query_scalar!(
        "SELECT facility_id FROM fms.bookable_resources WHERE id = $1",
        resource_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("找不到這個可預約資源"))
}

/// `GET /bookable-resources/{resourceId}/amenities`
///
/// 權限比照 `list_amenities`（見該函式的說明——REQUESTER 訂房前要看得到
/// 這間房有什麼設備）：任一個「跟預約沾得上邊」的權限碼即可，場域範圍限定。
pub async fn list_resource_amenities(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(resource_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let facility_id = require_bookable_resource_facility(&mut tx, resource_id).await?;
    let codes = permission_codes(&mut tx, Some(facility_id), None).await?;
    let allowed = [
        "reservation:read",
        "reservation:read_own",
        "reservation:create",
    ];
    if !allowed.iter().any(|c| codes.contains(*c)) {
        return Err(Problem::permission_denied(format!(
            "missing permission: one of {allowed:?}"
        )));
    }

    let rows: Vec<ResourceAmenityDto> = sqlx::query_as!(
        ResourceAmenityDto,
        r#"SELECT ra.amenity_id,
                  a.code::text AS "code!", a.name::text AS "name!",
                  a.name_en::text AS name_en, a.category::text AS "category!",
                  a.icon::text AS icon,
                  ra.quantity AS "quantity!", ra.is_operational AS "is_operational!",
                  ra.note::text AS note
             FROM fms.resource_amenities ra
             JOIN fms.amenities a ON a.id = ra.amenity_id
            WHERE ra.bookable_resource_id = $1
            ORDER BY a.display_order, a.code"#,
        resource_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": rows })))
}

#[derive(Debug, Deserialize)]
pub struct PutResourceAmenity {
    pub amenity_id: Uuid,
    #[serde(default = "default_amenity_quantity")]
    pub quantity: i16,
    pub note: Option<String>,
}

fn default_amenity_quantity() -> i16 {
    1
}

#[derive(Debug, Deserialize)]
pub struct PutResourceAmenitiesRequest {
    /// 完整覆寫——同 `ReservationUpdate.services` 的語意：送一次全量清單，
    /// 不是逐項增減。空陣列合法，代表清空。
    pub amenities: Vec<PutResourceAmenity>,
}

/// `PUT /bookable-resources/{resourceId}/amenities`
///
/// 權限同 `PATCH /bookable-resources/{resourceId}`（`bookable_resource:write`）
/// ——改資源的附屬設備跟改其他預約規則是同一種管理動作。
pub async fn put_resource_amenities(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(resource_id): Path<Uuid>,
    Json(req): Json<PutResourceAmenitiesRequest>,
) -> Result<Json<serde_json::Value>, Problem> {
    for (i, a) in req.amenities.iter().enumerate() {
        if a.quantity < 1 {
            return Err(Problem::validation(format!(
                "amenities[{i}].quantity 必須 >= 1"
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let facility_id = require_bookable_resource_facility(&mut tx, resource_id).await?;
    require_permission(&mut tx, "bookable_resource:write", Some(facility_id), None).await?;

    // 全量覆寫：先清空、再整批寫入，兩者在同一個交易裡，不會有「清空了但
    // 新的還沒寫完」的空窗。amenity_id 是否存在（platform 或本租戶自訂）
    // 交給外鍵驗證——一個帶了不存在／別租戶 amenity_id 的請求應該 404，
    // 而不是靜默忽略那一項。
    sqlx::query!(
        "DELETE FROM fms.resource_amenities WHERE bookable_resource_id = $1",
        resource_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;

    let tenant_id = tx.context().tenant_id;
    for a in &req.amenities {
        let exists: bool = sqlx::query_scalar!(
            "SELECT EXISTS (SELECT 1 FROM fms.amenities WHERE id = $1)",
            a.amenity_id
        )
        .fetch_one(tx.conn())
        .await
        .map_err(Problem::from)?
        .unwrap_or(false);
        if !exists {
            return Err(Problem::not_found(format!(
                "amenity_id {} 不存在（或不屬於這個租戶）",
                a.amenity_id
            )));
        }

        sqlx::query!(
            r#"INSERT INTO fms.resource_amenities
                 (bookable_resource_id, amenity_id, tenant_id, quantity, note)
               VALUES ($1, $2, $3, $4, $5)"#,
            resource_id,
            a.amenity_id,
            tenant_id,
            a.quantity,
            a.note,
        )
        .execute(tx.conn())
        .await
        .map_err(Problem::from)?;
    }
    tx.commit().await?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let rows: Vec<ResourceAmenityDto> = sqlx::query_as!(
        ResourceAmenityDto,
        r#"SELECT ra.amenity_id,
                  a.code::text AS "code!", a.name::text AS "name!",
                  a.name_en::text AS name_en, a.category::text AS "category!",
                  a.icon::text AS icon,
                  ra.quantity AS "quantity!", ra.is_operational AS "is_operational!",
                  ra.note::text AS note
             FROM fms.resource_amenities ra
             JOIN fms.amenities a ON a.id = ra.amenity_id
            WHERE ra.bookable_resource_id = $1
            ORDER BY a.display_order, a.code"#,
        resource_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": rows })))
}

// =============================================================================
// DELETE /reservations/holds/{token}
// =============================================================================

/// `DELETE /reservations/holds/{holdToken}`
///
/// 需要 `reservation:create`（持有佔位的人就是要建立預約的人），
/// **並且必須是自己的佔位** —— 釋放別人的佔位等於把他的時段搶走。
///
/// 三種狀態的回應見模組檔頭。這一支是 `status = 'RELEASED'` 的第一個寫入者：
/// 005 的 CHECK 從一開始就列了那個值，而在此之前沒有任何程式碼寫得出它。
pub async fn release_hold(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(token): Path<String>,
) -> Result<StatusCode, Problem> {
    if token.is_empty() || token.len() > 64 {
        return Err(Problem::validation("`holdToken` 長度不合"));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // 條件式 UPDATE + 一次回報「原本是什麼狀態」，好讓下面分得出
    // 204／403／409。分兩個語句查再改會留一個窗，而佔位的重點正是併發。
    let row = sqlx::query!(
        r#"WITH target AS (
             -- 別名不加 `!`：sqlx 不改寫 SQL，`AS "x!"` 會讓 Postgres 真的
             -- 建一個叫 `x!` 的欄位，於是 CTE 內部的 `t.x` 找不到它。
             -- `!`（非空覆寫）只寫在最外層的投影上。
             SELECT h.id, h.status, h.user_id, h.facility_id,
                    h.user_id = fms.current_user_id() AS is_mine
               FROM fms.reservation_holds h
              WHERE h.hold_token = $1
           ), released AS (
             UPDATE fms.reservation_holds h
                SET status = 'RELEASED'
               FROM target t
              WHERE h.id = t.id AND t.is_mine AND t.status = 'ACTIVE'
             RETURNING h.id
           )
           SELECT t.status, t.is_mine AS "is_mine!", t.facility_id,
                  EXISTS (SELECT 1 FROM released) AS "released!"
             FROM target t"#,
        token,
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;

    // 找不到 token → 404。不區分「不存在」與「不是你的」以外的情況：
    // token 是不可猜的隨機字串，所以這裡沒有列舉的風險。
    let Some(row) = row else {
        return Err(Problem::not_found("找不到這個佔位"));
    };

    require_permission(&mut tx, "reservation:create", Some(row.facility_id), None).await?;

    if !row.is_mine {
        return Err(Problem::permission_denied(
            "只能釋放自己的佔位 —— 釋放別人的等於把他的時段搶走",
        ));
    }
    if row.released {
        tx.commit().await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    match row.status.as_str() {
        // **幂等**：呼叫者要的是「這個時段不再被我佔著」，而那已經成立。
        "EXPIRED" | "RELEASED" => {
            tx.commit().await?;
            Ok(StatusCode::NO_CONTENT)
        }
        // 那個佔位已經變成一筆預約。回 204 等於謊稱時段空了 ——
        // 客戶端接著會訂同一個時段，然後拿到一個沒有提到那筆預約的排除約束錯誤。
        "CONSUMED" => Err(Problem::new(ProblemCode::Conflict)
            .with_detail("這個佔位已經被用來建立預約了，無法釋放。要放掉那個時段請取消那筆預約")),
        other => Err(Problem::new(ProblemCode::Conflict)
            .with_detail(format!("狀態為 {other} 的佔位無法釋放"))),
    }
}

// =============================================================================
// POST /reservations/{id}/check-out
// =============================================================================

/// `POST /reservations/{reservationId}/check-out`
///
/// 使用者本人（主辦人或代訂對象）；其他人需要 `reservation:update`
/// —— 與 `check_in` 完全相同的判斷，因為離場與報到是同一個人的動作。
///
/// **只有 `CHECKED_IN` 能離場。** 沒報到就離場在語意上不成立，而更重要的是
/// 它會讓 `checked_out_at IS NOT NULL AND checked_in_at IS NULL` 這種列存在
/// —— 之後任何算「實際使用時長」的東西都要處理那個不可能的組合。
///
/// 狀態轉成 `COMPLETED`，時間欄位一個都不動。理由見模組檔頭。
pub async fn check_out(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let current = crate::repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這筆預約"))?;

    if current.organizer_id != caller.user_id && current.on_behalf_of_id != Some(caller.user_id) {
        require_permission(
            &mut tx,
            "reservation:update",
            Some(current.facility_id),
            None,
        )
        .await?;
    }

    let updated = sqlx::query!(
        r#"UPDATE fms.reservations
              SET status = 'COMPLETED',
                  checked_out_at = clock_timestamp()
            WHERE id = $1 AND status = 'CHECKED_IN'
          RETURNING checked_in_at, checked_out_at,
                    extract(epoch FROM (clock_timestamp() - checked_in_at))::float8 / 60.0
                      AS "used_minutes!",
                    extract(epoch FROM (end_at - start_at))::float8 / 60.0
                      AS "booked_minutes!""#,
        id,
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;

    let Some(u) = updated else {
        return Err(Problem::new(ProblemCode::Conflict).with_detail(format!(
            "只有已報到（CHECKED_IN）的預約可以離場，這一筆是 {}",
            current.status
        )));
    };
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": {
            "reservation_id": id,
            "status": "COMPLETED",
            "checked_in_at": u.checked_in_at,
            "checked_out_at": u.checked_out_at,
        },
        "meta": {
            // 實際用了多久 vs 訂了多久。**兩者都回，而且不互相取代** ——
            // 時間欄位沒有被改寫，所以 booked 仍然是當初的約定。
            "used_minutes": (u.used_minutes * 10.0).round() / 10.0,
            "booked_minutes": (u.booked_minutes * 10.0).round() / 10.0,
            // 時段現在可以被別人訂了，而這是狀態改變的結果不是時間改變的結果。
            "slot_released": true,
            "slot_released_by": "status → COMPLETED（excl_reservations_no_overlap 只約束 PENDING_APPROVAL／CONFIRMED／CHECKED_IN）",
        },
    })))
}

// =============================================================================
// DELETE /reservation-series/{recurrenceGroupId}
// =============================================================================

/// `DELETE /reservation-series/{recurrenceGroupId}`
///
/// 需要 `reservation:update`，或者是系列的主辦人本人。
///
/// # 只取消還沒開始的
///
/// 已經發生過的預約是歷史，取消它不會讓那段時間回來，而它會讓
/// `report_space_utilization` 的過去區間數字改變 —— 一個「取消整個系列」的
/// 動作不該回頭改寫上個月的使用率報表。
///
/// # 三個數字分開回
///
/// `cancelled`／`skipped_past`／`skipped_terminal`。只回「取消了 N 筆」的話，
/// 一個全部都已經結束的系列會回 0，而那與「找不到這個系列」長得一樣 ——
/// 前者該回 200 加三個數字，後者該回 404。
pub async fn cancel_series(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(group_id): Path<Uuid>,
    body: Option<Json<CancelSeriesRequest>>,
) -> Result<Json<serde_json::Value>, Problem> {
    let reason = body.and_then(|Json(b)| b.reason);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // 先確認系列存在且看得見（RLS 已收斂），並取出場域與主辦人。
    // 一個系列理論上跨場域是可能的（recurrence_group_id 沒有那個約束），
    // 所以權限對**每一個涉及的場域**都要有 —— 用 max(facility) 檢查一次
    // 是不夠的。這裡取全部場域逐一檢查。
    let facilities: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT DISTINCT facility_id FROM fms.reservations WHERE recurrence_group_id = $1",
        group_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;
    if facilities.is_empty() {
        return Err(Problem::not_found("找不到這個週期系列"));
    }

    let all_mine: bool = sqlx::query_scalar!(
        r#"SELECT bool_and(organizer_id = fms.current_user_id()) AS "all_mine!"
             FROM fms.reservations WHERE recurrence_group_id = $1"#,
        group_id
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;

    if !all_mine {
        // 不是全部都是自己的 → 需要權限，而且每一個場域都要有。
        // 只檢查第一個場域會讓一個跨場域的系列被只有單一場域權限的人取消。
        for f in &facilities {
            require_permission(&mut tx, "reservation:update", Some(*f), None).await?;
        }
    }

    let counts = sqlx::query!(
        r#"WITH classified AS (
             SELECT r.id,
                    r.start_at <= clock_timestamp() AS is_past,
                    r.status NOT IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN') AS is_terminal
               FROM fms.reservations r
              WHERE r.recurrence_group_id = $1
           ), cancelled AS (
             UPDATE fms.reservations r
                SET status = 'CANCELLED',
                    cancelled_at = clock_timestamp(),
                    cancelled_by = fms.current_user_id(),
                    cancellation_reason = $2
               FROM classified c
              WHERE r.id = c.id AND NOT c.is_past AND NOT c.is_terminal
             RETURNING r.id
           )
           SELECT (SELECT count(*) FROM cancelled) AS "cancelled!",
                  count(*) FILTER (WHERE c.is_past) AS "skipped_past!",
                  count(*) FILTER (WHERE NOT c.is_past AND c.is_terminal)
                    AS "skipped_terminal!",
                  count(*) AS "total!"
             FROM classified c"#,
        group_id,
        reason.as_deref(),
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": {
            "recurrence_group_id": group_id,
            "cancelled": counts.cancelled,
            // **已經開始的不取消。** 取消它不會讓那段時間回來，而它會回頭
            // 改寫過去區間的使用率報表。
            "skipped_past": counts.skipped_past,
            // 已經取消／已完成／已 no-show 的。
            "skipped_terminal": counts.skipped_terminal,
            "total_in_series": counts.total,
        },
        "meta": {
            "facilities_touched": facilities.len(),
            "cancelled_zero_is_valid": counts.cancelled == 0,
        },
    })))
}

#[derive(Debug, Deserialize)]
pub struct CancelSeriesRequest {
    pub reason: Option<String>,
}
