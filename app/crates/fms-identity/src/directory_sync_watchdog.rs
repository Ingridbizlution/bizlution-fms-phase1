//! 排程觸發目錄同步（migration 058 記過的已知限制，migration 078 補上答案）。
//!
//! # 為什麼需要一個掃描迴圈
//!
//! 「排程時刻到了」與 PM 日曆計畫、SLA 逾期、證照到期是同一個形狀 ——
//! 沒有人做任何動作，時間到了就是到了，沒有發生點可以發事件。
//!
//! # 為什麼跨租戶查詢、但逐租戶處理
//!
//! 與 `fms_maintenance::pm_worker::PmGenerator` 同一個手法：找出「誰到期」
//! 必須跨租戶（`identity_providers` 是 FORCE RLS），因此用平台情境的一次性
//! 查詢；但實際對帳要落在各自的租戶情境裡（`reconcile_directory_roles`
//! 讀寫的表都受 `facility_scope`／`tenant_isolation` 政策約束）。
//!
//! # 服務帳號的權限就是排程能授出的上限
//!
//! migration 078 檔頭已經完整說明：服務帳號只有 `directory:sync`，因此
//! 排程同步能授出的角色被限制在不含危險權限的範圍內。指向危險角色的對應
//! 會在這裡被記成 `PARTIAL`，與人類觸發但沒有那項危險權限時完全相同 ——
//! 這是刻意的安全預設值，不是縮水版功能。

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, cron, ActorType, Problem, TenantContext};

use crate::identity_providers;

pub struct DirectorySyncWatchdog {
    /// 必須是 `fms_owner`（`fms_platform` 成員）：`due_providers` 要跨租戶
    /// 讀 `identity_providers`，而該表是 FORCE RLS。與 `PmGenerator` 的
    /// `pool` 欄位同一個理由，同一個症狀（安靜地什麼都不做）。
    pool: PgPool,
    /// 排程同步的寫入身分。必須是持有 `directory:sync` 的服務帳號
    /// （migration 078），而不是借用某個真人的 id ——理由見模組檔頭。
    actor_user_id: Uuid,
}

/// 一輪掃描的結果。
#[derive(Debug, Default, Clone, Copy)]
pub struct Sweep {
    /// 這一輪成功且沒有被擋下任何對應。
    pub succeeded: i32,
    /// 這一輪跑完了，但至少有一個對應被提權防護擋下（見 [`Self::is_quiet`]）。
    pub partial: i32,
    /// 資料庫層失敗（不是「這個身分來源沒有 sync_enabled」——那種情況不會
    /// 進到 `due_providers` 的候選清單，因為候選查詢本身就篩了 `sync_enabled`）。
    pub failed: i32,
}

impl Sweep {
    pub fn is_quiet(&self) -> bool {
        self.succeeded == 0 && self.partial == 0 && self.failed == 0
    }
}

impl DirectorySyncWatchdog {
    pub fn new(pool: PgPool, actor_user_id: Uuid) -> Arc<Self> {
        Arc::new(Self {
            pool,
            actor_user_id,
        })
    }

    /// 掃一輪：找出到期的身分來源，逐一對帳。
    ///
    /// 一個身分來源的失敗不該讓整批中止 —— 與 `PmGenerator::run_calendar_scan`
    /// 的判斷方向一致，但這裡明確地捕捉每一筆的錯誤並繼續，而不是讓 `?`
    /// 中止整個迴圈：一個設定壞掉的身分來源，不該連帶讓同一輪裡其他到期
    /// 的來源也錯過這次排程。
    pub async fn run_once(&self) -> Result<Sweep, Problem> {
        let due = self.due_providers().await?;
        let mut sweep = Sweep::default();

        for (tenant_id, provider_id) in due {
            let mut tx = begin_tenant_tx(
                &self.pool,
                TenantContext::background(tenant_id, self.actor_user_id, ActorType::ServiceAccount),
            )
            .await?;

            match identity_providers::run_sync(
                &mut tx,
                provider_id,
                self.actor_user_id,
                "SCHEDULED",
            )
            .await
            {
                Ok(outcome) => {
                    tx.commit().await?;
                    if outcome.status == "PARTIAL" {
                        sweep.partial += 1;
                        // warn：管理者設定的對應有一部分永遠不會由排程生效，
                        // 而排程沒有人在旁邊看回應 —— 這是唯一會被看見的地方。
                        tracing::warn!(
                            %provider_id,
                            blocked = ?outcome.blocked,
                            "排程同步完成，但有對應被提權防護擋下"
                        );
                    } else {
                        sweep.succeeded += 1;
                    }
                }
                Err(err) => {
                    // `tx` 在此處被丟棄 —— sqlx 的 `Transaction::drop` 會送出
                    // ROLLBACK，因此不需要顯式呼叫。
                    sweep.failed += 1;
                    tracing::error!(error = %err, %provider_id, "排程目錄同步失敗，下一輪重試");
                }
            }
        }

        Ok(sweep)
    }

    /// 跨租戶找出到期的身分來源。
    ///
    /// 只在這裡跨租戶讀取，理由與 `PmGenerator::due_plans` 完全相同：
    /// 連線必須通過 RLS，而 `identity_providers` 是 FORCE RLS 表。
    async fn due_providers(&self) -> Result<Vec<(Uuid, Uuid)>, Problem> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('app.is_platform', 'on', true)")
            .execute(&mut *tx)
            .await?;

        let rows: Vec<(Uuid, Uuid, String, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT tenant_id, id, sync_cron, last_sync_at
               FROM fms.identity_providers
              WHERE sync_enabled AND sync_cron IS NOT NULL AND deleted_at IS NULL",
        )
        .fetch_all(&mut *tx)
        .await?;

        tx.rollback().await?;

        let now = Utc::now();
        let mut due = Vec::new();
        for (tenant_id, provider_id, expr, last_sync_at) in rows {
            match cron::is_due(&expr, last_sync_at, now) {
                Ok(true) => due.push((tenant_id, provider_id)),
                Ok(false) => {}
                Err(err) => {
                    // 目前沒有寫入路徑能把壞掉的 cron 字串存進去（`sync_cron`
                    // 不可 PATCH，見 identity_providers 的 NOT_PATCHABLE），
                    // 但若有人直接改資料庫，這裡要能看見而不是讓整輪掃描炸掉。
                    tracing::warn!(error = %err, %provider_id, "sync_cron 格式不合法，略過這個身分來源");
                }
            }
        }
        Ok(due)
    }
}

/// 掃描間隔設定。
#[derive(Debug, Clone)]
pub struct DirectorySyncWatchdogConfig {
    pub interval: Duration,
}

impl Default for DirectorySyncWatchdogConfig {
    fn default() -> Self {
        Self {
            // 5 分鐘：`sync_cron` 的粒度理論上可以到分鐘，但 002 的預設值與
            // 實際會設定的排程都是「每幾小時」這個量級（見 openapi 的範例
            // `0 */4 * * *`）。5 分鐘讓分鐘級的排程也不會被錯過太久，
            // 又遠比 SLA 逾期（1 分鐘一次）的量級粗——排程同步不是使用者
            // 盯著看的東西，晚幾分鐘生效沒有 SLA 那種「畫面顯示錯誤狀態」
            // 的後果。
            interval: Duration::from_secs(5 * 60),
        }
    }
}

/// 常駐迴圈。**啟動時立刻跑一次** —— 部署或重啟期間錯過的排程時刻，
/// 該在程序恢復的第一時間補上，而不是等到下一個 5 分鐘。
pub async fn run_until_shutdown(
    watchdog: Arc<DirectorySyncWatchdog>,
    cfg: DirectorySyncWatchdogConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("目錄同步排程迴圈收到關機訊號");
            return;
        }

        match watchdog.run_once().await {
            Ok(s) if s.is_quiet() => tracing::debug!("目錄同步排程：這輪沒有到期的身分來源"),
            Ok(s) => tracing::info!(
                succeeded = s.succeeded,
                partial = s.partial,
                failed = s.failed,
                "目錄同步排程完成"
            ),
            // 資料庫層失敗不該讓迴圈退出：與其他 watchdog 同一個判斷，
            // 半個 worker 在跑比完全沒跑更難察覺。
            Err(err) => tracing::error!(error = %err, "目錄同步排程掃描失敗，下一輪重試"),
        }

        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
