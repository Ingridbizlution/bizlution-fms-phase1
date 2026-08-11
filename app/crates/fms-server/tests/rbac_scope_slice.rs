//! `min_scope_level` 的執行（`docs/security-review-open-items.md` 第 2 項）。
//!
//! 這一項的行為只能在有示範資料時驗證，而 026／027 在 CORE 裡執行、位置
//! 早於 009，因此 migration 的自我驗證刻意只斷言宣告與述詞存在，
//! 行為層的斷言全部在這裡。
//!
//! 三個方向都要有：
//!   1. **收斂真的發生** —— 場域範圍的授權無法執行租戶級動作
//!   2. **沒有過度收斂** —— 兩支已上線的讀取端點對場域範圍的角色仍然可用
//!      （026 若不修正那四格宣告，這裡就會變成 403）
//!   3. **ORG 範圍不能逃出自己的子樹** —— 建立根組織需要 TENANT 範圍

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::json;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
/// 009 的組織樹：集團 → 事業部 → 各場域所屬組織。
const ORG_HQ_DIVISION: &str = "bbbbbbbb-0000-4000-8000-000000000002";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 把某個角色指派給某個使用者，範圍指定為單一場域。
///
/// 這是第 2 項的攻擊形狀：管理員以為「範圍限在一個場域」就限制住了，
/// 但對沒有 `facility_id` 的物件而言，026 之前那個範圍在權限展開時被丟掉。
async fn grant_at_facility(ctx: &TestContext, username: &str, role_code: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.user_role_assignments
             (tenant_id, user_id, role_id, scope_type, scope_id, source)
         SELECT u.tenant_id, u.id, r.id, 'FACILITY', $3::uuid, 'MANUAL'
           FROM fms.users u, fms.roles r
          WHERE u.username::text = $1 AND r.code = $2",
    )
    .bind(username)
    .bind(role_code)
    .bind(FACILITY_HQ)
    .execute(&mut *tx)
    .await
    .expect("grant role at facility scope");
    tx.commit().await.expect("commit grant");
}

#[tokio::test]
async fn a_facility_scoped_grant_cannot_perform_tenant_level_actions() {
    let ctx = &TestContext::setup().await;

    // fm.lin 原本是 FACILITY_ADMIN@FACILITY。額外把 TENANT_ADMIN 也指派給他，
    // 但**範圍仍限在單一場域**。026 之前這會讓他取得 TENANT_ADMIN 的全部權限。
    grant_at_facility(ctx, USERNAME_FACILITY_ADMIN, "TENANT_ADMIN").await;
    let token = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/organizations",
                json!({ "code": "escalated", "name": "越權建立的組織", "org_type": "DEPARTMENT" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "場域範圍的授權不得建立租戶級物件（026 之前這裡是 201）: {body}"
    );
    assert_eq!(body["code"], "PERMISSION_DENIED");

    // 建立場域同樣是租戶／組織級動作。027 之前這裡也會過權限檢查，
    // 只是被 007 的 facility_scope 政策擋在後面 —— 現在應由權限判定給出 403。
    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/facilities",
                json!({ "org_id": ORG_HQ_DIVISION, "code": "ESCALATED", "name": "越權建立的場域" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "PERMISSION_DENIED");

    ctx.teardown().await;
}

/// 026 的視圖述詞本身。
///
/// 前一個測試驗的其實是 handler 的修正：`create_org` 與 `create_facility` 現在
/// 走 `user_permission_codes` 的範圍述詞（016 就有），那已經足以擋住那兩條路。
/// 026 守的是不同的東西 —— **所有仍用 `require_permission(.., None, None)` 慣例的
/// 端點**，因為那個組合會落到 `user_permission_codes_anywhere`，而它沒有範圍述詞。
///
/// `/auth/me` 是最直接的觀測點：它讀同一個視圖。若視圖沒有過濾，租戶級權限
/// 就會出現在清單裡 —— 那既是「未來的端點只要照現有慣例寫就會有洞」的證據，
/// 也是一個當下的問題：API 不該向前端宣告一組實際上用不了的權限。
#[tokio::test]
async fn a_narrow_grant_does_not_confer_tenant_level_permissions_anywhere() {
    let ctx = &TestContext::setup().await;
    grant_at_facility(ctx, USERNAME_FACILITY_ADMIN, "TENANT_ADMIN").await;
    let token = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, body) = ctx.send(authed(get("/api/v1/auth/me"), &token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let held: Vec<String> = body["permissions"]
        .as_array()
        .expect("permissions 應為陣列")
        .iter()
        .map(|p| {
            // 格式是 `permission@scope_type[:scope_id]`
            p.as_str().unwrap().split('@').next().unwrap().to_string()
        })
        .collect();

    // 這些都宣告 TENANT，而他的授權全部是 FACILITY 範圍。
    // role:write + role:assign 合起來是提權鏈，是這一項最該擋住的組合。
    for tenant_only in [
        "role:write",
        "tenant:update",
        "identity_provider:write",
        "user:write",
        "quota:manage",
    ] {
        assert!(
            !held.contains(&tenant_only.to_string()),
            "場域範圍的授權不該取得 {tenant_only}（實際持有：{held:?}）"
        );
    }

    // 反面：場域級的權限必須還在，否則就是把收斂做成了功能回歸。
    for still_held in ["work_order:create", "asset:write", "facility:update"] {
        assert!(
            held.contains(&still_held.to_string()),
            "{still_held} 是場域級權限，不該被收斂掉（實際持有：{held:?}）"
        );
    }

    ctx.teardown().await;
}

#[tokio::test]
async fn facility_scoped_roles_keep_the_reads_they_need() {
    let ctx = &TestContext::setup().await;
    // FACILITY_ADMIN，範圍只有總部一個場域。009 給他的三個 TENANT 宣告權限
    // （asset_model:read / organization:read / user:read）若不在 026 裡改成
    // FACILITY，這兩支端點會從 200 變 403 —— 也就是把安全修正做成功能回歸。
    let token = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, body) = ctx.send(authed(get("/api/v1/asset-models"), &token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "asset_model:read 是共用型錄查詢，不該需要 TENANT 範圍: {body}"
    );

    let (status, body) = ctx.send(authed(get("/api/v1/organizations"), &token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "場域管理員要看得到自己屬於哪個組織: {body}"
    );

    ctx.teardown().await;
}

#[tokio::test]
async fn creating_a_root_organization_needs_tenant_scope() {
    let ctx = &TestContext::setup().await;

    // `ORG_MANAGER` 指派在**組織**範圍。
    //
    // 這一段原本必須用「TENANT_ADMIN 指派在 ORG 範圍」這個權宜做法：026 讓
    // `organization:write` 宣告 ORG，但 008 從未把它給過 ORG_MANAGER，
    // 因此那條路徑實際上沒有人走得到。031 補上了那筆授權（產品決定 #5），
    // 於是測試可以用真正的角色，而不是一個為了驗述詞而拼出來的組合。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.user_role_assignments
                 (tenant_id, user_id, role_id, scope_type, scope_id, source)
             SELECT u.tenant_id, u.id, r.id, 'ORG', $2::uuid, 'MANUAL'
               FROM fms.users u, fms.roles r
              WHERE u.username::text = $1 AND r.code = 'ORG_MANAGER'",
        )
        .bind(USERNAME_REQUESTER)
        .bind(ORG_HQ_DIVISION)
        .execute(&mut *tx)
        .await
        .expect("grant ORG_MANAGER at org scope");
        tx.commit().await.expect("commit");
    }
    let token = ctx.login_as(USERNAME_REQUESTER).await;

    // 子樹內：parent 指向自己範圍的組織 → 應可建立
    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/organizations",
                json!({
                    "code": "sub_dept", "name": "子樹內的部門",
                    "org_type": "DEPARTMENT", "parent_id": ORG_HQ_DIVISION
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "ORG 範圍的授權應能在自己的子樹內建立組織: {body}"
    );

    // 不帶 parent_id → 根組織，落在任何 ORG 子樹之外，只有 TENANT 範圍能做。
    // 述詞的 ORG 分支要求 o_target.org_path IS NOT NULL，因此這是既有判定的
    // 自然結果，不是額外加上的規則。
    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/organizations",
                json!({ "code": "new_root", "name": "根組織", "org_type": "COMPANY" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ORG 範圍的授權不得建立根組織（那是逃出自己的子樹）: {body}"
    );
    assert_eq!(body["code"], "PERMISSION_DENIED");

    ctx.teardown().await;
}

#[tokio::test]
async fn tenant_scoped_admin_is_unaffected() {
    let ctx = &TestContext::setup().await;
    // admin.chen 是 TENANT_ADMIN@TENANT。收斂不該碰到他 ——
    // 一個把管理員也擋掉的「安全修正」只會被關掉。
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/organizations",
                json!({ "code": "legit_root", "name": "合法的根組織", "org_type": "COMPANY" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/facilities",
                json!({ "org_id": ORG_HQ_DIVISION, "code": "NEWFAC", "name": "新場域" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    ctx.teardown().await;
}

/// 目錄層的不變量：**沒有角色持有比自己宣告範圍更寬的權限。**
///
/// 那種組合在該角色的正常指派範圍內永遠用不到（026 之後展不開），
/// 因此它只會誤導讀目錄的人 ——「場域級的 VIEWER 看得到稽核日誌」。
///
/// 031 量了當時剩下的五筆並把門檻定在「不超過 5」（只擋惡化）；
/// 045 把它們清成 0。
///
/// **這個測試存在的理由是 migration 的自我驗證守不住未來。** 045 的 DO 區塊
/// 只在部署那一刻（與 roundtrip 重跑時）檢查；日後某個 migration 加回一筆
/// 這種組合，045 不會再跑，也不會有任何症狀 —— 那正是 026 之前的狀態。
/// 放在測試裡才是每次都檢查。
#[tokio::test]
async fn no_role_holds_a_permission_wider_than_its_own_scope() {
    let ctx = &TestContext::setup().await;
    let mut tx = ctx.owner_tx().await;

    let offenders: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT r.code, r.scope_level, p.code, p.min_scope_level
           FROM fms.roles r
           JOIN fms.role_permissions rp ON rp.role_id = r.id
           JOIN fms.permissions p ON p.code = rp.permission_code
          WHERE fms.scope_width(r.scope_level) < fms.scope_width(p.min_scope_level)
          ORDER BY r.code, p.code",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("查範圍不一致");

    assert!(
        offenders.is_empty(),
        "有角色持有比自己範圍更寬的權限，那些授權永遠不會生效：\n{}\n\
         要嘛移除授權，要嘛在**知道對應端點 payload** 的情況下降低 \
         permissions.min_scope_level —— 045 的檔頭說明了為什麼預設選前者。",
        offenders
            .iter()
            .map(|(role, rs, perm, ps)| format!("  {role}（{rs}）持有 {perm}（要求 {ps}）"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    ctx.teardown().await;
}
