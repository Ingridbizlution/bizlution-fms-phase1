//! 認證用的短命列的清理：
//!
//!   * refresh token 撤銷黑名單（070 的 `fms.purge_expired_refresh_revocations`）
//!   * SSO 授權請求（073 的 `fms.purge_expired_sso_requests`）
//!
//! # 為什麼兩件事在同一個迴圈裡
//!
//! 兩者是同一件事的兩個實例：一張表裡有些列的 `expires_at` 已經過去，
//! 而消耗它們的條件都含 `expires_at`，因此那些列**再也不可能被使用**。
//! 兩者都需要平台情境、都沒有時效性、都是一天一次。
//!
//! 分成兩個迴圈只會多一份要維護的排程，而且更容易讓下一個加清理函式的人
//! 以為要再開第三個。073 的清理函式在 #47 合併後**沒有任何呼叫者**（是在
//! 盤查「宣告了但沒有人讀」時發現的）—— 那正是「每個清理各自一個迴圈」
//! 這種結構會產生的漏接。
//!
//! # 為什麼需要它
//!
//! `revoked_refresh_tokens` 每次**換發**都會多一列（070 的 ROTATED，見該檔
//! 檔頭：少了它 logout 只殺得掉換發鏈上最後一個 token）。一個每 15 分鐘換發
//! 一次的客戶端一天產生約 96 列，而列在 token 過期之後就沒有作用了。
//!
//! 沒有這個迴圈，那張表**只增不減** —— 不會壞掉，但它會變成整個 schema 裡
//! 成長最快的表，而它守的東西只有 7 天的意義。
//!
//! # 為什麼在 fms-worker 而不是 fms-identity
//!
//! 與 [`crate::partitions`] 同一個判斷：這件事沒有領域。它關心的是「一張表
//! 裡有沒有已經沒有作用的列」，與身分規則無關。而且它需要**平台情境**
//! （跨租戶刪除），那是 fms_owner 的連線，不是端點拿得到的東西 ——
//! 070 刻意沒有把 EXECUTE 給 fms_app。
//!
//! # 刪掉為什麼是安全的
//!
//! `jwt::verify` 在查黑名單**之前**就擋掉過期的 token，因此一列的
//! `expires_at` 過了之後，它守的那個 token 再也走不到黑名單檢查 ——
//! 刪掉不會讓任何東西復活。完整的論證在 070 檔頭。

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

pub struct TokenPurger {
    /// 必須是 `fms_owner`：070 的 EXECUTE 只給了它，而且刪除需要平台情境。
    pool: PgPool,
}

impl TokenPurger {
    pub fn new(pool: PgPool) -> Arc<Self> {
        Arc::new(Self { pool })
    }

    /// 刪掉已經不可能再被使用的列，回傳 `(撤銷紀錄, SSO 請求)` 的筆數。幂等。
    ///
    /// 沒有參數是刻意的：要刪哪些列由「它自己過期了沒有」決定，那是
    /// `expires_at` 這一欄的事實，不是一個可以調的旋鈕。開一個「保留天數」
    /// 參數只會讓人以為多留幾天有安全上的好處 —— 沒有，見模組說明。
    ///
    /// 兩個 DELETE 在**同一個交易**裡：它們之間沒有依賴，但一個交易少一次
    /// 往返，而且「這一輪清理成功了嗎」變成一個答案而不是兩個。
    pub async fn run_once(&self) -> Result<PurgeCounts, sqlx::Error> {
        let mut tx = crate::begin_platform_tx(&self.pool).await?;
        let revocations: i64 = sqlx::query_scalar("SELECT fms.purge_expired_refresh_revocations()")
            .fetch_one(&mut *tx)
            .await?;
        let sso_requests: i64 = sqlx::query_scalar("SELECT fms.purge_expired_sso_requests()")
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(PurgeCounts {
            revocations,
            sso_requests,
        })
    }
}

/// 一輪清理刪掉的列數。
///
/// 兩個數字**分開回報**而不是加總：加總之後「SSO 請求從來沒有被清掉過」
/// 這件事就看不出來了，而那正是本次要修的缺陷的症狀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgeCounts {
    pub revocations: i64,
    pub sso_requests: i64,
}

impl PurgeCounts {
    fn total(&self) -> i64 {
        self.revocations + self.sso_requests
    }
}

/// 執行間隔。
///
/// 一天一次。這件事沒有時效性 —— 晚一天刪不會有任何後果（過期的列已經
/// 不起作用），而頻繁掃描一張以 `expires_at` 建了索引的表只是白費。
pub struct TokenPurgeConfig {
    pub interval: Duration,
}

impl Default for TokenPurgeConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// 迴圈。與 [`crate::partitions`] 一樣啟動時立刻跑一次：如果這個迴圈曾經
/// 停過一段時間，重新上線的第一件事就該是把積壓的列清掉。
pub async fn run_until_shutdown(
    purger: Arc<TokenPurger>,
    cfg: TokenPurgeConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            tracing::info!("認證列清理迴圈收到關機訊號");
            return;
        }
        match purger.run_once().await {
            Ok(c) if c.total() == 0 => tracing::debug!("認證列清理：沒有過期的列"),
            // 兩個數字分別記在 log 裡。合成一個總數的話，「SSO 請求那一半
            // 從來沒有被清掉」在 log 上看起來與「本來就沒有過期的」一樣。
            Ok(c) => tracing::info!(
                revocations = c.revocations,
                sso_requests = c.sso_requests,
                "已清掉過期的認證列"
            ),
            // 失敗只是 warn：積壓的後果是磁碟，不是正確性。**撤銷與 state
            // 的一次性都不會因此失效** —— 多留的列只是多守著已經沒用的東西。
            Err(e) => tracing::warn!(error = %e, "認證列清理失敗，下一輪重試"),
        }
        tokio::select! {
            _ = tokio::time::sleep(cfg.interval) => {}
            _ = shutdown.changed() => {}
        }
    }
}
