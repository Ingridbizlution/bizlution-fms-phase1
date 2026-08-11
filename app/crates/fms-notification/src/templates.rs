//! 通知範本的維護（`/notification-templates`）。
//!
//! # 為什麼需要它
//!
//! 041 讓範本有了讀取點，但只有 migration 能改它們 —— 而 041 自己就報出
//! 十條有 `notify` 卻沒有對應範本的轉移。那十份文案是**內容工作**，
//! 把它們寫進 migration 等於讓「改一句通知的措辭」變成一次部署。
//!
//! # 平台範本改不了，只能覆寫
//!
//! 007 的 RLS 已經把模型定好了（`tenant_read` 讀得到 `tenant_id IS NULL`，
//! `tenant_write` 只允許 `tenant_id = current_tenant_id()`）。009 種的 13 個
//! 範本全部是平台的，因此租戶客製的方式是**建一個同 `(code, channel, locale)`
//! 的租戶版本**，而 042 讓那個版本確定地勝出。
//!
//! 這一層要做的是把 RLS 的沉默（影響 0 列）翻譯成契約形狀的錯誤：
//! 對平台範本送 PATCH 應該得到一個說得出「請改成建立覆寫」的回應，
//! 而不是一個看起來像找不到的 404。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, require_permission, require_tenant_scoped_permission, Caller, FieldError,
    Problem,
};

use crate::handlers::NotificationState;

const CHANNELS: [&str; 6] = ["EMAIL", "SMS", "PUSH", "WEBHOOK", "IN_APP", "LINE"];

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TemplateDto {
    pub id: Uuid,
    /// `true` 代表平台提供的範本 —— 讀得到但改不了，客製要建覆寫版本。
    pub is_platform: bool,
    pub code: String,
    pub channel: String,
    pub locale: String,
    pub subject_template: Option<String>,
    pub body_template: String,
    pub is_active: bool,
    /// 這個範本用到的 `{{變數}}`。
    ///
    /// 打錯一個變數的後果是收件人看到 `{{assignee}}` 那串字 ——
    /// `render_template` 刻意原樣留下找不到的變數（041 檔頭說明了理由）。
    /// 因此把它列出來，讓客戶端能與契約文件列的可用變數對照。
    pub placeholders: Vec<String>,
    /// 這個 `(code, channel, locale)` 是否已被租戶覆寫。
    ///
    /// 平台範本被覆寫時仍然會出現在清單裡（它還在），但**不會生效** ——
    /// 少了這個欄位，UI 沒辦法解釋「我改了那一列卻沒有作用」。
    pub is_overridden: bool,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub code: Option<String>,
    pub channel: Option<String>,
    #[serde(default)]
    pub include_inactive: bool,
}

#[derive(Debug, Deserialize)]
pub struct TemplateCreate {
    pub code: String,
    pub channel: String,
    pub locale: Option<String>,
    pub subject_template: Option<String>,
    pub body_template: String,
    #[serde(default = "default_true")]
    pub is_active: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct TemplateUpdate {
    pub subject_template: Option<String>,
    pub body_template: Option<String>,
    pub is_active: Option<bool>,
}

fn check_channel(value: &str) -> Result<(), Problem> {
    if CHANNELS.contains(&value) {
        return Ok(());
    }
    Err(
        Problem::validation(format!("`channel` 必須是 {} 之一", CHANNELS.join("／"))).with_errors(
            vec![FieldError {
                pointer: "/channel".to_string(),
                code: "ENUM".to_string(),
                message: format!("`{value}` 不是支援的頻道"),
            }],
        ),
    )
}

fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        Some("uq_notification_templates") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail("這個 (code, channel, locale) 你已經有一個覆寫版本了 —— 請改那一筆"),
        _ => Problem::from(err),
    }
}

const SELECT_COLUMNS: &str = "nt.id,
        nt.tenant_id IS NULL AS is_platform,
        nt.code, nt.channel, nt.locale,
        nt.subject_template, nt.body_template, nt.is_active,
        fms.template_placeholders(coalesce(nt.subject_template, '') || ' ' || nt.body_template)
          AS placeholders,
        (nt.tenant_id IS NULL AND EXISTS (
           SELECT 1 FROM fms.notification_templates o
            WHERE o.tenant_id = fms.current_tenant_id()
              AND lower(o.code) = lower(nt.code)
              AND o.channel = nt.channel
              AND o.locale = nt.locale
              AND o.is_active)) AS is_overridden";

/// `GET /notification-templates`
///
/// 平台範本與租戶自己的覆寫都會回。不分頁：範本的數量級是「事件種類 ×
/// 頻道」，而客戶端需要全部才能顯示「哪些事件還沒有範本」——
/// 那正是這支端點最重要的用途（041 報出十條轉移沒有範本）。
pub async fn list(
    State(state): State<NotificationState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "notification_template:read", None, None).await?;

    let rows: Vec<TemplateDto> = sqlx::query_as(&format!(
        "SELECT {SELECT_COLUMNS}
           FROM fms.notification_templates nt
          WHERE ($1::text IS NULL OR lower(nt.code) = lower($1))
            AND ($2::text IS NULL OR nt.channel = $2)
            AND ($3::bool OR nt.is_active)
          ORDER BY nt.code, nt.channel, nt.locale, (nt.tenant_id IS NULL)"
    ))
    .bind(q.code.as_deref())
    .bind(q.channel.as_deref())
    .bind(q.include_inactive)
    .fetch_all(tx.conn())
    .await
    .map_err(translate)?;

    // 哪些「宣告了要通知」的轉移還沒有範本 —— 041 把它計入 `no_template`
    // 並記 warn，但那要等事件真的發生。這裡讓它**事先**看得見。
    let missing: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT t.action::text,
                coalesce(t.side_effects ->> 'template', '(未指定)')
           FROM fms.work_order_transitions_allowed t
          WHERE t.is_active
            AND t.side_effects ? 'notify'
            AND NOT EXISTS (
                  SELECT 1 FROM fms.notification_templates nt
                   WHERE lower(nt.code) = lower(t.side_effects ->> 'template')
                     AND nt.is_active)
          ORDER BY 1",
    )
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": rows,
        "meta": {
            // 每一項都是「有人宣告要被通知，但沒有文案可以送」。
            "transitions_without_template": missing
                .into_iter()
                .map(|(action, template)| serde_json::json!({
                    "action": action, "template": template
                }))
                .collect::<Vec<_>>(),
        },
    })))
}

/// `POST /notification-templates`
///
/// 建立**租戶**範本。`tenant_id` 由伺服端填入當前租戶，客戶端無法指定 ——
/// 平台範本只能由平台情境建立（migration）。
pub async fn create(
    State(state): State<NotificationState>,
    caller: Caller,
    Json(body): Json<TemplateCreate>,
) -> Result<(StatusCode, Json<TemplateDto>), Problem> {
    check_channel(&body.channel)?;
    if body.body_template.trim().is_empty() {
        return Err(
            Problem::validation("`body_template` 不得為空").with_errors(vec![FieldError {
                pointer: "/body_template".to_string(),
                code: "REQUIRED".to_string(),
                message: "通知的內容不能是空字串".to_string(),
            }]),
        );
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // TENANT 範圍：一句措辭套用到整個租戶收到的每一封通知。
    //
    // **真正在把關的是宣告**：042 讓 `notification_template:write` 的
    // `min_scope_level` 是 TENANT，而 026 的收斂讓 FACILITY 範圍的授權
    // 展不開它。因此這裡改成 `require_permission(.., None, None)` 行為不變
    // —— 突變測試證實了那一點。
    //
    // 仍然用這個呼叫，是因為它把意圖寫在讀得到的地方，並且在日後有人把
    // 宣告改成 FACILITY 時仍然擋住。與 037（`sla_policy:write` 宣告
    // FACILITY，那裡這個呼叫是**唯一**的把關）不同 —— 那裡它不可省。
    require_tenant_scoped_permission(&mut tx, "notification_template:write").await?;

    let row: TemplateDto = sqlx::query_as(&format!(
        "WITH ins AS (
           INSERT INTO fms.notification_templates
             (tenant_id, code, channel, locale, subject_template, body_template, is_active)
           VALUES (fms.current_tenant_id(), $1, $2, coalesce($3, 'zh-TW'), $4, $5, $6)
           RETURNING *
         )
         SELECT {SELECT_COLUMNS} FROM ins nt"
    ))
    .bind(&body.code)
    .bind(&body.channel)
    .bind(body.locale.as_deref())
    .bind(body.subject_template.as_deref())
    .bind(&body.body_template)
    .bind(body.is_active)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `PATCH /notification-templates/{templateId}`
///
/// 只能改租戶自己的範本。**對平台範本回 409 而不是 404** ——
/// 那一筆確實存在、也讀得到，只是改不了；回 404 會讓人以為 id 錯了。
/// 錯誤訊息說出正確的做法（建立覆寫版本）。
///
/// `code`／`channel`／`locale` 不可變更：它們合起來是範本的身分，
/// 而改身分等於「刪掉一個、建立另一個」——那應該是兩個明確的動作。
pub async fn update(
    State(state): State<NotificationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<TemplateUpdate>,
) -> Result<Json<TemplateDto>, Problem> {
    if let Some(b) = &body.body_template {
        if b.trim().is_empty() {
            return Err(Problem::validation("`body_template` 不得為空"));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "notification_template:write").await?;

    // 先問它是不是平台的。RLS 的 `tenant_write` 會讓 UPDATE 影響 0 列，
    // 而 0 列分不出「不存在」與「不是你的」—— 對平台範本那個差別很重要。
    let owner: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT tenant_id FROM fms.notification_templates WHERE id = $1")
            .bind(id)
            .fetch_optional(tx.conn())
            .await?;
    match owner {
        None => return Err(Problem::not_found("notification template not found")),
        Some(None) => {
            return Err(Problem::new(fms_shared::ProblemCode::Conflict).with_detail(
                "這是平台提供的範本，不能直接修改。\
                 請以相同的 (code, channel, locale) 建立一個租戶版本 —— 它會覆寫平台版",
            ))
        }
        Some(Some(_)) => {}
    }

    let row: TemplateDto = sqlx::query_as(&format!(
        "WITH upd AS (
           UPDATE fms.notification_templates SET
             subject_template = coalesce($2, subject_template),
             body_template    = coalesce($3, body_template),
             is_active        = coalesce($4, is_active)
           WHERE id = $1
           RETURNING *
         )
         SELECT {SELECT_COLUMNS} FROM upd nt"
    ))
    .bind(id)
    .bind(body.subject_template.as_deref())
    .bind(body.body_template.as_deref())
    .bind(body.is_active)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok(Json(row))
}

/// `DELETE /notification-templates/{templateId}`
///
/// 刪掉租戶的覆寫版本 → 平台版重新生效。真的刪除：`notifications` 存的是
/// `template_code` 而不是 id，因此沒有任何東西參照這一列。
pub async fn delete(
    State(state): State<NotificationState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "notification_template:write").await?;

    let owner: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT tenant_id FROM fms.notification_templates WHERE id = $1")
            .bind(id)
            .fetch_optional(tx.conn())
            .await?;
    match owner {
        None => return Err(Problem::not_found("notification template not found")),
        Some(None) => {
            return Err(Problem::new(fms_shared::ProblemCode::Conflict)
                .with_detail("這是平台提供的範本，不能刪除"))
        }
        Some(Some(_)) => {}
    }

    sqlx::query("DELETE FROM fms.notification_templates WHERE id = $1")
        .bind(id)
        .execute(tx.conn())
        .await?;
    tx.commit().await?;

    Ok(StatusCode::NO_CONTENT)
}
