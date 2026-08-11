//! 登入失敗節流。
//!
//! # 為什麼鍵是 `(tenant_code, username)` 而不是來源 IP
//!
//! 要防的是「對某個帳號猜密碼」。以帳號為鍵直接限制的就是這件事，
//! 而且**換 IP 繞不過**——這是選它的主要理由：以 IP 為鍵的節流對任何
//! 有代理池的攻擊者形同不存在。
//!
//! 反過來說，以 IP 為鍵才能擋「同一來源掃描大量帳號」，而那需要**可信的**
//! 對端位址。應用層目前拿不到：`axum::serve` 沒有帶 `ConnectInfo`，
//! 而 `X-Forwarded-For` 在沒有「可信代理清單」的設定之前是客戶端可任意
//! 偽造的字串——用它當節流鍵等於讓攻擊者自己決定要不要被限流。
//! 因此 IP 維度刻意留在反向代理（它知道真正的對端），
//! 應用層只做代理做不到的那一半。兩者不是替代關係。
//!
//! # 為什麼計數失敗而不是鎖定帳號
//!
//! 成功登入即歸零。若改成「N 次失敗鎖 M 分鐘」，任何人只要知道某個
//! username 就能定時送錯密碼把該帳號永久鎖住——把防暴力破解換成了
//! 針對特定人的阻斷服務。窗式計數沒有這個性質：受害者打對密碼就恢復。
//!
//! # 這一層的限制：每個行程一份
//!
//! 狀態在記憶體裡。水平擴充到 N 個實例時，實際容許的嘗試次數是 N 倍。
//! 這是刻意接受的：把它換成 Redis 會讓「登入」依賴一個新的外部元件，
//! 而 Redis 不可用時要在「拒絕所有登入」與「完全不限流」之間選一個，
//! 兩個答案都不好。以帳號為鍵的每行程計數已經把窮舉速度壓到不可行的
//! 量級，跨行程精確計數要換的那點嚴謹度不值得那個可用性風險。
//! 真的需要全域精確時，該做的是在反向代理層限流，而不是讓應用層變脆。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use fms_shared::LoginThrottleSettings;

/// 節流的鍵。`tenant_code` 必須含在內：不同租戶可以有同名的 username，
/// 少了它，A 租戶的失敗會拖累 B 租戶的同名使用者。
fn key_of(tenant_code: &str, username: &str) -> String {
    format!("{tenant_code}\u{1f}{username}")
}

struct Window {
    failures: u32,
    /// 窗的起點。第一次失敗時設定，窗到期後由下一次失敗重設。
    started: Instant,
}

/// 以 `(tenant_code, username)` 為鍵的失敗計數器。
pub struct LoginThrottle {
    windows: Mutex<HashMap<String, Window>>,
    settings: LoginThrottleSettings,
}

impl LoginThrottle {
    pub fn new(settings: LoginThrottleSettings) -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
            settings,
        }
    }

    /// 目前是否應該拒絕這個帳號的嘗試。回傳 `Some(還要等幾秒)` 表示拒絕。
    ///
    /// 刻意**不**在這裡計數：被擋掉的請求根本沒有驗證密碼，把它算成一次
    /// 失敗會讓攻擊者持續送請求就能無限延長封鎖期——那又變回可被利用的
    /// 帳號鎖定了。窗的長度只由真正的失敗決定。
    pub fn check(&self, tenant_code: &str, username: &str) -> Option<u64> {
        let key = key_of(tenant_code, username);
        let windows = self.lock();
        let w = windows.get(&key)?;
        let elapsed = w.started.elapsed();
        if w.failures >= self.settings.max_failures && elapsed < self.settings.window {
            // 至少回 1：`Retry-After: 0` 的意思是「現在就可以再試」，
            // 與我們剛剛拒絕它自相矛盾。
            Some((self.settings.window - elapsed).as_secs().max(1))
        } else {
            None
        }
    }

    /// 記下一次真正的認證失敗（密碼錯誤、帳號不存在、租戶不存在、帳號停用）。
    pub fn record_failure(&self, tenant_code: &str, username: &str) {
        let key = key_of(tenant_code, username);
        let mut windows = self.lock();

        // 順手清掉過期的窗。沒有這一步，攻擊者用大量隨機 username 就能
        // 讓這個 map 無上限成長——節流本身反而成了記憶體耗盡的途徑。
        //
        // 全表掃描是可接受的：能留在表裡的鍵數受「窗長 × 失敗速率」限制，
        // 而失敗速率本身已經被 argon2 的成本壓住。
        let window = self.settings.window;
        windows.retain(|_, w| w.started.elapsed() < window);

        let entry = windows.entry(key).or_insert_with(|| Window {
            failures: 0,
            started: Instant::now(),
        });
        if entry.started.elapsed() >= window {
            entry.failures = 0;
            entry.started = Instant::now();
        }
        entry.failures += 1;
    }

    /// 認證成功：清掉該帳號的窗。
    pub fn clear(&self, tenant_code: &str, username: &str) {
        self.lock().remove(&key_of(tenant_code, username));
    }

    /// 取鎖。`Mutex` 被 poison 的唯一途徑是持鎖時 panic，而臨界區內只有
    /// HashMap 操作、不會 panic。真的發生時取 `into_inner` 繼續，
    /// 而不是讓每一次登入從此都 panic——那會把一次意外升級成全面停擺。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Window>> {
        self.windows.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn throttle(max_failures: u32, window_secs: u64) -> LoginThrottle {
        LoginThrottle::new(LoginThrottleSettings {
            max_failures,
            window: Duration::from_secs(window_secs),
        })
    }

    #[test]
    fn allows_exactly_max_failures_then_blocks() {
        let t = throttle(3, 300);
        for n in 1..=3 {
            assert!(t.check("T", "u").is_none(), "第 {n} 次嘗試應放行");
            t.record_failure("T", "u");
        }
        assert!(t.check("T", "u").is_some(), "第 4 次嘗試應被擋");
    }

    #[test]
    fn retry_after_is_never_zero() {
        let t = throttle(0, 1);
        t.record_failure("T", "u");
        assert_eq!(t.check("T", "u"), Some(1));
    }

    #[test]
    fn success_clears_the_window() {
        let t = throttle(1, 300);
        t.record_failure("T", "u");
        t.record_failure("T", "u");
        assert!(t.check("T", "u").is_some());
        t.clear("T", "u");
        assert!(t.check("T", "u").is_none());
    }

    #[test]
    fn tenants_do_not_share_a_counter() {
        let t = throttle(0, 300);
        t.record_failure("TENANT_A", "same.name");
        assert!(t.check("TENANT_A", "same.name").is_some());
        assert!(
            t.check("TENANT_B", "same.name").is_none(),
            "不同租戶的同名使用者不得共用計數"
        );
    }

    #[test]
    fn expired_windows_are_dropped() {
        let t = throttle(0, 0);
        t.record_failure("T", "gone");
        t.record_failure("T", "other");
        assert_eq!(
            t.lock().len(),
            1,
            "窗長為 0 時每次記錄都應清掉先前的鍵，只留當次"
        );
    }
}
