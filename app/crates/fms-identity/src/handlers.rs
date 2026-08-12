//! `/auth/*` 端點：`/auth/token`、`/auth/token/refresh`、`/auth/me`、
//! `/auth/logout`、`/auth/password/change`。
//!
//! `/auth/sso/*`（OIDC/SAML 的 authorize 與 callback）仍未實作 ——
//! 沒有可對接的 IdP，寫出來的東西無法驗證。
//!
//! # logout 為什麼牽動 refresh
//!
//! refresh token 原本是純無狀態 JWT，logout 因此無從撤銷。migration 070 加了
//! 黑名單，而讓 logout 完整的前提是**換發會消耗舊 token**（輪替）——
//! 少了輪替，登出只殺掉客戶端手上最後那一個。因此 [`refresh_grant`] 在這支
//! migration 之後多了三件事：查黑名單、寫入 ROTATED、偵測重播。
//! 完整的論證在 070 檔頭。

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use axum::{Json, RequestPartsExt};
use sqlx::PgPool;
use std::sync::Arc;

use fms_shared::{begin_tenant_tx, verify_tenant_header, Caller, Problem, ProblemCode, Settings};

use crate::dto::*;
use crate::throttle::LoginThrottle;
use crate::{jwt, password, repo};

/// 身分模組所需的狀態。
#[derive(Clone)]
pub struct IdentityState {
    /// 以 `fms_app` 連線的 pool。RLS 完整生效。
    pub pool: PgPool,
    pub settings: Arc<Settings>,
    /// 登入失敗計數。
    ///
    /// `Arc` 不是為了省記憶體：`build_router` 為每個子路由 clone 一次
    /// `IdentityState`，若計數器隨 clone 各自一份，節流就永遠不會生效。
    /// 用 `Arc` 是為了讓「同一個行程只有一份計數」由型別保證。
    pub throttle: Arc<LoginThrottle>,
}

impl IdentityState {
    /// 由設定建立。刻意提供建構子而非讓呼叫端寫結構實字：
    /// `throttle` 必須從 `settings` 導出，兩者分開傳會讓它們有機會不一致。
    pub fn new(pool: PgPool, settings: Settings) -> Self {
        let throttle = Arc::new(LoginThrottle::new(settings.login_throttle.clone()));
        Self {
            pool,
            settings: Arc::new(settings),
            throttle,
        }
    }
}

/// 登入失敗一律回同一個錯誤。
///
/// 刻意不區分「租戶不存在／使用者不存在／密碼錯誤／帳號停用」——
/// 任何區分都會變成帳號枚舉的側通道。
///
/// 回應統一只解決了一半：四條路徑的**耗時**原本差一個數量級
/// （沒有雜湊可比對就不會跑 argon2），時間本身就是側通道。
/// 因此每一條不比對密碼的路徑都會呼叫 [`password::verify_dummy`]。
fn invalid_credentials() -> Problem {
    Problem::unauthenticated("invalid credentials")
}

/// 登入被拒的兩種性質。分開是因為它們的後續處理相反：
/// 認證失敗要記軌、要計入節流、對外統一成 401；
/// 內部錯誤要原樣回傳，且**不得**計入節流 ——
/// 否則資料庫抖一下就會把使用者鎖在門外。
enum Rejection {
    Auth {
        tenant_id: Option<uuid::Uuid>,
        user_id: Option<uuid::Uuid>,
        /// 只進 `auth_events.failure_reason`，不會出現在回應裡。
        reason: &'static str,
    },
    Passthrough(Problem),
}

/// 認證成功的結果。除了回應本身，還要把識別資訊帶出來寫進登入軌。
struct Granted {
    response: TokenResponse,
    tenant_id: uuid::Uuid,
    user_id: uuid::Uuid,
}

/// `auth_events.user_agent` 的長度上限。
///
/// 欄位型別是 `text`（無限制），但失敗登入是**未認證**就能產生的列：
/// 不截斷等於讓任何人以一個數 KB 的標頭換一列數 KB 的稽核資料。
const USER_AGENT_MAX: usize = 512;

fn user_agent_of(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::USER_AGENT)?.to_str().ok()?;
    // 以字元邊界截斷：直接切位元組會在多位元組字元中間斷開而 panic。
    Some(match raw.char_indices().nth(USER_AGENT_MAX) {
        Some((end, _)) => raw[..end].to_string(),
        None => raw.to_string(),
    })
}

/// `POST /auth/token`
pub async fn token(
    State(state): State<IdentityState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<TokenRequest>,
) -> Result<Json<TokenResponse>, Problem> {
    match req.grant_type.as_str() {
        "password" => password_grant(&state, req, user_agent_of(&headers).as_deref())
            .await
            .map(Json),
        "refresh_token" => refresh_grant(&state, req).await.map(Json),
        "authorization_code" | "client_credentials" => Err(Problem::new(
            ProblemCode::ValidationError,
        )
        .with_detail(format!(
            "grant_type '{}' is not implemented yet (requires /auth/sso/* or api_clients)",
            req.grant_type
        ))),
        other => Err(Problem::validation(format!(
            "unsupported grant_type: {other}"
        ))),
    }
}

/// password grant 的外層：節流、登入軌、以及把所有認證失敗收斂成同一個 401。
///
/// 與 [`attempt_password_grant`] 分開，是為了讓「每一條失敗路徑都必須
/// 計入節流並留下軌跡」由結構保證，而不是靠在五個 `return` 前面各記一次 ——
/// 後者只要日後新增一條失敗路徑就會漏掉，而漏掉的症狀是無聲的。
async fn password_grant(
    state: &IdentityState,
    req: TokenRequest,
    user_agent: Option<&str>,
) -> Result<TokenResponse, Problem> {
    // 這三個是「請求連被處理的前提都不成立」，回 422。
    // 刻意不計入節流也不記登入軌：它們不是認證嘗試。
    let tenant_code = req
        .tenant_code
        .as_deref()
        .ok_or_else(|| Problem::validation("tenant_code is required for the password grant"))?;
    // 契約的欄位名仍是 `username`，但語意是**識別碼**：email 或 username 皆可
    // （見 `repo::find_auth_user_by_identifier`）。前端的登入畫面送 email。
    let identifier = req
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Problem::validation("username is required for the password grant (email or username)")
        })?;
    let supplied = req
        .password
        .as_deref()
        .ok_or_else(|| Problem::validation("password is required for the password grant"))?;

    // 節流的鍵一律小寫。查詢改成不分大小寫之後，若鍵還照原樣帶進來，
    // `admin@x` 與 `Admin@x` 會落在兩個不同的桶子裡 —— 同一個帳號的
    // 可嘗試次數就等於乘上大小寫排列數，節流形同虛設。
    let throttle_key = identifier.to_lowercase();
    let throttle_key = throttle_key.as_str();

    if let Some(retry_after) = state.throttle.check(tenant_code, throttle_key) {
        // 被擋掉的嘗試刻意**不**寫進 auth_events：導致封鎖的那幾筆失敗
        // 已經在軌裡了，而持續被擋的請求若也各寫一列，攻擊者就能用
        // 送請求換稽核表成長 —— 節流反而成了放大器。
        // 封鎖本身是營運訊號，因此走 log 與告警。
        tracing::warn!(
            tenant_code,
            identifier,
            retry_after,
            "登入節流生效：窗內失敗次數超過門檻"
        );
        return Err(Problem::too_many_requests(
            retry_after,
            "too many failed login attempts",
        ));
    }

    match attempt_password_grant(state, tenant_code, identifier, supplied).await {
        Ok(granted) => {
            state.throttle.clear(tenant_code, throttle_key);
            repo::record_login_event(
                &state.pool,
                Some(granted.tenant_id),
                Some(granted.user_id),
                true,
                None,
                user_agent,
            )
            .await;
            Ok(granted.response)
        }
        Err(Rejection::Auth {
            tenant_id,
            user_id,
            reason,
        }) => {
            state.throttle.record_failure(tenant_code, throttle_key);
            repo::record_login_event(
                &state.pool,
                tenant_id,
                user_id,
                false,
                Some(reason),
                user_agent,
            )
            .await;
            Err(invalid_credentials())
        }
        Err(Rejection::Passthrough(problem)) => Err(problem),
    }
}

/// 實際的認證。四條失敗路徑都回 [`Rejection::Auth`]，且**都**跑過一次
/// argon2 —— 沒有雜湊可比對的三條靠 [`password::verify_dummy`] 補上，
/// 否則耗時差異會洩漏帳號是否存在。
async fn attempt_password_grant(
    state: &IdentityState,
    tenant_code: &str,
    identifier: &str,
    supplied: &str,
) -> Result<Granted, Rejection> {
    // 尚無 RLS 情境，因此經 014 的 SECURITY DEFINER 函式解析租戶。
    let tenant_id = match repo::resolve_tenant_by_code(&state.pool, tenant_code).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            password::verify_dummy_async(supplied.to_string()).await;
            return Err(Rejection::Auth {
                tenant_id: None,
                user_id: None,
                reason: "TENANT_NOT_FOUND",
            });
        }
        Err(problem) => return Err(Rejection::Passthrough(problem)),
    };

    // 租戶確定後即可進入正常的 RLS 情境。user_id 先以 nil 佔位：
    // 此刻還沒完成認證，不應宣稱任何使用者身分。
    // 尚未認證，因此 user_id 是 nil、沒有 actor 可記。登入本身的軌跡
    // 走 auth_events（024），不是 audit_log。
    let ctx = fms_shared::TenantContext::background(
        tenant_id,
        uuid::Uuid::nil(),
        fms_shared::ActorType::System,
    );
    let mut tx = begin_tenant_tx(&state.pool, ctx)
        .await
        .map_err(Rejection::Passthrough)?;

    let user = match repo::find_auth_user_by_identifier(&mut tx, identifier).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            password::verify_dummy_async(supplied.to_string()).await;
            return Err(Rejection::Auth {
                tenant_id: Some(tenant_id),
                user_id: None,
                reason: "USER_NOT_FOUND",
            });
        }
        Err(problem) => return Err(Rejection::Passthrough(problem)),
    };

    // 密碼為 NULL 表示此帳號只能經目錄來源登入（規格書：password_hash
    // 僅 LOCAL provider 有值），不可視為「空密碼通過」。
    let Some(hash) = user.password_hash.as_deref() else {
        password::verify_dummy_async(supplied.to_string()).await;
        return Err(Rejection::Auth {
            tenant_id: Some(tenant_id),
            user_id: Some(user.id),
            reason: "NO_LOCAL_PASSWORD",
        });
    };
    if !password::verify_async(supplied.to_string(), hash.to_string()).await {
        return Err(Rejection::Auth {
            tenant_id: Some(tenant_id),
            user_id: Some(user.id),
            reason: "BAD_PASSWORD",
        });
    }
    if user.status != "ACTIVE" {
        return Err(Rejection::Auth {
            tenant_id: Some(tenant_id),
            user_id: Some(user.id),
            reason: "ACCOUNT_NOT_ACTIVE",
        });
    }

    repo::touch_last_login(&mut tx, user.id)
        .await
        .map_err(Rejection::Passthrough)?;
    tx.commit().await.map_err(Rejection::Passthrough)?;

    let response = issue_pair(state, user.id, tenant_id, user.must_change_password)
        .map_err(Rejection::Passthrough)?;
    Ok(Granted {
        response,
        tenant_id,
        user_id: user.id,
    })
}

/// `POST /auth/token/refresh`
///
/// 契約把它定義成一條**獨立路徑**，request body 只有 `refresh_token`
/// （沒有 `grant_type`）。實作原本只在 `POST /auth/token` 以
/// `grant_type=refresh_token` 支援，因此契約定義的這條路徑是 404 ——
/// 照契約寫的客戶端會打不到。
///
/// 兩條路徑共用同一個 [`refresh_grant`]，不是各自實作：token 的簽發與帳號
/// 狀態複驗只該有一份。
pub async fn refresh(
    State(state): State<IdentityState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<TokenResponse>, Problem> {
    refresh_grant(
        &state,
        TokenRequest {
            grant_type: "refresh_token".to_string(),
            tenant_code: None,
            username: None,
            password: None,
            refresh_token: Some(req.refresh_token),
        },
    )
    .await
    .map(Json)
}

/// refresh grant 刻意不節流。
///
/// 沒有可用的鍵。refresh token 是 256 bit 等級的簽章字串，猜中的機率不是
/// 「慢慢試就會中」那種量級，而唯一能當鍵的東西（token 本身）對每次嘗試
/// 都不同，計數毫無意義。
///
/// # 換發會消耗舊 token（輪替）
///
/// 070 之後每次換發都把用掉的 jti 寫進 `revoked_refresh_tokens`（原因
/// `ROTATED`）。這不是為了記帳，是 `POST /auth/logout` 正確性的前提：
/// 少了它，一條換發鏈上先前的 token 全都還活著，使用者登出只殺掉最後一個。
/// 完整的理由在 070 檔頭。
///
/// 成功的換發**不寫** `auth_events`。`TOKEN_REFRESH` 這個事件型別在 002 的
/// 欄位註解裡列著，但正常換發是每 15 分鐘一次的例行動作，記下來只會把
/// 認證軌淹掉 —— 而軌跡的用途是事後調查。**失敗**的換發要記，而且只記一種：
/// 已被輪替掉的 token 又出現（`TOKEN_REUSE`），那是 token 被複製的訊號。
async fn refresh_grant(state: &IdentityState, req: TokenRequest) -> Result<TokenResponse, Problem> {
    let token = req
        .refresh_token
        .as_deref()
        .ok_or_else(|| Problem::validation("refresh_token is required"))?;

    let claims = jwt::verify(&state.settings.jwt.secret, token, jwt::TokenType::Refresh)?;
    let jti = claims.refresh_jti()?;
    let expires_at = expiry_of(&claims)?;

    // 重新確認帳號仍然有效：token 尚未過期不代表帳號還能用。
    //
    // 標成 SYSTEM 而非 USER：refresh 是 token 的換發，不是使用者對某筆資料的
    // 操作。（070 之後這條路徑會寫一列黑名單，但那一列不是稽核對象 ——
    // audit_log 記的是領域資料的變更。）
    let ctx = fms_shared::TenantContext::background(
        claims.tid,
        claims.sub,
        fms_shared::ActorType::System,
    );
    let mut tx = begin_tenant_tx(&state.pool, ctx).await?;

    // 已撤銷的一律拒絕。`LOGOUT` 與 `ROTATED` 對客戶端是同一個結果（401），
    // 但只有後者值得留下軌跡：登出過的 token 再被送來通常只是客戶端還沒清乾淨，
    // 而已經被換掉的 token 再出現代表有第二份副本。
    //
    // # 這一段看起來多餘，但不是
    //
    // 就「會不會被拒絕」而言它確實是多餘的：下面輪替那一步以 jti 為主鍵寫入，
    // 已撤銷的 token 一定撞號，於是走到 `!consumed` 那條路也回 401。突變測試
    // 證實了這件事 —— 把這個 if 改成永遠不成立，`a_`（登出後換不了）仍然通過。
    //
    // 它獨有的貢獻是**事件分類**：少了它，已登出的 token 被重送會被記成
    // `TOKEN_REUSE`，而那條軌是用來認出 token 被複製的。客戶端沒清乾淨本機
    // 那一份是常態流量，混進去之後這條軌就只是雜訊。抓到這件事的是 `e_`。
    if let Some(reason) = repo::refresh_token_revocation(&mut tx, jti).await? {
        if reason == "ROTATED" {
            repo::record_auth_event_tx(
                &mut tx,
                claims.sub,
                "TOKEN_REUSE",
                "FAILURE",
                Some("已輪替的 refresh token 再次被使用"),
                None,
            )
            .await?;
            // 請求會失敗，但事件必須留下 —— 所以在回錯之前先提交。
            tx.commit().await?;
        }
        return Err(invalid_credentials());
    }

    let profile = repo::load_user_profile(&mut tx, claims.sub).await?;
    if profile.status != "ACTIVE" {
        return Err(invalid_credentials());
    }

    // 消耗掉這一個。
    //
    // 回傳 false 表示這一列已經存在 —— 在同一個交易裡不可能（前面剛查過），
    // 因此只有一種情況：另一個請求拿著**同一個 token** 並發換發，在我們查完
    // 之後先提交了。`ON CONFLICT DO NOTHING` 會等它提交再回 0 列，於是這裡
    // 得到 false。
    //
    // 這一格讓「查黑名單 → 寫黑名單」變成原子的：少了它，一次並發會從一個
    // token 產出兩條有效的換發鏈，而 logout 只殺得掉其中一條。不需要顯式鎖，
    // 主鍵衝突就是那把鎖。
    if !repo::revoke_refresh_token(&mut tx, jti, claims.sub, expires_at, "ROTATED").await? {
        repo::record_auth_event_tx(
            &mut tx,
            claims.sub,
            "TOKEN_REUSE",
            "FAILURE",
            Some("同一個 refresh token 並發換發"),
            None,
        )
        .await?;
        tx.commit().await?;
        return Err(invalid_credentials());
    }

    // 先簽再提交。反過來的話，`issue_pair` 失敗會留下一個「舊 token 已作廢、
    // 新 token 沒發出」的狀態，而使用者手上就沒有任何可用的 refresh token 了。
    let pair = issue_pair(state, claims.sub, claims.tid, false)?;
    tx.commit().await?;
    Ok(pair)
}

/// 從 claims 的 `exp` 取出過期時刻。
///
/// 黑名單的 `expires_at` 必須是 token 自己的 exp —— 清理靠它判斷「這一列守的
/// token 已經不可能通過驗證」（070 檔頭）。用「現在 + refresh_ttl」估算會在
/// 換過 TTL 設定之後悄悄失準，而失準的方向是**提早刪**，也就是撤銷失效。
fn expiry_of(claims: &jwt::Claims) -> Result<chrono::DateTime<chrono::Utc>, Problem> {
    chrono::DateTime::from_timestamp(claims.exp, 0).ok_or_else(|| {
        // exp 來自我們自己簽的 token，超出 chrono 範圍實務上不會發生；
        // 但這裡不能 unwrap —— 那會把一個可疑的 token 變成 panic。
        Problem::unauthenticated("token 的 exp 不是合法的時間點")
    })
}

/// `POST /auth/logout`
///
/// 撤銷 request body 裡那一個 refresh token。契約寫的是「撤銷 refresh token」，
/// 而在 070 之前這件事**做不到**（refresh token 是純無狀態 JWT）——
/// 該 migration 的檔頭記錄了機制與它的邊界。
///
/// # 為什麼要驗 token 屬於呼叫者
///
/// 少了這一格，任何一個已登入的使用者只要拿到別人的 refresh token 字串就能
/// 把對方登出。更要緊的是它把 logout 變成一個**攻擊工具**：撤銷是不可撤回的
/// （070 沒給 fms_app DELETE），所以每一次越權登出都是一次無法復原的騷擾。
///
/// # 為什麼不是 204
///
/// 見 [`LogoutResponse`]：有兩件事必須說出來（是否早就撤銷過、access token
/// 仍然有效多久），而空回應說不了。
pub async fn logout(
    State(state): State<IdentityState>,
    caller: Caller,
    Json(req): Json<LogoutRequest>,
) -> Result<Json<LogoutResponse>, Problem> {
    let claims = jwt::verify(
        &state.settings.jwt.secret,
        &req.refresh_token,
        jwt::TokenType::Refresh,
    )?;
    let jti = claims.refresh_jti()?;
    let expires_at = expiry_of(&claims)?;

    if claims.sub != caller.user_id || claims.tid != caller.tenant_id {
        // 刻意是 403 而不是 404／401：這個 token 是合法的（簽章過了），
        // 只是不屬於呼叫者。回 404 會讓「這個 token 存在嗎」變成可探測的。
        return Err(Problem::permission_denied(
            "這個 refresh token 不屬於呼叫者",
        ));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let newly =
        repo::revoke_refresh_token(&mut tx, jti, caller.user_id, expires_at, "LOGOUT").await?;

    // 只在真的撤銷了才記事件。重複登出（客戶端重試）不該在認證軌裡留下
    // 一串看起來像「這個帳號一直在登出」的列。
    if newly {
        repo::record_auth_event_tx(
            &mut tx,
            caller.user_id,
            "LOGOUT",
            "SUCCESS",
            None,
            user_agent_of_caller(),
        )
        .await?;
    }
    tx.commit().await?;

    Ok(Json(LogoutResponse {
        // 撤銷是幂等的，因此兩種情況下「這個 token 現在是撤銷狀態」都成立。
        revoked: true,
        already_revoked: !newly,
        access_token_remains_valid_for_seconds: state.settings.jwt.access_ttl.as_secs() as i64,
    }))
}

/// logout／改密碼的 user_agent。
///
/// 目前固定 `None`：這兩支的 handler 拿到的是 [`Caller`]，裡面沒有 header。
/// 要記就得把 `HeaderMap` 也抽出來 —— 而 `auth_events.user_agent` 的用途是
/// 認出「同一個人從哪個客戶端登入」，那個問題在登入事件上已經有答案
/// （[`record_login_event`] 有記）。這裡留一個具名的函式而不是散在兩處寫
/// `None`，是為了讓「刻意不記」與「忘了記」看得出差別。
///
/// [`record_login_event`]: crate::repo::record_login_event
fn user_agent_of_caller() -> Option<&'static str> {
    None
}

/// `POST /auth/password/change`
///
/// # 為什麼要 `current_password`
///
/// access token 可能是從一台沒鎖的機器上拿到的，而改密碼會把帳號的控制權
/// 交出去（新密碼的持有者從此能自己登入，不再受 access token 15 分鐘的限制）。
///
/// # 最短長度不寫死
///
/// 走 `tenants.settings.password_min_length`（067 的機制、070 把鍵加進形狀
/// 約束）。密碼政策是管理者定義的條件。
///
/// # 這支不會撤銷其他裝置的 refresh token
///
/// 撤銷的粒度是單一 token，而這個請求手上沒有其他裝置的 jti。回應裡的
/// `other_sessions_remain_valid` 就是在說這件事 —— 見 070 檔頭與
/// [`PasswordChangeResponse`]。
pub async fn password_change(
    State(state): State<IdentityState>,
    caller: Caller,
    Json(req): Json<PasswordChangeRequest>,
) -> Result<Json<PasswordChangeResponse>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let ctx = repo::load_password_change_context(&mut tx, caller.user_id).await?;

    // require_auth 只驗簽章，不看帳號狀態 —— 被停權的人手上那張 access token
    // 還有效到它自己過期。因此這一格不是多餘的重複檢查。
    if ctx.status != "ACTIVE" {
        return Err(Problem::permission_denied("帳號目前不是 ACTIVE 狀態"));
    }

    // 沒有本地密碼的帳號只能經目錄來源登入（002：password_hash 僅 LOCAL
    // provider 有值）。這裡若讓它「改」密碼，會憑空造出一條本地登入途徑，
    // 繞過管理者刻意選擇的身分來源。
    let Some(current_hash) = ctx.password_hash.as_deref() else {
        return Err(Problem::new(ProblemCode::Conflict)
            .with_detail("這個帳號沒有本地密碼（只能經目錄來源登入），無法在此變更"));
    };

    if !password::verify_async(req.current_password.clone(), current_hash.to_string()).await {
        // **不是 401。** 呼叫者的身分是有效的（access token 過了 require_auth），
        // 錯的是 body 裡的一個欄位。回 401 會讓客戶端的「token 過期了，
        // 去 refresh」邏輯被觸發，然後帶著同樣錯的密碼再試一次。
        return Err(
            Problem::validation("current_password 不正確").with_errors(vec![
                fms_shared::FieldError {
                    pointer: "/current_password".to_string(),
                    code: "INVALID".to_string(),
                    message: "現在的密碼不正確".to_string(),
                },
            ]),
        );
    }

    if (req.new_password.chars().count() as i32) < ctx.min_length {
        return Err(
            Problem::validation("新密碼不符合租戶的最短長度政策").with_errors(vec![
                fms_shared::FieldError {
                    pointer: "/new_password".to_string(),
                    code: "MINIMUM".to_string(),
                    message: format!("至少需要 {} 個字元", ctx.min_length),
                },
            ]),
        );
    }

    // 新舊相同時擋掉。不擋的話這支會回 `changed: true` 而什麼都沒變 ——
    // 而客戶端據此顯示「密碼已更新」。
    if password::verify_async(req.new_password.clone(), current_hash.to_string()).await {
        return Err(
            Problem::validation("新密碼與現在的密碼相同").with_errors(vec![
                fms_shared::FieldError {
                    pointer: "/new_password".to_string(),
                    code: "INVALID".to_string(),
                    message: "新密碼不能與現在的密碼相同".to_string(),
                },
            ]),
        );
    }

    let new_hash = password::hash_async(req.new_password.clone()).await?;
    repo::update_password(&mut tx, caller.user_id, &new_hash).await?;
    repo::record_auth_event_tx(
        &mut tx,
        caller.user_id,
        "PASSWORD_CHANGED",
        "SUCCESS",
        None,
        user_agent_of_caller(),
    )
    .await?;
    tx.commit().await?;

    Ok(Json(PasswordChangeResponse {
        changed: true,
        other_sessions_remain_valid: true,
        min_length_applied: ctx.min_length,
    }))
}

fn issue_pair(
    state: &IdentityState,
    user_id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    must_change_password: bool,
) -> Result<TokenResponse, Problem> {
    let (access_token, expires_in) = jwt::issue(
        &state.settings.jwt.secret,
        user_id,
        tenant_id,
        jwt::TokenType::Access,
        state.settings.jwt.access_ttl,
    )?;
    let (refresh_token, _) = jwt::issue(
        &state.settings.jwt.secret,
        user_id,
        tenant_id,
        jwt::TokenType::Refresh,
        state.settings.jwt.refresh_ttl,
    )?;

    Ok(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in,
        refresh_token,
        tenant_id,
        user_id,
        must_change_password,
    })
}

/// `GET /auth/me`
pub async fn me(
    State(state): State<IdentityState>,
    caller: Caller,
) -> Result<Json<CurrentUserResponse>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let profile = repo::load_user_profile(&mut tx, caller.user_id).await?;
    let tenant = repo::load_tenant(&mut tx).await?;
    let facilities = repo::load_accessible_facilities(&mut tx, caller.user_id).await?;
    let roles = repo::load_roles(&mut tx, caller.user_id).await?;
    let permissions = repo::load_permission_strings(&mut tx, caller.user_id).await?;

    tx.commit().await?;

    Ok(Json(CurrentUserResponse {
        user: UserDto {
            id: profile.id,
            employee_no: profile.employee_no,
            username: profile.username,
            email: profile.email,
            display_name: profile.display_name,
            phone: profile.phone,
            job_title: profile.job_title,
            user_type: profile.user_type,
            primary_org_id: profile.primary_org_id,
            default_facility_id: profile.default_facility_id,
            status: profile.status,
            last_login_at: profile.last_login_at,
        },
        tenant: TenantDto {
            id: tenant.id,
            code: tenant.code,
            name: tenant.name,
            industry: tenant.industry,
            feature_flags: tenant.feature_flags,
        },
        accessible_facilities: facilities
            .into_iter()
            .map(|f| FacilityDto {
                id: f.id,
                code: f.code,
                name: f.name,
                org_id: f.org_id,
            })
            .collect(),
        roles: roles
            .into_iter()
            .map(|r| RoleDto {
                role_code: r.role_code,
                scope_type: r.scope_type,
                scope_id: r.scope_id,
            })
            .collect(),
        permissions,
    }))
}

/// 認證 middleware：驗證 Bearer access token，交叉比對 `X-Tenant-ID`，
/// 並把 [`Caller`] 放進 extensions。
///
/// 兩件事刻意在此完成而非交給 handler：
/// 1. `X-Tenant-ID` 與 token `tid` 的一致性 —— 不符時在進入資料層前就拒絕，
///    不把 RLS 當作唯一防線（規格書 §4.2 明訂「不一致直接 403，不進入業務層」）。
/// 2. token 種類必須是 access —— refresh token 不得用於一般請求。
pub async fn require_auth(
    State(state): State<IdentityState>,
    mut req: Request,
    next: Next,
) -> Result<Response, Problem> {
    let (mut parts, body) = req.into_parts();

    let header = parts
        .extract::<axum::http::HeaderMap>()
        .await
        .ok()
        .and_then(|h| h.get(AUTHORIZATION).cloned())
        .ok_or_else(|| Problem::unauthenticated("missing Authorization header"))?;

    let token = header
        .to_str()
        .ok()
        .and_then(|v| v.strip_prefix("Bearer ").map(str::to_owned))
        .ok_or_else(|| Problem::unauthenticated("expected a Bearer token"))?;

    let claims = jwt::verify(&state.settings.jwt.secret, &token, jwt::TokenType::Access)?;
    verify_tenant_header(&parts.headers, claims.tid)?;

    parts.extensions.insert(Caller {
        user_id: claims.sub,
        tenant_id: claims.tid,
        // 供稽核軌關聯 log（029）。SetRequestIdLayer 產生的是 uuid；
        // 客戶端自帶非 uuid 的值時解析失敗，稽核列的 request_id 留空。
        request_id: parts
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok()),
    });

    req = Request::from_parts(parts, body);
    Ok(next.run(req).await)
}
