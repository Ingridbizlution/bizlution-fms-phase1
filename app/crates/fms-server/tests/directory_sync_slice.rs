//! 身分來源與目錄同步（`/identity-providers`）。
//!
//! # 兩個核心
//!
//! **`b_`：同步會收回。** `directory_sync_runs` 有 `roles_revoked` 欄位，
//! schema 本身就在說這件事；而 `DELETE /role-assignments/{id}` 的錯誤訊息寫著
//! 「要移除請改群組對應」—— **同步若只加不減，那句話就是假的**，
//! 而使用者會發現改了對應卻沒有效果。
//!
//! **`c_`：同步是繞過 052 的第三條路徑。** 提權防護擋住了 API 直接指派
//! 與目錄對應的建立，但對應**可以被種子或手寫 SQL 建立**。若同步不檢查，
//! 一條既有的對應就能把 `PLATFORM_ADMIN` 發給整個群組。
//!
//! # 這裡的「同步」不連 AD
//!
//! 對帳從 `user_directory_groups`（成員關係）與 `directory_role_mappings`
//! （規則）算出 `user_role_assignments`。**成員關係由測試自己佈置** ——
//! 真實環境由 SCIM 或未來的 connector 放進來。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const USER_REQUESTER: &str = "ffffffff-0000-4000-8000-000000000004";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 佈置：一個啟用同步的 provider、一個群組、一位成員、一條對應。
///
/// 回傳 `(provider_id, group_id)`。
async fn setup_directory(ctx: &TestContext, role_code: &str) -> (String, String) {
    let mut tx = ctx.owner_tx().await;
    let provider: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.identity_providers
           (tenant_id, code, name, provider_type, ldap_host, ldap_base_dn, sync_enabled)
         VALUES ($1::uuid, 'TEST_AD', '測試 AD', 'LDAP',
                 'ad.example.com', 'dc=example,dc=com', true)
         RETURNING id",
    )
    .bind(TENANT_ID)
    .fetch_one(&mut *tx)
    .await
    .expect("建 provider");

    let group: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.directory_groups
           (tenant_id, identity_provider_id, external_group_id, name)
         VALUES ($1::uuid, $2, 'ext-1', '測試群組')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(provider)
    .fetch_one(&mut *tx)
    .await
    .expect("建群組");

    // 成員關係：真實環境由 SCIM／connector 放進來。
    sqlx::query(
        "INSERT INTO fms.user_directory_groups (user_id, directory_group_id, tenant_id)
         VALUES ($1::uuid, $2, $3::uuid)",
    )
    .bind(USER_REQUESTER)
    .bind(group)
    .bind(TENANT_ID)
    .execute(&mut *tx)
    .await
    .expect("加入群組");

    // 對應：**直接用 SQL 建**，刻意繞過 `POST /directory-role-mappings` 的
    // 052 檢查 —— 那正是 `c_` 要驗的情境（種子與手寫 SQL 都會這樣）。
    sqlx::query(
        "INSERT INTO fms.directory_role_mappings
           (tenant_id, directory_group_id, role_id, scope_type, scope_id)
         SELECT $1::uuid, $2, r.id, 'FACILITY', $3::uuid
           FROM fms.roles r WHERE r.code = $4 AND r.tenant_id IS NULL",
    )
    .bind(TENANT_ID)
    .bind(group)
    .bind(FACILITY_HQ)
    .bind(role_code)
    .execute(&mut *tx)
    .await
    .expect("建對應");
    tx.commit().await.expect("commit");

    (provider.to_string(), group.to_string())
}

async fn assignments_from_sync(ctx: &TestContext) -> i64 {
    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.user_role_assignments
          WHERE source = 'DIRECTORY_SYNC' AND user_id = $1::uuid",
    )
    .bind(USER_REQUESTER)
    .fetch_one(&mut *tx)
    .await
    .expect("查授權");
    tx.commit().await.expect("commit");
    n
}

/// 建立、列出、以及 `client_secret_ref` 不接受明文密鑰。
#[tokio::test]
async fn a_providers_can_be_created_and_listed() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, created) = ctx
        .send(authed(
            post(
                "/api/v1/identity-providers",
                json!({
                    "code": "ENTRA_MAIN", "name": "公司 Entra ID",
                    "provider_type": "OIDC",
                    "issuer": "https://login.microsoftonline.com/abc/v2.0",
                    "client_id": "11111111-2222-3333-4444-555555555555",
                    "client_secret_ref": "kv/fms/entra-client-secret",
                    "sync_enabled": true
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["provider_type"], "OIDC");
    assert_eq!(created["sync_enabled"], true);

    let (status, listed) = ctx
        .send(authed(get("/api/v1/identity-providers"), &admin))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "ENTRA_MAIN"),
        "{listed}"
    );

    // **明文密鑰要被擋。** 只在文件裡寫「請填參照」的話，第一個整合的人
    // 會直接貼密鑰進去，而它會進資料庫、備份與稽核的 after_data。
    let (status, secret) = ctx
        .send(authed(
            post(
                "/api/v1/identity-providers",
                json!({
                    "code": "BAD_SECRET", "name": "貼了密鑰",
                    "provider_type": "OIDC", "issuer": "https://x/v2.0",
                    "client_id": "cid",
                    "client_secret_ref": "Xq7~pL9mZ2vB4nR8sT1wY6uI3oA5eG0hJcKdFbNvMxQzWrEyUt"
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{secret}");
    assert!(
        secret["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("參照"),
        "訊息要說出該填什麼：{secret}"
    );

    // LDAP 沒有 host 是「建了但連不上」—— 建立時就該失敗。
    let (status, noldap) = ctx
        .send(authed(
            post(
                "/api/v1/identity-providers",
                // 只給 host、缺 base_dn —— 002 的 ck_idp_ldap_fields 要兩者都有。
                json!({ "code": "NO_BASE_DN", "name": "缺 base_dn",
                        "provider_type": "LDAP", "ldap_host": "ad.example.com" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{noldap}");
    assert!(
        noldap["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("base_dn"),
        "訊息要說出缺的是 base_dn，不是只說 LDAP 設定錯：{noldap}"
    );

    ctx.teardown().await;
}

/// **同步會收回。** 這一組最重要的一格。
///
/// 三個階段：發放 → 把人移出群組 → 再同步，授權必須消失。
/// 而且**人工指派不能被吃掉** —— 那是 `source = MANUAL`，同步不該碰。
#[tokio::test]
async fn b_sync_revokes_not_only_grants() {
    let ctx = &TestContext::setup().await;
    // VIEWER 沒有任何危險權限，所以不會被提權防護擋（那是 c_ 的事）。
    let (provider, group) = setup_directory(ctx, "VIEWER").await;
    let admin = ctx.login_as(USERNAME).await;

    // 另外給同一個人一筆**人工**指派，驗證同步不會吃掉它。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.user_role_assignments
               (tenant_id, user_id, role_id, scope_type, scope_id, source)
             SELECT $1::uuid, $2::uuid, r.id, 'FACILITY', $3::uuid, 'MANUAL'
               FROM fms.roles r WHERE r.code = 'DISPATCHER' AND r.tenant_id IS NULL",
        )
        .bind(TENANT_ID)
        .bind(USER_REQUESTER)
        .bind(FACILITY_HQ)
        .execute(&mut *tx)
        .await
        .expect("人工指派");
        tx.commit().await.expect("commit");
    }

    // --- 第一次同步：發放 ---
    let (status, first) = ctx
        .send(authed(
            post(
                &format!("/api/v1/identity-providers/{provider}/sync"),
                json!({}),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    assert_eq!(first["status"], "SUCCEEDED", "{first}");
    assert_eq!(first["roles_granted"], 1, "對應應該產生一筆授權：{first}");
    assert_eq!(first["roles_revoked"], 0);
    assert_eq!(assignments_from_sync(ctx).await, 1);

    // --- 重跑：不該重複發放（冪等）---
    let (_, again) = ctx
        .send(authed(
            post(
                &format!("/api/v1/identity-providers/{provider}/sync"),
                json!({}),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        again["roles_granted"], 0,
        "已經有的授權不該再算一次 —— roles_granted 是「真的新增」的數量：{again}"
    );
    assert_eq!(assignments_from_sync(ctx).await, 1, "也不該產生第二筆");

    // --- 把人移出群組，再同步：必須收回 ---
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("DELETE FROM fms.user_directory_groups WHERE directory_group_id = $1::uuid")
            .bind(&group)
            .execute(&mut *tx)
            .await
            .expect("移出群組");
        tx.commit().await.expect("commit");
    }

    let (_, third) = ctx
        .send(authed(
            post(
                &format!("/api/v1/identity-providers/{provider}/sync"),
                json!({}),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        third["roles_revoked"], 1,
        "**同步只加不減的話，DELETE /role-assignments 的錯誤訊息就是假的**：{third}"
    );
    assert_eq!(assignments_from_sync(ctx).await, 0, "授權必須真的消失");

    // --- 人工指派仍然在 ---
    // 精確數**我插的那一筆**（DISPATCHER），不是所有 MANUAL。
    // 第一版數了全部，而 009 已經給 user.huang 一筆 REQUESTER@總部 的
    // MANUAL 指派 —— 拿到 2 而不是 1。錯的是斷言，不是程式。
    let mut tx = ctx.owner_tx().await;
    let manual: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.user_role_assignments ura
           JOIN fms.roles r ON r.id = ura.role_id
          WHERE ura.source = 'MANUAL' AND ura.user_id = $1::uuid
            AND r.code = 'DISPATCHER'",
    )
    .bind(USER_REQUESTER)
    .fetch_one(&mut *tx)
    .await
    .expect("查人工指派");
    tx.commit().await.expect("commit");
    assert_eq!(
        manual, 1,
        "同步不該吃掉管理員手動給的東西 —— 收回只能動 source = DIRECTORY_SYNC"
    );

    ctx.teardown().await;
}

/// **同步是繞過 052 的第三條路徑。**
///
/// 對應用 SQL 直接建（種子與手寫 SQL 都是這樣），因此不能假設
/// 「建立時已經被 `POST /directory-role-mappings` 檢查過了」。
///
/// `PLATFORM_ADMIN` 帶 `user:impersonate`，而**連 TENANT_ADMIN 都沒有它** ——
/// 所以租戶裡權力最大的人觸發同步也不該把它發出去。
#[tokio::test]
async fn c_sync_cannot_bypass_the_escalation_guard() {
    let ctx = &TestContext::setup().await;
    let (provider, _) = setup_directory(ctx, "PLATFORM_ADMIN").await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ctx
        .send(authed(
            post(
                &format!("/api/v1/identity-providers/{provider}/sync"),
                json!({}),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    assert_eq!(
        body["roles_granted"], 0,
        "**同步繞過了提權防護** —— 一條既有的對應就把 PLATFORM_ADMIN 發出去了：{body}"
    );
    assert_eq!(
        body["status"], "PARTIAL",
        "被擋下的對應要讓這一輪是 PARTIAL —— 回 SUCCEEDED 會讓\
         「這條對應設定了但不生效」完全看不見：{body}"
    );
    assert!(
        body["blocked_roles"]
            .as_array()
            .map(|a| a.iter().any(|r| r == "PLATFORM_ADMIN"))
            .unwrap_or(false),
        "被擋的角色要具名：{body}"
    );
    assert_eq!(assignments_from_sync(ctx).await, 0, "一筆授權都不該產生");

    // 歷程查得到，而且理由寫在 error_summary 裡。
    let (status, runs) = ctx
        .send(authed(
            get(&format!("/api/v1/identity-providers/{provider}/sync-runs")),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{runs}");
    let run = runs["items"][0].clone();
    assert_eq!(run["status"], "PARTIAL");
    assert!(
        run["error_summary"]
            .as_str()
            .unwrap_or_default()
            .contains("PLATFORM_ADMIN"),
        "歷程要說出是哪個角色被擋 —— 那是設定問題，不是系統錯誤：{run}"
    );

    ctx.teardown().await;
}

/// `sync_enabled = false` 要擋下來，而不是照樣跑一輪。
///
/// 那個開關是管理員刻意關掉的。照樣跑會讓它看起來沒有作用。
#[tokio::test]
async fn d_a_disabled_provider_refuses_to_sync() {
    let ctx = &TestContext::setup().await;
    let (provider, _) = setup_directory(ctx, "VIEWER").await;
    let admin = ctx.login_as(USERNAME).await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.identity_providers SET sync_enabled = false WHERE id = $1::uuid")
            .bind(&provider)
            .execute(&mut *tx)
            .await
            .expect("關閉同步");
        tx.commit().await.expect("commit");
    }

    let (status, body) = ctx
        .send(authed(
            post(
                &format!("/api/v1/identity-providers/{provider}/sync"),
                json!({}),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("sync_enabled"),
        "訊息要說出是哪個開關：{body}"
    );
    assert_eq!(assignments_from_sync(ctx).await, 0, "不該產生任何授權");

    ctx.teardown().await;
}

/// 權限：三支各要不同的權限，而且都是 TENANT 範圍。
#[tokio::test]
async fn e_permissions_are_tenant_scoped() {
    let ctx = &TestContext::setup().await;
    let (provider, _) = setup_directory(ctx, "VIEWER").await;

    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    for (label, req) in [
        ("list", get("/api/v1/identity-providers")),
        (
            "sync",
            post(
                &format!("/api/v1/identity-providers/{provider}/sync"),
                json!({}),
            ),
        ),
        (
            "runs",
            get(&format!("/api/v1/identity-providers/{provider}/sync-runs")),
        ),
    ] {
        let (status, body) = ctx.send(authed(req, &fm)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "FACILITY_ADMIN 不該通過 {label}：{body}"
        );
    }

    // 找不到的 provider 是 404，不是 500 也不是空成功。
    let admin = ctx.login_as(USERNAME).await;
    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/identity-providers/00000000-0000-4000-8000-0000000000ff/sync",
                json!({}),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    ctx.teardown().await;
}
