//! 角色指派（`/users/{id}/role-assignments`、`/role-assignments/{id}`）。
//!
//! 這一組守的是**兩道獨立的閘**。兩道擋不同的事，缺一不可：
//!
//!   閘 1 範圍：`role:assign` 必須在這次指派的範圍上持有
//!             → 擋「org A 的管理員指派進 org B」與「指派到全租戶」
//!   閘 2 提權：不能授出自己沒有的危險權限（052）
//!             → 擋「ORG_MANAGER 把 TENANT_ADMIN 指派給自己」
//!
//! 每一道都有一格**反面**測試：只驗「擋下來了」的話，一個「一律拒絕」的
//! 實作會全部通過，而那等於這支端點不存在。
//!
//! # 為什麼要自己造一個 ORG_MANAGER
//!
//! 009 的示範租戶沒有任何 ORG_MANAGER 指派（現存只有 TENANT_ADMIN／
//! FACILITY_ADMIN／TECHNICIAN／SERVICE_STAFF／REQUESTER／PM_GENERATOR），
//! 而提權要測的正是「權限比租戶管理員少的人」。
//!
//! 用 `fm.lin`（FACILITY_ADMIN，範圍只在台北總部）加掛一個 ORG_MANAGER
//! 是刻意的選擇：他因此持有 `role:assign` **但沒有** `role:read` ——
//! 那正是契約原本不一致的地方（見 `d_...` 那一格）。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 不動產事業部 —— `fm.lin` 的場域（台北總部大樓）就掛在這個組織下。
const ORG_PROP: &str = "bbbbbbbb-0000-4000-8000-000000000002";
/// 影城事業部 —— **不在** `fm.lin` 的範圍內。
const ORG_CINEMA: &str = "bbbbbbbb-0000-4000-8000-000000000003";
const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
/// `fm.lin` —— FACILITY_ADMIN，範圍只在台北總部。
const USER_FACILITY_ADMIN: &str = "ffffffff-0000-4000-8000-000000000002";
const USER_TECH: &str = "ffffffff-0000-4000-8000-000000000003";
const USER_REQUESTER: &str = "ffffffff-0000-4000-8000-000000000004";

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

fn del(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// 把 ORG_MANAGER 掛到 `fm.lin` 的不動產事業部上。
///
/// 走 `owner_tx`（平台情境）而不是 API：031 記過一條規則 —— 改動
/// `user_role_assignments` 的連線必須先宣告平台情境，否則 029 的稽核
/// 觸發器寫不進 `audit_log`，連帶讓業務寫入一起失敗。
async fn grant_org_manager(ctx: &TestContext, user_id: &str, org_id: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.user_role_assignments
           (tenant_id, user_id, role_id, scope_type, scope_id, source)
         SELECT $1::uuid, $2::uuid, r.id, 'ORG', $3::uuid, 'MANUAL'
           FROM fms.roles r WHERE r.code = 'ORG_MANAGER' AND r.tenant_id IS NULL",
    )
    .bind(TENANT_ID)
    .bind(user_id)
    .bind(org_id)
    .execute(&mut *tx)
    .await
    .expect("掛上 ORG_MANAGER");
    tx.commit().await.expect("commit");
}

async fn assign(ctx: &TestContext, token: &str, user_id: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request(
            "POST",
            &format!("/api/v1/users/{user_id}/role-assignments"),
            body,
        ),
        token,
    ))
    .await
}

/// 閘 2 —— **ORG_MANAGER 不能把 TENANT_ADMIN 指派出去。**
///
/// 這是這一整組的核心。實測過若沒有這道閘，那次指派會多給 14 項權限
/// （含 `asset:delete` 與 `reservation:override`）—— 026 收斂掉了大部分，
/// 但沒有把這條路關上。
///
/// 也順帶驗**錯誤訊息說得出缺哪幾項**。一個只回「不行」的 403 會變成一張工單。
#[tokio::test]
async fn a_org_manager_cannot_grant_tenant_admin() {
    let ctx = &TestContext::setup().await;
    grant_org_manager(ctx, USER_FACILITY_ADMIN, ORG_PROP).await;
    let om = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, body) = assign(
        ctx,
        &om,
        USER_TECH,
        json!({ "role_code": "TENANT_ADMIN", "scope_type": "ORG", "scope_id": ORG_PROP }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ORG_MANAGER 指派 TENANT_ADMIN 是提權，必須擋下：{body}"
    );
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("asset:delete"),
        "訊息要說得出缺哪幾項危險權限，否則對方只能開工單來問：{detail}"
    );

    ctx.teardown().await;
}

/// 閘 2 的**反面** —— 同一個人指派 TECHNICIAN 必須成功。
///
/// 少了這一格，把判定寫成「一律拒絕」也會讓上一格通過。
///
/// 這一格也是為什麼提權判定用 `is_dangerous` 而不是「權限子集」：
/// ORG_MANAGER 沒有 `work_order:execute`／`part:read`／`work_order:read_own`，
/// 子集規則下他連技師都指派不了（實測 11 個角色只剩 2 個可指派）。
/// 一個不會修設備的主管當然可以聘技師。
#[tokio::test]
async fn b_org_manager_can_still_grant_an_operational_role() {
    let ctx = &TestContext::setup().await;
    grant_org_manager(ctx, USER_FACILITY_ADMIN, ORG_PROP).await;
    let om = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, body) = assign(
        ctx,
        &om,
        USER_REQUESTER,
        json!({ "role_code": "TECHNICIAN", "scope_type": "FACILITY", "scope_id": FACILITY_HQ }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "TECHNICIAN 不含任何危險權限，ORG_MANAGER 必須指派得了：{body}"
    );
    assert_eq!(body["role_code"], "TECHNICIAN");
    assert_eq!(body["source"], "MANUAL", "由 API 建立的指派來源是 MANUAL");
    assert_eq!(
        body["scope_label"], "台北總部大樓",
        "清單要看得懂 —— 只回 scope_id 的話 UI 得為每一列再查一次那個 uuid"
    );

    ctx.teardown().await;
}

/// 閘 1 —— **範圍**。同一個角色、同一個授權者，只因為範圍不同就必須被擋。
///
/// 三個方向一起驗，因為它們是三種不同的越界：
///   * 別人的組織（影城事業部不在不動產事業部的子樹裡）
///   * 全租戶（`role:assign` 只在 ORG 範圍持有）
///   * SPATIAL_NODE（016 的述詞不認這個 scope_type，建了也不會生效）
///
/// 特別注意第二項：若實作用 `require_permission(.., None, None)`，那的語意是
/// 「在**任一**範圍持有」，這一格就會漏過去 —— 一個 ORG 範圍的授權會足以
/// 指派到全租戶。
#[tokio::test]
async fn c_scope_gate_blocks_out_of_scope_targets() {
    let ctx = &TestContext::setup().await;
    grant_org_manager(ctx, USER_FACILITY_ADMIN, ORG_PROP).await;
    let om = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, body) = assign(
        ctx,
        &om,
        USER_REQUESTER,
        json!({ "role_code": "TECHNICIAN", "scope_type": "ORG", "scope_id": ORG_CINEMA }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "影城事業部不在這個 ORG_MANAGER 的子樹裡：{body}"
    );

    let (status, body) = assign(
        ctx,
        &om,
        USER_REQUESTER,
        json!({ "role_code": "TECHNICIAN", "scope_type": "TENANT" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ORG 範圍的 role:assign 不該足以指派到全租戶：{body}"
    );

    let (status, body) = assign(
        ctx,
        &om,
        USER_REQUESTER,
        json!({ "role_code": "TECHNICIAN", "scope_type": "SPATIAL_NODE", "scope_id": FACILITY_HQ }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "SPATIAL_NODE 的指派一項權限都不會生效，建得起來比擋下來更難查：{body}"
    );

    ctx.teardown().await;
}

/// 契約改了一個字，這一格是那個改動的理由。
///
/// 原契約說 `GET` 要 `role:read`。但 `role:read` 宣告 TENANT、
/// `role:assign` 宣告 ORG，而 ORG_MANAGER 只有後者 —— 照原契約做出來就是
/// **指派得了角色卻看不到自己指派了什麼**，連撤銷都做不到（要 id，而 id
/// 只能從這支清單拿）。
///
/// 兩格一起驗：
///   * 加掛 ORG_MANAGER **之前**，`fm.lin` 兩個權限都沒有 → 403
///   * 加掛**之後**，只憑 `role:assign` → 200
///
/// 少了第一格，把判定寫成「不檢查」也會通過。
#[tokio::test]
async fn d_role_assign_alone_grants_read_access_to_the_list() {
    let ctx = &TestContext::setup().await;

    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/users/{USER_TECH}/role-assignments")),
            &fm,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "FACILITY_ADMIN 兩個權限都沒有（031 拿掉了它的 role:assign）：{body}"
    );

    grant_org_manager(ctx, USER_FACILITY_ADMIN, ORG_PROP).await;
    let om = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/users/{USER_TECH}/role-assignments")),
            &om,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "只有 role:assign 也要看得到 —— 否則指派得了卻看不到自己指派了什麼：{body}"
    );
    let items = body["items"].as_array().cloned().unwrap_or_default();
    assert!(
        items.iter().any(|i| i["role_code"] == "TECHNICIAN"),
        "tech.wang 的 TECHNICIAN 指派要在清單裡：{items:?}"
    );

    ctx.teardown().await;
}

/// 撤銷：兩種「不撤」與一種「真的撤掉」。
///
/// 兩種 422 都是同一類問題 —— **回報一個不會成立的結果**：
///   * `DIRECTORY_SYNC` 的指派下一輪同步就會加回來
///   * 撤銷自己的角色可能讓最後一個管理員把自己鎖在門外
///     （與 `POST /users/{id}/suspend` 同一條理由）
#[tokio::test]
async fn e_revoke_refuses_the_two_cases_that_would_not_stick() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // (1) 目錄同步來的指派
    let synced: String = {
        let mut tx = ctx.owner_tx().await;
        let id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fms.user_role_assignments
               (tenant_id, user_id, role_id, scope_type, scope_id, source)
             SELECT $1::uuid, $2::uuid, r.id, 'FACILITY', $3::uuid, 'DIRECTORY_SYNC'
               FROM fms.roles r WHERE r.code = 'VIEWER' AND r.tenant_id IS NULL
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(USER_REQUESTER)
        .bind(FACILITY_HQ)
        .fetch_one(&mut *tx)
        .await
        .expect("插入目錄同步指派");
        tx.commit().await.expect("commit");
        id.to_string()
    };
    let (status, body) = ctx
        .send(authed(
            del(&format!("/api/v1/role-assignments/{synced}")),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "目錄同步的指派撤了也會回來，回 204 等於報告一個不成立的結果：{body}"
    );

    // (2) 撤銷自己的
    let own: String = {
        let (_, list) = ctx
            .send(authed(
                get(&format!(
                    "/api/v1/users/{}/role-assignments",
                    admin_user_id()
                )),
                &admin,
            ))
            .await;
        list["items"][0]["id"].as_str().expect("自己的指派").into()
    };
    let (status, body) = ctx
        .send(authed(
            del(&format!("/api/v1/role-assignments/{own}")),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "撤掉自己最後一個管理角色之後沒有人能把你放回來：{body}"
    );

    // (3) 反面：正常的撤銷要真的成功，而且真的消失。
    //     角色刻意不是 VIEWER —— (1) 已經給了 USER_REQUESTER 一筆
    //     VIEWER@總部，而 (user_id, role_id, scope_type, scope_id) 是唯一的。
    //     第一版就是踩到這裡：回了一個沒有主詞的 409。
    let body =
        json!({ "role_code": "DISPATCHER", "scope_type": "FACILITY", "scope_id": FACILITY_HQ });
    let (status, created) = assign(ctx, &admin, USER_REQUESTER, body.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap();

    // (4) 重複指派要回 409，而且訊息要說得出**為什麼**。
    //     原本的「a conflicting record already exists」沒有主詞，
    //     而重複最可能的成因（目錄同步已經給過了）完全看不出來。
    let (status, dup) = assign(ctx, &admin, USER_REQUESTER, body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{dup}");
    assert!(
        dup["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("已經有這個角色"),
        "409 要說得出是哪一種衝突：{dup}"
    );

    let (status, _) = ctx
        .send(authed(
            del(&format!("/api/v1/role-assignments/{id}")),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, after) = ctx
        .send(authed(
            get(&format!("/api/v1/users/{USER_REQUESTER}/role-assignments")),
            &admin,
        ))
        .await;
    let still_there = after["items"]
        .as_array()
        .map(|a| a.iter().any(|i| i["id"] == id))
        .unwrap_or(false);
    assert!(!still_there, "204 之後那筆必須真的不見了：{after}");

    ctx.teardown().await;
}

/// 029 稽核了 `user_role_assignments`，但在這支端點之前**沒有任何已實作的
/// 端點會寫它** —— 那份軌跡至今沒有一列來自真實操作。
///
/// 授權變更是稽核最該記的東西：`role:assign` 就在 `is_dangerous` 清單裡。
#[tokio::test]
async fn f_assignment_writes_an_audit_row_with_the_real_actor() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, created) = assign(
        ctx,
        &admin,
        USER_REQUESTER,
        json!({ "role_code": "VIEWER", "scope_type": "FACILITY", "scope_id": FACILITY_HQ }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();

    ctx.send(authed(
        del(&format!("/api/v1/role-assignments/{id}")),
        &admin,
    ))
    .await;

    let mut tx = ctx.owner_tx().await;
    let rows: Vec<(String, Option<uuid::Uuid>)> = sqlx::query_as(
        "SELECT action, actor_user_id FROM fms.audit_log
          WHERE entity_type = 'USER_ROLE_ASSIGNMENTS' AND entity_id = $1::uuid
          ORDER BY occurred_at",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await
    .expect("讀稽核");
    tx.commit().await.expect("commit");

    let actions: Vec<&str> = rows.iter().map(|(a, _)| a.as_str()).collect();
    assert!(
        actions.contains(&"CREATE") && actions.contains(&"DELETE"),
        "指派與撤銷都要留痕：{actions:?}"
    );
    let expected = admin_user_id();
    assert!(
        rows.iter().all(|(_, a)| *a == Some(expected)),
        "稽核列的 actor 必須是真正發出請求的人：{rows:?}"
    );

    ctx.teardown().await;
}
