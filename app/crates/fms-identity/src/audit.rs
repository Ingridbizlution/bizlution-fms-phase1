//! 稽核日誌查詢（`/audit-log`）。
//!
//! # 這支端點之前，稽核軌跡是寫給沒有人看的
//!
//! 029 建了 `audit_log` 與觸發器，049 把稽核擴大到 `work_orders`／`assets`，
//! 而 users／role-assignments／roles 三輪讓身分與授權也真的有了軌跡。
//! **但沒有任何端點讀得到它們。**
//!
//! 更糟的是量出來的那件事（見 migration 053）：`facility_scope` 政策用
//! `facility_id = ANY(current_facility_ids())` 比對，而租戶級事件的
//! `facility_id` 是 NULL —— `NULL = ANY(...)` 是 NULL 不是 true，
//! 於是**整個身分與授權的軌跡連租戶管理員都看不到**。
//! 實測看得到 34 列裡的 7 列。
//!
//! 046 刻意不讓場域受限的讀者看到租戶級列（`audit_trail_slice.rs` 有一格
//! 釘著那個意圖），那個判斷是對的；壞掉的是它的實作**連租戶管理員一起擋**
//! —— `app.facility_ids` 分不出 TENANT_ADMIN 與 FACILITY_ADMIN，兩者都是
//! 非 NULL 清單，只差長度（050 的檔頭就寫著這句）。053 因此只加一條分支：
//! `facility_id IS NULL AND tenant_wide_write_allowed()`。
//!
//! 也就是說這支端點若在 053 之前上線，它會**安靜地少回 79% 的列**，
//! 而且少掉的正是最該看的那一批。
//!
//! # 為什麼不回 `before_data` / `after_data`
//!
//! 契約的 `AuditEntry` 只有 `diff_keys`，這裡照做，而且理由是實的：
//! `after_data` 是整列的 jsonb 快照，`users` 那一批裡就有電話、員工編號、
//! 電子郵件。「哪些欄位被改了」足以回答稽核問題；「改成什麼」是另一個
//! 需要另一層授權的問題。要看細節就去查那張表本身。
//!
//! # 分割裁剪
//!
//! `audit_log` 依 `occurred_at` 分割。帶 `from`／`to` 的查詢只會碰到相關的
//! 分割；不帶的話會碰到全部（目前 6 個，含 DEFAULT）。沒有強制要求時間範圍
//! —— 契約說它是選填，而且「最近 50 筆」是最常見的用法，
//! 那個查詢靠 `idx_audit_log_tenant_time` 就夠。

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_tenant_scoped_permission, Caller, Cursor, PageMeta,
    Problem, SortSpec,
};

#[derive(Clone)]
pub struct AuditState {
    pub pool: PgPool,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AuditEntryDto {
    pub id: i64,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub actor_user_id: Option<Uuid>,
    /// 從 `users` 帶出來的顯示名稱。稽核列只存 id，而「誰做的」這個問題
    /// 用 uuid 回答等於沒有回答。已刪除的使用者會是 null —— 那是誠實的：
    /// 稽核列不該因為帳號消失就跟著改寫。
    pub actor_name: Option<String>,
    pub actor_type: String,
    pub action: String,
    pub entity_type: String,
    pub entity_id: Option<Uuid>,
    pub diff_keys: Option<Vec<String>>,
    pub request_id: Option<String>,
    pub ip_address: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    pub actor_user_id: Option<Uuid>,
    pub action: Option<String>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `GET /audit-log`
pub async fn list(
    State(state): State<AuditState>,
    caller: Caller,
    Query(q): Query<AuditQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    if let (Some(from), Some(to)) = (q.from, q.to) {
        if from > to {
            return Err(Problem::validation("from 不能晚於 to"));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // `audit:read` 的 `min_scope_level` 是 TENANT。
    //
    // **這一行與 `require_permission(.., None, None)` 今天等價** —— 突變測試
    // 證實了：把它換成後者，5 格測試全過。原因是 026 在
    // `v_user_effective_permissions` 就把「比指派範圍寬」的權限濾掉了，
    // 所以 `audit:read` 只可能來自 TENANT 範圍的指派。
    //
    // 仍然寫明確的那一支，理由是**耦合要看得見**：`min_scope_level` 是
    // 管理員改得動的資料。若有人把 `audit:read` 降成 FACILITY，
    // 他期待的是場域管理員看得到稽核，而這一行會讓那個設定**靜默失效**。
    // `audit_log_slice.rs` 的 `f_` 那一格因此釘住目錄裡的值 ——
    // 改了資料就會有一格測試失敗並說明程式碼也要一起改，
    // 而不是讓設定變成沒有人讀的宣告。
    require_tenant_scoped_permission(&mut tx, "audit:read").await?;

    let limit = clamp_limit(q.limit);
    // 最新的在前 —— 稽核查詢幾乎都是「剛剛發生了什麼」。
    // 破平鍵是 `id`（bigint），與 PK `(occurred_at, id)` 同序，走得到索引。
    let sort = SortSpec {
        column: "occurred_at".to_string(),
        desc: true,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.as_timestamp()?), Some(c.bigint_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<AuditEntryDto> = sqlx::query_as(
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
            AND ($7::timestamptz IS NULL
                 OR (a.occurred_at, a.id) < ($7::timestamptz, $8::bigint))
          ORDER BY a.occurred_at DESC, a.id DESC
          LIMIT $9",
    )
    .bind(
        q.entity_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
    .bind(q.entity_id)
    .bind(q.actor_user_id)
    .bind(q.action.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(q.from)
    .bind(q.to)
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1) // 多取一列判斷還有沒有下一頁
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| {
        (r.occurred_at.to_rfc3339(), r.id)
    });
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}
