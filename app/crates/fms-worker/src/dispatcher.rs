//! 通知投遞（`notification-dispatcher`）。
//!
//! 041／042 建立 `fms.notifications` 的列，但沒有任何東西把它們送出去 ——
//! `EMAIL` 的列停在 `QUEUED`。這一層把它們送掉。
//!
//! # 形狀照 outbox relay
//!
//! 撈取（`FOR UPDATE SKIP LOCKED`）、退避、達上限後停放 —— 與 relay 完全
//! 相同，因為問題相同（多實例、at-least-once、暫時性失敗要重試）。
//! 而 schema 從一開始就預備好了：`idx_notifications_queue` 是
//! `WHERE status IN ('QUEUED','FAILED')` 的部分索引，也就是說
//! **`QUEUED`／`FAILED` 才是可撈取的狀態**，`FAILED` 的原意是「稍後重試」
//! 而非終態 —— 與 `event_outbox` 的語意一致。
//!
//! # 為什麼「哪些頻道能送」是設定而不是資料
//!
//! ADR-09 紀律 2 要把判斷交給資料庫，但這一個不是資料判斷：
//! 「這個部署有沒有設 SMTP」是**部署事實**，不是租戶資料。
//! 因此它住在 worker 的設定裡，而資料庫只記錄結果。
//!
//! # 沒有傳輸層的頻道會被停放，不會永遠排隊
//!
//! `SMS`／`PUSH`／`WEBHOOK`／`LINE` 目前沒有實作。它們會被標成
//! `SUPPRESSED` 並寫明原因 —— 而不是留在 `QUEUED`。
//! 一個持續成長的 `QUEUED` 堆看起來像「還沒送」，實際是「永遠不會送」，
//! 而那個差別正是監控要看的東西。

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

/// 一筆待送的通知。
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub id: uuid::Uuid,
    pub channel: String,
    /// 收件地址。`recipient_address` 為空時由 `users.email` 補上 ——
    /// **在送出時解析而不是扇出時快照**：email 要送到當下的地址。
    pub address: Option<String>,
    pub subject: Option<String>,
    pub body: String,
    pub attempt_count: i16,
}

/// 郵件傳輸。抽成 trait 讓測試能用 stub —— 撈取、標記、退避的邏輯要能
/// 確定性地驗證，而那不該依賴一個真的 SMTP 伺服器。
pub trait MailTransport: Send + Sync {
    fn send(
        &self,
        to: &str,
        subject: &str,
        body: &str,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// lettre 的 SMTP 實作。
pub struct SmtpMailer {
    transport: lettre::AsyncSmtpTransport<lettre::Tokio1Executor>,
    from: lettre::message::Mailbox,
}

impl SmtpMailer {
    /// `url` 例如 `smtp://localhost:1025`（開發用 mailpit）或
    /// `smtps://user:pass@mail.example.com`。
    pub fn new(url: &str, from: &str) -> Result<Self, String> {
        let transport = lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::from_url(url)
            .map_err(|e| format!("SMTP_URL 無法解析：{e}"))?
            .build();
        let from = from
            .parse::<lettre::message::Mailbox>()
            .map_err(|e| format!("MAIL_FROM 無法解析：{e}"))?;
        Ok(Self { transport, from })
    }
}

impl MailTransport for SmtpMailer {
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), String> {
        use lettre::AsyncTransport;
        let to = to
            .parse::<lettre::message::Mailbox>()
            .map_err(|e| format!("PERMANENT: 收件地址無法解析：{e}"))?;
        let email = lettre::Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(subject)
            .body(body.to_string())
            .map_err(|e| format!("PERMANENT: 郵件無法組成：{e}"))?;
        self.transport
            .send(email)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// 一輪的結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Dispatched {
    pub sent: usize,
    /// 暫時性失敗，會退避重試。
    pub retried: usize,
    /// 停放（永久性失敗、達重試上限、或該頻道沒有傳輸層）。
    pub suppressed: usize,
}

impl Dispatched {
    pub fn total(&self) -> usize {
        self.sent + self.retried + self.suppressed
    }
}

pub struct DispatcherConfig {
    pub batch_size: i64,
    pub max_attempts: i16,
    pub backoff_base: Duration,
    pub interval: Duration,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            batch_size: 50,
            max_attempts: 5,
            // 與 relay 相同的退避基數。郵件的暫時性失敗（對方擋、連線逾時）
            // 通常要等更久才有意義，但先與既有機制一致比另訂一組數字好。
            backoff_base: Duration::from_secs(2),
            // ENDPOINTS.md 記載 notification-dispatcher 為每 10 秒。
            interval: Duration::from_secs(10),
        }
    }
}

/// 需要傳輸層才能送出的頻道 —— 目前只有 `EMAIL` 有實作。
const DELIVERABLE: [&str; 1] = ["EMAIL"];

/// **存在即送達**的頻道。
///
/// `IN_APP` 的列本身就是通知（`GET /notifications` 讀得到），因此它不需要
/// 任何傳輸層。043 讓扇出直接把它插成 `SENT`，但這裡仍然處理 `QUEUED` 的
/// 情況 —— 任何日後忘了那個約定的寫入路徑都會產生它。
///
/// 這一格是突變測試逼出來的：第一版只有 `DELIVERABLE`，於是 `QUEUED` 的
/// `IN_APP` 會落進「沒有傳輸層」那條路被標成 `SUPPRESSED` ——
/// **把一封讀得到的站內通知標成「已抑制」**。
const SELF_DELIVERING: [&str; 1] = ["IN_APP"];

/// **由另一個迴圈投遞**的頻道。
///
/// `WEBHOOK` 的傳輸層在 [`crate::webhook`]（HMAC 簽章 + SSRF 閘門 + HTTPS
/// POST），不在這裡 —— 它與 email 幾乎沒有共用的東西，硬塞進 `MailTransport`
/// 會讓兩者互相拖累。
///
/// **這個常數存在的唯一理由是不要把它們停放掉。** 下面第 (2) 段會把
/// 「既不 deliverable 也不 self-delivering」的頻道標成 `SUPPRESSED`；
/// 少了這一行，webhook 的列會在自己的迴圈看到它們**之前**就被這一輪掃掉，
/// 而症狀是「訂閱建好了、事件也扇出了，但一封都沒送出去，
/// 而且 last_error 說沒有傳輸層」。
const DELIVERED_BY_OTHER_LOOP: [&str; 1] = ["WEBHOOK"];

pub struct NotificationDispatcher<T: MailTransport> {
    /// 必須是 `fms_owner`：`notifications` 是 FORCE RLS，而投遞是跨租戶的。
    pool: PgPool,
    mailer: T,
}

impl<T: MailTransport> NotificationDispatcher<T> {
    pub fn new(pool: PgPool, mailer: T) -> Arc<Self> {
        Arc::new(Self { pool, mailer })
    }

    /// 傳輸層本身，供測試斷言它收到了什麼（例如地址是不是從
    /// `users.email` 正確解析出來的）。
    pub fn mailer(&self) -> &T {
        &self.mailer
    }

    /// 送一輪。
    ///
    /// 撈取與標記在同一交易內（與 relay 相同）：`FOR UPDATE SKIP LOCKED`
    /// 持有的鎖直到 COMMIT，因此多個 dispatcher 實例不會送重複的信。
    ///
    /// **但送信本身在交易內進行**，而那意味著若 COMMIT 前程序崩潰，
    /// 信已經送出而狀態還是 `QUEUED` → 重啟後會再送一次。
    /// 這是 at-least-once，與 relay 相同；對通知而言重複比漏掉好。
    pub async fn run_once(&self, cfg: &DispatcherConfig) -> Result<Dispatched, sqlx::Error> {
        let mut out = Dispatched::default();
        // **整輪都在同一個交易裡。** 第一版把「停放沒有傳輸層的頻道」放在
        // 自己的交易，於是那筆 commit 與下面的撈取之間有一個窗口 ——
        // 期間插入的 SMS 列會被撈到，然後拿去當 email 送。
        let mut tx = crate::begin_platform_tx(&self.pool).await?;

        // (1) 存在即送達的頻道：標 SENT。不是 SUPPRESSED —— 那會把一封
        //     讀得到的站內通知記成「已抑制」。
        out.sent += sqlx::query(
            "UPDATE fms.notifications
                SET status = 'SENT', sent_at = coalesce(sent_at, clock_timestamp())
              WHERE status IN ('QUEUED', 'FAILED')
                AND channel = ANY($1)",
        )
        .bind(&SELF_DELIVERING[..])
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;

        // (2) 既不需要傳輸層、也沒有傳輸層的頻道：停放並寫明原因。
        //     不留在 QUEUED —— 一個持續成長的 QUEUED 堆看起來像「還沒送」，
        //     實際是「永遠不會送」，而那個差別正是監控要看的。
        out.suppressed += sqlx::query(
            "UPDATE fms.notifications
                SET status = 'SUPPRESSED',
                    last_error = 'no transport configured for channel ' || channel
              WHERE status IN ('QUEUED', 'FAILED')
                AND NOT (channel = ANY($1))
                AND NOT (channel = ANY($2))
                AND NOT (channel = ANY($3))",
        )
        .bind(&DELIVERABLE[..])
        .bind(&SELF_DELIVERING[..])
        .bind(&DELIVERED_BY_OTHER_LOOP[..])
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize;

        // 排序對齊 idx_notifications_queue 的 (status, scheduled_for)。
        let rows: Vec<Outgoing> = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                String,
                Option<String>,
                Option<String>,
                String,
                i16,
            ),
        >(
            "SELECT n.id, n.channel,
                    coalesce(n.recipient_address, u.email::text),
                    n.subject, n.body, n.attempt_count
               FROM fms.notifications n
               LEFT JOIN fms.users u ON u.id = n.recipient_user_id
              WHERE n.status IN ('QUEUED', 'FAILED')
                AND n.scheduled_for <= clock_timestamp()
                -- 上面兩段已經把非 EMAIL 的列處理掉了，因此這個條件在
                -- 同一個交易內是多餘的。留著它是為了讓「只有 EMAIL 會
                -- 進到 mailer」這件事在**這一個查詢裡**就成立，而不必
                -- 依賴讀者記得前面兩段 —— 沒有確定性的測試分得出來，
                -- 突變測試證實了那一點。
                AND n.channel = ANY($2)
              ORDER BY n.status, n.scheduled_for, n.id
              FOR UPDATE OF n SKIP LOCKED
              LIMIT $1",
        )
        .bind(cfg.batch_size)
        .bind(&DELIVERABLE[..])
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(
            |(id, channel, address, subject, body, attempt_count)| Outgoing {
                id,
                channel,
                address,
                subject,
                body,
                attempt_count,
            },
        )
        .collect();

        for row in &rows {
            let Some(address) = row.address.as_deref().filter(|a| !a.trim().is_empty()) else {
                // 收件人沒有 email。**這是永久性的**，重試五次也不會長出一個
                // 地址來 —— 直接停放並寫明原因。
                mark_suppressed(&mut tx, row.id, "recipient has no email address").await?;
                out.suppressed += 1;
                continue;
            };

            match self
                .mailer
                .send(address, row.subject.as_deref().unwrap_or(""), &row.body)
                .await
            {
                Ok(()) => {
                    sqlx::query(
                        "UPDATE fms.notifications
                            SET status = 'SENT', sent_at = clock_timestamp(), last_error = NULL
                          WHERE id = $1",
                    )
                    .bind(row.id)
                    .execute(&mut *tx)
                    .await?;
                    out.sent += 1;
                }
                // `PERMANENT:` 前綴代表重試沒有意義（地址格式錯、郵件組不起來）。
                // 沒有這個區分，一封收件地址打錯的信會重試五次才停放，
                // 而那五次都注定失敗。
                Err(err) if err.starts_with("PERMANENT:") => {
                    mark_suppressed(&mut tx, row.id, &err).await?;
                    out.suppressed += 1;
                }
                Err(err) => {
                    let next = row.attempt_count + 1;
                    if next >= cfg.max_attempts {
                        mark_suppressed(
                            &mut tx,
                            row.id,
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
                        .bind(row.id)
                        .bind(next)
                        .bind(&err)
                        .bind(backoff)
                        .execute(&mut *tx)
                        .await?;
                        out.retried += 1;
                    }
                }
            }
        }

        tx.commit().await?;
        Ok(out)
    }
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

/// 迴圈。
pub async fn run_until_shutdown<T: MailTransport>(
    dispatcher: Arc<NotificationDispatcher<T>>,
    cfg: DispatcherConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("通知投遞迴圈收到關機訊號");
            return;
        }
        match dispatcher.run_once(&cfg).await {
            Ok(d) if d.total() == 0 => tracing::debug!("通知投遞：沒有待送的"),
            Ok(d) => {
                tracing::info!(sent = d.sent, "通知已送出");
                // 停放是 warn：那些是**不會再送**的通知，而「沒有傳輸層的頻道」
                // 與「收件人沒有 email」都需要有人去處理設定或資料。
                if d.suppressed > 0 {
                    tracing::warn!(
                        suppressed = d.suppressed,
                        "部分通知已停放（無傳輸層／無地址／達重試上限）"
                    );
                }
                if d.retried > 0 {
                    tracing::info!(retried = d.retried, "部分通知將重試");
                }
            }
            Err(e) => tracing::error!(error = %e, "通知投遞失敗，下一輪重試"),
        }
        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
