//! outbox relay —— 規格書 ADR-05「事件以 transactional outbox 落地」的執行端。
//!
//! # 為什麼 relay 必須以 `fms_owner` 連線
//!
//! `fms.event_outbox` 有 `FORCE ROW LEVEL SECURITY` 與 `tenant_isolation` 政策，
//! 因此以 `fms_app` 連線只看得到單一租戶的事件，無法全域排空 outbox。
//! relay 是基礎設施而非請求路徑，因此以 `fms_owner`（屬 `fms_platform`）連線
//! 並開啟平台情境 —— 這與 ADR-09 的原則一致：**繞過 RLS 的能力只放在
//! 請求路徑之外**。`fms_app` 仍然完全拿不到平台情境。
//!
//! # 狀態語意來自 schema，不是自己發明
//!
//! `idx_event_outbox_claim` 是 `WHERE status IN ('PENDING','FAILED')` 的
//! 部分索引，這說明 `FAILED` 的原意是「稍後重試」而非終態；
//! `status` 的 CHECK 允許 `PENDING/PUBLISHED/FAILED/SKIPPED`，
//! 因此終態停放用 `SKIPPED`。順序也照索引 `(status, available_at, id)`。
//!
//! # 交付保證
//!
//! at-least-once。事件的取用、處理、狀態更新在同一交易內完成，
//! 且以 `FOR UPDATE SKIP LOCKED` 取用，因此多個 relay 實例可並行而不重複投遞。
//! handler 必須自行具備幂等性 —— 若 COMMIT 前程序崩潰，該事件會被重新取用。
//! 第二階段換成 Kafka relay 時，生產端（`emit_event`）不需要任何改動。

pub mod audit_export;
pub mod cert_watchdog;
pub mod dispatcher;
pub mod notifier;
pub mod partitions;
pub mod report_export;
pub mod sla_watchdog;
pub mod token_purge;
pub mod webhook;

use std::time::Duration;

use sqlx::{PgPool, Postgres, Transaction};

/// 從 outbox 取出的一筆事件。
#[derive(Debug, Clone)]
pub struct OutboxEvent {
    pub id: i64,
    pub tenant_id: uuid::Uuid,
    pub event_type: String,
    pub aggregate_type: String,
    pub aggregate_id: uuid::Uuid,
    pub payload: serde_json::Value,
    pub attempt_count: i16,
}

/// 處理單一事件的副作用（通知、附加服務 fan-out、資產狀態回寫等）。
///
/// 回傳 `Err` 會讓事件退回 `FAILED` 並依指數退避重試；
/// 因此 handler 應區分「暫時性失敗」（回 Err，值得重試）與
/// 「這筆資料永遠處理不了」（自行記錄後回 Ok，避免無意義的重試）。
pub trait EventHandler: Send + Sync {
    /// 是否處理這個 event_type。回 false 的事件會被標為 `SKIPPED`。
    fn handles(&self, event_type: &str) -> bool;

    /// 處理事件。實作必須是幂等的（見模組說明的交付保證）。
    fn handle(
        &self,
        event: &OutboxEvent,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// relay 的設定。
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// 每輪取用的筆數上限。
    pub batch_size: i64,
    /// 重試次數上限；超過即停放為 `SKIPPED`，需人工介入。
    pub max_attempts: i16,
    /// 退避基數：第 n 次失敗後延後 `base * 2^n`。
    pub backoff_base: Duration,
    /// 沒有事件可處理時的休息間隔。規格書 §L2b 訂 outbox-relay 為 2 秒。
    pub idle_interval: Duration,
    /// 只處理這些 event_type 的事件；`None` 表示排空整個 outbox。
    ///
    /// 用途一：把 relay 依事件家族分片，讓某個慢 handler 不會拖住其他事件。
    /// 用途二：測試隔離 —— relay 天生是全域排空的，若不能限縮範圍，
    /// 並行的測試會互相搶走事件（實測就是這樣讓併發測試只看到 15/20 筆）。
    ///
    /// **是清單而不是單值**：通知扇出要處理多個事件型別，而兩個沒有分片的
    /// relay 會互相把對方的事件標成 SKIPPED —— 那會靜默銷毀事件。
    /// 空的 `Vec` 與 `None` 不同：空清單代表「不處理任何事件」。
    pub event_types: Option<Vec<String>>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            max_attempts: 5,
            backoff_base: Duration::from_secs(2),
            idle_interval: Duration::from_secs(2),
            event_types: None,
        }
    }
}

/// 一輪處理的結果，供監控與測試斷言使用。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RelayBatch {
    pub published: usize,
    /// 失敗但仍會重試
    pub retried: usize,
    /// 無 handler 或已達重試上限而停放
    pub skipped: usize,
}

impl RelayBatch {
    pub fn total(&self) -> usize {
        self.published + self.retried + self.skipped
    }
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// 開啟具平台情境的交易。
///
/// 以 `set_config(..., true)`（交易級）而非 `SET`：交易結束即失效，
/// 連線歸還連線池時不會殘留平台情境。
async fn begin_platform_tx(pool: &PgPool) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.is_platform', 'on', true)")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// 處理一輪。回傳本輪的處理統計。
///
/// 取用與更新在同一交易內：`FOR UPDATE SKIP LOCKED` 持有的鎖直到 COMMIT
/// 才釋放，因此另一個 relay 實例不會取到同一筆。
pub async fn run_once<H: EventHandler>(
    pool: &PgPool,
    handler: &H,
    cfg: &RelayConfig,
) -> Result<RelayBatch, sqlx::Error> {
    let mut tx = begin_platform_tx(pool).await?;

    // 排序對齊 idx_event_outbox_claim 的 (status, available_at, id)
    let events = sqlx::query_as!(
        OutboxEvent,
        r#"SELECT id, tenant_id, event_type::text AS "event_type!",
                  aggregate_type::text AS "aggregate_type!", aggregate_id,
                  payload, attempt_count
           FROM fms.event_outbox
           WHERE status IN ('PENDING', 'FAILED')
             AND available_at <= clock_timestamp()
             AND ($2::text[] IS NULL OR event_type = ANY($2))
           ORDER BY status, available_at, id
           FOR UPDATE SKIP LOCKED
           LIMIT $1"#,
        cfg.batch_size,
        cfg.event_types.as_deref()
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut batch = RelayBatch::default();

    for event in &events {
        if !handler.handles(&event.event_type) {
            mark_skipped(
                &mut tx,
                event.id,
                "no handler registered for this event_type",
            )
            .await?;
            batch.skipped += 1;
            continue;
        }

        match handler.handle(event).await {
            Ok(()) => {
                sqlx::query!(
                    "UPDATE fms.event_outbox
                     SET status = 'PUBLISHED', published_at = clock_timestamp(), last_error = NULL
                     WHERE id = $1",
                    event.id
                )
                .execute(&mut *tx)
                .await?;
                batch.published += 1;
            }
            Err(err) => {
                let next_attempt = event.attempt_count + 1;
                if next_attempt >= cfg.max_attempts {
                    // 停放而非無限重試：反覆失敗的事件會排在索引前面，
                    // 拖慢後續正常事件的處理。
                    mark_skipped(
                        &mut tx,
                        event.id,
                        &format!("giving up after {next_attempt} attempts: {err}"),
                    )
                    .await?;
                    batch.skipped += 1;
                } else {
                    // 指數退避。以秒數乘 interval 而非綁 PgInterval：
                    // 前者不必為了型別轉換引入額外依賴，SQL 也更易讀。
                    let backoff_secs =
                        (cfg.backoff_base.as_secs() * 2u64.pow(next_attempt as u32)) as i64;
                    sqlx::query!(
                        "UPDATE fms.event_outbox
                         SET status = 'FAILED',
                             attempt_count = $2,
                             last_error = $3,
                             available_at = clock_timestamp() + ($4::bigint * interval '1 second')
                         WHERE id = $1",
                        event.id,
                        next_attempt,
                        err,
                        backoff_secs
                    )
                    .execute(&mut *tx)
                    .await?;
                    batch.retried += 1;
                }
            }
        }
    }

    tx.commit().await?;
    Ok(batch)
}

async fn mark_skipped(
    tx: &mut Transaction<'static, Postgres>,
    id: i64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE fms.event_outbox
         SET status = 'SKIPPED', last_error = $2, published_at = clock_timestamp()
         WHERE id = $1",
        id,
        reason
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// 持續執行到收到關機訊號。沒有事件時休息 `idle_interval`。
pub async fn run_until_shutdown<H: EventHandler>(
    pool: PgPool,
    handler: H,
    cfg: RelayConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("outbox relay 收到關機訊號");
            return;
        }

        match run_once(&pool, &handler, &cfg).await {
            Ok(batch) if batch.is_empty() => {
                // 沒事做才休息；有事做就立刻處理下一批，避免積壓
                tokio::select! {
                    _ = tokio::time::sleep(cfg.idle_interval) => {}
                    _ = shutdown.changed() => {}
                }
            }
            Ok(batch) => {
                tracing::info!(
                    published = batch.published,
                    retried = batch.retried,
                    skipped = batch.skipped,
                    "outbox relay 完成一輪"
                );
            }
            Err(err) => {
                // 資料庫層失敗（連線中斷等）不應讓 worker 直接退出
                tracing::error!(error = %err, "outbox relay 該輪失敗，稍後重試");
                tokio::time::sleep(cfg.idle_interval).await;
            }
        }
    }
}

/// Phase 1 的預設 handler：只記錄，不做副作用。
///
/// 真正的副作用（通知寄送、附加服務 fan-out、資產狀態回寫）應隨各自的模組
/// 一起實作並註冊進來；在此之前刻意不假裝已經處理完成，因此它只接受
/// 已知的事件型別，其餘一律讓 relay 標為 `SKIPPED` 而不是假裝成功。
pub struct LoggingHandler {
    pub accepted: Vec<String>,
}

impl EventHandler for LoggingHandler {
    fn handles(&self, event_type: &str) -> bool {
        self.accepted.iter().any(|e| e == event_type)
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), String> {
        tracing::info!(
            event_id = event.id,
            tenant_id = %event.tenant_id,
            event_type = %event.event_type,
            aggregate_id = %event.aggregate_id,
            "outbox event 已處理（Phase 1 僅記錄）"
        );
        Ok(())
    }
}
