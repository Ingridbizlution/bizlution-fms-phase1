//! CORS。
//!
//! # 為什麼這個切片存在
//!
//! 在此之前這個 router **完全沒有 CORS 設定** —— `tower-http` 的 feature
//! 只開了 `["trace", "request-id", "util"]`，最外層只有 Trace 與 RequestId。
//!
//! 那不是「某些功能在瀏覽器裡不能用」，是**前端一個請求都發不出去**：
//! dev server（`localhost:3000`）對 API 的每一個跨來源請求都會在 preflight
//! 就失敗。而伺服器對伺服器的呼叫完全不受影響 —— 所以 507 格測試全綠、
//! `curl` 全通，而一個瀏覽器客戶端一行也跑不動。
//!
//! **這是「交付給前端團隊」與「交付不了」之間的差別**，而它沒有出現在任何
//! 測試、任何文件、甚至那份很完整的安全審查紀錄裡。
//!
//! # 這裡守的四件事
//!
//! 1. **未設定時不加這一層**（`a_`）—— 空清單是「拒絕」，不是「允許全部」。
//! 2. **設定的來源會拿到 CORS 標頭，沒設定的不會**（`b_`）。
//! 3. **preflight 不需要 `Authorization`**（`c_`）。`OPTIONS` 不帶認證，
//!    若它先經過 `require_auth` 會拿到 401 而不是 CORS 標頭 ——
//!    症狀是「明明設了 CORS 還是被擋」。這一格因此打的是一個**需認證**的端點。
//! 4. **四個非簡單標頭與 `ETag` 都在清單裡**（`d_`）。漏掉任何一個，
//!    對應的功能會在瀏覽器裡靜默失效：漏 `If-Match` → 所有樂觀鎖 PATCH 失敗；
//!    不 expose `ETag` → 前端讀不到版本號，拼不出下一次的 `If-Match`。
//!
//! # 沒有被覆蓋的
//!
//! **真實瀏覽器的行為。** 這裡驗的是伺服器回了正確的標頭；「瀏覽器據此放行」
//! 是規範保證的，不是這裡觀測到的。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;

const ORIGIN: &str = "http://localhost:3000";

async fn ctx_with_cors() -> TestContext {
    TestContext::setup_with(|s| {
        s.cors_allowed_origins = vec![ORIGIN.to_string()];
    })
    .await
}

/// `OPTIONS` preflight 請求。刻意不帶 `Authorization` —— 瀏覽器也不會帶。
fn preflight(uri: &str, origin: &str, method: &str, headers: &str) -> Request<Body> {
    Request::builder()
        .method("OPTIONS")
        .uri(uri)
        .header("origin", origin)
        .header("access-control-request-method", method)
        .header("access-control-request-headers", headers)
        .body(Body::empty())
        .unwrap()
}

fn header_of(res: &axum::response::Response, name: &str) -> Option<String> {
    res.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

// =============================================================================

/// 未設定 `CORS_ALLOWED_ORIGINS` 時**不加這一層**。
///
/// 空清單是「拒絕」而不是「允許全部」。這一格守的是那個預設：
/// 一份預設就放行所有來源的設定，會讓任何網站都能拿使用者的 token 打這個 API。
#[tokio::test]
async fn a_without_configuration_no_cors_headers_are_sent() {
    let ctx = &TestContext::setup().await; // cors_allowed_origins 預設為空

    let res = ctx
        .send_raw(preflight("/api/v1/health", ORIGIN, "GET", "authorization"))
        .await;
    assert!(
        header_of(&res, "access-control-allow-origin").is_none(),
        "沒有設定 CORS_ALLOWED_ORIGINS 卻回了 allow-origin —— 預設變成了放行"
    );

    // 一般請求也不該有。
    let res = ctx
        .send_raw(
            Request::builder()
                .uri("/api/v1/health")
                .header("origin", ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status(), StatusCode::OK, "health 本身該是 200");
    assert!(
        header_of(&res, "access-control-allow-origin").is_none(),
        "未設定時一般請求也不該有 CORS 標頭"
    );

    ctx.teardown().await;
}

/// 設定的來源拿到標頭，沒設定的來源拿不到。
#[tokio::test]
async fn b_only_configured_origins_get_the_headers() {
    let ctx = &ctx_with_cors().await;

    let res = ctx
        .send_raw(preflight("/api/v1/health", ORIGIN, "GET", "authorization"))
        .await;
    assert_eq!(
        header_of(&res, "access-control-allow-origin").as_deref(),
        Some(ORIGIN),
        "設定過的來源沒有拿到 allow-origin"
    );
    // 帶憑證是必要的：這個 API 用 Authorization 標頭。
    assert_eq!(
        header_of(&res, "access-control-allow-credentials").as_deref(),
        Some("true"),
        "沒有 allow-credentials —— 帶 Authorization 的請求會被瀏覽器擋掉"
    );

    // **未設定的來源拿不到。** 少了這一格，一個回 `*` 或「照抄請求來源」的
    // 實作也會讓上面全綠 —— 而那兩者都等於對全世界開放。
    let res = ctx
        .send_raw(preflight(
            "/api/v1/health",
            "https://evil.example.com",
            "GET",
            "authorization",
        ))
        .await;
    let got = header_of(&res, "access-control-allow-origin");
    assert!(
        got.is_none(),
        "未在清單裡的來源拿到了 allow-origin `{got:?}` —— 等於對全世界開放"
    );

    ctx.teardown().await;
}

/// preflight 打在**需認證的端點**上也要拿到 CORS 標頭。
///
/// `OPTIONS` 不帶 `Authorization`（瀏覽器不會帶）。若 CORS 層在
/// `require_auth` 之內，preflight 會拿到 401 而不是 CORS 標頭，
/// 於是瀏覽器擋掉真正的請求 —— 症狀是「明明設了 CORS 還是被擋」。
///
/// 這是這個切片裡最容易寫錯的一格，也是為什麼 CORS 層要加在最外層。
#[tokio::test]
async fn c_preflight_on_an_authenticated_endpoint_does_not_need_a_token() {
    let ctx = &ctx_with_cors().await;

    let res = ctx
        .send_raw(preflight(
            "/api/v1/work-orders",
            ORIGIN,
            "POST",
            "authorization,content-type,x-tenant-id,idempotency-key",
        ))
        .await;

    assert_ne!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "preflight 被要求認證了 —— CORS 層在 require_auth 之內，\
         瀏覽器會因此擋掉所有真正的請求"
    );
    assert_eq!(
        header_of(&res, "access-control-allow-origin").as_deref(),
        Some(ORIGIN),
        "需認證端點的 preflight 沒有拿到 allow-origin"
    );

    ctx.teardown().await;
}

/// 四個非簡單標頭與 `ETag` 都在清單裡。
///
/// 每一個漏掉都會讓對應的功能在瀏覽器裡**靜默失效**，而錯誤看起來像後端問題：
///
/// | 漏掉 | 症狀 |
/// |---|---|
/// | `X-Tenant-ID` | 所有需認證的請求都失敗（那個標頭是必填的） |
/// | `If-Match` | 所有樂觀鎖的 PATCH 失敗 |
/// | `Idempotency-Key` | 五個冪等端點的重送保護不能用 |
/// | `ETag`（expose） | 前端讀不到版本號，拼不出下一次的 `If-Match` |
#[tokio::test]
async fn d_every_header_this_api_actually_uses_is_allowed() {
    let ctx = &ctx_with_cors().await;

    let res = ctx
        .send_raw(preflight(
            "/api/v1/reservations",
            ORIGIN,
            "PATCH",
            "authorization,content-type,x-tenant-id,if-match,idempotency-key,x-request-id",
        ))
        .await;

    let allowed = header_of(&res, "access-control-allow-headers")
        .unwrap_or_default()
        .to_ascii_lowercase();
    for h in [
        "authorization",
        "content-type",
        "x-tenant-id",
        "if-match",
        "idempotency-key",
        "x-request-id",
    ] {
        assert!(
            allowed.contains(h),
            "`{h}` 不在 allow-headers 裡 —— 用到它的功能會在瀏覽器裡靜默失效。\
             實際回傳：`{allowed}`"
        );
    }

    let methods = header_of(&res, "access-control-allow-methods")
        .unwrap_or_default()
        .to_ascii_uppercase();
    for m in ["GET", "POST", "PATCH", "DELETE"] {
        assert!(
            methods.contains(m),
            "`{m}` 不在 allow-methods 裡：`{methods}`"
        );
    }

    // **`ETag` 必須 expose。** 瀏覽器預設只讓 JS 讀到六個回應標頭，
    // `ETag` 不在其中 —— 不 expose 的話樂觀鎖從客戶端的角度就是壞的。
    //
    // 驗在**真實的 GET 回應**上，不是 preflight：`expose-headers` 出現在
    // 實際回應裡，而那也才是前端真的會讀到的地方。
    let admin = ctx.login_as(USERNAME).await;
    let res = ctx
        .send_raw({
            let (mut parts, body) = authed(
                Request::builder()
                    .uri("/api/v1/tenant")
                    .body(Body::empty())
                    .unwrap(),
                &admin,
            )
            .into_parts();
            parts.headers.insert("origin", ORIGIN.parse().unwrap());
            Request::from_parts(parts, body)
        })
        .await;
    assert_eq!(res.status(), StatusCode::OK, "GET /tenant 該是 200");
    let exposed = header_of(&res, "access-control-expose-headers")
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        exposed.contains("etag"),
        "ETag 沒有 expose —— 前端讀不到版本號，If-Match 因此拼不出來：`{exposed}`"
    );

    ctx.teardown().await;
}
