//! 產生器的兩個驅動路徑：outbox 事件（計量觸發）與定期掃描（日曆觸發）。
//!
//! 放在本 crate 而不是 `fms-worker`：`fms-worker` 提供的是**機制**
//! （claim、退避、停放），這裡是**政策**。機制不該認識維護計畫，
//! 否則每加一種事件就要改 relay。

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, Problem, TenantContext};

use crate::{generator, repo};

/// 事件與掃描共用的執行環境。
pub struct PmGenerator {
    /// **必須是 `fms_owner`（`fms_platform` 成員）的連線池。**
    ///
    /// 不是偏好而是硬需求：[`Self::due_plans`] 要跨租戶找出到期的計畫，
    /// 而那需要平台情境。以 `fms_app` 連線時 013 的硬化條件不成立
    /// （`pg_has_role(current_user, 'fms_platform')` 為假），
    /// `tenant_isolation` 政策就會濾掉全部列 —— 症狀是**產生器安靜地
    /// 什麼都不做**，沒有錯誤、沒有 log，只是永遠回 0。
    /// 這一點在實作時真的踩到了，因此寫在型別旁邊而不只在函式註解裡。
    pub pool: PgPool,
    /// 產生器以哪個使用者身分寫入。
    ///
    /// 必須是真實的使用者列：`work_orders.created_by` 有外鍵，
    /// 而 `fms.set_context()` 也需要一個 user_id。系統帳號（`SERVICE_ACCOUNT`）
    /// 是 002 已支援的 `user_type`，因此正確做法是為產生器配一個服務帳號，
    /// 而不是借用某個真人的 id。
    pub actor_user_id: Uuid,
}

/// 處理 `maintenance.meter_threshold_reached`。
///
/// # 為什麼由事件驅動而不是自己掃讀表
///
/// 4.9 的讀數端點已經在同一個交易裡判定了門檻並寫下事件。產生器再自己掃
/// 一遍讀表，等於把「門檻怎麼算」實作第二次 —— 而那條規則有型別分支
/// （累計型看週期、瞬時型看界線），複製必然漂移。
///
/// 事件是 at-least-once 的，因此重放安全性由占位的唯一索引保證
/// （見 [`generator`] 的說明），不需要在這裡另做去重。
impl PmGenerator {
    pub fn new(pool: PgPool, actor_user_id: Uuid) -> Arc<Self> {
        Arc::new(Self {
            pool,
            actor_user_id,
        })
    }

    /// 由計量門檻事件產生工單。
    pub async fn on_meter_threshold(
        &self,
        tenant_id: Uuid,
        payload: &serde_json::Value,
    ) -> Result<generator::Generated, Problem> {
        let plan_ids: Vec<Uuid> = payload
            .get("maintenance_plan_ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(|s| Uuid::parse_str(s).ok())
                    .collect()
            })
            .unwrap_or_default();

        let mut total = generator::Generated::default();
        if plan_ids.is_empty() {
            return Ok(total);
        }

        let mut tx = begin_tenant_tx(
            &self.pool,
            TenantContext::background(
                tenant_id,
                self.actor_user_id,
                fms_shared::ActorType::ServiceAccount,
            ),
        )
        .await?;

        // `scheduled_for` **必須**取自事件本身（讀數時刻），不能用處理時的時鐘。
        //
        // 唯一索引 `(plan_id, asset_id, scheduled_for)` 是冪等的唯一來源；
        // 用 `now()` 的話同一筆事件在不同秒重放就是不同的鍵，於是產生
        // 第二個占位與第二張工單。outbox 是 at-least-once，重放是常態。
        //
        // 這個 bug 真的存在過：測試曾因為兩次呼叫剛好落在同一秒而通過，
        // 加了別的工作讓它變慢之後才暴露出來。
        //
        // 舊事件（4.9 加上 `reading_at` 之前寫入的）沒有這個欄位，
        // 退回用現在時刻 —— 對它們冪等性較弱，但那些事件早已處理完畢。
        let scheduled_for = payload
            .get("reading_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let scheduled_for = truncate_to_second(scheduled_for);

        for plan_id in plan_ids {
            let Some(plan) = repo::get(&mut tx, plan_id).await? else {
                // 計畫已被刪除：事件過期了，不是暫時性失敗。
                tracing::warn!(%plan_id, "計量事件指向不存在的維護計畫，略過");
                continue;
            };
            if !plan.is_active {
                continue;
            }
            let g = generator::generate_for(&mut tx, &plan, scheduled_for).await?;
            total.occurrence_ids.extend(g.occurrence_ids);
            total.work_order_ids.extend(g.work_order_ids);
            total.skipped += g.skipped;
        }

        tx.commit().await?;
        Ok(total)
    }

    /// 掃描一輪到期的日曆型計畫。回傳處理的計畫數。
    ///
    /// 每個計畫各自一個交易：一個計畫的失敗不該讓整批回滾，
    /// 而占位的唯一索引讓「部分成功後重跑」是安全的。
    pub async fn run_calendar_scan(&self, batch: i64) -> Result<usize, Problem> {
        // 掃描本身要跨租戶，但 `begin_tenant_tx` 是租戶綁定的。
        // 因此先以無租戶條件取出候選（RLS 對此需要平台情境，見下），
        // 再逐一以該計畫的租戶身分處理。
        let due = self.due_plans(batch).await?;

        let mut handled = 0usize;
        for (tenant_id, plan_id) in due {
            let mut tx = begin_tenant_tx(
                &self.pool,
                TenantContext::background(
                    tenant_id,
                    self.actor_user_id,
                    fms_shared::ActorType::ServiceAccount,
                ),
            )
            .await?;
            if let Some(plan) = repo::get(&mut tx, plan_id).await? {
                if plan.is_active {
                    generator::run_calendar_plan(&mut tx, &plan).await?;
                    handled += 1;
                }
            }
            tx.commit().await?;
        }
        Ok(handled)
    }

    /// 跨租戶取出到期的計畫。
    ///
    /// 這是**唯一**需要跨租戶讀取的地方，因此刻意獨立成一個小函式並在此
    /// 說明：連線必須是能通過 RLS 的角色。`fms_app` 在未設租戶情境時
    /// `current_tenant_id()` 為 NULL，`tenant_isolation` 政策會擋掉全部列
    /// —— 也就是說這個查詢在 `fms_app` 下會回空清單而不是報錯，
    /// 症狀是「產生器安靜地什麼都不做」。因此 worker 必須以
    /// `fms_owner`（`fms_platform` 成員）連線並取得平台情境。
    async fn due_plans(&self, batch: i64) -> Result<Vec<(Uuid, Uuid)>, Problem> {
        let mut tx = self.pool.begin().await.map_err(Problem::from)?;
        sqlx::query("SELECT set_config('app.is_platform', 'on', true)")
            .execute(&mut *tx)
            .await
            .map_err(Problem::from)?;

        let rows = sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT p.tenant_id, p.id
               FROM fms.maintenance_plans p
              WHERE p.is_active
                AND p.trigger_type IN ('CALENDAR', 'HYBRID')
                AND p.rrule IS NOT NULL
                AND p.next_due_at IS NOT NULL
                AND p.next_due_at <= clock_timestamp()
                                     + (p.generate_lead_days::int * interval '1 day')
              ORDER BY p.next_due_at
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

/// 截到整秒。占位的唯一鍵含 `scheduled_for`，微秒級精度會讓
/// 「同一次重放」被當成不同的排程時刻，冪等就失效了。
fn truncate_to_second(t: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    t - chrono::Duration::nanoseconds(t.timestamp_subsec_nanos() as i64)
}

/// 產生器的兩個常駐迴圈設定。
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// 每次掃描處理的計畫數上限。有上界才不會讓單一輪次無限長 ——
    /// 剩下的下一輪會再取到（`next_due_at` 還沒被推進）。
    pub batch: i64,
    /// 掃描間隔。
    ///
    /// 一小時是刻意的：`generate_lead_days` 的粒度是**天**，
    /// 因此以分鐘為單位掃描只是白費查詢。太長則會讓「剛建立就已到期」的
    /// 計畫等太久，一小時在兩者之間。
    pub interval: std::time::Duration,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            batch: 100,
            interval: std::time::Duration::from_secs(3600),
        }
    }
}

/// 日曆型計畫的常駐掃描迴圈。
///
/// # 為什麼日曆型是掃描而計量型是事件
///
/// 計量觸發有明確的**發生點**（一筆讀數跨過門檻），因此可以事件驅動。
/// 日曆觸發沒有發生點 —— 「時間到了」不是任何人做的動作，
/// 沒有東西會發出事件。只能定期問「現在有誰到期了」。
///
/// 掃描本身失敗不讓迴圈退出：資料庫短暫不可用之後應該自己恢復，
/// 而 worker 直接結束會讓保養工單從此不再產生，
/// 那比慢一輪嚴重得多。
pub async fn run_scan_until_shutdown(
    generator: Arc<PmGenerator>,
    cfg: ScanConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("PM 掃描迴圈收到關機訊號");
            return;
        }

        match generator.run_calendar_scan(cfg.batch).await {
            Ok(0) => tracing::debug!("PM 掃描：目前沒有到期的計畫"),
            Ok(n) => tracing::info!(plans = n, "PM 掃描完成"),
            Err(e) => tracing::error!(error = %e, "PM 掃描失敗，下一輪重試"),
        }

        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
