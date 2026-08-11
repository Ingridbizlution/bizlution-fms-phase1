//! 預約提醒通知（前端稽核文件 P1）：開會前提醒。
//!
//! # 為什麼是掃描而不是事件
//!
//! 「時間到了」沒有發生點——沒有任何動作可以發出事件，跟 `no_show`／
//! `pm_worker` 的日曆型計畫是同一類問題，同樣用定期掃描。掃描與範本渲染
//! 都在 SQL 那一側（`fms.sweep_reservation_reminders()`，見 085），
//! 跟 `fms-worker::cert_watchdog` 是同一個模式——這裡只是薄的排程外殼。
//!
//! # 為什麼需要平台情境
//!
//! 掃描要跨租戶（單一 worker 服務所有租戶），而 `reservations` 有
//! FORCE RLS。以 `fms_app` 或未宣告平台情境的連線呼叫只會安靜地掃到
//! 0 筆——跟 `no_show`／PM 產生器同一個陷阱。

use std::sync::Arc;

use sqlx::PgPool;

use fms_shared::Problem;

/// 一輪掃描的結果，對齊 `fms.sweep_reservation_reminders()` 的回傳欄位。
#[derive(Debug, Clone, Copy)]
pub struct Sweep {
    pub reminded: i32,
    pub no_template: i32,
    pub already_reminded: i32,
}

impl Sweep {
    fn is_quiet(&self) -> bool {
        self.reminded == 0 && self.no_template == 0
    }
}

pub struct ReminderScanner {
    /// **必須是 `fms_owner` 連線**：理由同 `no_show::NoShowScanner`。
    pub pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct ReminderConfig {
    /// 提醒窗是固定 15 分鐘（見 085 的說明），掃描間隔要比窗小得多，
    /// 否則會議可能整個落在兩次掃描之間。跟 `no_show` 用同一個 60 秒。
    pub interval: std::time::Duration,
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self {
            interval: std::time::Duration::from_secs(60),
        }
    }
}

impl ReminderScanner {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self { pool })
    }

    pub async fn run_once(&self) -> Result<Sweep, Problem> {
        let mut tx = self.pool.begin().await.map_err(Problem::from)?;
        sqlx::query("SELECT set_config('app.is_platform', 'on', true)")
            .execute(&mut *tx)
            .await
            .map_err(Problem::from)?;

        let row: (i32, i32, i32) =
            sqlx::query_as("SELECT * FROM fms.sweep_reservation_reminders()")
                .fetch_one(&mut *tx)
                .await
                .map_err(Problem::from)?;

        tx.commit().await.map_err(Problem::from)?;
        Ok(Sweep {
            reminded: row.0,
            no_template: row.1,
            already_reminded: row.2,
        })
    }
}

/// 常駐掃描迴圈。掃描失敗記錄後重試而不退出——同 `no_show` 的理由：
/// worker 死掉會讓提醒從此停止，比慢一輪嚴重得多。
pub async fn run_until_shutdown(
    scanner: Arc<ReminderScanner>,
    cfg: ReminderConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("預約提醒掃描迴圈收到關機訊號");
            return;
        }
        match scanner.run_once().await {
            Ok(s) if s.is_quiet() => tracing::debug!("預約提醒掃描：目前沒有需要提醒的預約"),
            Ok(s) if s.no_template > 0 => tracing::warn!(
                reminded = s.reminded,
                no_template = s.no_template,
                "預約提醒掃描完成，但有預約該提醒卻找不到範本——沒有人會收到"
            ),
            Ok(s) => tracing::info!(reminded = s.reminded, "預約提醒掃描完成"),
            Err(e) => tracing::error!(error = %e, "預約提醒掃描失敗，下一輪重試"),
        }
        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
