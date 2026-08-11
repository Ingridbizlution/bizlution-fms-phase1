//! 背景作業的執行檔：outbox relay + 通知扇出 + 通知投遞 + PM 計畫掃描
//! + no-show 掃描 + 分區維護 + SLA 逾期掃描。
//!
//! 連線角色刻意是 `fms_owner` 而非 `fms_app`：每個迴圈都必須跨租戶運作，
//! 而相關表都有 FORCE RLS（詳見 lib.rs 模組說明與
//! `fms_maintenance::pm_worker::PmGenerator` 的欄位註解）。
//!
//! # 這些迴圈為什麼在同一個程序
//!
//! 它們共用連線池與關機訊號，而兩者的負載都很低（relay 空轉時每 2 秒一次
//! 查詢、掃描每小時一次）。拆成兩個程序會多一份部署與監控成本，
//! 換不到什麼。若日後某一邊的量成長到會拖累另一邊，
//! `RelayConfig.event_types` 已經支援依事件家族分片。

use std::sync::Arc;
use std::time::Duration;

use fms_maintenance::pm_worker::{PmGenerator, ScanConfig};
use fms_maintenance::relay_handler::{MaintenanceEventHandler, HANDLED_EVENT};
use fms_worker::{run_until_shutdown, RelayConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    // `_guard` 不能寫成 `_`：後者會立刻 drop，匯出器在第一個請求之前就關掉，
    // 而症狀是「collector 什麼都沒收到」，看起來像設定錯誤。
    let _guard = fms_shared::init_telemetry("fms-jobs");

    let url = std::env::var("OWNER_DATABASE_URL").map_err(|_| {
        anyhow::anyhow!("OWNER_DATABASE_URL 未設定（背景作業需以 fms_owner 連線以跨租戶運作）")
    })?;

    // 產生器的寫入身分。必須是**有 TENANT 範圍角色指派**的服務帳號
    // （見 migration 019）：沒有指派的使用者可見場域為空，
    // 場域級 RLS 會濾掉每一列，產生器會安靜地什麼都不做。
    let actor = std::env::var("PM_GENERATOR_USER_ID")
        .map_err(|_| anyhow::anyhow!("PM_GENERATOR_USER_ID 未設定（見 migration 019）"))?
        .parse::<uuid::Uuid>()
        .map_err(|e| anyhow::anyhow!("PM_GENERATOR_USER_ID 不是合法的 uuid: {e}"))?;

    // 目錄同步排程的寫入身分。同一個理由（見 019 與 078 的檔頭）：
    // 沒有 TENANT 範圍角色指派的使用者，場域級 RLS 會濾掉每一列，
    // 排程會安靜地什麼都不做。
    let directory_sync_actor = std::env::var("DIRECTORY_SYNC_USER_ID")
        .map_err(|_| anyhow::anyhow!("DIRECTORY_SYNC_USER_ID 未設定（見 migration 078）"))?
        .parse::<uuid::Uuid>()
        .map_err(|e| anyhow::anyhow!("DIRECTORY_SYNC_USER_ID 不是合法的 uuid: {e}"))?;

    // 日曆同步（ADR-14）的寫入身分。**選擇性**——與通知投遞同一個判斷
    // （見下方 SMTP_URL 那段）：不是每個部署都已經設定日曆整合，沒設就不啟動
    // 這個迴圈，而不是啟動一個永遠掃不到任何整合的空轉迴圈。
    // MS365 的 app 憑證是全平台共用一份（不是 per-tenant），因此只需要一組。
    let calendar_sync_actor = match std::env::var("CALENDAR_SYNC_USER_ID") {
        Ok(v) => Some(
            v.parse::<uuid::Uuid>()
                .map_err(|e| anyhow::anyhow!("CALENDAR_SYNC_USER_ID 不是合法的 uuid: {e}"))?,
        ),
        Err(_) => {
            tracing::warn!(
                "未設定 CALENDAR_SYNC_USER_ID —— 日曆同步排程不啟動（見 migration 084）"
            );
            None
        }
    };
    let ms365_app_client_id = std::env::var("MS365_APP_CLIENT_ID").unwrap_or_default();
    let ms365_app_client_secret = std::env::var("MS365_APP_CLIENT_SECRET").unwrap_or_default();

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(6)
        .connect(&url)
        .await?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(true);
        }
    });

    let generator = PmGenerator::new(pool.clone(), actor);

    // relay：只處理計量門檻事件。其餘型別會被標為 SKIPPED 而不是假裝成功
    // —— 讓「沒有人處理的事件」在資料庫裡看得出來。
    let relay = tokio::spawn(run_until_shutdown(
        pool.clone(),
        MaintenanceEventHandler {
            generator: Arc::clone(&generator),
        },
        RelayConfig {
            idle_interval: Duration::from_secs(2),
            // 分片：本程序只排空維護事件，不動別的事件家族。
            event_types: Some(vec![HANDLED_EVENT.to_string()]),
            ..Default::default()
        },
        shutdown_rx.clone(),
    ));

    // 通知扇出：第二個 relay，分片在「宣告了 notify 的轉移會發出的事件型別」。
    //
    // **必須分片。** 兩個沒有分片的 relay 會互相把對方的事件標成 SKIPPED
    // —— 那會靜默銷毀事件。而分片的清單是**從目錄查出來的**，不是寫死的：
    // 管理者為某個轉移加上 notify 之後，重啟就會納入。
    let notifier = fms_worker::notifier::NotificationHandler::new(pool.clone()).await?;
    let notify_types = notifier.event_types.clone();
    tracing::info!(
        count = notify_types.len(),
        "通知扇出的事件型別（由目錄查出）"
    );
    let notifications = tokio::spawn(run_until_shutdown(
        pool.clone(),
        notifier,
        RelayConfig {
            idle_interval: Duration::from_secs(2),
            event_types: Some(notify_types),
            ..Default::default()
        },
        shutdown_rx.clone(),
    ));

    // 稽核匯出：第三個 relay，分片在單一事件型別。
    //
    // **分片是必須的而不是選項** —— 見上面通知扇出的說明：兩個沒有分片的
    // relay 會互相把對方的事件標成 SKIPPED，靜默銷毀事件。
    //
    // 物件儲存在啟動時就建立：設定缺漏該讓程序起不來，而不是等到第一份匯出
    // 才失敗 —— 那時使用者已經按了按鈕，而他看到的是一個永遠 RUNNING 的作業。
    let export_storage = fms_shared::Storage::new(
        &fms_shared::StorageSettings::from_env()
            .map_err(|e| anyhow::anyhow!("物件儲存未設定，稽核匯出無法運作：{e}"))?,
    );
    let exports = tokio::spawn(run_until_shutdown(
        pool.clone(),
        fms_worker::audit_export::AuditExportHandler::new(pool.clone(), export_storage.clone()),
        RelayConfig {
            idle_interval: Duration::from_secs(2),
            event_types: Some(vec![fms_worker::audit_export::EVENT_TYPE.to_string()]),
            ..Default::default()
        },
        shutdown_rx.clone(),
    ));

    // 報表匯出：第四個 relay，分片在它自己的事件型別。
    //
    // **不與稽核匯出共用一個 relay。** 兩者的事件型別不同，而 relay 的分片
    // 是「只取這些型別」—— 一個 relay 帶兩個型別會讓一份跨數百萬列的稽核
    // 匯出擋住後面所有報表匯出（反之亦然）。分開之後兩條佇列互不影響。
    let report_exports = tokio::spawn(run_until_shutdown(
        pool.clone(),
        fms_worker::report_export::ReportExportHandler::new(pool.clone(), export_storage),
        RelayConfig {
            idle_interval: Duration::from_secs(2),
            event_types: Some(vec![fms_worker::report_export::EVENT_TYPE.to_string()]),
            ..Default::default()
        },
        shutdown_rx.clone(),
    ));

    // 掃描：日曆型計畫沒有「發生點」可以發事件，只能定期問誰到期了。
    let scan = tokio::spawn(fms_maintenance::pm_worker::run_scan_until_shutdown(
        generator,
        ScanConfig::default(),
        shutdown_rx.clone(),
    ));

    // no-show：同樣沒有發生點 ——「時間到了而某人沒出現」不是任何人的動作。
    let no_show = tokio::spawn(fms_reservation::no_show::run_until_shutdown(
        fms_reservation::no_show::NoShowScanner::new(pool.clone(), actor),
        fms_reservation::no_show::ScanConfig::default(),
        shutdown_rx.clone(),
    ));

    // 預約提醒：同樣沒有發生點——「開會前 15 分鐘」不是任何動作觸發的，
    // 掃描與範本渲染都在 085 的 SQL 函式裡，這裡只是排程外殼。
    let reminders = tokio::spawn(fms_reservation::reminder::run_until_shutdown(
        fms_reservation::reminder::ReminderScanner::new(pool.clone()),
        fms_reservation::reminder::ReminderConfig::default(),
        shutdown_rx.clone(),
    ));

    // 分區維護：儲存層的維護，沒有領域，因此住在 fms-worker（機制）而非
    // 任何領域 crate。001 只建到 2026m08 —— 沒有這個迴圈，9 月起的列會全部
    // 落進 DEFAULT，而那個狀態會自我鎖死（見 migration 028）。
    let partitions = tokio::spawn(fms_worker::partitions::run_until_shutdown(
        fms_worker::partitions::PartitionMaintainer::new(pool.clone()),
        fms_worker::partitions::PartitionConfig::default(),
        shutdown_rx.clone(),
    ));

    // 通知投遞。**沒有設 SMTP_URL 就不啟動** —— 那樣 QUEUED 才誠實地代表
    // 「沒有傳輸層」，而不是「dispatcher 壞了」。啟動時記一筆 warn，
    // 因為一個不會送通知的部署應該有人知道。
    let mail = match (std::env::var("SMTP_URL"), std::env::var("MAIL_FROM")) {
        (Ok(url), Ok(from)) => match fms_worker::dispatcher::SmtpMailer::new(&url, &from) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::error!(error = %e, "SMTP 設定無法解析 —— 通知投遞不會啟動");
                None
            }
        },
        _ => {
            tracing::warn!(
                "未設定 SMTP_URL / MAIL_FROM —— 通知投遞不啟動，EMAIL 通知會留在 QUEUED"
            );
            None
        }
    };
    let dispatcher = mail.map(|m| {
        tokio::spawn(fms_worker::dispatcher::run_until_shutdown(
            fms_worker::dispatcher::NotificationDispatcher::new(pool.clone(), m),
            fms_worker::dispatcher::DispatcherConfig::default(),
            shutdown_rx.clone(),
        ))
    });

    // SLA 逾期：與上面兩個掃描同一個形狀 ——「時間到了而某事沒有發生」
    // 沒有發生點。沒有這個迴圈，沒有人碰的工單逾期了也不會被標記
    // （報表不受影響，它從時刻算；但 UI 上會一直顯示 ON_TRACK）。
    let sla = tokio::spawn(fms_worker::sla_watchdog::run_until_shutdown(
        fms_worker::sla_watchdog::SlaWatchdog::new(pool.clone()),
        fms_worker::sla_watchdog::WatchdogConfig::default(),
        shutdown_rx.clone(),
    ));

    // 證照到期掃描。一天一次 —— 證照到期是以天為單位的事實，
    // 而 059 的幂等靠 reminded_for_expiry，更頻繁只是白掃整張表。
    //
    // 這是 idx_user_skills_expiring 的第一個讀者（004 為它建了那個部分索引，
    // 而到期提醒那一半直到 059 才實作）。
    let certs = tokio::spawn(fms_worker::cert_watchdog::run_until_shutdown(
        fms_worker::cert_watchdog::CertWatchdog::new(pool.clone()),
        fms_worker::cert_watchdog::CertWatchdogConfig::default(),
        shutdown_rx.clone(),
    ));

    // webhook 扇出：事件 → notifications（channel = WEBHOOK）。
    // 與通知扇出同一個位置（relay 的一個 handler），因此重試與退避是共用的。
    let webhook_fanout = tokio::spawn(run_until_shutdown(
        pool.clone(),
        fms_worker::webhook::WebhookFanout::new(pool.clone()),
        RelayConfig {
            event_types: Some(
                fms_worker::webhook::SUBSCRIBABLE_EVENT_TYPES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            ..RelayConfig::default()
        },
        shutdown_rx.clone(),
    ));

    // webhook 投遞。**傳輸層在自己的迴圈**，不在 notification dispatcher 裡 ——
    // 兩者共用 notifications 佇列與退避語意，但一個要 SMTP、一個要 HMAC 簽章
    // 與 SSRF 閘門。dispatcher 有一個 `DELIVERED_BY_OTHER_LOOP` 常數把 WEBHOOK
    // 排除在「沒有傳輸層 → 停放」那一段之外；少了它這個迴圈看不到任何列。
    let webhooks = tokio::spawn(fms_worker::webhook::run_until_shutdown(
        fms_worker::webhook::WebhookDispatcher::new(
            pool.clone(),
            fms_shared::OutboundSettings::default(),
        ),
        fms_worker::webhook::WebhookConfig::default(),
        shutdown_rx.clone(),
    ));

    // refresh token 撤銷紀錄的清理。070 的 revoked_refresh_tokens 每次換發都
    // 多一列（那是 logout 正確性的前提，見該 migration 檔頭），而列在 token
    // 過期後就沒有作用了。沒有這個迴圈，那張表只增不減。
    let token_purge = tokio::spawn(fms_worker::token_purge::run_until_shutdown(
        fms_worker::token_purge::TokenPurger::new(pool.clone()),
        fms_worker::token_purge::TokenPurgeConfig::default(),
        shutdown_rx.clone(),
    ));

    // 目錄同步排程：058 記過的已知限制（排程觸發沒有人類觸發者），
    // 078 補上服務帳號當答案。與 PM 日曆掃描同一個形狀 ——
    // 「排程時刻到了」沒有發生點，只能定期問誰到期了。
    let directory_sync = tokio::spawn(fms_identity::directory_sync_watchdog::run_until_shutdown(
        fms_identity::directory_sync_watchdog::DirectorySyncWatchdog::new(
            pool.clone(),
            directory_sync_actor,
        ),
        fms_identity::directory_sync_watchdog::DirectorySyncWatchdogConfig::default(),
        shutdown_rx.clone(),
    ));

    // 日曆同步（ADR-14）：拉入側跟目錄同步排程同一個形狀（輪詢外部日曆而非
    // 目錄服務）；推出側是第四個 relay，分片在
    // `CalendarPushHandler::HANDLED_EVENTS`。兩者共用同一個服務帳號與
    // MS365 app 憑證——Google Workspace 還沒有實作（決定 K），watchdog 對
    // 那個 provider 的整合會計成 skipped_unsupported_provider，推出側會
    // 直接略過，都不是失敗。
    let calendar_sync = calendar_sync_actor.map(|actor| {
        let watchdog = tokio::spawn(fms_calendar::watchdog::run_until_shutdown(
            fms_calendar::CalendarSyncWatchdog::new(
                pool.clone(),
                actor,
                ms365_app_client_id.clone(),
                ms365_app_client_secret.clone(),
            ),
            fms_calendar::CalendarSyncWatchdogConfig::default(),
            shutdown_rx.clone(),
        ));
        let push = tokio::spawn(run_until_shutdown(
            pool.clone(),
            fms_calendar::CalendarPushHandler::new(
                pool.clone(),
                actor,
                ms365_app_client_id,
                ms365_app_client_secret,
            ),
            RelayConfig {
                idle_interval: Duration::from_secs(2),
                event_types: Some(
                    fms_calendar::push::HANDLED_EVENTS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                ),
                ..Default::default()
            },
            shutdown_rx,
        ));
        (watchdog, push)
    });

    tracing::info!(
        dispatcher = dispatcher.is_some(),
        calendar_sync = calendar_sync.is_some(),
        "背景作業啟動：outbox relay + 通知扇出 + 稽核匯出 + 報表匯出 + PM 掃描 + no-show 掃描 + 預約提醒掃描 + 分區維護 + SLA 逾期掃描 + 證照到期掃描 + 撤銷紀錄清理 + webhook 扇出與投遞 + 目錄同步排程 + 日曆同步排程"
    );
    // 全部都在收到關機訊號後才回傳；任一個 panic 都應讓程序結束，
    // 因為半個 worker 在跑比完全沒跑更難察覺。
    let (
        relay_result,
        notify_result,
        export_result,
        report_export_result,
        scan_result,
        no_show_result,
        reminders_result,
        partition_result,
        sla_result,
        cert_result,
        token_purge_result,
        webhook_fanout_result,
        webhook_result,
        directory_sync_result,
    ) = tokio::join!(
        relay,
        notifications,
        exports,
        report_exports,
        scan,
        no_show,
        reminders,
        partitions,
        sla,
        certs,
        token_purge,
        webhook_fanout,
        webhooks,
        directory_sync
    );
    relay_result?;
    notify_result?;
    export_result?;
    report_export_result?;
    scan_result?;
    no_show_result?;
    reminders_result?;
    partition_result?;
    sla_result?;
    cert_result?;
    token_purge_result?;
    webhook_fanout_result?;
    webhook_result?;
    directory_sync_result?;
    if let Some(handle) = dispatcher {
        handle.await?;
    }
    if let Some((watchdog, push)) = calendar_sync {
        watchdog.await?;
        push.await?;
    }
    tracing::info!("背景作業已停止");
    Ok(())
}
