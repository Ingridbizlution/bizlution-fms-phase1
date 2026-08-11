//! 通知投遞（migration 043 + `fms_worker::dispatcher`）。
//!
//! 041／042 建立通知列，但沒有任何東西送出去 —— `EMAIL` 停在 `QUEUED`。
//! 這一層把它們送掉，而這組測試要驗的是**撈取、標記、退避、停放**的語意，
//! 不是 SMTP 本身。
//!
//! 因此傳輸層用 stub：這一組要驗的是撈取與標記的語意，不是 lettre 會不會用。
//!
//! 原本這裡還寫著「mailpit 不在 make verify 的服務清單裡，CI 不保證存在，
//! 所以 SMTP 本身對 mailpit 手動驗」。**那個前提已經不成立**：CI 的 app job
//! 第一步就是 `make up`，而它啟動 `postgres redis minio mailpit`。
//! 真實 SMTP 的路徑因此改成自動驗，見 `notification_smtp_outage_slice.rs`
//! —— 手動驗證等於沒有人會在第 20 次改動之後再驗一次。

mod common;

use std::sync::Mutex;

use common::*;

/// 記下送出的信，並可設定成失敗。
struct StubMailer {
    sent: Mutex<Vec<(String, String)>>,
    /// `None` = 成功；`Some(err)` = 每次都以那個訊息失敗。
    fail_with: Option<String>,
}

impl StubMailer {
    fn ok() -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
            fail_with: None,
        }
    }
    fn failing(err: &str) -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
            fail_with: Some(err.to_string()),
        }
    }
}

impl fms_worker::dispatcher::MailTransport for StubMailer {
    async fn send(&self, to: &str, subject: &str, _body: &str) -> Result<(), String> {
        if let Some(err) = &self.fail_with {
            return Err(err.clone());
        }
        self.sent
            .lock()
            .expect("lock")
            .push((to.to_string(), subject.to_string()));
        Ok(())
    }
}

/// 種一筆待送的通知，回傳 id。
async fn queue_email(
    ctx: &TestContext,
    recipient: Option<uuid::Uuid>,
    address: Option<&str>,
) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.notifications
           (tenant_id, recipient_user_id, recipient_address, channel, subject, body)
         VALUES ($1::uuid, $2, $3, 'EMAIL', '主旨', '內容')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(recipient)
    .bind(address)
    .fetch_one(&mut *tx)
    .await
    .expect("種通知");
    tx.commit().await.expect("commit");
    id
}

async fn queue_channel(ctx: &TestContext, channel: &str) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.notifications
           (tenant_id, recipient_user_id, channel, subject, body)
         VALUES ($1::uuid, $2, $3, '主旨', '內容')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(admin_user_id())
    .bind(channel)
    .fetch_one(&mut *tx)
    .await
    .expect("種通知");
    tx.commit().await.expect("commit");
    id
}

async fn state_of(ctx: &TestContext, id: uuid::Uuid) -> (String, i16, Option<String>, bool) {
    let mut tx = ctx.owner_tx().await;
    let row: (
        String,
        i16,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT status, attempt_count, last_error, sent_at
               FROM fms.notifications WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀狀態");
    (row.0, row.1, row.2, row.3.is_some())
}

async fn dispatch<T: fms_worker::dispatcher::MailTransport>(
    ctx: &TestContext,
    mailer: T,
) -> (
    fms_worker::dispatcher::Dispatched,
    std::sync::Arc<fms_worker::dispatcher::NotificationDispatcher<T>>,
) {
    let pool = ctx.owner_pool().await;
    let d = fms_worker::dispatcher::NotificationDispatcher::new(pool.clone(), mailer);
    let out = d
        .run_once(&fms_worker::dispatcher::DispatcherConfig::default())
        .await
        .expect("dispatch");
    pool.close().await;
    (out, d)
}

// =============================================================================
// 送出
// =============================================================================

/// 送成功 → `SENT` + `sent_at`，而地址從 `users.email` 補上。
///
/// 扇出沒有填 `recipient_address` —— 刻意的：email 要送到**當下**的地址，
/// 不是三天前扇出時的。
#[tokio::test]
async fn a_queued_email_is_sent_and_the_address_comes_from_the_user() {
    let ctx = &TestContext::setup().await;
    let id = queue_email(ctx, Some(admin_user_id()), None).await;
    assert_eq!(state_of(ctx, id).await.0, "QUEUED", "前提");

    // 管理員在種子裡的信箱。
    let expected_address: String = {
        let mut tx = ctx.owner_tx().await;
        sqlx::query_scalar("SELECT email::text FROM fms.users WHERE id = $1")
            .bind(admin_user_id())
            .fetch_one(&mut *tx)
            .await
            .expect("讀信箱")
    };

    let pool = ctx.owner_pool().await;
    let d = fms_worker::dispatcher::NotificationDispatcher::new(pool.clone(), StubMailer::ok());
    let out = d
        .run_once(&fms_worker::dispatcher::DispatcherConfig::default())
        .await
        .expect("dispatch");

    assert_eq!(out.sent, 1, "{out:?}");
    let (status, _, err, sent_at) = state_of(ctx, id).await;
    assert_eq!(status, "SENT");
    assert!(
        sent_at,
        "SENT 必須有 sent_at —— 否則「什麼時候送的」查不出來"
    );
    assert!(err.is_none(), "成功要清掉 last_error：{err:?}");

    // **地址是從 users.email 解析出來的**，不是扇出時快照的
    // （`recipient_address` 種進去時是 NULL）。
    let recorded = d.mailer().sent.lock().expect("lock").clone();
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].0, expected_address, "{recorded:?}");
    assert_eq!(recorded[0].1, "主旨", "{recorded:?}");
    pool.close().await;

    ctx.teardown().await;
}

/// 第二輪不會再送 —— `SENT` 不在可撈取的狀態裡。
#[tokio::test]
async fn a_sent_notification_is_not_sent_again() {
    let ctx = &TestContext::setup().await;
    queue_email(ctx, Some(admin_user_id()), None).await;

    let (first, _) = dispatch(ctx, StubMailer::ok()).await;
    assert_eq!(first.sent, 1);

    let (second, _) = dispatch(ctx, StubMailer::ok()).await;
    assert_eq!(second.sent, 0, "不該重送：{second:?}");

    ctx.teardown().await;
}

/// `IN_APP` 不經過投遞 —— 043 讓它在扇出時就是 `SENT`。
///
/// 少了這一格，`IN_APP` 會落進「沒有傳輸層」那條路被停放，
/// 而那會把已經送達的站內通知標成 `SUPPRESSED`。
#[tokio::test]
async fn in_app_notifications_are_untouched_by_the_dispatcher() {
    let ctx = &TestContext::setup().await;
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.notifications
           (tenant_id, recipient_user_id, channel, subject, body, status, sent_at)
         VALUES ($1::uuid, $2, 'IN_APP', '主旨', '內容', 'SENT', clock_timestamp())
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(admin_user_id())
    .fetch_one(&mut *tx)
    .await
    .expect("種站內通知");
    tx.commit().await.expect("commit");

    let (out, _) = dispatch(ctx, StubMailer::ok()).await;
    assert_eq!(out.total(), 0, "站內通知不該被投遞層碰到：{out:?}");
    assert_eq!(state_of(ctx, id).await.0, "SENT");

    ctx.teardown().await;
}

/// `QUEUED` 的 `IN_APP` 被標成 **`SENT`，不是 `SUPPRESSED`**。
///
/// 043 讓扇出直接插成 SENT，所以正常路徑不會產生 QUEUED 的站內通知 ——
/// 但任何日後忘了那個約定的寫入路徑都會。第一版的 dispatcher 會把它們
/// 當成「沒有傳輸層」而停放，也就是**把一封讀得到的通知記成「已抑制」**。
/// 突變測試（拿掉撈取的頻道過濾）沒抓到這件事，是追那個突變時發現的。
#[tokio::test]
async fn a_queued_in_app_notification_is_marked_sent_not_suppressed() {
    let ctx = &TestContext::setup().await;
    let id = queue_channel(ctx, "IN_APP").await;
    assert_eq!(state_of(ctx, id).await.0, "QUEUED", "前提");

    let (out, _) = dispatch(ctx, StubMailer::ok()).await;
    assert_eq!(out.suppressed, 0, "站內通知不是「沒有傳輸層」：{out:?}");
    assert_eq!(out.sent, 1, "{out:?}");

    let (status, _, err, sent_at) = state_of(ctx, id).await;
    assert_eq!(status, "SENT", "存在即送達");
    assert!(sent_at, "要記下送達時刻");
    assert!(err.is_none(), "沒有錯誤可記：{err:?}");

    ctx.teardown().await;
}

// =============================================================================
// 停放：不會永遠排隊
// =============================================================================

/// 沒有傳輸層的頻道被停放，而不是留在 `QUEUED`。
///
/// 一個持續成長的 `QUEUED` 堆看起來像「還沒送」，實際是「永遠不會送」。
/// 那個差別正是監控要看的東西 —— 041／042 的檔頭都說監控方式是查 `QUEUED`，
/// 而若沒有傳輸層的列都堆在裡面，那個查詢從第一天就永遠在響。
///
/// # `WEBHOOK` 從這個清單裡移出去了（migration 072）
///
/// 它現在有傳輸層 —— 在 `fms_worker::webhook` 自己的迴圈裡（HMAC 簽章 +
/// SSRF 閘門），不在這個 dispatcher 裡。因此它**不該**被這一輪停放。
///
/// 這一格原本斷言 4 個頻道都被停放，而加上 webhook 傳輸層之後它變成 3 ——
/// 也就是說**這個測試抓到了那次變更**。它現在守的是兩件事：
/// 真的沒有傳輸層的三個要被停放，而 `WEBHOOK` 要**原封不動留在 QUEUED**
/// 讓另一個迴圈接手。
///
/// 後者比前者重要：若哪天有人把 WEBHOOK 從 `DELIVERED_BY_OTHER_LOOP` 拿掉，
/// 症狀是「訂閱建好了、事件也扇出了，但一封都沒送出去，而且 last_error 說
/// 沒有傳輸層」—— 而客戶那一側只會看到沈默。
#[tokio::test]
async fn channels_without_a_transport_are_suppressed_not_queued_forever() {
    let ctx = &TestContext::setup().await;
    let mut ids = Vec::new();
    for channel in ["SMS", "PUSH", "LINE"] {
        ids.push((channel, queue_channel(ctx, channel).await));
    }
    // 由另一個迴圈投遞 —— 這一輪不該碰它。
    let webhook_id = queue_channel(ctx, "WEBHOOK").await;

    let (out, _) = dispatch(ctx, StubMailer::ok()).await;
    assert_eq!(
        out.suppressed, 3,
        "只有真的沒有傳輸層的三個該被停放（WEBHOOK 由另一個迴圈負責）：{out:?}"
    );
    assert_eq!(out.sent, 0);

    for (channel, id) in ids {
        let (status, _, err, _) = state_of(ctx, id).await;
        assert_eq!(status, "SUPPRESSED", "{channel} 應被停放");
        let err = err.unwrap_or_default();
        assert!(
            err.contains("no transport") && err.contains(channel),
            "原因要說出是哪個頻道沒有傳輸層：{err}"
        );
    }

    // **WEBHOOK 原封不動。** 被這一輪停放的話，webhook 的迴圈永遠看不到它。
    let (status, _, err, _) = state_of(ctx, webhook_id).await;
    assert_eq!(
        status, "QUEUED",
        "WEBHOOK 被 email dispatcher 停放了 —— 那個頻道的投遞迴圈就永遠看不到它"
    );
    assert!(
        err.is_none(),
        "WEBHOOK 不該被寫入任何錯誤（它還沒被嘗試投遞）：{err:?}"
    );

    // 幂等：第二輪沒有東西可停放。
    let (again, _) = dispatch(ctx, StubMailer::ok()).await;
    assert_eq!(again.total(), 0, "{again:?}");

    ctx.teardown().await;
}

/// 收件人沒有 email → **直接停放，不重試**。
///
/// 重試五次也不會長出一個地址來。沒有這個區分，一個沒有 email 的使用者
/// 會讓每輪投遞都多做五次注定失敗的工作。
#[tokio::test]
async fn a_recipient_without_an_address_is_suppressed_immediately() {
    let ctx = &TestContext::setup().await;

    // 建一個沒有 email 的使用者。
    let no_email: uuid::Uuid = {
        let mut tx = ctx.owner_tx().await;
        let id = sqlx::query_scalar(
            "INSERT INTO fms.users (tenant_id, username, display_name, status)
             VALUES ($1::uuid, 'no.email', '沒有信箱', 'ACTIVE') RETURNING id",
        )
        .bind(TENANT_ID)
        .fetch_one(&mut *tx)
        .await
        .expect("建使用者");
        tx.commit().await.expect("commit");
        id
    };
    // 另一個：email 是**空白字串**而不是 NULL。
    //
    // 這一格是突變測試逼出來的：拿掉 `!a.trim().is_empty()` 的過濾，
    // 十個測試全數通過 —— 因為原本的案例是 email 為 NULL，而那條路徑由
    // `Option` 本身處理。空白字串會讓 `coalesce` 回一個「有值」的地址
    // （實測 citext 存進去就是三個空格、長度 3），於是那封信會被送到 `"   "`。
    let blank_email: uuid::Uuid = {
        let mut tx = ctx.owner_tx().await;
        let id = sqlx::query_scalar(
            "INSERT INTO fms.users (tenant_id, username, display_name, email, status)
             VALUES ($1::uuid, 'blank.email', '空白信箱', '   ', 'ACTIVE') RETURNING id",
        )
        .bind(TENANT_ID)
        .fetch_one(&mut *tx)
        .await
        .expect("建使用者");
        tx.commit().await.expect("commit");
        id
    };

    let id = queue_email(ctx, Some(no_email), None).await;
    let blank_id = queue_email(ctx, Some(blank_email), None).await;

    let (out, d) = dispatch(ctx, StubMailer::ok()).await;
    assert_eq!(
        (out.sent, out.retried, out.suppressed),
        (0, 0, 2),
        "{out:?}"
    );
    assert!(
        d.mailer().sent.lock().expect("lock").is_empty(),
        "一封都不該被交給 mailer"
    );

    let (status, attempts, err, _) = state_of(ctx, blank_id).await;
    assert_eq!(status, "SUPPRESSED", "空白信箱也該停放");
    assert_eq!(attempts, 0);
    assert!(
        err.unwrap_or_default().contains("no email"),
        "空白信箱的原因要與 NULL 相同"
    );

    let (status, attempts, err, _) = state_of(ctx, id).await;
    assert_eq!(status, "SUPPRESSED");
    assert_eq!(attempts, 0, "永久性失敗不該累加重試次數");
    assert!(
        err.unwrap_or_default().contains("no email"),
        "原因要可行動（去補那個人的 email）"
    );

    ctx.teardown().await;
}

/// 永久性錯誤（`PERMANENT:` 前綴）直接停放。
#[tokio::test]
async fn a_permanent_transport_error_is_not_retried() {
    let ctx = &TestContext::setup().await;
    let id = queue_email(ctx, Some(admin_user_id()), None).await;

    let (out, _) = dispatch(ctx, StubMailer::failing("PERMANENT: 地址無法解析")).await;
    assert_eq!((out.retried, out.suppressed), (0, 1), "{out:?}");
    let (status, attempts, _, _) = state_of(ctx, id).await;
    assert_eq!(status, "SUPPRESSED");
    assert_eq!(attempts, 0, "永久性失敗不累加重試次數");

    ctx.teardown().await;
}

// =============================================================================
// 退避與重試上限
// =============================================================================

/// 暫時性失敗 → `FAILED` + 退避，而**退避期間不會被再次撈取**。
#[tokio::test]
async fn a_transient_failure_backs_off_and_is_not_reclaimed_immediately() {
    let ctx = &TestContext::setup().await;
    let id = queue_email(ctx, Some(admin_user_id()), None).await;

    let (out, _) = dispatch(ctx, StubMailer::failing("connection reset")).await;
    assert_eq!(
        (out.sent, out.retried, out.suppressed),
        (0, 1, 0),
        "{out:?}"
    );
    let (status, attempts, err, _) = state_of(ctx, id).await;
    assert_eq!(status, "FAILED", "FAILED 的原意是稍後重試，不是終態");
    assert_eq!(attempts, 1);
    assert!(err.unwrap_or_default().contains("connection reset"));

    // 立刻再跑一輪：退避還沒到，不該被撈取。
    let (again, _) = dispatch(ctx, StubMailer::ok()).await;
    assert_eq!(
        again.total(),
        0,
        "退避期間被重新撈取的話，退避就沒有作用：{again:?}"
    );
    assert_eq!(state_of(ctx, id).await.0, "FAILED");

    // 把 scheduled_for 拉回現在 → 應該被撈到並送成功。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.notifications SET scheduled_for = clock_timestamp() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .expect("拉回時刻");
        tx.commit().await.expect("commit");
    }
    let (retryed, _) = dispatch(ctx, StubMailer::ok()).await;
    assert_eq!(retryed.sent, 1, "退避到期後應重試並成功：{retryed:?}");
    assert_eq!(state_of(ctx, id).await.0, "SENT");

    ctx.teardown().await;
}

/// 達重試上限後停放，而不是無限重試。
#[tokio::test]
async fn giving_up_after_max_attempts_parks_the_notification() {
    let ctx = &TestContext::setup().await;
    let id = queue_email(ctx, Some(admin_user_id()), None).await;

    let cfg = fms_worker::dispatcher::DispatcherConfig {
        max_attempts: 3,
        ..Default::default()
    };
    let pool = ctx.owner_pool().await;
    let d = fms_worker::dispatcher::NotificationDispatcher::new(
        pool.clone(),
        StubMailer::failing("smtp timeout"),
    );

    for expected in 1..=2 {
        let out = d.run_once(&cfg).await.expect("dispatch");
        assert_eq!(out.retried, 1, "第 {expected} 輪應重試：{out:?}");
        assert_eq!(state_of(ctx, id).await.1, expected);
        // 每輪都要把退避拉回來，否則下一輪撈不到。
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.notifications SET scheduled_for = clock_timestamp() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .expect("拉回時刻");
        tx.commit().await.expect("commit");
    }

    let out = d.run_once(&cfg).await.expect("dispatch");
    pool.close().await;
    assert_eq!((out.retried, out.suppressed), (0, 1), "{out:?}");

    let (status, attempts, err, _) = state_of(ctx, id).await;
    assert_eq!(status, "SUPPRESSED");
    assert_eq!(attempts, 2, "停放時不再累加 —— 那個數字是「試了幾次」");
    let err = err.unwrap_or_default();
    assert!(
        err.contains("giving up") && err.contains("smtp timeout"),
        "訊息要同時說出放棄與最後一次的原因：{err}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 整條鏈
// =============================================================================

/// **逾期 → 升級 → 扇出 → 投遞 → 信真的送出去。**
///
/// 這是把 032–043 全部串起來的那一個測試。
#[tokio::test]
async fn an_sla_breach_ends_up_in_an_email() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, wo) = ctx
        .send(authed(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/work-orders")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "work_order_type": "CORRECTIVE",
                        "facility_id": "cccccccc-0000-4000-8000-000000000001",
                        "asset_id": "20000000-0000-4000-8000-000000000002",
                        "title": "整條鏈",
                        "priority": "HIGH"
                    })
                    .to_string(),
                ))
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{wo}");
    let id = wo["id"].as_str().expect("id").to_string();

    for action in [
        serde_json::json!({ "action": "ASSIGN", "assignee_id": "ffffffff-0000-4000-8000-000000000003" }),
        serde_json::json!({ "action": "START_WORK" }),
    ] {
        let (status, resp) = ctx
            .send(authed(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/work-orders/{id}/transitions"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(action.to_string()))
                    .unwrap(),
                &token,
            ))
            .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{resp}");
    }

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_orders
                SET response_due_at = clock_timestamp() - interval '2 hours',
                    resolution_due_at = clock_timestamp() - interval '2 hours'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("推 due");
        tx.commit().await.expect("commit");
    }

    let pool = ctx.owner_pool().await;
    fms_worker::sla_watchdog::SlaWatchdog::new(pool.clone())
        .run_once()
        .await
        .expect("sweep");
    let handler = fms_worker::notifier::NotificationHandler::new(pool.clone())
        .await
        .expect("handler");
    let types = handler.event_types.clone();
    fms_worker::run_once(
        &pool,
        &handler,
        &fms_worker::RelayConfig {
            event_types: Some(types),
            ..Default::default()
        },
    )
    .await
    .expect("relay");

    let mailer = StubMailer::ok();
    let d = fms_worker::dispatcher::NotificationDispatcher::new(pool.clone(), mailer);
    let out = d
        .run_once(&fms_worker::dispatcher::DispatcherConfig::default())
        .await
        .expect("dispatch");
    pool.close().await;

    assert!(out.sent >= 1, "逾期升級的 EMAIL 通知該被送出：{out:?}");

    // 站內那一封仍然是 SENT（043），沒有被投遞層重複處理。
    let mut tx = ctx.owner_tx().await;
    let by_channel: Vec<(String, String)> = sqlx::query_as(
        "SELECT channel, status FROM fms.notifications
          WHERE entity_id = $1::uuid AND template_code = 'WO_SLA_BREACH'
          ORDER BY channel",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await
    .expect("讀通知");
    assert!(
        by_channel.iter().all(|(_, s)| s == "SENT"),
        "EMAIL 與 IN_APP 都該是 SENT：{by_channel:?}"
    );
    assert_eq!(by_channel.len(), 2, "兩個頻道各一封：{by_channel:?}");

    ctx.teardown().await;
}

/// 沒有待送的東西時，一輪投遞什麼都不做。
///
/// dispatcher 每 10 秒跑一次，因此「沒事」是最常見的情況 ——
/// 那條路徑不能是沒有跑過的。
#[tokio::test]
async fn an_idle_round_does_nothing() {
    let ctx = &TestContext::setup().await;
    let mailer = StubMailer::ok();
    let pool = ctx.owner_pool().await;
    let d = fms_worker::dispatcher::NotificationDispatcher::new(pool.clone(), mailer);
    let out = d
        .run_once(&fms_worker::dispatcher::DispatcherConfig::default())
        .await
        .expect("dispatch");
    assert_eq!(out.total(), 0, "{out:?}");
    pool.close().await;
    ctx.teardown().await;
}
