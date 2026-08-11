//! JWT 簽發與驗證。
//!
//! Claims 依規格書 §4.2：「至少含 sub、tid、scope、exp」。
//!
//! `scope` 刻意只放粗粒度標記，**不放展開後的權限清單**。理由與
//! ADR-09 實作紀律 2 一致：權限的真實來源是
//! `fms.user_has_permission()` 與 `user_role_assignments`。若把權限寫進
//! token，撤權要等到 token 過期才生效，且 token 會成為權限模型的第二份副本。

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

use fms_shared::Problem;

/// 區分 access 與 refresh，避免其中一種被當成另一種使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenType {
    Access,
    Refresh,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// user_id
    pub sub: Uuid,
    /// tenant_id
    pub tid: Uuid,
    /// 粗粒度授權範圍（見模組說明）
    pub scope: String,
    pub typ: TokenType,
    pub iat: i64,
    pub exp: i64,
    /// token 的識別碼。**只有 refresh token 有**，見 [`issue`]。
    ///
    /// `Option` 不是為了「可有可無」，而是因為 access token 沒有它，
    /// 以及 070 之前簽出的 refresh token 也沒有。refresh 路徑遇到 `None`
    /// 一律拒絕（見 [`Claims::refresh_jti`]）—— 一個撤銷不了的 refresh token
    /// 比一次重新登入危險得多。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<Uuid>,
}

impl Claims {
    /// 取出 refresh token 的 jti，`None` 視為錯誤。
    ///
    /// 070 引入撤銷黑名單之後，沒有 jti 的 refresh token 是**撤銷不了的**：
    /// 黑名單以 jti 為鍵，沒有 jti 就沒有可以寫進去的東西，於是 logout 對它
    /// 只能靜默地無效。放行這種 token 會讓「已登出」與「還能換發」同時成立。
    ///
    /// 因此這裡 fail-closed。代價是 070 部署的那一刻，手上還握著舊 refresh
    /// token 的客戶端要重新登入一次 —— 一次性的，而且 access token 不受影響
    /// （它本來就沒有 jti，也不走這條路徑）。
    pub fn refresh_jti(&self) -> Result<Uuid, Problem> {
        self.jti.ok_or_else(|| {
            Problem::unauthenticated("refresh token 沒有 jti（於撤銷機制上線前簽發），請重新登入")
        })
    }
}

/// 簽發一組 token。回傳 `(token, 有效秒數)`。
///
/// # 為什麼只有 refresh token 有 jti
///
/// jti 的用途是撤銷（070 的 `revoked_refresh_tokens`）。access token 刻意不做
/// 成可撤銷的：要撤銷它就得在 [`super::handlers::require_auth`]（每一個請求都會
/// 過）裡加一次資料庫查詢，把整個 API 的認證從驗簽章變成查表，換來的只是把
/// 15 分鐘的窗縮短。給 access token 一個沒有人讀的 jti 只會讓下一個讀這段
/// 程式的人以為它可以撤銷。
pub fn issue(
    secret: &str,
    user_id: Uuid,
    tenant_id: Uuid,
    typ: TokenType,
    ttl: Duration,
) -> Result<(String, i64), Problem> {
    let now = chrono::Utc::now().timestamp();
    let ttl_secs = ttl.as_secs() as i64;
    let claims = Claims {
        sub: user_id,
        tid: tenant_id,
        scope: "api".to_string(),
        typ,
        iat: now,
        exp: now + ttl_secs,
        jti: match typ {
            TokenType::Refresh => Some(Uuid::new_v4()),
            TokenType::Access => None,
        },
    };
    let token = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(Problem::internal)?;
    Ok((token, ttl_secs))
}

/// 驗證並解出 claims，同時檢查 token 種類相符。
pub fn verify(secret: &str, token: &str, expected: TokenType) -> Result<Claims, Problem> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::new(Algorithm::HS256),
    )
    .map_err(|_| Problem::unauthenticated("invalid or expired token"))?;

    if data.claims.typ != expected {
        return Err(Problem::unauthenticated(
            "token is not of the expected type",
        ));
    }
    Ok(data.claims)
}
