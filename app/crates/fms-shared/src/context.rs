//! 請求情境的萃取與驗證，對應規格書 §4.3 的 Context 標頭規範。

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use uuid::Uuid;

use crate::db::TenantContext;
use crate::problem::{Problem, ProblemCode};

/// 已認證的呼叫者。由 `require_auth` middleware 放進 extensions。
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub user_id: Uuid,
    pub tenant_id: Uuid,
    /// `X-Request-ID`，供稽核軌關聯 log（migration 029）。
    ///
    /// 型別是 `Uuid` 而非 `String`，好處是 `Caller` 與 `TenantContext`
    /// 維持 `Copy` —— 它們在整個應用層都是傳值的。
    /// 代價：客戶端若送了非 uuid 的 `X-Request-ID`，稽核列的 `request_id`
    /// 會是 NULL。`SetRequestIdLayer` 產生的是 uuid，因此只有**客戶端自帶
    /// 非 uuid 值**時才會發生，而那時我們無法把它塞進一個 `Copy` 型別 ——
    /// 這個取捨寫在這裡，不要讓下一個人以為是 bug。
    pub request_id: Option<Uuid>,
}

impl From<Caller> for TenantContext {
    fn from(c: Caller) -> Self {
        TenantContext {
            tenant_id: c.tenant_id,
            user_id: c.user_id,
            request_id: c.request_id,
            // 經 API 進來的一律是 USER。背景作業自己用
            // `TenantContext::background` 標成 SYSTEM／SERVICE_ACCOUNT。
            actor_type: ActorType::User,
        }
    }
}

/// `audit_log.actor_type` 的四個合法值（001 的 CHECK）。
///
/// 做成列舉而不是傳字串：非法值會讓稽核 INSERT 失敗，而那會連帶回滾業務寫入
/// （029 刻意不在觸發器裡吞例外）。讓它在 Rust 這一層就不可能出現。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorType {
    User,
    ServiceAccount,
    System,
    DirectorySync,
}

impl ActorType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "USER",
            Self::ServiceAccount => "SERVICE_ACCOUNT",
            Self::System => "SYSTEM",
            Self::DirectorySync => "DIRECTORY_SYNC",
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Caller>()
            .copied()
            .ok_or_else(|| Problem::unauthenticated("not authenticated"))
    }
}

/// 驗證 `X-Tenant-ID` 並與 token 的 `tid` 交叉比對。
///
/// 規格書 §4.3：缺少回 400、與 token `tid` 不符回 403。
/// 這道檢查刻意在業務層之前完成 —— 不符時直接拒絕，不進入資料層，
/// 因此不依賴 RLS 作為唯一防線。
pub fn verify_tenant_header(headers: &HeaderMap, token_tenant_id: Uuid) -> Result<(), Problem> {
    let raw = headers
        .get("x-tenant-id")
        .ok_or_else(|| Problem::bad_request("X-Tenant-ID header is required"))?
        .to_str()
        .map_err(|_| Problem::bad_request("X-Tenant-ID is not valid ASCII"))?;

    let header_tenant: Uuid = raw
        .parse()
        .map_err(|_| Problem::bad_request("X-Tenant-ID is not a valid UUID"))?;

    if header_tenant != token_tenant_id {
        return Err(Problem::new(ProblemCode::TenantMismatch)
            .with_detail("X-Tenant-ID does not match the tenant in the access token"));
    }
    Ok(())
}
