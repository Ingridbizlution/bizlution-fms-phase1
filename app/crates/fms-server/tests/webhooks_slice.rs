//! `GET /webhooks`、`POST /webhooks`，以及扇出與投遞的可測部分。
//!
//! # 沒有 HTTPS 的端到端投遞測試 —— 而那是刻意的
//!
//! 投遞路徑上的 SSRF 閘門**強制 https**。測試裡起一個有效憑證的 TLS 伺服器需要
//! 簽發憑證並把根憑證塞進客戶端，是一整套機具。
//!
//! 第一版試著用 http 的模擬伺服器繞過去，結果 `WebhookDispatcher` 正確地把它
//! 判成 `PERMANENT: scheme 是 http` —— **那不是測試環境的障礙，那是閘門真的在
//! 投遞路徑上**。`g_` 因此保留下來當那件事的守門人。
//!
//! 覆蓋改成這樣分配：
//!
//! | 什麼 | 在哪裡 |
//! |---|---|
//! | 簽章的位元組（含固定向量） | `fms-worker` 的 `webhook::tests`（6 格） |
//! | 狀態碼分類（4xx 永久 vs 5xx 重試） | 同上，`classify` 的單元測試 |
//! | 位址判斷的網段表 | `fms-shared` 的 `safe_http::tests` |
//! | 扇出、幂等、停用、閘門在投遞路徑上 | 這裡 |
//!
//! **沒有被任何測試覆蓋的**只剩「真的完成一次 HTTPS 往返」那一段。
//! 明寫在這裡，不假裝有覆蓋。
//!
//! # `c_` 守的是「金鑰讀不回來」
//!
//! 072 對 `fms_app` 做了欄位級 `REVOKE SELECT (signing_secret)`。`c_` 斷言
//! 清單與更新的回應都沒有它 —— 而那不是靠程式碼記得不要選，是靠資料庫權限。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 009 的示範規則與計量點（觸發 `alarm.raised`）。
const RULE_HVAC: &str = "a4000000-0000-4000-8000-000000000001";
const POINT_HVAC: &str = "a3000000-0000-4000-8000-000000000002";

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

async fn upsert(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(post("/api/v1/webhooks", body), token))
        .await
}

async fn list(ctx: &TestContext, token: &str, query: &str) -> (StatusCode, Value) {
    ctx.send(authed(get(&format!("/api/v1/webhooks{query}")), token))
        .await
}

/// 觸發一個 `alarm.raised` 事件。
async fn raise_alarm(ctx: &TestContext) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("SELECT fms.set_context($1::uuid, NULL, false)")
        .bind(TENANT_ID)
        .execute(&mut *tx)
        .await
        .expect("set_context");
    let _: uuid::Uuid =
        sqlx::query_scalar("SELECT fms.raise_alarm($1::uuid, $2::uuid, $3::numeric, $4)")
            .bind(RULE_HVAC)
            .bind(POINT_HVAC)
            .bind(41.0_f64)
            .bind("webhook 測試用告警")
            .fetch_one(&mut *tx)
            .await
            .expect("raise_alarm");
    tx.commit().await.expect("commit");
}

/// 直接建一筆訂閱（繞過端點的 https 限制，供投遞測試用）。
///
/// 端點刻意只接受 https（見 072 的 CHECK 與 SSRF 閘門），而測試的接收端是
/// http —— 因此投遞那幾格從資料庫建訂閱。這**不會**繞過投遞時的檢查：
/// `WebhookDispatcher` 每次送出前都會再跑一次閘門。
async fn insert_subscription(ctx: &TestContext, url: &str, secret: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    // 072 的 CHECK 只允許 https，而測試的接收端是 http，所以在**這個測試自己的
    // 資料庫副本**裡把那條約束拿掉（`TestContext::setup` 每個測試複製一份新的
    // 資料庫，因此不會影響 template 或別的測試）。
    //
    // **不加回去**：第一版用 `NOT VALID` 重新加上，而 `NOT VALID` 只是不回頭
    // 驗證既有列 —— 它對**之後的 UPDATE** 仍然生效，於是
    // `record_webhook_result()` 回寫投遞結果時就撞上那條 CHECK。
    // `IF EXISTS`：同一個測試呼叫這個 helper 多次時，第二次那條約束已經不在了。
    sqlx::query(
        "ALTER TABLE fms.webhook_subscriptions DROP CONSTRAINT IF EXISTS ck_webhook_url_https",
    )
    .execute(&mut *tx)
    .await
    .expect("drop check");
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.webhook_subscriptions
                (tenant_id, url, event_types, signing_secret, description)
         VALUES ($1::uuid, $2, ARRAY['alarm.raised'], $3, '測試接收端')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(url)
    .bind(secret)
    .fetch_one(&mut *tx)
    .await
    .expect("insert subscription");
    tx.commit().await.expect("commit");
    id.to_string()
}

/// 只跑扇出（不投遞）。
async fn run_fanout(ctx: &TestContext) {
    let pool = ctx.owner_pool().await;
    let events: Vec<(i64, uuid::Uuid, String, String, uuid::Uuid, Value)> = {
        let mut tx = ctx.owner_tx().await;
        sqlx::query_as(
            "SELECT id, tenant_id, event_type, aggregate_type, aggregate_id, payload
               FROM fms.event_outbox WHERE event_type = 'alarm.raised' ORDER BY id",
        )
        .fetch_all(&mut *tx)
        .await
        .expect("read outbox")
    };
    assert!(!events.is_empty(), "沒有 alarm.raised 事件可扇出");
    let fanout = fms_worker::webhook::WebhookFanout::new(pool);
    for (id, tenant_id, event_type, aggregate_type, aggregate_id, payload) in events {
        fanout
            .fan_out(&fms_worker::OutboxEvent {
                id,
                tenant_id,
                event_type,
                aggregate_type,
                aggregate_id,
                payload,
                attempt_count: 0,
            })
            .await
            .expect("fan_out");
    }
}

/// 扇出 + 投遞一輪。`allow` 是放進 SSRF 白名單的 `host:port`。
async fn run_delivery(ctx: &TestContext, allow: &str) -> fms_worker::webhook::Delivered {
    run_fanout(ctx).await;
    let outbound = fms_shared::OutboundSettings {
        private_target_allowlist: vec![allow.to_string()],
        connect_timeout: std::time::Duration::from_millis(500),
        total_timeout: std::time::Duration::from_secs(2),
        ..Default::default()
    };
    let dispatcher = fms_worker::webhook::WebhookDispatcher::new(ctx.owner_pool().await, outbound);
    dispatcher
        .run_once(&fms_worker::webhook::WebhookConfig::default())
        .await
        .expect("run_once")
}

/// 目前有哪些訂閱被排入了投遞（`notifications.entity_id`）。
async fn webhook_notification_targets(ctx: &TestContext) -> Vec<String> {
    let mut tx = ctx.owner_tx().await;
    let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT entity_id FROM fms.notifications
          WHERE channel = 'WEBHOOK' AND entity_type = 'WEBHOOK_SUBSCRIPTION'
          ORDER BY created_at",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("read notifications");
    drop(tx);
    ids.into_iter().map(|i| i.to_string()).collect()
}

async fn webhook_last_error(ctx: &TestContext) -> Option<String> {
    let mut tx = ctx.owner_tx().await;
    let err: Option<String> = sqlx::query_scalar(
        "SELECT last_error FROM fms.notifications WHERE channel = 'WEBHOOK' LIMIT 1",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("read last_error");
    drop(tx);
    err
}

async fn subscription_row(ctx: &TestContext, id: &str) -> (bool, i32, Option<String>) {
    let mut tx = ctx.owner_tx().await;
    let row: (bool, i32, Option<String>) = sqlx::query_as(
        "SELECT is_active, consecutive_failures, disabled_reason
           FROM fms.webhook_subscriptions WHERE id = $1::uuid",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("read subscription");
    drop(tx);
    row
}

// =============================================================================

/// 建立回 201 並帶 `signing_secret`；同 url 再 POST 是更新、回 200 且**不**帶金鑰。
#[tokio::test]
async fn a_create_returns_the_secret_once_then_updates() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = upsert(
        ctx,
        &admin,
        json!({
            "url": "https://example.com/fms",
            "event_types": ["alarm.raised", "work_order.created"],
            "description": "客戶的事件匯流排"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let secret = body["signing_secret"]
        .as_str()
        .unwrap_or_else(|| panic!("新建沒有回 signing_secret：{body}"))
        .to_string();
    assert_eq!(
        secret.len(),
        64,
        "金鑰不是 256 bit 的十六進位字串：{secret}"
    );
    assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(body["meta"]["secret_shown_once"], json!(true));

    // 同一個 url 再 POST → 更新（契約沒有 PATCH），而且**不重新產生金鑰**。
    let (status, body) = upsert(
        ctx,
        &admin,
        json!({
            "url": "https://example.com/fms",
            "event_types": ["alarm.raised"],
            "description": "改成只訂告警"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "同 url 應該是更新而不是衝突：{body}"
    );
    assert!(
        body["signing_secret"].is_null(),
        "更新時重新產生了金鑰 —— 對方的簽章驗證會全部失敗：{body}"
    );
    assert_eq!(body["data"]["event_types"], json!(["alarm.raised"]));

    // 停用 = 帶 is_active: false（契約沒有 DELETE）。
    let (status, body) = upsert(
        ctx,
        &admin,
        json!({
            "url": "https://example.com/fms",
            "event_types": ["alarm.raised"],
            "is_active": false
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["is_active"],
        json!(false),
        "停用不了 —— 那是一條關不掉的資料外送通道：{body}"
    );

    ctx.teardown().await;
}

/// 驗證：非 https、空 event_types、不認識的事件型別、指向內網的網址。
#[tokio::test]
async fn b_validation_blocks_silent_failures_and_ssrf() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    for (label, body) in [
        (
            "http",
            json!({"url": "http://hooks.example.com/x", "event_types": ["alarm.raised"]}),
        ),
        (
            "空 event_types",
            json!({"url": "https://example.com/x", "event_types": []}),
        ),
        (
            // 訂一個系統不會發出的事件是**靜默失敗**：訂閱建得起來、
            // 清單裡看起來正常、而永遠不會收到東西。
            "不認識的事件型別",
            json!({"url": "https://example.com/x", "event_types": ["work_order.exploded"]}),
        ),
        (
            // 雲端 metadata。072 的 CHECK 只擋非 https —— 這個要靠 SSRF 閘門。
            "指向 metadata 端點",
            json!({"url": "https://169.254.169.254/latest/meta-data/",
                   "event_types": ["alarm.raised"]}),
        ),
        (
            "指向 loopback",
            json!({"url": "https://127.0.0.1/hook", "event_types": ["alarm.raised"]}),
        ),
    ] {
        let (status, resp) = upsert(ctx, &admin, body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "「{label}」被接受了：{resp}"
        );
    }

    // 不認識的事件型別要**列出**可訂閱的有哪些 —— 否則整合方只能猜。
    let (_, resp) = upsert(
        ctx,
        &admin,
        json!({"url": "https://example.com/y", "event_types": ["nope"]}),
    )
    .await;
    assert!(
        resp["detail"].as_str().unwrap().contains("alarm.raised"),
        "沒有列出可訂閱的事件型別：{resp}"
    );

    ctx.teardown().await;
}

/// **金鑰讀不回來。**
///
/// 072 對 `fms_app` 做了欄位級 `REVOKE SELECT (signing_secret)` ——
/// 因此這不是「程式碼記得不要選它」，而是資料庫層擋著。
#[tokio::test]
async fn c_the_secret_is_never_readable_again() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, created) = upsert(
        ctx,
        &admin,
        json!({"url": "https://example.com/fms", "event_types": ["alarm.raised"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let secret = created["signing_secret"].as_str().unwrap().to_string();

    let (status, listed) = list(ctx, &admin, "").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let text = listed.to_string();
    assert!(
        !text.contains(&secret),
        "清單回應裡出現了簽章金鑰 —— 任何人拿到它就能偽造我們的 webhook"
    );
    assert!(
        !text.contains("signing_secret"),
        "清單回應裡有 signing_secret 欄位：{listed}"
    );

    // 更新的回應也不能有。
    let (_, updated) = upsert(
        ctx,
        &admin,
        json!({"url": "https://example.com/fms", "event_types": ["alarm.raised"]}),
    )
    .await;
    assert!(!updated.to_string().contains(&secret), "{updated}");

    ctx.teardown().await;
}

/// 清單帶整合方一定需要的三件事。
#[tokio::test]
async fn d_list_tells_integrators_what_they_need() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = list(ctx, &admin, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let types = body["meta"]["subscribable_event_types"].as_array().unwrap();
    assert!(
        types.iter().any(|t| t == "alarm.raised"),
        "沒有列出可訂閱的事件型別：{body}"
    );

    // **簽章是對 `<timestamp>.<body>` 算的。** 不說的話整合方會只驗 body，
    // 而那種實作接受重放。
    let scheme = body["meta"]["signature_scheme"].to_string();
    assert!(
        scheme.contains("timestamp") || scheme.contains("Timestamp"),
        "簽章規格沒有說時間戳也在簽章裡：{scheme}"
    );
    assert!(scheme.contains("HMAC-SHA256"), "{scheme}");

    // **at-least-once。** 不說的話整合方會假設恰好一次。
    let semantics = body["meta"]["delivery_semantics"].as_str().unwrap();
    assert!(
        semantics.contains("at-least-once"),
        "沒有說出投遞語意：{semantics}"
    );

    ctx.teardown().await;
}

/// 權限：`tenant:update`。看得到工單的人不該看得到資料被送到哪裡去。
#[tokio::test]
async fn e_tenant_update_is_required_for_both() {
    let ctx = &TestContext::setup().await;
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, body) = list(ctx, &fm, "").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "場域管理員看得到客戶的內部端點網址：{body}"
    );

    let (status, body) = upsert(
        ctx,
        &fm,
        json!({"url": "https://example.com/x", "event_types": ["alarm.raised"]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // **授權必須先於驗證。**
    //
    // 這一組請求同時是「沒有權限」與「網址不合法」。回 422 表示驗證跑在授權
    // 之前 —— 而那讓一個沒有 `tenant:update` 的使用者可以拿這支端點當 DNS
    // 與內網探測器：他送一個內部主機名，從回應是「解析失敗」還是「解析到
    // 私有位址」就知道那個主機存不存在。
    for probe in [
        json!({"url": "https://169.254.169.254/x", "event_types": ["alarm.raised"]}),
        json!({"url": "http://internal.corp.invalid/x", "event_types": ["alarm.raised"]}),
        json!({"url": "https://example.com/x", "event_types": ["nope.not.an.event"]}),
    ] {
        let (status, body) = upsert(ctx, &fm, probe.clone()).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "未授權的呼叫者從驗證結果得到了資訊（應該先回 403）：{probe} → {body}"
        );
    }

    ctx.teardown().await;
}

/// 扇出：一個事件對每一個「啟用中且訂了這個型別」的訂閱建一筆投遞。
#[tokio::test]
async fn f_fanout_creates_one_delivery_per_matching_subscription() {
    let ctx = &TestContext::setup().await;
    // 三筆訂閱：兩筆訂了 alarm.raised（一筆停用）、一筆訂了別的事件。
    let wanted = insert_subscription(ctx, "http://127.0.0.1:1/a", "s").await;
    let disabled = insert_subscription(ctx, "http://127.0.0.1:2/b", "s").await;
    let other = insert_subscription(ctx, "http://127.0.0.1:3/c", "s").await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.webhook_subscriptions SET is_active = false WHERE id = $1::uuid")
            .bind(&disabled)
            .execute(&mut *tx)
            .await
            .expect("disable");
        sqlx::query(
            "UPDATE fms.webhook_subscriptions SET event_types = ARRAY['work_order.created']
              WHERE id = $1::uuid",
        )
        .bind(&other)
        .execute(&mut *tx)
        .await
        .expect("change types");
        tx.commit().await.expect("commit");
    }

    raise_alarm(ctx).await;
    run_fanout(ctx).await;

    let targets = webhook_notification_targets(ctx).await;
    assert_eq!(
        targets,
        vec![wanted],
        "扇出的對象不對 —— 停用的或沒訂這個型別的訂閱不該收到"
    );

    ctx.teardown().await;
}

/// **投遞路徑上的閘門真的在守。**
///
/// 這一格是從一個失敗的嘗試變出來的：原本想用 http 的模擬伺服器做端到端測試，
/// 而 `WebhookDispatcher` 正確地把它判成 `PERMANENT: scheme 是 http`。
///
/// 那個行為值得一格測試：投遞時的檢查是防 DNS 在建立之後被改掉的唯一防線，
/// 而它若失效，症狀只有「客戶的資料被送到某個內網位址」。
#[tokio::test]
async fn g_the_outbound_guard_runs_on_the_delivery_path() {
    let ctx = &TestContext::setup().await;
    let sub = insert_subscription(ctx, "http://127.0.0.1:9/hook", "s").await;

    raise_alarm(ctx).await;
    let out = run_delivery(ctx, "127.0.0.1:9").await;

    assert_eq!(out.sent, 0, "http 的網址被送出去了：{out:?}");
    assert_eq!(
        out.suppressed, 1,
        "http 的網址應該是永久性失敗（重試不會讓它變成 https）：{out:?}"
    );
    assert_eq!(out.retrying, 0, "被閘門擋下的不該進重試：{out:?}");

    // 停放的理由要說得出是**哪一種**問題 —— 「投遞失敗」對維運沒有幫助。
    let err = webhook_last_error(ctx).await.unwrap_or_default();
    assert!(err.starts_with("PERMANENT:"), "沒有標明是永久性的：{err}");
    assert!(err.contains("https"), "沒有說出是 scheme 的問題：{err}");

    // 失敗要記在訂閱上，讓客戶在清單裡看得到。
    let (_, failures, _) = subscription_row(ctx, &sub).await;
    assert_eq!(failures, 1, "連續失敗數沒有累加");

    ctx.teardown().await;
}

/// 指向內網位址的訂閱在投遞時被擋下（不只在建立時）。
///
/// 建立時檢查過的是**當時**：一個 DNS 記錄可以在之後被改成指向 metadata 端點。
/// 這一格直接在資料庫裡放一筆那樣的訂閱 —— 模擬「建立之後 DNS 被改掉」。
#[tokio::test]
async fn h_private_targets_are_refused_at_delivery_time() {
    let ctx = &TestContext::setup().await;
    insert_subscription(ctx, "https://169.254.169.254/hook", "s").await;

    raise_alarm(ctx).await;
    // 白名單刻意給一個**不相關**的位址：這一格要驗的是閘門會擋，不是白名單。
    let out = run_delivery(ctx, "127.0.0.1:1").await;

    assert_eq!(out.sent, 0, "送去雲端 metadata 端點了：{out:?}");
    assert_eq!(out.suppressed, 1, "{out:?}");
    let err = webhook_last_error(ctx).await.unwrap_or_default();
    assert!(
        err.contains("link-local") || err.contains("169.254"),
        "拒絕理由沒說出是哪一類位址：{err}"
    );

    ctx.teardown().await;
}

/// 重放同一個事件**不會**再建一筆投遞。
///
/// relay 是 at-least-once，因此扇出會被重複呼叫。少了 072 的
/// `uq_notifications_webhook_event`（加 `ON CONFLICT DO NOTHING`），
/// 每次重放都會對客戶端再送一次同樣的事件。
#[tokio::test]
async fn j_replaying_the_event_does_not_create_a_second_delivery() {
    let ctx = &TestContext::setup().await;
    insert_subscription(ctx, "http://127.0.0.1:1/a", "s").await;

    raise_alarm(ctx).await;
    run_fanout(ctx).await;
    let after_first = webhook_notification_targets(ctx).await.len();
    assert_eq!(after_first, 1, "第一次扇出沒有建立投遞");

    // 再跑一次 —— 那就是 relay 重放的樣子。
    run_fanout(ctx).await;
    assert_eq!(
        webhook_notification_targets(ctx).await.len(),
        1,
        "重放又建了一筆 —— 客戶端會收到兩次同樣的事件而分不出來"
    );

    ctx.teardown().await;
}

/// 停用的訂閱不再投遞，而排隊中的列會被停放（不是永遠 QUEUED）。
#[tokio::test]
async fn i_disabled_subscription_stops_delivery_and_parks_the_queue() {
    let ctx = &TestContext::setup().await;
    let sub_id = insert_subscription(ctx, "http://127.0.0.1:1/a", "s").await;
    raise_alarm(ctx).await;

    // 扇出之後、投遞之前把訂閱停用 —— 那正是客戶按下「停用」的時序。
    run_fanout(ctx).await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.webhook_subscriptions SET is_active = false WHERE id = $1::uuid")
            .bind(&sub_id)
            .execute(&mut *tx)
            .await
            .expect("disable");
        tx.commit().await.expect("commit");
    }

    let outbound = fms_shared::OutboundSettings {
        private_target_allowlist: vec!["127.0.0.1:1".to_string()],
        ..Default::default()
    };
    let dispatcher = fms_worker::webhook::WebhookDispatcher::new(ctx.owner_pool().await, outbound);
    let out = dispatcher
        .run_once(&fms_worker::webhook::WebhookConfig::default())
        .await
        .expect("run_once");

    assert_eq!(out.sent, 0, "停用之後還是送出去了 —— 那讓「停用」變成假的");
    // **排隊中的列要被停放。** 留在 QUEUED 會看起來像「還沒送」，
    // 實際是「永遠不會送」—— 而那個差別正是監控要看的。
    assert_eq!(
        out.suppressed, 1,
        "排隊中的列沒有被停放，會永遠是 QUEUED：{out:?}"
    );
    let err = webhook_last_error(ctx).await.unwrap_or_default();
    assert!(
        err.contains("inactive"),
        "停放的理由沒說出是訂閱被停用了：{err}"
    );

    ctx.teardown().await;
}
