//! RRULE（RFC 5545）排程展開。
//!
//! # 為什麼用 crate 而不自己寫
//!
//! RRULE 是**規格**，不是這個系統的領域規則。自己實作等於再造一份規格，
//! 而它的 bug 都在最不容易測到的地方：月底夾值（`BYMONTHDAY=31` 遇到二月）、
//! 夏令時邊界、`BYDAY` 與 `INTERVAL` 的交互。這正是 ADR-09
//! 「不要製造第二份真實來源」該套用的地方 —— 只是這次的真實來源是 RFC。
//!
//! # 時區是本模組唯一的領域決策
//!
//! `maintenance_plans` 沒有時區欄位，但 `facilities.timezone` 有
//! （預設 `Asia/Taipei`）。展開**必須在場域當地時區**進行：
//! 「每月 5 號上午 9 點保養」是當地時間的敘述。若在 UTC 展開，
//! 台北時間 08:00 的排程會落在 UTC 前一天，`BYMONTHDAY=5` 就變成 4 號；
//! 跨夏令時的場域則會整批位移一小時。
//!
//! 展開結果轉回 UTC 再存回 `timestamptz`，因此資料庫端仍然是絕對時刻。

use chrono::TimeZone;

use crate::problem::Problem;

/// 展開所需的計畫資訊。
pub struct PlanSchedule<'a> {
    pub rrule: &'a str,
    /// 展開的起點。用 `next_due_at`（若有）否則計畫建立時間 ——
    /// `maintenance_plans` 沒有 `dtstart` 欄位，這是本實作補的定義。
    pub dtstart: chrono::DateTime<chrono::Utc>,
    /// 場域時區（IANA 名稱）。
    pub timezone: &'a str,
}

/// 展開接下來的排程時刻。
///
/// `limit` 是硬上界：RRULE 可以是無限序列（沒有 `UNTIL`／`COUNT`），
/// 沒有上界的展開會直接把記憶體吃掉。契約的 preview-schedule
/// 預設 12、上限 100，產生器則用自己的視窗大小。
///
/// `until` 額外收斂：只回傳這個時刻之前的排程。
pub fn expand(
    plan: &PlanSchedule<'_>,
    limit: u16,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<chrono::DateTime<chrono::Utc>>, Problem> {
    let tz: chrono_tz::Tz = plan.timezone.parse().map_err(|_| {
        // 時區字串壞掉是設定問題而非客戶端輸入，但它會讓整個計畫無法展開，
        // 因此要在訊息裡指名，不要只回一個泛用的 500。
        Problem::internal(std::io::Error::other(format!(
            "facility timezone `{}` is not a known IANA name",
            plan.timezone
        )))
    })?;

    let local_start = plan.dtstart.with_timezone(&tz);
    // rrule 的字串形式需要 DTSTART 與 RRULE 兩行。刻意組合成字串而不是
    // 用 builder：這樣 `rrule` 欄位就是原樣交給解析器，
    // 應用層不會偷偷改寫使用者寫的規則。
    let spec = format!(
        "DTSTART;TZID={}:{}\nRRULE:{}",
        tz.name(),
        local_start.format("%Y%m%dT%H%M%S"),
        plan.rrule.trim()
    );

    let set: rrule::RRuleSet = spec.parse().map_err(|e| {
        // RRULE 是使用者（管理員）輸入的，因此語法錯誤是 422 而非 500。
        Problem::validation(format!("invalid RRULE: {e}"))
    })?;

    let result = set.all(limit);
    let mut out: Vec<chrono::DateTime<chrono::Utc>> = result
        .dates
        .into_iter()
        .map(|d| d.with_timezone(&chrono::Utc))
        .collect();

    if let Some(until) = until {
        out.retain(|d| *d <= until);
    }
    Ok(out)
}

/// 由本地日期時間組出 UTC 時刻，供測試與 dtstart 推導使用。
///
/// 夏令時的「不存在時刻」（例如春季往前跳的那一小時）在此回錯誤而非
/// 靜默挑一個 —— 靜默挑值會讓排程無聲地偏移。
pub fn local_to_utc(
    naive: chrono::NaiveDateTime,
    timezone: &str,
) -> Result<chrono::DateTime<chrono::Utc>, Problem> {
    let tz: chrono_tz::Tz = timezone
        .parse()
        .map_err(|_| Problem::validation(format!("unknown timezone `{timezone}`")))?;
    match tz.from_local_datetime(&naive).single() {
        Some(dt) => Ok(dt.with_timezone(&chrono::Utc)),
        None => Err(Problem::validation(format!(
            "{naive} does not exist exactly once in {timezone} (daylight-saving boundary)"
        ))),
    }
}
