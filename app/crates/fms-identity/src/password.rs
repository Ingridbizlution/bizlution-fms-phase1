//! 密碼雜湊與驗證（argon2id，對應規格書身分整合的本地帳號軌）。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use fms_shared::Problem;

/// 驗證密碼。雜湊格式錯誤視為驗證失敗而非內部錯誤 —— 一列壞資料
/// 不應讓呼叫端看到 500，也不該洩漏「這個帳號的雜湊有問題」。
pub fn verify(password: &str, phc_hash: &str) -> bool {
    match PasswordHash::new(phc_hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => {
            tracing::warn!("stored password hash is not valid PHC format");
            false
        }
    }
}

/// 對一個固定的假雜湊跑一次完整驗證，回傳值永遠是 `false`。
///
/// # 為什麼需要它
///
/// argon2 的驗證是刻意昂貴的（預設參數約數十毫秒），而登入路徑上其他所有
/// 工作都是次毫秒級。因此「有沒有跑過 argon2」在牆上時鐘裡是一個清楚可測的
/// 差異：帳號不存在時直接回錯誤，會比密碼錯誤快一個數量級，
/// 攻擊者據此就能在**不知道任何密碼**的前提下枚舉出哪些帳號真的存在。
///
/// [`super::handlers`] 的登入失敗處理刻意不區分四種原因（租戶不存在／
/// 使用者不存在／無本地密碼／密碼錯誤），但那只統一了**回應內容**；
/// 沒有這一支，時間本身仍然把答案洩漏出去。
///
/// 假雜湊在首次使用時以 [`hash`] 產生，因此參數與真實雜湊完全相同 ——
/// 寫死一個常數會在日後調整 argon2 成本時悄悄失去等時性。
pub fn verify_dummy(password: &str) {
    static DUMMY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let dummy = DUMMY.get_or_init(|| hash("argon2-timing-equaliser").ok());
    match dummy {
        Some(h) => {
            let _ = verify(password, h);
        }
        // hash() 失敗（實務上不會發生）時不能靜默跳過：那會讓等時性
        // 在無人察覺的情況下失效，正是本函式要防的事。
        None => tracing::error!("無法產生假雜湊，登入的時間等化已失效"),
    }
}

/// 產生雜湊。呼叫者：測試與種子資料，以及
/// [`super::handlers::password_change`]（`POST /auth/password/change`）。
pub fn hash(password: &str) -> Result<String, Problem> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Problem::internal(std::io::Error::other(e.to_string())))
}

/// argon2 的並發上限。
///
/// # 為什麼 `spawn_blocking` 一個人不夠
///
/// 這是實測出來的，不是推導的。先只加 `spawn_blocking` 之後重量一次，
/// 穩態的 p99 從 1,717 ms 只降到 1,576 ms（8%）—— 幾乎沒動。
///
/// 原因：tokio 的 blocking 池預設 **512 條執行緒**，而機器只有 10 核。
/// 250 個 argon2 任務各拿到一條執行緒，然後**一起搶那 10 個核**。
/// 瓶頸從「tokio worker 被佔住」變成「CPU 被佔滿」，而 async worker 同樣
/// 拿不到 CPU —— 只是換成被 OS 排程器餓死而不是被 tokio 餓死。
///
/// `spawn_blocking` 解決的是「runtime 完全無法前進」，
/// 但 CPU 飽和要用**入場管制**解決。
///
/// # 為什麼是核數的一半
///
/// 留一半的核給其他請求。登入因此排隊（第 N 個人等得比較久），
/// 而**不相關的請求不受影響** —— 那是正確的取捨：
/// 一個人等 2 秒登入好過所有人的每一個請求都變慢。
///
/// 這與連線池的 10 是同一個道理（見 `docs/perf-baseline.md`）：
/// 有界的資源就是入場管制，而入場管制保護的是尾端延遲。
///
/// 可用 `ARGON2_MAX_CONCURRENCY` 覆寫。核數取不到時退回 2 ——
/// 不是 1：單一 slot 會讓登入完全序列化。
fn argon2_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| {
        let n = std::env::var("ARGON2_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|p| (p.get() / 2).max(1))
                    .unwrap_or(2)
            });
        tracing::info!(argon2_max_concurrency = n, "argon2 並發上限");
        tokio::sync::Semaphore::new(n)
    })
}

/// 在並發上限之內跑一段 argon2。
///
/// permit 在 `spawn_blocking` **完成之後**才釋放 —— 在之前釋放等於沒有上限。
async fn bounded<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let permit = argon2_slots()
        .acquire()
        .await
        .map_err(|e| format!("argon2 semaphore closed: {e}"))?;
    let out = tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string());
    drop(permit);
    out
}

/// [`verify`] 的非阻塞包裝。**登入路徑一律用這一支。**
///
/// # 為什麼必須換執行緒
///
/// argon2id（m=19456, t=2）是**數十毫秒的純 CPU**，而那是它的意義所在。
/// 但直接在 async handler 裡呼叫會佔住一條 tokio worker 執行緒，
/// 而 worker 的數量等於 CPU 核數 —— 也就是說 N 個同時登入會讓
/// **整個行程沒有任何 worker 在處理別的請求**。
///
/// 這不是理論。2026-08-04 在 10 核機器上實測（`docs/perf-baseline.md`
/// 的「發現一」）：一邊跑 250 人的穩態負載、一邊注入三次「250 人同時登入」，
/// 八個操作的 p99 從 500 ms 齊一地跳到 1,716–1,745 ms ——
/// 連 `GET /facilities/{id}/occupancy` 這種最輕的讀取也一樣。
///
/// **齊一的懲罰是關鍵證據**：若原因是各自的查詢變慢，不同操作受到的影響
/// 會不一樣。一模一樣的懲罰只可能來自一個共用資源被佔住。
///
/// 真實情境是「早上八點 250 個人同時打卡登入」，而那一刻整個系統的
/// p99 變成 3.4 倍。
///
/// # 為什麼是 `spawn_blocking` 而不是降低 argon2 成本
///
/// 降低成本會讓離線破解變便宜 —— 那是拿安全性換延遲。
/// 要改的是**它跑在哪個執行緒池**，不是它花多久。
///
/// # 失敗時回 `false`
///
/// `JoinError` 只在 blocking 池 panic 或被關閉時發生。回 `false`
/// （認證失敗）是唯一安全的方向 —— 但要記一筆 error，否則一個壞掉的
/// blocking 池會表現成「所有人的密碼都錯了」而沒有任何線索。
pub async fn verify_async(password: String, phc_hash: String) -> bool {
    match bounded(move || verify(&password, &phc_hash)).await {
        Ok(ok) => ok,
        Err(e) => {
            tracing::error!(error = %e, "argon2 驗證的 blocking 任務失敗，一律視為認證失敗");
            false
        }
    }
}

/// [`verify_dummy`] 的非阻塞包裝。理由與 [`verify_async`] 相同。
///
/// **等時性仍然成立**：它照樣跑一次完整的 argon2，只是在別的執行緒上。
/// 換執行緒不會讓它變快 —— 而那正是這支函式存在的理由（見 [`verify_dummy`]）。
pub async fn verify_dummy_async(password: String) {
    if let Err(e) = bounded(move || verify_dummy(&password)).await {
        // 這一支沒有回傳值，因此失敗時**等時性已經失效** ——
        // 那是一個安全性退化，必須記下來。
        tracing::error!(error = %e, "假雜湊的 blocking 任務失敗，登入的時間等化已失效");
    }
}

/// [`hash`] 的非阻塞包裝。理由與 [`verify_async`] 相同 ——
/// 產生雜湊與驗證雜湊的 CPU 成本是同一個量級。
///
/// 呼叫者是 `POST /auth/password/change`。改密碼不是熱路徑，但
/// 「租戶強制全體改密碼」是一個真實的尖峰，而那時的形狀與登入風暴一樣。
pub async fn hash_async(password: String) -> Result<String, Problem> {
    match bounded(move || hash(&password)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "argon2 雜湊的 blocking 任務失敗");
            Err(Problem::internal(std::io::Error::other(e.to_string())))
        }
    }
}
