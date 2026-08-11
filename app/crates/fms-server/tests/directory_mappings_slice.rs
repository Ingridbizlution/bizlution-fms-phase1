//! 目錄群組 → 角色對應（`/directory-role-mappings`）。
//!
//! 這一組的核心是 `b_`：**這裡是 052 的一條繞道，若不補上同一道閘。**
//!
//! `POST /users/{id}/role-assignments` 擋得住「`TENANT_ADMIN` 指派
//! `PLATFORM_ADMIN`」（他沒有 `user:impersonate`）。但一條
//! `群組 X → PLATFORM_ADMIN @ TENANT` 的對應做的是同一件事，只是晚一輪同步。
//!
//! `b_` 把兩條路徑**並排跑**：同一個人、同一個角色、同一個範圍，
//! 兩邊都必須 403。只測其中一邊的話，另一邊就是開著的門。
//!
//! # `d_` 的第二格：只填 `claim_value` 的對應必須建不起來（migration 077）
//!
//! 這一組原本用只填 `claim_value` 的請求體當「正常路徑」，而那正好是缺陷本身：
//! 002 允許二選一、handler 照著放行回 201，但 058 的對帳是對
//! `directory_groups` 的內連接，那種列會被靜默丟掉 —— 對應建得起來、
//! 永遠不授予任何角色、而且**沒有任何症狀**。
//!
//! 所以那個請求體現在是 `d_` 的斷言對象，而不是 `a_` 的前提。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const USER_REQUESTER: &str = "ffffffff-0000-4000-8000-000000000004";
/// 009 種下的兩個目錄群組。對應必須錨定在已同步的群組上（077），
/// 因此每個請求體都要一個真的群組 id。
const GROUP_FACILITY_ADMINS: &str = "eeeeeeee-0000-4000-8000-000000000001";
const GROUP_TECHNICIANS: &str = "eeeeeeee-0000-4000-8000-000000000002";

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

async fn create_mapping(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request("POST", "/api/v1/directory-role-mappings", body),
        token,
    ))
    .await
}

/// 正常路徑：建得起來、列得到、而且欄位看得懂。
#[tokio::test]
async fn a_a_mapping_can_be_created_and_listed() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 009 的示範租戶**已經有**目錄對應。第一版假設這張表是空的，
    // 於是斷言「清單長度 = 1」而拿到 3。量增量而不是絕對值。
    let (_, before) = ctx
        .send(authed(get("/api/v1/directory-role-mappings"), &admin))
        .await;
    let n_before = before["items"].as_array().map(|a| a.len()).unwrap_or(0);

    let (status, created) = create_mapping(
        ctx,
        &admin,
        json!({
            "directory_group_id": GROUP_TECHNICIANS,
            "role_code": "TECHNICIAN",
            "scope_type": "FACILITY",
            "scope_id": FACILITY_HQ,
            "priority": 50
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["role_code"], "TECHNICIAN");
    assert_eq!(
        created["directory_group_name"], "FMS-Technicians",
        "對應必須錨定在一列已同步的群組上，而名稱要帶出來：{created}"
    );
    assert!(
        created["claim_value"].is_null(),
        "claim_value 沒有任何消費者，API 不寫入它：{created}"
    );
    assert_eq!(created["is_active"], true, "預設啟用");
    assert_eq!(
        created["scope_label"], "台北總部大樓",
        "只回 scope_id 的話 UI 得為每一列再查一次那個 uuid：{created}"
    );

    let (status, listed) = ctx
        .send(authed(get("/api/v1/directory-role-mappings"), &admin))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        items.len(),
        n_before + 1,
        "新增的那一條要在清單裡：{listed}"
    );
    assert!(
        items.iter().any(|m| m["id"] == created["id"]),
        "而且要是**這一條** —— 只比數量的話，建錯一條也會通過：{listed}"
    );
    // priority 50 比種子的預設小，所以排在最前面（數字小的先套用）。
    assert_eq!(
        items[0]["id"], created["id"],
        "priority 要真的影響順序，否則那個欄位只是裝飾：{listed}"
    );

    ctx.teardown().await;
}

/// **繞道測試。** 同一個人、同一個角色、同一個範圍，兩條路徑都必須 403。
///
/// `user:impersonate` 是 `is_dangerous` 且**連 TENANT_ADMIN 都沒有**
/// （只有 PLATFORM_ADMIN 持有），因此 `PLATFORM_ADMIN` 是租戶裡權力最大的人
/// 也授不出去的角色。
///
/// 少了目錄對應這一半，`role:write` 就是 `role:assign` 的無限制版本：
/// 擋得住直接指派，擋不住「對應到一個自己加得進去的 AD 群組」。
#[tokio::test]
async fn b_the_mapping_route_cannot_bypass_the_escalation_guard() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 路徑 1：直接指派 —— 已知會被 052 擋下。先驗它，確立基準。
    let (status, direct) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/users/{USER_REQUESTER}/role-assignments"),
                json!({ "role_code": "PLATFORM_ADMIN", "scope_type": "TENANT" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "基準沒有成立 —— 直接指派 PLATFORM_ADMIN 本來就該被擋：{direct}"
    );
    assert!(
        direct["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("user:impersonate"),
        "{direct}"
    );

    // 路徑 2：同一件事，改走目錄對應。必須同樣被擋。
    let (status, via_mapping) = create_mapping(
        ctx,
        &admin,
        json!({
            "directory_group_id": GROUP_FACILITY_ADMINS,
            "role_code": "PLATFORM_ADMIN",
            "scope_type": "TENANT"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "**繞道成立了** —— 直接指派擋得住，但把同一個角色對應到目錄群組就過了。\
         下一輪同步會把 user:impersonate 發給群組裡的每一個人：{via_mapping}"
    );
    assert!(
        via_mapping["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("user:impersonate"),
        "訊息要說得出是哪一項，兩條路徑一致：{via_mapping}"
    );

    ctx.teardown().await;
}

/// 刪除對應**不會**撤銷已經發出的授權，而回應要說出這件事。
///
/// 少了那個數字，「我刪掉對應了，為什麼他還進得來」會變成一次除錯，
/// 而答案只是「還沒同步」。這是這個專案反覆出現的缺陷類型：
/// 動作成功了，但它沒有達成使用者以為它達成的事。
#[tokio::test]
async fn c_deleting_a_mapping_reports_the_assignments_it_leaves_behind() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 造一個目錄群組 + 一筆由它產生的授權，再造對應指向同一個群組。
    let group_id: uuid::Uuid = {
        let mut tx = ctx.owner_tx().await;
        let gid: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fms.directory_groups
               (tenant_id, identity_provider_id, external_group_id, name)
             SELECT $1::uuid, ip.id, 'ext-tech', '設施維護群組'
               FROM fms.identity_providers ip WHERE ip.tenant_id = $1::uuid LIMIT 1
             RETURNING id",
        )
        .bind(TENANT_ID)
        .fetch_one(&mut *tx)
        .await
        .expect("建群組");
        sqlx::query(
            "INSERT INTO fms.user_role_assignments
               (tenant_id, user_id, role_id, scope_type, scope_id, source,
                origin_directory_group_id)
             SELECT $1::uuid, $2::uuid, r.id, 'FACILITY', $3::uuid,
                    'DIRECTORY_SYNC', $4::uuid
               FROM fms.roles r WHERE r.code = 'TECHNICIAN' AND r.tenant_id IS NULL",
        )
        .bind(TENANT_ID)
        .bind(USER_REQUESTER)
        .bind(FACILITY_HQ)
        .bind(gid)
        .execute(&mut *tx)
        .await
        .expect("建同步授權");
        tx.commit().await.expect("commit");
        gid
    };

    let (status, created) = create_mapping(
        ctx,
        &admin,
        json!({
            "directory_group_id": group_id,
            "role_code": "TECHNICIAN",
            "scope_type": "FACILITY",
            "scope_id": FACILITY_HQ
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        created["directory_group_name"], "設施維護群組",
        "群組名稱要帶出來：{created}"
    );
    let id = created["id"].as_str().unwrap();

    let (status, deleted) = ctx
        .send(authed(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/directory-role-mappings/{id}"))
                .body(Body::empty())
                .unwrap(),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(
        deleted["orphaned_assignments"], 1,
        "刪掉對應不會撤銷已發出的授權，回應必須說出還有幾筆掛著：{deleted}"
    );

    // 反面：那筆授權真的還在（若它其實被連帶刪了，上面的數字就是在說謊）。
    let mut tx = ctx.owner_tx().await;
    let still: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.user_role_assignments
          WHERE origin_directory_group_id = $1",
    )
    .bind(group_id)
    .fetch_one(&mut *tx)
    .await
    .expect("查授權");
    tx.commit().await.expect("commit");
    assert_eq!(still, 1, "授權應該還在，等下一輪同步收回");

    ctx.teardown().await;
}

/// 輸入驗證：對應必須錨定在群組上、`claim_value` 不接受寫入，且權限要擋得住。
#[tokio::test]
async fn d_input_and_permission_are_checked() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 沒給群組 —— 交給 CHECK 擋只會得到一個說不出「該去哪裡拿 id」的 23514。
    let (status, body) = create_mapping(
        ctx,
        &admin,
        json!({ "role_code": "VIEWER", "scope_type": "TENANT" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("directory_group_id"),
        "訊息要說得出缺什麼：{body}"
    );

    // **這個缺陷本身。** 只填 claim_value 曾經回 201，然後那條規則永遠不會
    // 授予任何角色 —— 058 的對帳是對 directory_groups 的內連接，它會被靜默
    // 丟掉，而且沒有任何症狀。回 422 而不是「201 + 一句提醒」的理由與下一格
    // 的 SPATIAL_NODE 相同：永遠不會生效的授權規則是設定錯誤，不是資訊不足。
    let (status, body) = create_mapping(
        ctx,
        &admin,
        json!({
            "claim_value": "CN=Facilities,OU=Groups,DC=bizlution,DC=com",
            "role_code": "VIEWER",
            "scope_type": "TENANT"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "只填 claim_value 的對應**建起來了** —— 它永遠不會授予任何角色，\
         而管理者拿到 201 之後不會知道：{body}"
    );
    // 訊息要說得出**該改用什麼**。少了這一格，把 handler 的檢查拿掉也會通過
    // 上面那個斷言 —— 077 的約束會擋下 INSERT，但回的是一句只有約束名字的話，
    // 而讀到它的人拿不到「去 sync 群組」這個下一步。
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("directory_group_id"),
        "422 說不出該改用什麼：{body}"
    );

    // 連同一個真的群組一起填也要擋：靜默存下一個沒有人讀的值，
    // 等於讓下一個讀這張表的人以為 claim 比對是活的。
    let (status, body) = create_mapping(
        ctx,
        &admin,
        json!({
            "directory_group_id": GROUP_TECHNICIANS,
            "claim_value": "CN=Facilities,DC=bizlution,DC=com",
            "role_code": "VIEWER",
            "scope_type": "TENANT"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "claim_value 沒有消費者，不該被靜默存下來：{body}"
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("claim_value"),
        "訊息要說得出是哪個欄位不被接受：{body}"
    );

    // SPATIAL_NODE 會產生不生效的授權 —— 與角色指派同一個理由。
    let (status, body) = create_mapping(
        ctx,
        &admin,
        json!({
            "directory_group_id": GROUP_TECHNICIANS,
            "role_code": "VIEWER",
            "scope_type": "SPATIAL_NODE",
            "scope_id": FACILITY_HQ
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // FACILITY_ADMIN 沒有 role:write。
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, body) = create_mapping(
        ctx,
        &fm,
        json!({
            "directory_group_id": GROUP_TECHNICIANS,
            "role_code": "VIEWER",
            "scope_type": "TENANT"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = ctx
        .send(authed(get("/api/v1/directory-role-mappings"), &fm))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "清單要 role:read，FACILITY_ADMIN 沒有：{body}"
    );

    ctx.teardown().await;
}
