//! `GET /auth/sso/{providerCode}/authorize` 與 `/callback`。
//!
//! # 這一組的價值集中在 `/callback` 的前半
//!
//! `/callback` 完成不了登入（缺 client_secret 的解析器、缺可對接的 IdP，
//! 見 `fms_identity::sso` 檔頭），但它的**前半是這條流程裡的安全核心**：
//! state 的驗證與一次性消耗。那是 CSRF 與重放防護所在，也是最容易寫錯的地方。
//!
//! 因此 `e_`～`h_` 四格全部在守那一段，而 `i_` 斷言後半真的回 501 而不是
//! 一個假的成功 —— **一支核發身分卻沒有驗證任何東西的 callback 會是這個系統裡
//! 最危險的程式碼**。
//!
//! # `/authorize` 的兩格是「不要洩漏、不要猜」
//!
//! `b_` 守 `tenant_code` 必填（provider code 只在租戶內唯一 —— 沒有它就得猜，
//! 而猜錯會讓 A 租戶的登入跳到 B 租戶的 IdP）。
//! `c_` 守 redirect_uri 來自部署設定而不是請求（否則是開放轉址器）。
//!
//! # 沒有被覆蓋的
//!
//! `/authorize` 的**成功路徑要抓 IdP 的 discovery 文件**，而閘門強制 https ——
//! 與 `test-connection` 和 webhook 投遞同一道牆。因此 `a_` 驗的是「抓不到
//! discovery 時回什麼」，而「真的抓到並組出授權網址」沒有端到端測試。
//! PKCE 的 S256 計算另有單元測試（純函式，有固定向量）。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::Value;

/// 009 的示範 OIDC 來源。
const PROVIDER_OIDC: &str = "entra-hq";
/// 009 的示範 LDAP 來源 —— 用來驗「非 OIDC 不走這條流程」。
const PROVIDER_LDAP: &str = "ad-cinema";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// `/authorize` 與 `/callback` 都**不需認證**（使用者還沒登入），
/// 因此刻意不用 `authed()` —— 用了反而測不到未認證可用這件事。
async fn authorize(ctx: &TestContext, query: &str) -> (StatusCode, Value) {
    ctx.send(get(&format!(
        "/api/v1/auth/sso/{PROVIDER_OIDC}/authorize{query}"
    )))
    .await
}

async fn callback(ctx: &TestContext, query: &str) -> (StatusCode, Value) {
    ctx.send(get(&format!(
        "/api/v1/auth/sso/{PROVIDER_OIDC}/callback{query}"
    )))
    .await
}

/// 直接建一筆授權請求，回傳它的 state。
///
/// `/authorize` 的成功路徑需要抓 IdP 的 discovery 文件（閘門強制 https），
/// 因此 callback 那幾格從資料庫佈置 —— 那與 `/authorize` 寫進去的形狀相同。
async fn seed_request(ctx: &TestContext, minutes_valid: i64) -> String {
    let state = format!("state-{}", uuid::Uuid::new_v4().simple());
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.sso_auth_requests
                (tenant_id, identity_provider_id, state, nonce, pkce_verifier,
                 redirect_uri, expires_at)
         SELECT $1::uuid, p.id, $2, 'nonce-x', 'verifier-x',
                'https://fms.test.example.com/api/v1/auth/sso/entra-hq/callback',
                clock_timestamp() + ($3::bigint * interval '1 minute')
           FROM fms.identity_providers p
          WHERE p.tenant_id = $1::uuid AND p.code = $4",
    )
    .bind(TENANT_ID)
    .bind(&state)
    .bind(minutes_valid)
    .bind(PROVIDER_OIDC)
    .execute(&mut *tx)
    .await
    .expect("插入授權請求");
    tx.commit().await.expect("commit");
    state
}

async fn consumed_at(ctx: &TestContext, state: &str) -> Option<i64> {
    let mut tx = ctx.owner_tx().await;
    let v: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT consumed_at FROM fms.sso_auth_requests WHERE state = $1")
            .bind(state)
            .fetch_one(&mut *tx)
            .await
            .expect("read consumed_at");
    drop(tx);
    v.map(|t| t.timestamp())
}

// =============================================================================

/// `/authorize` 走到抓 discovery 文件那一步，並在抓不到時說出**是哪一種**問題。
///
/// 009 的 `entra-hq` 的 issuer 指向一個不存在的主機，因此這一格實際驗的是
/// 「閘門與 discovery 取得的錯誤路徑說得清楚」—— 而不是成功組出授權網址
/// （那需要真實 IdP，見檔頭）。
#[tokio::test]
async fn a_authorize_reaches_discovery_and_explains_failures() {
    let ctx = &TestContext::setup().await;

    let (status, body) = authorize(ctx, &format!("?tenant_code={TENANT_CODE}")).await;
    assert_ne!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "抓不到 discovery 不該是 500 —— 那會讓整合的人去查伺服器日誌：{body}"
    );
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("discovery") || detail.contains("身分來源"),
        "錯誤訊息沒說出是 discovery 那一步的問題：{body}"
    );

    // **不該留下一筆授權請求。** 抓不到 discovery 就組不出授權網址，
    // 而先寫 state 再失敗會在表裡堆一堆永遠不會被 callback 用到的列。
    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM fms.sso_auth_requests")
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    drop(tx);
    assert_eq!(
        n, 0,
        "authorize 失敗卻留下了授權請求 —— 那些列永遠不會被使用"
    );

    ctx.teardown().await;
}

/// `tenant_code` 必填，而錯誤訊息要說出**為什麼**。
///
/// provider code 只在租戶內唯一（002 的 `uq_identity_providers_code` 是
/// `(tenant_id, lower(code))`）。少了租戶判別就得猜，而猜錯會讓 A 租戶的
/// 登入跳到 B 租戶的 IdP —— 使用者會把密碼輸入到別人的身分來源裡。
#[tokio::test]
async fn b_tenant_code_is_required_and_the_reason_is_stated() {
    let ctx = &TestContext::setup().await;

    let (status, body) = authorize(ctx, "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("租戶") && detail.contains("唯一"),
        "沒說出為什麼需要 tenant_code（provider code 只在租戶內唯一）：{detail}"
    );

    let (status, body) = authorize(ctx, "?tenant_code=NO_SUCH_TENANT").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    ctx.teardown().await;
}

/// 非 OIDC 的來源回 501 並說明原因，而不是組一個永遠失敗的網址。
#[tokio::test]
async fn c_non_oidc_providers_are_refused_with_a_reason() {
    let ctx = &TestContext::setup().await;

    let (status, body) = ctx
        .send(get(&format!(
            "/api/v1/auth/sso/{PROVIDER_LDAP}/authorize?tenant_code={TENANT_CODE}"
        )))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_IMPLEMENTED,
        "LDAP 來源不該走 OIDC 的授權碼流程：{body}"
    );
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("LDAP"), "沒說出是哪一種型別：{detail}");
    assert!(detail.contains("OIDC"), "沒說出目前支援的是什麼：{detail}");

    ctx.teardown().await;
}

/// **`redirect_uri` 來自部署設定，不是請求。**
///
/// 接受呼叫端給的 redirect_uri 就是一個開放轉址器：攻擊者把它指向自己的網站，
/// IdP 就會把授權碼送到那裡去。這一格用兩件事守它：
/// 帶了 `redirect_uri` 參數會被 `deny_unknown_fields`／忽略而不是採用，
/// 而資料庫裡存的那個值是從 `PUBLIC_BASE_URL` 組出來的。
#[tokio::test]
async fn d_redirect_uri_comes_from_deployment_config_not_the_request() {
    let ctx = &TestContext::setup().await;
    let state = seed_request(ctx, 10).await;

    let mut tx = ctx.owner_tx().await;
    let stored: String =
        sqlx::query_scalar("SELECT redirect_uri FROM fms.sso_auth_requests WHERE state = $1")
            .bind(&state)
            .fetch_one(&mut *tx)
            .await
            .expect("read redirect_uri");
    drop(tx);
    assert!(
        stored.starts_with("https://fms.test.example.com"),
        "redirect_uri 不是從部署設定組出來的：{stored}"
    );

    // 呼叫端塞一個自己的 redirect_uri —— 不該被採用。
    // （authorize 會在抓 discovery 那一步失敗，但重點是它**沒有**把那個值
    //  當成 redirect_uri —— 上面那格已經證明來源是設定。這裡驗的是多帶一個
    //  未知的查詢參數不會讓端點改變行為。）
    let (status, _) = authorize(
        ctx,
        &format!("?tenant_code={TENANT_CODE}&redirect_uri=https://evil.example.com/steal"),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "帶了自己的 redirect_uri 之後端點成功了 —— 那會是一個開放轉址器"
    );

    ctx.teardown().await;
}

/// **state 是一次性的。** 第二次用同一個 state 回 409 並說出可能是重放。
#[tokio::test]
async fn e_state_is_single_use() {
    let ctx = &TestContext::setup().await;
    let state = seed_request(ctx, 10).await;

    assert!(consumed_at(ctx, &state).await.is_none(), "佈置時就被消耗了");

    // 第一次：state 通過（然後在 token 交換那一步回 501）。
    let (first, body) = callback(ctx, &format!("?code=abc123&state={state}")).await;
    assert_eq!(
        first,
        StatusCode::NOT_IMPLEMENTED,
        "state 有效時應該走到 token 交換才停：{body}"
    );
    assert!(
        consumed_at(ctx, &state).await.is_some(),
        "state 沒有被標記為已使用 —— 那個 callback URL 可以被重複使用"
    );

    // 第二次：拒絕，而且說出可能是重放。
    let (second, body) = callback(ctx, &format!("?code=abc123&state={state}")).await;
    assert_eq!(
        second,
        StatusCode::CONFLICT,
        "同一個 state 用了第二次還通過 —— 被攔截的 callback URL 可以重放：{body}"
    );
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("重放") || detail.contains("使用過"),
        "沒說出這可能是重放：{detail}"
    );

    ctx.teardown().await;
}

/// 過期的 state 與已使用的 state **回不同的錯誤**。
///
/// 合併成一個「無效」會讓「可能的攻擊」與「使用者在 IdP 上待太久」在日誌裡
/// 長得一樣 —— 而那兩件事的處置完全不同。
#[tokio::test]
async fn f_expired_and_replayed_states_are_told_apart() {
    let ctx = &TestContext::setup().await;

    // 過期的（把期限撥到過去）。
    let expired = seed_request(ctx, 10).await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.sso_auth_requests
                SET expires_at = clock_timestamp() - interval '1 minute'
              WHERE state = $1",
        )
        .bind(&expired)
        .execute(&mut *tx)
        .await
        .expect("expire");
        tx.commit().await.expect("commit");
    }
    let (status, expired_body) = callback(ctx, &format!("?code=x&state={expired}")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{expired_body}");
    let expired_detail = expired_body["detail"].as_str().unwrap_or_default();
    assert!(
        expired_detail.contains("過期"),
        "過期沒有被說成過期：{expired_detail}"
    );

    // 已使用的。
    let used = seed_request(ctx, 10).await;
    let _ = callback(ctx, &format!("?code=x&state={used}")).await;
    let (_, used_body) = callback(ctx, &format!("?code=x&state={used}")).await;
    let used_detail = used_body["detail"].as_str().unwrap_or_default();

    assert_ne!(
        expired_detail, used_detail,
        "過期與重放回了同一個訊息 —— 日誌上分不出可能的攻擊與正常的逾時"
    );

    // 不存在的 state：401，與上面兩種都不同。
    let (status, body) = callback(ctx, "?code=x&state=never-existed").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "不存在的 state 應該與過期／重放分開：{body}"
    );

    ctx.teardown().await;
}

/// 缺 state 直接拒絕，而且說出那是 CSRF 防護所在。
#[tokio::test]
async fn g_missing_state_is_refused() {
    let ctx = &TestContext::setup().await;

    let (status, body) = callback(ctx, "?code=abc123").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or_default().contains("CSRF"),
        "沒說出缺 state 為什麼是安全問題：{body}"
    );

    ctx.teardown().await;
}

/// IdP 回錯誤（使用者按拒絕）時，**state 仍然被消耗**。
///
/// 那個 state 已經曝光在瀏覽器的位址欄裡，不該還能再用一次。
#[tokio::test]
async fn h_idp_error_still_consumes_the_state() {
    let ctx = &TestContext::setup().await;
    let state = seed_request(ctx, 10).await;

    let (status, body) = callback(
        ctx,
        &format!("?error=access_denied&error_description=User+declined&state={state}"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("access_denied"),
        "沒有把 IdP 的錯誤帶出來：{detail}"
    );

    assert!(
        consumed_at(ctx, &state).await.is_some(),
        "IdP 回錯誤時 state 沒有被消耗 —— 它已經曝光在位址欄裡，不該還能再用"
    );

    ctx.teardown().await;
}

/// **後半回 501，而且列出缺什麼 —— 絕不回一個假的成功。**
///
/// 一支核發身分卻沒有驗證 id_token 的 callback 會是這個系統裡最危險的程式碼：
/// 任何人只要拿到一個 state 就能變成任何人。
#[tokio::test]
async fn i_token_exchange_is_501_with_the_blockers_named() {
    let ctx = &TestContext::setup().await;
    let state = seed_request(ctx, 10).await;

    let (status, body) = callback(ctx, &format!("?code=authcode-xyz&state={state}")).await;
    assert_eq!(
        status,
        StatusCode::NOT_IMPLEMENTED,
        "不是 501 —— 而任何 2xx 都代表核發了身分：{body}"
    );

    let detail = body["detail"].as_str().unwrap_or_default();
    // 剩下的那個阻礙要指名，而且要說出根本原因（不是「還沒做」）。
    assert!(
        detail.contains("id_token") || detail.contains("JWKS"),
        "沒指出 id_token 的簽章驗證做不到：{detail}"
    );
    // **`client_secret` 已經不是阻礙了**（`fms_shared::secrets` 的解析器）。
    // 這條斷言守的是訊息不要繼續說一件已經不成立的事 —— 一個列出錯誤阻礙的
    // 501 會讓讀它的人往錯的方向修。訊息要把讀者導向 test-connection 的
    // secret_reference_resolvable，那才是密鑰問題的診斷處。
    assert!(
        detail.contains("secret_reference_resolvable"),
        "沒把密鑰的診斷指向 test-connection：{detail}"
    );
    assert!(
        !detail.contains("沒有解析器") && !detail.contains("尚無解析器"),
        "訊息還在說缺密鑰解析器 —— 那件事已經不成立：{detail}"
    );
    // **回應裡不該有任何 token。**
    let text = body.to_string();
    assert!(
        !text.contains("access_token") && !text.contains("refresh_token"),
        "501 的回應裡出現了 token 欄位：{text}"
    );
    // 授權碼不該被原樣回顯（它是憑證）。
    assert!(
        !text.contains("authcode-xyz"),
        "把授權碼回顯出來了 —— 那是一個憑證：{text}"
    );

    ctx.teardown().await;
}
