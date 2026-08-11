//! 時間分區的預先建立（呼叫 028 的 `fms.ensure_time_partitions`）。
//!
//! # 為什麼在 fms-worker 而不是某個領域 crate
//!
//! 這件事沒有領域：它關心的是「儲存層有沒有地方放下個月的列」，
//! 與工單、預約、遙測的規則無關。`fms-worker` 的定位就是**機制**
//! （outbox claim、退避、停放），分區維護屬於同一類。
//!
//! # 為什麼需要它
//!
//! 001 只建了 2026 年 7、8 月的分區加一個 DEFAULT。沒有這個迴圈，
//! 9 月起所有列會落進 DEFAULT —— 不會失敗，但分區的用意（保留期到了
//! 直接 DROP 一整個月）就失效了。而且會**自我鎖死**：列一旦進了 DEFAULT，
//! 就再也不能為那個月建立分區，修復需要 ACCESS EXCLUSIVE 鎖。
//!
//! 判斷邏輯全在 SQL 函式裡，這一層只負責「定期呼叫」與「把結果記進 log」
//! ——與 ADR-09 實作紀律 2 一致。

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

/// 一次執行的結果。分成兩個數字而不是只回總數：
/// 「這輪建了 3 個」與「這輪什麼都不用建」在維運上是不同的訊息，
/// 而穩定狀態下正確的輸出**就是** created = 0。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub created: usize,
    pub already_present: usize,
}

pub struct PartitionMaintainer {
    /// 必須是 `fms_owner`：建立分區是 DDL，而 `fms_app` 刻意沒有 DDL 能力。
    pool: PgPool,
}

impl PartitionMaintainer {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self { pool })
    }

    /// 確保未來 `months_ahead` 個月（含當月）都有分區。幂等。
    pub async fn run_once(&self, months_ahead: i32) -> Result<Outcome, sqlx::Error> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT parent_table, partition_name, action FROM fms.ensure_time_partitions($1)",
        )
        .bind(months_ahead)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Outcome::default();
        for (parent, name, action) in rows {
            if action == "created" {
                out.created += 1;
                // 每一個新分區都記一筆：這是 schema 變更，即使是預期中的。
                tracing::info!(parent = %parent, partition = %name, "已預建月分區");
            } else {
                out.already_present += 1;
            }
        }
        Ok(out)
    }
}

/// 執行間隔與提前月數。
///
/// 一天一次遠比需要的頻繁（邊界是月），但便宜且讓「剛部署就補齊」不必等一個月。
/// `months_ahead = 3` 給的是**容錯窗**：即使這個迴圈整整停了兩個月沒跑，
/// 也還有分區可用 —— 而那正是它該防的情況。
pub struct PartitionConfig {
    pub interval: Duration,
    pub months_ahead: i32,
}

impl Default for PartitionConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(24 * 60 * 60),
            months_ahead: 3,
        }
    }
}

/// 迴圈。啟動時**立刻**跑一次，不等第一個間隔 ——
/// 部署當下就該補齊，否則一個剛上線的系統要等到明天才有下個月的分區。
pub async fn run_until_shutdown(
    maintainer: Arc<PartitionMaintainer>,
    cfg: PartitionConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("分區維護迴圈收到關機訊號");
            return;
        }
        match maintainer.run_once(cfg.months_ahead).await {
            Ok(o) if o.created == 0 => {
                tracing::debug!(present = o.already_present, "分區維護：無須新建")
            }
            Ok(o) => tracing::info!(
                created = o.created,
                present = o.already_present,
                "分區維護完成"
            ),
            // 失敗不能只是 warn：`ensure_time_partitions` 唯一預期的失敗是
            // 「DEFAULT 裡已有該月的列」，那需要人工介入（停機窗），
            // 而且會隨時間變得更貴。
            Err(e) => tracing::error!(error = %e, "分區維護失敗 —— 需要人工檢查，下一輪重試"),
        }
        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
