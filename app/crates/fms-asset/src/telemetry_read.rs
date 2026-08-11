//! 遙測讀取（`/telemetry/latest`、`/telemetry/series`）。
//!
//! 與 `telemetry.rs`（寫入）分開，因為關心的事完全不同：那邊是逐筆 savepoint
//! 與規則評估，這邊是分桶聚合與「圖表會不會說謊」。
//!
//! # 在這兩支之前，資料進得去出不來
//!
//! `telemetry_readings` 從 006 就存在、`ingest_telemetry()` 一直在寫，
//! 而**沒有任何端點讀它**。#17 補上了 `POST /telemetry:batch-ingest`，
//! 於是這個系統可以收 IoT 資料 —— 但沒有人能把它讀回來。
//!
//! # 降採樣只回 avg 會讓圖表說謊
//!
//! 一個五分鐘的桶裡溫度衝到 31 度三秒鐘，avg 會是 24。而告警的門檻是
//! 「超過 28」—— 也就是說**看圖的人看不到那件真的發生過的事**。
//!
//! 所以每個桶回 `min`／`max`／`avg`／`count`，而不是一個 `value`。
//! 前端要畫線用 avg，要看有沒有越界用 max —— 兩個問題不同，
//! 而後端替他們選一個就是把資訊丟掉。
//!
//! `count` 是「這個桶有幾筆原始讀數」：0 筆的桶不會出現（沒有資料就是沒有），
//! 而 1 筆的桶其 avg = min = max，那本身就是「這段時間只採到一筆」的訊號。
//!
//! # 桶的邊界必須是絕對的
//!
//! `date_bin` 需要一個 origin。用 `from` 當 origin 的話，
//! `from=10:02` 與 `from=10:00` 會切出**錯開的桶**，於是同一個點位的兩次查詢
//! 拿到的數列無法互相比較，疊圖會歪。
//!
//! 所以 origin 是一個固定的時刻（`2000-01-01 00:00:00+00`）——
//! 5 分鐘的桶永遠落在整 5 分，跟誰查、什麼時候查都無關。
//!
//! # 上限要說出來，不能安靜地砍
//!
//! 一個月的 1 Hz 原始資料是 260 萬筆。回傳上限是必要的，
//! 但**被截斷這件事必須出現在回應裡** —— 前端拿到一段看起來完整的數列、
//! 而它其實只到一半，比拿到錯誤更糟。
//!
//! # 「最新值」需要一個年齡
//!
//! `telemetry_latest` 存的是最後一筆，而它可能是三天前的。一個儀表板
//! 把三天前的 24 度顯示成當前室溫，比顯示「無資料」危險得多。
//!
//! 所以 `/telemetry/latest` 回 `age_seconds` 與 `is_stale`，
//! 而判定用的是**裝置自己的** `offline_alarm_after_seconds`
//! —— 與 `devices.rs` 的 `connectivity` 同一個管理者設定的門檻。

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, Problem};

use crate::handlers::AssetState;

/// 原始資料的回傳上限。超過就截斷並在 `meta.truncated` 說明。
const MAX_RAW_POINTS: i64 = 10_000;
/// 分桶後的上限。5000 個桶已經遠超任何螢幕的像素寬度。
const MAX_BUCKETS: i64 = 5_000;
/// 一次查幾個點位。多點是這支端點存在的理由（儀表板一次要十幾個讀數），
/// 但不該變成「把整個租戶的點位一次撈出來」。
const MAX_POINTS_PER_QUERY: usize = 50;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LatestDto {
    pub telemetry_point_id: Uuid,
    pub point_code: String,
    pub point_name: String,
    pub unit: Option<String>,
    pub device_id: Uuid,
    pub device_code: String,
    pub facility_id: Uuid,
    pub asset_id: Option<Uuid>,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub value_num: Option<f64>,
    pub value_bool: Option<bool>,
    pub value_text: Option<String>,
    pub quality: String,
    /// 這筆「最新值」有多舊。
    pub age_seconds: i64,
    /// 超過裝置自己的 `offline_alarm_after_seconds` 就是 stale。
    /// 見模組檔頭：三天前的讀數不是當前室溫。
    pub is_stale: bool,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct BucketDto {
    pub telemetry_point_id: Uuid,
    pub bucket_start: chrono::DateTime<chrono::Utc>,
    /// 桶內原始筆數。1 表示這段時間只採到一筆，avg／min／max 會相同。
    pub sample_count: i64,
    pub avg_value: Option<f64>,
    /// **不要只用 avg 畫圖**：越界是 max 的問題，見模組檔頭。
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    /// 桶內最後一筆的時刻與值 —— 狀態型的點位（開／關）看 avg 沒有意義。
    pub last_observed_at: chrono::DateTime<chrono::Utc>,
    pub last_value_num: Option<f64>,
    pub last_value_bool: Option<bool>,
    pub last_value_text: Option<String>,
    /// 桶內有沒有非 GOOD 的品質。混進 BAD 讀數的平均值不能當事實用。
    pub has_suspect_quality: bool,
}

#[derive(Debug, Deserialize)]
pub struct LatestQuery {
    /// 逗號分隔的點位 id。
    pub point_ids: Option<String>,
    /// 或整台裝置的所有點位。
    pub device_id: Option<Uuid>,
    /// 或整個場域（儀表板首頁）。
    pub facility_id: Option<Uuid>,
    /// 只回 stale 的 —— 「哪些讀數已經不能信」那個問題。
    pub stale_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct SeriesQuery {
    pub point_ids: Option<String>,
    pub device_id: Option<Uuid>,
    pub point_code: Option<String>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    /// `raw`、或 `30s`／`5m`／`1h`／`1d`。省略時預設 `raw`。
    pub interval: Option<String>,
}

/// `GET /telemetry/latest`
pub async fn latest(
    State(state): State<AssetState>,
    caller: Caller,
    Query(q): Query<LatestQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let point_ids = parse_point_ids(q.point_ids.as_deref())?;
    if point_ids.is_none() && q.device_id.is_none() && q.facility_id.is_none() {
        return Err(Problem::validation(
            "要有 point_ids、device_id 或 facility_id 其中之一 —— \
             沒有範圍的「最新值」會是整個租戶的每一個點位",
        ));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "telemetry:read", q.facility_id, None).await?;

    // stale 判定用裝置的 `offline_alarm_after_seconds`，與 devices.rs 一致。
    // 這裡不必再過濾場域：060 之後 `telemetry_latest` 有 RESTRICTIVE 的
    // facility_scope，範圍外的列根本不會出現。
    let rows: Vec<LatestDto> = sqlx::query_as(
        "SELECT l.telemetry_point_id,
                p.point_code::text AS point_code, p.name::text AS point_name,
                p.unit::text AS unit,
                l.device_id, d.device_code::text AS device_code,
                d.facility_id, d.asset_id,
                l.observed_at,
                l.value_num::float8 AS value_num, l.value_bool,
                l.value_text::text AS value_text, l.quality::text AS quality,
                floor(extract(epoch FROM now() - l.observed_at))::bigint AS age_seconds,
                (l.observed_at < now()
                   - (d.offline_alarm_after_seconds || ' seconds')::interval) AS is_stale
           FROM fms.telemetry_latest l
           JOIN fms.telemetry_points p ON p.id = l.telemetry_point_id
           JOIN fms.devices d ON d.id = l.device_id
          WHERE d.deleted_at IS NULL
            AND ($1::uuid[] IS NULL OR l.telemetry_point_id = ANY($1::uuid[]))
            AND ($2::uuid IS NULL OR l.device_id = $2::uuid)
            AND ($3::uuid IS NULL OR d.facility_id = $3::uuid)
            AND (NOT $4::bool OR l.observed_at < now()
                 - (d.offline_alarm_after_seconds || ' seconds')::interval)
          ORDER BY d.device_code, p.point_code",
    )
    .bind(point_ids.as_deref())
    .bind(q.device_id)
    .bind(q.facility_id)
    .bind(q.stale_only.unwrap_or(false))
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let stale = rows.iter().filter(|r| r.is_stale).count();
    Ok(Json(serde_json::json!({
        "items": rows,
        "meta": {
            // 「有幾個讀數已經不能信」放在 meta 而不是要前端自己數 ——
            // 那是儀表板該顯示警示的依據。
            "stale_count": stale,
        },
    })))
}

/// `GET /telemetry/series`
pub async fn series(
    State(state): State<AssetState>,
    caller: Caller,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let point_ids = parse_point_ids(q.point_ids.as_deref())?;
    if point_ids.is_none() && q.device_id.is_none() {
        return Err(Problem::validation(
            "要有 point_ids 或 device_id（可再加 point_code 縮小）",
        ));
    }

    // 預設近 24 小時。沒有預設會讓「忘記帶 from」變成掃全部歷史。
    let to = q.to.unwrap_or_else(chrono::Utc::now);
    let from = q.from.unwrap_or(to - chrono::Duration::hours(24));
    if from >= to {
        return Err(Problem::validation("from 必須早於 to"));
    }

    let bucket = parse_interval(q.interval.as_deref())?;
    if let Some(secs) = bucket {
        // 先算桶數再查：一個 `interval=1s` 加一年的範圍會是 3100 萬個桶，
        // 而那件事該在送出查詢**之前**被擋下來，不是讓資料庫算完再截斷。
        let span = (to - from).num_seconds().max(1);
        let buckets = span / secs;
        if buckets > MAX_BUCKETS {
            return Err(Problem::validation(format!(
                "這個範圍與 interval 會產生約 {buckets} 個桶，上限 {MAX_BUCKETS} —— \
                 請縮小範圍或加大 interval"
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "telemetry:read", None, None).await?;

    let limit = if bucket.is_some() {
        MAX_BUCKETS
    } else {
        MAX_RAW_POINTS
    };

    let rows: Vec<BucketDto> = match bucket {
        // 原始資料：每一筆自己是一個桶。這樣兩種模式的回應形狀相同 ——
        // 前端不必為 `raw` 走另一條分支。
        None => {
            sqlx::query_as(
                "SELECT r.telemetry_point_id, r.observed_at AS bucket_start,
                    1::bigint AS sample_count,
                    r.value_num::float8 AS avg_value,
                    r.value_num::float8 AS min_value,
                    r.value_num::float8 AS max_value,
                    r.observed_at AS last_observed_at,
                    r.value_num::float8 AS last_value_num,
                    r.value_bool AS last_value_bool,
                    r.value_text::text AS last_value_text,
                    (r.quality <> 'GOOD') AS has_suspect_quality
               FROM fms.telemetry_readings r
               JOIN fms.telemetry_points p ON p.id = r.telemetry_point_id
              WHERE r.observed_at >= $1 AND r.observed_at < $2
                AND ($3::uuid[] IS NULL OR r.telemetry_point_id = ANY($3::uuid[]))
                AND ($4::uuid IS NULL OR p.device_id = $4::uuid)
                AND ($5::text IS NULL OR lower(p.point_code) = lower($5::text))
              ORDER BY r.telemetry_point_id, r.observed_at
              LIMIT $6",
            )
            .bind(from)
            .bind(to)
            .bind(point_ids.as_deref())
            .bind(q.device_id)
            .bind(q.point_code.as_deref())
            .bind(limit + 1)
            .fetch_all(tx.conn())
            .await?
        }

        // 分桶。origin 是固定時刻，見模組檔頭 —— 桶的邊界必須絕對。
        Some(secs) => {
            sqlx::query_as(
                "SELECT r.telemetry_point_id,
                    date_bin(($1::text || ' seconds')::interval, r.observed_at,
                             timestamptz '2000-01-01 00:00:00+00') AS bucket_start,
                    count(*)::bigint AS sample_count,
                    avg(r.value_num)::float8 AS avg_value,
                    min(r.value_num)::float8 AS min_value,
                    max(r.value_num)::float8 AS max_value,
                    max(r.observed_at) AS last_observed_at,
                    -- 桶內最後一筆的值。`ORDER BY ... DESC` 的第一個非 NULL：
                    -- 狀態型點位（value_bool）的平均沒有意義，要的是最後狀態。
                    (array_agg(r.value_num::float8 ORDER BY r.observed_at DESC))[1]
                      AS last_value_num,
                    (array_agg(r.value_bool ORDER BY r.observed_at DESC))[1]
                      AS last_value_bool,
                    (array_agg(r.value_text::text ORDER BY r.observed_at DESC))[1]
                      AS last_value_text,
                    bool_or(r.quality <> 'GOOD') AS has_suspect_quality
               FROM fms.telemetry_readings r
               JOIN fms.telemetry_points p ON p.id = r.telemetry_point_id
              WHERE r.observed_at >= $2 AND r.observed_at < $3
                AND ($4::uuid[] IS NULL OR r.telemetry_point_id = ANY($4::uuid[]))
                AND ($5::uuid IS NULL OR p.device_id = $5::uuid)
                AND ($6::text IS NULL OR lower(p.point_code) = lower($6::text))
              GROUP BY r.telemetry_point_id, 2
              ORDER BY r.telemetry_point_id, 2
              LIMIT $7",
            )
            .bind(secs.to_string())
            .bind(from)
            .bind(to)
            .bind(point_ids.as_deref())
            .bind(q.device_id)
            .bind(q.point_code.as_deref())
            .bind(limit + 1)
            .fetch_all(tx.conn())
            .await?
        }
    };
    tx.commit().await?;

    // 截斷必須說出來。多取一筆來判斷，回傳時砍掉。
    let truncated = rows.len() as i64 > limit;
    let items: Vec<BucketDto> = rows.into_iter().take(limit as usize).collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "meta": {
            "from": from,
            "to": to,
            // 回報實際用的桶寬（秒），null = raw。前端不該去反推自己傳了什麼。
            "bucket_seconds": bucket,
            "bucket_origin": "2000-01-01T00:00:00Z",
            "limit": limit,
            // **看得見的截斷。** 見模組檔頭：一段看起來完整而其實只到一半的
            // 數列，比一個錯誤更難發現。
            "truncated": truncated,
        },
    })))
}

/// 解析逗號分隔的點位 id。
fn parse_point_ids(raw: Option<&str>) -> Result<Option<Vec<Uuid>>, Problem> {
    let Some(s) = raw else { return Ok(None) };
    let parts: Vec<&str> = s
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .collect();
    if parts.is_empty() {
        return Ok(None);
    }
    if parts.len() > MAX_POINTS_PER_QUERY {
        return Err(Problem::validation(format!(
            "一次最多 {MAX_POINTS_PER_QUERY} 個點位，這次給了 {}",
            parts.len()
        )));
    }
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        out.push(
            Uuid::parse_str(p).map_err(|_| {
                Problem::validation(format!("point_ids 裡的「{p}」不是合法的 uuid"))
            })?,
        );
    }
    Ok(Some(out))
}

/// `raw` → None；`30s`／`5m`／`1h`／`1d` → Some(秒數)。
///
/// 自己解析而不是把字串丟給 `::interval`：那會讓 `interval=1 year` 之類的
/// 東西通過，而桶數上限的檢查需要一個秒數才算得出來。
fn parse_interval(raw: Option<&str>) -> Result<Option<i64>, Problem> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if s.eq_ignore_ascii_case("raw") {
        return Ok(None);
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: i64 = num.parse().map_err(|_| {
        Problem::validation(format!("interval「{s}」格式應為 raw 或 30s／5m／1h／1d"))
    })?;
    if n <= 0 {
        return Err(Problem::validation("interval 必須是正數"));
    }
    let secs = match unit {
        "s" | "S" => n,
        "m" | "M" => n * 60,
        "h" | "H" => n * 3_600,
        "d" | "D" => n * 86_400,
        _ => {
            return Err(Problem::validation(format!(
                "interval 的單位「{unit}」不認得，應為 s／m／h／d"
            )))
        }
    };
    Ok(Some(secs))
}

#[cfg(test)]
mod tests {
    use super::parse_interval;

    #[test]
    fn interval_parsing() {
        assert_eq!(parse_interval(None).unwrap(), None);
        assert_eq!(parse_interval(Some("raw")).unwrap(), None);
        assert_eq!(parse_interval(Some("30s")).unwrap(), Some(30));
        assert_eq!(parse_interval(Some("5m")).unwrap(), Some(300));
        assert_eq!(parse_interval(Some("1h")).unwrap(), Some(3_600));
        assert_eq!(parse_interval(Some("1d")).unwrap(), Some(86_400));
        // 單位錯、數字錯、非正數都必須是 422 而不是被當成 raw ——
        // 「打錯的 interval 安靜地變成原始資料」會回傳幾萬筆。
        assert!(parse_interval(Some("5x")).is_err());
        assert!(parse_interval(Some("m")).is_err());
        assert!(parse_interval(Some("0m")).is_err());
        assert!(parse_interval(Some("-5m")).is_err());
    }
}
