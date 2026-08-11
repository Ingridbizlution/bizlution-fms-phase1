//! 預約端點。本切片實作契約中的四支：
//! `GET /reservations`、`POST /reservations`、`GET /reservations/{id}`、
//! `PATCH /reservations/{id}`。
//!
//! 選這四支是因為它們合起來剛好帶出全部四個橫切關注點：
//! cursor 分頁、Idempotency-Key、If-Match 樂觀鎖、409 衝突映射。
//! `holds`／`check-in`／核准／即時佔用、`hold_token` 消耗、附加服務、
//! 週期展開與 `participants` 後續都補上了。仍待補的是「服務 → 工單」的
//! fan-out worker —— 沒有任何程式碼消費 `reservation.confirmed`，因此
//! `services[].work_order` 恆為 null。

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, concurrency, deny_unless_own, has_permission, page,
    permission_codes, read_scope, require_permission, Caller, Cursor, FieldError, OptionalJson,
    Paged, Problem, ProblemCode, SortSpec, TenantTx,
};

use crate::dto::{self, *};
use crate::repo;

#[derive(Clone)]
pub struct ReservationState {
    pub pool: PgPool,
}

const ENDPOINT: &str = "POST /reservations";

/// 私人預約的可見性判定。
///
/// # 為什麼需要這個型別
///
/// 011 為 `reservations.is_private` 寫下的規格是：
///
/// > 私人預約：非本人／非管理員只看得到「已預約」與時段，看不到標題、
/// > 與會者與備註。由 API 層遮罩。
///
/// 而**在此之前 API 層完全沒有讀這一欄** —— `is_private` 在整個 Rust 樹裡
/// 沒有任何出現。也就是說使用者把預約標記為私人之後，標題、備註與主辦人姓名
/// 對所有持有 `reservation:read` 的人照樣全開，包含牆面板上的佔用地圖。
/// `reservation:view_private` 是一個已經被四個管理角色持有、卻守不住任何
/// 東西的權限。
///
/// 判定條件刻意只有兩項，而且兩項都不寫死在程式碼裡：
///
///   * **本人** —— 與 `deny_unless_own` 用同一組欄位（`organizer_id` 與
///     `on_behalf_of_id`）。代訂的預約對被代訂的人是「本人的」，這個定義
///     在這個 crate 裡已經存在，不該在這裡發明第二套。
///   * **持有 `reservation:view_private`** —— 誰算「管理員」由權限資料決定，
///     不是由這裡的一份角色清單決定。目前是 PLATFORM_ADMIN／TENANT_ADMIN／
///     ORG_MANAGER／FACILITY_ADMIN，而那是 seed 的事，改它不必動這段程式碼。
#[derive(Debug, Clone, Copy)]
struct PrivateView {
    may_view: bool,
    me: Uuid,
}

impl PrivateView {
    /// `facility_id` 為 `None` 時問的是「在任何範圍持有」——
    /// 與 `read_scope` 在跨場域清單上的作法一致（026 的取捨）。
    async fn resolve(tx: &mut TenantTx, facility_id: Option<Uuid>) -> Result<Self, Problem> {
        let me = tx.context().user_id;
        // `permission_codes` 是請求級快取（見 fms-shared::db），因此這裡
        // 不會多一次往返 —— `read_scope` 已經以同一個鍵取過整組權限。
        let may_view = permission_codes(tx, facility_id, None)
            .await?
            .contains("reservation:view_private");
        Ok(Self { may_view, me })
    }

    /// 這一列要不要遮罩。
    fn masks(&self, r: &repo::ReservationRow) -> bool {
        self.masks_raw(r.is_private, Some(r.organizer_id), r.on_behalf_of_id)
    }

    /// 同一套判定，但吃原始欄位而非整列 `ReservationRow` —— 給
    /// `availability()` 的 `busy[]` 用：那些列不是每一列都是預約
    /// （HOLD／BLACKOUT 沒有主辦人），`organizer_id` 因此是 `Option`。
    fn masks_raw(
        &self,
        is_private: bool,
        organizer_id: Option<Uuid>,
        on_behalf_of_id: Option<Uuid>,
    ) -> bool {
        is_private
            && !self.may_view
            && organizer_id != Some(self.me)
            && on_behalf_of_id != Some(self.me)
    }
}

/// `is_private` 為真且無權查看時，遮掉 011 指名的三樣東西。
///
/// **`start_at`／`end_at`／`status` 刻意不遮** —— 那正是規格說「看得到」的
/// 部分，也是可用性的來源：看不到時段的行事曆沒有意義。
///
/// 沒有遮的殘留，寫出來以免下一個人以為已經全包：`party_size`（會議人數）、
/// `reservation_no`（單號）與 `resource_id`。011 沒有提到它們，而它們對
/// 「這個時段有人用」這件事是必要資訊。
fn to_dto(r: repo::ReservationRow, view: &PrivateView) -> ReservationDto {
    let masked = view.masks(&r);
    ReservationDto {
        id: r.id,
        reservation_no: r.reservation_no,
        facility_id: r.facility_id,
        resource_id: r.resource_id,
        resource_name: r.resource_name,
        resource_type: r.resource_type,
        title: if masked { None } else { r.title },
        purpose: if masked { None } else { r.purpose },
        party_size: r.party_size,
        start_at: r.start_at,
        end_at: r.end_at,
        status: r.status,
        organizer: if masked {
            None
        } else {
            Some(OrganizerDto {
                id: r.organizer_id,
                display_name: r.organizer_display_name,
            })
        },
        approval_required: r.approval_required,
        requires_check_in: r.requires_check_in,
        checked_in_at: r.checked_in_at,
        auto_release_at: r.auto_release_at,
        recurrence_group_id: r.recurrence_group_id,
        created_via: r.created_via,
        version: r.version,
        is_private: r.is_private,
    }
}

/// `GET /reservations`
///
/// 讀取範圍由權限決定（WBS 3.9）：有 `reservation:read` 看全部，
/// 只有 `reservation:read_own` 就強制只看自己的，兩者都沒有回 403。
/// 這個分流刻意在應用層做而非交給 RLS：RLS 是租戶邊界，
/// 「看得到同租戶的誰的預約」是授權問題，權威在 `user_has_permission`。
///
/// 先前的實作是「`mine=true` 就跳過權限檢查」，那讓
/// `reservation:read_own` 完全成為裝飾 —— 沒有任何預約權限的使用者
/// 只要加上 `mine=true` 就能讀。現在 `mine` 只是**收窄**條件，不再是繞道。
pub async fn list(
    State(state): State<ReservationState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<Paged<ReservationDto>>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let scope = read_scope(
        &mut tx,
        "reservation:read",
        "reservation:read_own",
        q.facility_id,
    )
    .await?;
    let organizer_filter = match scope.own_user_id() {
        // 權限只允許自己的：客戶端無法用參數放寬
        Some(me) => Some(me),
        None if q.mine => Some(caller.user_id),
        None => q.organizer_id,
    };

    let limit = clamp_limit(q.limit);
    // 契約未提供 sort；固定 start_at 降冪，游標仍記下欄位以便日後擴充。
    let sort = SortSpec {
        column: repo::RESERVATION_SORT_COLUMN.to_string(),
        desc: true,
    };
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };

    let rows = repo::list(
        &mut tx,
        q.facility_id,
        q.resource_id,
        organizer_filter,
        q.status.as_deref(),
        q.from,
        q.to,
        cursor.as_ref(),
        limit,
    )
    .await?;
    // 遮罩判定要在 commit **之前**解析：它需要查權限，而那需要交易。
    let view = PrivateView::resolve(&mut tx, q.facility_id).await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, col| r.cursor_key(col));
    Ok(Json(Paged {
        data: paged.data.into_iter().map(|r| to_dto(r, &view)).collect(),
        page: paged.page,
    }))
}

/// `GET /reservations/{id}`
///
/// 回應是契約的 `ReservationDetail`：基底 `Reservation` 加上 `services` 與
/// `participants`。回傳型別因此是 `Value` 而不是 `ReservationDto` ——
/// 這兩個附加清單要併進同一個物件。
pub async fn get(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<(HeaderMap, Json<serde_json::Value>), Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let row = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("reservation not found"))?;

    let scope = read_scope(
        &mut tx,
        "reservation:read",
        "reservation:read_own",
        Some(row.facility_id),
    )
    .await?;
    deny_unless_own(
        scope,
        &[Some(row.organizer_id), row.on_behalf_of_id],
        "reservation",
    )?;

    let version = row.version;
    let view = PrivateView::resolve(&mut tx, Some(row.facility_id)).await?;
    let mut body = serde_json::to_value(to_dto(row, &view)).map_err(Problem::internal)?;
    attach_services(&mut tx, id, &mut body).await?;
    attach_participants(&mut tx, id, &mut body).await?;
    tx.commit().await?;

    // 回 ETag，讓客戶端能直接把它用在後續 PATCH 的 If-Match
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::ETAG,
        format!("\"{version}\"").parse().expect("valid etag"),
    );
    Ok((headers, Json(body)))
}

/// `POST /reservations`
///
/// 交易內的順序：冪等登記 → 授權 →（回放則到此為止）→ 可用性預檢 → 寫入
/// → 標記冪等完成。全部在同一交易，因此不會出現「冪等鍵記下了但預約沒建立」。
///
/// **登記**仍然在最前面（`IN_FLIGHT` 與鍵重用要在做任何事之前判斷），
/// 但**回放**移到授權之後：先前回放走在 `require_permission` 前面，
/// 於是命中鍵的請求完全不經授權（見 docs/security-review-open-items.md 第 1 項）。
/// 型別上由 `PendingReplay` 保證這個順序 —— 取出內容需要 `Authorized`。
///
/// 可用性預檢只為了給出可讀錯誤；**真正的權威是排他約束**（ADR-04）。
/// 因此 INSERT 仍可能以 23P01／40P01 失敗，兩者都由
/// `Problem::from(sqlx::Error)` 映射成 409 `RESERVATION_CONFLICT`。
pub async fn create(
    State(state): State<ReservationState>,
    caller: Caller,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let req: CreateReservation = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid ReservationCreate: {e}")))?;

    if req.end_at <= req.start_at {
        return Err(Problem::validation("end_at must be after start_at"));
    }

    let idem_key = concurrency::key_from(&headers)?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // 冪等**登記**要在做任何事之前
    let pending = match idem_key.as_deref() {
        Some(key) => concurrency::begin(&mut tx, key, ENDPOINT, &raw)
            .await?
            .pending(),
        None => None,
    };

    // 先解析資源所屬場域，才能用正確的範圍檢查權限。
    //
    // 這個順序是修正而非偏好：原本以 `facility_id = None` 檢查，
    // 於是只有 TENANT 範圍的角色通得過 —— `REQUESTER`、`FACILITY_ADMIN`
    // 等**所有場域範圍的角色都無法建立任何預約**，而那是絕大多數使用者。
    // `user_has_permission` 的 FACILITY 分支比對的是 `scope_id = p_facility_id`，
    // 傳 NULL 就永遠不成立。
    //
    // 回放路徑也走這一段：因此若資源在這 24 小時內被刪掉，重試會得到 404
    // 而不是回放。這是刻意的 —— 回放與首次執行要走同一道門，
    // 讓其中一條繞過資源檢查就是繞過授權範圍的來源。
    let booking = repo::find_bookable(&mut tx, req.resource_id)
        .await?
        .ok_or_else(|| Problem::not_found("resource is not bookable"))?;

    let auth = require_permission(
        &mut tx,
        "reservation:create",
        Some(booking.facility_id),
        None,
    )
    .await?;

    if let Some(replay) = pending {
        let (code, body) = replay.release(auth);
        tx.commit().await?;
        return Ok((code, Json(body)));
    }

    // 兩階段預約的第二階段。
    //
    // 排在可用性預檢**之前**：消耗失敗代表呼叫端根本不持有這個時段，
    // 那個答案比「時段衝突」更具體，而且不必先做完整的可用性判定。
    //
    // 可用性預檢仍然要跑，不能因為持有佔位就跳過 —— 排他約束只管
    // **佔位與佔位之間**，看不到 reservations。持有 `reservation:override`
    // 的人仍可能在這個窗內建立了預約。
    //
    // 失敗時整個交易回滾，佔位因此回到 `ACTIVE`：一次失敗的建立不該
    // 順手燒掉呼叫端的佔位。
    if let Some(token) = req.hold_token.as_deref() {
        let consumed = repo::consume_hold(
            &mut tx,
            token,
            booking.bookable_resource_id,
            req.start_at,
            req.end_at,
        )
        .await?;
        if !consumed {
            return Err(Problem::new(ProblemCode::Conflict).with_detail(
                "the hold is no longer usable: it may have expired, been consumed, \
                 or not cover the requested time range. Re-check availability and hold again",
            ));
        }
    }

    // 可用性預檢已移進 `expand_occurrences` → `check_all`：單筆與週期都必須
    // 逐個時段檢查，分兩處寫會讓其中一條路徑遲早漏掉。

    // 附加服務先驗證再建立預約：全部規則都通過了才動資料。
    // 交易本來就會回滾，因此這不是正確性需求 —— 但「先驗證」讓失敗路徑
    // 不必先寫再退，錯誤訊息也不會夾在一個已經半成形的預約後面。
    let services = resolve_services(&mut tx, &booking, &req).await?;

    // 與會者身分同樣先驗證：DB 的 ck_participant_identity 也會擋，但那個
    // 錯誤訊息對呼叫端不友善（看不出是哪一筆）。
    for (i, p) in req.participants.iter().enumerate() {
        if p.user_id.is_none() && p.external_email.is_none() {
            return Err(Problem::validation(format!(
                "participants[{i}]: either user_id or external_email is required"
            )));
        }
    }

    // 週期展開。沒帶 recurrence_rule 就是單筆，`occurrences` 只有一個元素，
    // 因此下面的迴圈對兩種情況是同一條路徑 —— 不必為單筆另寫一個分支。
    let occurrences = expand_occurrences(&mut tx, &booking, &req, caller.user_id).await?;
    let group = req
        .recurrence_rule
        .as_deref()
        .map(|rule| (uuid::Uuid::new_v4(), rule));

    let mut first_id = None;
    for (start_at, end_at) in &occurrences {
        let id = repo::create(
            &mut tx,
            &booking,
            req.resource_id,
            caller.user_id,
            &req,
            &repo::Occurrence {
                start_at: *start_at,
                end_at: *end_at,
                recurrence: group,
            },
        )
        .await?;
        if !services.is_empty() {
            // 服務的班表是相對於**該次**預約的開始時刻，因此每一次都要重算。
            let shifted: Vec<repo::NewReservationService> = services
                .iter()
                .map(|s| repo::NewReservationService {
                    service_item_id: s.service_item_id,
                    quantity: s.quantity,
                    payload: s.payload.clone(),
                    notes: s.notes.clone(),
                    service_start_at: *start_at + (s.service_start_at - req.start_at),
                    service_end_at: *start_at + (s.service_end_at - req.start_at),
                    status: s.status,
                    estimated_cost: s.estimated_cost,
                })
                .collect();
            repo::insert_services(&mut tx, id, &shifted).await?;
        }
        if !req.participants.is_empty() {
            repo::insert_participants(&mut tx, id, &req.participants).await?;
        }
        first_id.get_or_insert(id);
    }
    // `expand_occurrences` 保證至少一個時段，因此這裡必定有值。
    let id = first_id
        .ok_or_else(|| Problem::internal(std::io::Error::other("no occurrence was created")))?;

    let created = repo::get(&mut tx, id).await?.ok_or_else(|| {
        Problem::internal(std::io::Error::other("reservation vanished after insert"))
    })?;
    // 建立者必然是主辦人（或代訂的對象），因此這裡永遠不會遮罩 ——
    // 仍然走同一條判定，好處是「建立回應與後續 GET 形狀一致」不需要靠記憶維持。
    let view = PrivateView::resolve(&mut tx, Some(created.facility_id)).await?;
    let mut body = serde_json::to_value(to_dto(created, &view)).map_err(Problem::internal)?;
    attach_services(&mut tx, id, &mut body).await?;
    attach_participants(&mut tx, id, &mut body).await?;

    if let Some(key) = idem_key.as_deref() {
        concurrency::complete(&mut tx, key, ENDPOINT, 201, &body).await?;
    }
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(body)))
}

/// 一次請求最多展開幾個時段。
///
/// 主要上界是資源自己宣告的 `advance_booking_days`（預設 90 天），
/// 這個常數只是**記憶體護欄**：`FREQ=MINUTELY` 在 90 天內是十二萬筆，
/// 而 RRULE 沒有 `UNTIL`／`COUNT` 時是無限序列。上界必須存在，
/// 但它不該是決定「能訂多久」的那個數字 —— 那件事屬於資源設定。
const MAX_OCCURRENCES: u16 = 200;

/// 算出這次請求要建立的全部時段。
///
/// 沒帶 `recurrence_rule` 時回傳單一元素，因此呼叫端對單筆與週期是同一條路徑。
///
/// # 三個決定
///
/// **全有或全無。** 任何一次落在不可用的時段，整個請求 409。契約的回應是
/// 單一個 `ReservationDetail`，沒有欄位能表達「這 13 次裡有 2 次跳過了」——
/// 部分成功因此無法誠實回報。與其回一個看起來成功、實際有缺口的系列，
/// 不如明確拒絕並指出第一個衝突的日期。要支援部分成功得先改契約。
///
/// **視窗來自資源，且不截斷。** 上界是 `now + advance_booking_days`，那是資源
/// 自己宣告的「可提前多久預約」；應用層不另定一個數字。超出時整批拒絕而非
/// 靜默截斷 —— 少給一次而不說，與部分成功是同一類問題。
///
/// **與 `hold_token` 互斥。** 佔位鎖的是**一個**時段；用它來建立一整個系列
/// 沒有意義，而靜默只把它用在第一次會讓呼叫端以為整個系列都被保護過。
async fn expand_occurrences(
    tx: &mut fms_shared::TenantTx,
    booking: &repo::ResourceBooking,
    req: &CreateReservation,
    user_id: uuid::Uuid,
) -> Result<Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>, Problem> {
    let rule = req
        .recurrence_rule
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());

    let Some(rule) = rule else {
        // 單筆：唯一的「展開」結果就是請求本身，但仍然走下面同一個
        // 可用性迴圈，避免第一次被檢查兩遍或某一條路徑漏檢查。
        return check_all(tx, req, user_id, vec![req.start_at], false).await;
    };

    if req.hold_token.is_some() {
        return Err(Problem::validation(
            "hold_token cannot be combined with recurrence_rule: a hold covers a single \
             time range, so it cannot protect an entire series",
        ));
    }

    let timezone = repo::facility_timezone(tx, booking.facility_id)
        .await?
        .ok_or_else(|| {
            Problem::internal(std::io::Error::other("bookable resource has no facility"))
        })?;

    // 刻意**不**用 `until` 去截斷，而是展開後檢查再決定接受或整批拒絕。
    //
    // 截斷是靜默的：`COUNT=4` 若有一次落在預約窗外，呼叫端會拿到三筆而
    // 沒有任何提示 —— 那與「部分成功」同一類問題，也與本端點對衝突採取的
    // 全有或全無自相矛盾。（這個 bug 是測試抓到的：ROOM_401 的窗是 60 天，
    // 從第 40 天起算的四週系列第四次落在第 61 天，於是靜默變成三筆。）
    //
    // 多取一個是為了分辨「剛好 MAX_OCCURRENCES」與「超過」。
    let starts = fms_shared::schedule::expand(
        &fms_shared::schedule::PlanSchedule {
            rrule: rule,
            dtstart: req.start_at,
            timezone: &timezone,
        },
        MAX_OCCURRENCES + 1,
        None,
    )?;

    if starts.is_empty() {
        return Err(Problem::validation(
            "the recurrence rule produces no occurrence",
        ));
    }
    if starts.len() > usize::from(MAX_OCCURRENCES) {
        return Err(Problem::validation(format!(
            "the recurrence rule expands to more than {MAX_OCCURRENCES} occurrences; \
             bound it with UNTIL or COUNT"
        )));
    }

    // 預約窗是資源自己宣告的。落在窗外就整批拒絕並說出窗有多長。
    //
    // 沒有 UNTIL／COUNT 的規則是無限序列，因此一定會走到這裡 —— 那也是對的：
    // 60 天的窗無法承諾「每週開會直到永遠」。「開放式系列隨時間往後滾動延長」
    // 需要一個背景作業定期補建，那是獨立的功能，不在本切片內。
    let window_end =
        chrono::Utc::now() + chrono::Duration::days(i64::from(booking.advance_booking_days));
    if let Some(last) = starts.last().copied() {
        if last > window_end {
            return Err(Problem::validation(format!(
                "the series extends to {last}, beyond the {} days this resource can be booked \
                 ahead; bound the rule with UNTIL or COUNT",
                booking.advance_booking_days
            )));
        }
    }

    check_all(tx, req, user_id, starts, true).await
}

/// 對每一個時段跑可用性預檢，任何一個不通過就整批拒絕。
///
/// 可用性預檢只為了給出可讀錯誤；**真正的權威是排他約束**（ADR-04）。
/// 但對週期預約，錯誤訊息的價值高得多：約束擋下來時只會說「時段衝突」，
/// 不會說是 13 次裡的**哪一次**，而那個差別決定了呼叫端能不能自己修好請求。
async fn check_all(
    tx: &mut fms_shared::TenantTx,
    req: &CreateReservation,
    user_id: uuid::Uuid,
    starts: Vec<chrono::DateTime<chrono::Utc>>,
    is_series: bool,
) -> Result<Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>, Problem> {
    let duration = req.end_at - req.start_at;
    let mut out = Vec::with_capacity(starts.len());
    for start in starts {
        let end = start + duration;
        let availability =
            repo::check_availability(tx, req.resource_id, start, end, user_id).await?;
        if !availability.is_available {
            let kind = availability.conflict_type.unwrap_or_default();
            let detail = availability.detail.unwrap_or_else(|| {
                "resource is not available for the requested window".to_string()
            });
            // 時段衝突是 409；規則性問題（時長、預約窗）是 422 ——
            // 前者重試可能成功，後者不改請求就永遠不會成功。
            let code = match kind.as_str() {
                "RESERVATION_CONFLICT" | "HELD_BY_OTHER" | "BLACKOUT" | "DESK_ASSIGNED" => {
                    ProblemCode::ReservationConflict
                }
                _ => ProblemCode::ValidationError,
            };
            let detail = if is_series {
                format!(
                    "occurrence starting {start} is not available: {detail}. \
                     The whole series was rejected — this endpoint does not book partially."
                )
            } else {
                detail
            };
            return Err(Problem::new(code).with_detail(detail));
        }
        out.push((start, end));
    }
    Ok(out)
}

/// 把請求裡的 `services[]` 對照 `service_items` 的宣告，驗證後算出可寫入的列。
///
/// # 每一條規則都對應 schema 上的一個欄位
///
/// `service_items` 宣告了六件事，而在此之前**沒有任何一條被執行過**
/// —— 整個 `services` 陣列是被靜默忽略的。這與 `min_scope_level`
/// 是同一型缺陷（見 docs/security-review-open-items.md 第 2 項），
/// 只是這裡連「宣告了沒人讀」都還多一層：連欄位都沒人讀。
///
/// 逐條說明為什麼不能省：
///   * `is_attachable_to_reservation` —— 為 false 的項目只能獨立申請
///     （`DEEP_CLEAN` 就是）。附上去會產生一筆永遠不會被排程的服務。
///   * `facility_id`（NULL = 全場域）—— 不比對就會把影廳的清潔服務
///     掛到總部的會議室上。
///   * `lead_time_minutes` —— 前置時間不足就是承諾了做不到的服務。
///   * `max_quantity` —— 宣告的上限。
///   * `form_schema` —— 欄位註解本來就寫「validated against form_schema」。
///   * `relative_offset_minutes` / `default_duration_minutes` ——
///     不算出 `service_start_at` / `service_end_at`，服務班表就是空的，
///     清潔人員不知道幾點到。這與 `auto_release_at` 從未被填入
///     （no-show 整條斷掉）是同一種失敗。
async fn resolve_services(
    tx: &mut fms_shared::TenantTx,
    booking: &repo::ResourceBooking,
    req: &CreateReservation,
) -> Result<Vec<repo::NewReservationService>, Problem> {
    if req.services.is_empty() {
        return Ok(Vec::new());
    }

    let ids: Vec<uuid::Uuid> = req.services.iter().map(|s| s.service_item_id).collect();
    let items = repo::load_service_items(tx, &ids).await?;
    let by_id: std::collections::HashMap<uuid::Uuid, &repo::AttachableService> =
        items.iter().map(|i| (i.id, i)).collect();

    let mut out = Vec::with_capacity(req.services.len());
    for (idx, want) in req.services.iter().enumerate() {
        // 指標帶上索引：一次帶五個服務時，「payload 有錯」而不說是哪一個
        // 等於沒說。
        let at = |suffix: &str| format!("/services/{idx}{suffix}");

        let item = by_id.get(&want.service_item_id).ok_or_else(|| {
            Problem::validation(format!("unknown service_item_id: {}", want.service_item_id))
                .with_errors(vec![FieldError {
                    pointer: at("/service_item_id"),
                    code: "NOT_FOUND".into(),
                    message: "service item does not exist".into(),
                }])
        })?;

        if !item.is_attachable_to_reservation {
            return Err(Problem::validation(format!(
                "service `{}` cannot be attached to a reservation (standalone request only)",
                item.name
            ))
            .with_errors(vec![FieldError {
                pointer: at("/service_item_id"),
                code: "NOT_ATTACHABLE".into(),
                message: "is_attachable_to_reservation is false".into(),
            }]));
        }

        // facility_id 為 NULL 代表適用所有場域。
        if let Some(fac) = item.facility_id {
            if fac != booking.facility_id {
                return Err(Problem::validation(format!(
                    "service `{}` is not offered at this facility",
                    item.name
                ))
                .with_errors(vec![FieldError {
                    pointer: at("/service_item_id"),
                    code: "WRONG_FACILITY".into(),
                    message: "the service item belongs to a different facility".into(),
                }]));
            }
        }

        if want.quantity <= 0.0 {
            return Err(
                Problem::validation("quantity must be greater than zero").with_errors(vec![
                    FieldError {
                        pointer: at("/quantity"),
                        code: "MINIMUM".into(),
                        message: "quantity must be > 0".into(),
                    },
                ]),
            );
        }
        if let Some(max) = item.max_quantity {
            if want.quantity > f64::from(max) {
                return Err(Problem::validation(format!(
                    "quantity exceeds the maximum of {max} for `{}`",
                    item.name
                ))
                .with_errors(vec![FieldError {
                    pointer: at("/quantity"),
                    code: "MAXIMUM".into(),
                    message: format!("max_quantity is {max}"),
                }]));
            }
        }

        // 前置時間從**服務開始時刻**起算，不是從預約開始時刻：offset 為負的
        // 項目（例如 TEA_SETUP 提前 15 分鐘佈置）實際上更早就要動工。
        let service_start =
            req.start_at + chrono::Duration::minutes(i64::from(item.relative_offset_minutes));
        let earliest =
            chrono::Utc::now() + chrono::Duration::minutes(i64::from(item.lead_time_minutes));
        if service_start < earliest {
            return Err(Problem::validation(format!(
                "service `{}` needs {} minutes of lead time",
                item.name, item.lead_time_minutes
            ))
            .with_errors(vec![FieldError {
                pointer: at("/service_item_id"),
                code: "LEAD_TIME".into(),
                message: format!(
                    "must be requested at least {} minutes before the service starts",
                    item.lead_time_minutes
                ),
            }]));
        }

        let payload = want.payload.clone().unwrap_or(serde_json::json!({}));
        fms_shared::form_schema::validate(&item.form_schema, &payload, &at("/payload"))?;

        out.push(repo::NewReservationService {
            service_item_id: item.id,
            quantity: want.quantity,
            payload,
            notes: want.notes.clone(),
            service_start_at: service_start,
            service_end_at: service_start
                + chrono::Duration::minutes(i64::from(item.default_duration_minutes)),
            // requires_approval 的項目不能直接進 REQUESTED，否則核准這一關
            // 被靜默跳過。
            status: if item.requires_approval {
                "PENDING_APPROVAL"
            } else {
                "REQUESTED"
            },
            estimated_cost: match (item.chargeable, item.unit_price) {
                (true, Some(price)) => Some(price * want.quantity),
                _ => None,
            },
        });
    }
    Ok(out)
}

/// 把 `services` 併進 `ReservationDetail` 的回應。
///
/// 契約把它列在 `ReservationDetail` 上，因此 `POST` 與 `GET` 都該帶
/// —— 少了它，客戶端剛剛送出的服務在回應裡看不到，只能再打一次 GET。
async fn attach_services(
    tx: &mut fms_shared::TenantTx,
    reservation_id: uuid::Uuid,
    body: &mut serde_json::Value,
) -> Result<(), Problem> {
    let rows = repo::load_services(tx, reservation_id).await?;
    let dtos: Vec<dto::ReservationServiceDto> = rows
        .into_iter()
        .map(|r| dto::ReservationServiceDto {
            id: r.id,
            service_item_id: r.service_item_id,
            service_name: r.service_name,
            quantity: r.quantity.unwrap_or(0.0),
            payload: r.payload,
            service_start_at: r.service_start_at,
            status: r.status,
            // fan-out worker 尚未實作。契約標為可空，因此 null 是合法狀態。
            work_order: None,
        })
        .collect();

    let Some(obj) = body.as_object_mut() else {
        return Err(Problem::internal(std::io::Error::other(
            "reservation serialised to a non-object",
        )));
    };
    obj.insert(
        "services".into(),
        serde_json::to_value(dtos).map_err(Problem::internal)?,
    );
    Ok(())
}

async fn attach_participants(
    tx: &mut fms_shared::TenantTx,
    reservation_id: uuid::Uuid,
    body: &mut serde_json::Value,
) -> Result<(), Problem> {
    let rows = repo::load_participants(tx, reservation_id).await?;
    let dtos: Vec<dto::ParticipantDto> = rows
        .into_iter()
        .map(|r| dto::ParticipantDto {
            user_id: r.user_id,
            display_name: r.display_name,
            external_email: r.external_email,
            role: r.role,
            response: r.response,
        })
        .collect();

    let Some(obj) = body.as_object_mut() else {
        return Err(Problem::internal(std::io::Error::other(
            "reservation serialised to a non-object",
        )));
    };
    obj.insert(
        "participants".into(),
        serde_json::to_value(dtos).map_err(Problem::internal)?,
    );
    Ok(())
}

/// `PATCH /reservations/{id}`
///
/// `If-Match` 為必填（契約），缺少回 428、不符回 412。
/// 版本比對在同一交易內完成，因此不會有「檢查通過但寫入前被改掉」的空窗。
pub async fn update(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(req): Json<UpdateReservation>,
) -> Result<Json<ReservationDto>, Problem> {
    let expected_version = concurrency::required_if_match(&headers)?;

    if let (Some(s), Some(e)) = (req.start_at, req.end_at) {
        if e <= s {
            return Err(Problem::validation("end_at must be after start_at"));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // **先鎖，再讀。** 讀出來的 `version` 要用來比對，而沒有鎖的讀取會讓
    // 兩個並發的 PATCH 讀到同一個版本、都通過比對、都寫入（lost update）。
    // 見 `concurrency::check_version` 的說明與 `concurrency_correctness_slice.rs` 的 `d_`。
    repo::lock(&mut tx, id).await?;

    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("reservation not found"))?;

    if current.organizer_id != caller.user_id {
        require_permission(
            &mut tx,
            "reservation:update",
            Some(current.facility_id),
            None,
        )
        .await?;
    }
    concurrency::check_version(expected_version, current.version)?;

    // ADR-14：時間是外部日曆同步進來的鏡像，FMS 這邊改了也會在下一輪拉入時
    // 被外部原生事件的現況覆蓋回去——只擋改期，其他欄位（標題、備註、
    // is_private）仍然是 FMS 本地的中繼資料，不受外部日曆管轄，允許照常改。
    if (req.start_at.is_some() || req.end_at.is_some())
        && matches!(current.created_via.as_str(), "OUTLOOK" | "GOOGLE")
    {
        return Err(Problem::new(ProblemCode::Conflict).with_detail(
            "this reservation's time comes from an external calendar (Outlook/Google) — \
             reschedule it there; changes made in the external calendar will sync back",
        ));
    }

    // **`is_private` 的變更需要比其他欄位更強的條件。**
    //
    // 上面那道閘門對非主辦人只要求 `reservation:update`。而把 `is_private`
    // 從 `true` 改成 `false` 的效果就是**揭露內容** —— 一個沒有
    // `reservation:view_private` 的人只要有 update 權限，就能改完旗標再讀一次，
    // 於是整個遮罩被繞過。那是一條完整的提權路徑。
    //
    // 判定重用遮罩自己的條件，而不是另寫一組：**看得到遮罩後內容的人，
    // 不能改這個旗標。** 一個定義、兩個用途 —— 兩者不可能漂移。
    //
    // 回 403 而不是靜默忽略：忽略會回 200 而旗標沒變，而呼叫端會以為改了。
    let view = PrivateView::resolve(&mut tx, Some(current.facility_id)).await?;
    if req.is_private.is_some() && view.masks(&current) {
        return Err(Problem::permission_denied(
            "只有主辦人或持有 reservation:view_private 的人可以變更 is_private —— \
             否則把旗標關掉就等於繞過遮罩讀到內容",
        ));
    }

    match req.apply_scope.as_str() {
        "THIS" => {
            repo::update(&mut tx, id, &req).await?;
        }
        "THIS_AND_FOLLOWING" | "ALL" => {
            if req.start_at.is_some() || req.end_at.is_some() {
                return Err(Problem::validation(
                    "apply_scope 非 THIS 時不能同時帶 start_at／end_at——時段是每一次 \
                     各自的，沒有「整個系列一起改時間」的單一定義",
                ));
            }
            let group_id = current.recurrence_group_id.ok_or_else(|| {
                Problem::validation("apply_scope 非 THIS，但這筆預約不屬於任何週期系列")
            })?;
            let from_start_at =
                (req.apply_scope == "THIS_AND_FOLLOWING").then_some(current.start_at);

            // 系列可能跨場域（recurrence_group_id 沒有那個約束），權限要對
            // 每一個涉及的場域都檢查——同 `tail::cancel_series` 的判斷。
            let scope = repo::series_scope(&mut tx, group_id, from_start_at).await?;
            if !scope.all_mine {
                for f in &scope.facilities {
                    require_permission(&mut tx, "reservation:update", Some(*f), None).await?;
                }
            }
            repo::update_series(&mut tx, group_id, from_start_at, &req).await?;
        }
        other => {
            return Err(Problem::validation(format!(
                "invalid apply_scope: {other} (expected THIS, THIS_AND_FOLLOWING, or ALL)"
            )));
        }
    }

    let updated = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("reservation not found"))?;
    tx.commit().await?;

    Ok(Json(to_dto(updated, &view)))
}

// =============================================================================
// S3：可用性查詢
// =============================================================================

/// 查詢範圍的上限（天）。
///
/// 有上限才有可預期的回應大小：一年 × 50 個資源 × 30 分鐘 slot
/// 是 87 萬個 slot，足以讓一個請求打垮服務。31 天涵蓋「看下個月」這個
/// 實際需求，超過就回 422 並說明 —— 靜默截斷會讓前端以為後面沒有預約。
const AVAILABILITY_MAX_DAYS: i64 = 31;
/// slot 粒度的界線。0 或負數會讓 slot 產生迴圈不終止。
const SLOT_MIN: i32 = 5;
const SLOT_MAX: i32 = 480;

/// 由忙碌區塊算出空閒 slot。
///
/// # 這裡刻意**不**套用規則
///
/// 只做「營業時間 − 忙碌區塊」的幾何運算。最短時長、預約窗、每人上限、
/// 配額都不在這裡判 —— 那些是 `check_resource_availability` 的職責，
/// 而權威判定只在 `POST /reservations` 發生。
///
/// 因此 `free_slots` 是**繪圖用的建議，不是保留**。要真正保留時段
/// 必須用 `POST /reservations/holds`。這一點寫在契約說明裡。
fn free_slots(
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
    slot_minutes: i32,
    busy: &[dto::BusyBlockDto],
) -> Vec<dto::FreeSlotDto> {
    let step = chrono::Duration::minutes(slot_minutes as i64);
    let mut out = Vec::new();
    let mut cursor = from;
    while cursor + step <= to {
        let slot_end = cursor + step;
        // 與任一忙碌區塊重疊就不算空閒。半開區間 [start, end) ——
        // 與資料庫的 tstzrange '[)' 一致，否則邊界相接的兩筆預約
        // 會被誤判成重疊。
        let overlaps = busy
            .iter()
            .any(|b| b.start_at < slot_end && cursor < b.end_at);
        if !overlaps {
            out.push(dto::FreeSlotDto {
                start_at: cursor,
                end_at: slot_end,
            });
        }
        cursor = slot_end;
    }
    out
}

/// `GET /facilities/{facilityId}/availability`
pub async fn availability(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Query(q): Query<dto::AvailabilityQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let from = q
        .from
        .ok_or_else(|| Problem::validation("from is required"))?;
    let to = q.to.ok_or_else(|| Problem::validation("to is required"))?;
    if to <= from {
        return Err(Problem::validation("to must be after from"));
    }
    if (to - from).num_days() > AVAILABILITY_MAX_DAYS {
        return Err(Problem::validation(format!(
            "the requested range spans more than {AVAILABILITY_MAX_DAYS} days; \
             narrow it rather than receiving a truncated answer"
        )));
    }
    let slot_minutes = q.slot_minutes.unwrap_or(30);
    if !(SLOT_MIN..=SLOT_MAX).contains(&slot_minutes) {
        return Err(Problem::validation(format!(
            "slot_minutes must be between {SLOT_MIN} and {SLOT_MAX}"
        )));
    }

    // 逗號分隔的 uuid 清單。任何一個解析失敗都回 422 而非默默略過 ——
    // 略過會讓客戶端拿到一份少了某個資源的回應卻不知道。
    let requested: Option<Vec<Uuid>> = match q.resource_ids.as_deref() {
        Some(raw) => {
            let mut ids = Vec::new();
            for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                ids.push(Uuid::parse_str(part).map_err(|_| {
                    Problem::validation(format!("`{part}` is not a valid resource id"))
                })?);
            }
            Some(ids)
        }
        None => None,
    };

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 082：REQUESTER 只有 `reservation:read_availability`（窄權限，只開這支
    // 端點），沒有 `reservation:read`（那個會連 `GET /reservations`／
    // `/occupancy` 都打開）。兩者任一即可——已持有 `reservation:read` 的
    // 角色不需要重新授予新碼。
    if !has_permission(&mut tx, "reservation:read", Some(facility_id), None).await?
        && !has_permission(
            &mut tx,
            "reservation:read_availability",
            Some(facility_id),
            None,
        )
        .await?
    {
        return Err(Problem::permission_denied(
            "missing permission: reservation:read or reservation:read_availability",
        ));
    }
    let view = PrivateView::resolve(&mut tx, Some(facility_id)).await?;

    // 省略 resource_ids 時取該設施所有可預約資源。
    // 上限刻意寬鬆但有界：一個設施的可預約資源數是有限的，
    // 而分頁一份「時間軸」對前端沒有意義。
    let resources =
        repo::list_resources(&mut tx, Some(facility_id), q.min_capacity, true, 500).await?;
    let resources: Vec<repo::ResourceRow> = match &requested {
        Some(ids) => resources
            .into_iter()
            .filter(|r| ids.contains(&r.resource_id))
            .collect(),
        None => resources,
    };

    let ids: Vec<Uuid> = resources.iter().map(|r| r.resource_id).collect();
    let busy = if ids.is_empty() {
        Vec::new()
    } else {
        repo::busy_blocks(&mut tx, facility_id, &ids, from, to).await?
    };
    tx.commit().await?;

    let data: Vec<dto::ResourceAvailabilityDto> = resources
        .into_iter()
        .map(|r| {
            let blocks: Vec<dto::BusyBlockDto> = busy
                .iter()
                .filter(|b| b.resource_id == r.resource_id)
                .map(|b| {
                    // 私人預約的標題不該洩漏到可用性查詢——見模組檔頭 082 的說明。
                    // 種類與時段刻意不遮：那正是這支端點存在的理由。
                    let masked = view.masks_raw(b.is_private, b.organizer_id, b.on_behalf_of_id);
                    dto::BusyBlockDto {
                        start_at: b.start_at,
                        end_at: b.end_at,
                        kind: b.kind.clone(),
                        reason: if masked { None } else { b.reason.clone() },
                    }
                })
                .collect();
            let slots = free_slots(from, to, slot_minutes, &blocks);
            dto::ResourceAvailabilityDto {
                resource_id: r.resource_id,
                resource_type: r.resource_type,
                display_name: r.display_name,
                capacity: r.capacity,
                opening_hours: r.opening_hours,
                rules: dto::ResourceRulesDto {
                    min_duration_minutes: r.min_duration_minutes,
                    max_duration_minutes: r.max_duration_minutes,
                    slot_granularity_minutes: r.slot_granularity_minutes,
                    requires_approval: r.requires_approval,
                    advance_booking_days: r.advance_booking_days,
                },
                busy: blocks,
                free_slots: slots,
            }
        })
        .collect();

    Ok(Json(serde_json::json!({ "data": data })))
}

// =============================================================================
// S4：兩階段佔位、報到、取消
// =============================================================================

/// 佔位 TTL 的界線，對齊契約（default 180、maximum 900）。
const HOLD_TTL_DEFAULT: i32 = 180;
const HOLD_TTL_MAX: i32 = 900;

/// `POST /reservations/holds`
///
/// 兩階段預約的第一步：先佔住時段，讓使用者填完附加服務表單再確認。
///
/// 原子性來自 `excl_reservation_holds_overlap` 排他約束，與
/// `POST /reservations` 是同一個機制 —— 不使用 advisory lock，理由見
/// `repo::create_hold` 的說明。
pub async fn create_hold(
    State(state): State<ReservationState>,
    caller: Caller,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let req: dto::HoldCreate = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid hold request: {e}")))?;
    let resource_id = req
        .resource_id
        .ok_or_else(|| Problem::validation("resource_id is required"))?;
    let start_at = req
        .start_at
        .ok_or_else(|| Problem::validation("start_at is required"))?;
    let end_at = req
        .end_at
        .ok_or_else(|| Problem::validation("end_at is required"))?;
    if end_at <= start_at {
        return Err(Problem::validation("end_at must be after start_at"));
    }
    let ttl = req.ttl_seconds.unwrap_or(HOLD_TTL_DEFAULT);
    if !(1..=HOLD_TTL_MAX).contains(&ttl) {
        return Err(Problem::validation(format!(
            "ttl_seconds must be between 1 and {HOLD_TTL_MAX}"
        )));
    }

    let idem_key = concurrency::key_from(&headers)?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let pending = match idem_key.as_deref() {
        Some(key) => concurrency::begin(&mut tx, key, "POST /reservations/holds", &raw)
            .await?
            .pending(),
        None => None,
    };

    let booking = repo::find_bookable(&mut tx, resource_id)
        .await?
        .ok_or_else(|| Problem::not_found("resource is not bookable"))?;
    let auth = require_permission(
        &mut tx,
        "reservation:create",
        Some(booking.facility_id),
        None,
    )
    .await?;

    // 回放要等授權跑完（見 create 的說明與 PendingReplay）
    if let Some(replay) = pending {
        let (code, body) = replay.release(auth);
        tx.commit().await?;
        return Ok((code, Json(body)));
    }

    // 佔位也要避開**既有預約**：排他約束只管 hold 之間，看不到 reservations。
    // 規則與衝突判定的權威來源是 check_resource_availability。
    let availability =
        repo::check_availability(&mut tx, resource_id, start_at, end_at, caller.user_id).await?;
    if !availability.is_available {
        let kind = availability.conflict_type.unwrap_or_default();
        let detail = availability
            .detail
            .unwrap_or_else(|| "resource is not available for the requested window".to_string());
        let code = match kind.as_str() {
            "RESERVATION_CONFLICT" | "HELD_BY_OTHER" | "BLACKOUT" | "DESK_ASSIGNED" => {
                ProblemCode::ReservationConflict
            }
            "QUOTA_EXCEEDED" => ProblemCode::QuotaExceeded,
            _ => ProblemCode::ValidationError,
        };
        return Err(Problem::new(code).with_detail(detail));
    }

    let (hold_token, expires_at) = repo::create_hold(
        &mut tx,
        booking.bookable_resource_id,
        booking.facility_id,
        start_at,
        end_at,
        ttl,
    )
    .await?;

    let body = serde_json::to_value(dto::HoldDto {
        hold_token,
        expires_at,
        resource_id,
        start_at,
        end_at,
    })
    .map_err(Problem::internal)?;

    if let Some(key) = idem_key.as_deref() {
        concurrency::complete(&mut tx, key, "POST /reservations/holds", 201, &body).await?;
    }
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// `POST /reservations/{reservationId}/check-in`
///
/// 未報到的預約會由背景作業在 `auto_release_at` 之後轉為 `NO_SHOW`
/// 並釋放時段（S5 的 worker）。這支端點是那條計算的輸入。
pub async fn check_in(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(req): Json<dto::CheckInRequest>,
) -> Result<Json<ReservationDto>, Problem> {
    const METHODS: &[&str] = &["MANUAL", "QR", "NFC", "OCCUPANCY_SENSOR", "ACCESS_CONTROL"];
    let method = req.method.unwrap_or_else(|| "MANUAL".to_string());
    if !METHODS.contains(&method.as_str()) {
        return Err(Problem::validation(format!(
            "invalid method `{method}`; allowed: {METHODS:?}"
        )));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("reservation not found"))?;
    // 報到是主辦人（或代訂對象）自己的動作；其他人需要 update 權限。
    if current.organizer_id != caller.user_id && current.on_behalf_of_id != Some(caller.user_id) {
        require_permission(
            &mut tx,
            "reservation:update",
            Some(current.facility_id),
            None,
        )
        .await?;
    }

    if repo::check_in(&mut tx, id, &method).await? == 0 {
        // 資料列存在但狀態不允許 —— 409 而非 422：請求本身沒問題，
        // 是當前狀態讓它不可行，而重送同樣的請求永遠不會成功。
        return Err(Problem::new(ProblemCode::Conflict).with_detail(format!(
            "cannot check in a reservation in status {}",
            current.status
        )));
    }
    let updated = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("reservation vanished")))?;
    // **寫入路徑的回應也遮罩。** 一個持有 `reservation:cancel_any` 但沒有
    // `view_private` 的人取消了別人的私人預約，不該從回應裡讀到標題 ——
    // 「所有讀取路徑同一條規則」比「寫入路徑例外」少一個要記住的特例。
    let view = PrivateView::resolve(&mut tx, Some(updated.facility_id)).await?;
    tx.commit().await?;
    Ok(Json(to_dto(updated, &view)))
}

/// `DELETE /reservations/{reservationId}`
///
/// 軟取消：狀態改 `CANCELLED` 並留下時間、取消者與原因。
/// 不硬刪 —— 預約的稽核軌跡與配額退還都需要那一列還在。
///
/// **沒有取消時間窗判定**：schema 沒有存放該政策的欄位
/// （見 `repo::cancel` 的說明）。憑空定一個時限會是發明政策。
pub async fn cancel(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    OptionalJson(body): OptionalJson<dto::CancelRequest>,
) -> Result<StatusCode, Problem> {
    let reason = body.and_then(|b| b.reason);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("reservation not found"))?;
    // 主辦人可取消自己的；取消別人的需要 cancel_any。
    if current.organizer_id != caller.user_id {
        require_permission(
            &mut tx,
            "reservation:cancel_any",
            Some(current.facility_id),
            None,
        )
        .await?;
    }

    // ADR-14：這一列是外部日曆同步進來的鏡像，唯一的編輯入口是外部日曆
    // 本身——FMS 這邊取消它只會在下一輪拉入時被外部原生事件的現況覆蓋回去，
    // 看起來像「取消了又自己復活」。直接擋掉並指出正確的操作位置。
    if matches!(current.created_via.as_str(), "OUTLOOK" | "GOOGLE") {
        return Err(Problem::new(ProblemCode::Conflict).with_detail(
            "this reservation was created from an external calendar (Outlook/Google) — \
             cancel it there; changes made in the external calendar will sync back",
        ));
    }

    if repo::cancel(&mut tx, id, reason.as_deref()).await? == 0 {
        return Err(Problem::new(ProblemCode::Conflict).with_detail(format!(
            "a reservation in status {} cannot be cancelled",
            current.status
        )));
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// =============================================================================
// S5：核准／駁回、即時佔用
// =============================================================================

/// 核准與駁回共用的前置：取出待審預約並確認呼叫者有核准權。
///
/// 權限用 `reservation:approve`，而 `bookable_resources.approver_role_code`
/// 可以指定「只有某個角色能核准這個資源」—— **目前未實作那一層**：
/// 權限目錄裡沒有依資源指定核准者的機制，而在應用層比對角色碼
/// 會繞過 `user_has_permission` 這個唯一權威。已列為已知缺口。
async fn load_pending(
    tx: &mut fms_shared::TenantTx,
    caller_facility_check: Uuid,
) -> Result<(), Problem> {
    require_permission(tx, "reservation:approve", Some(caller_facility_check), None)
        .await
        .map(|_| ())
}

/// `POST /reservations/{reservationId}/approve`
pub async fn approve(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<ReservationDto>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("reservation not found"))?;
    load_pending(&mut tx, current.facility_id).await?;

    if repo::approve(&mut tx, id).await? == 0 {
        return Err(Problem::new(ProblemCode::Conflict).with_detail(format!(
            "only a reservation awaiting approval can be approved (current status: {})",
            current.status
        )));
    }
    let updated = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("reservation vanished")))?;
    // **寫入路徑的回應也遮罩。** 一個持有 `reservation:cancel_any` 但沒有
    // `view_private` 的人取消了別人的私人預約，不該從回應裡讀到標題 ——
    // 「所有讀取路徑同一條規則」比「寫入路徑例外」少一個要記住的特例。
    let view = PrivateView::resolve(&mut tx, Some(updated.facility_id)).await?;
    tx.commit().await?;
    Ok(Json(to_dto(updated, &view)))
}

/// `POST /reservations/{reservationId}/reject`
///
/// 原因必填：被駁回的人需要知道為什麼，而 `rejection_reason` 是 schema
/// 早就準備好的欄位。允許空原因等於讓那個欄位永遠是 NULL。
pub async fn reject(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(req): Json<dto::RejectRequest>,
) -> Result<Json<ReservationDto>, Problem> {
    let reason = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| Problem::validation("reason is required when rejecting"))?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("reservation not found"))?;
    load_pending(&mut tx, current.facility_id).await?;

    if repo::reject(&mut tx, id, reason).await? == 0 {
        return Err(Problem::new(ProblemCode::Conflict).with_detail(format!(
            "only a reservation awaiting approval can be rejected (current status: {})",
            current.status
        )));
    }
    let updated = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("reservation vanished")))?;
    // **寫入路徑的回應也遮罩。** 一個持有 `reservation:cancel_any` 但沒有
    // `view_private` 的人取消了別人的私人預約，不該從回應裡讀到標題 ——
    // 「所有讀取路徑同一條規則」比「寫入路徑例外」少一個要記住的特例。
    let view = PrivateView::resolve(&mut tx, Some(updated.facility_id)).await?;
    tx.commit().await?;
    Ok(Json(to_dto(updated, &view)))
}

/// `GET /facilities/{facilityId}/occupancy`
///
/// 現場管理用的即時佔用地圖。刻意與 `availability` 分開：
/// availability 回答「未來哪些時段可訂」，occupancy 回答「**此刻**誰在裡面」。
/// 用 availability 加一個很窄的時間範圍來模擬會拿到 slot 陣列，
/// 而現場要的是每個資源一列的當下狀態。
pub async fn occupancy(
    State(state): State<ReservationState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 佔用地圖會顯示主辦人姓名與標題，因此需要完整的 read 權限，
    // 不接受 read_own —— 只看得到自己預約的人看到的地圖沒有意義，
    // 而看得到別人的名字就不是 own 的範圍。
    require_permission(&mut tx, "reservation:read", Some(facility_id), None).await?;
    let rows = repo::occupancy(&mut tx, facility_id).await?;
    // 佔用地圖是這個缺陷最顯眼的一面：它是牆面板與樓層圖的資料來源，
    // 而在此之前它把私人會議的標題與主辦人姓名顯示給所有持有
    // `reservation:read` 的人 —— 也就是幾乎所有人。
    let may_view = permission_codes(&mut tx, Some(facility_id), None)
        .await?
        .contains("reservation:view_private");
    tx.commit().await?;

    let data: Vec<dto::OccupancyDto> = rows
        .into_iter()
        .map(|r| {
            // 這裡沒有「本人不遮」的例外，與清單／單筆不同。
            //
            // 理由是這支端點的查詢沒有 organizer_id：它 JOIN users 只為了取
            // 顯示名稱。要加上「本人看得到自己的」就得多帶一欄出來，而佔用地圖
            // 的用途是「這個空間現在有沒有人」——**在地圖上看到自己那格寫著
            // 「已預約」不是缺陷**。少一個欄位、少一條分支。
            //
            // 代價寫在這裡：同一筆預約在 /reservations 看得到標題（本人），
            // 在 /occupancy 看不到。那個不一致是刻意的，不是漏改。
            let masked = r.is_private == Some(true) && !may_view;
            dto::OccupancyDto {
                resource_id: r.resource_id,
                display_name: r.display_name,
                resource_type: r.resource_type,
                capacity: r.capacity,
                state: r.state,
                reservation_id: r.reservation_id,
                title: if masked { None } else { r.title },
                organizer_name: if masked { None } else { r.organizer_name },
                start_at: r.start_at,
                end_at: r.end_at,
                is_private: r.is_private == Some(true),
            }
        })
        .collect();
    Ok(Json(serde_json::json!({ "data": data })))
}
