//! 角色目錄與權限字典（`/roles`、`/permissions`）。
//!
//! 這一組的核心是 **`c_...`：把提權鏈整條跑一次。**
//!
//! `docs/security-review-open-items.md` 記著這條鏈 ——
//! 「`role:write` + `role:assign` 合起來是提權鏈：鑄造一個含任意權限的角色
//! 再指派給自己」。052 的檔頭主張那條鏈已經斷了，但那是**推論**。
//! 這一格真的鑄造、真的指派，證明兩步都被擋，而且是被**不同的**東西擋的。
//!
//! 素材是 `user:impersonate`：它 `is_dangerous`，而**連 TENANT_ADMIN 都沒有**
//! （目錄裡只有 PLATFORM_ADMIN 持有）。因此租戶裡權力最大的人也造不出
//! 一個含它的可用角色 —— 那正是這條防線該有的形狀。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const ORG_PROP: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const USER_FACILITY_ADMIN: &str = "ffffffff-0000-4000-8000-000000000002";
const USER_REQUESTER: &str = "ffffffff-0000-4000-8000-000000000004";
/// `is_dangerous`，且**連 TENANT_ADMIN 都沒有**（只有 PLATFORM_ADMIN 持有）。
const DANGEROUS_NOBODY_HAS: &str = "user:impersonate";

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

/// `GET /roles` 收 `role:assign` —— 與角色指派清單同一個病灶。
///
/// 目錄裡只有 PLATFORM_ADMIN 與 TENANT_ADMIN 持有 `role:read`，
/// 而 ORG_MANAGER 持有的是 `role:assign`。照原契約做，他**指派得了角色卻
/// 列不出有哪些角色可以指派** —— UI 連下拉選單都填不出來。
///
/// 三格一起驗，缺任何一格都可以用錯的實作騙過去：
///   * 兩個權限都沒有 → 403（少了它，「不檢查」也會通過）
///   * 只有 `role:assign` → 200（少了它，維持原契約也會通過）
///   * `role:read` → 200
#[tokio::test]
async fn a_listing_roles_accepts_either_read_or_assign() {
    let ctx = &TestContext::setup().await;

    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, body) = ctx.send(authed(get("/api/v1/roles"), &fm)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "FACILITY_ADMIN 兩個權限都沒有：{body}"
    );

    grant_org_manager(ctx, USER_FACILITY_ADMIN, ORG_PROP).await;
    let om = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, body) = ctx.send(authed(get("/api/v1/roles"), &om)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "只有 role:assign 也要列得出角色，否則指派時填不出 role_code：{body}"
    );
    let items = body["items"].as_array().cloned().unwrap_or_default();
    assert!(items.len() >= 12, "12 個平台角色都要在：{}", items.len());

    let admin = ctx.login_as(USERNAME).await;
    let (status, body) = ctx
        .send(authed(
            get("/api/v1/roles?q=tech&assignable_only=true"),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let codes: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["code"].as_str())
        .collect();
    assert_eq!(codes, vec!["TECHNICIAN"], "q 過濾要真的過濾：{codes:?}");

    ctx.teardown().await;
}

/// 權限字典必須回 `is_dangerous` 與 `min_scope_level`。
///
/// 兩個欄位都是「不回就會出事」而不是「回了比較好看」：
///   * `is_dangerous` —— 052 之後它決定誰可以把這項權限授出去。
///     UI 看不到它，管理員就無法理解指派為什麼回 403。
///   * `min_scope_level` —— 更隱蔽：一項宣告 TENANT 的權限被指派在 ORG 範圍時
///     **不會報錯，只是靜默地不生效**（026 在視圖層過濾掉）。
///
/// 也驗權限**維持** `role:read`（沒有跟著 `GET /roles` 一起放寬）：
/// 那裡放寬是因為下拉選單真的填不出來，這裡沒有那個斷掉的流程。
/// 沒有這一格，「順手一起放寬」不會被任何東西擋下。
#[tokio::test]
async fn b_permission_dictionary_exposes_the_two_fields_that_change_behaviour() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ctx.send(authed(get("/api/v1/permissions"), &admin)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().cloned().unwrap_or_default();

    let dangerous = items.iter().filter(|p| p["is_dangerous"] == true).count();
    assert!(
        dangerous >= 10,
        "is_dangerous 是提權防護的依據，字典裡不該只剩零星幾項：{dangerous}"
    );
    assert!(
        items
            .iter()
            .any(|p| p["min_scope_level"] == "TENANT" && p["code"] == "role:write"),
        "min_scope_level 要如實回報 —— role:write 是 TENANT"
    );

    let (status, filtered) = ctx
        .send(authed(get("/api/v1/permissions?module=ADMIN"), &admin))
        .await;
    assert_eq!(status, StatusCode::OK, "{filtered}");
    assert!(
        filtered["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["module"] == "ADMIN"),
        "module 過濾要真的過濾"
    );

    // 維持 role:read：ORG_MANAGER 沒有它。
    grant_org_manager(ctx, USER_FACILITY_ADMIN, ORG_PROP).await;
    let om = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, denied) = ctx.send(authed(get("/api/v1/permissions"), &om)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "字典維持 role:read —— GET /roles 放寬是因為下拉選單填不出來，這裡沒有那個斷掉的流程：{denied}"
    );

    ctx.teardown().await;
}

/// **整條提權鏈跑一次。**
///
/// review 記的鏈：`role:write` + `role:assign` → 鑄造一個含任意權限的角色，
/// 再指派給自己。052 的檔頭主張那條鏈已經斷了 —— 這一格證明它，
/// 而且證明**兩步各自被不同的東西擋住**：
///
///   步驟 1（鑄造）→ `POST /roles` 的縱深防禦
///   步驟 2（指派）→ 052 的 `role_grant_blocked_by`
///
/// 為了測步驟 2，必須繞過步驟 1 的守衛（用 `owner_tx` 直接鑄造）——
/// 那正是這一格的價值：**即使鑄造的守衛整個不存在，鏈仍然是斷的。**
/// 只測步驟 1 的話，拿掉 052 也不會有任何測試失敗。
#[tokio::test]
async fn c_the_mint_then_assign_escalation_chain_is_broken_at_both_steps() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // --- 步驟 1：連 TENANT_ADMIN 都鑄造不出含 user:impersonate 的角色 -------
    let (status, denied) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/roles",
                json!({
                    "code": "ESCALATOR",
                    "name": "提權測試角色",
                    "permissions": ["alarm:read", DANGEROUS_NOBODY_HAS],
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "租戶管理員也沒有 user:impersonate，不該能把它放進新角色：{denied}"
    );
    assert!(
        denied["detail"]
            .as_str()
            .unwrap_or_default()
            .contains(DANGEROUS_NOBODY_HAS),
        "訊息要說得出是哪一項：{denied}"
    );

    // --- 步驟 2：繞過步驟 1 直接鑄造，指派仍然要被 052 擋下 -----------------
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "WITH r AS (
               INSERT INTO fms.roles (tenant_id, code, name, is_system, scope_level)
               VALUES ($1::uuid, 'ESCALATOR', '繞過鑄造守衛的角色', false, 'FACILITY')
               RETURNING id)
             INSERT INTO fms.role_permissions (role_id, permission_code)
             SELECT r.id, c FROM r, unnest(ARRAY['alarm:read', $2]) c",
        )
        .bind(TENANT_ID)
        .bind(DANGEROUS_NOBODY_HAS)
        .execute(&mut *tx)
        .await
        .expect("直接鑄造");
        tx.commit().await.expect("commit");
    }

    let (status, denied) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/users/{USER_REQUESTER}/role-assignments"),
                json!({ "role_code": "ESCALATOR", "scope_type": "TENANT" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "即使角色已經鑄造出來，指派仍必須被 052 擋下 —— 這才是鏈斷掉的地方：{denied}"
    );
    assert!(
        denied["detail"]
            .as_str()
            .unwrap_or_default()
            .contains(DANGEROUS_NOBODY_HAS),
        "052 的訊息要說得出缺哪一項：{denied}"
    );

    ctx.teardown().await;
}

/// 反面：正常的自訂角色要建得起來、列得到、而且真的能指派。
///
/// 少了這一格，把 `POST /roles` 寫成「一律拒絕」會讓上一格通過，
/// 而那等於這支端點不存在。
#[tokio::test]
async fn d_a_custom_role_can_be_created_listed_and_assigned() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/roles",
                json!({
                    "code": "contract_reviewer",
                    "name": "合約覆核",
                    "scope_level": "ORG",
                    "permissions": ["alarm:read", "alarm:acknowledge"],
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["is_system"], false, "自訂角色不是系統角色");
    assert!(
        created["tenant_id"].is_string(),
        "自訂角色必須綁租戶，否則會變成所有租戶共用的平台角色：{created}"
    );
    assert_eq!(
        created["permissions"],
        json!(["alarm:acknowledge", "alarm:read"])
    );

    let (_, listed) = ctx
        .send(authed(get("/api/v1/roles?q=contract"), &admin))
        .await;
    assert_eq!(listed["items"].as_array().map(|a| a.len()), Some(1));

    // 真的能指派 —— 只驗「建得起來」的話，一個不含權限的空殼也會通過。
    let (status, assigned) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/users/{USER_REQUESTER}/role-assignments"),
                json!({ "role_code": "contract_reviewer", "scope_type": "ORG", "scope_id": ORG_PROP }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "自訂角色要指派得了：{assigned}"
    );

    ctx.teardown().await;
}

/// 兩種輸入錯誤，訊息都要說得出主詞。
///
/// 權限碼是手打的字串 —— 拼錯是常態不是例外。交給外鍵擋只會得到一個 23503，
/// 看不出是哪一個碼錯了。
#[tokio::test]
async fn e_bad_input_says_which_thing_was_wrong() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/roles",
                json!({ "code": "TYPO_ROLE", "name": "拼錯", "permissions": ["alarm:reed"] }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("alarm:reed"),
        "要說得出是哪一個碼拼錯了：{body}"
    );

    // 與平台角色同名 —— **資料庫其實允許**（唯一索引的鍵是
    // `coalesce(tenant_id, 全零 uuid)`，兩者分屬不同命名空間）。
    // handler 擋下來，因為 assign 解析 role_code 時 `ORDER BY tenant_id NULLS LAST`
    // 會讓租戶角色遮蔽平台角色，於是「指派了 TECHNICIAN 卻什麼都不能做」——
    // 而那一路上不會有任何錯誤訊息。
    //
    // 這一格原本寫錯：我以為兩者共用命名空間、以為唯一索引會擋。跑出來是 201。
    // 假設錯了，但問題問對了。
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/roles",
                json!({ "code": "technician", "name": "撞名" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "與平台角色同名會在指派時無聲遮蔽它：{body}"
    );
    assert!(
        body["detail"].as_str().unwrap_or_default().contains("遮蔽"),
        "訊息要說得出為什麼不是「重複」而是「會遮蔽」：{body}"
    );

    // 反面：本租戶自己的自訂角色重複，走的是唯一索引那條路。
    let dup = json!({ "code": "DUP_ROLE", "name": "重複" });
    let (status, _) = ctx
        .send(authed(
            json_request("POST", "/api/v1/roles", dup.clone()),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = ctx
        .send(authed(json_request("POST", "/api/v1/roles", dup), &admin))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("自訂角色"),
        "兩種 409 要分得出來：{body}"
    );

    ctx.teardown().await;
}
