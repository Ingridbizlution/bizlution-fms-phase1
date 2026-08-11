//! 標準 5 欄 crontab 語法（`identity_providers.sync_cron` 用）。
//!
//! # 為什麼用 crate 而不自己寫
//!
//! 與 [`crate::schedule`]（RRULE）同一個判斷：crontab 語法是規格，不是這個
//! 系統的領域規則。自己寫「這個表達式在這段時間內有沒有排定時刻」等於
//! 重新實作月份／星期交互、`*/N` 步進、月底邊界這些規格細節，而它們的
//! bug 都在最不容易測到的地方。
//!
//! # 「到期」的定義：區間內有排定時刻，不是「現在符合」
//!
//! 不能只問「現在這一刻符合 cron 表達式嗎」（`Cron::contains`）——
//! 掃描迴圈不保證每分鐘都跑一次，錯過那一分鐘就永遠不會補跑。
//! 正確的問法是「自從上次跑完，到現在這段時間內，有沒有排定的時刻」，
//! 這正是 [`is_due`] 用 `next_after(since)` 而不是 `contains(now)` 的理由。

use chrono::{DateTime, Utc};

use crate::problem::Problem;

/// 判斷從 `since`（不含）到 `now`（含）之間，這個 cron 表達式是否有排定的時刻。
///
/// `since = None` 代表「從未執行過」—— 一律視為到期，讓部署或重啟後能立刻
/// 補跑一次，與其他掃描迴圈（PM 計畫、no-show）同一個判斷：停機期間累積的
/// 工作不該等到下一個整點才被看見。
pub fn is_due(
    expr: &str,
    since: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<bool, Problem> {
    let cron: saffron::Cron = expr
        .parse()
        .map_err(|e| Problem::validation(format!("invalid cron expression `{expr}`: {e}")))?;
    Ok(match since {
        None => true,
        Some(t) => cron.next_after(t).is_some_and(|next| next <= now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .unwrap()
            .and_utc()
    }

    #[test]
    fn never_run_is_always_due() {
        assert!(is_due("0 */4 * * *", None, t("2026-08-05 00:00:00")).unwrap());
    }

    #[test]
    fn due_when_a_fire_time_falls_in_the_window() {
        // 每 4 小時（0/4/8/12/16/20 點）。上次跑完是 07:00，現在 08:05 ——
        // 08:00 那個時刻落在區間內。
        assert!(is_due(
            "0 */4 * * *",
            Some(t("2026-08-05 07:00:00")),
            t("2026-08-05 08:05:00")
        )
        .unwrap());
    }

    #[test]
    fn not_due_before_the_next_fire_time() {
        // 上次跑完是 08:00，現在 09:00 —— 下一次是 12:00，還沒到。
        assert!(!is_due(
            "0 */4 * * *",
            Some(t("2026-08-05 08:00:00")),
            t("2026-08-05 09:00:00")
        )
        .unwrap());
    }

    #[test]
    fn malformed_expression_is_a_validation_error_not_a_panic() {
        assert!(is_due("not a cron", None, t("2026-08-05 00:00:00")).is_err());
    }
}
