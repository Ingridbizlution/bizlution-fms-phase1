//! 證照到期掃描（migration 059 的 `sweep_certification_expiry`）。
//!
//! # 為什麼需要一個掃描迴圈
//!
//! 「證照過期」沒有一個發生點可以發事件 —— 沒有人做任何動作，
//! 時間到了就是到了。這與 `sla_watchdog` 是同一個形狀（PM 掃描也是），
//! 而與工單轉移那種事件驅動的通知不同。
//!
//! # 這個迴圈的頻率與 SLA 的差三個數量級，那是刻意的
//!
//! SLA 掃描一分鐘一次：那個延遲會出現在 UI 上（工單看起來還是 `ON_TRACK`）。
//!
//! 證照到期是**以天為單位**的事實。一天掃一次就夠，而更頻繁沒有任何好處
//! —— 059 的幂等是靠 `reminded_for_expiry`，重複掃描不會多寄信，
//! 但會白白掃過整張表。
//!
//! # 沒有門檻參數，只有間隔
//!
//! 提醒的前置期在 `skills.reminder_days_before` 裡，由管理者定義
//! （電氣執照 60 天、急救證 7 天 —— 那是證照類型的性質）。
//! 這一層再開一個旋鈕就是一個能蓋掉他們設定的東西 ——
//! `sla_watchdog` 的檔頭已經定了這條規矩。

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

/// 一輪掃描的結果。
#[derive(Debug, Clone, Copy)]
pub struct Sweep {
    /// 這一輪真的建立的通知數（一個人一張證照可能兩筆：EMAIL + IN_APP）。
    pub reminded: i32,
    /// **該通知卻沒有範本** —— 沒有人會收到。與 041 同一個判斷：
    /// 不拋錯，但必須被計數，否則就是另一個沉默失效。
    pub no_template: i32,
    /// 已經針對同一個到期日提醒過而跳過的。幂等生效的證據。
    pub already_reminded: i32,
}

impl Sweep {
    /// 沒有任何新提醒、也沒有缺範本 —— 值得降到 debug。
    pub fn is_quiet(&self) -> bool {
        self.reminded == 0 && self.no_template == 0
    }
}

pub struct CertWatchdog {
    /// 必須是 `fms_owner`：059 的 EXECUTE 只給了它，而且掃描要跨租戶
    /// （`is_platform_context()` 的雙條件之一是 `fms_platform` 成員身分）。
    pool: PgPool,
}

impl CertWatchdog {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self { pool })
    }

    /// 掃一輪。**幂等** —— 已針對同一個到期日提醒過的不會再寄。
    ///
    /// 沒有參數是刻意的，見模組檔頭。
    pub async fn run_once(&self) -> Result<Sweep, sqlx::Error> {
        let mut tx = crate::begin_platform_tx(&self.pool).await?;
        let row: (i32, i32, i32) = sqlx::query_as("SELECT * FROM fms.sweep_certification_expiry()")
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Sweep {
            reminded: row.0,
            no_template: row.1,
            already_reminded: row.2,
        })
    }
}

/// 執行間隔。
#[derive(Debug, Clone)]
pub struct CertWatchdogConfig {
    pub interval: Duration,
}

impl Default for CertWatchdogConfig {
    fn default() -> Self {
        Self {
            // 一天一次。見模組檔頭：證照到期是以天為單位的事實，
            // 更頻繁只是白掃。
            interval: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// 迴圈。**啟動時立刻跑一次** —— 部署或重啟當下就該把停機期間到期的證照補提醒。
///
/// 一天一次的間隔讓這件事更重要：若只在下一個間隔才跑，一次重啟可能
/// 讓某張證照的提醒晚一整天，而它可能已經過期了。
pub async fn run_until_shutdown(
    watchdog: Arc<CertWatchdog>,
    cfg: CertWatchdogConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("證照到期掃描迴圈收到關機訊號");
            return;
        }

        match watchdog.run_once().await {
            Ok(s) if s.is_quiet() => {
                tracing::debug!(already_reminded = s.already_reminded, "證照掃描：無新提醒")
            }
            Ok(s) => {
                if s.no_template > 0 {
                    // warn：這些人**該被通知而不會被通知**。
                    // 而範本是管理者可以自己建的（042 的
                    // notification_templates CRUD），所以這是可修的設定問題。
                    tracing::warn!(
                        no_template = s.no_template,
                        "有證照到期該通知，但找不到 CERT_EXPIRING 範本 —— 沒有人會收到"
                    );
                }
                if s.reminded > 0 {
                    tracing::info!(
                        reminded = s.reminded,
                        already_reminded = s.already_reminded,
                        "證照到期提醒已建立"
                    );
                }
            }
            Err(err) => {
                // 資料庫層失敗不該讓迴圈退出 —— 與 sla_watchdog 同一個判斷：
                // 半個 worker 在跑比完全沒跑更難察覺。
                tracing::error!(error = %err, "證照到期掃描該輪失敗，稍後重試");
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
