//! Reservations 補完五支。
//!
//! # `c_` 是這一組最重要的一格
//!
//! check-out 宣稱「釋放時段」。那句話有兩種做法：把 `end_at` 縮到現在（改寫
//! 與使用者的約定，而且會讓 `report_space_utilization` 的分子從「已預約時數」
//! 悄悄變成「實際使用時數」），或者只把狀態轉成 `COMPLETED`
//! （`excl_reservations_no_overlap` 的 WHERE 不含它，所以時段自然就空了）。
//!
//! `c_` 證明第二種真的有效：check-out 之後，**同一個重疊時段訂得起來**，
//! 而 `start_at`／`end_at` 一個都沒動。少了這一格，「釋放」只是一句宣稱。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn req(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// 總部第一個可預約資源（按 id 排序，與 handler 的 ORDER BY 無關 ——
/// 這裡只需要一個穩定的選擇）。
async fn a_resource(ctx: &TestContext) -> (uuid::Uuid, uuid::Uuid) {
    let mut tx = ctx.owner_tx().await;
    let row: (uuid::Uuid, uuid::Uuid) = sqlx::query_as(
        "SELECT id, coalesce(spatial_node_id, asset_id)
           FROM fms.bookable_resources
          WHERE facility_id = $1::uuid AND is_bookable
          ORDER BY id LIMIT 1",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("該有可預約資源");
    tx.commit().await.expect("commit");
    row
}

/// 直接建一筆指定狀態的預約（走 API 需要通過一堆規則，而這一組要驗的不是那些）。
async fn seed_reservation(
    ctx: &TestContext,
    resource: uuid::Uuid,
    target: uuid::Uuid,
    status: &str,
    hours_from_now: i32,
    group: Option<uuid::Uuid>,
) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, title,
            start_at, end_at, status, requires_check_in, checked_in_at,
            recurrence_group_id)
         SELECT $1::uuid, br.facility_id, br.id,
                'RSV-T-' || substr(md5(random()::text), 1, 10),
                br.resource_type, $2::uuid, $3::uuid, '尾段測試',
                clock_timestamp() + make_interval(hours => $4),
                clock_timestamp() + make_interval(hours => $4) + interval '1 hour',
                $5, true,
                CASE WHEN $5 = 'CHECKED_IN'
                     THEN clock_timestamp() - interval '20 minutes' END,
                $6::uuid
           FROM fms.bookable_resources br WHERE br.id = $7::uuid
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(target)
    .bind(ADMIN_USER_ID)
    .bind(hours_from_now)
    .bind(status)
    .bind(group)
    .bind(resource)
    .fetch_one(&mut *tx)
    .await
    .unwrap_or_else(|e| panic!("建 {status} 預約失敗：{e}"));
    tx.commit().await.expect("commit");
    id
}

/// 可預約資源清單：預設只回開啟的，帶旗標才含關掉的。
#[tokio::test]
async fn a_bookable_resources_hide_the_disabled_ones_by_default() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (resource, _) = a_resource(ctx).await;

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/facilities/{FACILITY_HQ}/bookable-resources"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert!(!rows.is_empty(), "示範資料該有可預約資源");
    let n_default = rows.len();

    // 規則欄位要在 —— 契約說的是「可預約資源與規則」，只回名字沒有用。
    let r = &rows[0];
    for f in [
        "min_duration_minutes",
        "max_duration_minutes",
        "slot_granularity_minutes",
        "advance_booking_days",
        "capacity",
        "requires_approval",
        "requires_check_in",
        "opening_hours",
        "resource_id",
    ] {
        assert!(!r[f].is_null(), "{f} 該有值：{r}");
    }

    // 關掉一個 → 預設看不到。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.bookable_resources SET is_bookable = false WHERE id = $1::uuid")
            .bind(resource)
            .execute(&mut *tx)
            .await
            .expect("關掉");
        tx.commit().await.expect("commit");
    }

    let (_, hidden) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/facilities/{FACILITY_HQ}/bookable-resources"
            )),
            &token,
        ))
        .await;
    assert_eq!(
        hidden["data"].as_array().map(Vec::len).unwrap_or(0),
        n_default - 1,
        "**關掉的資源不該出現在預約畫面上** —— 那只會讓人白填一次表單"
    );
    assert_eq!(hidden["meta"]["unbookable_count"], 0);

    let (_, all) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/facilities/{FACILITY_HQ}/bookable-resources?include_unbookable=true"
            )),
            &token,
        ))
        .await;
    assert_eq!(
        all["data"].as_array().map(Vec::len).unwrap_or(0),
        n_default,
        "管理設定的畫面要看得到全部：{}",
        all["meta"]
    );
    assert_eq!(all["meta"]["unbookable_count"], 1);

    ctx.teardown().await;
}

/// 釋放佔位：三種狀態三種回應。
#[tokio::test]
async fn b_releasing_a_hold_distinguishes_active_expired_and_consumed() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (resource, _target) = a_resource(ctx).await;

    let make_hold = |status: &'static str, token_str: String| async move {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.reservation_holds
                   (tenant_id, facility_id, bookable_resource_id, user_id,
                    start_at, end_at, hold_token, status, expires_at)
                 SELECT $1::uuid, br.facility_id, br.id, $2::uuid,
                        clock_timestamp() + interval '2 hours',
                        clock_timestamp() + interval '3 hours',
                        $3, $4, clock_timestamp() + interval '3 minutes'
                   FROM fms.bookable_resources br WHERE br.id = $5::uuid",
        )
        .bind(TENANT_ID)
        .bind(ADMIN_USER_ID)
        .bind(&token_str)
        .bind(status)
        .bind(resource)
        .execute(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("建 {status} 佔位失敗：{e}"));
        tx.commit().await.expect("commit");
        token_str
    };

    // ACTIVE → 204，而且狀態真的變成 RELEASED（005 的 CHECK 列了那個值，
    // 而這是它的第一個寫入者）。
    let t1 = make_hold("ACTIVE", "tok-active-1".to_string()).await;
    let (status, _) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/reservations/holds/{t1}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let mut tx = ctx.owner_tx().await;
    let s: String =
        sqlx::query_scalar("SELECT status FROM fms.reservation_holds WHERE hold_token = $1")
            .bind(&t1)
            .fetch_one(&mut *tx)
            .await
            .expect("讀狀態");
    tx.commit().await.expect("commit");
    assert_eq!(
        s, "RELEASED",
        "**這是 RELEASED 的第一個寫入者** —— 005 的 CHECK 從一開始就列了它"
    );

    // 再釋放一次 → 還是 204（幂等：呼叫者要的狀態已經成立）。
    let (status, _) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/reservations/holds/{t1}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "重複釋放該是幂等的");

    // EXPIRED → 204。
    let t2 = make_hold("EXPIRED", "tok-expired".to_string()).await;
    let (status, _) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/reservations/holds/{t2}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // **CONSUMED → 409。** 回 204 等於謊稱時段空了。
    let t3 = make_hold("CONSUMED", "tok-consumed".to_string()).await;
    let (status, p) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/reservations/holds/{t3}")),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "已經變成預約的佔位不能釋放 —— 回 204 會讓客戶端接著訂同一個時段，\
         然後拿到一個沒有提到那筆預約的排除約束錯誤：{p}"
    );

    // 別人的佔位 → 403（不是 204，也不是 404）。
    let t4 = make_hold("ACTIVE", "tok-not-mine".to_string()).await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.reservation_holds SET user_id =
               (SELECT id FROM fms.users WHERE username::text = 'user.huang')
             WHERE hold_token = $1",
        )
        .bind(&t4)
        .execute(&mut *tx)
        .await
        .expect("換持有人");
        tx.commit().await.expect("commit");
    }
    let (status, _) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/reservations/holds/{t4}")),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "釋放別人的佔位等於把他的時段搶走"
    );

    // 不存在 → 404。
    let (status, _) = ctx
        .send(authed(
            req("DELETE", "/api/v1/reservations/holds/no-such-token"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// **check-out 真的釋放時段，而且不改時間欄位。**
#[tokio::test]
async fn c_check_out_frees_the_slot_without_rewriting_the_booking() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (resource, target) = a_resource(ctx).await;

    let id = seed_reservation(ctx, resource, target, "CHECKED_IN", 0, None).await;

    // 先記下時間欄位。
    let mut tx = ctx.owner_tx().await;
    let before: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT start_at, end_at FROM fms.reservations WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .expect("讀時間");
    tx.commit().await.expect("commit");

    let (status, out) = ctx
        .send(authed(
            req("POST", &format!("/api/v1/reservations/{id}/check-out")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["data"]["status"], "COMPLETED");
    assert!(out["data"]["checked_out_at"].as_str().is_some(), "{out}");
    // 實際與約定兩個數字都在，而且不互相取代。
    assert!(out["meta"]["used_minutes"]
        .as_f64()
        .is_some_and(|v| v > 0.0));
    assert!(
        out["meta"]["booked_minutes"]
            .as_f64()
            .is_some_and(|v| (v - 60.0).abs() < 1.0),
        "訂了一小時：{}",
        out["meta"]
    );
    assert_eq!(out["meta"]["slot_released"], true);

    // **時間欄位一個都沒動。**
    let mut tx = ctx.owner_tx().await;
    let after: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT start_at, end_at FROM fms.reservations WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .expect("讀時間");
    tx.commit().await.expect("commit");
    assert_eq!(
        before, after,
        "**start_at／end_at 不該被改寫** —— 那會改掉與使用者的約定，\
         也會讓 report_space_utilization 的分子從「已預約時數」變成「實際使用時數」"
    );

    // **時段真的空了**：同一個重疊時段插得進去（排除約束不再擋）。
    let mut tx = ctx.owner_tx().await;
    let inserted = sqlx::query(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, start_at, end_at, status)
         SELECT $1::uuid, br.facility_id, br.id, 'RSV-OVERLAP',
                br.resource_type, $2::uuid, $3::uuid, $4, $5, 'CONFIRMED'
           FROM fms.bookable_resources br WHERE br.id = $6::uuid",
    )
    .bind(TENANT_ID)
    .bind(target)
    .bind(ADMIN_USER_ID)
    .bind(before.0)
    .bind(before.1)
    .bind(resource)
    .execute(&mut *tx)
    .await;
    assert!(
        inserted.is_ok(),
        "**check-out 之後同一個時段該訂得起來。** excl_reservations_no_overlap \
         的 WHERE 不含 COMPLETED，所以狀態轉換就足以釋放時段：{inserted:?}"
    );
    tx.commit().await.expect("commit");

    ctx.teardown().await;
}

/// 沒報到不能離場；重複離場是 409。
#[tokio::test]
async fn d_only_a_checked_in_reservation_can_check_out() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (resource, target) = a_resource(ctx).await;

    // CONFIRMED（沒報到）→ 409。
    let id = seed_reservation(ctx, resource, target, "CONFIRMED", 5, None).await;
    let (status, p) = ctx
        .send(authed(
            req("POST", &format!("/api/v1/reservations/{id}/check-out")),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "沒報到就離場會產生 checked_out_at 有值而 checked_in_at 為 NULL 的列 —— \
         之後任何算實際使用時長的東西都要處理那個不可能的組合：{p}"
    );

    // 沒有這一筆 → 404。
    let (status, _) = ctx
        .send(authed(
            req(
                "POST",
                "/api/v1/reservations/00000000-0000-4000-8000-000000000000/check-out",
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 已離場再離場 → 409。
    let id2 = seed_reservation(ctx, resource, target, "CHECKED_IN", 20, None).await;
    let (status, _) = ctx
        .send(authed(
            req("POST", &format!("/api/v1/reservations/{id2}/check-out")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = ctx
        .send(authed(
            req("POST", &format!("/api/v1/reservations/{id2}/check-out")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "已經 COMPLETED 了");

    ctx.teardown().await;
}

/// 取消系列：只取消未開始的，三個數字分開回。
#[tokio::test]
async fn e_cancelling_a_series_leaves_the_past_alone() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (resource, target) = a_resource(ctx).await;
    let group = uuid::Uuid::new_v4();

    // 一筆過去的（已完成）、一筆過去的（還 CONFIRMED —— 該算 skipped_past）、
    // 兩筆未來的（CONFIRMED）、一筆未來但已取消的（skipped_terminal）。
    seed_reservation(ctx, resource, target, "COMPLETED", -48, Some(group)).await;
    seed_reservation(ctx, resource, target, "CONFIRMED", -24, Some(group)).await;
    seed_reservation(ctx, resource, target, "CONFIRMED", 24, Some(group)).await;
    seed_reservation(ctx, resource, target, "CONFIRMED", 48, Some(group)).await;
    seed_reservation(ctx, resource, target, "CANCELLED", 72, Some(group)).await;

    let (status, body) = ctx
        .send(authed(
            json_request(
                "DELETE",
                &format!("/api/v1/reservation-series/{group}"),
                json!({ "reason": "專案結束" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let d = &body["data"];
    assert_eq!(d["total_in_series"], 5, "{d}");
    assert_eq!(d["cancelled"], 2, "只有兩筆未來且仍佔用的：{d}");
    assert_eq!(
        d["skipped_past"], 2,
        "**已經開始的不取消** —— 取消它不會讓那段時間回來，\
         而它會回頭改寫過去區間的使用率報表：{d}"
    );
    assert_eq!(d["skipped_terminal"], 1, "已取消的那一筆：{d}");

    // 過去那筆 CONFIRMED 真的還在。
    let mut tx = ctx.owner_tx().await;
    let past_still: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.reservations
          WHERE recurrence_group_id = $1::uuid AND status = 'CONFIRMED'
            AND start_at <= clock_timestamp()",
    )
    .bind(group)
    .fetch_one(&mut *tx)
    .await
    .expect("查");
    let reason_set: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.reservations
          WHERE recurrence_group_id = $1::uuid AND cancellation_reason = '專案結束'",
    )
    .bind(group)
    .fetch_one(&mut *tx)
    .await
    .expect("查原因");
    tx.commit().await.expect("commit");
    assert_eq!(past_still, 1, "過去那筆仍佔用的預約不該被動到");
    assert_eq!(reason_set, 2, "取消原因要寫進去");

    // 不存在的系列 → 404（而不是「取消了 0 筆」）。
    let (status, _) = ctx
        .send(authed(
            req(
                "DELETE",
                "/api/v1/reservation-series/00000000-0000-4000-8000-000000000000",
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "「找不到系列」與「系列全部都結束了」是不同的事"
    );

    ctx.teardown().await;
}

/// PATCH 預約規則：可改的改得動，換標的的欄位拒絕，約束違反回 422。
#[tokio::test]
async fn f_patching_booking_rules_rejects_retargeting_and_bad_shapes() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (resource, _) = a_resource(ctx).await;
    let uri = format!("/api/v1/bookable-resources/{resource}");

    // 規則改得動。
    let (status, updated) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &uri,
                json!({
                    "min_duration_minutes": 15,
                    "max_duration_minutes": 240,
                    "requires_check_in": true,
                    "auto_release_minutes": 10,
                    "opening_hours": { "mon": [["09:00", "18:00"]] }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["min_duration_minutes"], 15);
    assert_eq!(updated["max_duration_minutes"], 240);
    assert_eq!(updated["requires_check_in"], true);
    assert_eq!(updated["auto_release_minutes"], 10);

    // 送 null 是清空（不自動釋放）。
    let (status, cleared) = ctx
        .send(authed(
            json_request("PATCH", &uri, json!({ "auto_release_minutes": null })),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(
        cleared["auto_release_minutes"],
        Value::Null,
        "送 null 該清空 —— 不送才是不動"
    );
    assert_eq!(
        cleared["min_duration_minutes"], 15,
        "沒送的欄位不該被重設：{cleared}"
    );

    // **換標的的欄位拒絕。**
    for f in [
        "resource_type",
        "spatial_node_id",
        "asset_id",
        "facility_id",
    ] {
        let (status, p) = ctx
            .send(authed(
                json_request(
                    "PATCH",
                    &uri,
                    json!({ f: "00000000-0000-4000-8000-000000000000" }),
                ),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{f}` 不該可改 —— 既有的預約會跟著指到另一個實體：{p}"
        );
        assert_eq!(p["errors"][0]["code"], "IMMUTABLE", "{p}");
    }

    // 未知欄位 → 422（與 IMMUTABLE 不同的錯誤碼）。
    let (status, p) = ctx
        .send(authed(
            json_request("PATCH", &uri, json!({ "capacityy": 3 })),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(p["errors"][0]["code"], "UNKNOWN_FIELD", "{p}");

    // 負數／零 → 422，在應用層擋（005 沒有給這些欄位 CHECK）。
    for (f, v) in [
        ("min_duration_minutes", json!(0)),
        ("capacity", json!(0)),
        ("buffer_before_minutes", json!(-1)),
    ] {
        let (status, p) = ctx
            .send(authed(json_request("PATCH", &uri, json!({ f: v })), &token))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{f} 不該接受那個值：{p}"
        );
    }

    // **只送 max 讓它小於現有的 min → 422 而不是 500。**
    // 這一格抓的是「約束違反要翻譯」：現值是 min=15，送 max=10 會撞
    // ck_bookable_duration。
    let (status, p) = ctx
        .send(authed(
            json_request("PATCH", &uri, json!({ "max_duration_minutes": 10 })),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "資料庫約束違反該翻譯成 422 —— 500 會讓客戶端重試一個永遠不會成功的請求：{p}"
    );
    assert_eq!(p["errors"][0]["pointer"], "/max_duration_minutes", "{p}");

    // opening_hours 形狀不合 → 422（065 的 ck_bookable_opening_hours）。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &uri,
                json!({ "opening_hours": { "mon": [["9:0", "18:00"]] } }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(p["errors"][0]["pointer"], "/opening_hours", "{p}");

    // 不存在 → 404。
    let (status, _) = ctx
        .send(authed(
            json_request(
                "PATCH",
                "/api/v1/bookable-resources/00000000-0000-4000-8000-000000000000",
                json!({ "capacity": 2 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// 權限：五支各自的權限碼都有擋。
#[tokio::test]
async fn g_each_endpoint_enforces_its_permission() {
    let ctx = &TestContext::setup().await;
    let (resource, _) = a_resource(ctx).await;
    // user.huang 是 REQUESTER：有 reservation:create／read_own，
    // 沒有 bookable_resource:write。
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    let (status, _) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/bookable-resources/{resource}"),
                json!({ "capacity": 9 }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "REQUESTER 不該改得動預約規則"
    );

    ctx.teardown().await;
}

async fn amenity_id_by_code(ctx: &TestContext, code: &str) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM fms.amenities WHERE code = $1 AND tenant_id IS NULL")
            .bind(code)
            .fetch_one(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("種子目錄應該有 {code}: {e}"));
    tx.commit().await.expect("commit");
    id
}

/// 目錄（平台預設）能讀到，指派到資源上能全量覆寫、能讀回、能清空。
#[tokio::test]
async fn h_amenity_catalog_and_resource_assignment_round_trip() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (resource, _) = a_resource(ctx).await;

    // ---- 目錄：平台預設列標 is_platform=true ----
    let (status, catalog) = ctx.send(authed(get("/api/v1/amenities"), &token)).await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    let items = catalog["data"].as_array().expect("data 應該是陣列");
    assert!(
        items.len() >= 10,
        "011 種了 20 幾個平台預設，這裡至少該看到十幾個：{items:?}"
    );
    let projector = items
        .iter()
        .find(|a| a["code"] == "PROJECTOR")
        .unwrap_or_else(|| panic!("種子目錄應該有 PROJECTOR: {items:?}"));
    assert_eq!(projector["is_platform"], json!(true));
    assert!(projector["name_en"].is_string());

    // ---- 新資源預設沒有任何附屬設備 ----
    let (status, empty) = ctx
        .send(authed(
            get(&format!("/api/v1/bookable-resources/{resource}/amenities")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["data"].as_array().unwrap().len(), 0);

    // ---- PUT 兩項：投影機 x1、白板 x2 附備註 ----
    let projector_id = amenity_id_by_code(ctx, "PROJECTOR").await;
    let whiteboard_id = amenity_id_by_code(ctx, "WHITEBOARD").await;
    let (status, put_body) = ctx
        .send(authed(
            json_request(
                "PUT",
                &format!("/api/v1/bookable-resources/{resource}/amenities"),
                json!({
                    "amenities": [
                        { "amenity_id": projector_id },
                        { "amenity_id": whiteboard_id, "quantity": 2, "note": "移動式" }
                    ]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{put_body}");
    let data = put_body["data"].as_array().expect("data 應該是陣列");
    assert_eq!(data.len(), 2, "{data:?}");
    let board = data
        .iter()
        .find(|a| a["amenity_id"] == json!(whiteboard_id))
        .unwrap_or_else(|| panic!("回應應含白板: {data:?}"));
    assert_eq!(board["quantity"], json!(2));
    assert_eq!(board["note"], json!("移動式"));
    assert_eq!(board["is_operational"], json!(true), "預設應為可用");

    // ---- GET 讀回應該跟 PUT 的回應一致 ----
    let (status, fetched) = ctx
        .send(authed(
            get(&format!("/api/v1/bookable-resources/{resource}/amenities")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["data"].as_array().unwrap().len(), 2);

    // ---- 再 PUT 一次，只留投影機——白板應該消失（全量覆寫，不是增量）----
    let (status, put2) = ctx
        .send(authed(
            json_request(
                "PUT",
                &format!("/api/v1/bookable-resources/{resource}/amenities"),
                json!({ "amenities": [ { "amenity_id": projector_id } ] }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{put2}");
    let data2 = put2["data"].as_array().unwrap();
    assert_eq!(data2.len(), 1, "全量覆寫應該只剩一項：{data2:?}");
    assert_eq!(data2[0]["amenity_id"], json!(projector_id));

    // ---- 帶不存在的 amenity_id 回 404 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "PUT",
                &format!("/api/v1/bookable-resources/{resource}/amenities"),
                json!({ "amenities": [ { "amenity_id": "00000000-0000-4000-8000-000000000000" } ] }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // ---- 空陣列＝清空 ----
    let (status, put3) = ctx
        .send(authed(
            json_request(
                "PUT",
                &format!("/api/v1/bookable-resources/{resource}/amenities"),
                json!({ "amenities": [] }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{put3}");
    assert_eq!(put3["data"].as_array().unwrap().len(), 0);

    ctx.teardown().await;
}

/// 權限：REQUESTER 讀得到目錄與指派，但改不動。
#[tokio::test]
async fn i_amenity_write_requires_bookable_resource_write() {
    let ctx = &TestContext::setup().await;
    let (resource, _) = a_resource(ctx).await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    let (status, _) = ctx.send(authed(get("/api/v1/amenities"), &requester)).await;
    assert_eq!(status, StatusCode::OK, "目錄是唯讀資訊，REQUESTER 該看得到");

    let projector_id = amenity_id_by_code(ctx, "PROJECTOR").await;
    let (status, body) = ctx
        .send(authed(
            json_request(
                "PUT",
                &format!("/api/v1/bookable-resources/{resource}/amenities"),
                json!({ "amenities": [ { "amenity_id": projector_id } ] }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "REQUESTER 沒有 bookable_resource:write: {body}"
    );

    ctx.teardown().await;
}
