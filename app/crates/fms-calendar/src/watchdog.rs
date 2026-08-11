//! `CalendarSyncWatchdog`：拉入側的排程輪詢（ADR-14 決定 H：輪詢而非
//! webhook）。形狀完全比照 `fms_identity::directory_sync_watchdog`——單一
//! `fms_owner` 連線跨租戶找到期的整合，逐一開 per-tenant tx 處理，一個
//! 整合的失敗不中止整批。
//!
//! # 防迴圈與衝突判定（ADR-14 決定 I）
//!
//! 每個從外部拉回來的事件分兩種：
//!
//!   * 帶 `fms_reservation_id` 標記的 —— 是我們自己推出去的回音。時間對得上
//!     就什麼都不做；對不上或被刪除，代表使用者直接在外部日曆改了/刪了
//!     我們建立的事件，寫進 `calendar_sync_conflicts`，**不覆蓋** FMS 這邊
//!     的狀態。
//!   * 沒有標記的 —— 外部原生事件。這一半是「外部是權威來源」（ADR 的
//!     模型 B）：`reservations` 上這一列的 `created_via='OUTLOOK'`，唯一的
//!     編輯入口就是外部日曆本身（`fms-reservation` 拒絕從 FMS 端改這種
//!     來源的預約），因此直接覆蓋時間/標題不算「靜默丟棄使用者的修改」——
//!     沒有第二個修改來源可以丟。
//!
//! # 為什麼每個事件的寫入要包一個 SAVEPOINT
//!
//! 一整個整合的所有資源對應共用同一個 tenant tx（提交時就是這一輪同步的
//! 邊界）。但插入一列外部原生事件時可能撞上既有的 `excl_reservations_no_overlap`
//! （有人已經在 FMS 這邊訂了同一個時段，跟外部日曆各自為政地訂了同一個
//! 房間）——這是**真正的雙系統衝突**，該進 `calendar_sync_conflicts`，不該讓
//! 整個交易失敗、拖累同一輪其他資源對應的同步。SAVEPOINT 讓這類「預期內會
//! 失敗」的寫入可以局部回滾，不影響交易的其餘部分。

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, ActorType, Problem, ProblemCode, TenantContext, TenantTx};

use crate::provider::{CalendarProvider, ExternalEvent};

pub struct CalendarSyncWatchdog {
    /// 必須是 `fms_owner`：`due_integrations` 要跨租戶讀
    /// `calendar_integrations`，該表是 FORCE RLS。與
    /// `DirectorySyncWatchdog::pool` 同一個理由。
    pool: PgPool,
    /// 寫入身分——合成的 OUTLOOK/GOOGLE 來源預約的 `organizer_id`，
    /// 因為外部日曆的真實主辦人不一定在 FMS 有帳號（`organizer_id` 是
    /// NOT NULL，不能留空）。必須是持有對應權限的服務帳號。
    actor_user_id: Uuid,
    ms_app_client_id: String,
    ms_app_client_secret: String,
    /// 測試專用：把 Microsoft 端點換成本地假伺服器，見
    /// `MicrosoftGraphProvider::with_bases`。生產環境是 `None`，用預設的
    /// 真實端點。
    ms_bases_override: Option<(String, String)>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Sweep {
    pub succeeded: i32,
    pub failed: i32,
    /// provider 欄位是 `GOOGLE` 但還沒有實作（ADR-14：Phase 1 只做
    /// Microsoft）——這不是錯誤，是明確排除的範圍，因此獨立計數，
    /// 不跟 `failed` 混在一起讓維運誤以為是故障。
    pub skipped_unsupported_provider: i32,
}

impl Sweep {
    pub fn is_quiet(&self) -> bool {
        self.succeeded == 0 && self.failed == 0 && self.skipped_unsupported_provider == 0
    }
}

impl CalendarSyncWatchdog {
    pub fn new(
        pool: PgPool,
        actor_user_id: Uuid,
        ms_app_client_id: String,
        ms_app_client_secret: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            actor_user_id,
            ms_app_client_id,
            ms_app_client_secret,
            ms_bases_override: None,
        })
    }

    /// 測試專用：把 Microsoft 端點換成本地假伺服器（`(token_base, graph_base)`）。
    #[doc(hidden)]
    pub fn with_ms_bases(
        pool: PgPool,
        actor_user_id: Uuid,
        token_base: String,
        graph_base: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            actor_user_id,
            ms_app_client_id: "test-client-id".to_string(),
            ms_app_client_secret: "test-client-secret".to_string(),
            ms_bases_override: Some((token_base, graph_base)),
        })
    }

    pub async fn run_once(&self) -> Result<Sweep, Problem> {
        let due = self.due_integrations().await?;
        let mut sweep = Sweep::default();

        for row in due {
            let provider = match crate::provider::build_provider(
                &row.provider,
                row.ms_tenant_id.as_deref(),
                &self.ms_app_client_id,
                &self.ms_app_client_secret,
                self.ms_bases_override.as_ref(),
            ) {
                Ok(p) => p,
                Err(crate::provider::ProviderBuildIssue::MissingMsTenantId) => {
                    sweep.failed += 1;
                    tracing::error!(integration_id = %row.id, "MS365 整合缺 ms_tenant_id，略過這一輪");
                    continue;
                }
                Err(crate::provider::ProviderBuildIssue::Unsupported(other)) => {
                    sweep.skipped_unsupported_provider += 1;
                    tracing::debug!(
                        integration_id = %row.id, provider = other,
                        "這個 provider 還沒有實作（ADR-14 決定 K：Google 留待之後）"
                    );
                    continue;
                }
            };

            match self
                .sync_one(row.tenant_id, row.id, provider.as_ref())
                .await
            {
                Ok(()) => sweep.succeeded += 1,
                Err(err) => {
                    sweep.failed += 1;
                    tracing::error!(error = %err, integration_id = %row.id, "日曆同步失敗，下一輪重試");
                    self.record_sync_error(row.id, &err).await;
                }
            }
        }

        Ok(sweep)
    }

    /// 失敗訊息回寫到 `calendar_integrations.last_sync_error`，讓管理者在
    /// `GET /calendar-integrations` 看得到「上次同步為什麼失敗」，不必去翻
    /// 伺服器日誌。用獨立連線——這一輪的 tenant tx 已經因為錯誤而作廢。
    async fn record_sync_error(&self, integration_id: Uuid, err: &Problem) {
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(_) => return,
        };
        if sqlx::query("SELECT set_config('app.is_platform', 'on', true)")
            .execute(&mut *tx)
            .await
            .is_err()
        {
            return;
        }
        let _ =
            sqlx::query("UPDATE fms.calendar_integrations SET last_sync_error = $1 WHERE id = $2")
                .bind(err.to_string())
                .bind(integration_id)
                .execute(&mut *tx)
                .await;
        let _ = tx.commit().await;
    }

    async fn due_integrations(&self) -> Result<Vec<DueIntegration>, Problem> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('app.is_platform', 'on', true)")
            .execute(&mut *tx)
            .await?;

        let rows: Vec<DueIntegration> = sqlx::query_as!(
            DueIntegration,
            r#"SELECT tenant_id, id, provider, ms_tenant_id, sync_cron, last_synced_at
                 FROM fms.calendar_integrations
                WHERE status = 'ACTIVE'"#
        )
        .fetch_all(&mut *tx)
        .await?;

        tx.rollback().await?;

        let now = Utc::now();
        let mut due = Vec::new();
        for row in rows {
            match fms_shared::cron::is_due(&row.sync_cron, row.last_synced_at, now) {
                Ok(true) => due.push(row),
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(error = %err, integration_id = %row.id, "sync_cron 格式不合法，略過這個整合");
                }
            }
        }
        Ok(due)
    }

    async fn sync_one(
        &self,
        tenant_id: Uuid,
        integration_id: Uuid,
        provider: &dyn CalendarProvider,
    ) -> Result<(), Problem> {
        let mut tx = begin_tenant_tx(
            &self.pool,
            TenantContext::background(tenant_id, self.actor_user_id, ActorType::ServiceAccount),
        )
        .await?;

        let mappings: Vec<MappingRow> = sqlx::query_as!(
            MappingRow,
            r#"SELECT id, spatial_node_id, external_resource_id, delta_token
                 FROM fms.calendar_resource_mappings
                WHERE calendar_integration_id = $1 AND status = 'ACTIVE'"#,
            integration_id
        )
        .fetch_all(tx.conn())
        .await?;

        for mapping in mappings {
            let page = provider
                .fetch_events_delta(
                    &mapping.external_resource_id,
                    mapping.delta_token.as_deref(),
                )
                .await?;

            for event in &page.events {
                self.reconcile_event(
                    &mut tx,
                    tenant_id,
                    mapping.id,
                    mapping.spatial_node_id,
                    event,
                )
                .await?;
            }

            sqlx::query!(
                "UPDATE fms.calendar_resource_mappings SET delta_token = $1 WHERE id = $2",
                page.next_delta_token,
                mapping.id
            )
            .execute(tx.conn())
            .await?;
        }

        sqlx::query!(
            "UPDATE fms.calendar_integrations
                SET last_synced_at = clock_timestamp(), last_sync_error = NULL
              WHERE id = $1",
            integration_id
        )
        .execute(tx.conn())
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn reconcile_event(
        &self,
        tx: &mut TenantTx,
        tenant_id: Uuid,
        mapping_id: Uuid,
        spatial_node_id: Uuid,
        event: &ExternalEvent,
    ) -> Result<(), Problem> {
        match event.fms_reservation_id {
            Some(reservation_id) => {
                self.reconcile_echo(tx, mapping_id, reservation_id, event)
                    .await
            }
            None => {
                self.reconcile_external(tx, tenant_id, mapping_id, spatial_node_id, event)
                    .await
            }
        }
    }

    /// 這個事件帶著我們自己的標記——比對時間是否一致，不一致就進待審佇列。
    async fn reconcile_echo(
        &self,
        tx: &mut TenantTx,
        mapping_id: Uuid,
        reservation_id: Uuid,
        event: &ExternalEvent,
    ) -> Result<(), Problem> {
        let existing = sqlx::query!(
            r#"SELECT status, start_at, end_at FROM fms.reservations WHERE id = $1"#,
            reservation_id
        )
        .fetch_optional(tx.conn())
        .await?;

        let Some(existing) = existing else {
            // 我們推出去的事件標記著一個 FMS 這邊已經不存在的預約 id——
            // 理論上不該發生（推出與這裡讀的是同一個 tenant 的同一張表），
            // 但外部系統的資料不受我們控制，記錄下來但不中止整輪同步。
            tracing::warn!(%reservation_id, "回音事件標記的預約在 FMS 端找不到");
            return Ok(());
        };

        const ACTIVE_STATUSES: [&str; 3] = ["PENDING_APPROVAL", "CONFIRMED", "CHECKED_IN"];
        let fms_is_active = ACTIVE_STATUSES.contains(&existing.status.as_str());

        let time_matches = !event.is_deleted
            && (event.start_at - existing.start_at).num_seconds().abs() < 2
            && (event.end_at - existing.end_at).num_seconds().abs() < 2;

        if time_matches || (event.is_deleted && !fms_is_active) {
            // 回音與 FMS 端一致（時間對得上，或兩邊都已經是「不生效」的狀態）。
            return Ok(());
        }

        self.write_conflict(
            tx,
            reservation_id,
            mapping_id,
            &event.external_event_id,
            serde_json::json!({
                "status": existing.status,
                "start_at": existing.start_at,
                "end_at": existing.end_at,
            }),
            serde_json::json!({
                "is_deleted": event.is_deleted,
                "start_at": event.start_at,
                "end_at": event.end_at,
            }),
        )
        .await
    }

    /// 這個事件沒有我們的標記——外部原生事件。外部是這種列的權威來源
    /// （見模組檔頭），因此直接 upsert／取消，不走待審佇列。
    async fn reconcile_external(
        &self,
        tx: &mut TenantTx,
        tenant_id: Uuid,
        mapping_id: Uuid,
        spatial_node_id: Uuid,
        event: &ExternalEvent,
    ) -> Result<(), Problem> {
        let existing = sqlx::query!(
            r#"SELECT id, status FROM fms.reservations
                WHERE tenant_id = $1 AND external_event_id = $2"#,
            tenant_id,
            event.external_event_id
        )
        .fetch_optional(tx.conn())
        .await?;

        if event.is_deleted {
            if let Some(existing) = existing {
                sqlx::query!(
                    r#"UPDATE fms.reservations
                          SET status = 'CANCELLED', cancelled_at = clock_timestamp(),
                              cancellation_reason = '外部日曆已刪除此事件'
                        WHERE id = $1 AND status NOT IN ('CANCELLED','COMPLETED','REJECTED','NO_SHOW','EXPIRED')"#,
                    existing.id
                )
                .execute(tx.conn())
                .await?;
            }
            return Ok(());
        }

        if let Some(existing) = existing {
            // 外部事件被移動或改名——外部是權威來源，直接覆蓋。
            sqlx::query!(
                r#"UPDATE fms.reservations
                      SET start_at = $1, end_at = $2, title = $3
                    WHERE id = $4"#,
                event.start_at,
                event.end_at,
                event.subject,
                existing.id
            )
            .execute(tx.conn())
            .await?;
            return Ok(());
        }

        // 全新的外部事件，需要先解出對應的 bookable_resource。
        let bookable = sqlx::query!(
            r#"SELECT id, facility_id FROM fms.bookable_resources
                WHERE spatial_node_id = $1 AND resource_type = 'SPATIAL_NODE'"#,
            spatial_node_id
        )
        .fetch_optional(tx.conn())
        .await?;

        let Some(bookable) = bookable else {
            tracing::error!(
                %spatial_node_id,
                "calendar_resource_mappings 指向的空間節點不是 bookable_resource，略過這個事件"
            );
            return Ok(());
        };

        // 用 SAVEPOINT 包住這個 INSERT：可能撞上
        // excl_reservations_no_overlap（真正的雙系統衝突，見模組檔頭），
        // 那不該讓整個交易（含其他資源對應的同步）一起失敗。
        sqlx::query("SAVEPOINT calendar_sync_event")
            .execute(tx.conn())
            .await?;

        let insert_result = sqlx::query!(
            r#"INSERT INTO fms.reservations
                 (tenant_id, facility_id, bookable_resource_id, reservation_no,
                  resource_type, resource_id, organizer_id, title,
                  party_size, start_at, end_at, status, created_via, external_event_id)
               VALUES
                 ($1, $2, $3, fms.next_document_no($1, 'RESERVATION', 'RSV'),
                  'SPATIAL_NODE', $4, $5, $6, 1, $7, $8, 'CONFIRMED', 'OUTLOOK', $9)"#,
            tenant_id,
            bookable.facility_id,
            bookable.id,
            spatial_node_id,
            self.actor_user_id,
            event.subject,
            event.start_at,
            event.end_at,
            event.external_event_id,
        )
        .execute(tx.conn())
        .await;

        match insert_result {
            Ok(_) => {
                sqlx::query("RELEASE SAVEPOINT calendar_sync_event")
                    .execute(tx.conn())
                    .await?;
                Ok(())
            }
            Err(err) => {
                let problem = Problem::from(err);
                sqlx::query("ROLLBACK TO SAVEPOINT calendar_sync_event")
                    .execute(tx.conn())
                    .await?;
                sqlx::query("RELEASE SAVEPOINT calendar_sync_event")
                    .execute(tx.conn())
                    .await?;

                if problem.code == ProblemCode::ReservationConflict {
                    self.write_conflict_for_new_external_event(
                        tx,
                        tenant_id,
                        spatial_node_id,
                        mapping_id,
                        event,
                    )
                    .await
                } else {
                    Err(problem)
                }
            }
        }
    }

    /// 一個全新的外部事件撞上既有的 FMS 預約時，把它記到既有預約底下——
    /// 這種情況沒有 FMS 端的 `reservation_id` 可以歸屬給*這個外部事件*，
    /// 因此附掛在**擋住它的那一列**現有預約上，讓管理者知道「這個房間有
    /// 兩邊各自為政的預約在打架」。
    async fn write_conflict_for_new_external_event(
        &self,
        tx: &mut TenantTx,
        tenant_id: Uuid,
        spatial_node_id: Uuid,
        mapping_id: Uuid,
        event: &ExternalEvent,
    ) -> Result<(), Problem> {
        let blocker = sqlx::query!(
            r#"SELECT r.id, r.status, r.start_at, r.end_at
                 FROM fms.reservations r
                 JOIN fms.bookable_resources b ON b.id = r.bookable_resource_id
                WHERE r.tenant_id = $1 AND b.spatial_node_id = $2
                  AND r.status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN')
                  AND r.time_range && tstzrange($3, $4, '[)')
                LIMIT 1"#,
            tenant_id,
            spatial_node_id,
            event.start_at,
            event.end_at,
        )
        .fetch_optional(tx.conn())
        .await?;

        let Some(blocker) = blocker else {
            // 競態視窗：擋住它的那一列在寫入衝突記錄前被取消了。
            // 沒有東西可以歸屬，下一輪 delta 若這個外部事件還在會再試一次。
            tracing::warn!(
                external_event_id = %event.external_event_id,
                "偵測到雙系統時段衝突，但找不到擋住它的既有預約（可能剛好被取消）"
            );
            return Ok(());
        };

        self.write_conflict(
            tx,
            blocker.id,
            mapping_id,
            &event.external_event_id,
            serde_json::json!({
                "status": blocker.status,
                "start_at": blocker.start_at,
                "end_at": blocker.end_at,
            }),
            serde_json::json!({
                "subject": event.subject,
                "start_at": event.start_at,
                "end_at": event.end_at,
            }),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_conflict(
        &self,
        tx: &mut TenantTx,
        reservation_id: Uuid,
        mapping_id: Uuid,
        external_event_id: &str,
        fms_side: serde_json::Value,
        external_side: serde_json::Value,
    ) -> Result<(), Problem> {
        sqlx::query!(
            r#"INSERT INTO fms.calendar_sync_conflicts
                 (tenant_id, reservation_id, calendar_resource_mapping_id,
                  external_event_id, fms_side_snapshot, external_side_snapshot)
               SELECT tenant_id, $1, $2, $3, $4, $5 FROM fms.reservations WHERE id = $1"#,
            reservation_id,
            mapping_id,
            external_event_id,
            fms_side,
            external_side,
        )
        .execute(tx.conn())
        .await?;
        Ok(())
    }
}

struct DueIntegration {
    tenant_id: Uuid,
    id: Uuid,
    provider: String,
    ms_tenant_id: Option<String>,
    sync_cron: String,
    last_synced_at: Option<DateTime<Utc>>,
}

struct MappingRow {
    id: Uuid,
    spatial_node_id: Uuid,
    external_resource_id: String,
    delta_token: Option<String>,
}

/// 掃描間隔設定——與 `DirectorySyncWatchdogConfig` 同一個量級（ADR-14 決定 H）。
#[derive(Debug, Clone)]
pub struct CalendarSyncWatchdogConfig {
    pub interval: Duration,
}

impl Default for CalendarSyncWatchdogConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5 * 60),
        }
    }
}

/// 常駐迴圈。啟動時立刻跑一次，理由與 `directory_sync_watchdog` 相同：
/// 部署或重啟期間錯過的排程時刻，該在恢復的第一時間補上。
pub async fn run_until_shutdown(
    watchdog: Arc<CalendarSyncWatchdog>,
    cfg: CalendarSyncWatchdogConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("日曆同步排程迴圈收到關機訊號");
            return;
        }

        match watchdog.run_once().await {
            Ok(s) if s.is_quiet() => tracing::debug!("日曆同步排程：這輪沒有到期的整合"),
            Ok(s) => tracing::info!(
                succeeded = s.succeeded,
                failed = s.failed,
                skipped_unsupported_provider = s.skipped_unsupported_provider,
                "日曆同步排程完成"
            ),
            Err(err) => tracing::error!(error = %err, "日曆同步排程掃描失敗，下一輪重試"),
        }

        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
