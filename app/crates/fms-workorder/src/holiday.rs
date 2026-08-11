//! 假日與補班日的維護（`/holiday-calendars`）。
//!
//! 038 讓行事曆決定 SLA 期限，但只有 migration 能寫它 —— 一張只有工程師
//! 能填的假日表，等於把每年的行事曆變成一次部署。
//!
//! 範圍規則與 `sla_policy` 完全相同（`facility_id IS NULL` 需要 TENANT
//! 範圍、搬移場域要兩端都有權限），因為問題完全相同：租戶通用的那一類
//! 影響每一個場域的每一張工單。
//!
//! 與 SLA 政策的一個差別：**這裡有 `DELETE`。** 政策不能刪只能停用，
//! 因為已開立的工單快照了它的 id；而行事曆沒有任何東西參照它 ——
//! 期限在開單時就算成絕對時刻了，刪掉一筆假日不會改變任何已開立的工單。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, require_permission, require_tenant_scoped_permission, Caller, FieldError,
    Problem, TenantTx,
};

#[derive(Clone)]
pub struct HolidayState {
    pub pool: PgPool,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct HolidayDto {
    pub id: Uuid,
    pub facility_id: Option<Uuid>,
    pub holiday_date: chrono::NaiveDate,
    pub name: String,
    pub is_working_day: bool,
    pub windows: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
}

#[derive(Debug, Deserialize)]
pub struct HolidayCreate {
    pub facility_id: Option<Uuid>,
    pub holiday_date: chrono::NaiveDate,
    pub name: String,
    #[serde(default)]
    pub is_working_day: bool,
    pub windows: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct HolidayUpdate {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub facility_id: Option<Option<Uuid>>,
    pub holiday_date: Option<chrono::NaiveDate>,
    pub is_working_day: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    pub windows: Option<Option<serde_json::Value>>,
}

fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

/// 把資料庫的約束違反轉成契約形狀的錯誤（與 `sla_policy::translate` 同一個
/// 理由：`23514` 的通用映射會落到 500，而管理者打錯一個時刻不該是伺服器錯誤）。
fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        Some("holiday_calendars_windows_check") => Problem::validation(
            "`windows` 必須是 [[\"09:00\",\"18:00\"], ...] 的形式，結束時刻要晚於開始",
        )
        .with_errors(vec![FieldError {
            pointer: "/windows".to_string(),
            code: "SHAPE".to_string(),
            message: "時刻要 HH:MM（結束可為 24:00），且結束嚴格晚於開始".to_string(),
        }]),
        Some("uq_holiday_calendars_date") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail(
                "該場域的那一天已經有一筆行事曆。同一天只能有一筆，\
                 否則「這一天營不營業」會有兩個答案 —— 請改既有那一筆",
            ),
        _ => Problem::from(err),
    }
}

/// 補班日不給時段是**沉默失效**的設定。
///
/// 038 的 `business_windows` 對「補班日但 `windows` 為 NULL」會沿用那個星期
/// 在 `operating_hours` 裡的時段 —— 而補班日通常落在平常不營業的星期
/// （台灣的補班日是週六，多數辦公場域只排週一至五），於是那一天可用
/// 0 分鐘，整筆設定沒有作用而且沒有任何提示。
///
/// 不能在資料庫層用 CHECK 擋：那要跨表看場域的 `operating_hours`，
/// 而 CHECK 不能含子查詢。因此在這裡擋，並把理由說出來。
async fn check_make_up_day(
    tx: &mut TenantTx,
    facility_id: Option<Uuid>,
    date: chrono::NaiveDate,
    is_working_day: bool,
    windows: Option<&serde_json::Value>,
) -> Result<(), Problem> {
    if !is_working_day || windows.is_some() {
        return Ok(());
    }
    // 場域通用（facility_id 為 NULL）的補班日要對每個場域都成立，無法在
    // 這裡逐一檢查；那種設定本來就該明確給時段。
    let usable: Option<bool> = match facility_id {
        Some(fid) => sqlx::query_scalar(
            "SELECT jsonb_array_length(
                      coalesce(f.operating_hours ->
                        (ARRAY['mon','tue','wed','thu','fri','sat','sun'])[
                          extract(isodow FROM $2::date)::int], '[]'::jsonb)) > 0
               FROM fms.facilities f WHERE f.id = $1",
        )
        .bind(fid)
        .bind(date)
        .fetch_optional(tx.conn())
        .await?
        .flatten(),
        None => Some(false),
    };

    if usable == Some(true) {
        return Ok(());
    }
    Err(Problem::validation(
        "補班日必須指定 `windows` —— 該場域在這個星期沒有常規班表，\
         沿用班表會讓這一天可用 0 分鐘，整筆設定不會有任何作用",
    )
    .with_errors(vec![FieldError {
        pointer: "/windows".to_string(),
        code: "REQUIRED".to_string(),
        message: "is_working_day = true 且該星期沒有常規班表時必填".to_string(),
    }]))
}

async fn require_scope(tx: &mut TenantTx, facility_id: Option<Uuid>) -> Result<(), Problem> {
    match facility_id {
        Some(fid) => require_permission(tx, "holiday:write", Some(fid), None)
            .await
            .map(|_| ()),
        None => require_tenant_scoped_permission(tx, "holiday:write")
            .await
            .map(|_| ()),
    }
}

const COLUMNS: &str = "id, facility_id, holiday_date, name, is_working_day, windows";

/// `GET /holiday-calendars`
pub async fn list(
    State(state): State<HolidayState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "holiday:read", q.facility_id, None).await?;

    let rows: Vec<HolidayDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM fms.holiday_calendars
          WHERE ($1::uuid IS NULL OR facility_id = $1::uuid)
            AND ($2::date IS NULL OR holiday_date >= $2::date)
            AND ($3::date IS NULL OR holiday_date <= $3::date)
          ORDER BY holiday_date, facility_id NULLS FIRST"
    ))
    .bind(q.facility_id)
    .bind(q.from)
    .bind(q.to)
    .fetch_all(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": rows })))
}

/// `POST /holiday-calendars`
pub async fn create(
    State(state): State<HolidayState>,
    caller: Caller,
    Json(body): Json<HolidayCreate>,
) -> Result<(StatusCode, Json<HolidayDto>), Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_scope(&mut tx, body.facility_id).await?;
    check_make_up_day(
        &mut tx,
        body.facility_id,
        body.holiday_date,
        body.is_working_day,
        body.windows.as_ref(),
    )
    .await?;

    let row: HolidayDto = sqlx::query_as(&format!(
        "INSERT INTO fms.holiday_calendars
           (tenant_id, facility_id, holiday_date, name, is_working_day, windows)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5)
         RETURNING {COLUMNS}"
    ))
    .bind(body.facility_id)
    .bind(body.holiday_date)
    .bind(&body.name)
    .bind(body.is_working_day)
    .bind(&body.windows)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `PATCH /holiday-calendars/{holidayId}`
///
/// **改行事曆不影響已經開立的工單** —— 期限在開單時就算成絕對時刻
/// （ADR-12 決定 F 的快照）。新的行事曆只對之後開立的工單生效。
pub async fn update(
    State(state): State<HolidayState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<HolidayUpdate>,
) -> Result<Json<HolidayDto>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let current: Option<(
        Option<Uuid>,
        chrono::NaiveDate,
        bool,
        Option<serde_json::Value>,
    )> = sqlx::query_as(
        "SELECT facility_id, holiday_date, is_working_day, windows
               FROM fms.holiday_calendars WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(tx.conn())
    .await?;
    let (cur_facility, cur_date, cur_working, cur_windows) =
        current.ok_or_else(|| Problem::not_found("holiday entry not found"))?;

    require_scope(&mut tx, cur_facility).await?;
    if let Some(target) = body.facility_id {
        if target != cur_facility {
            require_scope(&mut tx, target).await?;
        }
    }

    // 合併之後的值才是要驗的對象：把 windows 清成 null 而保留
    // is_working_day = true，同樣是那個沉默失效的設定。
    let merged_facility = body.facility_id.unwrap_or(cur_facility);
    let merged_date = body.holiday_date.unwrap_or(cur_date);
    let merged_working = body.is_working_day.unwrap_or(cur_working);
    let merged_windows = match &body.windows {
        Some(v) => v.clone(),
        None => cur_windows,
    };
    check_make_up_day(
        &mut tx,
        merged_facility,
        merged_date,
        merged_working,
        merged_windows.as_ref(),
    )
    .await?;

    let row: HolidayDto = sqlx::query_as(&format!(
        "UPDATE fms.holiday_calendars SET
           name           = coalesce($2, name),
           facility_id    = CASE WHEN $3::bool THEN $4::uuid ELSE facility_id END,
           holiday_date   = coalesce($5, holiday_date),
           is_working_day = coalesce($6, is_working_day),
           windows        = CASE WHEN $7::bool THEN $8::jsonb ELSE windows END,
           updated_at     = clock_timestamp()
         WHERE id = $1
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(&body.name)
    // facility_id 與 windows 都不能用 coalesce：null 是有意義的值
    // （租戶通用／沿用該星期的班表）。
    .bind(body.facility_id.is_some())
    .bind(body.facility_id.flatten())
    .bind(body.holiday_date)
    .bind(body.is_working_day)
    .bind(body.windows.is_some())
    .bind(body.windows.clone().flatten())
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok(Json(row))
}

/// `DELETE /holiday-calendars/{holidayId}`
///
/// 真的刪除，不是軟刪除 —— 沒有任何東西參照這一列（期限在開單時就算成
/// 絕對時刻了）。這與 SLA 政策不同：那些只能停用，因為已開立的工單
/// 快照了它們的 id。
pub async fn delete(
    State(state): State<HolidayState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let current: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT facility_id FROM fms.holiday_calendars WHERE id = $1")
            .bind(id)
            .fetch_optional(tx.conn())
            .await?;
    let current = current.ok_or_else(|| Problem::not_found("holiday entry not found"))?;
    require_scope(&mut tx, current).await?;

    sqlx::query("DELETE FROM fms.holiday_calendars WHERE id = $1")
        .bind(id)
        .execute(tx.conn())
        .await?;
    tx.commit().await?;

    Ok(StatusCode::NO_CONTENT)
}
