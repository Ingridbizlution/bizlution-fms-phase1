//! OIDC 授權碼流程（`/auth/sso/{providerCode}/authorize` 與 `/callback`）。
//!
//! # 這一刀誠實的邊界
//!
//! **`/authorize` 完整可用。** 它產生 state／nonce／PKCE、存進 073 的
//! `sso_auth_requests`、抓 IdP 的 discovery 文件取得 `authorization_endpoint`、
//! 組出授權網址。前端今天就可以拿它做跳轉。
//!
//! **`/callback` 只完成前半。** 它驗證並一次性消耗 state（那是 CSRF 防護所在，
//! 也是這條流程裡最容易寫錯的部分），然後**停在 token 交換之前**並回 501，
//! 因為那一步缺一樣 Phase 1 沒有的東西：
//!
//! **token 交換與 id_token 驗證。** 簽章要用 IdP 的 JWKS 驗，而沒有真實 IdP
//! 可以產生一張真的 id_token 來驗證那段程式碼。一段「沒驗簽就核發身分」的
//! 程式碼寫出來也沒有辦法證明它是對的。
//!
//! `client_secret` **不再是缺口**：`fms_shared::secrets` 的解析器已經能把
//! `client_secret_ref` 換成值，而它解不解得開由
//! `POST /identity-providers/{id}/test-connection` 的
//! `secret_reference_resolvable` 那一格回答。剩下的一個缺口是上面那個。
//!
//! 回 501 而不是 500 或假的成功：**一個回 200 並發出 token 的 callback 會是
//! 這個系統裡最危險的一段程式碼** —— 它等於在沒有驗證任何東西的情況下核發身分。
//!
//! 回應的 `remaining_steps` 逐條列出缺什麼，形狀比照 `test_connection` 的
//! `checks_not_performed`。
//!
//! # `redirect_uri` 絕不來自請求
//!
//! 它由部署設定的 `PUBLIC_BASE_URL` 組出來。接受呼叫端給的值就是一個開放
//! 轉址器：攻擊者把它指向自己的網站，IdP 就會把授權碼送到那裡去。
//! 未設定 `PUBLIC_BASE_URL` 時回 501 並說明要設什麼，不猜。
//!
//! # `/authorize` 為什麼需要 `tenant_code`
//!
//! 契約的路徑只有 `{providerCode}`，而 002 的唯一鍵是
//! `(tenant_id, lower(code))` —— **provider code 只在租戶內唯一**。
//! 沒有租戶判別就定位不到 provider，而「猜第一個符合的」會讓 A 租戶的登入
//! 跳到 B 租戶的 IdP。
//!
//! 因此 `/authorize` 要 `?tenant_code=`，走 014 的 `resolve_tenant_by_code`
//! （與 password grant 同一個機制）。`/callback` 不需要 —— `state` 那一列
//! already 帶著 tenant_id。

use axum::extract::{Path, Query, State};
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use fms_shared::{Problem, ProblemCode};

use crate::handlers::IdentityState;
use crate::repo;

/// state／nonce／PKCE verifier 的長度來源。
///
/// 兩個 UUIDv4 的隨機位元（各 122 bit）以 hex 表示 —— 與
/// `fms_notification::webhooks` 產生簽章金鑰同一個做法，理由也相同：
/// `uuid` 的 v4 走作業系統的 CSPRNG，不必為此引入一個 RNG crate。
fn random_token() -> String {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    a.as_bytes()
        .iter()
        .chain(b.as_bytes().iter())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// PKCE 的 S256 challenge：`base64url(SHA256(verifier))`，不帶 padding。
///
/// **不帶 padding 是規格要求**（RFC 7636 §4.2 指定 base64url without padding）。
/// 帶了 `=` 的 challenge 會被嚴格的 IdP 拒絕，而症狀是使用者跳到 IdP 之後
/// 看到一個我們這邊看不到的錯誤。
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    /// **必填。** provider code 只在租戶內唯一，見模組說明。
    pub tenant_code: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuthorizeResponse {
    pub authorize_url: String,
    /// 回傳 state 讓前端可以在 callback 之後比對自己發起的那一次
    /// （伺服器端也會驗，這是給前端的便利，不是安全機制）。
    pub state: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub redirect_uri: String,
}

/// `GET /auth/sso/{providerCode}/authorize`
///
/// 不需認證（使用者還沒登入）。
///
/// # 回 200 + `authorize_url`，不回 302
///
/// 契約寫「回傳 302 或 authorize_url」。選 200 的理由：這是一個 JSON API，
/// 而呼叫它的是 SPA —— 一個 302 在 `fetch()` 裡會被自動跟隨，於是瀏覽器對
/// IdP 發出一個 XHR（而不是導航），結果是 CORS 錯誤而不是登入畫面。
/// 前端拿到網址自己 `location.assign()` 才是可行的做法。
pub async fn authorize(
    State(state): State<IdentityState>,
    Path(provider_code): Path<String>,
    Query(q): Query<AuthorizeQuery>,
) -> Result<Json<AuthorizeResponse>, Problem> {
    let tenant_code = q
        .tenant_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Problem::validation(
                "tenant_code 為必填 —— provider code 只在租戶內唯一（002 的 \
                 uq_identity_providers_code），沒有它無法定位身分來源",
            )
        })?;

    // redirect_uri 必須是伺服器端的事實，見模組說明。
    let base = state.settings.public_base_url.as_deref().ok_or_else(|| {
        Problem::new(ProblemCode::NotImplemented).with_detail(
            "這個部署沒有設定 PUBLIC_BASE_URL，因此組不出 SSO 的 redirect_uri。\
             那個值不能來自請求（會變成開放轉址器），也不猜（猜出來的值 IdP 那邊\
             沒有註冊，症狀會出現在 IdP 那一側）",
        )
    })?;
    let redirect_uri = format!("{base}/api/v1/auth/sso/{provider_code}/callback");

    // 尚無 RLS 情境（使用者未登入），因此經 014 的 SECURITY DEFINER 函式解析租戶
    // —— 與 password grant 同一條路徑。
    let tenant_id = repo::resolve_tenant_by_code(&state.pool, tenant_code)
        .await?
        .ok_or_else(|| Problem::not_found("tenant_code 找不到對應的租戶"))?;

    let provider = repo::load_sso_provider(&state.pool, tenant_id, &provider_code).await?;

    // 只有 OIDC 走這條流程。SAML2 的 metadata 是密鑰服務的參照、沒有解析器，
    // 而且 SAML 的斷言驗證是另一套機制 —— 假裝支援會回一個永遠失敗的網址。
    if provider.provider_type != "OIDC" {
        return Err(
            Problem::new(ProblemCode::NotImplemented).with_detail(format!(
                "身分來源 `{provider_code}` 的型別是 {} —— 目前只實作 OIDC 的授權碼流程。\
             SAML2 需要 metadata 解析與斷言簽章驗證，兩者都需要 Phase 1 沒有的\
             密鑰解析器",
                provider.provider_type
            )),
        );
    }
    if provider.status != "ACTIVE" {
        return Err(Problem::new(ProblemCode::Conflict).with_detail(format!(
            "身分來源 `{provider_code}` 目前是 {} —— 不是 ACTIVE 的來源不該接受登入",
            provider.status
        )));
    }
    let client_id = provider.client_id.as_deref().ok_or_else(|| {
        // 002 的 ck_idp_oidc_fields 保證 OIDC 一定有 client_id，所以這裡到不了。
        // 不 unwrap：一條 CHECK 日後被放寬時，這裡要回一個說得清楚的錯誤，
        // 而不是 panic。
        Problem::validation("這個 OIDC 來源沒有 client_id")
    })?;

    // 從 discovery 文件取 authorization_endpoint。
    //
    // **不從 issuer 拼出來**：OIDC 允許授權端點在任何路徑上，而 discovery
    // 文件就是為此存在的。硬拼 `{issuer}/authorize` 對多數 IdP 是錯的。
    let discovery_url = provider
        .discovery_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .or_else(|| {
            provider.issuer.as_deref().map(|iss| {
                format!(
                    "{}/.well-known/openid-configuration",
                    iss.trim_end_matches('/')
                )
            })
        })
        .ok_or_else(|| Problem::validation("這個 OIDC 來源既沒有 discovery_url 也沒有 issuer"))?;

    let authorization_endpoint =
        fetch_authorization_endpoint(&discovery_url, &state.settings.outbound).await?;

    let state_token = random_token();
    let nonce = random_token();
    let verifier = random_token();
    let challenge = pkce_challenge(&verifier);
    // 10 分鐘：使用者在 IdP 上輸入密碼與 MFA 需要時間，但一個放了一小時的
    // 授權請求沒有正當用途。
    let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);

    repo::insert_sso_request(
        &state.pool,
        tenant_id,
        provider.id,
        &state_token,
        &nonce,
        &verifier,
        &redirect_uri,
        expires_at,
    )
    .await?;

    // `scope` 固定 `openid profile email`：那是把使用者對應到 `fms.users` 所需
    // 的最小集合（sub、name、email）。不做成可設定的 —— 一個少了 `openid` 的
    // scope 會讓 IdP 回一個沒有 id_token 的回應，而那是純粹的組態陷阱。
    let authorize_url = format!(
        "{}{}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}\
         &code_challenge={}&code_challenge_method=S256",
        authorization_endpoint,
        if authorization_endpoint.contains('?') {
            "&"
        } else {
            "?"
        },
        urlencode(client_id),
        urlencode(&redirect_uri),
        urlencode("openid profile email"),
        urlencode(&state_token),
        urlencode(&nonce),
        urlencode(&challenge),
    );

    Ok(Json(AuthorizeResponse {
        authorize_url,
        state: state_token,
        expires_at,
        redirect_uri,
    }))
}

/// 抓 discovery 文件並取出 `authorization_endpoint`。
///
/// 走 `fms_shared::safe_http` 的閘門（強制 https、逐一檢查解析出的位址、
/// pin 住檢查過的 IP、不跟隨轉址）。
///
/// **這支端點不需認證，因此它是一個出站請求的放大器** —— 任何人都能讓伺服器
/// 去抓那份文件。可接受的理由：目標網址來自**儲存的設定**，不是請求，因此
/// 目標集合被已設定的 provider 數量限制住。要進一步收斂應該加快取，
/// 那是一個獨立的決定。
async fn fetch_authorization_endpoint(
    discovery_url: &str,
    outbound: &fms_shared::OutboundSettings,
) -> Result<String, Problem> {
    let checked = fms_shared::safe_http::resolve_and_check(discovery_url, outbound)
        .await
        .map_err(|rejection| {
            Problem::validation(format!("身分來源的 discovery_url 不可用：{rejection}"))
        })?;
    let (status, body) = fms_shared::safe_http::get_capped(&checked, outbound)
        .await
        .map_err(|e| {
            Problem::new(ProblemCode::Conflict)
                .with_detail(format!("取不到身分來源的 discovery 文件：{e}"))
        })?;
    if !status.is_success() {
        return Err(Problem::new(ProblemCode::Conflict)
            .with_detail(format!("身分來源的 discovery 文件回 HTTP {status}")));
    }
    let doc: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        Problem::new(ProblemCode::Conflict)
            .with_detail(format!("身分來源的 discovery 文件不是合法 JSON：{e}"))
    })?;
    doc.get("authorization_endpoint")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            Problem::new(ProblemCode::Conflict).with_detail(
                "discovery 文件沒有 authorization_endpoint —— \
                 那個欄位是 OIDC Discovery 的必要項",
            )
        })
}

/// 最小的百分號編碼。
///
/// 只處理查詢字串裡真的會出問題的字元。不引入 `urlencoding` crate：
/// 需要編碼的輸入只有 client_id、我們自己產生的 hex token、以及一個固定的
/// scope 字串 —— 範圍窄到不值得一個依賴。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    /// IdP 在使用者拒絕授權時回這個，而不是 code。
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// `GET /auth/sso/{providerCode}/callback`
///
/// 不需認證。**前半完整實作、後半回 501**，見模組說明。
///
/// # 為什麼先消耗 state 才回 501
///
/// state 的驗證與一次性消耗是這條流程裡的安全核心（CSRF 與重放防護）。
/// 把它做完並測到，比整支端點直接回 501 有價值得多 —— 後者會讓那套機制
/// 在接上真實 IdP 的那一天才第一次被執行。
///
/// 副作用：一次失敗的 callback 會把 state 用掉，使用者必須重新發起登入。
/// 那對一次性授權碼來說本來就是正確的行為。
pub async fn callback(
    State(state): State<IdentityState>,
    Path(provider_code): Path<String>,
    Query(q): Query<CallbackQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let state_token = q
        .state
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Problem::validation(
                "缺少 state —— 沒有它無法確認這次回跳對應到哪一次登入嘗試，\
                 而那正是 CSRF 防護所在",
            )
        })?;

    // 先消耗 state，即使 IdP 回的是錯誤：那個 state 已經曝光在瀏覽器的
    // 位址欄裡，不該還能再用一次。
    let consumed = repo::consume_sso_state(&state.pool, state_token).await?;

    match consumed.outcome.as_str() {
        "CONSUMED" => {}
        // **這三種分開回報。** 合併成一個「無效的 state」會讓重放
        // （可能的攻擊）與「使用者在 IdP 上待太久」在日誌裡長得一樣。
        "ALREADY_USED" => {
            return Err(Problem::new(ProblemCode::Conflict).with_detail(
                "這個 state 已經使用過了。若不是你重新載入了回跳頁面，\
                 這可能是一次重放 —— 請重新發起登入",
            ))
        }
        "EXPIRED" => {
            return Err(Problem::new(ProblemCode::Conflict)
                .with_detail("這次登入嘗試已過期（授權請求的有效期是 10 分鐘）—— 請重新發起登入"))
        }
        _ => {
            return Err(Problem::unauthenticated(
                "state 無效 —— 找不到對應的登入嘗試",
            ))
        }
    }

    // IdP 回的是錯誤而不是授權碼（使用者按了拒絕、或 IdP 端的組態問題）。
    if let Some(err) = q.error.as_deref() {
        return Err(Problem::unauthenticated(format!(
            "身分來源拒絕了這次授權：{err}{}",
            q.error_description
                .as_deref()
                .map(|d| format!("（{d}）"))
                .unwrap_or_default()
        )));
    }

    let code = q
        .code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Problem::validation("既沒有 code 也沒有 error —— 這不是一個合法的 OIDC 回跳")
        })?;

    // ---- 到這裡為止全部完成且驗證過。以下是做不到的部分。 ----
    //
    // **不回 200，也不核發任何 token。** 一個在沒有驗證 id_token 的情況下
    // 核發 session 的 callback 會是這個系統裡最危險的一段程式碼：
    // 任何人只要拿到一個 state 就能變成任何人。
    Err(
        Problem::new(ProblemCode::NotImplemented).with_detail(format!(
            "state 已驗證並消耗（CSRF 防護生效），但無法完成登入。\
         缺的是向 `{provider_code}` 的 token 端點換 token、並用 IdP 的 JWKS 驗證\
         id_token 簽章這一段 —— 沒有真實 IdP 可以產生一張真的 id_token，\
         那段程式碼寫出來也無法證明它是對的。\
         client_secret 已經**不是**缺口：密鑰參照可以解析，\
         解不解得開請看 POST /identity-providers/{{id}}/test-connection 的 \
         secret_reference_resolvable。\
         收到的授權碼長度 {} 字元，已丟棄（沒有被送到任何地方）。",
            code.len()
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PKCE 的 S256 固定向量 —— 直接取自 **RFC 7636 附錄 B**。
    ///
    /// 用規格書的向量而不是自己算出來的值：它同時證明演算法對、
    /// 而且編碼方式對（base64url、**不帶 padding**）。
    /// 帶了 `=` 的 challenge 會被嚴格的 IdP 拒絕，而症狀出現在 IdP 那一側 ——
    /// 我們這邊看不到。
    #[test]
    fn pkce_s256_matches_the_rfc_7636_test_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    /// challenge 不帶 padding，而且是 URL 安全的字元集。
    #[test]
    fn challenge_is_url_safe_and_unpadded() {
        for v in [
            "a",
            "verifier-with-lots-of-entropy-0123456789",
            &"x".repeat(128),
        ] {
            let c = pkce_challenge(v);
            assert!(!c.contains('='), "帶了 padding：{c}");
            assert!(
                !c.contains('+') && !c.contains('/'),
                "不是 URL 安全字元集：{c}"
            );
            // SHA-256 是 32 位元組 → base64（無 padding）是 43 個字元。
            assert_eq!(c.len(), 43, "{c}");
        }
    }

    /// 查詢字串的編碼：空白與 `:`／`/` 都必須被編碼。
    ///
    /// 少了它，`scope=openid profile email` 裡的空白會把授權網址切斷，
    /// 而 IdP 收到的 scope 只有 `openid`。
    #[test]
    fn urlencode_escapes_what_breaks_a_query_string() {
        assert_eq!(
            urlencode("openid profile email"),
            "openid%20profile%20email"
        );
        assert_eq!(
            urlencode("https://fms.example.com/cb"),
            "https%3A%2F%2Ffms.example.com%2Fcb"
        );
        // 未保留字元原樣通過（否則網址會變得沒必要地難讀）。
        assert_eq!(urlencode("aZ09-_.~"), "aZ09-_.~");
    }

    /// 每次呼叫都不同 —— state／nonce／verifier 若可預測，三道防護全部失效。
    #[test]
    fn random_tokens_are_distinct_and_long_enough() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        // 兩個 UUID → 32 位元組 → 64 個十六進位字元。
        assert_eq!(a.len(), 64, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
