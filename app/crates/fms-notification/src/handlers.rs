//! `GET /notifications`、`POST /notifications/{id}/read`

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, clamp_limit, Caller, Problem};

#[derive(Clone)]
pub struct NotificationState {
    pub pool: PgPool,
}

#[derive(Debug, Deserialize)]
pub struct InboxQuery {
    #[serde(default)]
    pub unread_only: bool,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct NotificationDto {
    pub id: Uuid,
    pub subject: Option<String>,
    pub body: String,
    pub priority: String,
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    pub template_code: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub read_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// `GET /notifications`
///
/// 只回**呼叫者自己的** `IN_APP` 通知，最新的在前。
///
/// `recipient_user_id = 呼叫者` 這個條件不是方便性的過濾，是**授權**：
/// `fms.notifications` 的 RLS 只隔離租戶，沒有按收件人過濾。少了它，
/// 任何登入者都能讀到同租戶每一個人的通知內容 —— 而那些內容包含工單標題、
/// 負責人姓名與地點。
///
/// 沒有游標分頁：收件匣是從最新往下讀的，而 `limit` 已經夠用。
/// 與 `/sla-policies`／`/holiday-calendars` 一致 —— 需要時再補，
/// 而不是先做一個沒有人用的游標。
pub async fn list(
    State(state): State<NotificationState>,
    caller: Caller,
    Query(q): Query<InboxQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let user_id = caller.user_id;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let limit = clamp_limit(q.limit);

    let rows: Vec<NotificationDto> = sqlx::query_as(
        "SELECT id, subject, body, priority, entity_type, entity_id,
                template_code, created_at, read_at
           FROM fms.notifications
          WHERE recipient_user_id = $1
            AND channel = 'IN_APP'
            AND (NOT $2::bool OR read_at IS NULL)
          ORDER BY created_at DESC, id
          LIMIT $3",
    )
    .bind(user_id)
    .bind(q.unread_only)
    .bind(limit)
    .fetch_all(tx.conn())
    .await?;

    let unread: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.notifications
          WHERE recipient_user_id = $1 AND channel = 'IN_APP' AND read_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": rows,
        // 未讀數是收件匣最常被問的一件事（紅點），而它與 `limit` 無關 ——
        // 從 `data.len()` 推是錯的。
        "meta": { "unread_count": unread },
    })))
}

/// `POST /notifications/{notificationId}/read`
///
/// 幂等：已讀的再標一次仍回 200，`read_at` 不變（第一次讀的時刻才是事實）。
pub async fn mark_read(
    State(state): State<NotificationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let user_id = caller.user_id;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // `recipient_user_id = $2` 同時是授權與定位條件。別人的通知回 404
    // 而不是 403 —— 回 403 會確認「那個 id 存在」，而收件匣的 id
    // 對不是收件人的人來說不該是可觀測的。
    let affected = sqlx::query(
        "UPDATE fms.notifications
            SET read_at = coalesce(read_at, clock_timestamp()),
                status = 'READ'
          WHERE id = $1 AND recipient_user_id = $2",
    )
    .bind(id)
    .bind(user_id)
    .execute(tx.conn())
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(Problem::not_found("notification not found"));
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
