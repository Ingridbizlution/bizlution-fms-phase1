//! auth 垂直切片的端到端測試。
//!
//! 驗證的是「跨模組機制接得起來」，而不只是端點回 200：
//!   * 014 的 SECURITY DEFINER 租戶解析（否則登入不可能）
//!   * `fms.set_context()` 注入後才能讀到本租戶資料
//!   * `X-Tenant-ID` 與 token `tid` 的一致性（缺少 400、不符 403）
//!   * 權限來自 `fms.v_user_effective_permissions`，非 Rust 內建模型
//!   * 錯誤回應為 RFC 9457 且 Content-Type 是 application/problem+json

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const TENANT_CODE: &str = "DEMO_GROUP";
const TENANT_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const USERNAME: &str = "admin.chen";
const TEST_PASSWORD: &str = "slice-test-password";

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn app_url() -> String {
    env_or(
        "APP_DATABASE_URL",
        "postgres://fms_app:change_me_app@localhost:5433/fms",
    )
}
fn owner_url() -> String {
    env_or(
        "OWNER_DATABASE_URL",
        "postgres://fms_owner:change_me_owner@localhost:5433/fms",
    )
}

/// 為 demo 使用者設定已知密碼（009 的使用者是目錄來源，password_hash 為 NULL）。
/// 回傳原值以便還原。
async fn set_test_password() -> Option<String> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&owner_url())
        .await
        .expect("connect as fms_owner");

    // 必須固定在同一條連線上：`SET` 是 session 層級的，
    // 若後續查詢從 pool 取到另一條連線，平台情境就不在了。
    let mut conn = pool.acquire().await.expect("acquire connection");

    sqlx::query("SET app.is_platform = 'on'")
        .execute(&mut *conn)
        .await
        .expect("claim platform context");

    let previous: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM fms.users WHERE username::text = $1")
            .bind(USERNAME)
            .fetch_one(&mut *conn)
            .await
            .expect("read existing hash");

    let hash = fms_identity::password::hash(TEST_PASSWORD).expect("hash password");
    sqlx::query("UPDATE fms.users SET password_hash = $1 WHERE username::text = $2")
        .bind(&hash)
        .bind(USERNAME)
        .execute(&mut *conn)
        .await
        .expect("set test hash");

    previous
}

async fn restore_password(previous: Option<String>) {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&owner_url())
        .await
        .expect("connect as fms_owner");
    let mut conn = pool.acquire().await.expect("acquire connection");
    sqlx::query("SET app.is_platform = 'on'")
        .execute(&mut *conn)
        .await
        .ok();
    sqlx::query("UPDATE fms.users SET password_hash = $1 WHERE username::text = $2")
        .bind(previous)
        .bind(USERNAME)
        .execute(&mut *conn)
        .await
        .expect("restore hash");
}

async fn router() -> axum::Router {
    let settings = common::test_settings(&app_url());
    let pool = PgPoolOptions::new()
        .max_connections(settings.database.max_connections)
        .connect(&settings.database.url)
        .await
        .expect("connect as fms_app");
    // 每次 send() 都重建 state，因此每個請求拿到的是一份**新的**登入失敗
    // 計數器。本檔的第 2、3 步刻意送錯密碼，共用計數會讓後續步驟被節流擋掉。
    // 節流本身由 auth_hardening_slice 驗證（那裡共用同一份 state）。
    fms_server::build_router(
        fms_identity::IdentityState::new(pool, settings),
        common::test_storage(),
        common::test_secrets(),
        String::new(),
        String::new(),
    )
}

async fn send(req: Request<Body>) -> (StatusCode, Option<String>, Value) {
    let app = router().await;
    let res = app.oneshot(req).await.expect("router call");
    let status = res.status();
    let content_type = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = res
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, content_type, json)
}

fn json_post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn auth_slice_end_to_end() {
    let previous = set_test_password().await;

    // --- 1. password grant 應成功，且回傳契約定義的全部欄位 ---
    let (status, _, body) = send(json_post(
        "/api/v1/auth/token",
        serde_json::json!({
            "grant_type": "password",
            "tenant_code": TENANT_CODE,
            "username": USERNAME,
            "password": TEST_PASSWORD,
        }),
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "login failed: {body}");
    for field in [
        "access_token",
        "token_type",
        "expires_in",
        "refresh_token",
        "tenant_id",
        "user_id",
        "must_change_password",
    ] {
        assert!(!body[field].is_null(), "TokenResponse 缺少 {field}: {body}");
    }
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["tenant_id"], TENANT_ID);
    let access = body["access_token"].as_str().unwrap().to_string();
    let refresh = body["refresh_token"].as_str().unwrap().to_string();

    // --- 2. 錯誤密碼應為 401，且錯誤格式符合 RFC 9457 ---
    let (status, content_type, body) = send(json_post(
        "/api/v1/auth/token",
        serde_json::json!({
            "grant_type": "password",
            "tenant_code": TENANT_CODE,
            "username": USERNAME,
            "password": "wrong",
        }),
    ))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        content_type.as_deref(),
        Some("application/problem+json"),
        "錯誤回應必須是 application/problem+json"
    );
    for field in ["type", "title", "status", "code"] {
        assert!(!body[field].is_null(), "Problem 缺少 {field}: {body}");
    }
    assert_eq!(body["status"], 401);
    assert_eq!(body["code"], "UNAUTHENTICATED");

    // --- 3. 不存在的租戶代碼也回同一個錯誤（避免帳號／租戶枚舉）---
    let (status, _, body) = send(json_post(
        "/api/v1/auth/token",
        serde_json::json!({
            "grant_type": "password",
            "tenant_code": "NO_SUCH_TENANT",
            "username": USERNAME,
            "password": TEST_PASSWORD,
        }),
    ))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "UNAUTHENTICATED");

    // --- 4. /auth/me 帶正確 token 與 X-Tenant-ID ---
    let (status, _, body) = send(
        Request::builder()
            .uri("/api/v1/auth/me")
            .header("authorization", format!("Bearer {access}"))
            .header("x-tenant-id", TENANT_ID)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "/auth/me failed: {body}");
    assert_eq!(body["user"]["username"], USERNAME);
    assert_eq!(body["tenant"]["code"], TENANT_CODE);
    assert!(
        body["tenant"]["feature_flags"].is_object(),
        "feature_flags 應為物件"
    );
    // 權限來自 fms.v_user_effective_permissions；admin.chen 有 TENANT 範圍授權
    let perms = body["permissions"]
        .as_array()
        .expect("permissions 應為陣列");
    assert!(
        !perms.is_empty(),
        "admin.chen 應有展開後的權限，實際為空 —— 表示未真的讀到 view"
    );
    assert!(
        perms.iter().any(|p| p.as_str().unwrap().contains('@')),
        "權限格式應為 permission@scope_type[:scope_id]，實際：{perms:?}"
    );
    assert!(
        !body["roles"].as_array().unwrap().is_empty(),
        "admin.chen 應至少有一個角色授權"
    );

    // --- 5. 缺少 X-Tenant-ID → 400（規格書 §4.3）---
    let (status, _, body) = send(
        Request::builder()
            .uri("/api/v1/auth/me")
            .header("authorization", format!("Bearer {access}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "BAD_REQUEST");

    // --- 6. X-Tenant-ID 與 token tid 不符 → 403 ---
    let (status, _, body) = send(
        Request::builder()
            .uri("/api/v1/auth/me")
            .header("authorization", format!("Bearer {access}"))
            .header("x-tenant-id", "aaaaaaaa-0000-4000-8000-0000000000ff")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "TENANT_MISMATCH");

    // --- 7. 用 refresh token 當 access token 應被拒 ---
    let (status, _, _) = send(
        Request::builder()
            .uri("/api/v1/auth/me")
            .header("authorization", format!("Bearer {refresh}"))
            .header("x-tenant-id", TENANT_ID)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "refresh token 不得用於一般請求"
    );

    // --- 8. refresh grant 應換到新的 access token ---
    let (status, _, body) = send(json_post(
        "/api/v1/auth/token",
        serde_json::json!({ "grant_type": "refresh_token", "refresh_token": refresh }),
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "refresh failed: {body}");
    assert!(body["access_token"].is_string());

    restore_password(previous).await;
}
