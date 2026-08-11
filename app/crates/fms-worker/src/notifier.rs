//! 通知扇出的 relay handler（呼叫 041 的 `fms.fan_out_notifications`）。
//!
//! # 為什麼在 fms-worker 而不是某個領域 crate
//!
//! `fms-maintenance` 的 `relay_handler` 檔頭寫了相反的理由：那裡是**政策**
//! （relay 不該認識維護計畫）。這裡不同 —— 扇出沒有領域知識：
//! 誰該收到、用哪個範本、變數怎麼填，全都在資料裡
//! （`side_effects.notify` / `side_effects.template` / `notification_templates`）。
//! 這一層只負責「把事件交給那個函式」與「把結果記進 log」。
//!
//! # 處理哪些事件由**目錄**決定
//!
//! `handled_event_types()` 去查 `work_order_transitions_allowed` 裡宣告了
//! `notify` 的規則的 `emit` 值。寫死一份清單會與目錄脫節，而脫節的症狀是
//! 靜默的：管理者為某個轉移加了 `notify`，事件照發，但 relay 不認識它 ——
//! 那筆事件會被標成 `SKIPPED`，看起來像「沒有人要處理」。
//!
//! # 這一層不投遞
//!
//! 041 只建 `notifications` 列。`IN_APP` 的列本身就是通知
//! （`GET /notifications` 讀得到）；`EMAIL`／`PUSH` 需要 SMTP／推播傳輸層，
//! 那是獨立的一件事，因此那些列會停在 `QUEUED`。

use sqlx::PgPool;

/// 一次扇出的結果。
///
/// 三個數字分開回，因為在維運上它們是三種不同的訊息 ——
/// 後兩者都是「宣告了要通知但**不會有人收到**」。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FanOut {
    pub created: i32,
    /// 規則宣告了 `notify` 但沒有對應範本。009 種的 12 個範本裡只有三個
    /// 對得上有 `notify` 的轉移；其餘十條會落在這裡。
    pub no_template: i32,
    /// 有代號解析不到任何人。`APPROVER` 就是這一類 —— 它既不是角色碼
    /// 也不是工單的欄位。
    pub unresolved: i32,
}

/// 查出目錄裡所有「宣告了 notify」的轉移會發出的事件型別。
///
/// 在 relay 啟動時呼叫一次，結果同時用於分片（`RelayConfig.event_types`）
/// 與 `handles()`，因此那兩者不可能不一致。
pub async fn handled_event_types(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    let mut tx = crate::begin_platform_tx(pool).await?;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT side_effects ->> 'emit'
           FROM fms.work_order_transitions_allowed
          WHERE is_active
            AND side_effects ? 'notify'
            AND side_effects ? 'emit'",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(|(t,)| t).collect())
}

pub struct NotificationHandler {
    /// 必須是 `fms_owner`：041 的 EXECUTE 只給了它，而扇出要跨租戶
    /// （`notifications` 是 FORCE RLS）。
    pub pool: PgPool,
    /// 由 `handled_event_types` 查出來，不是寫死的。
    pub event_types: Vec<String>,
}

impl NotificationHandler {
    pub async fn new(pool: PgPool) -> Result<Self, sqlx::Error> {
        let event_types = handled_event_types(&pool).await?;
        Ok(Self { pool, event_types })
    }

    async fn fan_out(&self, event_id: i64) -> Result<FanOut, sqlx::Error> {
        let mut tx = crate::begin_platform_tx(&self.pool).await?;
        let row: (i32, i32, i32) = sqlx::query_as("SELECT * FROM fms.fan_out_notifications($1)")
            .bind(event_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(FanOut {
            created: row.0,
            no_template: row.1,
            unresolved: row.2,
        })
    }
}

impl crate::EventHandler for NotificationHandler {
    fn handles(&self, event_type: &str) -> bool {
        self.event_types.iter().any(|t| t == event_type)
    }

    async fn handle(&self, event: &crate::OutboxEvent) -> Result<(), String> {
        // 幂等由資料庫保證：041 的
        // `uq_notifications_event_recipient (source_event_id, recipient, channel)`
        // 加上 `ON CONFLICT DO NOTHING`。
        //
        // 這裡需要它，是因為 `fan_out` 開自己的交易並自行 commit，而 relay
        // 的狀態更新在另一個交易裡 —— 若後者失敗，通知已經寫進去了，
        // 而事件會被重新取用。relay 的檔頭已經說明 handler 必須自行幂等。
        //
        // 重放時 `created` 會是 0，而那是正確的答案：這一輪沒有建立新通知。
        let out = self.fan_out(event.id).await.map_err(|e| e.to_string())?;

        if out.created > 0 {
            tracing::info!(
                event_id = event.id,
                event_type = %event.event_type,
                created = out.created,
                "已建立通知"
            );
        }
        // 這兩個是 warn 而不是 info：目錄宣告了要通知某些人，而那些人
        // 不會收到。沒有人會去查一筆 info。
        if out.no_template > 0 {
            tracing::warn!(
                event_id = event.id,
                event_type = %event.event_type,
                "轉移宣告了 notify 但沒有對應範本 —— 沒有人會收到通知"
            );
        }
        if out.unresolved > 0 {
            tracing::warn!(
                event_id = event.id,
                event_type = %event.event_type,
                unresolved = out.unresolved,
                "notify 清單有代號解析不到任何人"
            );
        }
        Ok(())
    }
}
