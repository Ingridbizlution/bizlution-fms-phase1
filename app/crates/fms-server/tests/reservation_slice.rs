//! reservations 切片的端到端測試。
//!
//! 重點不是「端點會回 200」，而是四個橫切關注點真的成立：
//!   * cursor 分頁：`PagedEnvelope` 形狀、limit 上限、next_cursor 可續頁且不重複
//!   * Idempotency-Key：同鍵同 body 回放、同鍵不同 body 422
//!   * If-Match 樂觀鎖：缺少 428、過期 412、正確則成功且 version 遞增
//!   * 409 衝突：重疊時段被排他約束擋下並映射為 RESERVATION_CONFLICT

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 009 示範資料的 401 會議室（spatial_node）
const ROOM_401: &str = "10000000-0000-4000-8000-000000000005";
const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const ROOM_402: &str = "10000000-0000-4000-8000-000000000006";

fn slot(day_offset: i64, hour: i64) -> (String, String) {
    // 相對日期：避免像 012 的 T9 那樣寫死絕對日期而隨時間失效。
    // 同時必須落在 bookable_resources.advance_booking_days 窗口內。
    let base = chrono::Utc::now() + chrono::Duration::days(day_offset);
    let start = base
        .date_naive()
        .and_hms_opt(hour as u32, 0, 0)
        .unwrap()
        .and_utc();
    (
        start.to_rfc3339(),
        (start + chrono::Duration::hours(1)).to_rfc3339(),
    )
}

#[tokio::test]
async fn reservation_slice_end_to_end() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    // ---- 建立一筆預約（無冪等鍵）----
    let (s1, e1) = slot(3, 9);
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "橫切測試 A",
                    "start_at": s1, "end_at": e1, "party_size": 4
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {body}");
    assert!(
        body["reservation_no"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "reservation_no 應由 fms.next_document_no 產生: {body}"
    );
    assert_eq!(body["status"], "CONFIRMED");
    assert_eq!(body["version"], 1, "新建列的 version 應為 1");
    let first_id = body["id"].as_str().unwrap().to_string();

    // ---- 409：同資源同時段重疊，由排他約束擋下 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "應被拒",
                    "start_at": s1, "end_at": e1
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "重疊時段應回 409: {body}");
    assert_eq!(body["code"], "RESERVATION_CONFLICT");

    // ---- 冪等：同鍵同 body 第二次應回放首次結果，不新建 ----
    let (s2, e2) = slot(4, 9);
    let payload = json!({
        "resource_id": ROOM_401, "title": "冪等測試",
        "start_at": s2, "end_at": e2
    });
    let key = format!("idem-{}", uuid::Uuid::new_v4());

    let (status, first) = ctx
        .send(authed_idem(
            json_request("POST", "/api/v1/reservations", payload.clone()),
            &token,
            &key,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let idem_id = first["id"].as_str().unwrap().to_string();

    let (status, replay) = ctx
        .send(authed_idem(
            json_request("POST", "/api/v1/reservations", payload.clone()),
            &token,
            &key,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "重放應回原狀態碼: {replay}");
    assert_eq!(
        replay["id"].as_str().unwrap(),
        idem_id,
        "重放必須回同一筆，而不是新建（否則冪等沒生效）"
    );

    // ---- 冪等：同鍵不同 body → 422 ----
    let (s3, e3) = slot(5, 9);
    let (status, body) = ctx
        .send(authed_idem(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "不同 body",
                    "start_at": s3, "end_at": e3
                }),
            ),
            &token,
            &key,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "IDEMPOTENCY_KEY_REUSED");

    // ---- GET 單筆應回 ETag ----
    let (status, etag, body) = ctx
        .send_with_headers(authed(
            Request::builder()
                .uri(format!("/api/v1/reservations/{first_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let etag = etag.expect("GET 單筆應回 ETag，供 PATCH 的 If-Match 使用");
    assert_eq!(etag, "\"1\"");

    // ---- If-Match 缺少 → 428 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/reservations/{first_id}"),
                json!({ "title": "改標題" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_REQUIRED,
        "缺 If-Match 應回 428: {body}"
    );
    assert_eq!(body["code"], "PRECONDITION_REQUIRED");

    // ---- If-Match 過期 → 412 ----
    let (status, body) = ctx
        .send(authed_if_match(
            json_request(
                "PATCH",
                &format!("/api/v1/reservations/{first_id}"),
                json!({ "title": "改標題" }),
            ),
            &token,
            "999",
        ))
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
    assert_eq!(body["code"], "STALE_VERSION");

    // ---- If-Match 正確 → 成功，且 version 由觸發器遞增 ----
    let (status, body) = ctx
        .send(authed_if_match(
            json_request(
                "PATCH",
                &format!("/api/v1/reservations/{first_id}"),
                json!({ "title": "改過的標題" }),
            ),
            &token,
            &etag,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "改過的標題");
    assert_eq!(
        body["version"], 2,
        "version 應由 trg_reservations_version 自動遞增"
    );

    // ---- 分頁：limit=1 應回 PagedEnvelope 且能用 next_cursor 續頁 ----
    let (status, page1) = ctx
        .send(authed(
            Request::builder()
                .uri("/api/v1/reservations?mine=true&limit=1")
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page1}");
    assert!(page1["data"].is_array(), "須為 PagedEnvelope: {page1}");
    assert_eq!(page1["data"].as_array().unwrap().len(), 1);
    assert_eq!(page1["page"]["limit"], 1);
    assert!(
        page1["page"]["total_estimate"].is_null(),
        "本切片不回精確總數"
    );
    let cursor = page1["page"]["next_cursor"]
        .as_str()
        .expect("有多筆資料時應給 next_cursor")
        .to_string();
    let first_page_id = page1["data"][0]["id"].as_str().unwrap().to_string();

    let (status, page2) = ctx
        .send(authed(
            Request::builder()
                .uri(format!(
                    "/api/v1/reservations?mine=true&limit=1&cursor={cursor}"
                ))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page2}");
    let second_page_id = page2["data"][0]["id"].as_str().unwrap().to_string();
    assert_ne!(
        first_page_id, second_page_id,
        "第二頁不得重複第一頁的列（keyset 分頁失效）"
    );

    // ---- 壞掉的 cursor → 400，而非 500 ----
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .uri("/api/v1/reservations?mine=true&cursor=not-a-cursor")
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // ---- limit 超過上限應被夾到 200，而非報錯 ----
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .uri("/api/v1/reservations?mine=true&limit=9999")
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["page"]["limit"], 200);

    // ---- reservation:read_own 的列級範圍（WBS 3.9）----
    // 先前 `mine=true` 會跳過權限檢查，等於任何登入者都能讀自己的預約，
    // 而 `reservation:read_own` 完全是裝飾。現在 `mine` 只收窄、不繞道。
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    // user.huang 只有 create／read_own／update：讀得到自己的
    let (s3, e3) = slot(5, 14);
    let (status, own) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "申請人自己的預約",
                    "start_at": s3, "end_at": e3, "party_size": 2
                }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{own}");
    let own_id = own["id"].as_str().unwrap().to_string();

    let (status, got) = ctx
        .send(authed(
            Request::builder()
                .uri(format!("/api/v1/reservations/{own_id}"))
                .body(Body::empty())
                .unwrap(),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "自己的預約應讀得到: {got}");

    // 但讀不到管理員建立的那一筆
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .uri(format!("/api/v1/reservations/{first_id}"))
                .body(Body::empty())
                .unwrap(),
            &requester,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "只有 read_own 時別人的預約應與不存在不可分辨: {body}"
    );

    // 列表不帶 mine 也只回自己的
    let (status, page) = ctx
        .send(authed(
            Request::builder()
                .uri("/api/v1/reservations?limit=200")
                .body(Body::empty())
                .unwrap(),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let ids: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&own_id.as_str()), "應包含自己的預約: {page}");
    assert!(
        !ids.contains(&first_id.as_str()),
        "不該漏出別人的預約: {page}"
    );

    availability_reports_busy_blocks_and_free_slots(&ctx).await;
    holds_check_in_and_cancel(&ctx).await;
    approval_occupancy_and_no_show(&ctx).await;

    ctx.teardown().await;
}

/// `GET /facilities/{facilityId}/availability`（Phase 2 S3）。
///
/// 重點：忙碌區塊來自資料（預約／佔位／封鎖）而非規則判定，
/// 而 `free_slots` 只是「營業時間 − 忙碌」的幾何結果 ——
/// **不是保留**。權威判定仍然只在 `POST /reservations` 發生。
async fn availability_reports_busy_blocks_and_free_slots(ctx: &TestContext) {
    let token = ctx.login().await;
    let (start, end) = slot(7, 10);
    let day_from = format!("{}T00:00:00Z", &start[..10]);
    let day_to = format!("{}T23:59:59Z", &start[..10]);

    // 先訂一個時段，讓它成為忙碌區塊
    let (status, booked) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "可用性測試",
                    "start_at": start, "end_at": end, "party_size": 3
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{booked}");

    let (status, avail) = ctx
        .send(authed(
            Request::builder()
                .uri(format!(
                    "/api/v1/facilities/{FACILITY_HQ}/availability?from={day_from}&to={day_to}&slot_minutes=60"
                ))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{avail}");
    let rooms = avail["data"].as_array().expect("應有 data");
    assert!(!rooms.is_empty(), "設施應有可預約資源：{avail}");

    let room = rooms
        .iter()
        .find(|r| r["resource_id"] == ROOM_401)
        .unwrap_or_else(|| panic!("應包含 401 會議室：{avail}"));

    // 規則原樣回傳，供客戶端顯示；權威判定不在這裡
    assert!(
        room["rules"]["slot_granularity_minutes"].as_i64().is_some(),
        "rules 應原樣帶出資源設定：{room}"
    );
    assert!(room["opening_hours"].is_object());

    // 剛才那筆預約必須出現在忙碌區塊裡
    let busy = room["busy"].as_array().expect("應有 busy");
    assert!(
        busy.iter().any(|b| {
            let k = b["kind"].as_str().unwrap_or_default();
            (k == "RESERVATION" || k == "BUFFER") && b["start_at"].as_str().is_some()
        }),
        "剛建立的預約應出現為忙碌區塊：{room}"
    );

    // 空閒 slot 不得與任何忙碌區塊重疊（半開區間，與 tstzrange '[)' 一致）
    let slots = room["free_slots"].as_array().expect("應有 free_slots");
    for s in slots {
        let ss: chrono::DateTime<chrono::Utc> = s["start_at"].as_str().unwrap().parse().unwrap();
        let se: chrono::DateTime<chrono::Utc> = s["end_at"].as_str().unwrap().parse().unwrap();
        for b in busy {
            let bs: chrono::DateTime<chrono::Utc> =
                b["start_at"].as_str().unwrap().parse().unwrap();
            let be: chrono::DateTime<chrono::Utc> = b["end_at"].as_str().unwrap().parse().unwrap();
            assert!(
                !(bs < se && ss < be),
                "空閒 slot {ss}–{se} 與忙碌區塊 {bs}–{be} 重疊"
            );
        }
    }
    assert!(!slots.is_empty(), "整天不可能全被佔滿：{room}");

    // ---- 半開區間：緊貼忙碌區塊邊界的 slot 必須算空閒 ----
    //
    // 這一段是補上來的：原本只斷言「空閒不與忙碌重疊」，那在把重疊判定
    // 改成閉區間（`<=`）時**仍然成立** —— 只是回傳的 slot 變少。
    // mutation test 因此存活。真正要驗的是邊界相接**不算**重疊，
    // 與資料庫 `tstzrange '[)'` 的語意一致；否則每個忙碌區塊的前後
    // 各一個 slot 會無故消失。
    let block = busy
        .iter()
        .min_by_key(|b| b["start_at"].as_str().unwrap().to_string())
        .expect("至少一個忙碌區塊");
    let block_start: chrono::DateTime<chrono::Utc> =
        block["start_at"].as_str().unwrap().parse().unwrap();
    let block_end: chrono::DateTime<chrono::Utc> =
        block["end_at"].as_str().unwrap().parse().unwrap();

    let ends_at_block_start = slots.iter().any(|s| {
        let se: chrono::DateTime<chrono::Utc> = s["end_at"].as_str().unwrap().parse().unwrap();
        se == block_start
    });
    let starts_at_block_end = slots.iter().any(|s| {
        let ss: chrono::DateTime<chrono::Utc> = s["start_at"].as_str().unwrap().parse().unwrap();
        ss == block_end
    });
    assert!(
        ends_at_block_start || starts_at_block_end,
        "與忙碌區塊 {block_start}–{block_end} 邊界相接的 slot 應算空閒（半開區間），\
         實際一個都沒有 —— 重疊判定用了閉區間：{room}"
    );

    // ---- resource_ids 過濾 ----
    let (_, one) = ctx
        .send(authed(
            Request::builder()
                .uri(format!(
                    "/api/v1/facilities/{FACILITY_HQ}/availability?from={day_from}&to={day_to}&resource_ids={ROOM_401}"
                ))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(one["data"].as_array().unwrap().len(), 1, "{one}");

    // ---- 壞的 uuid → 422（不默默略過）----
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .uri(format!(
                    "/api/v1/facilities/{FACILITY_HQ}/availability?from={day_from}&to={day_to}&resource_ids=not-a-uuid"
                ))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 範圍過大 → 422（不截斷）----
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .uri(format!(
                    "/api/v1/facilities/{FACILITY_HQ}/availability?from=2026-01-01T00:00:00Z&to=2027-01-01T00:00:00Z"
                ))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 缺 from／to、或 to <= from → 422 ----
    for qs in [
        format!("from={day_from}"),
        format!("from={day_to}&to={day_from}"),
        format!("from={day_from}&to={day_to}&slot_minutes=0"),
    ] {
        let (status, body) = ctx
            .send(authed(
                Request::builder()
                    .uri(format!(
                        "/api/v1/facilities/{FACILITY_HQ}/availability?{qs}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{qs} 應回 422：{body}"
        );
    }
}

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 兩階段佔位、報到、取消（Phase 2 S4）。
///
/// 原子性刻意**不**用 advisory lock：`reservation_holds` 有
/// `excl_reservation_holds_overlap` 排他約束，與 `reservations` 同一個機制。
/// 這裡證明它真的擋住第二個佔位。
async fn holds_check_in_and_cancel(ctx: &TestContext) {
    let token = ctx.login().await;

    // ---- 佔位 ----
    let (hs, he) = slot(9, 14);
    let (status, hold) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations/holds",
                json!({ "resource_id": ROOM_401, "start_at": hs, "end_at": he }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{hold}");
    assert!(
        hold["hold_token"].as_str().is_some_and(|t| t.len() >= 32),
        "hold_token 應是足夠長的隨機值（gen_random_bytes）：{hold}"
    );
    let expires: chrono::DateTime<chrono::Utc> =
        hold["expires_at"].as_str().unwrap().parse().unwrap();
    assert!(expires > chrono::Utc::now(), "佔位應有未來的失效時刻");

    // ---- 同一資源同一時段第二次佔位 → 409（由排他約束擇一）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations/holds",
                json!({ "resource_id": ROOM_401, "start_at": hs, "end_at": he }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "第二個佔位應被 excl_reservation_holds_overlap 擋下，不需要 advisory lock：{body}"
    );

    // ---- 佔位會出現在可用性的忙碌區塊裡，kind=HOLD ----
    let day_from = format!("{}T00:00:00Z", &hs[..10]);
    let day_to = format!("{}T23:59:59Z", &hs[..10]);
    let (_, avail) = ctx
        .send(authed(
            Request::builder()
                .uri(format!(
                    "/api/v1/facilities/{FACILITY_HQ}/availability?from={day_from}&to={day_to}&resource_ids={ROOM_401}"
                ))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert!(
        avail["data"][0]["busy"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["kind"] == "HOLD"),
        "佔位應顯示為 HOLD 區塊，讓前端能與『已被預約』區分：{avail}"
    );

    // ---- 對**已被預約**的時段佔位 → 409 ----
    //
    // 排他約束只管 hold 與 hold 之間，看不到 `reservations`。少了這一關，
    // 使用者能佔住一個早就被訂走的時段，填完表單才在確認時失敗 ——
    // 那正好是兩階段預約要避免的事。
    // （這段是 mutation test 揭露的缺口：拿掉可用性判定後測試原本仍然全綠。）
    let (bs, be) = slot(10, 15);
    let (status, booked) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({ "resource_id": ROOM_401, "title": "先佔走",
                        "start_at": bs, "end_at": be, "party_size": 2 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{booked}");

    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations/holds",
                json!({ "resource_id": ROOM_401, "start_at": bs, "end_at": be }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "已被預約的時段不該能佔位 —— 佔位必須經過 check_resource_availability：{body}"
    );

    // ---- ttl_seconds 超界 → 422 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations/holds",
                json!({ "resource_id": ROOM_401, "start_at": hs, "end_at": he,
                        "ttl_seconds": 99999 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 報到與取消：另建一筆預約 ----
    let (cs, ce) = slot(11, 9);
    let (status, res) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({ "resource_id": ROOM_401, "title": "報到測試",
                        "start_at": cs, "end_at": ce, "party_size": 2 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{res}");
    let res_id = res["id"].as_str().unwrap().to_string();

    // 未定義的報到方式 → 422
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/reservations/{res_id}/check-in"),
                json!({ "method": "TELEPATHY" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // 正常報到
    let (status, checked) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/reservations/{res_id}/check-in"),
                json!({ "method": "QR" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{checked}");
    assert_eq!(checked["status"], "CHECKED_IN");
    assert!(
        checked["checked_in_at"].as_str().is_some(),
        "報到應記下時刻，no-show 判定要用它：{checked}"
    );

    // 重複報到 → 409（狀態已不是 CONFIRMED）
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/reservations/{res_id}/check-in"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "重複報到是狀態問題（409），不是格式問題（422）：{body}"
    );

    // ---- 取消 ----
    let (status, _) = ctx
        .send(authed(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/reservations/{res_id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "reason": "會議取消" }).to_string()))
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, after) = ctx
        .send(authed(
            Request::builder()
                .uri(format!("/api/v1/reservations/{res_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(after["status"], "CANCELLED", "軟取消：資料列仍在：{after}");

    // 重複取消 → 409
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/reservations/{res_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // 取消後時段應釋放：同一時段可以重新預約
    let (status, again) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({ "resource_id": ROOM_401, "title": "取消後重訂",
                        "start_at": cs, "end_at": ce, "party_size": 2 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "取消後時段應釋放 —— 排他約束的 WHERE 子句排除終態：{again}"
    );
}

/// 核准／駁回、即時佔用、no-show 掃描（Phase 2 S5）。
async fn approval_occupancy_and_no_show(ctx: &TestContext) {
    let token = ctx.login().await;

    // 種子資料沒有「需要核准」的資源，因此暫時把 402 會議室設成需要核准。
    // 這是設定資料，不是程式行為 —— 測試結束後由 cleanup 還原。
    let mut cfg = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.bookable_resources SET requires_approval = true
          WHERE spatial_node_id = $1::uuid",
    )
    .bind(ROOM_402)
    .execute(&mut *cfg)
    .await
    .expect("arm approval");
    cfg.commit().await.expect("commit");

    // ---- 建立時應直接進入 PENDING_APPROVAL ----
    let (ps, pe) = slot(12, 10);
    let (status, pending) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({ "resource_id": ROOM_402, "title": "需要核准的會議",
                        "start_at": ps, "end_at": pe, "party_size": 4 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{pending}");
    assert_eq!(
        pending["status"], "PENDING_APPROVAL",
        "requires_approval 的資源應讓預約進入待審：{pending}"
    );
    let pending_id = pending["id"].as_str().unwrap().to_string();

    // ---- 駁回缺原因 → 422 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/reservations/{pending_id}/reject"),
                json!({ "reason": "   " }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "駁回必須附原因，否則 rejection_reason 永遠是 NULL：{body}"
    );

    // ---- 核准 ----
    let (status, approved) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/reservations/{pending_id}/approve"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["status"], "CONFIRMED");

    // 已核准的不能再核准或駁回 → 409
    for action in ["approve", "reject"] {
        let (status, body) = ctx
            .send(authed(
                json_request(
                    "POST",
                    &format!("/api/v1/reservations/{pending_id}/{action}"),
                    json!({ "reason": "太晚了" }),
                ),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{action} 一筆已核准的預約應回 409：{body}"
        );
    }

    // ---- 駁回另一筆 ----
    let (rs, re_) = slot(13, 10);
    let (_, pending2) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({ "resource_id": ROOM_402, "title": "會被駁回",
                        "start_at": rs, "end_at": re_, "party_size": 4 }),
            ),
            &token,
        ))
        .await;
    let p2 = pending2["id"].as_str().unwrap().to_string();
    let (status, rejected) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/reservations/{p2}/reject"),
                json!({ "reason": "當天有全公司活動" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{rejected}");
    assert_eq!(rejected["status"], "REJECTED");

    // 駁回後時段應釋放
    let (status, again) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({ "resource_id": ROOM_402, "title": "駁回後重訂",
                        "start_at": rs, "end_at": re_, "party_size": 2 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "駁回應釋放時段：{again}");

    // ---- 即時佔用地圖 ----
    let (status, occ) = ctx
        .send(authed(
            Request::builder()
                .uri(format!("/api/v1/facilities/{FACILITY_HQ}/occupancy"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{occ}");
    let rows = occ["data"].as_array().expect("應有 data");
    assert!(!rows.is_empty(), "設施應有可預約資源：{occ}");
    // 上面建立的預約都在未來，因此此刻每個資源都該是 FREE
    assert!(
        rows.iter().all(|r| {
            let s = r["state"].as_str().unwrap();
            ["FREE", "OCCUPIED", "RESERVED", "HELD"].contains(&s)
        }),
        "state 必須是四種之一：{occ}"
    );
    assert!(
        rows.iter().any(|r| r["state"] == "FREE"),
        "未來的預約不該讓資源此刻顯示為忙碌：{occ}"
    );

    // ---- no-show ----
    // 建立一筆需要報到的預約（401 會議室 auto_release_minutes = 15）
    let (ns, ne) = slot(14, 10);
    let (status, will_miss) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({ "resource_id": ROOM_401, "title": "不會出現的會議",
                        "start_at": ns, "end_at": ne, "party_size": 2 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{will_miss}");
    let miss_id = will_miss["id"].as_str().unwrap().to_string();

    // auto_release_at 必須被填入 —— 沒有它整條 no-show 機制是斷的
    let mut probe = ctx.tenant_tx().await;
    let release: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT auto_release_at FROM fms.reservations WHERE id = $1::uuid")
            .bind(&miss_id)
            .fetch_one(&mut *probe)
            .await
            .expect("read auto_release_at");
    let release = release
        .expect("requires_check_in 且資源設了 auto_release_minutes 時，auto_release_at 必須有值");
    let start: chrono::DateTime<chrono::Utc> = ns.parse().unwrap();
    assert_eq!(
        (release - start).num_minutes(),
        15,
        "auto_release_at 應是開始時間加上資源的 auto_release_minutes"
    );
    drop(probe);

    // 另建一筆並報到，證明掃描不會誤標已報到的
    let (cs2, ce2) = slot(15, 10);
    let (_, attended) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({ "resource_id": ROOM_401, "title": "會出現的會議",
                        "start_at": cs2, "end_at": ce2, "party_size": 2 }),
            ),
            &token,
        ))
        .await;
    let attended_id = attended["id"].as_str().unwrap().to_string();
    let (status, _) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/reservations/{attended_id}/check-in"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    // 把兩筆的 auto_release_at 都推到過去，讓掃描選中它們
    let mut arm = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.reservations SET auto_release_at = clock_timestamp() - interval '1 minute'
          WHERE id = ANY($1::uuid[])",
    )
    .bind(vec![
        uuid::Uuid::parse_str(&miss_id).unwrap(),
        uuid::Uuid::parse_str(&attended_id).unwrap(),
    ])
    .execute(&mut *arm)
    .await
    .expect("arm no-show");
    arm.commit().await.expect("commit");

    let scanner =
        fms_reservation::no_show::NoShowScanner::new(ctx.owner_pool().await, admin_user_id());
    let marked = scanner.run_once(100).await.expect("scan");
    assert!(marked >= 1, "應標記至少一筆逾期未報到，實際 {marked}");

    let mut probe = ctx.tenant_tx().await;
    let missed: String =
        sqlx::query_scalar("SELECT status FROM fms.reservations WHERE id = $1::uuid")
            .bind(&miss_id)
            .fetch_one(&mut *probe)
            .await
            .expect("status");
    assert_eq!(missed, "NO_SHOW", "逾期未報到應被標記");

    let kept: String =
        sqlx::query_scalar("SELECT status FROM fms.reservations WHERE id = $1::uuid")
            .bind(&attended_id)
            .fetch_one(&mut *probe)
            .await
            .expect("status");
    assert_eq!(
        kept, "CHECKED_IN",
        "已報到的不該被標記 —— 判定看的是 checked_in_at，不是時間"
    );
    drop(probe);

    // ---- 掃描與標記之間的競態：直接測 mark_no_show 的守衛 ----
    //
    // 掃描本身已經濾掉已報到的，所以走正常路徑永遠碰不到這個守衛 ——
    // 它存在的理由是**掃描與標記之間的時間差**：使用者可能剛好在那一瞬間報到。
    // 因此必須直接呼叫 mark_no_show 才測得到（mutation test 揭露了這個缺口：
    // 把守衛拿掉之後整套測試仍然全綠）。
    let mut race = ctx.tenant_tx_mut().await;
    let affected = fms_reservation::repo::mark_no_show(
        &mut race,
        uuid::Uuid::parse_str(&attended_id).unwrap(),
    )
    .await
    .expect("mark_no_show");
    assert_eq!(
        affected, 0,
        "已報到的預約即使被傳進 mark_no_show 也不該被標記 —— \
         條件重述在 UPDATE 的 WHERE 裡，競態由資料庫解決"
    );
    drop(race);

    // 再掃一次：已標記的不該重複處理
    let again = scanner.run_once(100).await.expect("second scan");
    assert_eq!(again, 0, "已標記的預約不該被重複處理，實際 {again}");
}

/// 兩階段預約的第二階段：`POST /reservations` 消耗 `hold_token`（WBS 7.5）。
///
/// 在此之前 `POST /reservations/holds` 通到一半就斷了 —— 可以取得佔位，
/// 但沒有任何辦法把它換成預約，`reservations.hold_token` 永遠是 NULL。
///
/// 驗的是四件事，其中三件在「看起來有寫」的情況下都可能失效：
///   1. 消耗成功時，佔位列真的變成 `CONSUMED`、預約列真的帶上 token
///   2. 同一個 token 不能用第二次（併發拿同一個 token 的防線）
///   3. 別人的 token 不能用（`user_id` 有進 `WHERE`）
///   4. 佔位範圍不涵蓋請求時段時拒絕，但**涵蓋**時允許子區段
#[tokio::test]
async fn two_phase_booking_consumes_the_hold() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let hold_at = |resource: &str, s: &str, e: &str| json!({ "resource_id": resource, "start_at": s, "end_at": e });

    // ---- 取得佔位（一小時）----
    let (hs, he) = slot(21, 10);
    let (status, hold) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations/holds",
                hold_at(ROOM_401, &hs, &he),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{hold}");
    let hold_token = hold["hold_token"].as_str().unwrap().to_string();

    // ---- 帶 token 建立預約：整個佔位範圍 ----
    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "兩階段預約",
                    "start_at": hs, "end_at": he, "hold_token": hold_token
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "帶佔位的建立應成功: {created}");
    let reservation_id = created["id"].as_str().unwrap().to_string();

    // 契約看不到這兩欄的關聯，因此直接查資料庫 —— 這是本測試的核心：
    // 少了消耗，佔位會一直是 ACTIVE 直到過期，而那段時間裡連建立者自己
    // 都會在別的時段判定上撞到它。
    {
        let mut tx = ctx.tenant_tx().await;
        let hold_status: String =
            sqlx::query_scalar("SELECT status FROM fms.reservation_holds WHERE hold_token = $1")
                .bind(&hold_token)
                .fetch_one(&mut *tx)
                .await
                .expect("read hold status");
        assert_eq!(hold_status, "CONSUMED", "佔位應已被消耗");

        let stored: Option<String> =
            sqlx::query_scalar("SELECT hold_token::text FROM fms.reservations WHERE id = $1::uuid")
                .bind(&reservation_id)
                .fetch_one(&mut *tx)
                .await
                .expect("read reservation hold_token");
        assert_eq!(
            stored.as_deref(),
            Some(hold_token.as_str()),
            "預約列應留下走過兩階段的憑據"
        );
    }

    // ---- 同一個 token 不能再用一次 ----
    let (hs2, he2) = slot(22, 10);
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "重複使用 token",
                    "start_at": hs2, "end_at": he2, "hold_token": hold_token
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "CONFLICT");

    // ---- 不存在的 token 得到同一個結果（不可分辨）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "亂編的 token",
                    "start_at": hs2, "end_at": he2,
                    "hold_token": "0000000000000000000000000000000000000000000000000"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "CONFLICT", "不存在與已消耗必須不可分辨");

    // ---- 別人的 token 不能用 ----
    // fm.lin 也持有 reservation:create（FACILITY_ADMIN，範圍含總部）。
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (hs3, he3) = slot(23, 10);
    let (status, others) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations/holds",
                hold_at(ROOM_401, &hs3, &he3),
            ),
            &fm,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{others}");
    let others_token = others["hold_token"].as_str().unwrap().to_string();

    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "盜用他人佔位",
                    "start_at": hs3, "end_at": he3, "hold_token": others_token
                }),
            ),
            &token, // admin.chen 用 fm.lin 的 token
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "佔位綁定使用者，別人不得消耗: {body}"
    );

    // 被拒之後那個佔位必須還在 —— 一次失敗的嘗試不該燒掉別人的佔位。
    {
        let mut tx = ctx.tenant_tx().await;
        let s: String =
            sqlx::query_scalar("SELECT status FROM fms.reservation_holds WHERE hold_token = $1")
                .bind(&others_token)
                .fetch_one(&mut *tx)
                .await
                .expect("read hold status");
        assert_eq!(s, "ACTIVE", "被拒的消耗不該改動佔位狀態");
    }

    // ---- 範圍：涵蓋則允許子區段，不涵蓋則拒絕 ----
    //
    // 佔位取兩小時，且所有時長都取 30 分的倍數：ROOM_402 的
    // `bookable_resources` 規則是 min=30／max=180／granularity=30，
    // 違反時回的是 422（規則問題）而不是 409（衝突），會讓斷言測到別的東西。
    let (hs4, _) = slot(24, 10);
    let start4: chrono::DateTime<chrono::Utc> = hs4.parse().unwrap();
    let at = |mins: i64| (start4 + chrono::Duration::minutes(mins)).to_rfc3339();

    let (status, h4) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations/holds",
                hold_at(ROOM_402, &hs4, &at(120)),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{h4}");
    let token4 = h4["hold_token"].as_str().unwrap().to_string();

    // 超出佔位範圍（150 分 > 佔位的 120 分，但仍在 max=180 內）→ 拒絕
    let beyond = at(150);
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_402, "title": "超出佔位範圍",
                    "start_at": hs4, "end_at": beyond, "hold_token": token4
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "佔位範圍不涵蓋請求時段時必須拒絕: {body}"
    );

    // 佔位內的子區段（30–60 分）→ 允許
    let sub_start = at(30);
    let sub_end = at(60);
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_402, "title": "佔位內的子區段",
                    "start_at": sub_start, "end_at": sub_end, "hold_token": token4
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "佔位涵蓋請求時段時應允許（確認時微調時間不必重新佔位）: {body}"
    );

    // ---- 消耗是整個消耗，不會留下剩餘區段 ----
    //
    // 上面只用掉佔位的 30–60 分。佔位的 60–90 分在時間上**完全空著**，
    // 時長也合法（30 分）。但佔位是一個鎖、不是可分割的庫存，
    // 因此同一個 token 不能再用。
    //
    // 這個斷言的形狀是刻意設計的：時段空著、時長合法、範圍在佔位內，
    // 於是唯一能擋下它的就是 `status = 'ACTIVE'`。
    //
    // 前面那個「重複使用 token」的案例用了**不同**時段，實際上是被範圍檢查
    // 擋下的 —— 實測移除 `status` 條件後那個案例仍然通過。多一個案例不是
    // 為了覆蓋率，是因為那一個證不到 compare-and-set 存在。
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_402, "title": "佔位的剩餘區段",
                    "start_at": at(60), "end_at": at(90), "hold_token": token4
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "已消耗的佔位不得再用於其剩餘時間（少了 status='ACTIVE' 這裡會是 201）: {body}"
    );
    assert_eq!(body["code"], "CONFLICT");

    ctx.teardown().await;
}

/// 附加的軟性服務（WBS 7.5）。
///
/// 在此之前 `services` 陣列被**靜默忽略**：客戶端照契約送，拿到 201，
/// 而 `fms.reservation_services` 一列都沒有。那不是「功能未完成」，
/// 是回應在說謊。
///
/// `service_items` 宣告了六條規則，而在此之前沒有任何一條被執行過。
/// 每一條都各驗一次，因為它們的失敗方式都是靜默的：
///   * `is_attachable_to_reservation` / `facility_id` / `lead_time_minutes`
///     / `max_quantity` / `form_schema` → 422，且指標要指到正確的陣列位置
///   * `relative_offset_minutes` / `default_duration_minutes`
///     → 服務班表算對（不算就等於清潔人員不知道幾點到）
const SVC_TEA: &str = "60000000-0000-4000-8000-000000000001"; // HQ、可附加、lead 120、offset -15、$60
const SVC_AV: &str = "60000000-0000-4000-8000-000000000003"; // HQ、可附加、lead 60、offset -15、dur 15
const SVC_DEEP_CLEAN: &str = "60000000-0000-4000-8000-000000000005"; // 影廳、**不可附加**

#[tokio::test]
async fn attached_services_are_recorded_with_a_resolved_schedule() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (s, e) = slot(30, 10);
    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "帶附加服務的會議",
                    "start_at": s, "end_at": e,
                    "services": [
                        { "service_item_id": SVC_TEA, "quantity": 8, "notes": "無糖",
                          "payload": { "headcount": 8, "beverage": "TEA" } },
                        { "service_item_id": SVC_AV }
                    ]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let reservation_id = created["id"].as_str().unwrap().to_string();

    // ---- 回應要帶 services（契約把它放在 ReservationDetail 上）----
    let svcs = created["services"]
        .as_array()
        .unwrap_or_else(|| panic!("POST 回應應含 services: {created}"));
    assert_eq!(svcs.len(), 2, "兩個服務都要出現：{created}");

    // ---- 班表：service_start_at = 預約開始 + relative_offset ----
    // 兩個項目的 offset 都是 -15，因此都比預約提前 15 分鐘。
    let start: chrono::DateTime<chrono::Utc> = s.parse().unwrap();
    let expected = start - chrono::Duration::minutes(15);
    for svc in svcs {
        let actual: chrono::DateTime<chrono::Utc> = svc["service_start_at"]
            .as_str()
            .unwrap_or_else(|| panic!("service_start_at 不該是 null（班表會是空的）: {svc}"))
            .parse()
            .unwrap();
        assert_eq!(actual, expected, "服務開始時刻應為預約開始 -15 分：{svc}");
        assert_eq!(svc["status"], "REQUESTED");
        assert!(svc["work_order"].is_null(), "fan-out worker 尚未實作");
    }

    // ---- 費用：TEA_SETUP 是 chargeable、單價 60，數量 8 → 480 ----
    // 契約沒有 estimated_cost 欄位，因此直接查資料庫。
    {
        let mut tx = ctx.tenant_tx().await;
        let cost: Option<f64> = sqlx::query_scalar(
            "SELECT estimated_cost::float8 FROM fms.reservation_services
              WHERE reservation_id = $1::uuid AND service_item_id = $2::uuid",
        )
        .bind(&reservation_id)
        .bind(SVC_TEA)
        .fetch_one(&mut *tx)
        .await
        .expect("read estimated_cost");
        assert_eq!(cost, Some(480.0), "chargeable 項目應算出 estimated_cost");

        // 不收費的項目不該憑空生出金額
        let av_cost: Option<f64> = sqlx::query_scalar(
            "SELECT estimated_cost::float8 FROM fms.reservation_services
              WHERE reservation_id = $1::uuid AND service_item_id = $2::uuid",
        )
        .bind(&reservation_id)
        .bind(SVC_AV)
        .fetch_one(&mut *tx)
        .await
        .expect("read estimated_cost");
        assert_eq!(av_cost, None, "非 chargeable 項目不該有金額");
    }

    // ---- GET 也要帶 services ----
    let (status, fetched) = ctx
        .send(authed(
            Request::builder()
                .uri(format!("/api/v1/reservations/{reservation_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(
        fetched["services"].as_array().map(|a| a.len()),
        Some(2),
        "GET 的 ReservationDetail 也要帶 services: {fetched}"
    );

    ctx.teardown().await;
}

#[tokio::test]
async fn service_item_rules_are_enforced() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    /// 送一筆帶單一服務的建立請求，回傳 (status, body)。
    async fn try_with(
        ctx: &TestContext,
        token: &str,
        day: i64,
        service: serde_json::Value,
    ) -> (StatusCode, Value) {
        let (s, e) = slot(day, 10);
        ctx.send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "服務規則測試",
                    "start_at": s, "end_at": e, "services": [service]
                }),
            ),
            token,
        ))
        .await
    }

    // ---- 不存在的 service_item_id ----
    let (status, body) = try_with(
        ctx,
        &token,
        31,
        json!({ "service_item_id": "60000000-0000-4000-8000-0000000000ff" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["pointer"], "/services/0/service_item_id");

    // ---- is_attachable_to_reservation = false ----
    // DEEP_CLEAN 只能獨立申請。附上去會產生一筆永遠不會被排程的服務。
    let (status, body) = try_with(
        ctx,
        &token,
        32,
        json!({ "service_item_id": SVC_DEEP_CLEAN }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["code"], "NOT_ATTACHABLE");

    // ---- 場域不符 ----
    // 把 DEEP_CLEAN 改成可附加，剩下的差異就只有場域（它屬影廳，ROOM_401 在總部）。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.service_items SET is_attachable_to_reservation = true WHERE id = $1::uuid",
        )
        .bind(SVC_DEEP_CLEAN)
        .execute(&mut *tx)
        .await
        .expect("make attachable");
        tx.commit().await.expect("commit");
    }
    let (status, body) = try_with(
        ctx,
        &token,
        33,
        json!({ "service_item_id": SVC_DEEP_CLEAN }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["code"], "WRONG_FACILITY");

    // ---- 前置時間不足 ----
    // TEA_SETUP 要 120 分鐘；把預約排在 30 分鐘後就不夠。
    let soon = chrono::Utc::now() + chrono::Duration::minutes(30);
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "前置時間不足",
                    "start_at": soon.to_rfc3339(),
                    "end_at": (soon + chrono::Duration::hours(1)).to_rfc3339(),
                    "services": [{ "service_item_id": SVC_TEA }]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["code"], "LEAD_TIME");

    // ---- max_quantity ----
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.service_items SET max_quantity = 10 WHERE id = $1::uuid")
            .bind(SVC_TEA)
            .execute(&mut *tx)
            .await
            .expect("set max_quantity");
        tx.commit().await.expect("commit");
    }
    let (status, body) = try_with(
        ctx,
        &token,
        34,
        json!({ "service_item_id": SVC_TEA, "quantity": 11 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["code"], "MAXIMUM");

    // ---- quantity 必須為正 ----
    let (status, body) = try_with(
        ctx,
        &token,
        35,
        json!({ "service_item_id": SVC_TEA, "quantity": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["code"], "MINIMUM");

    // ---- form_schema ----
    // 給 AV_PRECHECK 一個要求 room_code 的 schema，然後不帶它。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            r#"UPDATE fms.service_items
                  SET form_schema = '{"type":"object","required":["room_code"],
                                      "properties":{"room_code":{"type":"string"}}}'::jsonb
                WHERE id = $1::uuid"#,
        )
        .bind(SVC_AV)
        .execute(&mut *tx)
        .await
        .expect("set form_schema");
        tx.commit().await.expect("commit");
    }
    let (status, body) = try_with(ctx, &token, 36, json!({ "service_item_id": SVC_AV })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["code"], "SCHEMA_VIOLATION");
    assert_eq!(
        body["errors"][0]["pointer"], "/services/0/payload",
        "指標要指到出錯的那個服務，而不只是「payload 有錯」"
    );

    // 帶對了就通過
    let (status, body) = try_with(
        ctx,
        &token,
        37,
        json!({ "service_item_id": SVC_AV, "payload": { "room_code": "401" } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // ---- requires_approval → 服務進 PENDING_APPROVAL，不是 REQUESTED ----
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.service_items SET requires_approval = true WHERE id = $1::uuid")
            .bind(SVC_TEA)
            .execute(&mut *tx)
            .await
            .expect("set requires_approval");
        tx.commit().await.expect("commit");
    }
    let (status, body) = try_with(
        ctx,
        &token,
        38,
        json!({ "service_item_id": SVC_TEA, "payload": { "headcount": 4 } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["services"][0]["status"], "PENDING_APPROVAL",
        "requires_approval 的項目不能直接進 REQUESTED，否則核准那一關被靜默跳過"
    );

    ctx.teardown().await;
}

/// 週期預約（WBS 7.5 的最後一項）。
///
/// 契約：「RFC 5545 RRULE；伺服端展開為多筆預約並回傳 recurrence_group_id」。
/// 在此之前 `recurrence_rule` 被靜默忽略：只建立一筆，`recurrence_group_id`
/// 是 NULL，而客戶端以為自己訂了一整個系列。
#[tokio::test]
async fn a_recurrence_rule_expands_into_a_series() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 每週一次、共四次。用 COUNT 讓期望值不依賴 advance_booking_days。
    let (s, e) = slot(10, 10);
    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "每週例會",
                    "start_at": s, "end_at": e,
                    "recurrence_rule": "FREQ=WEEKLY;COUNT=4"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let group = created["recurrence_group_id"]
        .as_str()
        .unwrap_or_else(|| panic!("回應應帶 recurrence_group_id: {created}"))
        .to_string();

    // 回應描述的是**第一次**
    assert_eq!(
        created["start_at"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap(),
        s.parse::<chrono::DateTime<chrono::Utc>>().unwrap(),
        "回應應描述系列的第一次"
    );

    // 四筆都落地，時間間隔一週，且每一筆都帶同一個 group 與規則
    {
        let mut tx = ctx.tenant_tx().await;
        let rows: Vec<(chrono::DateTime<chrono::Utc>, Option<String>)> = sqlx::query_as(
            "SELECT start_at, recurrence_rule FROM fms.reservations
              WHERE recurrence_group_id = $1::uuid ORDER BY start_at",
        )
        .bind(&group)
        .fetch_all(&mut *tx)
        .await
        .expect("read series");
        assert_eq!(rows.len(), 4, "COUNT=4 應展開成四筆");
        for (i, (start, rule)) in rows.iter().enumerate() {
            let expected = s.parse::<chrono::DateTime<chrono::Utc>>().unwrap()
                + chrono::Duration::weeks(i as i64);
            assert_eq!(*start, expected, "第 {} 次的時間不對", i + 1);
            assert_eq!(
                rule.as_deref(),
                Some("FREQ=WEEKLY;COUNT=4"),
                "規則要寫在每一列上，否則單筆查詢無法自己回答屬於哪個系列"
            );
        }
    }

    ctx.teardown().await;
}

#[tokio::test]
async fn a_series_is_all_or_nothing() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 先佔掉「第三週」的同一個時段
    let (s, e) = slot(12, 14);
    let blocker_start =
        s.parse::<chrono::DateTime<chrono::Utc>>().unwrap() + chrono::Duration::weeks(2);
    let (status, blocker) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "卡住第三週",
                    "start_at": blocker_start.to_rfc3339(),
                    "end_at": (blocker_start + chrono::Duration::hours(1)).to_rfc3339()
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{blocker}");

    // 現在送四週的系列 —— 第三次會撞到
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "會撞到的系列",
                    "start_at": s, "end_at": e,
                    "recurrence_rule": "FREQ=WEEKLY;COUNT=4"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "RESERVATION_CONFLICT");
    // 訊息要指出**哪一次**：對 13 次的系列，「時段衝突」不足以讓呼叫端修好請求
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(&blocker_start.format("%Y-%m-%d").to_string()),
        "錯誤訊息要指出衝突的那一次，實際：{detail}"
    );

    // 全有或全無：一筆都不該落地（除了先前那個 blocker）
    {
        let mut tx = ctx.tenant_tx().await;
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM fms.reservations WHERE recurrence_group_id IS NOT NULL",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("count series rows");
        assert_eq!(n, 0, "被拒的系列不該留下任何一筆（部分成立比失敗更糟）");
    }

    ctx.teardown().await;
}

#[tokio::test]
async fn recurrence_rejects_incoherent_or_empty_requests() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // ---- hold_token 與 recurrence_rule 互斥 ----
    // 佔位鎖的是一個時段；靜默只用在第一次會讓呼叫端以為整個系列被保護過。
    let (s, e) = slot(55, 10);
    let (status, hold) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations/holds",
                json!({ "resource_id": ROOM_401, "start_at": s, "end_at": e }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{hold}");

    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "佔位 + 週期",
                    "start_at": s, "end_at": e,
                    "hold_token": hold["hold_token"].as_str().unwrap(),
                    "recurrence_rule": "FREQ=WEEKLY;COUNT=3"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 語法錯誤的 RRULE 是 422，不是 500 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_402, "title": "壞規則",
                    "start_at": s, "end_at": e,
                    "recurrence_rule": "FREQ=NOT_A_FREQUENCY"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "RRULE 是使用者輸入，語法錯誤不該是 500: {body}"
    );

    // ---- 展開後落在預約窗外 → 一筆都沒有，回 422 而不是靜默建立零筆 ----
    // ROOM_402 的 advance_booking_days 是 90 天（種子預設）；
    // UNTIL 設在昨天，因此窗內沒有任何一次。
    let yesterday = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y%m%dT000000Z");
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_402, "title": "窗外的系列",
                    "start_at": s, "end_at": e,
                    "recurrence_rule": format!("FREQ=WEEKLY;UNTIL={yesterday}")
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    ctx.teardown().await;
}

#[tokio::test]
async fn participants_are_persisted_and_returned() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let huang_id: uuid::Uuid = {
        let mut tx = ctx.tenant_tx().await;
        sqlx::query_scalar("SELECT id FROM fms.users WHERE username::text = $1")
            .bind("user.huang")
            .fetch_one(&mut *tx)
            .await
            .expect("user.huang should exist in demo data")
    };

    // ---- 缺身分的一筆先擋在建立之前，不該把整個交易一起送進 DB ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "缺身分的與會者",
                    "start_at": slot(11, 10).0, "end_at": slot(11, 10).1,
                    "participants": [ { "role": "ATTENDEE" } ]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "participant 缺 user_id 與 external_email 應該回 422: {body}"
    );

    // ---- 正常帶兩筆：一個內部使用者、一個外部 email ----
    let (s, e) = slot(11, 14);
    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "帶與會者的會議",
                    "start_at": s, "end_at": e,
                    "participants": [
                        { "user_id": huang_id, "role": "OPTIONAL" },
                        { "external_email": "guest@example.com" }
                    ]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let reservation_id = created["id"].as_str().unwrap().to_string();

    let assert_participants = |body: &Value| {
        let participants = body["participants"]
            .as_array()
            .unwrap_or_else(|| panic!("回應應含 participants: {body}"));
        assert_eq!(participants.len(), 2, "兩筆與會者都要出現：{body}");

        let by_user = participants
            .iter()
            .find(|p| p["user_id"] == json!(huang_id))
            .unwrap_or_else(|| panic!("找不到 user_id 那筆與會者: {body}"));
        assert_eq!(by_user["role"], "OPTIONAL");
        assert_eq!(by_user["response"], "PENDING");
        assert!(
            by_user["display_name"].is_string(),
            "user_id 型的與會者應該解出 display_name: {by_user}"
        );

        let by_email = participants
            .iter()
            .find(|p| p["external_email"] == json!("guest@example.com"))
            .unwrap_or_else(|| panic!("找不到 external_email 那筆與會者: {body}"));
        assert_eq!(by_email["role"], "ATTENDEE", "沒帶 role 應預設 ATTENDEE");
        assert!(by_email["user_id"].is_null());
    };

    // POST 回應本身就要帶
    assert_participants(&created);

    // GET 也要帶——不是只有建立當下才有
    let (status, fetched) = ctx
        .send(authed(
            Request::builder()
                .uri(format!("/api/v1/reservations/{reservation_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_participants(&fetched);

    ctx.teardown().await;
}

#[tokio::test]
async fn apply_scope_bulk_updates_the_series_but_rejects_time_changes() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 一個非週期預約：apply_scope != THIS 應該回 422（不屬於任何系列）。
    let (s, e) = slot(12, 9);
    let (status, plain) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({ "resource_id": ROOM_401, "title": "單筆", "start_at": s, "end_at": e }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{plain}");
    let plain_id = plain["id"].as_str().unwrap();
    let (status, body) = ctx
        .send(authed_if_match(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/reservations/{plain_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"title": "改", "apply_scope": "ALL"}).to_string(),
                ))
                .unwrap(),
            &token,
            &plain["version"].as_i64().unwrap().to_string(),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "非週期預約不該接受 apply_scope=ALL: {body}"
    );

    // 週期系列：每週一次、三次。
    let (s, e) = slot(13, 10);
    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/reservations",
                json!({
                    "resource_id": ROOM_401, "title": "原標題",
                    "start_at": s, "end_at": e,
                    "recurrence_rule": "FREQ=WEEKLY;COUNT=3"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let first_id = created["id"].as_str().unwrap().to_string();
    let group = created["recurrence_group_id"].as_str().unwrap().to_string();

    // apply_scope=ALL 同時帶 start_at → 422（時段沒有整系列一起改的語意）。
    let (status, body) = ctx
        .send(authed_if_match(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/reservations/{first_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"apply_scope": "ALL", "start_at": s}).to_string(),
                ))
                .unwrap(),
            &token,
            &created["version"].as_i64().unwrap().to_string(),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "apply_scope 非 THIS 時帶 start_at 應該回 422: {body}"
    );

    // apply_scope=ALL 改標題 → 三筆都要改到，不是只有目標那一筆。
    let (status, body) = ctx
        .send(authed_if_match(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/reservations/{first_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"apply_scope": "ALL", "title": "整系列改標題"}).to_string(),
                ))
                .unwrap(),
            &token,
            &created["version"].as_i64().unwrap().to_string(),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "整系列改標題");

    let mut tx = ctx.tenant_tx().await;
    let titles: Vec<String> = sqlx::query_scalar(
        "SELECT title FROM fms.reservations WHERE recurrence_group_id = $1::uuid ORDER BY start_at",
    )
    .bind(&group)
    .fetch_all(&mut *tx)
    .await
    .expect("read titles");
    assert_eq!(titles.len(), 3, "系列應該有三筆");
    assert!(
        titles.iter().all(|t| t == "整系列改標題"),
        "apply_scope=ALL 應該改到系列裡的每一筆：{titles:?}"
    );

    ctx.teardown().await;
}
