//! No-show 掃描（Phase 2 S5）。
//!
//! # 為什麼是掃描而不是事件
//!
//! 「時間到了而某人沒出現」沒有發生點 —— 沒有任何動作可以發出事件。
//! 與 PM 的日曆型計畫是同一類問題，因此同樣用定期掃描
//! （見 `fms_maintenance::pm_worker` 的相同說明）。
//!
//! # 判定條件
//!
//! `auto_release_at` 由建立預約時依 `bookable_resources.auto_release_minutes`
//! 算出。**在本次之前那一欄從未被填入**，所以整條 no-show 機制是斷的：
//! 掃描會永遠找不到任何列，而 `NO_SHOW` 這個狀態值形同虛設。
//!
//! 標記時**重述一次條件**而不是信任掃描結果：掃描與標記之間有時間差，
//! 使用者可能剛好在這段時間內報到。條件寫進 UPDATE 的 WHERE，
//! 那個競態就由資料庫解決（影響 0 列＝他及時到了）。

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, Problem, TenantContext};

use crate::repo;

/// no-show 掃描器。
pub struct NoShowScanner {
    /// **必須是 `fms_owner` 連線**：掃描要跨租戶，而 `reservations`
    /// 有 FORCE RLS。以 `fms_app` 連線時會安靜地回 0 筆 ——
    /// 與 PM 產生器完全相同的陷阱，理由見該處。
    pub pool: PgPool,
    /// 標記 no-show 的身分（服務帳號）。
    pub actor_user_id: Uuid,
}

/// 掃描設定。
#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub batch: i64,
    /// 掃描間隔。分鐘級：`auto_release_minutes` 的粒度是分鐘，
    /// 而「遲到 15 分鐘算 no-show」的判定晚一分鐘生效沒有實務差別。
    pub interval: std::time::Duration,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            batch: 200,
            interval: std::time::Duration::from_secs(60),
        }
    }
}

impl NoShowScanner {
    pub fn new(pool: PgPool, actor_user_id: Uuid) -> Arc<Self> {
        Arc::new(Self {
            pool,
            actor_user_id,
        })
    }

    /// 掃描一輪，回傳實際標記的筆數。
    pub async fn run_once(&self, batch: i64) -> Result<usize, Problem> {
        let due = self.overdue(batch).await?;
        let mut marked = 0usize;

        for (tenant_id, reservation_id) in due {
            // SYSTEM：把預約標成 NO_SHOW 不是任何人的動作，是「時間到了而
            // 某人沒出現」。稽核軌若把它記成 USER，看起來就像 actor_user_id
            // 那個人主動取消了別人的預約。
            let mut tx = begin_tenant_tx(
                &self.pool,
                TenantContext::background(
                    tenant_id,
                    self.actor_user_id,
                    fms_shared::ActorType::System,
                ),
            )
            .await?;
            if repo::mark_no_show(&mut tx, reservation_id).await? == 1 {
                marked += 1;
                tracing::info!(%reservation_id, "預約逾期未報到，標記為 NO_SHOW");
            }
            tx.commit().await?;
        }
        Ok(marked)
    }

    /// 跨租戶取出逾期未報到的預約。需要平台情境，理由同 PM 產生器。
    async fn overdue(&self, batch: i64) -> Result<Vec<(Uuid, Uuid)>, Problem> {
        let mut tx = self.pool.begin().await.map_err(Problem::from)?;
        sqlx::query("SELECT set_config('app.is_platform', 'on', true)")
            .execute(&mut *tx)
            .await
            .map_err(Problem::from)?;

        let rows = sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT r.tenant_id, r.id
               FROM fms.reservations r
              WHERE r.status = 'CONFIRMED'
                AND r.requires_check_in
                AND r.checked_in_at IS NULL
                AND r.auto_release_at IS NOT NULL
                AND r.auto_release_at <= clock_timestamp()
              ORDER BY r.auto_release_at
              LIMIT $1",
        )
        .bind(batch)
        .fetch_all(&mut *tx)
        .await
        .map_err(Problem::from)?;

        tx.rollback().await.map_err(Problem::from)?;
        Ok(rows)
    }
}

/// 常駐掃描迴圈。掃描失敗記錄後重試而不退出 ——
/// worker 死掉會讓 no-show 統計從此停止，比慢一輪嚴重得多。
pub async fn run_until_shutdown(
    scanner: Arc<NoShowScanner>,
    cfg: ScanConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("no-show 掃描迴圈收到關機訊號");
            return;
        }
        match scanner.run_once(cfg.batch).await {
            Ok(0) => tracing::debug!("no-show 掃描：目前沒有逾期未報到的預約"),
            Ok(n) => tracing::info!(marked = n, "no-show 掃描完成"),
            Err(e) => tracing::error!(error = %e, "no-show 掃描失敗，下一輪重試"),
        }
        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
