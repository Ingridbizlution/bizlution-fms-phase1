//! 把 outbox relay 接上 PM 產生器。
//!
//! 放在本 crate 而非 `fms-worker`：relay 提供的是機制（claim、退避、停放），
//! 這裡是政策。機制不該認識維護計畫，否則每加一種事件都要改 relay。

use std::sync::Arc;

use crate::pm_worker::PmGenerator;

/// 事件型別 → 產生器的分派器。
pub struct MaintenanceEventHandler {
    pub generator: Arc<PmGenerator>,
}

/// relay 認得的事件型別。
///
/// 只列出**真的有處理器**的型別：`EventHandler::handles` 回 false 的事件
/// 會被標為 `SKIPPED` 而不是假裝成功，這樣「沒有人處理的事件」在資料庫裡
/// 看得出來，不會被誤讀成已送達。
pub const HANDLED_EVENT: &str = "maintenance.meter_threshold_reached";

impl fms_worker::EventHandler for MaintenanceEventHandler {
    fn handles(&self, event_type: &str) -> bool {
        event_type == HANDLED_EVENT
    }

    async fn handle(&self, event: &fms_worker::OutboxEvent) -> Result<(), String> {
        // 回 Err 會讓事件退回 FAILED 並依指數退避重試，因此只有**暫時性**
        // 失敗才該回 Err。產生器內部已把「計畫已刪除」這類永久性狀況
        // 處理成略過（見 on_meter_threshold），所以這裡剩下的錯誤
        // ——連線中斷、鎖等待逾時——都值得重試。
        self.generator
            .on_meter_threshold(event.tenant_id, &event.payload)
            .await
            .map(|g| {
                if !g.work_order_ids.is_empty() || g.skipped > 0 {
                    tracing::info!(
                        event_id = event.id,
                        created = g.work_order_ids.len(),
                        skipped = g.skipped,
                        "計量門檻事件已處理"
                    );
                }
            })
            .map_err(|e| e.to_string())
    }
}
