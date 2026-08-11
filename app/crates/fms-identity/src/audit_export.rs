//! 稽核匯出（`/audit-log:export`、`/audit-log/exports/{id}`）。
//!
//! # 為什麼是兩支端點
//!
//! 契約原本只列了 `POST /audit-log:export`。**只有那一支的話，發起者拿不回
//! 檔案** —— 非同步作業必須有一個可輪詢的資源，否則 202 之後就沒有下文。
//! 這與 `GET /users/{id}/role-assignments` 那次是同一類不一致：
//! 一個動作做得到、結果看不到。
//!
//! # 佇列複用 event_outbox，狀態放 audit_exports
//!
//! `event_outbox` 已經有重試、退避與 `EventHandler` 分派，不新造第二套。
//! 但 outbox 列成功後會被標為 PUBLISHED 並最終清掉，不是可以拿 id 回查的
//! 東西；產出的物件鍵與列數也沒地方放。所以：
//! **outbox 觸發，`audit_exports` 記狀態與結果**（migration 054）。
//!
//! # 這裡不做產檔
//!
//! 產檔在 `fms_worker::audit_export`。那裡有一個非做不可的正確性要求：
//! worker 跑在平台情境下，必須以 `requested_by` 的身分重新注入情境再查，
//! 否則一次匯出就繞過了 053 剛修好的 `audit_log.facility_scope`。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_tenant_scoped_permission, Caller, Problem, Storage};

/// outbox 的事件型別。**這個常數同時給 handler 的 `handles()` 用**，
/// 因此發送端與接收端不可能寫成不同的字串。
pub const EVENT_TYPE: &str = "audit_export.requested";

#[derive(Clone)]
pub struct AuditExportState {
    pub pool: PgPool,
    pub storage: Storage,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct ExportFilters {
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    pub actor_user_id: Option<Uuid>,
    pub action: Option<String>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}

impl ExportFilters {
    fn is_empty(&self) -> bool {
        self.entity_type.is_none()
            && self.entity_id.is_none()
            && self.actor_user_id.is_none()
            && self.action.is_none()
            && self.from.is_none()
            && self.to.is_none()
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ExportRow {
    id: Uuid,
    status: String,
    filters: serde_json::Value,
    row_count: Option<i64>,
    object_key: Option<String>,
    error: Option<String>,
    requested_by: Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ExportDto {
    pub id: Uuid,
    pub status: String,
    pub filters: serde_json::Value,
    pub row_count: Option<i64>,
    pub download_url: Option<String>,
    pub error: Option<String>,
    pub requested_by: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// `POST /audit-log:export`
pub async fn create(
    State(state): State<AuditExportState>,
    caller: Caller,
    body: Option<Json<ExportFilters>>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let filters = body.map(|Json(f)| f).unwrap_or_default();
    if let (Some(from), Some(to)) = (filters.from, filters.to) {
        if from > to {
            return Err(Problem::validation("from 不能晚於 to"));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "audit:export").await?;

    let filters_json = serde_json::to_value(&filters)
        .map_err(|e| Problem::internal(std::io::Error::other(e.to_string())))?;

    let row: ExportRow = sqlx::query_as(
        "INSERT INTO fms.audit_exports (tenant_id, requested_by, filters)
         VALUES (fms.current_tenant_id(), $1, $2)
         RETURNING id, status, filters, row_count, object_key, error,
                   requested_by, created_at, completed_at",
    )
    .bind(caller.user_id)
    .bind(&filters_json)
    .fetch_one(tx.conn())
    .await?;

    // 觸發：同一個交易裡寫 outbox，因此「作業建立了但沒有人去做」不可能發生。
    // 那正是 outbox 模式要解決的問題。
    sqlx::query(
        "INSERT INTO fms.event_outbox
           (tenant_id, event_type, aggregate_type, aggregate_id, payload)
         VALUES (fms.current_tenant_id(), $1, 'AUDIT_EXPORT', $2,
                 jsonb_build_object('export_id', $2::text))",
    )
    .bind(EVENT_TYPE)
    .bind(row.id)
    .execute(tx.conn())
    .await?;
    tx.commit().await?;

    // 沒有任何條件代表匯出全部。那是合法的，但值得說出來 ——
    // 一個誤觸的空請求會產出整個租戶的稽核史，而回應看起來跟正常的一樣。
    let mut out = serde_json::to_value(dto(row, None))
        .map_err(|e| Problem::internal(std::io::Error::other(e.to_string())))?;
    if filters.is_empty() {
        out["meta"] = serde_json::json!({
            "warning": "沒有帶任何過濾條件 —— 這會匯出這個租戶可見的全部稽核紀錄"
        });
    }
    Ok((StatusCode::ACCEPTED, Json(out)))
}

/// `GET /audit-log/exports/{id}`
pub async fn get(
    State(state): State<AuditExportState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<ExportDto>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "audit:export").await?;

    let row: Option<ExportRow> = sqlx::query_as(
        "SELECT id, status, filters, row_count, object_key, error,
                requested_by, created_at, completed_at
           FROM fms.audit_exports WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(tx.conn())
    .await?;
    tx.commit().await?;

    let row = row.ok_or_else(|| Problem::not_found("找不到這個匯出作業"))?;

    // 預簽網址只在真的有檔案時才產生。054 的 CHECK 保證 COMPLETED 一定有
    // object_key，這裡的 `if let` 因此不是防禦性判斷，而是型別上的必要。
    let url = match (row.status.as_str(), row.object_key.as_deref()) {
        ("COMPLETED", Some(key)) => Some(
            state
                .storage
                .presign_get(key, &format!("audit-export-{}.csv", row.id))
                .await?,
        ),
        _ => None,
    };

    Ok(Json(dto(row, url)))
}

fn dto(row: ExportRow, download_url: Option<String>) -> ExportDto {
    ExportDto {
        id: row.id,
        status: row.status,
        filters: row.filters,
        row_count: row.row_count,
        download_url,
        error: row.error,
        requested_by: row.requested_by,
        created_at: row.created_at,
        completed_at: row.completed_at,
    }
}
