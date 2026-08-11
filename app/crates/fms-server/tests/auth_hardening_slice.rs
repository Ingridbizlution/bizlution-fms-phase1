//! 登入強化的端到端測試（`docs/security-review-open-items.md` 第 4 項）。
//!
//! 三件事各自都可能在「看起來有寫」的情況下失效，因此都由行為驗證而非
//! 讀程式碼確認：
//!
//!   1. **`auth_events` 真的有列落地**。這裡最容易出錯的不是 INSERT 寫錯，
//!      而是被 RLS 靜默擋掉（024 之前每一筆都被擋）或跟著失敗的認證交易
//!      一起回滾。兩種失敗都不會有任何錯誤浮上來 —— 只會沒有列。
//!   2. **節流跨請求生效**。計數器若隨 `IdentityState` 的 clone 各自一份，
//!      每個請求都是新的空計數，程式碼看起來完全正確而節流從不觸發。
//!   3. **帳號不存在與密碼錯誤耗時相當**。這是唯一只能用時間量的性質。
//!
//! 每個測試各自一份資料庫與一份 `IdentityState`（見 `common/mod.rs`），
//! 因此彼此的失敗計數與事件列不會互相污染。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestContext, TENANT_CODE, TENANT_ID, TEST_PASSWORD, USERNAME};
use serde_json::json;
use sqlx::Row;

/// 送一次 password grant。`user_agent` 一併帶上，因為它會進 `auth_events`。
fn login_request(tenant_code: &str, username: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/auth/token")
        .header("content-type", "application/json")
        .header("user-agent", "fms-test-agent/1.0")
        .body(Body::from(
            json!({
                "grant_type": "password",
                "tenant_code": tenant_code,
                "username": username,
                "password": password,
            })
            .to_string(),
        ))
        .unwrap()
}

/// 一列 `auth_events`，只取斷言用得到的欄位。
struct Event {
    tenant_id: Option<uuid::Uuid>,
    user_id: Option<uuid::Uuid>,
    event_type: String,
    result: String,
    failure_reason: Option<String>,
    user_agent: Option<String>,
}

/// 讀出本測試資料庫裡的全部登入事件。
///
/// 必須用**平台情境**：`tenant_id` 為 NULL 的列（tenant_code 解析失敗那種）
/// 依 007 的 `tenant_isolation` 只有平台情境讀得到 —— 這本身就是設計的一部分，
/// 用租戶情境查會看不到它們而誤判成「沒有寫入」。
async fn events(ctx: &TestContext) -> Vec<Event> {
    let mut tx = ctx.owner_tx().await;
    let rows = sqlx::query(
        "SELECT tenant_id, user_id, event_type, result, failure_reason, user_agent
           FROM fms.auth_events
          ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("read auth_events");

    rows.into_iter()
        .map(|r| Event {
            tenant_id: r.get("tenant_id"),
            user_id: r.get("user_id"),
            event_type: r.get("event_type"),
            result: r.get("result"),
            failure_reason: r.get("failure_reason"),
            user_agent: r.get("user_agent"),
        })
        .collect()
}

#[tokio::test]
async fn successful_login_is_recorded() {
    let ctx = TestContext::setup().await;

    let (status, body) = ctx
        .send(login_request(TENANT_CODE, USERNAME, TEST_PASSWORD))
        .await;
    assert_eq!(status, StatusCode::OK, "login failed: {body}");
    let user_id = body["user_id"].as_str().unwrap().to_string();

    let rows = events(&ctx).await;
    assert_eq!(rows.len(), 1, "一次成功登入應恰好留下一列");
    let e = &rows[0];
    assert_eq!(e.event_type, "LOGIN_SUCCESS");
    assert_eq!(e.result, "SUCCESS");
    assert_eq!(e.failure_reason, None, "成功的列不該有失敗原因");
    assert_eq!(
        e.tenant_id.map(|t| t.to_string()).as_deref(),
        Some(TENANT_ID)
    );
    assert_eq!(e.user_id.map(|u| u.to_string()).as_deref(), Some(&*user_id));
    assert_eq!(e.user_agent.as_deref(), Some("fms-test-agent/1.0"));

    ctx.teardown().await;
}

#[tokio::test]
async fn wrong_password_is_recorded_with_tenant_and_user() {
    let ctx = TestContext::setup().await;

    let (status, _) = ctx
        .send(login_request(TENANT_CODE, USERNAME, "definitely-not-it"))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let rows = events(&ctx).await;
    assert_eq!(rows.len(), 1, "失敗登入必須留下一列（回滾掉就等於沒記）");
    let e = &rows[0];
    assert_eq!(e.event_type, "LOGIN_FAILED");
    assert_eq!(e.result, "FAILURE");
    assert_eq!(e.failure_reason.as_deref(), Some("BAD_PASSWORD"));
    // 帶得出 tenant_id 與 user_id 才有用：租戶的管理員要能看到
    // 「是哪一個帳號被試密碼」，而那需要這一列落在他自己的租戶裡。
    assert_eq!(
        e.tenant_id.map(|t| t.to_string()).as_deref(),
        Some(TENANT_ID)
    );
    assert!(e.user_id.is_some(), "使用者存在時應記下 user_id");

    ctx.teardown().await;
}

#[tokio::test]
async fn unknown_tenant_is_recorded_without_a_tenant_id() {
    let ctx = TestContext::setup().await;

    let (status, _) = ctx
        .send(login_request("NO_SUCH_TENANT", USERNAME, TEST_PASSWORD))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 這一列是 024 的政策唯一能讓它落地的情況：無租戶情境、無 tenant_id。
    // 024 之前它會被 tenant_isolation 的 WITH CHECK 擋掉。
    let rows = events(&ctx).await;
    assert_eq!(rows.len(), 1, "租戶不存在的嘗試也必須留下痕跡");
    let e = &rows[0];
    assert_eq!(e.event_type, "LOGIN_FAILED");
    assert_eq!(e.failure_reason.as_deref(), Some("TENANT_NOT_FOUND"));
    assert_eq!(e.tenant_id, None);
    assert_eq!(e.user_id, None);

    ctx.teardown().await;
}

#[tokio::test]
async fn unknown_user_is_recorded_with_tenant_but_no_user() {
    let ctx = TestContext::setup().await;

    let (status, _) = ctx
        .send(login_request(TENANT_CODE, "no.such.person", TEST_PASSWORD))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let rows = events(&ctx).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].failure_reason.as_deref(), Some("USER_NOT_FOUND"));
    assert_eq!(
        rows[0].tenant_id.map(|t| t.to_string()).as_deref(),
        Some(TENANT_ID),
        "租戶已知時這一列要落在該租戶，否則管理員看不到有人在猜他的帳號"
    );
    assert_eq!(rows[0].user_id, None);

    ctx.teardown().await;
}

#[tokio::test]
async fn repeated_failures_are_throttled_with_retry_after() {
    let ctx = TestContext::setup().await;
    let max = ctx.max_login_failures();

    // 門檻內的失敗一律 401：節流不該提前生效，否則正常的打錯字就被擋。
    for n in 1..=max {
        let (status, _) = ctx
            .send(login_request(TENANT_CODE, USERNAME, "wrong"))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "第 {n} 次失敗應是 401");
    }

    // 累積到門檻後，下一次嘗試連密碼都不會被驗證。
    let res = ctx
        .send_raw(login_request(TENANT_CODE, USERNAME, "wrong"))
        .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = res
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("契約的 TooManyRequests 回應帶 Retry-After");
    assert!(retry_after >= 1, "Retry-After 為 0 等於叫對方立刻重試");

    // 被擋掉的嘗試刻意不寫 auth_events（見 handlers 的說明），
    // 因此列數應停在門檻，不隨後續被擋的請求成長。
    let rows = events(&ctx).await;
    assert_eq!(rows.len(), max as usize, "被節流擋掉的請求不應各自再寫一列");

    // 正確的密碼此時也被擋 —— 這是節流的代價，明確寫下來以免日後被當成 bug。
    let (status, _) = ctx
        .send(login_request(TENANT_CODE, USERNAME, TEST_PASSWORD))
        .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "門檻用盡後連正確密碼也擋；要放行必須等窗到期"
    );

    ctx.teardown().await;
}

#[tokio::test]
async fn a_successful_login_resets_the_failure_count() {
    let ctx = TestContext::setup().await;
    let max = ctx.max_login_failures();
    assert!(max >= 2, "本測試需要門檻至少 2 才有意義");

    // 停在門檻之前，否則連正確密碼都進不去。
    for _ in 0..(max - 1) {
        let (status, _) = ctx
            .send(login_request(TENANT_CODE, USERNAME, "wrong"))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    let (status, _) = ctx
        .send(login_request(TENANT_CODE, USERNAME, TEST_PASSWORD))
        .await;
    assert_eq!(status, StatusCode::OK);

    // 歸零後再失敗同樣的次數，仍不該被擋：若成功沒有清掉計數，
    // 累計就是 2×(max-1) ≥ max，這一輪的最後一次會變成 429。
    for n in 1..max {
        let (status, _) = ctx
            .send(login_request(TENANT_CODE, USERNAME, "wrong"))
            .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "成功登入應已清掉計數，第 {n} 次失敗不該是 429"
        );
    }

    ctx.teardown().await;
}

#[tokio::test]
async fn unknown_account_takes_as_long_as_a_wrong_password() {
    let ctx = TestContext::setup().await;

    // 取多次的**最小值**而非平均：要問的是「這條路徑最快能多快」，
    // 而 min 是對雜訊最不敏感的統計量。CI 上的干擾只會讓時間變長，
    // 也就是往安全的方向偏 —— 會讓測試誤過，不會讓它誤敗。
    let mut existing = u128::MAX;
    let mut unknown = u128::MAX;
    for i in 0..2 {
        let t = std::time::Instant::now();
        let (status, _) = ctx
            .send(login_request(TENANT_CODE, USERNAME, "wrong"))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        existing = existing.min(t.elapsed().as_micros());

        // 每輪換一個不存在的 username：同一個會累積失敗計數，
        // 而被節流擋掉的請求不跑 argon2，量到的就不是我們要比的東西。
        let t = std::time::Instant::now();
        let (status, _) = ctx
            .send(login_request(
                TENANT_CODE,
                &format!("no.such.person.{i}"),
                "wrong",
            ))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        unknown = unknown.min(t.elapsed().as_micros());
    }

    // argon2 的預設參數是數十毫秒，登入路徑上其他工作合計不到數毫秒，
    // 因此少跑一次 argon2 的差距是一個數量級。門檻取 1/3 留足雜訊空間：
    // 修正存在時比值約 1，缺少時約 1/10 以上。
    assert!(
        unknown * 3 > existing,
        "帳號不存在時明顯較快（{unknown}µs vs {existing}µs）—— \
         時間側通道仍可用來枚舉帳號"
    );

    ctx.teardown().await;
}
