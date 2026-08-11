//! SMTP 真的斷線時會發生什麼（`fms_worker::dispatcher` + 真實 lettre + mailpit）。
//!
//! `notification_dispatch_slice.rs` 的檔頭寫了一個刻意的決定：傳輸層用 stub，
//! 因為「一個真的 SMTP 伺服器（mailpit）不在 `make verify` 的服務清單裡，
//! 而讓測試依賴一個 CI 不保證存在的服務」。
//!
//! **那個前提現在不成立了。** CI 的 app job 第一步就是 `make up`，而 `make up`
//! 啟動的是 `postgres redis minio mailpit` —— mailpit 與 postgres 一樣是保證
//! 存在的。同一段檔頭也寫了「SMTP 本身對 mailpit 手動驗」，而手動驗證等於
//! 沒有人會在第 20 次改動之後再驗一次。
//!
//! 因此本檔補的是 stub **在定義上測不到**的兩件事：
//!
//!   1. **真實 lettre 錯誤字串的分類。** dispatcher 用 `PERMANENT:` 前綴區分
//!      「重試沒有意義」與「稍後再試」。stub 只能餵預先寫好的字串，
//!      也就是永遠在測「我寫的字串會被怎麼分類」，而不是「lettre 實際吐出
//!      的字串會被怎麼分類」。一次 SMTP 斷線若被誤判成永久性失敗，
//!      **整批通知會被停放而不是重試** —— 停放是終態，沒有人會再送它們。
//!   2. **信真的離開了這個程序。** stub 的 `Ok(())` 只證明我們呼叫了它。
//!
//! -----------------------------------------------------------------------------
//! 順帶記錄一個**沒有修**的觀察
//! -----------------------------------------------------------------------------
//! lettre 對 SMTP **永久性回應碼**（例如 550 mailbox unavailable）產生的錯誤
//! 字串同樣沒有 `PERMANENT:` 前綴，因此那一類會被重試 5 次才停放 ——
//! 五次都注定失敗。要修的話是在 `SmtpMailer::send` 裡看 lettre 錯誤的
//! response code 分類。這裡不做：它是效率問題不是正確性問題，
//! 而且需要決定「哪些回應碼算永久」，那是一份清單。

mod common;

use common::*;

fn smtp_port() -> String {
    std::env::var("MAILPIT_SMTP_PORT").unwrap_or_else(|_| "1025".into())
}
fn ui_port() -> String {
    std::env::var("MAILPIT_UI_PORT").unwrap_or_else(|_| "8025".into())
}

/// 一個**確定沒有人在聽**的埠：綁 0 讓系統配一個，然後立刻放掉。
///
/// 不寫死 `9` 或 `1`：那些埠可能被防火牆丟包而不是拒絕連線，
/// 於是測試會卡在連線逾時而不是拿到「連線被拒」——
/// 症狀是測試很慢而不是失敗，那種測試最後會被標成 ignore。
fn a_closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = l.local_addr().expect("addr").port();
    drop(l);
    port
}

/// 種一筆待送的 EMAIL，主旨帶一個唯一標記好在 mailpit 裡找得到。
async fn queue_email_with_marker(ctx: &TestContext, marker: &str) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.notifications
           (tenant_id, recipient_user_id, recipient_address, channel, subject, body)
         VALUES ($1::uuid, $2, 'outage-test@example.test', 'EMAIL', $3, '內容')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(admin_user_id())
    .bind(format!("SMTP 斷線測試 {marker}"))
    .fetch_one(&mut *tx)
    .await
    .expect("種通知");
    tx.commit().await.expect("commit");
    id
}

struct Row {
    status: String,
    attempts: i16,
    last_error: Option<String>,
    backoff_is_future: bool,
    sent: bool,
}

async fn state_of(ctx: &TestContext, id: uuid::Uuid) -> Row {
    let mut tx = ctx.owner_tx().await;
    let r: (String, i16, Option<String>, bool, bool) = sqlx::query_as(
        "SELECT status, attempt_count, last_error,
                coalesce(scheduled_for > clock_timestamp(), false),
                sent_at IS NOT NULL
           FROM fms.notifications WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀狀態");
    tx.commit().await.expect("commit");
    Row {
        status: r.0,
        attempts: r.1,
        last_error: r.2,
        backoff_is_future: r.3,
        sent: r.4,
    }
}

/// 用**真的** SmtpMailer 跑一輪。
async fn dispatch_via_smtp(ctx: &TestContext, url: &str) -> fms_worker::dispatcher::Dispatched {
    let mailer =
        fms_worker::dispatcher::SmtpMailer::new(url, "fms@example.test").expect("建 SmtpMailer");
    let pool = ctx.owner_pool().await;
    let d = fms_worker::dispatcher::NotificationDispatcher::new(pool.clone(), mailer);
    let out = d
        .run_once(&fms_worker::dispatcher::DispatcherConfig::default())
        .await
        .expect("dispatch");
    pool.close().await;
    out
}

/// **SMTP 斷線必須被當成暫時性失敗。**
///
/// 若真實的 lettre 錯誤字串剛好以 `PERMANENT:` 開頭（或有人「順手」把連線
/// 錯誤加上那個前綴），一次 SMTP 中斷就會把整批通知標成 `SUPPRESSED` ——
/// 而那是終態，沒有任何東西會再送它們。stub 測不到這件事，因為 stub 餵的是
/// 我自己寫的字串。
#[tokio::test]
async fn an_smtp_outage_is_transient_not_permanent() {
    let ctx = &TestContext::setup().await;
    let id = queue_email_with_marker(ctx, "transient").await;

    let out = dispatch_via_smtp(ctx, &format!("smtp://127.0.0.1:{}", a_closed_port())).await;

    assert_eq!(out.sent, 0, "沒有 SMTP 伺服器，不該有送成功的：{out:?}");
    assert_eq!(
        out.suppressed, 0,
        "**斷線不是永久性失敗** —— 停放是終態，沒有人會再送它們：{out:?}"
    );
    assert_eq!(out.retried, 1, "應該排入重試：{out:?}");

    let r = state_of(ctx, id).await;
    assert_eq!(r.status, "FAILED", "FAILED 的原意是「稍後重試」");
    assert_eq!(r.attempts, 1);
    assert!(!r.sent, "沒送出去就不該有 sent_at");
    assert!(
        r.backoff_is_future,
        "退避必須把 scheduled_for 推到未來，否則下一輪會立刻重打一個死掉的埠"
    );

    let err = r.last_error.unwrap_or_default();
    assert!(
        !err.starts_with("PERMANENT:"),
        "lettre 對連線失敗吐出的字串不能被當成永久性失敗：{err}"
    );
    assert!(
        !err.is_empty(),
        "要記下原因，否則值班的人不知道為什麼沒送出"
    );

    ctx.teardown().await;
}

/// **退避真的被遵守，而且復原之後信真的離開這個程序。**
///
/// 三段：斷線 → FAILED；SMTP 恢復但退避還沒到 → **不撈**；把 scheduled_for
/// 推到過去 → 送出，而且 **mailpit 真的收到了**。
///
/// 中間那一段是這個測試的防空轉設計：少了它，「dispatcher 每輪都重打」
/// 也會讓最後一段通過。
#[tokio::test]
async fn backoff_is_honoured_and_the_mail_really_leaves_the_process() {
    let ctx = &TestContext::setup().await;
    let marker = uuid::Uuid::new_v4().to_string();
    let id = queue_email_with_marker(ctx, &marker).await;

    // (1) 斷線
    let out = dispatch_via_smtp(ctx, &format!("smtp://127.0.0.1:{}", a_closed_port())).await;
    assert_eq!(out.retried, 1, "{out:?}");
    assert!(state_of(ctx, id).await.backoff_is_future);

    // (2) SMTP 恢復了，但退避還沒到 → 不該撈
    let live = format!("smtp://127.0.0.1:{}", smtp_port());
    let out = dispatch_via_smtp(ctx, &live).await;
    assert_eq!(
        (out.sent, out.retried, out.suppressed),
        (0, 0, 0),
        "退避未到就不該撈這一列 —— 否則退避是裝飾品：{out:?}"
    );
    assert_eq!(
        state_of(ctx, id).await.attempts,
        1,
        "沒被重打，次數不該增加"
    );

    // (3) 退避到了
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.notifications
                SET scheduled_for = clock_timestamp() - interval '1 second'
              WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .expect("把退避推到過去");
        tx.commit().await.expect("commit");
    }

    let out = dispatch_via_smtp(ctx, &live).await;
    assert_eq!(
        out.sent, 1,
        "mailpit 活著、退避已過，應該送出。若這裡是 0，先確認 make up 起來了：{out:?}"
    );

    let r = state_of(ctx, id).await;
    assert_eq!(r.status, "SENT");
    assert!(r.sent, "SENT 必須有 sent_at");
    assert_eq!(r.last_error, None, "送成功要清掉上一次的錯誤，否則會誤導");

    // (4) **信真的到了伺服器** —— stub 給不了這個保證。
    let body = reqwest::get(format!(
        "http://127.0.0.1:{}/api/v1/search?query={}",
        ui_port(),
        marker
    ))
    .await
    .expect("問 mailpit 的 API（沒起來的話 make up）")
    .text()
    .await
    .expect("讀回應");

    // 兩個獨立的判準，因為單看其中一個都有空轉的風險：
    //
    //   * `contains(marker)` —— 實測 mailpit **不會**把查詢字串回顯在回應裡
    //     （查一個不存在的標記回的是 `"messages":[]`，不含該字串），所以這個
    //     斷言目前有效。但那是它這一版的行為，不是契約。
    //   * `messages_count >= 1` —— 即使日後某一版開始回顯查詢，這一項仍然
    //     分得出「找到了」與「沒找到」。
    let json: serde_json::Value = serde_json::from_str(&body).expect("mailpit 回的不是 JSON");
    let found = json["messages_count"].as_i64().unwrap_or(0);

    assert!(
        found >= 1,
        "mailpit 應該找到至少一封（messages_count={found}）：{body}"
    );
    assert!(
        body.contains(&marker),
        "找到的信主旨要帶 {marker}，否則可能是別的測試留下的：{body}"
    );

    ctx.teardown().await;
}
