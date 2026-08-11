//! 排程執行紀錄（`/maintenance-occurrences`）。
//!
//! # 這張表在 063 之前沒有終結狀態
//!
//! `api/ENDPOINTS.md` 把這支端點註明為「排程執行紀錄（**PM 合規率來源**）」，
//! 而在 063 之前那張表的寫入者只有兩個：`claim_occurrence`（`PLANNED`）與
//! `mark_generated`（`GENERATED`）。**沒有任何東西寫 COMPLETED，
//! 也沒有任何東西設 `completed_at`。**
//!
//! 所以「PM 準時完成率」若照原樣寫，會對每個租戶永遠回 0% ——
//! 而它看起來會像一支正常的報表。063 用一個綁在 `work_orders.completed_at`
//! 上的觸發器把鏈接起來。
//!
//! # `MISSED` 是算出來的，不是存的
//!
//! 004 的 CHECK 允許 `'MISSED'`，但**沒有任何東西寫它**，而那是刻意的：
//!
//!   * 「保養做完了」是一個發生的事件 → 寫下來（063 的觸發器）。
//!   * 「保養沒做」是一個**沒有發生**的事 → 沒有時刻、沒有行為者。
//!     存它需要有人定期去寫，而那個人不存在。
//!
//! 所以這裡的 `is_missed` 由 `scheduled_for + 計畫的容許窗` 與資料庫的現在
//! 比較得出 —— 與 `devices.connectivity`、`skills.status` 同一個判斷。
//!
//! # 容許窗來自計畫，不是這裡的常數
//!
//! `maintenance_plans.completion_grace_days`（063 新增）由管理者定義：
//! 法定年檢 0 天，月保養 7 天。一個寫死的數字會讓這兩者被同一把尺量。

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_permission, Caller, Cursor, PageMeta, Problem,
    SortSpec,
};

use crate::handlers::MaintenanceState;

const STATUSES: [&str; 5] = ["PLANNED", "GENERATED", "SKIPPED", "COMPLETED", "MISSED"];

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct OccurrenceDto {
    pub id: Uuid,
    pub plan_id: Uuid,
    pub plan_code: String,
    pub plan_name: String,
    pub facility_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub asset_code: Option<String>,
    pub scheduled_for: chrono::DateTime<chrono::Utc>,
    /// **儲存的**狀態。永遠不會是 `MISSED` —— 見模組檔頭。
    pub status: String,
    pub work_order_id: Option<Uuid>,
    pub work_order_no: Option<String>,
    pub skip_reason: Option<String>,
    pub generated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 計畫的容許窗。放進回應是因為底下三個判定都用它算的 ——
    /// 少了它，前端無法解釋「為什麼這筆算逾時」。
    pub grace_days: i32,
    /// 容許窗的截止時刻（`scheduled_for + grace_days`）。
    pub deadline: chrono::DateTime<chrono::Utc>,
    /// **算出來的**：過了容許窗而仍未完成。這就是 `MISSED`。
    pub is_missed: bool,
    /// 已完成但超過容許窗。
    pub is_late: bool,
    /// 逾時幾天（只有 `is_late` 時有值）。
    pub days_late: Option<f64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub plan_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    /// 逗號分隔的**儲存**狀態。
    pub status: Option<String>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    /// 只回傳算出來是漏做的。這是「哪些保養沒做」那個問題的正確問法 ——
    /// 用 `status=MISSED` 問會得到空清單，因為沒有人寫那個值。
    pub missed_only: Option<bool>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SkipBody {
    pub reason: Option<String>,
}

/// 容許窗與三個判定的 SQL。基準是**資料庫的現在**：
/// 應用伺服器與資料庫時鐘不同步時，Rust 端算的「過期了嗎」會漂移，
/// 而那種偏差只在部署環境出現。
const DERIVED: &str = "
  p.completion_grace_days::int AS grace_days,
  (o.scheduled_for + make_interval(days => p.completion_grace_days)) AS deadline,
  (o.completed_at IS NULL
   AND o.status <> 'SKIPPED'
   AND now() > o.scheduled_for + make_interval(days => p.completion_grace_days))
    AS is_missed,
  (o.completed_at IS NOT NULL
   AND o.completed_at > o.scheduled_for + make_interval(days => p.completion_grace_days))
    AS is_late,
  CASE WHEN o.completed_at IS NOT NULL
        AND o.completed_at > o.scheduled_for + make_interval(days => p.completion_grace_days)
       -- `::float8` 是必要的：`extract(epoch …)` 在 PG14+ 回 numeric，
       -- 而 DTO 是 f64 —— 少了它 sqlx 解不開，症狀是整支端點回 500。
       THEN (extract(epoch FROM (o.completed_at - o.scheduled_for
              - make_interval(days => p.completion_grace_days))) / 86400.0)::float8
  END AS days_late";

const COLUMNS: &str = "o.id, o.plan_id, p.code::text AS plan_code, p.name::text AS plan_name,
                       p.facility_id, o.asset_id, a.asset_code::text AS asset_code,
                       o.scheduled_for, o.status, o.work_order_id,
                       wo.wo_no::text AS work_order_no,
                       o.skip_reason::text AS skip_reason,
                       o.generated_at, o.completed_at, o.created_at";

const FROM: &str = "FROM fms.maintenance_occurrences o
                    JOIN fms.maintenance_plans p ON p.id = o.plan_id
                    LEFT JOIN fms.assets a ON a.id = o.asset_id
                    LEFT JOIN fms.work_orders wo ON wo.id = o.work_order_id";

/// `GET /maintenance-occurrences`
pub async fn list(
    State(state): State<MaintenanceState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let statuses = match q.status.as_deref() {
        Some(s) => {
            let v: Vec<String> = s
                .split(',')
                .map(|x| x.trim().to_uppercase())
                .filter(|x| !x.is_empty())
                .collect();
            for s in &v {
                if !STATUSES.contains(&s.as_str()) {
                    return Err(Problem::validation(format!(
                        "status 必須是 {} 其中之一（可逗號分隔）—— \
                         注意 MISSED 從來不會被寫入，要問漏做請用 missed_only",
                        STATUSES.join("／")
                    )));
                }
            }
            Some(v)
        }
        None => None,
    };

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "maintenance_plan:read", q.facility_id, None).await?;

    let limit = clamp_limit(q.limit);
    // 依排定時刻遞減：要看的是「最近該做什麼」。
    let sort = SortSpec {
        column: "scheduled_for".to_string(),
        desc: true,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.as_timestamp()?), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<OccurrenceDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS}, {DERIVED} {FROM}
          WHERE ($1::uuid IS NULL OR p.facility_id = $1::uuid)
            AND ($2::uuid IS NULL OR o.plan_id = $2::uuid)
            AND ($3::uuid IS NULL OR o.asset_id = $3::uuid)
            AND ($4::text[] IS NULL OR o.status = ANY($4::text[]))
            AND ($5::timestamptz IS NULL OR o.scheduled_for >= $5::timestamptz)
            AND ($6::timestamptz IS NULL OR o.scheduled_for < $6::timestamptz)
            AND (NOT $7::bool OR (o.completed_at IS NULL
                                  AND o.status <> 'SKIPPED'
                                  AND now() > o.scheduled_for
                                      + make_interval(days => p.completion_grace_days)))
            AND ($8::timestamptz IS NULL
                 OR (o.scheduled_for, o.id) < ($8::timestamptz, $9::uuid))
          ORDER BY o.scheduled_for DESC, o.id DESC
          LIMIT $10"
    ))
    .bind(q.facility_id)
    .bind(q.plan_id)
    .bind(q.asset_id)
    .bind(statuses.as_deref())
    .bind(q.from)
    .bind(q.to)
    .bind(q.missed_only.unwrap_or(false))
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| {
        (r.scheduled_for.to_rfc3339(), r.id)
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

/// `POST /maintenance-occurrences/{occurrenceId}/skip`
///
/// 契約寫「跳過本次（**需理由**）」。理由是必填而且會進合規報表的
/// `skip_reasons` 分佈 —— 那是「全部跳過卻得到 100%」時唯一能解釋的東西。
pub async fn skip(
    State(state): State<MaintenanceState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SkipBody>,
) -> Result<Json<OccurrenceDto>, Problem> {
    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Problem::validation(
                "reason 為必填 —— 跳過的保養會從合規率的分母裡拿掉，\
                 而一個沒有理由的排除等於把數字調好看",
            )
        })?;
    if reason.chars().count() > 200 {
        return Err(Problem::validation("reason 最多 200 個字"));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "maintenance_plan:write", None, None).await?;

    // **條件式 UPDATE，不是先讀再寫。** 已完成的不能改成跳過 ——
    // 那會把一件做過的保養從分母裡拿掉。
    // 在 handler 裡先查狀態再更新，在並發下會讓兩個請求都通過檢查。
    let updated: Option<Uuid> = sqlx::query_scalar(
        "UPDATE fms.maintenance_occurrences
            SET status = 'SKIPPED', skip_reason = $2
          WHERE id = $1
            AND status IN ('PLANNED', 'GENERATED')
          RETURNING id",
    )
    .bind(id)
    .bind(reason)
    .fetch_optional(tx.conn())
    .await?;

    if updated.is_none() {
        // 分開兩種原因：找不到 vs 狀態不允許。混成一個訊息會讓
        // 「已經完成了」看起來像「不存在」。
        let existing: Option<String> =
            sqlx::query_scalar("SELECT status FROM fms.maintenance_occurrences WHERE id = $1")
                .bind(id)
                .fetch_optional(tx.conn())
                .await?;
        return Err(match existing.as_deref() {
            None => Problem::not_found("找不到這筆排程紀錄（或它不在你的範圍內）"),
            Some("COMPLETED") => Problem::new(fms_shared::ProblemCode::Conflict).with_detail(
                "這次保養已經完成了，不能改成跳過 —— \
                 那會把一件做過的事從合規率的分母裡拿掉",
            ),
            Some("SKIPPED") => {
                Problem::new(fms_shared::ProblemCode::Conflict).with_detail("這次已經被跳過了")
            }
            Some(s) => Problem::new(fms_shared::ProblemCode::Conflict)
                .with_detail(format!("狀態 {s} 不能改成跳過")),
        });
    }

    let row: OccurrenceDto = sqlx::query_as(&format!(
        "SELECT {COLUMNS}, {DERIVED} {FROM} WHERE o.id = $1"
    ))
    .bind(id)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(row))
}
