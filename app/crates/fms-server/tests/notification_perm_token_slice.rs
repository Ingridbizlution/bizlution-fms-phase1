//! `PERM:<權限碼>` 收件人代號與新補的六份文案（migration 047）。
//!
//! 041 把 `APPROVER` 歸進「解析不到任何人」那一類並計數。它該指的是
//! **能核准這張工單的人**，而那件事目錄裡早就有權威定義：`APPROVE` 這個動作的
//! `required_permission` 就是 `work_order:approve`。
//!
//! 047 因此加了 `PERM:<權限碼>` 這種代號形式。本檔驗三件事：
//!
//!   * **解析對象正確** —— 持有該權限的人，跨範圍層級（TENANT 與 FACILITY）
//!     都要拿到，而沒有那個權限的人不能拿到。
//!   * **不跨租戶** —— 扇出跑在平台情境下，RLS 不會幫忙過濾；擋住它的必須是
//!     查詢裡明寫的 `tenant_id` 條件。
//!   * **文案真的能算出字來** —— 每一份新文案渲染後都不該留著 `{{變數}}`。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};
use uuid::Uuid;

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

async fn create_wo(ctx: &TestContext, token: &str) -> String {
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "PERM 代號測試",
                    "priority": "HIGH"
                }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    wo["id"].as_str().expect("id").to_string()
}

async fn transition(ctx: &TestContext, token: &str, id: &str, body: Value) {
    let (status, out) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                body,
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
}

/// 解析 `notify` 清單，回傳解析到的 username（排序後）。
async fn resolve(ctx: &TestContext, wo_id: &str, notify: &str) -> Vec<String> {
    let mut tx = ctx.owner_tx().await;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT u.username
           FROM fms.notification_recipients($1::uuid, $2::jsonb) r
           JOIN fms.users u ON u.id = r.user_id
          ORDER BY u.username",
    )
    .bind(wo_id)
    .bind(notify)
    .fetch_all(&mut *tx)
    .await
    .expect("解析收件人");
    tx.commit().await.expect("commit");
    rows.into_iter().map(|(u,)| u).collect()
}

// =============================================================================
// 解析對象
// =============================================================================

/// `PERM:work_order:approve` 解析成持有該權限的人，**不分範圍層級**。
///
/// 種子裡有兩個：`admin.chen`（TENANT_ADMIN，TENANT 範圍）與
/// `fm.lin`（FACILITY_ADMIN，FACILITY 範圍）。兩種都要拿到 ——
/// 只拿到其中一種，代表範圍展開只處理了一半。
#[tokio::test]
async fn a_perm_token_resolves_to_every_holder_of_that_permission() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token).await;

    let resolved = resolve(ctx, &id, r#"["PERM:work_order:approve"]"#).await;

    assert!(
        resolved.contains(&"admin.chen".to_string()),
        "TENANT 範圍的核准者要拿到：{resolved:?}"
    );
    assert!(
        resolved.contains(&"fm.lin".to_string()),
        "FACILITY 範圍的核准者要拿到：{resolved:?}"
    );

    // 反面：沒有這個權限的人不能出現。少了這一格，
    // 「解析成全租戶所有人」也會讓上面兩個斷言通過。
    assert!(
        !resolved.contains(&"tech.wang".to_string()),
        "技師沒有 work_order:approve，不該收到核准請求：{resolved:?}"
    );
    assert!(
        !resolved.contains(&"emp.wang".to_string()),
        "一般員工不該收到核准請求：{resolved:?}"
    );

    ctx.teardown().await;
}

/// 打錯的權限碼 → 解析不到人，而且**看得見**。
///
/// 這是 047 要消掉的那個失效模式的反面：`APPROVER` 之前的行為是安靜地
/// 誰都不通知。換成 `PERM:` 之後如果打錯字，症狀不能又變回安靜。
#[tokio::test]
async fn a_perm_token_with_an_unknown_permission_resolves_to_nobody_visibly() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token).await;

    let resolved = resolve(ctx, &id, r#"["PERM:work_order:aprove"]"#).await;
    assert!(
        resolved.is_empty(),
        "打錯的權限碼不該解析到人：{resolved:?}"
    );

    // 但它必須回一列 user_id 為 NULL（`unresolved` 才數得到），
    // 而不是回空集合。
    let mut tx = ctx.owner_tx().await;
    let nulls: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.notification_recipients($1::uuid,
                  '[\"PERM:work_order:aprove\"]'::jsonb) r
          WHERE r.user_id IS NULL",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("解析");
    tx.commit().await.expect("commit");

    assert_eq!(
        nulls, 1,
        "打錯的 PERM: 代號要計入 unresolved —— 不能安靜地誰都不通知"
    );

    ctx.teardown().await;
}

/// **`PERM:` 不能跨租戶。**
///
/// 扇出是以 `begin_platform_tx` 呼叫的（要跨租戶處理 outbox），因此
/// `v_user_effective_permissions` **不受 RLS 過濾**。
///
/// 這個測試綁的是**結果**，不管是哪一層擋下來的 —— 而那是刻意的。
/// 實測擋下來的是場域包含條件（他租戶使用者的可存取場域永遠不含這張工單的
/// 場域），不是 047 加上的 `ep.tenant_id = wo.tenant_id`：把那行拿掉，
/// 這個測試仍然通過。那行條件因此是縱深防禦，由 migration 的結構斷言 (6)
/// 守著，不是由這裡守著。
///
/// 寫成綁結果而不是綁機制，是因為「哪一層在擋」會變，「不能跨租戶」不會。
#[tokio::test]
async fn a_perm_token_never_crosses_tenants() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token).await;

    let mut tx = ctx.owner_tx().await;
    // 另一個租戶裡也造一個持有 work_order:approve 的人。
    //
    // template 資料庫只有一個租戶（009 只種示範租戶），所以第二個租戶要自己
    // 造。**不能改成「找一個既有的」** —— 找不到時 `fetch_optional` 會讓
    // 整個測試安靜地什麼都沒驗到，而這是一個跨租戶隔離的測試。
    let other_tenant: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.tenants (code, name, status)
         VALUES ('perm-token-other', 'PERM 代號測試用他租戶', 'ACTIVE')
         RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("建第二個租戶");

    let intruder: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.users (tenant_id, username, display_name, email, status)
         VALUES ($1, 'other.approver', 'other.approver',
                 'other.approver@example.test', 'ACTIVE')
         RETURNING id",
    )
    .bind(other_tenant)
    .fetch_one(&mut *tx)
    .await
    .expect("建他租戶的核准者");

    sqlx::query(
        "INSERT INTO fms.user_role_assignments
           (tenant_id, user_id, role_id, scope_type, scope_id, source)
         SELECT $1, $2, r.id, 'TENANT', NULL, 'MANUAL'
           FROM fms.roles r WHERE r.code = 'TENANT_ADMIN'",
    )
    .bind(other_tenant)
    .bind(intruder)
    .execute(&mut *tx)
    .await
    .expect("指派角色");
    tx.commit().await.expect("commit");

    let resolved = resolve(ctx, &id, r#"["PERM:work_order:approve"]"#).await;

    assert!(
        !resolved.contains(&"other.approver".to_string()),
        "他租戶的核准者絕不能收到本租戶的工單：{resolved:?}"
    );
    // 反面：本租戶的核准者還是拿得到（避免「政策收成永遠 false」也通過）。
    assert!(
        resolved.contains(&"admin.chen".to_string()),
        "本租戶的核准者仍該拿到：{resolved:?}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 文案
// =============================================================================

/// `REQUEST_APPROVAL` 走完整條鏈：轉移 → 事件 → 扇出 → 核准者收到信。
///
/// 047 之前這條鏈的結尾是 `no_template + 1` 與 `unresolved + 1`
/// —— 兩個計數器同時亮，沒有任何人收到任何東西。
#[tokio::test]
async fn requesting_approval_reaches_the_approvers() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token).await;

    transition(ctx, &token, &id, json!({ "action": "REQUEST_APPROVAL" })).await;

    let mut tx = ctx.owner_tx().await;
    let event_id: i64 = sqlx::query_scalar(
        "SELECT id FROM fms.event_outbox
          WHERE event_type = 'work_order.approval_requested' AND aggregate_id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("應有 approval_requested 事件");

    let (created, no_template, unresolved): (i32, i32, i32) =
        sqlx::query_as("SELECT * FROM fms.fan_out_notifications($1)")
            .bind(event_id)
            .fetch_one(&mut *tx)
            .await
            .expect("扇出");

    assert_eq!(no_template, 0, "047 補了 WO_APPROVAL_REQUESTED");
    assert_eq!(unresolved, 0, "PERM:work_order:approve 解析得到人");
    assert!(
        created >= 2,
        "TENANT 與 FACILITY 兩個核准者都該收到：{created}"
    );

    // 主旨與內文都算完了，沒有留下未代換的變數。
    let leftover: Vec<(String,)> = sqlx::query_as(
        "SELECT coalesce(subject, '') || ' / ' || body FROM fms.notifications
          WHERE body LIKE '%{{%' OR coalesce(subject, '') LIKE '%{{%'",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("查未代換的變數");
    tx.commit().await.expect("commit");

    assert!(
        leftover.is_empty(),
        "通知裡留著沒代換的變數：{:?}",
        leftover.iter().map(|(s,)| s).collect::<Vec<_>>()
    );

    ctx.teardown().await;
}

/// 駁回通知要把**操作者填的原因**送到報修人手上。
///
/// `WO_REJECTED` 的 `{{reason}}` 刻意沒有兜底，依據是目錄裡 REJECT 的
/// `required_fields` 是 `{reason}`。這個測試同時驗兩件事：那個保證真的被
/// 執行（少填會被擋下），以及填了的字真的會出現在通知裡。
///
/// 這裡原本還有一個「沒填原因 → 顯示兜底字串」的案例，而它失敗了 ——
/// 因為 API 直接回 422。那個失敗是有用的：它證明我當初判斷「REJECT 不強制
/// 填原因」是看錯了地方（看的是 `p_reason DEFAULT NULL` 這個**函式簽章
/// 的預設值**，而強制與否是由目錄的 `required_fields` 決定的）。
#[tokio::test]
async fn a_rejection_carries_the_reason_the_operator_typed() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // (1) 前提：少了 reason 會被擋下 —— 這就是 {{reason}} 不必兜底的依據。
    let missing_reason = create_wo(ctx, &token).await;
    let (status, problem) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{missing_reason}/transitions"),
                json!({ "action": "REJECT" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "REJECT 少了 reason 必須被擋下：{problem}"
    );

    // (2) 填了的原因要出現在報修人收到的字裡。
    let rejected = create_wo(ctx, &token).await;
    transition(
        ctx,
        &token,
        &rejected,
        json!({ "action": "REJECT", "reason": "重複報修，已併入 WO-0001" }),
    )
    .await;

    let mut tx = ctx.owner_tx().await;
    let body: String = sqlx::query_scalar(
        "SELECT fms.render_template(t.body_template, fms.notification_vars($1::uuid))
           FROM fms.notification_templates t
          WHERE t.tenant_id IS NULL AND t.code = 'WO_REJECTED' AND t.channel = 'EMAIL'",
    )
    .bind(&rejected)
    .fetch_one(&mut *tx)
    .await
    .expect("渲染駁回文案");
    tx.commit().await.expect("commit");

    assert!(
        body.contains("重複報修，已併入 WO-0001"),
        "駁回內文要有操作者填的原因：{body}"
    );
    assert!(!body.contains("{{"), "報修人不該看到未代換的變數：{body}");

    ctx.teardown().await;
}

/// `WO_SCHEDULED` 的**主旨**帶了 `{{scheduled_start_at}}` 而沒有兜底。
///
/// 那是刻意的：目錄裡 SCHEDULE 的 `required_fields` 保證那個欄位有值。
/// 這個測試驗那個保證在實際走過 API 之後成立 —— 047 的自我驗證只看目錄
/// 那一列還在，這裡看渲染結果。
#[tokio::test]
async fn a_scheduled_notification_has_a_real_time_in_its_subject() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token).await;

    transition(
        ctx,
        &token,
        &id,
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
    )
    .await;
    transition(
        ctx,
        &token,
        &id,
        json!({ "action": "SCHEDULE", "scheduled_start_at": "2026-09-01T09:00:00Z" }),
    )
    .await;

    let mut tx = ctx.owner_tx().await;
    let (subject, body): (String, String) = sqlx::query_as(
        "SELECT fms.render_template(t.subject_template, fms.notification_vars($1::uuid)),
                fms.render_template(t.body_template,    fms.notification_vars($1::uuid))
           FROM fms.notification_templates t
          WHERE t.tenant_id IS NULL AND t.code = 'WO_SCHEDULED' AND t.channel = 'EMAIL'",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("渲染排程文案");
    tx.commit().await.expect("commit");

    assert!(
        !subject.contains("{{"),
        "主旨裡的大括號是最顯眼的一種壞：{subject}"
    );
    assert!(
        subject.contains("2026-09-01"),
        "主旨要有真的日期：{subject}"
    );
    // 結束時間是選填的，所以它走兜底而不是留 placeholder。
    assert!(
        body.contains("未指定"),
        "沒填結束時間時該說「未指定」，而不是留 {{{{scheduled_end_at}}}}：{body}"
    );
    assert!(!body.contains("{{"), "內文也不該有未代換的變數：{body}");

    ctx.teardown().await;
}

/// 目錄不變量：**沒有任何一條有 `notify` 的規則缺文案。**
///
/// 047 的自我驗證已經有這一格，但那只在 migration 跑的那一刻成立。
/// 這裡讓它變成每次測試都檢查 —— 之後有人在目錄加一條 `notify` 規則卻忘了
/// 寫文案時，會在這裡亮，而不是在正式環境靜默地不通知任何人。
#[tokio::test]
async fn every_notify_rule_has_a_template() {
    let ctx = &TestContext::setup().await;

    let mut tx = ctx.owner_tx().await;
    let missing: Vec<(String, String)> = sqlx::query_as(
        "SELECT w.action, coalesce(w.side_effects ->> 'template', '(沒有 template 鍵)')
           FROM fms.work_order_transitions_allowed w
          WHERE w.is_active
            AND w.side_effects ? 'notify'
            AND NOT EXISTS (
                  SELECT 1 FROM fms.notification_templates t
                   WHERE t.tenant_id IS NULL
                     AND lower(t.code) = lower(w.side_effects ->> 'template')
                     AND t.is_active)
          ORDER BY w.action",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("查缺文案的規則");
    tx.commit().await.expect("commit");

    assert!(
        missing.is_empty(),
        "這些規則宣告了要通知卻沒有字可送：{missing:?}"
    );

    ctx.teardown().await;
}
