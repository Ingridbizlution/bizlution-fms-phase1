//! `POST /auth/logout` 與 `POST /auth/password/change`，以及它們依賴的
//! refresh token 輪替（migration 070）。
//!
//! # 這一組真正在守什麼
//!
//! 契約說 logout 撤銷 refresh token。要讓那句話是真的，需要三件事同時成立，
//! 而其中兩件從端點的回應**看不出來**：
//!
//!   1. 撤銷後那個 token 不能再換發（`a_`）。
//!   2. 換發會消耗舊 token（`b_`）。少了這一步，logout 只殺掉客戶端手上最後
//!      那一個，換發鏈上先前的 token 全都還活著 —— 而 logout 仍然回 200。
//!   3. 清理不會把還有效的撤銷刪掉（`h_`）。清理是為了控制表的成長，但它刪的
//!      是「撤銷紀錄」，刪錯的後果是**已登出的 token 復活**，而且沒有任何
//!      症狀 —— 直到有人拿舊 token 換發成功。
//!
//! 這三格都是「若壞掉，端點仍然回 200」的性質，所以只有測試看得見。
//!
//! # 誠實回報那一格（`g_`）
//!
//! 改密碼**不會**登出其他裝置（撤銷粒度是單一 token，見 070 檔頭）。`g_` 斷言
//! 的正是這個限制真的存在、而回應誠實地說了 —— 而不是假裝做到。
//! 哪一天補上 `tokens_valid_from`，這一格會失敗，那時該改的是這一格。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 登入並取回 **access 與 refresh 兩個** token。
///
/// `common::login_as` 只回 access token，而這一組測的東西全在 refresh 上。
async fn login_pair(ctx: &TestContext, username: &str) -> (String, String) {
    let (status, body) = ctx
        .send(json_request(
            "POST",
            "/api/v1/auth/token",
            json!({
                "grant_type": "password",
                "tenant_code": TENANT_CODE,
                "username": username,
                "password": TEST_PASSWORD
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "login failed: {body}");
    (
        body["access_token"].as_str().unwrap().to_string(),
        body["refresh_token"].as_str().unwrap().to_string(),
    )
}

async fn refresh(ctx: &TestContext, refresh_token: &str) -> (StatusCode, Value) {
    ctx.send(json_request(
        "POST",
        "/api/v1/auth/token/refresh",
        json!({"refresh_token": refresh_token}),
    ))
    .await
}

async fn logout(ctx: &TestContext, access: &str, refresh_token: &str) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request(
            "POST",
            "/api/v1/auth/logout",
            json!({"refresh_token": refresh_token}),
        ),
        access,
    ))
    .await
}

async fn change_password(
    ctx: &TestContext,
    access: &str,
    current: &str,
    new: &str,
) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request(
            "POST",
            "/api/v1/auth/password/change",
            json!({"current_password": current, "new_password": new}),
        ),
        access,
    ))
    .await
}

/// 這個租戶的 auth_events 裡某個型別有幾筆。
async fn auth_event_count(ctx: &TestContext, event_type: &str) -> i64 {
    let mut tx = ctx.owner_tx().await;
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM fms.auth_events WHERE event_type = $1 AND tenant_id = $2::uuid",
    )
    .bind(event_type)
    .bind(TENANT_ID)
    .fetch_one(&mut *tx)
    .await
    .expect("count auth_events")
}

// =============================================================================

/// 登出之後那個 refresh token 換不了 token。
///
/// **這一格就是契約那句「撤銷 refresh token」。** 070 之前它必定失敗：
/// 當時 refresh 只驗簽章，登出無論做什麼都不會影響它。
#[tokio::test]
async fn a_logout_makes_the_refresh_token_unusable() {
    let ctx = &TestContext::setup().await;

    let (access, refresh_token) = login_pair(ctx, USERNAME).await;

    // 撤銷之前是可以用的 —— 先確認這件事，否則下面的 401 可能是別的原因。
    let (before, body) = refresh(ctx, &refresh_token).await;
    assert_eq!(before, StatusCode::OK, "撤銷前就換不了：{body}");
    // 上一步已經把 refresh_token 輪替掉了，所以拿它換到的那一個來登出。
    let rotated = body["refresh_token"].as_str().unwrap().to_string();

    let (status, body) = logout(ctx, &access, &rotated).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revoked"], json!(true));
    assert_eq!(body["already_revoked"], json!(false));
    // access token 不在撤銷範圍內，回應必須說出來（客戶端要自己清本機那一份）。
    assert!(
        body["access_token_remains_valid_for_seconds"]
            .as_i64()
            .unwrap()
            > 0,
        "回應沒有說 access token 還有效多久：{body}"
    );

    let (after, body) = refresh(ctx, &rotated).await;
    assert_eq!(
        after,
        StatusCode::UNAUTHORIZED,
        "登出之後 refresh token 仍然換得到 token —— 撤銷沒有生效：{body}"
    );

    // 認證軌要留下這件事。
    assert_eq!(auth_event_count(ctx, "LOGOUT").await, 1);

    ctx.teardown().await;
}

/// 換發會消耗舊 token：舊的立刻失效，新的可用。
///
/// 少了輪替，logout 是可證明不完整的 —— 見模組說明第 2 點。
#[tokio::test]
async fn b_refresh_consumes_the_old_token() {
    let ctx = &TestContext::setup().await;

    let (_, first) = login_pair(ctx, USERNAME).await;

    let (status, body) = refresh(ctx, &first).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let second = body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(second, first, "換發回傳了同一個 refresh token");

    // 舊的不能再用。
    let (reused, body) = refresh(ctx, &first).await;
    assert_eq!(
        reused,
        StatusCode::UNAUTHORIZED,
        "已經換發過的 refresh token 還能再用 —— 沒有輪替：{body}"
    );

    // 新的可以用。
    let (fresh, body) = refresh(ctx, &second).await;
    assert_eq!(fresh, StatusCode::OK, "新發的 refresh token 不能用：{body}");

    ctx.teardown().await;
}

/// 重複登出是幂等的，而且能分辨「剛剛撤銷」與「早就撤銷」。
#[tokio::test]
async fn c_logout_is_idempotent_and_says_so() {
    let ctx = &TestContext::setup().await;

    let (access, refresh_token) = login_pair(ctx, USERNAME).await;

    let (s1, b1) = logout(ctx, &access, &refresh_token).await;
    assert_eq!(s1, StatusCode::OK, "{b1}");
    assert_eq!(b1["already_revoked"], json!(false));

    let (s2, b2) = logout(ctx, &access, &refresh_token).await;
    assert_eq!(s2, StatusCode::OK, "第二次登出不是 200：{b2}");
    assert_eq!(
        b2["already_revoked"],
        json!(true),
        "第二次登出沒有回報 already_revoked：{b2}"
    );
    // 兩次都說「這個 token 現在是撤銷狀態」—— 幂等的語意。
    assert_eq!(b2["revoked"], json!(true));

    // 重試不該在認證軌裡堆出一串看起來像「這個帳號一直在登出」的列。
    assert_eq!(
        auth_event_count(ctx, "LOGOUT").await,
        1,
        "重複登出寫了多筆 LOGOUT 事件"
    );

    ctx.teardown().await;
}

/// 別人的 refresh token 撤銷不了，而且那個 token 之後仍然可用。
///
/// 少了這一格，任何已登入的使用者拿到別人的 refresh token 字串就能把對方登出，
/// 而撤銷是不可撤回的（070 沒給 fms_app DELETE）—— 每一次都是無法復原的騷擾。
#[tokio::test]
async fn d_cannot_revoke_someone_elses_token() {
    let ctx = &TestContext::setup().await;

    let (admin_access, _) = login_pair(ctx, USERNAME).await;
    let (_, victim_refresh) = login_pair(ctx, USERNAME_REQUESTER).await;

    let (status, body) = logout(ctx, &admin_access, &victim_refresh).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "管理員撤銷了別人的 refresh token：{body}"
    );

    // 關鍵的後半：被拒絕之後那個 token 必須**還能用**。
    // 只斷言 403 會漏掉「先寫黑名單再檢查授權」這種寫法。
    let (still, body) = refresh(ctx, &victim_refresh).await;
    assert_eq!(
        still,
        StatusCode::OK,
        "越權登出被拒，但受害者的 token 已經失效：{body}"
    );

    assert_eq!(auth_event_count(ctx, "LOGOUT").await, 0);

    ctx.teardown().await;
}

/// 已輪替的 token 再被使用會留下 `TOKEN_REUSE`；已登出的不會。
///
/// 兩者對客戶端都是 401，但只有前者是 token 被複製的訊號
/// （RFC 6819 §5.2.2.3）。分不出來的話，這條軌就只是雜訊。
#[tokio::test]
async fn e_replayed_rotated_token_is_recorded_as_reuse() {
    let ctx = &TestContext::setup().await;

    let (access, first) = login_pair(ctx, USERNAME).await;
    let (status, body) = refresh(ctx, &first).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let second = body["refresh_token"].as_str().unwrap().to_string();

    assert_eq!(auth_event_count(ctx, "TOKEN_REUSE").await, 0);

    // 已被輪替掉的 token 再送一次。
    let (status, body) = refresh(ctx, &first).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(
        auth_event_count(ctx, "TOKEN_REUSE").await,
        1,
        "重播已輪替的 token 沒有留下 TOKEN_REUSE"
    );

    // 登出，然後拿已登出的 token 再送一次 —— 這一種不記。
    let (status, body) = logout(ctx, &access, &second).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = refresh(ctx, &second).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        auth_event_count(ctx, "TOKEN_REUSE").await,
        1,
        "已登出的 token 被重送也記成 TOKEN_REUSE —— 這條軌會被正常流量淹掉"
    );

    ctx.teardown().await;
}

/// 改密碼：四種拒絕，以及成功之後新舊密碼的效果。
///
/// 最短長度刻意**改成 20** 再測，而不是靠預設的 12：那樣才驗得到
/// `tenants.settings.password_min_length` 真的被讀了。用預設值測的話，
/// 一個把政策讀取整段刪掉、直接寫死 12 的實作會通過。
#[tokio::test]
async fn f_password_change_enforces_policy_and_actually_changes_it() {
    let ctx = &TestContext::setup().await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.tenants
                SET settings = coalesce(settings, '{}'::jsonb)
                             || '{\"password_min_length\": 20}'::jsonb
              WHERE id = $1::uuid",
        )
        .bind(TENANT_ID)
        .execute(&mut *tx)
        .await
        .expect("set password_min_length");
        tx.commit().await.expect("commit");
    }

    let (access, _) = login_pair(ctx, USERNAME).await;

    // (1) 現在的密碼不對 → 422（**不是** 401，見 handler 說明）。
    let (status, body) =
        change_password(ctx, &access, "wrong-password", "a-long-enough-password-x").await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "現有密碼錯誤沒有回 422：{body}"
    );
    assert_eq!(body["errors"][0]["pointer"], json!("/current_password"));

    // (2) 新密碼太短 → 422，而且門檻必須是租戶設的 20，不是預設的 12。
    let sixteen = "0123456789abcdef";
    assert_eq!(sixteen.len(), 16, "這個案例要在 12 與 20 之間");
    let (status, body) = change_password(ctx, &access, TEST_PASSWORD, sixteen).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "16 字元的密碼在 min_length=20 的租戶被接受了 —— 政策沒有被讀：{body}"
    );
    assert_eq!(body["errors"][0]["pointer"], json!("/new_password"));
    assert_eq!(body["errors"][0]["code"], json!("MINIMUM"));
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("20"),
        "錯誤訊息沒有說出真正生效的門檻：{body}"
    );

    // (3) 新舊相同 → 422。不擋的話會回 changed: true 而什麼都沒變。
    let (status, body) = change_password(ctx, &access, TEST_PASSWORD, TEST_PASSWORD).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "新舊密碼相同被接受了：{body}"
    );

    // (4) 成功。
    let new_password = "a-new-password-of-sufficient-length";
    let (status, body) = change_password(ctx, &access, TEST_PASSWORD, new_password).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], json!(true));
    assert_eq!(
        body["min_length_applied"],
        json!(20),
        "回應沒有說出這次套用的門檻：{body}"
    );

    // 舊密碼登不進、新密碼可以。
    let (status, _) = ctx
        .send(json_request(
            "POST",
            "/api/v1/auth/token",
            json!({
                "grant_type": "password",
                "tenant_code": TENANT_CODE,
                "username": USERNAME,
                "password": TEST_PASSWORD
            }),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "改了密碼之後舊密碼還能登入"
    );

    let (status, body) = ctx
        .send(json_request(
            "POST",
            "/api/v1/auth/token",
            json!({
                "grant_type": "password",
                "tenant_code": TENANT_CODE,
                "username": USERNAME,
                "password": new_password
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "新密碼登不進去：{body}");

    // `password_updated_at` 在這支端點之前**全專案沒有寫入者**（002 建了欄位，
    // 沒有一處 UPDATE 它）。這一格是它的第一個讀者。
    let mut tx = ctx.owner_tx().await;
    let updated: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT password_updated_at FROM fms.users WHERE id = $1::uuid")
            .bind(ADMIN_USER_ID)
            .fetch_one(&mut *tx)
            .await
            .expect("read password_updated_at");
    drop(tx);
    assert!(
        updated.is_some(),
        "password_updated_at 沒有被寫入 —— 那一欄仍然沒有寫入者"
    );

    assert_eq!(auth_event_count(ctx, "PASSWORD_CHANGED").await, 1);

    ctx.teardown().await;
}

/// 改密碼**不會**讓其他裝置的 refresh token 失效，而回應誠實地說了。
///
/// 這是 070 那個決策（per-token 撤銷）的邊界。斷言它存在，是為了讓
/// 「回應說 true 但其實已經撤銷了」與「回應說 true 而確實沒撤銷」分開 ——
/// 前者是文件錯，後者是設計決策。
#[tokio::test]
async fn g_password_change_does_not_revoke_other_sessions() {
    let ctx = &TestContext::setup().await;

    // 兩個「裝置」。
    let (access_a, _) = login_pair(ctx, USERNAME).await;
    let (_, refresh_b) = login_pair(ctx, USERNAME).await;

    let new_password = "another-sufficiently-long-password";
    let (status, body) = change_password(ctx, &access_a, TEST_PASSWORD, new_password).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["other_sessions_remain_valid"],
        json!(true),
        "回應沒有說出這個限制：{body}"
    );

    // 而它真的成立 —— 另一台的 refresh token 仍然換得到 token。
    let (status, body) = refresh(ctx, &refresh_b).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "回應說其他 session 仍有效，實際上已經失效：{body}"
    );

    ctx.teardown().await;
}

/// 清理只刪過期的列，還有效的撤銷刪不掉。
///
/// **刪錯的後果是已登出的 token 復活，而且沒有任何症狀。** 所以這一格不是
/// 「驗一下清理有沒有跑」，而是驗它的判準。
#[tokio::test]
async fn h_purge_removes_only_expired_revocations() {
    let ctx = &TestContext::setup().await;

    let (access, refresh_token) = login_pair(ctx, USERNAME).await;
    let (status, body) = logout(ctx, &access, &refresh_token).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 塞一列已過期的（jti 是隨機的，不對應任何真 token —— 清理只看 expires_at）。
    let expired_jti = uuid::Uuid::new_v4();
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.revoked_refresh_tokens
                    (jti, tenant_id, user_id, expires_at, reason)
             VALUES ($1, $2::uuid, $3::uuid, now() - interval '1 day', 'ROTATED')",
        )
        .bind(expired_jti)
        .bind(TENANT_ID)
        .bind(ADMIN_USER_ID)
        .execute(&mut *tx)
        .await
        .expect("insert expired revocation");
        tx.commit().await.expect("commit");
    }

    // 順帶塞一列已過期的 SSO 授權請求。**073 的清理函式在 #47 合併時沒有
    // 任何呼叫者**（盤查「宣告了但沒有人讀」時發現），因此
    // `sso_auth_requests` 會無限成長。這一格現在同時守著那一半。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.sso_auth_requests
                    (tenant_id, identity_provider_id, state, nonce, pkce_verifier,
                     redirect_uri, expires_at)
             SELECT $1::uuid, p.id, 'expired-state-for-purge-test', 'n', 'v',
                    'https://fms.example.com/auth/sso/callback',
                    clock_timestamp() - interval '1 hour'
               FROM fms.identity_providers p
              WHERE p.tenant_id = $1::uuid AND p.deleted_at IS NULL
              LIMIT 1",
        )
        .bind(TENANT_ID)
        .execute(&mut *tx)
        .await
        .expect("insert expired sso request");
        tx.commit().await.expect("commit");
    }

    let purger = fms_worker::token_purge::TokenPurger::new(ctx.owner_pool().await);
    let counts = purger.run_once().await.expect("purge");

    // **先驗這一格。** 「刪掉幾列」是手段，「已登出的 token 沒有復活」是目的；
    // 把計數放前面會讓一個亂刪的清理在計數那裡就失敗，於是真正重要的那一格
    // 永遠不會被執行到（突變測試時實際發生過）。
    let (status, body) = refresh(ctx, &refresh_token).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "清理之後已登出的 token 又能換發了 —— 撤銷被清掉了：{body}"
    );

    assert_eq!(
        counts.revocations, 1,
        "清理刪掉的撤銷紀錄數不對（應該只有那一列過期的）"
    );
    // **兩個數字分開斷言。** 加總的話，「SSO 那一半根本沒有被呼叫」會被
    // 撤銷紀錄那一列蓋過去 —— 那正是這次要修的缺陷的症狀。
    assert_eq!(
        counts.sso_requests, 1,
        "過期的 SSO 授權請求沒有被清掉 —— 073 的清理函式又變成沒有呼叫者了"
    );

    // 過期的那一列不在了。
    let mut tx = ctx.owner_tx().await;
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fms.revoked_refresh_tokens WHERE jti = $1")
            .bind(expired_jti)
            .fetch_one(&mut *tx)
            .await
            .expect("count");
    drop(tx);
    assert_eq!(remaining, 0);

    // 過期的 SSO 請求也不在了。
    let mut tx = ctx.owner_tx().await;
    let remaining_sso: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.sso_auth_requests WHERE state = 'expired-state-for-purge-test'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("count sso");
    drop(tx);
    assert_eq!(remaining_sso, 0);

    // 幂等：再跑一次兩邊都沒有東西可刪。
    let again = purger.run_once().await.expect("purge again");
    assert_eq!(again.revocations, 0);
    assert_eq!(again.sso_requests, 0);

    ctx.teardown().await;
}

/// 沒有 jti 的 refresh token（070 上線前簽的）一律拒絕。
///
/// 這是 fail-closed 的那一格：放行的話，「已登出」與「還能換發」會同時成立，
/// 因為黑名單以 jti 為鍵，沒有 jti 就沒有可以寫進去的東西。
#[tokio::test]
async fn i_refresh_token_without_jti_is_rejected() {
    let ctx = &TestContext::setup().await;

    // 手簽一個 070 之前形狀的 refresh token：claims 完全合法，只是沒有 jti。
    // 用的是測試設定裡同一把密鑰，所以簽章會過 —— 被拒的理由只能是缺 jti。
    let now = chrono::Utc::now().timestamp();
    let legacy = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({
            "sub": ADMIN_USER_ID,
            "tid": TENANT_ID,
            "scope": "api",
            "typ": "refresh",
            "iat": now,
            "exp": now + 3600,
        }),
        &jsonwebtoken::EncodingKey::from_secret(test_settings("unused").jwt.secret.as_bytes()),
    )
    .expect("sign legacy token");

    let (status, body) = refresh(ctx, &legacy).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "沒有 jti 的 refresh token 被接受了 —— 那是一個撤銷不了的 token：{body}"
    );

    // 對照組：同樣手簽、但帶了 jti 的 token 要能用。
    // 少了這一格，上面的 401 可能只是「手簽的 token 一律不接受」。
    let with_jti = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({
            "sub": ADMIN_USER_ID,
            "tid": TENANT_ID,
            "scope": "api",
            "typ": "refresh",
            "iat": now,
            "exp": now + 3600,
            "jti": uuid::Uuid::new_v4(),
        }),
        &jsonwebtoken::EncodingKey::from_secret(test_settings("unused").jwt.secret.as_bytes()),
    )
    .expect("sign token with jti");

    let (status, body) = refresh(ctx, &with_jti).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "帶 jti 的手簽 token 也被拒 —— 上面那格的 401 不是因為缺 jti：{body}"
    );

    ctx.teardown().await;
}
