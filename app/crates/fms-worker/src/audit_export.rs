//! 稽核匯出的產檔（`audit_export.requested` 的 handler）。
//!
//! # 這個檔案唯一真正困難的地方
//!
//! relay 跑在**平台情境**下 —— 它必須跨租戶取用 `event_outbox`。
//! 若就這樣執行匯出查詢，`is_platform_context()` 為真，`audit_log` 的
//! 兩條政策（`tenant_isolation` 與 `facility_scope`）第一個 OR 分支就成立，
//! 產出的檔案會是**整個資料庫的稽核紀錄**。
//!
//! 一次匯出就繞過 053 剛修好的東西，而且是往更壞的方向：
//! 053 修的是「該看到的看不到」，這裡的失效是「不該看到的全看到」。
//!
//! 因此 `produce` **一定**先把情境切成 `requested_by` 的：
//!
//!   1. `set_config('app.is_platform', 'off', true)` —— 交易層級，關掉平台旁路
//!   2. `fms.set_context(tenant, requested_by, false)`
//!   3. `set_config('app.facility_ids', <他能存取的場域>, true)`
//!
//! 第 3 步不能省。`current_facility_ids()` 是 NULL 時 `facility_in_scope()`
//! 一律放行 —— 那會讓場域收斂**看起來**有做，實際沒做。
//! `audit_export_slice.rs` 有一格專門盯這個：場域受限的發起者匯出的檔案裡
//! 不能出現別的場域的列。
//!
//! # 幂等
//!
//! relay 保證至少一次投遞，因此重放會發生。`produce` 只處理 `PENDING`
//! 與 `RUNNING` 的作業；`COMPLETED` 直接回成功而不重做（檔案已經在了）。
//! 物件鍵用 export id 而不是時間戳，所以重做也會覆寫同一個物件而不是留下垃圾。

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// 與發送端共用同一個常數，兩邊不可能寫成不同的字串。
pub const EVENT_TYPE: &str = fms_identity_event_type::AUDIT_EXPORT_REQUESTED;

/// 事件型別的單一來源。
///
/// 刻意不從 `fms-identity` import：那會讓 worker 依賴一個 HTTP 層的 crate。
/// 兩邊各自宣告、由 `audit_export_slice.rs` 的一格斷言它們相等 ——
/// 「宣告了但沒有人比對」正是這個專案反覆出現的缺陷類型。
pub mod fms_identity_event_type {
    pub const AUDIT_EXPORT_REQUESTED: &str = "audit_export.requested";
}

/// CSV 的欄位。與 `GET /audit-log` 的投影一致 —— 匯出是同一份讀取的批次形式，
/// 不是更寬的投影。**不含 `before_data`／`after_data`**：那是整列快照
///（`users` 那批裡有電話與員工編號），要看它是另一個需要另一層授權的決定。
const HEADER: &str =
    "id,occurred_at,actor_user_id,actor_name,actor_type,action,entity_type,entity_id,diff_keys,request_id,ip_address";

pub struct AuditExportHandler {
    pool: PgPool,
    storage: fms_shared::Storage,
}

impl AuditExportHandler {
    pub fn new(pool: PgPool, storage: fms_shared::Storage) -> Self {
        Self { pool, storage }
    }

    /// 產出一份匯出。回傳寫入的列數。
    ///
    /// **作業表的三次存取都要自己開平台情境交易。**
    /// `begin_platform_tx` 把 `app.is_platform` 設在**交易層級**，
    /// 而 relay 的那個交易在另一條連線上 —— handler 用自己的連線查
    /// `audit_exports` 時沒有那個情境，`tenant_isolation` 會直接擋掉。
    ///
    /// 第一版就是這樣寫的，症狀特別安靜：UPDATE 影響 0 列，
    /// `fetch_optional` 回 None，`produce` 走「已完成或作業不見了」那條路
    /// **回 Ok(0)**。作業永遠停在 PENDING，而 relay 認為它成功了。
    /// 端到端測試抓到它（`object_key` 是 NULL），單看程式碼看不出來。
    /// `notifier.rs` 早就是這樣做的。
    pub async fn produce(&self, export_id: Uuid) -> Result<i64, String> {
        let mut tx = crate::begin_platform_tx(&self.pool)
            .await
            .map_err(|e| format!("開啟平台情境交易失敗：{e}"))?;
        let job = sqlx::query(
            "UPDATE fms.audit_exports
                SET status = 'RUNNING', started_at = coalesce(started_at, clock_timestamp())
              WHERE id = $1 AND status IN ('PENDING','RUNNING')
              RETURNING tenant_id, requested_by, filters",
        )
        .bind(export_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("讀取匯出作業失敗：{e}"))?;

        let Some(job) = job else {
            // 已經 COMPLETED（重放）或作業不見了。前者是正常的重放，
            // 後者代表有人刪了它 —— 兩種都不該讓 relay 無限重試。
            let _ = tx.rollback().await;
            return Ok(0);
        };
        let tenant_id: Uuid = job.get("tenant_id");
        let requested_by: Uuid = job.get("requested_by");
        let filters: serde_json::Value = job.get("filters");
        // 先 commit「已認領」的狀態，再開始產檔：產檔可能很久，
        // 而把作業表鎖在一個長交易裡會擋住輪詢的讀取。
        tx.commit()
            .await
            .map_err(|e| format!("提交 RUNNING 狀態失敗：{e}"))?;

        match self
            .write_csv(export_id, tenant_id, requested_by, &filters)
            .await
        {
            Ok(n) => Ok(n),
            Err(e) => {
                // 失敗要落地，否則客戶端輪詢到的永遠是 RUNNING ——
                // 「還在跑」與「早就死了」看起來一樣。
                if let Ok(mut tx) = crate::begin_platform_tx(&self.pool).await {
                    let _ = sqlx::query(
                        "UPDATE fms.audit_exports
                            SET status = 'FAILED', error = $2, completed_at = clock_timestamp()
                          WHERE id = $1",
                    )
                    .bind(export_id)
                    .bind(&e)
                    .execute(&mut *tx)
                    .await;
                    let _ = tx.commit().await;
                }
                Err(e)
            }
        }
    }

    async fn write_csv(
        &self,
        export_id: Uuid,
        tenant_id: Uuid,
        requested_by: Uuid,
        filters: &serde_json::Value,
    ) -> Result<i64, String> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("開啟交易失敗：{e}"))?;

        // --- 情境切換。見模組檔頭 —— 這三步是這個檔案的重點 ---------------
        sqlx::query("SELECT set_config('app.is_platform', 'off', true)")
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("關閉平台情境失敗：{e}"))?;
        sqlx::query("SELECT fms.set_context($1, $2, false)")
            .bind(tenant_id)
            .bind(requested_by)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("注入發起者情境失敗：{e}"))?;
        // 與 `begin_tenant_tx` 的 `set_facility_scope` 同一份邏輯：
        // 空清單寫成全零 uuid 哨兵，而不是空字串 —— 空字串會讓
        // `current_facility_ids()` 變成 NULL，而那等於「不限制」。
        let facilities: Vec<Uuid> =
            sqlx::query_scalar("SELECT facility_id FROM fms.user_accessible_facilities($1)")
                .bind(requested_by)
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| format!("取得可存取場域失敗：{e}"))?;
        let ids = if facilities.is_empty() {
            "00000000-0000-0000-0000-000000000000".to_string()
        } else {
            facilities
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        sqlx::query("SELECT set_config('app.facility_ids', $1, true)")
            .bind(&ids)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("設定場域範圍失敗：{e}"))?;

        // --- 查詢。述詞與 GET /audit-log 完全相同 --------------------------
        let f = |k: &str| filters.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let fu = |k: &str| {
            filters
                .get(k)
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
        };
        let ft = |k: &str| {
            filters
                .get(k)
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
        };

        let rows = sqlx::query(
            "SELECT a.id, a.occurred_at, a.actor_user_id, u.display_name AS actor_name,
                    a.actor_type, a.action::text AS action, a.entity_type::text AS entity_type,
                    a.entity_id, a.diff_keys, a.request_id::text AS request_id,
                    a.ip_address::text AS ip_address
               FROM fms.audit_log a
               LEFT JOIN fms.users u ON u.id = a.actor_user_id
              WHERE ($1::text IS NULL OR a.entity_type = $1::text)
                AND ($2::uuid IS NULL OR a.entity_id = $2::uuid)
                AND ($3::uuid IS NULL OR a.actor_user_id = $3::uuid)
                AND ($4::text IS NULL OR a.action = $4::text)
                AND ($5::timestamptz IS NULL OR a.occurred_at >= $5::timestamptz)
                AND ($6::timestamptz IS NULL OR a.occurred_at <= $6::timestamptz)
              ORDER BY a.occurred_at, a.id",
        )
        .bind(f("entity_type"))
        .bind(fu("entity_id"))
        .bind(fu("actor_user_id"))
        .bind(f("action"))
        .bind(ft("from"))
        .bind(ft("to"))
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| format!("查詢稽核紀錄失敗：{e}"))?;

        // 唯讀交易，rollback 即可 —— 不留任何狀態。
        let _ = tx.rollback().await;

        let mut csv = String::with_capacity(HEADER.len() + rows.len() * 160);
        csv.push_str(HEADER);
        csv.push('\n');
        for r in &rows {
            let diff: Option<Vec<String>> = r.try_get("diff_keys").unwrap_or(None);
            let line = [
                r.get::<i64, _>("id").to_string(),
                r.get::<chrono::DateTime<chrono::Utc>, _>("occurred_at")
                    .to_rfc3339(),
                opt_uuid(r.try_get("actor_user_id").unwrap_or(None)),
                r.try_get::<Option<String>, _>("actor_name")
                    .unwrap_or(None)
                    .unwrap_or_default(),
                r.get::<String, _>("actor_type"),
                r.get::<String, _>("action"),
                r.get::<String, _>("entity_type"),
                opt_uuid(r.try_get("entity_id").unwrap_or(None)),
                diff.unwrap_or_default().join(" "),
                r.try_get::<Option<String>, _>("request_id")
                    .unwrap_or(None)
                    .unwrap_or_default(),
                r.try_get::<Option<String>, _>("ip_address")
                    .unwrap_or(None)
                    .unwrap_or_default(),
            ]
            .iter()
            .map(|s| quote(s))
            .collect::<Vec<_>>()
            .join(",");
            csv.push_str(&line);
            csv.push('\n');
        }

        // 物件鍵用 export id：重放會覆寫同一個物件，不會留下垃圾。
        let key = format!("audit-exports/{tenant_id}/{export_id}.csv");
        self.storage
            .put(&key, csv.into_bytes(), Some("text/csv; charset=utf-8"))
            .await
            .map_err(|e| format!("上傳匯出檔失敗：{e}"))?;

        let n = rows.len() as i64;
        let mut done = crate::begin_platform_tx(&self.pool)
            .await
            .map_err(|e| format!("開啟平台情境交易失敗：{e}"))?;
        sqlx::query(
            "UPDATE fms.audit_exports
                SET status = 'COMPLETED', object_key = $2, row_count = $3,
                    error = NULL, completed_at = clock_timestamp()
              WHERE id = $1",
        )
        .bind(export_id)
        .bind(&key)
        .bind(n)
        .execute(&mut *done)
        .await
        .map_err(|e| format!("回寫匯出結果失敗：{e}"))?;
        done.commit()
            .await
            .map_err(|e| format!("提交匯出結果失敗：{e}"))?;

        Ok(n)
    }
}

fn opt_uuid(v: Option<Uuid>) -> String {
    v.map(|u| u.to_string()).unwrap_or_default()
}

/// RFC 4180：欄位含逗號、引號或換行時加引號，內部的引號成對。
///
/// **一律加引號**而不是只在需要時加：稽核的 `action` 與 `entity_type` 是
/// 資料庫來的字串，判斷「這一個需不需要」比一律加更容易寫錯，
/// 而多餘的引號對所有 CSV 讀取器都是合法的。
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

impl crate::EventHandler for AuditExportHandler {
    fn handles(&self, event_type: &str) -> bool {
        event_type == EVENT_TYPE
    }

    async fn handle(&self, event: &crate::OutboxEvent) -> Result<(), String> {
        let id = event
            .payload
            .get("export_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| format!("事件 {} 的 payload 沒有可用的 export_id", event.id))?;

        let n = self.produce(id).await?;
        tracing::info!(event_id = event.id, export_id = %id, rows = n, "稽核匯出完成");
        Ok(())
    }
}
