//! SCIM 2.0 供裝端點的行為驗證。
//!
//! # 這個切片要守住什麼
//!
//! 1. **token 只出現一次，之後永不可取**（`a_`）。074 只存 SHA-256，
//!    因此「回應裡再看到一次」在結構上就不可能 —— 但那要真的驗過。
//! 2. **`scim_enabled` 是真的開關**（`b_`）。管理者關掉它就該立刻拒絕所有
//!    SCIM 請求。只在端點層檢查、漏一條路徑，那個開關就是裝飾。
//! 3. **錯誤是 SCIM 的形狀，不是 problem+json**（`c_`）。Entra 解析的是前者；
//!    形狀錯了，所有失敗在管理者眼裡都是「未知錯誤」。
//! 4. **不支援的 filter 是錯誤，不是全表掃描**（`d_`）。這是最糟的失敗方式：
//!    忽略 filter 會讓「查一個人」變成「回傳全部人」。
//! 5. **外部目錄不能接管它沒有佈建的帳號**（`e_`、`g_`）。讀取範圍限定在
//!    發出請求的那個 provider，寫入也一樣。
//! 6. **停用與刪除對應到既有的狀態詞彙**（`f_`、`h_`）。
//! 7. **成員只能是此來源佈建的使用者**（`i_`）。否則 SCIM token 就是一條
//!    完整的提權路徑：把任何人塞進一個有角色對應的群組（058）。
//! 8. **不支援的 PATCH 路徑要報錯，不能靜默略過**（`j_`）。略過會讓 Entra
//!    的同步報告顯示成功，而那個屬性從未被寫入。
//!
//! # 沒有被覆蓋的
//!
//! * **Entra ID 的真實往返。** 沒有可對接的租戶，因此所有請求都是本測試
//!   自己組的。對 Entra 實際行為的讓步（`op` 大寫、`active` 是字串
//!   `"True"`、`members[value eq "…"]` 形式的移除）由 `scim.rs` 的單元測試
//!   釘住格式，但「Entra 真的送這些」這件事來自它的文件，不來自這裡的觀測。
//! * **`itemsPerPage` 大於資料量時的多頁行為。** 測試資料只有個位數列，
//!   `startIndex`／`count` 的邊界由單元測試 `start_index_is_one_based…` 覆蓋。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 建一個啟用 SCIM 的身分來源，並取得它的 token。
///
/// 回傳 `(provider_id, token)`。
async fn provision_scim(ctx: &TestContext, code: &str) -> (String, String) {
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/identity-providers")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "code": code,
                        "name": "Entra ID（測試）",
                        "provider_type": "OIDC",
                        "issuer": "https://login.microsoftonline.com/t/v2.0",
                        "client_id": "test-client"
                    })
                    .to_string(),
                ))
                .unwrap(),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "建立身分來源失敗：{body}");
    // POST 回的是裸資源（沒有 `data` 封裝），PATCH 回的是 `{data, meta}`。
    let provider_id = body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("建立回應沒有 id：{body}"))
        .to_string();

    let (status, body) = patch_provider(
        ctx,
        &admin,
        &provider_id,
        json!({ "scim_enabled": true, "rotate_scim_token": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "開啟 SCIM 失敗：{body}");
    let token = body["meta"]["scim_token"]
        .as_str()
        .expect("回應沒有 meta.scim_token")
        .to_string();

    (provider_id, token)
}

async fn patch_provider(
    ctx: &TestContext,
    admin: &str,
    provider_id: &str,
    body: Value,
) -> (StatusCode, Value) {
    ctx.send(authed(
        Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/identity-providers/{provider_id}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        admin,
    ))
    .await
}

/// SCIM 請求。**刻意不用 `authed()`** —— 那個 helper 會加 `X-Tenant-ID`，
/// 而 SCIM 端點不接受它（Entra 送不出來）。這裡只加 Authorization。
fn scim_req(method: &str, uri: &str, token: &str, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if body.is_some() {
        b = b.header("content-type", "application/scim+json");
    }
    b.body(match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    })
    .unwrap()
}

/// 回 `(status, content-type, body)`。content-type 要驗，因為 SCIM 規範要求
/// `application/scim+json`，而回 `application/json` 是靠對方寬鬆才不出事。
async fn scim_send(ctx: &TestContext, req: Request<Body>) -> (StatusCode, String, Value) {
    let res = ctx.send_raw(req).await;
    let status = res.status();
    let ct = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, ct, json)
}

async fn create_scim_user(ctx: &TestContext, token: &str, user_name: &str) -> Value {
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "POST",
            "/api/v1/scim/v2/Users",
            token,
            Some(json!({
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
                "userName": user_name,
                "externalId": format!("aad-{user_name}"),
                "displayName": format!("測試 {user_name}"),
                "name": { "givenName": "小明", "familyName": "王" },
                "emails": [{ "value": format!("{user_name}@corp.example.com"), "primary": true }],
                "active": true
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "建立使用者失敗：{body}");
    body
}

// =============================================================================

/// token 在回應裡出現**一次**，而且之後任何路徑都拿不回明文。
///
/// 這是 074 整個設計的目的。假如 GET 能再讀到它，「只存雜湊」就白做了。
#[tokio::test]
async fn a_the_token_is_shown_once_and_is_never_retrievable_again() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (provider_id, token) = provision_scim(ctx, "entra-once").await;

    assert_eq!(token.len(), 64, "token 應該是 64 個十六進位字元（256 bit）");
    assert!(
        token.chars().all(|c| c.is_ascii_hexdigit()),
        "token 含非十六進位字元：{token}"
    );

    // 再 PATCH 一次（不要求輪替）—— 回應不該有 token。
    let (status, body) =
        patch_provider(ctx, &admin, &provider_id, json!({ "name": "改個名" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["meta"]["scim_token"].is_null(),
        "沒要求輪替卻回了 token：{body}"
    );

    // 清單與單筆讀取都不該出現 token 或雜湊。
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .uri("/api/v1/identity-providers")
                .body(Body::empty())
                .unwrap(),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let dumped = body.to_string();
    assert!(
        !dumped.contains(&token),
        "清單端點回傳了 SCIM token 的明文：{dumped}"
    );

    // 資料庫裡也只有雜湊，沒有明文。
    let mut tx = ctx.owner_tx().await;
    sqlx::query("SELECT set_config('app.is_platform','on',true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    let (hash, prefix): (String, String) = sqlx::query_as(
        "SELECT token_hash, token_prefix FROM fms.scim_tokens WHERE revoked_at IS NULL",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_ne!(hash, token, "資料庫存的是明文，不是雜湊");
    assert_eq!(hash.len(), 64);
    assert_eq!(prefix, token[..8], "prefix 應該是明文的前 8 字元");
    drop(tx);

    // 輪替一次：舊的立刻失效，而不是兩個都能用。
    let (status, body) = patch_provider(
        ctx,
        &admin,
        &provider_id,
        json!({ "rotate_scim_token": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new_token = body["meta"]["scim_token"].as_str().unwrap().to_string();
    assert_ne!(new_token, token);

    let (status, _, _) =
        scim_send(ctx, scim_req("GET", "/api/v1/scim/v2/Users", &token, None)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "輪替之後舊 token 還能用 —— 撤銷沒生效"
    );
    let (status, _, _) = scim_send(
        ctx,
        scim_req("GET", "/api/v1/scim/v2/Users", &new_token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "新 token 不能用");

    ctx.teardown().await;
}

/// `scim_enabled = false` 之後，**同一個 token 立刻認不過**。
///
/// 這一格守的是「開關是真的開關」。074 把檢查放在
/// `authenticate_scim_token` 裡而不是端點層，正是為了讓所有路徑都受它管。
#[tokio::test]
async fn b_disabling_scim_on_the_provider_rejects_the_token_immediately() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (provider_id, token) = provision_scim(ctx, "entra-switch").await;

    let (status, _, _) =
        scim_send(ctx, scim_req("GET", "/api/v1/scim/v2/Users", &token, None)).await;
    assert_eq!(status, StatusCode::OK, "開啟時應該可用");

    let (status, body) =
        patch_provider(ctx, &admin, &provider_id, json!({ "scim_enabled": false })).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 每一支端點都要被擋 —— 一條漏掉的路徑就讓開關失去意義。
    for (method, uri) in [
        ("GET", "/api/v1/scim/v2/Users"),
        ("POST", "/api/v1/scim/v2/Users"),
        ("GET", "/api/v1/scim/v2/Groups"),
        ("POST", "/api/v1/scim/v2/Groups"),
    ] {
        let payload = (method == "POST").then(|| json!({ "userName": "x", "displayName": "x" }));
        let (status, _, _) = scim_send(ctx, scim_req(method, uri, &token, payload)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} 在 scim_enabled=false 之後仍然通過"
        );
    }

    // 身分來源被停用（status <> ACTIVE）同樣要擋。
    let (status, body) = patch_provider(
        ctx,
        &admin,
        &provider_id,
        json!({ "scim_enabled": true, "status": "DISABLED" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, _) =
        scim_send(ctx, scim_req("GET", "/api/v1/scim/v2/Users", &token, None)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "身分來源 DISABLED 但 SCIM 仍然可用"
    );

    ctx.teardown().await;
}

/// 錯誤是 **SCIM 的 Error 結構**，`Content-Type: application/scim+json`。
///
/// 回 problem+json 的後果不是「格式不對」而已 —— Entra 的同步報告會顯示
/// 「未知錯誤」，管理者因此看不到任何可行動的訊息。
#[tokio::test]
async fn c_errors_use_the_scim_envelope_not_problem_json() {
    let ctx = &TestContext::setup().await;
    let (_, token) = provision_scim(ctx, "entra-errors").await;

    // 401：完全沒有標頭。
    let (status, ct, body) = scim_send(
        ctx,
        Request::builder()
            .uri("/api/v1/scim/v2/Users")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        ct.starts_with("application/scim+json"),
        "401 的 Content-Type 是 `{ct}`，不是 application/scim+json"
    );
    assert_eq!(
        body["schemas"],
        json!(["urn:ietf:params:scim:api:messages:2.0:Error"]),
        "錯誤沒有 SCIM 的 schemas：{body}"
    );
    // status 必須是**字串**。送數字會讓嚴格的客戶端解析失敗。
    assert_eq!(body["status"], json!("401"), "status 應該是字串：{body}");
    assert!(
        body.get("type").is_none() && body.get("title").is_none(),
        "回應含 problem+json 的欄位 —— 兩種格式混在一起了：{body}"
    );

    // 404：不存在的資源。
    let (status, ct, body) = scim_send(
        ctx,
        scim_req(
            "GET",
            "/api/v1/scim/v2/Users/00000000-0000-4000-8000-000000000000",
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(ct.starts_with("application/scim+json"), "{ct}");
    assert_eq!(body["status"], json!("404"));

    // 400：缺 userName。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "POST",
            "/api/v1/scim/v2/Users",
            &token,
            Some(json!({ "displayName": "沒有 userName" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["scimType"], json!("invalidValue"), "{body}");

    // 成功的回應也要是 scim+json。
    let (status, ct, _) =
        scim_send(ctx, scim_req("GET", "/api/v1/scim/v2/Users", &token, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ct.starts_with("application/scim+json"),
        "成功回應的 Content-Type 是 `{ct}`"
    );

    ctx.teardown().await;
}

/// 不支援的 filter 是 **400 `invalidFilter`**，不是「忽略條件後回傳全部」。
///
/// 這一格守的是本切片最危險的失敗方式。忽略 filter 時症狀是：Entra 查一個
/// 使用者，拿到 200 與一整頁結果，於是它認為那些人全都符合 —— 而後續的
/// 決策（要不要建立、要不要停用）全部建立在錯誤的前提上。
#[tokio::test]
async fn d_an_unsupported_filter_is_an_error_not_a_full_listing() {
    let ctx = &TestContext::setup().await;
    let (_, token) = provision_scim(ctx, "entra-filter").await;

    create_scim_user(ctx, &token, "a.wang").await;
    create_scim_user(ctx, &token, "b.chen").await;

    // 先確認沒有 filter 時真的回兩筆 —— 否則下面的斷言可能因為「本來就空」
    // 而假性通過。
    let (status, _, body) =
        scim_send(ctx, scim_req("GET", "/api/v1/scim/v2/Users", &token, None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totalResults"], json!(2), "{body}");

    // 支援的 filter：精確命中一筆。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "GET",
            "/api/v1/scim/v2/Users?filter=userName%20eq%20%22a.wang%22",
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totalResults"], json!(1), "{body}");
    assert_eq!(body["Resources"][0]["userName"], json!("a.wang"), "{body}");

    // 不支援的文法：**必須是 400**，而且不能回任何 Resources。
    for raw in [
        "userName%20co%20%22wang%22",
        "userName%20eq%20%22a.wang%22%20and%20active%20eq%20%22true%22",
        "emails.value%20eq%20%22a%40b.c%22",
        "meta.lastModified%20gt%20%222026-01-01T00%3A00%3A00Z%22",
    ] {
        let (status, _, body) = scim_send(
            ctx,
            scim_req(
                "GET",
                &format!("/api/v1/scim/v2/Users?filter={raw}"),
                &token,
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "filter `{raw}` 沒有被拒絕 —— 回了 {status}，body：{body}"
        );
        assert_eq!(body["scimType"], json!("invalidFilter"), "{raw}：{body}");
        assert!(
            body["Resources"].is_null(),
            "被拒絕的 filter 竟然回了 Resources：{body}"
        );
    }

    ctx.teardown().await;
}

/// 讀取範圍限定在發出請求的那個身分來源。
///
/// 種子租戶有本地建立的使用者（`009`）。SCIM 的清單**不能**看到他們 ——
/// 否則 Entra 會認為那些帳號是自己管的，接著就能停用租戶管理員。
#[tokio::test]
async fn e_scim_cannot_see_users_it_did_not_provision() {
    let ctx = &TestContext::setup().await;
    let (_, token) = provision_scim(ctx, "entra-scope").await;

    // 種子租戶本來就有使用者 —— 先確認這件事，否則下面的 0 是假性通過。
    let mut tx = ctx.tenant_tx().await;
    let seeded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.users WHERE deleted_at IS NULL AND status <> 'DEPROVISIONED'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    drop(tx);
    assert!(seeded > 0, "種子沒有使用者，這一格驗不到東西");

    let (status, _, body) =
        scim_send(ctx, scim_req("GET", "/api/v1/scim/v2/Users", &token, None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["totalResults"],
        json!(0),
        "SCIM 看到了 {seeded} 個本地使用者裡的一部分 —— \
         外部目錄不該能看到它沒有佈建的帳號：{body}"
    );

    // 用既有本地帳號的名稱建立 → 409 `uniqueness`，而且訊息要說出原因。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "POST",
            "/api/v1/scim/v2/Users",
            &token,
            Some(json!({ "userName": USERNAME, "displayName": "想接管管理員" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["scimType"], json!("uniqueness"), "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("不由此來源管理"),
        "409 沒說出「這個帳號不屬於此來源」，管理者無從處置：{body}"
    );

    // 而那個本地帳號一根頭髮都沒被動到。
    let mut tx = ctx.tenant_tx().await;
    let status_after: String =
        sqlx::query_scalar("SELECT status FROM fms.users WHERE username = $1::citext")
            .bind(USERNAME)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(status_after, "ACTIVE", "被拒絕的請求改到了本地帳號");
    drop(tx);

    ctx.teardown().await;
}

/// `active: false` → `SUSPENDED`（可復原），不是 `DEPROVISIONED`。
///
/// 兩者在 002 是不同狀態，而 `POST /users/{id}:suspend` 已經定義了語意：
/// SUSPENDED 可復原、DEPROVISIONED 是離職。SCIM 的 `active=false` 對應前者，
/// `DELETE` 對應後者。搞混會讓「暫時停用」變成不可逆。
#[tokio::test]
async fn f_active_false_suspends_and_true_restores() {
    let ctx = &TestContext::setup().await;
    let (_, token) = provision_scim(ctx, "entra-active").await;
    let user = create_scim_user(ctx, &token, "c.lin").await;
    let id = user["id"].as_str().unwrap().to_string();
    assert_eq!(user["active"], json!(true));

    // Entra 送的是大寫 op 與字串化的布林 —— 兩者都要能吃。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "PATCH",
            &format!("/api/v1/scim/v2/Users/{id}"),
            &token,
            Some(json!({
                "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
                "Operations": [{ "op": "Replace", "path": "active", "value": "False" }]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], json!(false), "{body}");

    let mut tx = ctx.tenant_tx().await;
    let st: String = sqlx::query_scalar("SELECT status FROM fms.users WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&id).unwrap())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(
        st, "SUSPENDED",
        "active=false 應該對應 SUSPENDED（可復原），實際是 {st}"
    );

    // 停用的使用者仍然讀得到（與刪除不同）。
    let (status, _, body) = scim_send(
        ctx,
        scim_req("GET", &format!("/api/v1/scim/v2/Users/{id}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "停用的使用者讀不到了：{body}");
    assert_eq!(body["active"], json!(false));

    // 沒有 path、value 是物件的形式（Entra 也會送這種）。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "PATCH",
            &format!("/api/v1/scim/v2/Users/{id}"),
            &token,
            Some(json!({
                "Operations": [{ "op": "replace", "value": { "active": true, "title": "課長" } }]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["active"],
        json!(true),
        "沒有 path 的形式沒生效：{body}"
    );
    assert_eq!(body["title"], json!("課長"), "{body}");

    ctx.teardown().await;
}

/// 一個 provider 的 token 改不到另一個 provider 佈建的使用者。
///
/// 同一個租戶內可以有多個身分來源（002 的唯一鍵是 `(tenant_id, code)`）。
/// 讀取範圍那條規則若只實作在 GET 上，寫入路徑就是一個繞道。
#[tokio::test]
async fn g_one_providers_token_cannot_touch_another_providers_users() {
    let ctx = &TestContext::setup().await;
    let (_, token_a) = provision_scim(ctx, "entra-a").await;
    let (_, token_b) = provision_scim(ctx, "entra-b").await;

    let user = create_scim_user(ctx, &token_a, "d.huang").await;
    let id = user["id"].as_str().unwrap().to_string();

    // B 看不到。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "GET",
            &format!("/api/v1/scim/v2/Users/{id}"),
            &token_b,
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "B 的 token 讀到了 A 的使用者：{body}"
    );

    // B 改不到。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "PATCH",
            &format!("/api/v1/scim/v2/Users/{id}"),
            &token_b,
            Some(json!({
                "Operations": [{ "op": "replace", "path": "active", "value": false }]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "B 改到了 A 的使用者：{body}");

    // B 刪不到。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "DELETE",
            &format!("/api/v1/scim/v2/Users/{id}"),
            &token_b,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "B 刪掉了 A 的使用者：{body}");

    // 而 A 的使用者仍然是 ACTIVE。
    let mut tx = ctx.tenant_tx().await;
    let st: String = sqlx::query_scalar("SELECT status FROM fms.users WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&id).unwrap())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(st, "ACTIVE", "跨 provider 的請求改到了狀態");

    ctx.teardown().await;
}

/// `DELETE` 改成 `DEPROVISIONED`，且後續的 GET 是 404（RFC 7644 §3.6）。
///
/// 那一列留著是因為工單與稽核軌引用它。SCIM 那一側看到的仍然是刪除 ——
/// 「留著但讀不到」是這裡唯一同時滿足兩邊的做法。
#[tokio::test]
async fn h_delete_deprovisions_but_keeps_the_row() {
    let ctx = &TestContext::setup().await;
    let (_, token) = provision_scim(ctx, "entra-delete").await;
    let user = create_scim_user(ctx, &token, "e.tsai").await;
    let id = user["id"].as_str().unwrap().to_string();

    let (status, _, _) = scim_send(
        ctx,
        scim_req(
            "DELETE",
            &format!("/api/v1/scim/v2/Users/{id}"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // 後續的 GET 是 404 —— 供裝端看到的是「已刪除」。
    let (status, _, body) = scim_send(
        ctx,
        scim_req("GET", &format!("/api/v1/scim/v2/Users/{id}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "刪除後仍然讀得到：{body}");

    // 清單也看不到。
    let (status, _, body) =
        scim_send(ctx, scim_req("GET", "/api/v1/scim/v2/Users", &token, None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["totalResults"],
        json!(0),
        "刪除的使用者還在清單裡：{body}"
    );

    // 但那一列還在，而且 deleted_at 沒有被設 —— 工單的「誰做的」不能變 NULL。
    let mut tx = ctx.tenant_tx().await;
    let (st, deleted): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, deleted_at FROM fms.users WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&id).unwrap())
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    drop(tx);
    assert_eq!(st, "DEPROVISIONED", "狀態不是 DEPROVISIONED 而是 {st}");
    assert!(deleted.is_none(), "SCIM 的刪除不該設 deleted_at");

    // 第二次 DELETE 是 404（幂等的觀察面：資源已經不存在）。
    let (status, _, _) = scim_send(
        ctx,
        scim_req(
            "DELETE",
            &format!("/api/v1/scim/v2/Users/{id}"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// 群組成員**只能**是此身分來源佈建的使用者，而且不合格就整批拒絕。
///
/// 這一格守的是一條完整的提權路徑：058 的目錄對應會依群組授予角色，
/// 因此「能把任何人塞進一個群組」等於「能給任何人任何角色」。
///
/// 「整批拒絕」而不是「加入合格的那些」：後者會讓 Entra 顯示成功，
/// 而漏掉的那個人永遠拿不到角色 —— 一個沒有人會去查的靜默失敗。
#[tokio::test]
async fn i_group_membership_is_restricted_to_users_this_provider_provisioned() {
    let ctx = &TestContext::setup().await;
    let (_, token) = provision_scim(ctx, "entra-groups").await;
    let (_, token_other) = provision_scim(ctx, "entra-other").await;

    let mine = create_scim_user(ctx, &token, "f.kuo").await;
    let mine_id = mine["id"].as_str().unwrap().to_string();
    let theirs = create_scim_user(ctx, &token_other, "g.hsu").await;
    let theirs_id = theirs["id"].as_str().unwrap().to_string();

    // 本地建立的管理員 id —— 最有價值的攻擊目標。
    let mut tx = ctx.tenant_tx().await;
    let admin_id: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM fms.users WHERE username = $1::citext")
            .bind(USERNAME)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    drop(tx);

    // 只含自己佈建的成員 → 成立。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "POST",
            "/api/v1/scim/v2/Groups",
            &token,
            Some(json!({
                "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
                "displayName": "設施維護組",
                "externalId": "aad-group-1",
                "members": [{ "value": mine_id }]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let group_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["members"].as_array().unwrap().len(), 1, "{body}");

    // member_count 是快取欄位（002）—— 不更新它，管理界面永遠顯示 0。
    let mut tx = ctx.tenant_tx().await;
    let count: i32 =
        sqlx::query_scalar("SELECT member_count FROM fms.directory_groups WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&group_id).unwrap())
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    drop(tx);
    assert_eq!(count, 1, "member_count 沒有被同步");

    // 加入不屬於此來源的成員 → 400，而且**一個都不加**。
    for outsider in [theirs_id.as_str(), &admin_id.to_string()] {
        let (status, _, body) = scim_send(
            ctx,
            scim_req(
                "PATCH",
                &format!("/api/v1/scim/v2/Groups/{group_id}"),
                &token,
                Some(json!({
                    "Operations": [{
                        "op": "add",
                        "path": "members",
                        "value": [{ "value": mine_id }, { "value": outsider }]
                    }]
                })),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "把 {outsider} 加進群組竟然成功了：{body}"
        );

        let mut tx = ctx.tenant_tx().await;
        let members: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM fms.user_directory_groups WHERE directory_group_id = $1",
        )
        .bind(uuid::Uuid::parse_str(&group_id).unwrap())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        drop(tx);
        assert_eq!(
            members, 1,
            "被拒絕的請求仍然改了成員 —— 應該整批回滾，實際有 {members} 人"
        );
    }

    // Entra 的移除形式：`members[value eq "…"]`。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "PATCH",
            &format!("/api/v1/scim/v2/Groups/{group_id}"),
            &token,
            Some(json!({
                "Operations": [{
                    "op": "remove",
                    "path": format!("members[value eq \"{mine_id}\"]")
                }]
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["members"].as_array().unwrap().len(),
        0,
        "members[value eq …] 形式的移除沒生效：{body}"
    );

    ctx.teardown().await;
}

/// 不支援的 PATCH 路徑要**報錯**。
///
/// 靜默略過的後果：Entra 的同步報告顯示成功，而那個屬性從來沒有被寫入 ——
/// 沒有人會去查一次「顯示成功」的同步。
#[tokio::test]
async fn j_an_unsupported_patch_path_is_reported_not_ignored() {
    let ctx = &TestContext::setup().await;
    let (_, token) = provision_scim(ctx, "entra-paths").await;
    let user = create_scim_user(ctx, &token, "h.chou").await;
    let id = user["id"].as_str().unwrap().to_string();

    for (path, value) in [
        ("userName", json!("改帳號")),
        (
            "emails[type eq \"work\"].value",
            json!("new@corp.example.com"),
        ),
        ("preferredLanguage", json!("zh-TW")),
        (
            "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:department",
            json!("設施部"),
        ),
    ] {
        let (status, _, body) = scim_send(
            ctx,
            scim_req(
                "PATCH",
                &format!("/api/v1/scim/v2/Users/{id}"),
                &token,
                Some(json!({
                    "Operations": [{ "op": "replace", "path": path, "value": value }]
                })),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "path `{path}` 沒有被拒絕（回 {status}）—— 靜默略過會讓同步假性成功：{body}"
        );
        assert_eq!(body["scimType"], json!("invalidPath"), "{path}：{body}");
        // 訊息要列出可以改什麼，否則對方只知道「不行」。
        assert!(
            body["detail"].as_str().unwrap().contains("active"),
            "錯誤沒有列出可用的路徑：{body}"
        );
    }

    // 空的 Operations 也要報錯，而不是回一個什麼都沒做的 200。
    let (status, _, body) = scim_send(
        ctx,
        scim_req(
            "PATCH",
            &format!("/api/v1/scim/v2/Users/{id}"),
            &token,
            Some(json!({ "Operations": [] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["scimType"], json!("invalidSyntax"), "{body}");

    // 而使用者一個欄位都沒被動到。
    let (status, _, after) = scim_send(
        ctx,
        scim_req("GET", &format!("/api/v1/scim/v2/Users/{id}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        after["userName"], user["userName"],
        "被拒絕的 PATCH 改到了 userName"
    );
    assert_eq!(
        after["emails"], user["emails"],
        "被拒絕的 PATCH 改到了 emails"
    );

    ctx.teardown().await;
}

/// 稽核軌把 SCIM 的寫入記成 `DIRECTORY_SYNC`，而秘密欄位的**值**被遮蔽。
///
/// 前半：SCIM 沒有人類發動者，而 029 的觸發器預設是 `USER`。記成 USER 會讓
/// 稽核軌看起來像「某個使用者建立了這個帳號」，而追查時找不到那個人。
///
/// 後半：074 的遮蔽清單。這一格要真的寫一次 `password_hash` 才驗得到 ——
/// 見下方註解。
#[tokio::test]
async fn k_scim_writes_are_attributed_to_directory_sync_in_the_audit_trail() {
    let ctx = &TestContext::setup().await;
    let (_, token) = provision_scim(ctx, "entra-audit").await;
    let user = create_scim_user(ctx, &token, "i.yeh").await;
    let id = uuid::Uuid::parse_str(user["id"].as_str().unwrap()).unwrap();

    let mut tx = ctx.owner_tx().await;
    sqlx::query("SELECT set_config('app.is_platform','on',true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    let actor_type: String = sqlx::query_scalar(
        "SELECT actor_type FROM fms.audit_log
          WHERE entity_type = 'USERS' AND entity_id = $1 AND action = 'CREATE'
          ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("SCIM 建立使用者沒有留下稽核列");

    assert_eq!(
        actor_type, "DIRECTORY_SYNC",
        "SCIM 的寫入被記成 {actor_type} —— 那會讓追查時去找一個不存在的人"
    );

    // -------------------------------------------------------------------------
    // 074 的稽核遮蔽，在**真實的觸發器路徑**上驗一次
    // -------------------------------------------------------------------------
    // 不能拿上面那一列來驗：SCIM 建立的使用者沒有密碼，`password_hash` 本來
    // 就是 NULL，因此「斷言它是 NULL」在遮蔽被拿掉之後**仍然會通過** ——
    // 一格空的斷言。（第一版就是那樣寫的，突變測試 M6 暴露出來：
    // 抓到它的只有 migration 的自我驗證，行為面沒有任何一格守著。）
    //
    // 因此這裡真的寫一個密碼雜湊進去，讓觸發器跑一次 UPDATE。
    sqlx::query(
        "SELECT fms.set_context($1, $2, false),
                fms.set_request_context(NULL, 'USER')",
    )
    .bind(uuid::Uuid::parse_str(TENANT_ID).unwrap())
    .bind(admin_user_id())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE fms.users
            SET password_hash = '$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA',
                password_updated_at = clock_timestamp()
          WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await
    .unwrap();

    sqlx::query("SELECT set_config('app.is_platform','on',true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    let (has_value, diff_keys): (bool, Vec<String>) = sqlx::query_as(
        "SELECT (after_data ->> 'password_hash') IS NOT NULL, diff_keys
           FROM fms.audit_log
          WHERE entity_type = 'USERS' AND entity_id = $1 AND action = 'UPDATE'
          ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("改密碼沒有留下稽核列");
    drop(tx);

    assert!(
        !has_value,
        "audit_log 存了 password_hash 的值 —— 074 的遮蔽沒有在觸發器路徑上生效，\
         而 audit_log 是 append-only 且長期保留（等於一份可離線破解的雜湊清單）"
    );
    // **但鍵仍要在 diff_keys 裡。** 遮蔽成 NULL 而不是刪掉鍵，就是為了保留
    // 「這次改動包含改密碼」這個事實 —— 刪掉鍵會讓稽核軌對改密碼完全沉默。
    assert!(
        diff_keys.iter().any(|k| k == "password_hash"),
        "diff_keys 少了 password_hash —— 遮蔽把「有人改了密碼」這個事實也一起\
         抹掉了：{diff_keys:?}"
    );

    ctx.teardown().await;
}
