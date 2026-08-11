//! 裝置與通訊點（`/devices`）。
//!
//! # `a_` 與 `b_` 是這個檔案的核心
//!
//! `devices.status` 在第一筆讀數之後永遠是 `ONLINE` —— `ingest_telemetry()` 會
//! 把它翻上去，而**沒有任何東西把它翻回 `OFFLINE`**（那要靠 057 明確未做的
//! `DEVICE_OFFLINE` 掃描規則）。所以「連線狀態」不能讀那一欄。
//!
//! `a_` 釘住「算出來的 connectivity 與儲存的 status 會分歧」，
//! `b_` 釘住「門檻來自裝置自己的 `offline_alarm_after_seconds`」——
//! 少了 `b_`，把門檻換成任何寫死的數字都不會有測試反應。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn patch(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn hq_node(ctx: &TestContext) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id =
        sqlx::query_scalar("SELECT id FROM fms.spatial_nodes WHERE facility_id = $1::uuid LIMIT 1")
            .bind(FACILITY_HQ)
            .fetch_one(&mut *tx)
            .await
            .expect("總部該有空間節點");
    tx.commit().await.expect("commit");
    id
}

/// 直接設定某台裝置的 `last_seen_at`／`status`／門檻。
async fn set_device(
    ctx: &TestContext,
    id: uuid::Uuid,
    seen_secs_ago: Option<i64>,
    status: &str,
    threshold: i32,
) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.devices
            SET last_seen_at = CASE WHEN $2::bigint IS NULL THEN NULL
                                    ELSE now() - ($2::bigint || ' seconds')::interval END,
                status = $3,
                offline_alarm_after_seconds = $4
          WHERE id = $1::uuid",
    )
    .bind(id)
    .bind(seen_secs_ago)
    .bind(status)
    .bind(threshold)
    .execute(&mut *tx)
    .await
    .expect("設定裝置");
    tx.commit().await.expect("commit");
}

async fn create_device(ctx: &TestContext, token: &str, code: &str) -> uuid::Uuid {
    let node = hq_node(ctx).await;
    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/devices",
                json!({
                    "facility_id": FACILITY_HQ,
                    "device_code": code,
                    "name": "測試裝置",
                    "device_type": "SENSOR",
                    "spatial_node_id": node,
                }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    uuid::Uuid::parse_str(body["id"].as_str().expect("id")).expect("uuid")
}

async fn fetch_device(ctx: &TestContext, token: &str, code: &str) -> Value {
    let (status, body) = ctx.send(authed(get("/api/v1/devices"), token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|d| d["device_code"] == code)
        .cloned()
        .unwrap_or_else(|| panic!("找不到 {code}：{}", body["data"]))
}

/// **算出來的 connectivity 與儲存的 status 會分歧，而分歧時 connectivity 才是對的。**
///
/// 這條同時是「直接回傳 status」的突變測試：那樣做的話一台一小時沒回報的裝置
/// 會顯示 ONLINE。
#[tokio::test]
async fn a_connectivity_is_derived_not_the_stored_status() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_device(&ctx, &token, "CONN_A").await;

    // status 說 ONLINE，但一小時沒回報，門檻 900 秒。
    set_device(&ctx, id, Some(3_600), "ONLINE", 900).await;
    let d = fetch_device(&ctx, &token, "CONN_A").await;
    assert_eq!(d["status"], "ONLINE", "儲存的欄位仍是 ONLINE（沒有人翻它）");
    assert_eq!(
        d["connectivity"], "OFFLINE",
        "一小時沒回報就是離線，不管 status 寫什麼；實際 {d}"
    );
    assert!(d["seconds_since_seen"].as_i64().unwrap_or(0) >= 3_599);

    // 剛剛回報過 → ONLINE。
    set_device(&ctx, id, Some(10), "ONLINE", 900).await;
    let d = fetch_device(&ctx, &token, "CONN_A").await;
    assert_eq!(d["connectivity"], "ONLINE");

    // 從未回報 → NEVER_SEEN，而 seconds_since_seen 是 null 不是 0。
    set_device(&ctx, id, None, "UNKNOWN", 900).await;
    let d = fetch_device(&ctx, &token, "CONN_A").await;
    assert_eq!(d["connectivity"], "NEVER_SEEN");
    assert_eq!(
        d["seconds_since_seen"],
        Value::Null,
        "「從未回報」與「剛剛回報」不能混成同一個數字"
    );

    ctx.teardown().await;
}

/// **門檻是每台裝置自己的。** 同一個 `last_seen_at`，門檻一改答案就翻。
#[tokio::test]
async fn b_the_offline_threshold_comes_from_the_device() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_device(&ctx, &token, "CONN_B").await;

    // 600 秒沒回報。門檻 300 → 離線。
    set_device(&ctx, id, Some(600), "ONLINE", 300).await;
    assert_eq!(
        fetch_device(&ctx, &token, "CONN_B").await["connectivity"],
        "OFFLINE"
    );

    // 同一筆 last_seen_at，門檻放寬到 1800 → 上線。
    set_device(&ctx, id, Some(600), "ONLINE", 1_800).await;
    let d = fetch_device(&ctx, &token, "CONN_B").await;
    assert_eq!(
        d["connectivity"], "ONLINE",
        "門檻放寬後同一筆 last_seen_at 不該是離線 —— 否則門檻沒有被讀；實際 {d}"
    );
    assert_eq!(
        d["offline_alarm_after_seconds"], 1_800,
        "門檻要回傳，否則前端無法解釋這個判定"
    );

    ctx.teardown().await;
}

/// 行政狀態直接透傳 —— 「我們故意關掉它」不該被「它沒在回報」蓋掉。
#[tokio::test]
async fn c_administrative_statuses_pass_through() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_device(&ctx, &token, "CONN_C").await;

    for admin_status in ["DISABLED", "MAINTENANCE"] {
        set_device(&ctx, id, Some(86_000), admin_status, 300).await;
        let d = fetch_device(&ctx, &token, "CONN_C").await;
        assert_eq!(
            d["connectivity"], admin_status,
            "{admin_status} 該透傳，而不是被算成 OFFLINE；實際 {d}"
        );
    }

    // 而且它們不該出現在 offline_only 裡 —— 那份清單是「該修的東西」。
    let (_s, body) = ctx
        .send(authed(get("/api/v1/devices?offline_only=true"), &token))
        .await;
    let codes: Vec<&str> = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|d| d["device_code"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !codes.contains(&"CONN_C"),
        "刻意停用的裝置不該出現在待修清單裡：{codes:?}"
    );

    ctx.teardown().await;
}

/// `offline_only` 找得到「從未回報」的裝置 —— 用 `status=OFFLINE` 問會是空的。
#[tokio::test]
async fn d_offline_only_finds_devices_that_never_reported() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    create_device(&ctx, &token, "CONN_D").await;

    let (_s, body) = ctx
        .send(authed(get("/api/v1/devices?offline_only=true"), &token))
        .await;
    let codes: Vec<&str> = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|d| d["device_code"].as_str().unwrap_or(""))
        .collect();
    assert!(
        codes.contains(&"CONN_D"),
        "新註冊而從未回報的裝置該在待修清單裡：{codes:?}"
    );

    // 對照：問儲存的 status 會漏掉它（它是 UNKNOWN，不是 OFFLINE）。
    let (_s, body) = ctx
        .send(authed(get("/api/v1/devices?status=OFFLINE"), &token))
        .await;
    let codes: Vec<&str> = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|d| d["device_code"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !codes.contains(&"CONN_D"),
        "這是對照組：status=OFFLINE 問不到它，所以那不是正確的問法"
    );

    ctx.teardown().await;
}

/// 建立時的驗證：沒有目標、重複編號、未知型別。
#[tokio::test]
async fn e_creation_rejects_devices_that_cannot_own_their_readings() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    // 沒有 asset_id 也沒有 spatial_node_id → 422（不是 500）。
    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/devices",
                json!({
                    "facility_id": FACILITY_HQ,
                    "device_code": "NO_TARGET",
                    "name": "無目標",
                    "device_type": "SENSOR",
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or("").contains("監測"),
        "訊息該說出理由，而不是回一個約束名稱：{body}"
    );

    create_device(&ctx, &token, "DUP_CODE").await;

    // 重複編號 → 409，而且不分大小寫（索引是 lower(device_code)）。
    let node = hq_node(&ctx).await;
    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/devices",
                json!({
                    "facility_id": FACILITY_HQ,
                    "device_code": "dup_code",
                    "name": "重複",
                    "device_type": "SENSOR",
                    "spatial_node_id": node,
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "device_code 唯一性不分大小寫；實際 {body}"
    );

    // 未知型別 → 422。
    let (status, _) = ctx
        .send(authed(
            post(
                "/api/v1/devices",
                json!({
                    "facility_id": FACILITY_HQ,
                    "device_code": "BAD_TYPE",
                    "name": "型別錯",
                    "device_type": "TOASTER",
                    "spatial_node_id": node,
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 門檻超過一天 → 422（等於關掉離線判定，該用 DISABLED 表達）。
    let (status, _) = ctx
        .send(authed(
            post(
                "/api/v1/devices",
                json!({
                    "facility_id": FACILITY_HQ,
                    "device_code": "HUGE_THRESHOLD",
                    "name": "門檻過大",
                    "device_type": "SENSOR",
                    "spatial_node_id": node,
                    "offline_alarm_after_seconds": 999_999,
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// PATCH 只動給的欄位；範圍外的裝置是 404 而不是空更新。
#[tokio::test]
async fn f_patch_touches_only_what_was_sent() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_device(&ctx, &token, "PATCH_ME").await;
    set_device(&ctx, id, Some(10), "ONLINE", 450).await;

    let (status, body) = ctx
        .send(authed(
            patch(
                &format!("/api/v1/devices/{id}"),
                json!({ "name": "改過的名字" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "改過的名字");
    assert_eq!(
        body["offline_alarm_after_seconds"], 450,
        "沒給的欄位不該被改；實際 {body}"
    );
    assert_eq!(body["device_code"], "PATCH_ME");

    // 不存在的裝置 → 404。
    let (status, _) = ctx
        .send(authed(
            patch(
                &format!("/api/v1/devices/{}", uuid::Uuid::new_v4()),
                json!({ "name": "x" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// 點位清單帶最新值；看不到的裝置是 404 而不是空清單。
#[tokio::test]
async fn g_points_carry_latest_values_and_missing_devices_are_404() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    // 示範資料的 AHU 裝置有兩個點位。
    let mut tx = ctx.owner_tx().await;
    let device: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM fms.devices WHERE device_code = 'SNS_AHU_4F_01'")
            .fetch_one(&mut *tx)
            .await
            .expect("示範裝置");
    tx.commit().await.expect("commit");

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/devices/{device}/points")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "AHU 該有兩個點位；實際 {}", body["items"]);
    // 欄位存在（值可能是 null —— 示範資料沒有讀數）。
    assert!(items[0].get("last_observed_at").is_some());
    assert!(items[0].get("asset_meter_id").is_some());

    // 不存在的裝置 → 404，**不是空清單**：那兩者無法區分。
    let (status, _) = ctx
        .send(authed(
            get(&format!("/api/v1/devices/{}/points", uuid::Uuid::new_v4())),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// 沒有 `device:write` 的人建不了裝置。
#[tokio::test]
async fn h_permissions_are_enforced() {
    let ctx = TestContext::setup().await;
    let token = ctx.login_as(USERNAME_REQUESTER).await;
    let node = hq_node(&ctx).await;

    let (status, _) = ctx
        .send(authed(
            post(
                "/api/v1/devices",
                json!({
                    "facility_id": FACILITY_HQ,
                    "device_code": "FORBIDDEN",
                    "name": "不該建得起來",
                    "device_type": "SENSOR",
                    "spatial_node_id": node,
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // REQUESTER 連 device:read 都沒有。
    let (status, _) = ctx.send(authed(get("/api/v1/devices"), &token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    ctx.teardown().await;
}
