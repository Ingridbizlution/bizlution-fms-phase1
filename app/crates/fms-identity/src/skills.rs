//! 技能與證照（`/skills`、`/users/{id}/skills`）。
//!
//! # 契約原本只有兩支 GET，而那兩張表是空的
//!
//! `fms.skills` 與 `fms.user_skills` 從 004 就存在，**兩張都是 0 列**，
//! 而且沒有任何端點會寫它們。照契約字面實作，會交付兩支永遠回空清單的端點。
//!
//! 所以這一輪補了最小的寫入面（`POST /skills`、`PUT /users/{id}/skills/{skillId}`）
//! 與一份平台技能目錄（migration 055）。與 users CRUD 那次的理由相同：
//! 一個讀得到但沒有人寫得進去的資源，等於沒有那個資源。
//!
//! # 到期狀態是算出來的，不是存的
//!
//! `status` 由 `expires_at` 與今天比較得出。存一個 `status` 欄位的話，
//! 它會在**沒有人更新的那一天**開始說謊 —— 而證照過期正是那種沒有人會去
//! 主動更新的事實。
//!
//! `EXPIRING` 的門檻（`expiring_within_days`，預設 30）是呼叫端該決定的條件，
//! 不是後端該寫死的數字：安全稽核想看 90 天，排班想看 7 天。
//!
//! # 到期提醒（migration 059）在這裡只露出一個欄位
//!
//! 主動提醒由 `fms.sweep_certification_expiry()` 與 `cert_watchdog` 做，
//! 不在這幾支端點裡。本模組唯一相關的是 `reminder_days_before` ——
//! **提前幾天提醒**，一個管理者該定義的條件：電氣執照要 60 天（換證要送審），
//! 急救證 7 天就夠。寫死一個 30 就是把那個判斷從他們手上拿走。
//!
//! 於是 004 的 `idx_user_skills_expiring`（部分索引，`WHERE expires_at IS NOT NULL`）
//! 到 059 才有第一個讀者 —— 本模組的查詢都是單一使用者，走不到它。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, Problem};

#[derive(Clone)]
pub struct SkillsState {
    pub pool: PgPool,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SkillDto {
    pub id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub code: String,
    pub name: String,
    pub domain: Option<String>,
    pub requires_certification: bool,
    /// 提前幾天提醒（059）。只有 `requires_certification` 的技能用得到，
    /// 但欄位對所有技能都存在 —— 一個「不適用時是什麼值」的分支不值得。
    pub reminder_days_before: i32,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserSkillDto {
    pub skill_id: Uuid,
    pub skill_code: String,
    pub skill_name: String,
    pub requires_certification: bool,
    pub level: i16,
    pub certified_at: Option<chrono::NaiveDate>,
    pub expires_at: Option<chrono::NaiveDate>,
    pub certificate_no: Option<String>,
    pub days_until_expiry: Option<i32>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct SkillQuery {
    pub domain: Option<String>,
    pub requires_certification: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UserSkillQuery {
    pub expiring_within_days: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct SkillCreate {
    pub code: Option<String>,
    pub name: Option<String>,
    pub domain: Option<String>,
    pub requires_certification: Option<bool>,
    /// 省略時走欄位預設（30 天）。
    pub reminder_days_before: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct UserSkillUpsert {
    pub level: Option<i16>,
    pub certified_at: Option<chrono::NaiveDate>,
    pub expires_at: Option<chrono::NaiveDate>,
    pub certificate_no: Option<String>,
}

/// `GET /skills`
pub async fn list(
    State(state): State<SkillsState>,
    caller: Caller,
    Query(q): Query<SkillQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // `team:read` 宣告 FACILITY，因此走「在任一範圍持有」——
    // 技能目錄沒有場域維度，場域管理員也該查得到。
    require_permission(&mut tx, "team:read", None, None).await?;

    let rows: Vec<SkillDto> = sqlx::query_as(
        "SELECT id, tenant_id, code::text AS code, name::text AS name,
                domain::text AS domain, requires_certification, reminder_days_before
           FROM fms.skills
          WHERE ($1::text IS NULL OR upper(domain) = upper($1))
            AND ($2::bool IS NULL OR requires_certification = $2::bool)
          ORDER BY tenant_id NULLS FIRST, code",
    )
    .bind(q.domain.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(q.requires_certification)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

/// `POST /skills`
pub async fn create(
    State(state): State<SkillsState>,
    caller: Caller,
    Json(body): Json<SkillCreate>,
) -> Result<(StatusCode, Json<SkillDto>), Problem> {
    let code = required(&body.code, "code")?;
    let name = required(&body.name, "name")?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "team:write", None, None).await?;

    let row: SkillDto = sqlx::query_as(
        "INSERT INTO fms.skills
           (tenant_id, code, name, domain, requires_certification, reminder_days_before)
         VALUES (fms.current_tenant_id(), $1, $2, $3, coalesce($4, false),
                 coalesce($5, 30))
         RETURNING id, tenant_id, code::text AS code, name::text AS name,
                   domain::text AS domain, requires_certification, reminder_days_before",
    )
    .bind(code)
    .bind(name)
    .bind(
        body.domain
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
    .bind(body.requires_certification)
    .bind(body.reminder_days_before)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `GET /users/{userId}/skills`
pub async fn list_for_user(
    State(state): State<SkillsState>,
    caller: Caller,
    Path(user_id): Path<Uuid>,
    Query(q): Query<UserSkillQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let within = q.expiring_within_days.unwrap_or(30);
    if within < 0 {
        return Err(Problem::validation("expiring_within_days 不能是負數"));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "team:read", None, None).await?;

    // 到期判定寫在 SQL：日期比較的基準必須是**資料庫的今天**，
    // 而不是應用伺服器的。兩者時區不同的話，跨日那幾個小時會給出不同答案。
    let rows: Vec<UserSkillDto> = sqlx::query_as(
        "SELECT us.skill_id, s.code::text AS skill_code, s.name::text AS skill_name,
                s.requires_certification, us.level, us.certified_at, us.expires_at,
                us.certificate_no::text AS certificate_no,
                CASE WHEN us.expires_at IS NULL THEN NULL
                     ELSE (us.expires_at - current_date)::int END AS days_until_expiry,
                CASE
                  WHEN us.expires_at IS NULL THEN
                    -- 需要證照卻沒有到期日：那是資料缺漏，不是「不適用」。
                    -- 把兩者混成同一個值會讓缺漏永遠沒有人發現。
                    CASE WHEN s.requires_certification THEN 'EXPIRED' ELSE 'NOT_APPLICABLE' END
                  WHEN us.expires_at < current_date THEN 'EXPIRED'
                  WHEN us.expires_at <= current_date + ($2::int || ' days')::interval
                    THEN 'EXPIRING'
                  ELSE 'VALID'
                END AS status
           FROM fms.user_skills us
           JOIN fms.skills s ON s.id = us.skill_id
          WHERE us.user_id = $1
          ORDER BY us.expires_at NULLS LAST, s.code",
    )
    .bind(user_id)
    .bind(within)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "items": rows,
        "meta": { "expiring_within_days": within }
    })))
}

/// `PUT /users/{userId}/skills/{skillId}`
pub async fn upsert_for_user(
    State(state): State<SkillsState>,
    caller: Caller,
    Path((user_id, skill_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UserSkillUpsert>,
) -> Result<Json<UserSkillDto>, Problem> {
    if let Some(l) = body.level {
        if !(1..=5).contains(&l) {
            return Err(Problem::validation("level 必須是 1 到 5"));
        }
    }
    if let (Some(c), Some(e)) = (body.certified_at, body.expires_at) {
        if e < c {
            return Err(Problem::validation("expires_at 不能早於 certified_at"));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "team:write", None, None).await?;

    // 需要證照的技能一定要有到期日。沒有它就回答不了「他現在還能不能做這件事」
    // —— 而那正是這張表存在的理由。
    let requires: Option<bool> =
        sqlx::query_scalar("SELECT requires_certification FROM fms.skills WHERE id = $1")
            .bind(skill_id)
            .fetch_optional(tx.conn())
            .await?;
    let requires = requires.ok_or_else(|| Problem::not_found("找不到這項技能"))?;
    if requires && body.expires_at.is_none() {
        return Err(Problem::validation(
            "這項技能需要執業證照，必須提供 expires_at —— \
             沒有到期日的證照紀錄回答不了「他現在還能不能做這件事」",
        ));
    }

    sqlx::query(
        "INSERT INTO fms.user_skills
           (user_id, skill_id, tenant_id, level, certified_at, expires_at, certificate_no)
         VALUES ($1, $2, fms.current_tenant_id(), coalesce($3, 1), $4, $5, $6)
         ON CONFLICT (user_id, skill_id) DO UPDATE
            SET level = excluded.level,
                certified_at = excluded.certified_at,
                expires_at = excluded.expires_at,
                certificate_no = excluded.certificate_no",
    )
    .bind(user_id)
    .bind(skill_id)
    .bind(body.level)
    .bind(body.certified_at)
    .bind(body.expires_at)
    .bind(body.certificate_no.as_deref())
    .execute(tx.conn())
    .await
    .map_err(translate)?;

    // 回讀而不是自己組：`status` 與 `days_until_expiry` 都由資料庫算，
    // 兩處各算一次遲早會漂移。
    let row: UserSkillDto = sqlx::query_as(
        "SELECT us.skill_id, s.code::text AS skill_code, s.name::text AS skill_name,
                s.requires_certification, us.level, us.certified_at, us.expires_at,
                us.certificate_no::text AS certificate_no,
                CASE WHEN us.expires_at IS NULL THEN NULL
                     ELSE (us.expires_at - current_date)::int END AS days_until_expiry,
                CASE
                  WHEN us.expires_at IS NULL THEN
                    CASE WHEN s.requires_certification THEN 'EXPIRED' ELSE 'NOT_APPLICABLE' END
                  WHEN us.expires_at < current_date THEN 'EXPIRED'
                  WHEN us.expires_at <= current_date + interval '30 days' THEN 'EXPIRING'
                  ELSE 'VALID'
                END AS status
           FROM fms.user_skills us
           JOIN fms.skills s ON s.id = us.skill_id
          WHERE us.user_id = $1 AND us.skill_id = $2",
    )
    .bind(user_id)
    .bind(skill_id)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(row))
}

fn required<'a>(v: &'a Option<String>, field: &str) -> Result<&'a str, Problem> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation(format!("{field} 為必填")))
}

fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        Some("uq_skills_code") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail("這個 code 已被使用（不分大小寫）"),
        Some("user_skills_user_id_fkey") => {
            Problem::not_found("找不到這個使用者（或不屬於這個租戶）")
        }
        Some("user_skills_level_check") => Problem::validation("level 必須是 1 到 5"),
        Some("ck_skills_reminder_days") => {
            Problem::validation("reminder_days_before 必須是 1 到 365")
        }
        _ => Problem::from(err),
    }
}
