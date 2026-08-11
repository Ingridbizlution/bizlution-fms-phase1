//! `CalendarPushHandler`：推出側（ADR-14 決定 C 的另一半）。
//!
//! 掛在既有的 outbox relay 機制上（`fms_worker::EventHandler`），跟
//! `fms_maintenance::relay_handler::MaintenanceEventHandler` 同一個形狀——
//! relay 提供機制（撈取、退避、停放），這裡是政策（`reservation.confirmed`
//! 該推去哪個外部日曆）。
//!
//! # 為什麼一定要濾掉 `created_via IN ('OUTLOOK','GOOGLE')`
//!
//! 拉入側（`watchdog.rs`）合成的外部原生預約，插入／更新時一樣會經過
//! `trg_reservation_events()`（005），一樣會發出 `reservation.confirmed`／
//! `reservation.cancelled`。若這裡不濾掉，會把「剛從外部日曆拉進來的事件」
//! 又推回外部日曆——不是無窮迴圈（`fms_reservation_id` 標記會讓下一輪
//! delta 查詢認出那是自己人），但完全是白工，而且會在外部日曆上製造出
//! 使用者從沒建立過的重複事件。只有 `created_via` 屬於 FMS 原生來源
//! （`WEB`／`MOBILE`／`KIOSK`／`API`／`IMPORT`）的事件才需要推出去。
//!
//! # 為什麼 `external_event_id` 已經有值時不重新建立
//!
//! `reservation.confirmed` 理論上一個預約只會發生一次（`PENDING_APPROVAL`
//! 核准通過那次也算，但那次的 `created_via` 仍是原本的值，`external_event_id`
//! 那時應該還是 NULL）。這裡仍然檢查一次是防禦性的——relay 是
//! at-least-once，一次重放不該在外部日曆上建立第二個事件。

use fms_shared::{begin_tenant_tx, ActorType, Problem, TenantContext};
use fms_worker::{EventHandler, OutboxEvent};
use uuid::Uuid;

use crate::provider::{build_provider, OutboundEvent, ProviderBuildIssue};

pub const HANDLED_EVENTS: [&str; 2] = ["reservation.confirmed", "reservation.cancelled"];

pub struct CalendarPushHandler {
    pool: sqlx::PgPool,
    actor_user_id: Uuid,
    ms_app_client_id: String,
    ms_app_client_secret: String,
    ms_bases_override: Option<(String, String)>,
}

impl CalendarPushHandler {
    pub fn new(
        pool: sqlx::PgPool,
        actor_user_id: Uuid,
        ms_app_client_id: String,
        ms_app_client_secret: String,
    ) -> Self {
        Self {
            pool,
            actor_user_id,
            ms_app_client_id,
            ms_app_client_secret,
            ms_bases_override: None,
        }
    }

    /// 測試專用：把 Microsoft 端點換成本地假伺服器。
    #[doc(hidden)]
    pub fn with_ms_bases(mut self, token_base: String, graph_base: String) -> Self {
        self.ms_bases_override = Some((token_base, graph_base));
        self
    }

    async fn handle_inner(&self, event: &OutboxEvent) -> Result<(), Problem> {
        let mut tx = begin_tenant_tx(
            &self.pool,
            TenantContext::background(
                event.tenant_id,
                self.actor_user_id,
                ActorType::ServiceAccount,
            ),
        )
        .await?;

        let reservation = sqlx::query!(
            r#"SELECT resource_type, resource_id, title, start_at, end_at,
                      created_via, external_event_id
                 FROM fms.reservations WHERE id = $1"#,
            event.aggregate_id
        )
        .fetch_optional(tx.conn())
        .await?;

        let Some(reservation) = reservation else {
            // relay 是 at-least-once：事件可能在預約被別的路徑刪除之後才處理到。
            tracing::warn!(reservation_id = %event.aggregate_id, "推出事件對應的預約已經不存在");
            return Ok(());
        };

        // 見模組檔頭：外部原生來源的回音不推回去。
        if matches!(reservation.created_via.as_str(), "OUTLOOK" | "GOOGLE") {
            return Ok(());
        }
        if reservation.resource_type != "SPATIAL_NODE" {
            // 目前只做房間／資源日曆（ADR-14 排除範圍），ASSET 類型的可預約
            // 資源沒有對應的 calendar_resource_mappings 可查，略過。
            return Ok(());
        }

        // **已知限制**：`reservations.external_event_id` 是單一欄位，若同一個
        // spatial_node 同時被兩個不同整合各自 ACTIVE 對應（`uq_calendar_resource_
        // mappings_node` 只擋同一整合內的重複，擋不住跨整合），第二次
        // `create_event` 會覆蓋第一次寫入的 id。Phase 1 只有 MS365 一種
        // provider，且 `uq_calendar_integrations_scope` 已經讓「同一個
        // provider、同一個範圍」只能有一個整合，這個情境需要刻意設定
        // 「租戶全域 + 特定場域各一個 MS365 整合」才會出現，是可以接受的
        // 邊界情況，不在這裡加額外的保護——真的需要多重外部對應時再處理。
        let mappings = sqlx::query!(
            r#"SELECT m.external_resource_id, i.provider, i.ms_tenant_id
                 FROM fms.calendar_resource_mappings m
                 JOIN fms.calendar_integrations i ON i.id = m.calendar_integration_id
                WHERE m.spatial_node_id = $1 AND m.status = 'ACTIVE'
                  AND i.status = 'ACTIVE'
                  AND m.sync_direction IN ('PUSH_ONLY', 'BIDIRECTIONAL')"#,
            reservation.resource_id
        )
        .fetch_all(tx.conn())
        .await?;

        for mapping in &mappings {
            let provider = match build_provider(
                &mapping.provider,
                mapping.ms_tenant_id.as_deref(),
                &self.ms_app_client_id,
                &self.ms_app_client_secret,
                self.ms_bases_override.as_ref(),
            ) {
                Ok(p) => p,
                Err(ProviderBuildIssue::MissingMsTenantId) => {
                    tracing::error!(
                        reservation_id = %event.aggregate_id,
                        "MS365 整合缺 ms_tenant_id，這個資源對應略過"
                    );
                    continue;
                }
                Err(ProviderBuildIssue::Unsupported(other)) => {
                    tracing::debug!(
                        provider = other,
                        "這個 provider 還沒有實作（ADR-14 決定 K：Google 留待之後），略過推出"
                    );
                    continue;
                }
            };

            if event.event_type == "reservation.cancelled" {
                if let Some(external_event_id) = &reservation.external_event_id {
                    provider
                        .cancel_event(&mapping.external_resource_id, external_event_id)
                        .await?;
                }
                // 沒有 external_event_id：從沒推出去過（或推出失敗），沒有東西要取消。
            } else if reservation.external_event_id.is_none() {
                // 見模組檔頭：已經有值就不重新建立。
                let outbound = OutboundEvent {
                    reservation_id: event.aggregate_id,
                    subject: reservation.title.clone().unwrap_or_default(),
                    start_at: reservation.start_at,
                    end_at: reservation.end_at,
                };
                let external_event_id = provider
                    .create_event(&mapping.external_resource_id, &outbound)
                    .await?;
                sqlx::query!(
                    "UPDATE fms.reservations SET external_event_id = $1 WHERE id = $2",
                    external_event_id,
                    event.aggregate_id
                )
                .execute(tx.conn())
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }
}

impl EventHandler for CalendarPushHandler {
    fn handles(&self, event_type: &str) -> bool {
        HANDLED_EVENTS.contains(&event_type)
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), String> {
        self.handle_inner(event).await.map_err(|e| e.to_string())
    }
}
