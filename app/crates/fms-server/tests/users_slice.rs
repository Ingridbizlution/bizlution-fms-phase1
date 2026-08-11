//! 使用者維護（`/users`）。
//!
//! 這一組要守的是**兩個範圍的不對稱**與**三個「不能做」**：
//!
//!   `user:read` FACILITY  → 場域管理員看得到全租戶的人（派工要選人）
//!   `user:write` TENANT   → 但建不了、停不了帳號
//!
//!   不能建出帶密碼的帳號、不能改 username、不能停用自己。
//!
//! 最後一格驗的是模組檔頭主張的理由之一：029 稽核了 `users`，
//! 但在這支端點之前**沒有任何已實作的端點會寫它** —— 那份軌跡至今
//! 沒有一列來自真實操作。

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

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn create_user(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(json_request("POST", "/api/v1/users", body), token))
        .await
}

/// **範圍不對稱：看得到，但建不了。**
///
/// 這不是疏漏而是設計 —— 026 的規則是「讀一個租戶級資源不是租戶級特權，
/// 寫它才是」。場域管理員派工要選人，所以要看得到；但新增帳號影響整個租戶。
///
/// 兩格一起驗：只驗「建不了」的話，把 `user:read` 也提到 TENANT 也會通過，
/// 而那會讓派工選不到人。
#[tokio::test]
async fn a_facility_admin_can_list_users_but_cannot_create_them() {
    let ctx = &TestContext::setup().await;
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, body) = ctx.send(authed(get("/api/v1/users"), &fm)).await;
    assert_eq!(status, StatusCode::OK, "場域管理員要看得到人：{body}");
    let n = body["data"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(
        n >= 3,
        "看到的應該是**全租戶**的使用者（users 沒有 facility_id，RLS 只隔離租戶）：{n}"
    );

    let (status, denied) = create_user(
        ctx,
        &fm,
        json!({ "username": "new.person", "display_name": "新人" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "建立帳號是租戶級動作，場域管理員不該能做：{denied}"
    );

    ctx.teardown().await;
}

/// 建立的帳號是 `INVITED` 且**沒有密碼** —— 因此還登不進來。
///
/// 「沒有密碼」這件事不能只看回應（回應本來就不含密碼欄位）。
/// 這裡直接試登入：拿不到 token 才證明那個帳號真的還不能用。
#[tokio::test]
async fn a_new_user_is_invited_and_cannot_log_in_yet() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, user) = create_user(
        ctx,
        &token,
        json!({
            "username": "invited.chen",
            "display_name": "陳受邀",
            "email": "invited.chen@example.test",
            "job_title": "設備技師"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{user}");
    assert_eq!(user["status"], "INVITED", "新帳號應為 INVITED：{user}");
    assert_eq!(user["user_type"], "EMPLOYEE", "未指定時的預設");

    // 直接試登入 —— 沒有密碼就登不進來。
    let (status, _) = ctx
        .send(json_request(
            "POST",
            "/api/v1/auth/token",
            json!({
                "grant_type": "password",
                "tenant_code": TENANT_CODE,
                "username": "invited.chen",
                "password": TEST_PASSWORD
            }),
        ))
        .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "沒有設定密碼的帳號不該登得進來 —— 否則 INVITED 只是一個標籤"
    );

    ctx.teardown().await;
}

/// 重複的 username 要說**是哪個欄位**撞到。
///
/// 只回 409 會讓前端自己猜是 username、email 還是員工編號 ——
/// 而那三個都有唯一約束。
#[tokio::test]
async fn a_duplicate_says_which_field_collided() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, _) = create_user(
        ctx,
        &token,
        json!({ "username": "dup.test", "display_name": "第一個", "email": "dup@example.test" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // 同 username
    let (status, e1) = create_user(
        ctx,
        &token,
        json!({ "username": "dup.test", "display_name": "第二個" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        e1["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("username"),
        "要指出是 username：{e1}"
    );

    // 同 email、不同 username
    let (status, e2) = create_user(
        ctx,
        &token,
        json!({ "username": "other.name", "display_name": "第三個", "email": "dup@example.test" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        e2["detail"].as_str().unwrap_or_default().contains("email"),
        "要指出是 email，而不是又說 username：{e2}"
    );

    ctx.teardown().await;
}

/// PATCH 要分得出「沒有提供」與「明確設為 null」。
///
/// 少了這個區分，「清掉某人的 email」就變成做不到的事 —— 而那正是
/// 員工離職、外包換人時要做的。
#[tokio::test]
async fn patch_distinguishes_absent_from_explicit_null() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (_, user) = create_user(
        ctx,
        &token,
        json!({
            "username": "patch.target", "display_name": "原名",
            "email": "before@example.test", "phone": "0900-000-000"
        }),
    )
    .await;
    let id = user["id"].as_str().expect("id").to_string();

    // 只給 display_name → email 與 phone 都不該動
    let (status, after) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/users/{id}"),
                json!({ "display_name": "改名" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(after["display_name"], "改名");
    assert_eq!(
        after["email"], "before@example.test",
        "沒有提供的欄位不該被清掉：{after}"
    );
    assert_eq!(after["phone"], "0900-000-000");

    // 明確給 null → 清空，而且只清那一個
    let (status, cleared) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/users/{id}"),
                json!({ "email": null }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(
        cleared["email"].is_null(),
        "明確的 null 應該清空：{cleared}"
    );
    assert_eq!(
        cleared["phone"], "0900-000-000",
        "只清指定的那個欄位：{cleared}"
    );
    assert_eq!(cleared["display_name"], "改名", "也不該回退前一次的修改");

    ctx.teardown().await;
}

/// **不能停用自己。**
///
/// 那是一個把自己鎖在門外的操作，而且若操作者是租戶最後一個管理員，
/// 就沒有人能把他放回來 —— 那需要平台介入。
#[tokio::test]
async fn you_cannot_suspend_yourself() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let me = admin_user_id();

    let (status, refused) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/users/{me}/suspend"),
                json!({ "reason": "測試" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");

    // 反面：停用別人可以，而且是停用不是刪除。
    let (_, other) = create_user(
        ctx,
        &token,
        json!({ "username": "leaver", "display_name": "離職者" }),
    )
    .await;
    let id = other["id"].as_str().expect("id");
    let (status, done) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/users/{id}/suspend"),
                json!({ "status": "DEPROVISIONED", "reason": "離職" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "DEPROVISIONED");

    ctx.teardown().await;
}

/// 預設不回 `DEPROVISIONED`，但明確要求時回得到。
///
/// 這支端點最主要的用途是派工時選人，而把離職者混進候選清單，
/// 最壞的情況是把工單指派給一個永遠不會看到它的帳號。
#[tokio::test]
async fn deprovisioned_users_are_hidden_unless_asked_for() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (_, u) = create_user(
        ctx,
        &token,
        json!({ "username": "gone.wu", "display_name": "吳離職" }),
    )
    .await;
    let id = u["id"].as_str().expect("id");
    ctx.send(authed(
        json_request(
            "POST",
            &format!("/api/v1/users/{id}/suspend"),
            json!({ "status": "DEPROVISIONED" }),
        ),
        &token,
    ))
    .await;

    let names = |b: &Value| -> Vec<String> {
        b["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|u| u["username"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    let (_, default_list) = ctx.send(authed(get("/api/v1/users"), &token)).await;
    assert!(
        !names(&default_list).contains(&"gone.wu".to_string()),
        "預設不該出現離職者：{:?}",
        names(&default_list)
    );
    assert_eq!(
        default_list["meta"]["default_excludes"][0], "DEPROVISIONED",
        "而且要明說排除了什麼 —— 否則「找不到某人」會變成一次除錯"
    );

    let (_, asked) = ctx
        .send(authed(get("/api/v1/users?status=DEPROVISIONED"), &token))
        .await;
    assert!(
        names(&asked).contains(&"gone.wu".to_string()),
        "明確要求時要看得到：{:?}",
        names(&asked)
    );

    ctx.teardown().await;
}

/// **身分變更現在留得下稽核。**
///
/// 029 稽核了 `users`，但在這支端點之前沒有任何已實作的端點會寫它 ——
/// 那份軌跡至今沒有一列來自真實操作。這一格把那個空洞補上並釘住。
#[tokio::test]
async fn creating_and_suspending_a_user_is_audited() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (_, u) = create_user(
        ctx,
        &token,
        json!({ "username": "audited.one", "display_name": "被稽核者" }),
    )
    .await;
    let id = u["id"].as_str().expect("id").to_string();

    ctx.send(authed(
        json_request(
            "POST",
            &format!("/api/v1/users/{id}/suspend"),
            json!({ "reason": "測試停用" }),
        ),
        &token,
    ))
    .await;

    let mut tx = ctx.owner_tx().await;
    let rows: Vec<(String, Option<Vec<String>>)> = sqlx::query_as(
        "SELECT action, diff_keys FROM fms.audit_log
          WHERE entity_type = 'USERS' AND entity_id = $1::uuid
          ORDER BY occurred_at",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await
    .expect("讀稽核");
    // 操作者是不是真的那個人 —— 稽核的意義就是「誰做的」。
    let actor_ok: bool = sqlx::query_scalar(
        "SELECT bool_and(actor_user_id = $2) FROM fms.audit_log
          WHERE entity_type = 'USERS' AND entity_id = $1::uuid",
    )
    .bind(&id)
    .bind(admin_user_id())
    .fetch_one(&mut *tx)
    .await
    .expect("讀 actor");
    tx.commit().await.expect("commit");

    let actions: Vec<&str> = rows.iter().map(|(a, _)| a.as_str()).collect();
    assert!(actions.contains(&"CREATE"), "建立帳號要留痕：{actions:?}");
    assert!(actions.contains(&"UPDATE"), "停用要留痕：{actions:?}");
    assert!(actor_ok, "稽核列的 actor 必須是真正發出請求的人");

    let suspend_diff = rows
        .iter()
        .rfind(|(a, _)| a == "UPDATE")
        .and_then(|(_, d)| d.clone())
        .unwrap_or_default();
    assert!(
        suspend_diff.iter().any(|k| k == "status"),
        "diff_keys 要指出動的是 status：{suspend_diff:?}"
    );

    ctx.teardown().await;
}
