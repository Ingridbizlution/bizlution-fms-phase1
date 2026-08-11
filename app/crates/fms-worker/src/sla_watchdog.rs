//! SLA 逾期掃描（呼叫 033 的 `fms.sweep_sla_states`）。
//!
//! # 為什麼要有掃描
//!
//! 逾期是**「時間到了而某事沒有發生」**，因此沒有觸發點。032 的狀態機只在
//! 有人推進工單時判定；一張沒有人碰的工單逾期了，不會有任何地方知道。
//! 這與預約的 no-show 掃描是同一個形狀。
//!
//! # 為什麼在 fms-worker
//!
//! 與 `partitions` 同一個理由：這件事沒有領域邏輯 —— 判斷全在 SQL 函式裡
//! （ADR-09 紀律 2），這一層只負責「定期呼叫」與「把結果記進 log」。
//!
//! # 平台情境
//!
//! `work_orders` 是 FORCE RLS，連 owner 都受限，而掃描是跨租戶的。
//! 033 刻意**不用** SECURITY DEFINER（理由見該檔檔頭），因此這裡必須自己
//! 開一個帶平台情境的交易 —— 用的是 relay 已經在用的 `begin_platform_tx`。

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

/// 一輪的結果。三個數字分開回而不是回總數：在維運上它們是不同的訊息 ——
/// `at_risk` 是提醒，兩個 breached 是已經發生的違約。
///
/// 穩定狀態下正確的輸出**就是**三個 0。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Sweep {
    pub at_risk: i64,
    pub response_breached: i64,
    pub resolution_breached: i64,
    /// 已自動升級為 `SLA_BREACHED` 的工單數。
    ///
    /// **一定小於或等於 breached 的總數** —— 差額落在 `not_escalatable`。
    pub escalated: i64,
    /// 目錄不允許從該狀態升級的工單數。
    ///
    /// **這不是失敗，是覆蓋缺口的量測值。** 哪些狀態可以升級由
    /// `work_order_transitions_allowed` 決定（管理者可改），目前只有
    /// `ASSIGNED` 與 `IN_PROGRESS`，因此還停在 `SUBMITTED` 或某個
    /// `WAITING` 狀態的逾期工單會落在這裡。詳見 migration 035／036 檔頭。
    pub not_escalatable: i64,
    /// 升級失敗的次數 —— 幾乎都是競態：從標記到轉移之間有人推進了工單。
    /// 逾期已經標好了，只是狀態沒變。
    pub escalation_failed: i64,
}

impl Sweep {
    pub fn breached(&self) -> i64 {
        self.response_breached + self.resolution_breached
    }
    pub fn is_quiet(&self) -> bool {
        self.at_risk == 0 && self.breached() == 0
    }
}

pub struct SlaWatchdog {
    /// 必須是 `fms_owner`：033 的 EXECUTE 只給了它，而且掃描需要平台情境
    /// （`is_platform_context()` 的雙條件之一是 `fms_platform` 角色成員身分）。
    pool: PgPool,
}

impl SlaWatchdog {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self { pool })
    }

    /// 掃一輪。幂等 —— 已經標記過的工單不會再被計入。
    ///
    /// 沒有參數是刻意的：預警門檻在各 policy 的 `escalation_rules` 裡、
    /// 可升級的狀態在 `work_order_transitions_allowed` 裡，兩者都是管理者
    /// 定義的資料。這一層若再開一個旋鈕，就是一個能蓋掉他們設定的東西。
    pub async fn run_once(&self) -> Result<Sweep, sqlx::Error> {
        let mut tx = crate::begin_platform_tx(&self.pool).await?;
        let row: (i64, i64, i64, i64, i64, i64) =
            sqlx::query_as("SELECT * FROM fms.sweep_sla_states()")
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(Sweep {
            at_risk: row.0,
            response_breached: row.1,
            resolution_breached: row.2,
            escalated: row.3,
            not_escalatable: row.4,
            escalation_failed: row.5,
        })
    }
}

/// 執行間隔。
///
/// **只有間隔** —— 門檻類的設定全部在資料庫裡由管理者定義（見
/// `run_once` 的說明）。這裡剩下的是純粹的排程參數。
///
/// 一分鐘一次：`api/ENDPOINTS.md` 記載的 `sla-watchdog` 頻率。這個頻率決定
/// **逾期標記的最大延遲**，而那個延遲會出現在 UI 上（工單看起來還是
/// `ON_TRACK`）。報表不受影響 —— 它從時刻欄位算，不讀 `sla_state`。
pub struct WatchdogConfig {
    pub interval: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
        }
    }
}

/// 迴圈。啟動時立刻跑一次 —— 部署（或重啟）當下就該把停機期間累積的逾期補標。
pub async fn run_until_shutdown(
    watchdog: Arc<SlaWatchdog>,
    cfg: WatchdogConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("SLA 掃描迴圈收到關機訊號");
            return;
        }
        match watchdog.run_once().await {
            Ok(s) if s.is_quiet() => tracing::debug!("SLA 掃描：無變化"),
            Ok(s) => {
                // 違約是 warn 而不是 info：它代表對外承諾沒有達成。
                //
                // **這段註解原本說 log 是唯一會送到維運眼前的通道，那已經
                // 不成立了。** 當時（035）`BREACH_SLA` 宣告的
                // `notify: [FACILITY_ADMIN, MAINTENANCE_SUPERVISOR]` 確實沒有
                // 消費者 —— 全 repo 零個 `INSERT INTO fms.notifications`，
                // 事件躺在 outbox 裡被標成 SKIPPED。
                //
                // **041 補上了那個消費者**：`fan_out_notifications` 會落地通知，
                // 而 `fms-jobs` 的通知 relay 分片在「目錄裡宣告了 notify 的
                // 轉移會發出的事件型別」上 —— `work_order.sla_breached` 就在
                // 那 13 個裡面，`WO_SLA_BREACH` 範本也存在。
                //
                // 保留 warn 的理由改成：通知是給**負責那個場域的人**看的，
                // 而 log 是給維運看的。兩者的讀者不同，不是備援關係。
                if s.breached() > 0 {
                    tracing::warn!(
                        response_breached = s.response_breached,
                        resolution_breached = s.resolution_breached,
                        escalated = s.escalated,
                        not_escalatable = s.not_escalatable,
                        at_risk = s.at_risk,
                        "SLA 逾期"
                    );
                } else {
                    tracing::info!(at_risk = s.at_risk, "SLA 掃描：有工單接近時限");
                }
                // 升級失敗單獨一筆：它不是「沒有逾期」，而是「逾期標了、
                // 狀態沒改」。混進上面那筆會被讀成競態的正常損耗，
                // 而持續發生代表工單一直在被推進，或有人給 `BREACH_SLA`
                // 這個系統動作加了 `required_permission`（掃描沒有 actor）。
                if s.escalation_failed > 0 {
                    tracing::warn!(
                        failed = s.escalation_failed,
                        "部分逾期工單升級失敗（已標記，狀態未變更）"
                    );
                }
            }
            // 掃描失敗會讓逾期標記停在原地。下一輪會補上（函式是幂等的
            // 且以絕對時刻判定，不依賴上一輪跑過），因此重試就夠 ——
            // 但持續失敗代表 UI 上的 sla_state 全部過期，要看得見。
            Err(e) => tracing::error!(error = %e, "SLA 掃描失敗，下一輪重試"),
        }
        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
