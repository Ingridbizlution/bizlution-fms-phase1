//! 稽核日誌查詢（`/audit-log`）。
//!
//! 這一組最重要的是 `a_...`：**租戶級的稽核事件讀得到。**
//!
//! 在 migration 053 之前讀不到，而那不是「少一個功能」——
//! `audit_log.facility_scope` 用 `facility_id = ANY(current_facility_ids())`
//! 比對，租戶級事件的 `facility_id` 是 NULL，而 `NULL = ANY(...)` 是 NULL。
//! 於是 users／user_role_assignments／roles／role_permissions／
//! identity_providers／tenants 的整條軌跡連租戶管理員都看不到。
//! 實測 34 列裡看得到 7 列。
//!
//! **這一組與 `audit_trail_slice.rs` 是一體兩面，兩邊都要過。**
//! 那邊釘的是 046 的意圖：場域受限的讀者**不該**看到租戶級列。
//! 這邊釘的是 053 補上的：租戶範圍的讀者**該**看到。
//! 實測過：拿掉 053 的分支，只有這邊失敗；把它放寬成「所有 NULL 都放行」，
//! 只有那邊失敗。
//!
//! **這支端點若在 053 之前上線，它會安靜地少回 79% 的列，而且少掉的正是
//! 最該看的那一批。** 那種缺陷不會有人回報 —— 沒有人知道自己少看到了什麼。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const USER_REQUESTER: &str = "ffffffff-0000-4000-8000-000000000004";
const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// **租戶級事件（`facility_id IS NULL`）必須讀得到。** 053 修的就是這個。
///
/// 用真的動作產生軌跡，不是自己插一列：這一格要驗的是「端到端會不會漏」，
/// 而自己插的列可以挑一個剛好看得到的 facility_id，那就驗不到東西了。
///
/// 建立一個使用者 → `USERS` 的稽核列（`facility_id` 必為 NULL，
/// 因為 `users` 沒有場域維度）→ 這支端點必須回得出來。
#[tokio::test]
async fn a_tenant_wide_audit_rows_are_visible() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/users",
                json!({ "username": "audit.probe", "display_name": "稽核探針" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let new_user = created["id"].as_str().unwrap().to_string();

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/audit-log?entity_type=USERS&entity_id={new_user}"
            )),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let rows = body["data"].as_array().cloned().unwrap_or_default();
    assert!(
        !rows.is_empty(),
        "租戶級稽核列讀不到 —— 053 的政策修正沒有生效，\
         身分與授權的軌跡對應用程式依然是空的：{body}"
    );
    assert_eq!(rows[0]["action"], "CREATE");
    assert_eq!(rows[0]["entity_type"], "USERS");
    assert_eq!(
        rows[0]["actor_name"], "陳系統",
        "只回 actor_user_id 等於沒有回答「誰做的」：{}",
        rows[0]
    );

    ctx.teardown().await;
}

/// 場域範圍仍然有效 —— `a_` 的反面。
///
/// 少了這一格，把政策改成 `USING (true)` 會讓 `a_` 通過，
/// 而那等於整個場域隔離消失。
///
/// `fm.lin` 的範圍只有台北總部，因此看不到信義影城的資產稽核列。
#[tokio::test]
async fn b_facility_scope_still_filters() {
    let ctx = &TestContext::setup().await;

    // 佈置：兩個場域各一列資產稽核。走 owner_tx（平台情境）——
    // 029 的 tenant_isolation WITH CHECK 需要它。
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.audit_log
           (tenant_id, occurred_at, actor_type, action, entity_type, facility_id)
         SELECT $1::uuid, clock_timestamp(), 'SYSTEM', 'PROBE', 'SLICE_PROBE', f.id
           FROM fms.facilities f WHERE f.tenant_id = $1::uuid",
    )
    .bind(TENANT_ID)
    .execute(&mut *tx)
    .await
    .expect("佈置兩列");
    tx.commit().await.expect("commit");

    let admin = ctx.login_as(USERNAME).await;
    let (_, all) = ctx
        .send(authed(
            get("/api/v1/audit-log?entity_type=SLICE_PROBE"),
            &admin,
        ))
        .await;
    let n_admin = all["data"].as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(n_admin, 2, "租戶管理員涵蓋兩個場域：{all}");

    // fm.lin 沒有 audit:read，所以無法用這支端點驗場域過濾。
    // 直接在 DB 層驗：這一格要證明的是**政策**，不是端點。
    //
    // 情境照 `begin_tenant_tx` 的方式注入（set_context + 一份具體的場域清單）
    // —— 少了第二步，`current_facility_ids()` 會是 NULL，
    // 而政策的 `current_facility_ids() IS NULL` 分支就無條件成立，
    // 這一格會變成永遠通過。
    let visible: i64 = {
        let mut tx = ctx.pool.begin().await.expect("begin");
        sqlx::query("SELECT fms.set_context($1::uuid, NULL, false)")
            .bind(TENANT_ID)
            .execute(&mut *tx)
            .await
            .expect("set_context");
        sqlx::query("SELECT set_config('app.facility_ids', $1, true)")
            .bind(FACILITY_HQ)
            .execute(&mut *tx)
            .await
            .expect("facility scope");
        sqlx::query_scalar("SELECT count(*) FROM fms.audit_log WHERE entity_type = 'SLICE_PROBE'")
            .fetch_one(&mut *tx)
            .await
            .expect("查稽核")
    };

    assert_eq!(
        visible, 1,
        "只涵蓋一個場域的情境應該只看得到一列 —— 053 若把政策放成 USING(true)，這裡會是 2"
    );

    ctx.teardown().await;
}

/// 權限：`audit:read` 且必須是 TENANT 範圍。
#[tokio::test]
async fn c_requires_tenant_scoped_audit_read() {
    let ctx = &TestContext::setup().await;

    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, body) = ctx.send(authed(get("/api/v1/audit-log"), &fm)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "FACILITY_ADMIN 沒有 audit:read：{body}"
    );

    let req = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = ctx.send(authed(get("/api/v1/audit-log"), &req)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    ctx.teardown().await;
}

/// 分頁走得完，而且**不重複也不跳號**。
///
/// 破平鍵是 `id`（bigint）—— `audit_log` 的 PK 是 `(occurred_at, id)`，
/// 而共用的 `Cursor` 原本只支援 uuid 破平鍵。少了破平鍵的話，
/// 同一微秒內寫入的兩列會在頁邊界被跳過，而那是**靜默的資料遺失**：
/// 沒有人知道自己少看到了什麼。這一格用一批同時寫入的列來逼出那個情況。
#[tokio::test]
async fn d_pagination_loses_no_rows_even_with_identical_timestamps() {
    let ctx = &TestContext::setup().await;

    // 25 列，occurred_at **完全相同** —— 破平鍵沒接好就會在這裡漏。
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.audit_log
           (tenant_id, occurred_at, actor_type, action, entity_type)
         SELECT $1::uuid, timestamptz '2026-07-15 10:00:00+00', 'SYSTEM',
                'PAGE_PROBE', 'PAGE_PROBE'
           FROM generate_series(1, 25)",
    )
    .bind(TENANT_ID)
    .execute(&mut *tx)
    .await
    .expect("佈置 25 列");
    tx.commit().await.expect("commit");

    let admin = ctx.login_as(USERNAME).await;
    let mut seen: Vec<i64> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let uri = match &cursor {
            Some(c) => format!("/api/v1/audit-log?entity_type=PAGE_PROBE&limit=10&cursor={c}"),
            None => "/api/v1/audit-log?entity_type=PAGE_PROBE&limit=10".to_string(),
        };
        let (status, body) = ctx.send(authed(get(&uri), &admin)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for r in body["data"].as_array().unwrap() {
            seen.push(r["id"].as_i64().unwrap());
        }
        cursor = body["page"]["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(seen.len(), 25, "分頁漏了列（拿到 {} 列）", seen.len());
    let mut uniq = seen.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), 25, "分頁重複回了同一列");

    ctx.teardown().await;
}

/// 過濾條件要真的過濾，而且 `from > to` 要擋下來而不是回空清單。
///
/// 回空清單是「查不到」與「你問錯了」共用同一個答案 —— 那會讓人以為
/// 那段時間真的沒有事件發生。
#[tokio::test]
async fn e_filters_apply_and_an_impossible_range_is_rejected() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    ctx.send(authed(
        json_request(
            "POST",
            &format!("/api/v1/users/{USER_REQUESTER}/role-assignments"),
            json!({ "role_code": "VIEWER", "scope_type": "FACILITY", "scope_id": FACILITY_HQ }),
        ),
        &admin,
    ))
    .await;

    let (status, body) = ctx
        .send(authed(
            get("/api/v1/audit-log?entity_type=USER_ROLE_ASSIGNMENTS"),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().cloned().unwrap_or_default();
    assert!(!rows.is_empty(), "角色指派的軌跡要查得到：{body}");
    assert!(
        rows.iter()
            .all(|r| r["entity_type"] == "USER_ROLE_ASSIGNMENTS"),
        "entity_type 過濾要真的過濾"
    );
    assert!(
        rows.iter()
            .any(|r| r["diff_keys"].is_array() || r["diff_keys"].is_null()),
        "diff_keys 要如實回傳"
    );
    assert!(
        rows[0].get("before_data").is_none() && rows[0].get("after_data").is_none(),
        "整列快照不回傳 —— 那裡有電話與員工編號，而「改了哪些欄位」已足以稽核：{}",
        rows[0]
    );

    let (status, body) = ctx
        .send(authed(
            get("/api/v1/audit-log?from=2026-08-01T00:00:00Z&to=2026-07-01T00:00:00Z"),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "from > to 要擋下來，回空清單會被讀成「那段時間沒有事件」：{body}"
    );

    ctx.teardown().await;
}

/// 釘住 `audit:read` 的 `min_scope_level`，因為 handler 依賴它。
///
/// handler 用 `require_tenant_scoped_permission`。突變測試顯示那與
/// 「在任一範圍持有」**今天等價** —— 026 已經保證 `audit:read` 只可能來自
/// TENANT 範圍的指派。也就是說那一行不是承重牆。
///
/// 但 `min_scope_level` 是**管理員改得動的資料**。若有人把它降成 FACILITY，
/// 期待的是場域管理員看得到稽核 —— 而 handler 那一行會讓那個期待
/// **靜默落空**：設定改了，行為沒變，沒有任何錯誤訊息。
///
/// 這一格讓那件事出聲。它不是在測目錄的內容，是在測**資料與程式碼之間的耦合
/// 沒有斷掉**。
#[tokio::test]
async fn f_the_catalogue_value_the_handler_depends_on_has_not_changed() {
    let ctx = &TestContext::setup().await;

    let mut tx = ctx.owner_tx().await;
    let level: String =
        sqlx::query_scalar("SELECT min_scope_level FROM fms.permissions WHERE code = 'audit:read'")
            .fetch_one(&mut *tx)
            .await
            .expect("讀權限目錄");
    tx.commit().await.expect("commit");

    assert_eq!(
        level, "TENANT",
        "audit:read 的 min_scope_level 被改成了 {level}。\n\
         若那是刻意的（要讓較窄範圍的角色讀稽核），\
         fms-identity/src/audit.rs 的 require_tenant_scoped_permission \
         也必須一起改成 require_permission(.., None, None) —— \
         否則那個設定不會有任何效果。"
    );

    ctx.teardown().await;
}
