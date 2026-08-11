//! 請求／回應型別。形狀對齊 `api/openapi.yaml`，不多不少。
//!
//! 契約方向不可反轉（ADR-09 實作紀律 1）：這些型別是為了符合手寫契約而存在。
//! 欄位若與契約不符，要改的是這裡，不是契約。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `TokenRequest`。
///
/// 契約允許四種 grant_type，本切片只實作 `password` 與 `refresh_token`；
/// `authorization_code`（OIDC/SAML 回呼）與 `client_credentials`（機器帳號）
/// 對應的 `/auth/sso/*` 與 `api_clients` 尚未實作，收到時回 422 而非靜默失敗。
#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    /// password grant 用來定位租戶（見 migration 014）
    pub tenant_code: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub refresh_token: Option<String>,
}

/// `POST /auth/token/refresh` 的請求。
///
/// 契約在這條路徑上只要求 `refresh_token`，**沒有** `grant_type` ——
/// 因此不能重用 `TokenRequest`（那會讓 `grant_type` 變成必填）。
#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// `POST /auth/logout` 的請求。
///
/// 帶 refresh token 而不是只靠 access token 的身分：撤銷的粒度是單一 token
/// （見 migration 070 檔頭），而 access token 裡沒有任何指向某個 refresh
/// token 的東西。要「登出這個裝置」就必須把那個裝置手上的 refresh token 交出來。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogoutRequest {
    pub refresh_token: String,
}

/// `POST /auth/logout` 的回應。
///
/// 不是 204。兩件事必須說出來，而空回應說不了：
///   * `already_revoked` —— 客戶端分不出「剛剛撤銷」與「早就撤銷」，
///     而重試造成的第二次呼叫是正常流量。
///   * `access_token_remains_valid_for_seconds` —— access token 不在撤銷範圍
///     內（070 檔頭），客戶端若以為登出是立即的，就不會去清掉本機那一份。
#[derive(Debug, Serialize)]
pub struct LogoutResponse {
    pub revoked: bool,
    pub already_revoked: bool,
    /// access token 的 TTL。**不是**剩餘秒數 —— 這裡拿不到客戶端手上那張
    /// access token（logout 的授權由 middleware 完成，handler 只看得到 Caller），
    /// 因此給的是上界。欄位名說的就是這件事。
    pub access_token_remains_valid_for_seconds: i64,
}

/// `POST /auth/password/change` 的請求。
///
/// 要求 `current_password` 而不是只憑 access token：access token 可能是從
/// 一台沒鎖的機器上拿到的，而改密碼會把帳號的控制權交出去。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordChangeRequest {
    pub current_password: String,
    pub new_password: String,
}

/// `POST /auth/password/change` 的回應。
#[derive(Debug, Serialize)]
pub struct PasswordChangeResponse {
    pub changed: bool,
    /// **改密碼不會讓其他裝置上的 refresh token 失效。**
    ///
    /// 永遠是 `true`，而且刻意是一個欄位而不是文件裡的一句話：撤銷機制的粒度
    /// 是單一 token（070 檔頭記錄了這個決策），而改密碼的請求手上沒有其他裝置
    /// 的 jti。回一個空的 200 會讓客戶端合理地以為「改了密碼就把別人踢掉了」。
    ///
    /// 要改成 false 需要在 users 加一欄 `tokens_valid_from`，那是一次獨立的決策。
    pub other_sessions_remain_valid: bool,
    /// 這次生效的最短長度政策（來自 `tenants.settings.password_min_length`）。
    /// 回傳它讓客戶端能顯示規則，也讓「政策沒被讀到」在回應裡看得見。
    pub min_length_applied: i32,
}

/// `TokenResponse`
#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub refresh_token: String,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub must_change_password: bool,
}

/// `CurrentUser.user`
#[derive(Debug, Serialize)]
pub struct UserDto {
    pub id: Uuid,
    pub employee_no: Option<String>,
    pub username: String,
    pub email: Option<String>,
    pub display_name: String,
    pub phone: Option<String>,
    pub job_title: Option<String>,
    pub user_type: String,
    pub primary_org_id: Option<Uuid>,
    pub default_facility_id: Option<Uuid>,
    pub status: String,
    pub last_login_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// `CurrentUser.tenant`
#[derive(Debug, Serialize)]
pub struct TenantDto {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub industry: Option<String>,
    pub feature_flags: serde_json::Value,
}

/// `CurrentUser.accessible_facilities[]`
#[derive(Debug, Serialize)]
pub struct FacilityDto {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub org_id: Uuid,
}

/// `CurrentUser.roles[]`
#[derive(Debug, Serialize)]
pub struct RoleDto {
    pub role_code: String,
    pub scope_type: String,
    pub scope_id: Option<Uuid>,
}

/// `CurrentUser`
#[derive(Debug, Serialize)]
pub struct CurrentUserResponse {
    pub user: UserDto,
    pub tenant: TenantDto,
    pub accessible_facilities: Vec<FacilityDto>,
    pub roles: Vec<RoleDto>,
    /// 格式 `permission@scope_type:scope_id`
    pub permissions: Vec<String>,
}
