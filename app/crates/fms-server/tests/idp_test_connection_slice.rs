//! `POST /identity-providers/{providerId}/test-connection`。
//!
//! # 這一組真正在守什麼
//!
//! 這支端點是整個系統第一段「往呼叫端填的字串發網路請求」的程式碼，所以
//! 一半的測試不是在驗它會不會成功，而是在驗**它拒絕得夠嚴**：
//!
//!   * `d_` http:// 被擋（不是 https）
//!   * `e_` 解析到私有位址被擋（OIDC 路徑）
//!   * `h_` 解析到私有位址被擋（**LDAP 路徑**）—— 少了這一格，
//!     「測試 LDAP 連線」就是一支指定主機與埠的連線工具，也就是埠掃描器
//!
//! 另一半守的是**誠實**：契約寫「測試 LDAP bind」，而 Phase 1 bind 不了
//! （沒有 LDAP 客戶端，`ldap_bind_secret_ref` 又是密鑰服務的參照、沒有解析器
//! —— 伺服器手上沒有密碼）。`f_` 因此斷言 TCP 通的時候
//! `checks_not_performed` 裡**必須**有 `ldap_bind`。少了它，一個只做過 TCP
//! 連線的結果會被讀成「LDAP 設定驗過了」。
//!
//! # 沒有被這一組覆蓋的部分
//!
//! **OIDC 真的抓一份 discovery 文件回來這件事沒有端到端測試。** 閘門強制
//! https，而在測試裡起一個有效憑證的 TLS 伺服器需要簽發憑證並把根憑證塞進
//! 客戶端 —— 那是一整套機具。
//!
//! 取而代之：文件的判讀邏輯（issuer 相符、缺端點、非 JSON）在
//! `fms-identity` 的 `identity_providers::tests` 有六格單元測試，
//! 閘門的位址判斷在 `fms-shared` 的 `safe_http::tests` 有四格。
//! 這裡則驗**閘門真的接在端點上**（`d_`／`e_`／`h_`）。
//! 缺口是「HTTP 抓取那一段本身」，明寫在這裡而不是假裝有覆蓋。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

fn post(uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn provider_id(ctx: &TestContext, token: &str, code: &str) -> String {
    let (status, body) = ctx
        .send(authed(get("/api/v1/identity-providers?limit=50"), token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["code"] == json!(code))
        .unwrap_or_else(|| panic!("找不到 code = {code}：{body}"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn test_connection(ctx: &TestContext, token: &str, id: &str) -> (StatusCode, Value) {
    ctx.send(authed(
        post(&format!("/api/v1/identity-providers/{id}/test-connection")),
        token,
    ))
    .await
}

fn check_status(body: &Value, name: &str) -> Option<String> {
    body["checks"]
        .as_array()?
        .iter()
        .find(|c| c["name"] == json!(name))
        .map(|c| c["status"].as_str().unwrap_or_default().to_string())
}

fn check_detail(body: &Value, name: &str) -> String {
    body["checks"]
        .as_array()
        .and_then(|a| a.iter().find(|c| c["name"] == json!(name)))
        .and_then(|c| c["detail"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn not_performed(body: &Value, name: &str) -> bool {
    body["checks_not_performed"]
        .as_array()
        .map(|a| a.iter().any(|c| c["name"] == json!(name)))
        .unwrap_or(false)
}

/// 直接改資料庫。LDAP 的連線欄位是 `NOT_PATCHABLE`（它們沒有讀者 ——
/// 在這支端點之前），所以測試只能從這裡設。
async fn set_ldap_target(ctx: &TestContext, id: &str, host: &str, port: i32) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.identity_providers
            SET ldap_host = $2, ldap_port = $3, ldap_use_tls = false
          WHERE id = $1::uuid",
    )
    .bind(id)
    .bind(host)
    .bind(port)
    .execute(&mut *tx)
    .await
    .expect("set ldap target");
    tx.commit().await.expect("commit");
}

/// 換掉 `ldap_bind_secret_ref`。
///
/// 測試用的解析器（`common::test_secrets`）只認得一個參照，因此
/// 「解得開」與「解不開」兩種部署狀態由這個 helper 切換。
async fn set_ldap_secret_ref(ctx: &TestContext, id: &str, reference: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("UPDATE fms.identity_providers SET ldap_bind_secret_ref = $2 WHERE id = $1::uuid")
        .bind(id)
        .bind(reference)
        .execute(&mut *tx)
        .await
        .expect("set ldap_bind_secret_ref");
    tx.commit().await.expect("commit");
}

/// 換掉 `client_secret_ref`。見 [`set_ldap_secret_ref`]。
async fn set_client_secret_ref(ctx: &TestContext, id: &str, reference: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("UPDATE fms.identity_providers SET client_secret_ref = $2 WHERE id = $1::uuid")
        .bind(id)
        .bind(reference)
        .execute(&mut *tx)
        .await
        .expect("set client_secret_ref");
    tx.commit().await.expect("commit");
}

async fn set_discovery_url(ctx: &TestContext, id: &str, url: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("UPDATE fms.identity_providers SET discovery_url = $2 WHERE id = $1::uuid")
        .bind(id)
        .bind(url)
        .execute(&mut *tx)
        .await
        .expect("set discovery_url");
    tx.commit().await.expect("commit");
}

// =============================================================================

/// LOCAL 來源回 `NOT_TESTABLE`，**不是** `PASSED`。
///
/// 一個空的檢查清單搭配 `checks.iter().all(passed)` 會回傳 true ——
/// 那是這支端點最容易寫錯的地方，而症狀是「測試通過」這句話失去意義。
#[tokio::test]
async fn a_local_provider_is_not_testable_not_passed() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "local").await;

    let (status, body) = test_connection(ctx, &admin, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["result"],
        json!("NOT_TESTABLE"),
        "LOCAL 來源沒有外部系統可測，卻回了 {}：{body}",
        body["result"]
    );
    assert!(
        not_performed(&body, "external_connection"),
        "沒有說明為什麼測不了：{body}"
    );
    assert_eq!(body["checks"], json!([]));
    // 這支端點不寫任何東西 —— 說出來，因為「測試連線」聽起來像會更新狀態。
    assert_eq!(body["meta"]["read_only"], json!(true));

    ctx.teardown().await;
}

/// SAML2 也是 `NOT_TESTABLE`，理由要指名 metadata 是個參照。
#[tokio::test]
async fn b_saml2_says_why_it_cannot_be_tested() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, created) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/identity-providers")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "code": "saml-test",
                        "name": "SAML 測試來源",
                        "provider_type": "SAML2"
                    })
                    .to_string(),
                ))
                .unwrap(),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap();

    let (status, body) = test_connection(ctx, &admin, id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"], json!("NOT_TESTABLE"), "{body}");
    assert!(not_performed(&body, "saml_metadata_fetch"), "{body}");

    ctx.teardown().await;
}

/// `http://` 被擋 —— 閘門的第 1 道防線真的接在端點上。
///
/// 訊息必須說出是 scheme 的問題。回一個籠統的「連線失敗」會讓整合的人去
/// 查防火牆，而答案是網址開頭要改成 https。
#[tokio::test]
async fn d_plain_http_targets_are_refused() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "entra-hq").await;

    // 主機名刻意是不存在的 `.invalid` —— 若實作先解析 DNS 再檢查 scheme，
    // 這一格會變成「解析失敗」，也就是說錯誤訊息會誤導。
    set_discovery_url(
        ctx,
        &id,
        "http://idp.invalid/.well-known/openid-configuration",
    )
    .await;
    // 種子的 entra-hq 沒有 client_secret_ref，因此預設不會產生
    // secret_reference_resolvable 那一格。這裡補上一個解得開的參照 ——
    // 下面才有「一格與網路無關的結果」可以驗它有沒有被早退吃掉。
    set_client_secret_ref(ctx, &id, "kv/fms/resolvable").await;

    let (status, body) = test_connection(ctx, &admin, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"], json!("FAILED"), "{body}");
    assert_eq!(
        check_status(&body, "target_is_permitted").as_deref(),
        Some("FAILED"),
        "{body}"
    );
    let detail = check_detail(&body, "target_is_permitted");
    assert!(
        detail.contains("https"),
        "訊息沒說出是 scheme 的問題：{detail}"
    );
    // **被擋下來不該把其他檢查的結果吃掉。** 早退曾經寫成 `vec![Check { … }]`，
    // 也就是丟掉前面已經算出來的每一格。密鑰參照解不解得開與網路目標無關，
    // 呼叫端同樣需要知道 —— 一次修好兩件事勝過修完一件再回來一次。
    assert!(
        check_status(&body, "secret_reference_resolvable").is_some(),
        "目標被擋下來時，與網路無關的 secret_reference_resolvable 被一起丟掉了：{body}"
    );

    ctx.teardown().await;
}

/// 解析到私有位址被擋（OIDC 路徑），而且訊息說得出是哪一類位址。
#[tokio::test]
async fn e_private_addresses_are_refused_on_the_oidc_path() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "entra-hq").await;

    for (url, expect) in [
        (
            "https://127.0.0.1/.well-known/openid-configuration",
            "loopback",
        ),
        // 雲端 metadata —— SSRF 最常見的目標。
        ("https://169.254.169.254/latest/meta-data/", "link-local"),
        ("https://10.1.2.3/.well-known/openid-configuration", "1918"),
    ] {
        set_discovery_url(ctx, &id, url).await;
        let (status, body) = test_connection(ctx, &admin, &id).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["result"],
            json!("FAILED"),
            "{url} 沒有被擋下來 —— 這是一條 SSRF 路徑：{body}"
        );
        let detail = check_detail(&body, "target_is_permitted");
        assert!(
            detail.contains(expect),
            "{url} 的拒絕理由沒說出是哪一類位址（預期含「{expect}」）：{detail}"
        );
    }

    ctx.teardown().await;
}

/// LDAP：TCP 通，但**必須明說 bind 沒有被驗證**。
///
/// 這一格是這一組裡最重要的：契約寫的是「測試 LDAP bind」，而我們只做了
/// TCP 連線。回一個沒有 `checks_not_performed` 的成功結果，等於宣稱驗過了
/// 一件根本沒做的事。
#[tokio::test]
async fn f_ldap_tcp_probe_passes_but_bind_is_declared_unverified() {
    // 先起模擬伺服器，才知道要放行哪一個 host:port。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock ldap");
    let port = listener.local_addr().unwrap().port();
    // 接受連線但不說話 —— 我們測的就是「有東西在聽」，不是 LDAP 協定。
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });

    let allow = format!("127.0.0.1:{port}");
    let ctx = &TestContext::setup_with(|s| {
        s.outbound.private_target_allowlist = vec![allow.clone()];
    })
    .await;
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "ad-cinema").await;
    set_ldap_target(ctx, &id, "127.0.0.1", port as i32).await;
    // 種子的參照在測試部署裡解不開（那是 `l_` 的題目）。這一格測的是
    // 「TCP 通、密鑰也齊備，但 bind 仍然沒被驗證」—— 也就是整體 PASSED
    // 卻必須明說 bind 沒做的那個狀態。
    set_ldap_secret_ref(ctx, &id, "kv/fms/resolvable").await;

    let (status, body) = test_connection(ctx, &admin, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        check_status(&body, "tcp_reachable").as_deref(),
        Some("PASSED"),
        "模擬伺服器在聽，TCP 檢查卻失敗：{body}"
    );
    assert_eq!(
        check_status(&body, "secret_reference_resolvable").as_deref(),
        Some("PASSED"),
        "參照在這個部署裡是解得開的，卻回 FAILED：{body}"
    );
    // **解得開的那條路徑才是密鑰真的存在於記憶體裡的時候。** 這條斷言守的是
    // 「有人為了好除錯，把解析出來的值塞進 detail」—— 那會讓 IdP 的
    // client_secret 出現在一支任何 identity_provider:write 都叫得動的端點回應裡。
    assert!(
        !check_detail(&body, "secret_reference_resolvable").contains("test-secret-value"),
        "detail 出現了解析出來的密鑰值：{body}"
    );
    assert_eq!(body["result"], json!("PASSED"), "{body}");
    // 種子的 ad-cinema 有 bind DN **也有** secret ref，所以憑證那一格不該失敗。
    // （這一格是回頭補的：第一次跑時它是 FAILED，因為 009 只填了 bind DN ——
    //  一組有帳號沒密碼、永遠無法認證的設定。修了種子，並在 `k_` 保留對這個
    //  檢查的覆蓋。）
    assert_ne!(
        check_status(&body, "bind_credentials_configured").as_deref(),
        Some("FAILED"),
        "示範租戶的 LDAP 憑證設定不完整：{body}"
    );

    // **這兩格才是重點。**
    assert!(
        not_performed(&body, "ldap_bind"),
        "TCP 通就回 PASSED，卻沒有說 bind 沒被驗證 —— 那是在宣稱驗過了：{body}"
    );
    assert!(
        not_performed(&body, "tls_handshake"),
        "沒有說 TLS 交握沒被驗證：{body}"
    );
    let reason = body["checks_not_performed"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!("ldap_bind"))
        .unwrap()["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("密碼") || reason.contains("secret"),
        "理由沒說出根本原因（伺服器手上沒有可以 bind 的密碼）：{reason}"
    );

    // 因為白名單才通過，這件事要回報 —— 它與一般的成功不同。
    assert_eq!(
        body["target"]["allowed_by_private_target_allowlist"],
        json!(true),
        "{body}"
    );

    ctx.teardown().await;
}

/// LDAP：沒有東西在聽 → `tcp_reachable` FAILED，整體 FAILED。
#[tokio::test]
async fn g_ldap_unreachable_port_fails() {
    // 綁一個埠再放掉，取得一個幾乎確定沒人在聽的埠號。
    let port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };

    let allow = format!("127.0.0.1:{port}");
    let ctx = &TestContext::setup_with(|s| {
        s.outbound.private_target_allowlist = vec![allow.clone()];
        // 連不上要快 —— 預設 5 秒會讓這一格拖慢整個 job。
        s.outbound.connect_timeout = std::time::Duration::from_millis(500);
    })
    .await;
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "ad-cinema").await;
    set_ldap_target(ctx, &id, "127.0.0.1", port as i32).await;

    let (status, body) = test_connection(ctx, &admin, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        check_status(&body, "tcp_reachable").as_deref(),
        Some("FAILED"),
        "{body}"
    );
    assert_eq!(body["result"], json!("FAILED"), "{body}");

    ctx.teardown().await;
}

/// **LDAP 路徑同樣受閘門管。**
///
/// 少了這一格，`ldap_host` 就是一個可以指向任何內部主機與埠的欄位，
/// 而 `tcp_reachable` 的通過／失敗與耗時就是掃描結果。
#[tokio::test]
async fn h_ldap_private_targets_are_refused_without_the_allowlist() {
    let ctx = &TestContext::setup().await; // 白名單是空的
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "ad-cinema").await;

    // Postgres 自己就在這個埠上 —— 一個真實存在、絕對不該被探測的目標。
    set_ldap_target(ctx, &id, "127.0.0.1", 5432).await;

    let (status, body) = test_connection(ctx, &admin, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"], json!("FAILED"), "{body}");
    assert_eq!(
        check_status(&body, "target_is_permitted").as_deref(),
        Some("FAILED"),
        "LDAP 路徑沒有經過位址檢查 —— 這支端點成了埠掃描器：{body}"
    );
    // 沒有做過任何連線，所以不該出現 tcp_reachable。
    assert_eq!(
        check_status(&body, "tcp_reachable"),
        None,
        "被閘門擋下之後還是連了：{body}"
    );

    ctx.teardown().await;
}

/// 需要 `identity_provider:write` —— 讀權限不夠。
///
/// 這支端點會讓伺服器對外發出網路請求，那是一個副作用；
/// 而 `identity_provider:read` 是給看設定的人的。
#[tokio::test]
async fn i_write_permission_is_required() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "local").await;

    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, body) = test_connection(ctx, &fm, &id).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "沒有 identity_provider:write 的人測得動：{body}"
    );

    ctx.teardown().await;
}

/// 不存在的 id 回 404。
#[tokio::test]
async fn j_missing_provider_is_a_404() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, _) = test_connection(ctx, &admin, "00000000-0000-4000-8000-000000000000").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// 有 bind DN 卻沒有 secret ref → `bind_credentials_configured` FAILED。
///
/// 那是一組有帳號沒密碼的設定：TCP 通、看起來一切正常，而等目錄客戶端接上時
/// 才會發現認證不了。這支端點的價值就在於提早說出來。
///
/// **這個檢查是被種子資料抓出來的**：009 原本只填 `ldap_bind_dn`，
/// 於是 `f_` 第一次跑就是 FAILED。種子已修，這一格接手覆蓋這條路徑。
#[tokio::test]
async fn k_bind_dn_without_a_secret_ref_is_reported() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock ldap");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });

    let allow = format!("127.0.0.1:{port}");
    let ctx = &TestContext::setup_with(|s| {
        s.outbound.private_target_allowlist = vec![allow.clone()];
    })
    .await;
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "ad-cinema").await;
    set_ldap_target(ctx, &id, "127.0.0.1", port as i32).await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.identity_providers SET ldap_bind_secret_ref = NULL WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("clear secret ref");
        tx.commit().await.expect("commit");
    }

    let (status, body) = test_connection(ctx, &admin, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // TCP 是通的 —— 所以整體的 FAILED 只可能來自憑證那一格。
    assert_eq!(
        check_status(&body, "tcp_reachable").as_deref(),
        Some("PASSED"),
        "{body}"
    );
    assert_eq!(
        check_status(&body, "bind_credentials_configured").as_deref(),
        Some("FAILED"),
        "有 bind DN 沒有 secret ref 卻沒被回報：{body}"
    );
    assert_eq!(body["result"], json!("FAILED"), "{body}");

    ctx.teardown().await;
}

/// 「參照設了，但這個部署沒有提供對應的密鑰」必須是一格 **FAILED**，
/// 而且要說出**該設哪個環境變數**。
///
/// # 這一格補的是一個在此之前完全不可觀察的組態錯誤
///
/// 在有解析器之前，`ldap_bind_secret_ref` 沒有任何讀者：填什麼都一樣，
/// 而「IdP 上設了參照、部署忘了提供密鑰」要等到有人真的去 bind 才會炸 ——
/// 那時症狀出現在遠端。
///
/// # 為什麼一定要驗到環境變數名出現在回應裡
///
/// 「解不開」這三個字對維運沒有用：他需要知道要設什麼。
/// 命名規則（非英數字換底線、大寫、加 `IDP_SECRET_` 前綴）寫在
/// `fms_shared::secrets` 裡，而回應把結果直接說出來就不必去讀那份文件。
#[tokio::test]
async fn l_an_unresolvable_secret_reference_fails_and_names_the_env_var() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock ldap");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });

    let allow = format!("127.0.0.1:{port}");
    let ctx = &TestContext::setup_with(|s| {
        s.outbound.private_target_allowlist = vec![allow.clone()];
    })
    .await;
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "ad-cinema").await;
    set_ldap_target(ctx, &id, "127.0.0.1", port as i32).await;
    set_ldap_secret_ref(ctx, &id, "kv/fms/ad-cinema-bind").await;

    let (status, body) = test_connection(ctx, &admin, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // TCP 是通的，所以整體 FAILED 只可能來自密鑰那一格。
    assert_eq!(
        check_status(&body, "tcp_reachable").as_deref(),
        Some("PASSED"),
        "{body}"
    );
    assert_eq!(
        check_status(&body, "secret_reference_resolvable").as_deref(),
        Some("FAILED"),
        "這個部署沒有提供密鑰，卻沒有回報：{body}"
    );
    assert_eq!(body["result"], json!("FAILED"), "{body}");

    let detail = check_detail(&body, "secret_reference_resolvable");
    assert!(
        detail.contains("IDP_SECRET_KV_FMS_AD_CINEMA_BIND"),
        "沒有說出該設哪個環境變數，維運看不出要做什麼：{detail}"
    );
    // 而且不能洩漏值 —— 這裡本來就沒有值可洩漏，但這條斷言守住的是
    // 「未來有人把 Secret 印進 detail」這件事。
    assert!(
        !detail.contains("test-secret-value"),
        "detail 出現了密鑰值：{detail}"
    );

    ctx.teardown().await;
}

/// OIDC 的 public client（沒有 `client_secret_ref`）不該被回報成 FAILED。
///
/// 這條守的是**誤報**：`identity_providers` 沒有任何一欄分得出 confidential
/// 與 public client，所以「沒設 client_secret_ref」既可能是漏填、也可能是
/// PKCE-only 的正常組態。回 FAILED 會讓一個正確的設定看起來壞了 ——
/// 與 `tls_handshake` 不做的理由完全相同。
#[tokio::test]
async fn m_a_provider_without_a_secret_ref_is_not_performed_not_failed() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = provider_id(ctx, &admin, "entra-hq").await;
    // 種子目前本來就是 NULL，這一段是為了讓這條測試**不依賴種子** ——
    // 哪天 009 幫 entra-hq 填了參照，這條測的東西才不會悄悄變成別的。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.identity_providers SET client_secret_ref = NULL WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("clear client_secret_ref");
        tx.commit().await.expect("commit");
    }

    let (status, body) = test_connection(ctx, &admin, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        check_status(&body, "secret_reference_resolvable"),
        None,
        "沒設參照時不該有這一格檢查（那會是誤報）：{body}"
    );
    assert!(
        not_performed(&body, "secret_reference_resolvable"),
        "沒設參照時必須放進 checks_not_performed —— 否則呼叫端看不出這格為何消失：{body}"
    );

    ctx.teardown().await;
}
