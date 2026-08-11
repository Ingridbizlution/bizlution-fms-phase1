//! 樂觀鎖（`If-Match` / `version`）與冪等鍵（`Idempotency-Key`）。
//!
//! 兩者都是規格書 §4.3 的標頭規範，且都由既有的資料庫結構支撐
//! （`reservations.version` 由 `trg_bump_version` 維護；
//! `fms.idempotency_keys` 已含 state／request_hash／expires_at）。
//! 應用層只負責轉譯 HTTP 語意，不重新發明機制。

use axum::http::{HeaderMap, StatusCode};
use uuid::Uuid;

use crate::db::{Authorized, TenantTx};
use crate::problem::{Problem, ProblemCode};

// ---------------------------------------------------------------------------
// 樂觀鎖
// ---------------------------------------------------------------------------

/// 解析 `If-Match` 標頭為 `version`。
///
/// 契約標明 `If-Match` 為 required，缺少回 428（規格書 §4.3），
/// 不符回 412。428 刻意與 400 區分：呼叫端只要補上標頭重送即可。
pub fn required_if_match(headers: &HeaderMap) -> Result<i32, Problem> {
    let raw = headers
        .get(axum::http::header::IF_MATCH)
        .ok_or_else(|| {
            Problem::new(ProblemCode::PreconditionRequired)
                .with_detail("If-Match header is required for this operation")
        })?
        .to_str()
        .map_err(|_| Problem::bad_request("If-Match is not valid ASCII"))?;

    // 容忍 ETag 的引號寫法（`"3"`）與裸值（`3`）
    raw.trim()
        .trim_matches('"')
        .parse::<i32>()
        .map_err(|_| Problem::bad_request("If-Match must be the resource's current version"))
}

/// 解析選用的 `If-Match`。缺少回 `None`（不是 428）。
///
/// 給契約把 `If-Match` 列為 parameters 但非 required 的端點用
/// （例如工單的 transitions）。帶了就必須檢查 —— 客戶端帶上標頭是為了
/// 表達「我以為版本是 N」，收下卻不比對等於默默取消它的並行保護。
pub fn optional_if_match(headers: &HeaderMap) -> Result<Option<i32>, Problem> {
    if headers.get(axum::http::header::IF_MATCH).is_none() {
        return Ok(None);
    }
    required_if_match(headers).map(Some)
}

/// 比對版本。不符即 412 `STALE_VERSION`。
///
/// # 呼叫前必須先鎖住那一列
///
/// 這個函式只比較兩個數字 —— 它**無法**提供原子性。若 `actual` 來自一次沒有
/// 加鎖的 `SELECT`，兩個並發的 PATCH 會讀到同一個版本、都通過這裡、都寫入，
/// 而後寫的默默覆蓋前一個（lost update）。**兩邊都收到 200**，沒有錯誤、
/// 沒有日誌，只有一筆消失的修改。
///
/// 因此讀取 `actual` 之前要先 `SELECT … FOR UPDATE` 那一列
/// （各 repo 的 `lock()`）。鎖持有到交易結束，於是第二個請求會**等**，
/// 等到之後讀到的是已經遞增的版本，比對正確失敗。
///
/// 這不是理論上的顧慮：`concurrency_correctness_slice.rs` 的 `d_` 在加鎖之前
/// 實測 6 路併發中**有 2 路成功**。
pub fn check_version(expected: i32, actual: i32) -> Result<(), Problem> {
    if expected == actual {
        Ok(())
    } else {
        Err(Problem::new(ProblemCode::StaleVersion).with_detail(format!(
            "resource has been modified (expected version {expected}, current {actual})"
        )))
    }
}

// ---------------------------------------------------------------------------
// 冪等
// ---------------------------------------------------------------------------

/// 已完成的回應，但**還不能回傳**。
///
/// # 為什麼不是直接給 `(status, body)`
///
/// 原本的實作在命中鍵時立刻回放並 `return`，也就是**跳過了授權檢查** ——
/// handler 的 `require_permission` 寫在回放之後。這個型別把「回放前必須先
/// 授權」變成編譯期的事：唯一取出內容的方法是 [`PendingReplay::release`]，
/// 而它要求一個 [`Authorized`]，而 `Authorized` 只有
/// [`crate::db::require_permission`] 產得出來。
///
/// 光靠 025 把 user_id 納入主鍵已經擋掉「別人的鍵」這條路
/// （查不到列就不會有回放）。這一層要擋的是剩下的那種：**同一個使用者**
/// 在 24 小時的窗內權限被撤銷後重送。那次回放洩漏的內容他本來就看過，
/// 所以嚴重度低 —— 但成本也低，`permission_codes` 有請求層級的記憶，
/// 因此 handler 已經做過的判定不會再往返資料庫一次。
#[must_use = "回放內容必須經 release 取出，直接丟掉會讓客戶端拿不到已完成的結果"]
pub struct PendingReplay {
    status: u16,
    body: serde_json::Value,
}

impl PendingReplay {
    /// 交出授權憑證以換取回放內容。
    pub fn release(self, _: Authorized) -> (StatusCode, serde_json::Value) {
        (
            // 存下來的狀態碼來自本服務自己的 handler，不會是非法值；
            // 真的壞了就退回 200 而不是 panic。
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::OK),
            self.body,
        )
    }
}

/// 冪等檢查的結果。
pub enum Idempotency {
    /// 首次見到此鍵，已登記為 `IN_FLIGHT`，呼叫端應繼續執行。
    Proceed,
    /// 同鍵且同 body 已完成。內容要等授權跑完才能取出，見 [`PendingReplay`]。
    Replay(PendingReplay),
}

impl Idempotency {
    /// 轉成 `Option`，方便 handler 在授權之後才決定要不要回放。
    ///
    /// 典型用法：
    /// ```ignore
    /// let pending = match key { Some(k) => begin(..).await?.pending(), None => None };
    /// let auth = require_permission(&mut tx, "asset:write", Some(facility_id), None).await?;
    /// if let Some(p) = pending {
    ///     let (code, body) = p.release(auth);
    ///     ...
    /// }
    /// ```
    pub fn pending(self) -> Option<PendingReplay> {
        match self {
            Self::Proceed => None,
            Self::Replay(p) => Some(p),
        }
    }
}

/// 以 `Idempotency-Key` 登記請求。
///
/// 三種情形依規格書 §4.3：
///   * 同鍵、同 body、已完成 → 回放首次結果
///   * 同鍵、進行中          → 409（`IDEMPOTENCY_IN_PROGRESS`）
///   * 同鍵、不同 body       → 422（`IDEMPOTENCY_KEY_REUSED`）
///
/// 登記與業務寫入在同一交易內，因此不會出現「鍵記下了但預約沒建立」。
///
/// # 鍵的範圍是 (租戶, 使用者, 鍵, 端點)
///
/// `user_id` 取自 `TenantTx` 的情境 —— 也就是 `require_auth` 交叉驗證過
/// JWT `sub` 之後的值，不是客戶端自稱的。呼叫端不需要（也不能）另外傳，
/// 這讓「綁錯使用者」在結構上不可能發生。
///
/// 025 之前主鍵不含 `user_id`，同租戶內任何人憑鍵就能取回別人的回應。
/// 納入主鍵之後，兩個使用者用同一個鍵字串會是兩列，彼此無關 ——
/// 刻意不回 `IDEMPOTENCY_KEY_REUSED`：鍵字串相同不是後來那個人的錯，
/// 而「這個鍵有人用過」本身也是不該外洩的資訊。
///
/// 注意 `state` 的合法值來自 `idempotency_keys_state_check`
/// （`IN_FLIGHT` / `COMPLETED` / `FAILED`）。`query!` 巨集會驗證欄位與型別，
/// 但**不會驗證 CHECK 約束的字串值** —— 這是 schema 採 `TEXT + CHECK`
/// 而非原生 ENUM 所留下的殘餘漂移風險，只能靠讀約束定義來確保一致。
pub async fn begin(
    tx: &mut TenantTx,
    key: &str,
    endpoint: &str,
    request_body: &serde_json::Value,
) -> Result<Idempotency, Problem> {
    let hash = request_hash(request_body);
    let ctx = tx.context();

    // 先查既有紀錄
    let existing = sqlx::query!(
        r#"SELECT request_hash::text AS "request_hash!",
                  state,
                  response_status,
                  response_body
           FROM fms.idempotency_keys
           WHERE tenant_id = $1 AND user_id = $2 AND idempotency_key = $3 AND endpoint = $4
             AND expires_at > now()"#,
        ctx.tenant_id,
        ctx.user_id,
        key,
        endpoint
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;

    if let Some(row) = existing {
        if row.request_hash.trim() != hash {
            return Err(Problem::new(ProblemCode::IdempotencyKeyReused).with_detail(
                "this Idempotency-Key was already used with a different request body",
            ));
        }
        return match row.state.as_str() {
            "COMPLETED" => Ok(Idempotency::Replay(PendingReplay {
                status: row.response_status.unwrap_or(200) as u16,
                body: row.response_body.unwrap_or(serde_json::Value::Null),
            })),
            _ => Err(Problem::new(ProblemCode::IdempotencyInProgress)
                .with_detail("a request with this Idempotency-Key is still in progress")),
        };
    }

    // **23505 在這裡有特定的語意，不是一般性衝突。**
    //
    // 兩個帶同一個鍵的請求同時抵達時，上面那次 SELECT 兩邊都看不到對方的列
    // （READ COMMITTED），於是兩邊都走到這個 INSERT，而主鍵讓其中一個失敗。
    //
    // 落敗者的處境**與上面 `IN_FLIGHT` 那一支完全相同** —— 有人先到、還在處理。
    // 交給 `Problem::from` 的話它會變成 `CONFLICT`「a conflicting record already
    // exists」，而那個訊息說不出「請用同一個鍵重試」，於是客戶端唯一合理的
    // 反應（重送）看起來像是錯的。
    //
    // 這個缺陷只在真的併發下出現：循序重送會命中上面的 SELECT 而正確回報。
    // `concurrency_correctness_slice.rs` 的 `c_` 是它的第一個讀者。
    let inserted = sqlx::query!(
        r#"INSERT INTO fms.idempotency_keys
             (tenant_id, user_id, idempotency_key, endpoint, request_hash, state, expires_at)
           VALUES ($1, $2, $3, $4, $5, 'IN_FLIGHT', now() + interval '24 hours')"#,
        ctx.tenant_id,
        ctx.user_id,
        key,
        endpoint,
        hash
    )
    .execute(tx.conn())
    .await;

    if let Err(sqlx::Error::Database(db)) = &inserted {
        if db.code().as_deref() == Some("23505") {
            return Err(Problem::new(ProblemCode::IdempotencyInProgress).with_detail(
                "a request with this Idempotency-Key is still in progress                  (registered concurrently) —— retry with the same key",
            ));
        }
    }
    inserted.map_err(Problem::from)?;

    Ok(Idempotency::Proceed)
}

/// 標記完成並保存回應，供 24 小時內的重放使用。
///
/// `WHERE` 必須與 [`begin`] 的 `INSERT` 完全對齊（含 `user_id`），
/// 否則 UPDATE 影響 0 列而回應永遠不會被存下 —— 症狀是「冪等鍵好像沒作用」，
/// 而且不會有任何錯誤。
pub async fn complete(
    tx: &mut TenantTx,
    key: &str,
    endpoint: &str,
    status: u16,
    body: &serde_json::Value,
) -> Result<(), Problem> {
    let ctx = tx.context();
    let updated = sqlx::query!(
        r#"UPDATE fms.idempotency_keys
           SET state = 'COMPLETED', response_status = $5, response_body = $6
           WHERE tenant_id = $1 AND user_id = $2 AND idempotency_key = $3 AND endpoint = $4"#,
        ctx.tenant_id,
        ctx.user_id,
        key,
        endpoint,
        status as i16,
        body
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;

    // 0 列表示 begin() 沒有登記成功，或兩邊的鍵條件漂移了。兩者都是我們的
    // bug，不是客戶端的問題，因此回 500 而不是靜默通過 ——
    // 靜默通過的後果是重試會重複執行，而那正是冪等鍵要防的事。
    if updated.rows_affected() == 0 {
        return Err(Problem::internal(std::io::Error::other(format!(
            "idempotency key was not registered before complete() (endpoint {endpoint})"
        ))));
    }
    Ok(())
}

/// 讀取 `Idempotency-Key`。契約標為選用，因此沒帶不是錯誤。
pub fn key_from(headers: &HeaderMap) -> Result<Option<String>, Problem> {
    match headers.get("idempotency-key") {
        None => Ok(None),
        Some(v) => {
            let s = v
                .to_str()
                .map_err(|_| Problem::bad_request("Idempotency-Key is not valid ASCII"))?;
            if s.len() > 120 {
                return Err(Problem::bad_request(
                    "Idempotency-Key exceeds 120 characters",
                ));
            }
            Ok(Some(s.to_owned()))
        }
    }
}

/// `request_hash` 是 `char(64)`，因此用 SHA-256 的十六進位表示。
fn request_hash(body: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_string(body).unwrap_or_default();
    let digest = Sha256::digest(canonical.as_bytes());
    format!("{digest:x}")
}

/// 型別佔位，避免呼叫端誤把 `Uuid` 當成鍵傳入。
pub type ReservationId = Uuid;
