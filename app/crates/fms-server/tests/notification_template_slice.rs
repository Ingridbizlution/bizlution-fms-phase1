//! 通知範本的維護（migration 042 + `/notification-templates`）。
//!
//! 041 讓範本有了讀取點，但只有 migration 能改它們 —— 而 041 自己就報出
//! 十條有 `notify` 卻沒有對應範本的轉移。那十份文案是內容工作。
//!
//! 兩個核心：
//!   * **覆寫要確定地勝出。** 041 的查詢沒有優先序，平台版與租戶版都會匹配，
//!     而唯一索引讓其中一個任意勝出 —— 管理者建了覆寫，系統有時候用它。
//!   * **缺哪些文案要事先看得見。** 041 把它記成 `warn`，但那要等事件發生。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";
const TECH_WANG: &str = "ffffffff-0000-4000-8000-000000000003";

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

async fn create_tpl(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request("POST", "/api/v1/notification-templates", body),
        token,
    ))
    .await
}

async fn list_tpl(ctx: &TestContext, token: &str, qs: &str) -> Value {
    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/notification-templates{qs}")),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// 走完「逾期 → 升級 → 扇出」，回傳收件人收到的主旨。
async fn breach_subjects(ctx: &TestContext, token: &str) -> Vec<String> {
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "範本測試",
                    "priority": "HIGH"
                }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    let id = wo["id"].as_str().expect("id").to_string();

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        let (status, resp) = ctx
            .send(authed(
                json_request(
                    "POST",
                    &format!("/api/v1/work-orders/{id}/transitions"),
                    action.clone(),
                ),
                token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{action} 失敗：{resp}");
    }

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_orders
                SET response_due_at   = clock_timestamp() - interval '2 hours',
                    resolution_due_at = clock_timestamp() - interval '2 hours'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("推 due");
        tx.commit().await.expect("commit");
    }

    let pool = ctx.owner_pool().await;
    fms_worker::sla_watchdog::SlaWatchdog::new(pool.clone())
        .run_once()
        .await
        .expect("sweep");
    let handler = fms_worker::notifier::NotificationHandler::new(pool.clone())
        .await
        .expect("handler");
    let types = handler.event_types.clone();
    fms_worker::run_once(
        &pool,
        &handler,
        &fms_worker::RelayConfig {
            event_types: Some(types),
            ..Default::default()
        },
    )
    .await
    .expect("relay");
    pool.close().await;

    let mut tx = ctx.owner_tx().await;
    sqlx::query_scalar(
        "SELECT subject FROM fms.notifications
          WHERE entity_id = $1::uuid AND template_code = 'WO_SLA_BREACH'
            AND channel = 'IN_APP'",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await
    .expect("讀通知")
}

// =============================================================================
// 覆寫要確定地勝出
// =============================================================================

/// **本檔最重要的測試。**
///
/// 租戶建一個同 `(code, channel, locale)` 的範本 → 之後的通知用的是它。
///
/// 041 的查詢沒有優先序：平台版與租戶版都匹配，`CROSS JOIN` 產出兩列，
/// 而 `uq_notifications_event_recipient` 讓其中一個以 `ON CONFLICT DO NOTHING`
/// 被丟掉 —— **哪一個勝出不確定**。042 加上 `DISTINCT ON (channel)` 與
/// 「租戶版優先」的排序。
#[tokio::test]
async fn a_tenant_override_wins_deterministically() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 平台版的主旨是「【SLA 逾期】{{wo_no}} 已超過要求完成時間」。
    let before = breach_subjects(ctx, &token).await;
    assert_eq!(before.len(), 1, "應恰好一封：{before:?}");
    assert!(
        before[0].starts_with("【SLA 逾期】"),
        "前提：用的是平台範本：{before:?}"
    );

    // 租戶覆寫同一個 (code, channel, locale)。
    let (status, override_tpl) = create_tpl(
        ctx,
        &token,
        json!({
            "code": "WO_SLA_BREACH",
            "channel": "IN_APP",
            "locale": "zh-TW",
            "subject_template": "★逾期★ {{wo_no}}",
            "body_template": "{{wo_no}}（{{title}}）逾期了，負責人 {{assignee_name}}。"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{override_tpl}");
    assert_eq!(override_tpl["is_platform"], false, "{override_tpl}");

    let after = breach_subjects(ctx, &token).await;
    assert_eq!(after.len(), 1, "仍然只該有一封（不是兩封）：{after:?}");
    assert!(
        after[0].starts_with("★逾期★"),
        "應用租戶的覆寫版本：{after:?}"
    );

    ctx.teardown().await;
}

/// 刪掉覆寫 → 平台版重新生效。
#[tokio::test]
async fn deleting_an_override_restores_the_platform_template() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, tpl) = create_tpl(
        ctx,
        &token,
        json!({
            "code": "WO_SLA_BREACH",
            "channel": "IN_APP",
            "subject_template": "★覆寫★ {{wo_no}}",
            "body_template": "內容"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{tpl}");
    let id = tpl["id"].as_str().expect("id");

    assert!(breach_subjects(ctx, &token).await[0].starts_with("★覆寫★"));

    let (status, _) = ctx
        .send(authed(
            del(&format!("/api/v1/notification-templates/{id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(
        breach_subjects(ctx, &token).await[0].starts_with("【SLA 逾期】"),
        "刪掉覆寫之後平台版該重新生效"
    );

    ctx.teardown().await;
}

/// 平台範本上會標 `is_overridden`。
///
/// 少了這個欄位，UI 沒辦法解釋「那一列還在清單裡、我也讀得到，
/// 但我改不了而且它也沒有生效」。
#[tokio::test]
async fn a_platform_template_is_marked_as_overridden() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let before = list_tpl(ctx, &token, "?code=WO_SLA_BREACH").await;
    for row in before["data"].as_array().expect("data") {
        assert_eq!(row["is_platform"], true, "種子全是平台範本：{row}");
        assert_eq!(row["is_overridden"], false, "{row}");
    }

    let (status, _) = create_tpl(
        ctx,
        &token,
        json!({
            "code": "WO_SLA_BREACH",
            "channel": "IN_APP",
            "subject_template": "★",
            "body_template": "內容"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let after = list_tpl(ctx, &token, "?code=WO_SLA_BREACH").await;
    let rows = after["data"].as_array().expect("data");
    let platform_in_app = rows
        .iter()
        .find(|r| r["is_platform"] == true && r["channel"] == "IN_APP")
        .unwrap_or_else(|| panic!("平台的 IN_APP 版本仍該在清單裡：{after}"));
    assert_eq!(
        platform_in_app["is_overridden"], true,
        "被覆寫了就要說出來：{platform_in_app}"
    );

    // 而 EMAIL 版沒有被覆寫。
    let platform_email = rows
        .iter()
        .find(|r| r["is_platform"] == true && r["channel"] == "EMAIL")
        .expect("平台的 EMAIL 版");
    assert_eq!(platform_email["is_overridden"], false, "{platform_email}");

    ctx.teardown().await;
}

// =============================================================================
// 平台範本改不了
// =============================================================================

/// 對平台範本送 PATCH／DELETE 回 **409 而不是 404**。
///
/// 那一筆確實存在也讀得到，只是改不了 —— 回 404 會讓人以為 id 打錯。
/// 訊息要說出正確做法（建立覆寫版本）。
#[tokio::test]
async fn a_platform_template_cannot_be_modified_but_says_why() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let list = list_tpl(ctx, &token, "?code=WO_SLA_BREACH&channel=EMAIL").await;
    let id = list["data"][0]["id"].as_str().expect("id");

    let (status, body) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/notification-templates/{id}"),
                json!({ "body_template": "改一下" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "不是 404：{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("覆寫"), "訊息要說出正確做法：{body}");

    let (status, body) = ctx
        .send(authed(
            del(&format!("/api/v1/notification-templates/{id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // 平台範本沒有被動到。
    let after = list_tpl(ctx, &token, "?code=WO_SLA_BREACH&channel=EMAIL").await;
    assert!(
        after["data"][0]["body_template"]
            .as_str()
            .unwrap_or_default()
            .contains("逾期"),
        "{after}"
    );

    ctx.teardown().await;
}

/// 同一個 `(code, channel, locale)` 只能有一個租戶版本。
#[tokio::test]
async fn a_duplicate_override_is_rejected() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let body = json!({
        "code": "WO_SLA_BREACH",
        "channel": "IN_APP",
        "subject_template": "★",
        "body_template": "內容"
    });
    let (status, _) = create_tpl(ctx, &token, body.clone()).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, dup) = create_tpl(ctx, &token, body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{dup}");
    assert!(
        dup["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("已經有"),
        "訊息要指向既有那一筆：{dup}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 缺哪些文案要事先看得見
// =============================================================================

/// `meta.transitions_without_template` 列出宣告了要通知卻沒有文案的轉移。
///
/// 041 把這件事計入 `no_template` 並記 `warn`，但那要等事件真的發生 ——
/// 而「等到有人逾期了才發現沒有通知文案」正是這個欄位要避免的。
///
/// **這個測試原本斷言那個清單是非空的**（041 當時有十條缺文案，其中
/// `WAIT_PARTS` 被拿來當代表）。047 把文案補齊之後那個前提消失了。
///
/// 改成驗**機制**而不是驗當時的缺口，分兩段：
///   1. 現在該是空的 —— 這本身就是 047 的驗收條件；
///   2. 刪掉一份文案，那一條就必須立刻出現在清單裡。
///
/// 第 2 段是關鍵。只留第 1 段的話，把這個欄位改成永遠回 `[]` 也會通過 ——
/// 而那正是這個欄位存在要防的那種靜默。
#[tokio::test]
async fn the_list_reports_which_transitions_have_no_template() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let body = list_tpl(ctx, &token, "").await;
    let actions: Vec<&str> = body["meta"]["transitions_without_template"]
        .as_array()
        .expect("transitions_without_template")
        .iter()
        .filter_map(|m| m["action"].as_str())
        .collect();
    assert!(
        actions.is_empty(),
        "047 之後不該還有轉移缺文案：{actions:?}"
    );

    // 刪掉 WAIT_PARTS 用的那份 → 它必須立刻被報出來。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "DELETE FROM fms.notification_templates
              WHERE tenant_id IS NULL AND code = 'WO_WAITING_PARTS'",
        )
        .execute(&mut *tx)
        .await
        .expect("刪掉一份文案");
        tx.commit().await.expect("commit");
    }

    let body = list_tpl(ctx, &token, "").await;
    let actions: Vec<&str> = body["meta"]["transitions_without_template"]
        .as_array()
        .expect("transitions_without_template")
        .iter()
        .filter_map(|m| m["action"].as_str())
        .collect();
    assert!(
        actions.contains(&"WAIT_PARTS"),
        "文案被刪掉之後 WAIT_PARTS 必須出現在缺漏清單：{actions:?}"
    );
    assert!(
        !actions.contains(&"BREACH_SLA"),
        "BREACH_SLA 的 WO_SLA_BREACH 還在，不該被牽連：{actions:?}"
    );

    ctx.teardown().await;
}

/// 每一列回報它用到的 `{{變數}}`。
///
/// 打錯一個變數的後果是收件人看到 `{{assignee}}` 那串字 ——
/// `render_template` 刻意原樣留下（041 檔頭說明了理由）。因此這個欄位
/// 是客戶端唯一能事先發現打錯字的方式。
#[tokio::test]
async fn each_template_reports_its_placeholders() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, created) = create_tpl(
        ctx,
        &token,
        json!({
            "code": "WO_TEST_VARS",
            "channel": "IN_APP",
            "subject_template": "{{wo_no}} 與 {{wo_no}}",
            "body_template": "{{title}} 由 {{assignee}} 處理"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let placeholders: Vec<&str> = created["placeholders"]
        .as_array()
        .expect("placeholders")
        .iter()
        .filter_map(|p| p.as_str())
        .collect();
    assert_eq!(
        placeholders,
        vec!["assignee", "title", "wo_no"],
        "應去重、排序，且涵蓋主旨與內容：{created}"
    );
    // `assignee` 是打錯的（正確的是 `assignee_name`）。伺服端不擋 ——
    // 不同事件家族的可用變數不同 —— 但它出現在清單裡，客戶端比對得出來。
    assert!(
        placeholders.contains(&"assignee"),
        "打錯的變數也要列出來，那才是這個欄位的用途"
    );

    ctx.teardown().await;
}

// =============================================================================
// 權限與驗證
// =============================================================================

/// 寫入需要 **TENANT 範圍** —— 一句措辭套用到整個租戶的每一封通知。
///
/// 把關的是**宣告**：042 讓 `notification_template:write` 的
/// `min_scope_level` 是 TENANT，而 026 的收斂讓 FACILITY 範圍的授權展不開它。
/// 因此這個測試要涵蓋兩種人：
///   * 完全沒有這個權限的（FACILITY_ADMIN）
///   * **有 TENANT_ADMIN 但被指派在單一場域的** —— 他「持有」這個權限，
///     只是範圍不夠。少了這一半，測試分不出「沒有權限」與「範圍不足」，
///     而突變（把 tenant-scoped 檢查改成 anywhere 檢查）會全數通過。
#[tokio::test]
async fn writing_a_template_needs_tenant_scope() {
    let ctx = &TestContext::setup().await;

    // 場域範圍的 TENANT_ADMIN。026 之前這會讓他取得全部租戶級權限。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.user_role_assignments
                 (tenant_id, user_id, role_id, scope_type, scope_id, source)
             SELECT u.tenant_id, u.id, r.id, 'FACILITY', $2::uuid, 'MANUAL'
               FROM fms.users u, fms.roles r
              WHERE u.username::text = $1 AND r.code = 'TENANT_ADMIN'",
        )
        .bind(USERNAME_FACILITY_ADMIN)
        .bind(FACILITY_HQ)
        .execute(&mut *tx)
        .await
        .expect("以場域範圍指派 TENANT_ADMIN");
        tx.commit().await.expect("commit");
    }

    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    // 讀得到（`notification_template:read` 宣告 FACILITY）。
    let body = list_tpl(ctx, &fm, "").await;
    assert!(!body["data"].as_array().expect("data").is_empty(), "{body}");

    // 但寫不了。
    let (status, denied) = create_tpl(
        ctx,
        &fm,
        json!({
            "code": "WO_SLA_BREACH",
            "channel": "IN_APP",
            "body_template": "場域範圍的授權不該能改全租戶的措辭"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "他持有 TENANT_ADMIN，但範圍只有一個場域：{denied}"
    );
    assert_eq!(denied["code"], "PERMISSION_DENIED");

    // 而 /auth/me 也不該宣告他持有這個權限 —— 那是 026 的收斂在起作用，
    // 也是這個測試真正釘住的東西。
    let (status, me) = ctx.send(authed(get("/api/v1/auth/me"), &fm)).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    let held: Vec<String> = me["permissions"]
        .as_array()
        .expect("permissions")
        .iter()
        .map(|p| p.as_str().unwrap().split('@').next().unwrap().to_string())
        .collect();
    assert!(
        !held.contains(&"notification_template:write".to_string()),
        "宣告 TENANT 的權限不該在場域範圍展開：{held:?}"
    );

    // 反面：租戶管理員（TENANT 範圍）可以。
    let admin = ctx.login().await;
    let (status, ok) = create_tpl(
        ctx,
        &admin,
        json!({
            "code": "WO_SLA_BREACH",
            "channel": "IN_APP",
            "body_template": "租戶管理員可以"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ok}");

    ctx.teardown().await;
}

/// 空內容與未知頻道回 422。
#[tokio::test]
async fn invalid_values_are_rejected_with_pointers() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, body) = create_tpl(
        ctx,
        &token,
        json!({ "code": "X", "channel": "TELEPATHY", "body_template": "內容" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["pointer"], "/channel", "{body}");

    let (status, body) = create_tpl(
        ctx,
        &token,
        json!({ "code": "X", "channel": "IN_APP", "body_template": "   " }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["pointer"], "/body_template", "{body}");

    ctx.teardown().await;
}

/// 停用覆寫 → 平台版重新生效（不必刪除）。
///
/// 這是 `is_active` 的用途：暫時退回平台版而保留自己的文案。
#[tokio::test]
async fn deactivating_an_override_falls_back_to_the_platform_version() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, tpl) = create_tpl(
        ctx,
        &token,
        json!({
            "code": "WO_SLA_BREACH",
            "channel": "IN_APP",
            "subject_template": "★覆寫★ {{wo_no}}",
            "body_template": "內容"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{tpl}");
    let id = tpl["id"].as_str().expect("id");
    assert!(breach_subjects(ctx, &token).await[0].starts_with("★覆寫★"));

    let (status, patched) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/notification-templates/{id}"),
                json!({ "is_active": false }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["is_active"], false);

    assert!(
        breach_subjects(ctx, &token).await[0].starts_with("【SLA 逾期】"),
        "停用的覆寫不該生效"
    );

    // 停用的覆寫預設不出現在清單裡，但查得到。
    let default_list = list_tpl(ctx, &token, "?code=WO_SLA_BREACH").await;
    assert!(
        default_list["data"]
            .as_array()
            .expect("data")
            .iter()
            .all(|r| r["is_platform"] == true),
        "{default_list}"
    );
    let all = list_tpl(ctx, &token, "?code=WO_SLA_BREACH&include_inactive=true").await;
    assert!(
        all["data"]
            .as_array()
            .expect("data")
            .iter()
            .any(|r| r["is_platform"] == false),
        "{all}"
    );

    ctx.teardown().await;
}
