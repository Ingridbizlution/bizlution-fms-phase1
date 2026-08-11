//! webhook 投遞（`notifications` 裡 `channel = 'WEBHOOK'` 的那些列）。
//!
//! # 為什麼是獨立的迴圈，而不是 dispatcher 的一個分支
//!
//! webhook 與 email 幾乎沒有共用的東西：一個要 HMAC 簽章、SSRF 檢查與
//! HTTP 狀態碼判讀，另一個要 SMTP 連線與地址驗證。硬塞進 `MailTransport`
//! 只會讓兩者互相拖累，而 dispatcher 那套重試邏輯是已經驗證過的資產。
//!
//! 因此：**佇列共用（`notifications`）、退避語意共用、傳輸層各自實作。**
//! dispatcher 有一個 `DELIVERED_BY_OTHER_LOOP` 常數把 WEBHOOK 排除在
//! 「沒有傳輸層 → 停放」那一段之外 —— 少了它，這個迴圈永遠看不到任何列。
//!
//! # 簽章
//!
//! 每個請求帶三個標頭：
//!
//! | 標頭 | 內容 |
//! |---|---|
//! | `X-FMS-Event` | 事件型別，讓接收端不必解析 body 就能路由 |
//! | `X-FMS-Timestamp` | Unix 秒。**簽章的一部分** |
//! | `X-FMS-Signature` | `sha256=<hex>`，對 `timestamp.body` 算 HMAC-SHA256 |
//!
//! **時間戳進簽章是防重放的關鍵**：只簽 body 的話，一個被錄下來的請求可以
//! 原封不動重送到永遠，而簽章依然有效。接收端應拒絕時間戳過舊的請求。
//!
//! 這件事的說明必須跟著 payload 一起給整合方，否則他們會只驗 body ——
//! 因此 `GET /webhooks` 的回應帶 `meta.signature_scheme`。
//!
//! # 每次送出前重新檢查位址
//!
//! 訂閱建立時檢查過，但那是**當時**：一個 DNS 記錄可以在建立之後被改成指向
//! 內網。因此每一次投遞都重跑 `fms_shared::safe_http::resolve_and_check`，
//! 並 pin 住檢查過的 IP。這比 test-connection 那次更重要 ——
//! webhook 是**持續**的出站流量，而且 payload 裡有租戶的資料。

use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::PgPool;

use fms_shared::safe_http::{self, OutboundSettings};

/// 一輪的結果。分成四個數字而不是「成功／失敗」：
/// 「對方回 500 所以要重試」與「網址被閘門擋下所以永遠不會成功」在維運上
/// 是完全不同的訊息。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Delivered {
    pub sent: usize,
    /// 這一輪失敗、之後會重試。
    pub retrying: usize,
    /// 永久性失敗（重試沒有意義）或重試次數用盡，已停放。
    pub suppressed: usize,
    /// 因為連續失敗達門檻而被自動停用的訂閱數。
    pub subscriptions_disabled: usize,
}

impl Delivered {
    pub fn is_quiet(&self) -> bool {
        self.sent == 0 && self.retrying == 0 && self.suppressed == 0
    }
}

pub struct WebhookDispatcher {
    /// 必須是 `fms_owner`：`notifications` 與 `webhook_subscriptions` 都是
    /// FORCE RLS，投遞是跨租戶的；而且 **`signing_secret` 的 SELECT 權限
    /// 只有 fms_owner 有**（072 的欄位級 REVOKE）。
    pool: PgPool,
    outbound: OutboundSettings,
}

/// 一筆待投遞的 webhook。
struct Outgoing {
    notification_id: uuid::Uuid,
    subscription_id: uuid::Uuid,
    url: String,
    event_type: String,
    body: String,
    signing_secret: String,
    attempt_count: i16,
}

impl WebhookDispatcher {
    pub fn new(pool: PgPool, outbound: OutboundSettings) -> Arc<Self> {
        Arc::new(Self { pool, outbound })
    }

    /// 撈一批、逐筆送、回寫結果。
    ///
    /// # 為什麼整輪在同一個交易裡
    ///
    /// 與 dispatcher 同一個理由：撈取與狀態更新之間若有窗口，另一個 worker
    /// 會撈到同一批。`FOR UPDATE ... SKIP LOCKED` 讓多個實例可以並行而不重複。
    ///
    /// 代價同樣是 at-least-once：COMMIT 前崩潰會讓已送出的請求重送。
    /// **這一點必須寫進契約**，因為 webhook 的接收端需要自己做幂等
    /// （`GET /webhooks` 的 `meta.delivery_semantics` 說了）。
    pub async fn run_once(&self, cfg: &WebhookConfig) -> Result<Delivered, sqlx::Error> {
        let mut out = Delivered::default();
        let mut tx = crate::begin_platform_tx(&self.pool).await?;

        let rows: Vec<Outgoing> = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                uuid::Uuid,
                String,
                Option<String>,
                String,
                String,
                i16,
            ),
        >(
            // JOIN webhook_subscriptions：訂閱可能已經被停用或刪掉。
            // **INNER JOIN 是刻意的** —— 一筆指向已刪除訂閱的通知沒有金鑰可簽，
            // 而它會在下面被停放（撈不到就永遠是 QUEUED，那才是問題）。
            "SELECT n.id, s.id, s.url, n.subject, n.body, s.signing_secret, n.attempt_count
               FROM fms.notifications n
               JOIN fms.webhook_subscriptions s
                 ON s.id = n.entity_id AND n.entity_type = 'WEBHOOK_SUBSCRIPTION'
              WHERE n.status IN ('QUEUED', 'FAILED')
                AND n.channel = 'WEBHOOK'
                AND n.scheduled_for <= clock_timestamp()
                -- 停用的訂閱不再投遞。**這是「停用」這個動作的實現處** ——
                -- 少了它，客戶把訂閱關掉之後排隊中的事件還是會送出去。
                AND s.is_active
              ORDER BY n.status, n.scheduled_for, n.id
              FOR UPDATE OF n SKIP LOCKED
              LIMIT $1",
        )
        .bind(cfg.batch_size)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(
            |(
                notification_id,
                subscription_id,
                url,
                subject,
                body,
                signing_secret,
                attempt_count,
            )| {
                Outgoing {
                    notification_id,
                    subscription_id,
                    url,
                    // subject 存的是事件型別（072 的扇出這樣寫）。
                    event_type: subject.unwrap_or_else(|| "unknown".to_string()),
                    body,
                    signing_secret,
                    attempt_count,
                }
            },
        )
        .collect();

        // 停放指向已停用／已刪除訂閱的列。
        //
        // 不做這件事的後果是一堆永遠 QUEUED 的列：它們看起來像「還沒送」，
        // 實際是「永遠不會送」—— 而那個差別正是監控要看的（dispatcher 檔頭的
        // 同一個判斷）。
        out.suppressed += sqlx::query(
            "UPDATE fms.notifications n
                SET status = 'SUPPRESSED',
                    last_error = 'webhook subscription is inactive or gone'
              WHERE n.status IN ('QUEUED', 'FAILED')
                AND n.channel = 'WEBHOOK'
                AND NOT EXISTS (
                  SELECT 1 FROM fms.webhook_subscriptions s
                   WHERE s.id = n.entity_id
                     AND n.entity_type = 'WEBHOOK_SUBSCRIPTION'
                     AND s.is_active)",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;

        for row in &rows {
            match self.deliver(row).await {
                Ok(()) => {
                    sqlx::query(
                        "UPDATE fms.notifications
                            SET status = 'SENT', sent_at = clock_timestamp(), last_error = NULL
                          WHERE id = $1",
                    )
                    .bind(row.notification_id)
                    .execute(&mut *tx)
                    .await?;
                    self.record(&mut tx, row.subscription_id, true, None, cfg)
                        .await?;
                    out.sent += 1;
                }
                // `PERMANENT:` 前綴＝重試沒有意義（網址被閘門擋下、scheme 錯、
                // 對方回 4xx）。與 dispatcher 同一個約定。
                Err(err) if err.starts_with("PERMANENT:") => {
                    mark_suppressed(&mut tx, row.notification_id, &err).await?;
                    if self
                        .record(&mut tx, row.subscription_id, false, Some(&err), cfg)
                        .await?
                    {
                        out.subscriptions_disabled += 1;
                    }
                    out.suppressed += 1;
                }
                Err(err) => {
                    let next = row.attempt_count + 1;
                    if next >= cfg.max_attempts {
                        mark_suppressed(
                            &mut tx,
                            row.notification_id,
                            &format!("giving up after {next} attempts: {err}"),
                        )
                        .await?;
                        out.suppressed += 1;
                    } else {
                        let backoff = (cfg.backoff_base.as_secs() * 2u64.pow(next as u32)) as i64;
                        sqlx::query(
                            "UPDATE fms.notifications
                                SET status = 'FAILED',
                                    attempt_count = $2,
                                    last_error = $3,
                                    scheduled_for = clock_timestamp()
                                                  + ($4::bigint * interval '1 second')
                              WHERE id = $1",
                        )
                        .bind(row.notification_id)
                        .bind(next)
                        .bind(&err)
                        .bind(backoff)
                        .execute(&mut *tx)
                        .await?;
                        out.retrying += 1;
                    }
                    if self
                        .record(&mut tx, row.subscription_id, false, Some(&err), cfg)
                        .await?
                    {
                        out.subscriptions_disabled += 1;
                    }
                }
            }
        }

        tx.commit().await?;
        Ok(out)
    }

    /// 回寫訂閱層級的結果。回傳 `true` 表示這一次讓訂閱被自動停用。
    async fn record(
        &self,
        tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
        subscription_id: uuid::Uuid,
        success: bool,
        error: Option<&str>,
        cfg: &WebhookConfig,
    ) -> Result<bool, sqlx::Error> {
        // 三個更新（歸零／累加／達門檻停用）在 072 的函式裡原子完成 ——
        // 拆成三個語句會在並發投遞下算錯連續失敗數。
        let disabled: bool = sqlx::query_scalar("SELECT fms.record_webhook_result($1, $2, $3, $4)")
            .bind(subscription_id)
            .bind(success)
            .bind(error)
            .bind(cfg.max_consecutive_failures)
            .fetch_one(&mut **tx)
            .await?;
        if disabled {
            tracing::warn!(
                subscription_id = %subscription_id,
                failures = cfg.max_consecutive_failures,
                "webhook 訂閱連續失敗達門檻，已自動停用"
            );
        }
        Ok(disabled)
    }

    /// 送一個請求。
    async fn deliver(&self, row: &Outgoing) -> Result<(), String> {
        // **每次都重新檢查位址。** 建立時檢查過的是「當時」——
        // DNS 記錄可以在之後被改成指向內網，而 webhook 是持續的出站流量。
        let checked = safe_http::resolve_and_check(&row.url, &self.outbound)
            .await
            // 閘門的拒絕是永久性的：重試不會讓一個內網位址變成公開位址。
            .map_err(|e| format!("PERMANENT: {e}"))?;

        let timestamp = chrono::Utc::now().timestamp();
        let signature = sign(&row.signing_secret, timestamp, &row.body);

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.outbound.connect_timeout)
            .timeout(self.outbound.total_timeout)
            // pin 住剛檢查過的 IP —— 檢查與連線之間不再解析一次 DNS。
            .resolve(&checked.host, checked.addr)
            .build()
            .map_err(|e| format!("PERMANENT: 無法建立 HTTP 客戶端：{e}"))?;

        let res = client
            .post(checked.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-FMS-Event", &row.event_type)
            .header("X-FMS-Timestamp", timestamp.to_string())
            .header("X-FMS-Signature", format!("sha256={signature}"))
            .body(row.body.clone())
            .send()
            .await
            .map_err(|e| format!("請求失敗：{e}"))?;

        classify(res.status())
    }
}

/// 把回應狀態碼分成「成功／值得重試／重試沒有意義」。
///
/// 抽成純函式是為了測得到：**投遞本身在測試裡跑不完整**（閘門強制 https，
/// 而在測試裡起一個有效憑證的 TLS 伺服器是另一套機具），而這個分類是投遞
/// 邏輯裡最容易寫錯的部分 —— 把 4xx 當成暫時性的，會讓一個對方永遠會拒絕的
/// 請求重試五次，那五次都是真實的出站流量。
pub fn classify(status: reqwest::StatusCode) -> Result<(), String> {
    if status.is_success() {
        return Ok(());
    }
    // 4xx 是對方說「這個請求有問題」—— 重試同一個請求不會有不同結果。
    // 唯一的例外是 408 與 429：那兩個明確地說「稍後再試」。
    if status.is_client_error()
        && status != reqwest::StatusCode::REQUEST_TIMEOUT
        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        return Err(format!("PERMANENT: 對方回 HTTP {status}"));
    }
    Err(format!("對方回 HTTP {status}"))
}

/// `HMAC-SHA256(secret, "<timestamp>.<body>")` 的十六進位字串。
///
/// **時間戳在簽章裡**，見模組說明：只簽 body 的話，錄下來的請求可以永遠重放。
pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC 接受任意長度的金鑰");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn mark_suppressed(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    id: uuid::Uuid,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE fms.notifications SET status = 'SUPPRESSED', last_error = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub struct WebhookConfig {
    pub interval: Duration,
    pub batch_size: i64,
    pub max_attempts: i16,
    pub backoff_base: Duration,
    /// 連續失敗幾次就自動停用訂閱。
    ///
    /// 這是維運參數，不是租戶政策：它問的是「幾次失敗算對方下線了」，
    /// 而答案取決於我們的重試節奏，不取決於客戶的偏好。
    pub max_consecutive_failures: i32,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            batch_size: 50,
            // 與通知投遞一致。
            max_attempts: 5,
            backoff_base: Duration::from_secs(2),
            // 20 次連續失敗（含各自的重試）才停用。訂得太低會讓一次五分鐘的
            // 對方維護就把訂閱關掉，而重新啟用需要人工。
            max_consecutive_failures: 20,
        }
    }
}

/// 迴圈。
pub async fn run_until_shutdown(
    dispatcher: Arc<WebhookDispatcher>,
    cfg: WebhookConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("webhook 投遞迴圈收到關機訊號");
            return;
        }
        match dispatcher.run_once(&cfg).await {
            Ok(o) if o.is_quiet() => {}
            Ok(o) => tracing::info!(
                sent = o.sent,
                retrying = o.retrying,
                suppressed = o.suppressed,
                subscriptions_disabled = o.subscriptions_disabled,
                "webhook 投遞完成"
            ),
            // 失敗不能只是 debug：webhook 是對外承諾，停擺時客戶端會以為
            // 「沒有事件發生」。
            Err(e) => tracing::error!(error = %e, "webhook 投遞失敗，下一輪重試"),
        }
        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}

/// 可訂閱的事件型別。
///
/// **這份清單是手維護的**，因為「系統會發出哪些事件」是程式碼的事實，
/// 不是管理者定義的條件（066 對報表代碼的同一個判斷）。
///
/// 漂移的風險是**單向的**：清單裡多了一個沒人發出的型別，訂閱它的人就永遠
/// 收不到東西 —— 而那是靜默的。因此 `POST /webhooks` **拒絕**不在這份清單裡的
/// 型別（於是打錯字在訂閱時就是 422，不是三個月後的沈默），而
/// `GET /webhooks` 把清單回傳出去，讓整合方看得到有哪些可訂閱。
///
/// 反向的漂移（發出了但不在清單裡）只會讓那個事件無法被訂閱，不會出錯。
pub const SUBSCRIBABLE_EVENT_TYPES: &[&str] = &[
    "alarm.raised",
    "maintenance.meter_threshold_reached",
    "reservation.confirmed",
    "reservation.created",
    "visitor.checked_in",
    "work_order.created",
    "work_order.sla_breached",
];

/// 事件 → notifications（channel = WEBHOOK）的扇出。
///
/// 它是 relay 的一個 handler，不是自己的迴圈 —— 與通知扇出同一個位置。
pub struct WebhookFanout {
    pool: PgPool,
}

impl WebhookFanout {
    /// 回傳 `Self` 而不是 `Arc<Self>`：relay 的 `run_until_shutdown` 要的是
    /// 一個實作 `EventHandler` 的值（`NotificationHandler` 也是這樣傳的）。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 回傳建立的投遞筆數。
    pub async fn fan_out(&self, event: &crate::OutboxEvent) -> Result<i32, sqlx::Error> {
        let mut tx = crate::begin_platform_tx(&self.pool).await?;
        let created: i32 = sqlx::query_scalar("SELECT fms.fanout_webhooks($1, $2, $3, $4, $5, $6)")
            .bind(event.id)
            .bind(event.tenant_id)
            .bind(&event.event_type)
            .bind(&event.aggregate_type)
            .bind(event.aggregate_id)
            .bind(&event.payload)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(created)
    }
}

impl crate::EventHandler for WebhookFanout {
    fn handles(&self, event_type: &str) -> bool {
        SUBSCRIBABLE_EVENT_TYPES.contains(&event_type)
    }

    async fn handle(&self, event: &crate::OutboxEvent) -> Result<(), String> {
        // 幂等由 072 的 `uq_notifications_webhook_event` 加
        // `ON CONFLICT DO NOTHING` 保證 —— relay 是 at-least-once，而
        // `fan_out` 開自己的交易並自行 commit（與通知扇出相同的結構）。
        // 重放時 `created` 是 0，那是正確的答案。
        let created = self.fan_out(event).await.map_err(|e| e.to_string())?;
        if created > 0 {
            tracing::info!(
                event_id = event.id,
                event_type = %event.event_type,
                created,
                "已排入 webhook 投遞"
            );
        }
        // created == 0 **不記 warn**：多數租戶沒有訂閱任何 webhook，
        // 而那是正常狀態。與通知扇出不同 —— 那裡的 0 代表「宣告了要通知但
        // 沒有範本」，是一個缺口；這裡的 0 只代表「沒有人訂閱」。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 簽章是**對 `timestamp.body`** 算的，不是只對 body。
    ///
    /// 只簽 body 的話，一個被錄下來的請求可以原封不動重送到永遠而簽章依然
    /// 有效 —— 而接收端沒有任何辦法分辨。這一格用「同樣的 body、不同的時間戳
    /// 必須得到不同的簽章」把那件事釘住。
    #[test]
    fn timestamp_is_part_of_the_signature() {
        let a = sign("secret", 1_700_000_000, r#"{"event":"x"}"#);
        let b = sign("secret", 1_700_000_001, r#"{"event":"x"}"#);
        assert_ne!(
            a, b,
            "同一個 body 在不同時間得到相同的簽章 —— 錄下來的請求可以永遠重放"
        );
    }

    /// 換金鑰就換簽章（否則金鑰根本沒有參與計算）。
    #[test]
    fn secret_changes_the_signature() {
        let a = sign("secret-a", 1_700_000_000, "{}");
        let b = sign("secret-b", 1_700_000_000, "{}");
        assert_ne!(a, b);
    }

    /// 改一個字元的 body 就換簽章。
    #[test]
    fn body_changes_the_signature() {
        let a = sign("s", 1, r#"{"a":1}"#);
        let b = sign("s", 1, r#"{"a":2}"#);
        assert_ne!(a, b);
    }

    /// 形狀：64 個十六進位字元（SHA-256）。接收端會照這個長度寫驗證程式。
    #[test]
    fn signature_is_64_hex_chars() {
        let s = sign("s", 1, "{}");
        assert_eq!(s.len(), 64, "{s}");
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()), "{s}");
    }

    /// 同樣的輸入永遠得到同樣的輸出 —— 接收端要能重算。
    #[test]
    fn signature_is_deterministic() {
        assert_eq!(sign("s", 42, "body"), sign("s", 42, "body"));
    }

    /// **狀態碼的分類。** 這是投遞邏輯裡最容易寫錯的部分：把 4xx 當成暫時性的，
    /// 會讓一個對方永遠會拒絕的請求重試五次 —— 而那五次都是真實的出站流量。
    #[test]
    fn status_codes_are_classified_by_whether_retrying_can_help() {
        use reqwest::StatusCode;

        for ok in [StatusCode::OK, StatusCode::CREATED, StatusCode::NO_CONTENT] {
            assert!(classify(ok).is_ok(), "{ok} 應該算成功");
        }

        // 5xx：對方壞了，之後可能好 → 重試。
        for retryable in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
            // 這兩個是 4xx 但明確地說「稍後再試」。
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let err = classify(retryable).expect_err("應該是錯誤");
            assert!(
                !err.starts_with("PERMANENT:"),
                "{retryable} 被當成永久性失敗 —— 對方只是暫時不可用：{err}"
            );
        }

        // 其餘 4xx：重試同一個請求不會有不同結果。
        for permanent in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::GONE,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            let err = classify(permanent).expect_err("應該是錯誤");
            assert!(
                err.starts_with("PERMANENT:"),
                "{permanent} 會被重試五次，而五次都注定失敗：{err}"
            );
        }
    }

    /// 一組固定的向量。接收端會照這個算，因此演算法不能悄悄改。
    ///
    /// 值是這個實作算出來的 —— 它的作用不是「證明數學正確」（那是 hmac crate
    /// 的事），而是**釘住我們簽的到底是哪些位元組**：把分隔符從 `.` 改成 `:`、
    /// 或把時間戳挪到 body 後面，都會讓這一格失敗。
    #[test]
    fn signature_is_pinned_to_a_known_vector() {
        assert_eq!(
            sign("shhh", 1_700_000_000, r#"{"event_type":"alarm.raised"}"#),
            "28d6f0cf9f6ef07b81004fca31674aeeb647938737fef19f8f53e77d5de978f0"
        );
    }
}
