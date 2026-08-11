//! 通知扇出與收件匣（migration 041 + `fms_worker::notifier` + `/notifications`）。
//!
//! 035 的檔頭當時寫得很明白：升級會改狀態、留稽核、發事件，
//! **但沒有任何程式碼寫 `fms.notifications`**，所以沒有人會被通知。
//!
//! 第一個測試走完整條鏈：工單逾期 → 掃描升級 → 事件 → relay 扇出 →
//! 收件人在 `GET /notifications` 看到它。那是這一輪存在的全部理由。
//!
//! 其餘測試分三類：
//!   * **不會有人收到的情況要被計數**（沒有範本、代號解析不到人）
//!   * **收件匣是授權邊界** —— `notifications` 的 RLS 只隔離租戶
//!   * **扇出必須幂等** —— relay 是 at-least-once

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};
use uuid::Uuid;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const FACILITY_CINEMA: &str = "cccccccc-0000-4000-8000-000000000002";
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

async fn create_wo(ctx: &TestContext, token: &str, priority: &str) -> String {
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "通知測試",
                    "priority": priority
                }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    wo["id"].as_str().expect("id").to_string()
}

async fn transition(ctx: &TestContext, token: &str, id: &str, body: Value) {
    let (status, resp) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                body.clone(),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body} 失敗：{resp}");
}

async fn age_due(ctx: &TestContext, id: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.work_orders
            SET response_due_at   = clock_timestamp() - interval '2 hours',
                resolution_due_at = clock_timestamp() - interval '2 hours'
          WHERE id = $1::uuid",
    )
    .bind(id)
    .execute(&mut *tx)
    .await
    .expect("推 due");
    tx.commit().await.expect("commit");
}

/// 建一個帶指定角色與場域範圍的使用者，回傳 (id, username)。
///
/// 種子裡每個角色**只有一個人**（一個 FACILITY_ADMIN、一個 REQUESTER），
/// 而那讓「多通知了人」測不出來 —— 單一列的斷言分不出「只選中他」與
/// 「選中了所有人而剛好只有他」。突變測試就是這樣讓兩個突變全數通過的。
async fn add_user(ctx: &TestContext, username: &str, role: &str, facility: Option<&str>) -> Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.users (tenant_id, username, display_name, email, status)
         VALUES ($1::uuid, $2, $2, $2 || '@example.test', 'ACTIVE')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(username)
    .fetch_one(&mut *tx)
    .await
    .expect("建使用者");

    sqlx::query(
        "INSERT INTO fms.user_role_assignments
           (tenant_id, user_id, role_id, scope_type, scope_id, source)
         SELECT $1::uuid, $2, r.id,
                CASE WHEN $4::uuid IS NULL THEN 'TENANT' ELSE 'FACILITY' END,
                $4::uuid, 'MANUAL'
           FROM fms.roles r WHERE r.code = $3",
    )
    .bind(TENANT_ID)
    .bind(id)
    .bind(role)
    .bind(facility)
    .execute(&mut *tx)
    .await
    .expect("指派角色");
    tx.commit().await.expect("commit");
    id
}

/// 執行掃描（會觸發 BREACH_SLA 並發事件）。
async fn sweep(ctx: &TestContext) {
    let pool = ctx.owner_pool().await;
    let watchdog = fms_worker::sla_watchdog::SlaWatchdog::new(pool.clone());
    let out = watchdog.run_once().await.expect("sweep");
    assert_eq!(out.escalated, 1, "前提：應升級一張：{out:?}");
    pool.close().await;
}

/// 執行一輪通知 relay，回傳處理筆數。
async fn relay_once(ctx: &TestContext) -> usize {
    let pool = ctx.owner_pool().await;
    let handler = fms_worker::notifier::NotificationHandler::new(pool.clone())
        .await
        .expect("handler");
    let types = handler.event_types.clone();
    let batch = fms_worker::run_once(
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
    batch.published
}

// =============================================================================
// 整條鏈
// =============================================================================

/// **本檔最重要的測試。**
///
/// 逾期 → 升級 → 事件 → 扇出 → 收件人真的看到它。
///
/// 035 之前這條鏈的最後一段是斷的：事件躺在 outbox 裡被標成 `SKIPPED`，
/// 而 `notify: [FACILITY_ADMIN, MAINTENANCE_SUPERVISOR]` 是個沒有人讀的宣告。
#[tokio::test]
async fn an_sla_breach_reaches_the_recipients_inbox() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id).await;
    sweep(ctx).await;

    // ASSIGN 也宣告了 notify（通知負責人），因此這一輪會處理兩筆事件。
    // 斷言下界而不是精確值：這個測試要驗的是逾期通知有沒有到，
    // 而綁死筆數會讓「目錄多加一條 notify」變成這個測試失敗。
    assert!(
        relay_once(ctx).await >= 1,
        "應處理 sla_breached（與 assigned）事件"
    );

    // fm.lin 是 FACILITY_ADMIN，總部範圍 —— 正是 notify 清單指定的角色。
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, inbox) = ctx.send(authed(get("/api/v1/notifications"), &fm)).await;
    assert_eq!(status, StatusCode::OK, "{inbox}");

    let rows = inbox["data"].as_array().expect("data");
    let breach = rows
        .iter()
        .find(|n| n["template_code"] == "WO_SLA_BREACH")
        .unwrap_or_else(|| panic!("場域管理員應收到逾期通知：{inbox}"));

    assert_eq!(breach["priority"], "HIGH", "逾期是高優先：{breach}");
    assert_eq!(breach["entity_id"], id, "應連回那張工單：{breach}");
    assert!(breach["read_at"].is_null(), "新通知未讀：{breach}");
    assert_eq!(inbox["meta"]["unread_count"], 1, "{inbox}");

    // 範本真的被渲染了 —— 沒有殘留的 placeholder。
    let subject = breach["subject"].as_str().expect("subject");
    assert!(
        subject.contains("WO-") && !subject.contains("{{"),
        "主旨應填入工單號且沒有殘留 placeholder：{subject}"
    );
    let body = breach["body"].as_str().expect("body");
    assert!(
        !body.contains("{{"),
        "內容不該有殘留 placeholder（範本要的變數全部有值）：{body}"
    );

    // 標記已讀是幂等的。
    let nid = breach["id"].as_str().expect("id");
    for _ in 0..2 {
        let (status, _) = ctx
            .send(authed(
                json_request(
                    "POST",
                    &format!("/api/v1/notifications/{nid}/read"),
                    json!({}),
                ),
                &fm,
            ))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let (_, after) = ctx.send(authed(get("/api/v1/notifications"), &fm)).await;
    assert_eq!(after["meta"]["unread_count"], 0, "{after}");

    ctx.teardown().await;
}

/// 扇出是幂等的 —— relay 是 at-least-once。
///
/// 重跑同一筆事件不該讓收件人再收到一次。041 用
/// `uq_notifications_event_recipient` + `ON CONFLICT DO NOTHING` 保證這件事，
/// 因為 `fan_out` 開的是自己的交易 —— relay 的狀態更新失敗時，
/// 通知已經寫進去了，而事件會被重新取用。
#[tokio::test]
async fn replaying_an_event_does_not_duplicate_notifications() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id).await;
    sweep(ctx).await;
    assert!(relay_once(ctx).await >= 1);

    let count_before = {
        let mut tx = ctx.owner_tx().await;
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM fms.notifications WHERE entity_id = $1::uuid")
                .bind(&id)
                .fetch_one(&mut *tx)
                .await
                .expect("數通知");
        n
    };
    assert!(count_before > 0);

    // 把事件退回 PENDING —— 那正是崩潰後重新取用的樣子。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.event_outbox SET status = 'PENDING', published_at = NULL
              WHERE event_type = 'work_order.sla_breached' AND aggregate_id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("重放");
        tx.commit().await.expect("commit");
    }
    assert!(relay_once(ctx).await >= 1, "重放的事件應被再處理一次");

    let mut tx = ctx.owner_tx().await;
    let count_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fms.notifications WHERE entity_id = $1::uuid")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .expect("數通知");
    assert_eq!(
        count_after, count_before,
        "重放不該產生重複的通知 —— 使用者不該因為 relay 重啟就收到兩次"
    );

    ctx.teardown().await;
}

// =============================================================================
// 不會有人收到的情況要被計數
// =============================================================================

/// `REQUESTER` 在 notify 清單裡是**報修的那個人**，不是角色。
///
/// `fms.roles` 裡同時存在 `REQUESTER` 角色。若解析成角色，一張工單完成
/// 會群發給場域內每一個 REQUESTER 角色的使用者 —— **一次群發事故，
/// 而它看起來完全像正常運作**。
#[tokio::test]
async fn requester_means_the_person_not_the_role() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // **第二個帶 REQUESTER 角色的人**，範圍同樣是總部。他不是這張工單的
    // 報修人，因此不該收到 —— 而若解析成角色，他會收到。
    add_user(ctx, "other.requester", "REQUESTER", Some(FACILITY_HQ)).await;

    // user.huang 報修（他有 REQUESTER 角色），admin.chen 處理到完成。
    let requester_token = ctx.login_as(USERNAME_REQUESTER).await;
    let id = create_wo(ctx, &requester_token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "COMPLETE", "resolution_notes": "修好了" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    assert!(relay_once(ctx).await >= 1, "應處理 completed 事件");

    let mut tx = ctx.owner_tx().await;
    let recipients: Vec<String> = sqlx::query_scalar(
        "SELECT u.username::text FROM fms.notifications n
           JOIN fms.users u ON u.id = n.recipient_user_id
          WHERE n.entity_id = $1::uuid AND n.template_code = 'WO_COMPLETED'
          ORDER BY u.username",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await
    .expect("讀收件人");

    assert_eq!(
        recipients,
        vec![USERNAME_REQUESTER.to_string()],
        "只有報修的那個人該收到 —— 若解析成角色，other.requester 也會出現在這裡"
    );

    ctx.teardown().await;
}

/// `WAIT_PARTS` 現在真的會通知維修主管。
///
/// **這個測試原本斷言的是相反的事**：047 之前 `work_order.waiting_parts`
/// 沒有範本，所以這裡驗的是「`no_template` 有被計數」。047 把那十份文案補上
/// 之後，那個前提不存在了 —— 沒有任何一條有 `notify` 的規則缺文案。
///
/// 沒有把測試刪掉，是因為它站在一條有價值的鏈上（轉移 → 事件 → 扇出）。
/// 改成驗新的行為：**有人真的收到了**。
///
/// `no_template` 那個計數器本身仍然有測試守著 ——
/// 見 `a_template_code_that_does_not_exist_is_counted`，走的是「範本碼打錯」
/// 那條路徑，而那條路徑不依賴「目錄裡剛好有規則缺文案」這個會被修掉的前提。
///
/// **種子裡沒有任何人持有 `MAINTENANCE_SUPERVISOR`**，所以這裡要自己造一個。
/// 那件事本身是個發現：光補文案不會讓這種通知送得出去，還得有人擔任那個角色。
/// 「角色存在但沒人持有」現在會計入 `unresolved` ——
/// 見 `a_role_nobody_holds_is_counted_not_silently_dropped`。
#[tokio::test]
async fn waiting_parts_notifies_the_supervisor() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    add_user(
        ctx,
        "sup.chang",
        "MAINTENANCE_SUPERVISOR",
        Some(FACILITY_HQ),
    )
    .await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "WAIT_PARTS", "reason": "等壓縮機" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }

    // `work_order.waiting_parts` 宣告 notify: [MAINTENANCE_SUPERVISOR]，
    // 047 之後有 `WO_WAITING_PARTS` 這份文案。直接呼叫扇出，看它怎麼回報。
    let mut tx = ctx.owner_tx().await;
    let event_id: i64 = sqlx::query_scalar(
        "SELECT id FROM fms.event_outbox
          WHERE event_type = 'work_order.waiting_parts' AND aggregate_id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("應有 waiting_parts 事件");

    let (created, no_template, unresolved): (i32, i32, i32) =
        sqlx::query_as("SELECT * FROM fms.fan_out_notifications($1)")
            .bind(event_id)
            .fetch_one(&mut *tx)
            .await
            .expect("扇出");

    assert_eq!(no_template, 0, "047 補了 WO_WAITING_PARTS，不該再算缺文案");
    assert_eq!(unresolved, 0, "MAINTENANCE_SUPERVISOR 是真的角色碼");
    assert!(created >= 1, "維修主管應該真的收到一封");

    // 而且送出去的字裡沒有沒代換掉的大括號。
    // 這是 `no_template = 0` 抓不到的一種失敗：文案存在，但變數名打錯，
    // 於是收件人收到「地點：{{facilty_name}}」。
    let leftover: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.notifications
          WHERE (body LIKE '%{{%' OR coalesce(subject, '') LIKE '%{{%')",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("查未代換的變數");
    tx.commit().await.expect("commit");

    assert_eq!(leftover, 0, "算好的通知裡不該留著 {{{{變數}}}}");

    ctx.teardown().await;
}

/// **角色存在、但這個場域範圍內沒有人持有它 → 要計入 `unresolved`。**
///
/// 041 的判準是「這個代號是不是一個合法的角色碼」，於是這種情況回空集合：
/// `created`、`no_template`、`unresolved` 三個計數器全是 0，看起來像成功。
///
/// 這不是假想的：種子裡 `MAINTENANCE_SUPERVISOR` 與 `DISPATCHER` 一個持有者
/// 都沒有，而 047 補的 `WO_WAITING_PARTS`／`WO_SUBMITTED` 正是發給這兩個角色。
/// 也就是說補完文案之後，那兩種通知依然送不出去 —— 而舊的判準會讓那件事
/// 完全看不見。047 把判準改成「有沒有解析到人」。
#[tokio::test]
async fn a_role_nobody_holds_is_counted_not_silently_dropped() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    // 刻意**不**造 MAINTENANCE_SUPERVISOR。
    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "WAIT_PARTS", "reason": "等壓縮機" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }

    let mut tx = ctx.owner_tx().await;
    let event_id: i64 = sqlx::query_scalar(
        "SELECT id FROM fms.event_outbox
          WHERE event_type = 'work_order.waiting_parts' AND aggregate_id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("應有 waiting_parts 事件");

    let (created, no_template, unresolved): (i32, i32, i32) =
        sqlx::query_as("SELECT * FROM fms.fan_out_notifications($1)")
            .bind(event_id)
            .fetch_one(&mut *tx)
            .await
            .expect("扇出");
    tx.commit().await.expect("commit");

    assert_eq!(created, 0, "沒有人擔任那個角色，就沒有人收到");
    assert_eq!(no_template, 0, "文案是有的 —— 缺的是人");
    assert_eq!(
        unresolved, 1,
        "「角色存在但沒人持有」必須看得見。三個計數器全 0 等於謊報成功"
    );

    ctx.teardown().await;
}

/// `APPROVER` 既不是角色碼也不是工單欄位 → 計入 `unresolved`。
#[tokio::test]
async fn an_unresolvable_token_is_counted() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    // 解析函式要一張工單（角色分支要用它的 facility_id 判斷範圍）。
    create_wo(ctx, &token, "HIGH").await;

    // 直接驗解析函式：APPROVER 回一筆 user_id 為 NULL 的列。
    let mut tx = ctx.owner_tx().await;
    let unresolved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.notification_recipients(
                  (SELECT id FROM fms.work_orders LIMIT 1),
                  '[\"APPROVER\"]'::jsonb) r
          WHERE r.user_id IS NULL",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("解析");
    assert_eq!(
        unresolved, 1,
        "APPROVER 解析不到任何人，而那必須看得見（不是回空集合）"
    );

    // 反面：真的角色碼解析得到人。
    let resolved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.notification_recipients(
                  (SELECT id FROM fms.work_orders WHERE facility_id = $1::uuid LIMIT 1),
                  '[\"FACILITY_ADMIN\"]'::jsonb) r
          WHERE r.user_id IS NOT NULL",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .unwrap_or(0);
    assert!(resolved >= 1, "FACILITY_ADMIN 應解析到總部的場域管理員");

    ctx.teardown().await;
}

/// 角色代號要**受場域範圍限制**。
///
/// `FACILITY_ADMIN` 指的是「這個工單所在場域的管理員」，不是全租戶的
/// 所有場域管理員。種子裡只有一個 FACILITY_ADMIN，因此少了第二個人
/// 這件事測不出來 —— 突變（拿掉範圍過濾）會全數通過。
#[tokio::test]
async fn a_role_token_respects_facility_scope() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 影廳的場域管理員。總部的工單逾期與他無關。
    add_user(ctx, "cinema.admin", "FACILITY_ADMIN", Some(FACILITY_CINEMA)).await;

    let id = create_wo(ctx, &token, "HIGH").await;
    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id).await;
    sweep(ctx).await;
    relay_once(ctx).await;

    let mut tx = ctx.owner_tx().await;
    let recipients: Vec<String> = sqlx::query_scalar(
        "SELECT u.username::text FROM fms.notifications n
           JOIN fms.users u ON u.id = n.recipient_user_id
          WHERE n.entity_id = $1::uuid AND n.template_code = 'WO_SLA_BREACH'
          ORDER BY u.username",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await
    .expect("讀收件人");

    assert!(
        recipients.contains(&USERNAME_FACILITY_ADMIN.to_string()),
        "總部的場域管理員該收到：{recipients:?}"
    );
    assert!(
        !recipients.contains(&"cinema.admin".to_string()),
        "影廳的場域管理員不該收到總部工單的逾期通知：{recipients:?}"
    );

    ctx.teardown().await;
}

/// `template` 指向一個不存在的範本碼 → 計入 `no_template`。
///
/// 這與「規則完全沒有 template 鍵」是不同的路徑，而後者才是先前測到的那個。
/// 管理者把範本碼打錯（或刪掉範本）時走的是這一條。
#[tokio::test]
async fn a_template_code_that_does_not_exist_is_counted() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_order_transitions_allowed
                SET side_effects = side_effects || '{\"template\": \"WO_TYPO\"}'::jsonb
              WHERE action = 'BREACH_SLA'",
        )
        .execute(&mut *tx)
        .await
        .expect("打錯範本碼");
        tx.commit().await.expect("commit");
    }

    let id = create_wo(ctx, &token, "HIGH").await;
    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id).await;
    sweep(ctx).await;

    let mut tx = ctx.owner_tx().await;
    let event_id: i64 = sqlx::query_scalar(
        "SELECT id FROM fms.event_outbox
          WHERE event_type = 'work_order.sla_breached' AND aggregate_id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("應有事件");

    let (created, no_template, _): (i32, i32, i32) =
        sqlx::query_as("SELECT * FROM fms.fan_out_notifications($1)")
            .bind(event_id)
            .fetch_one(&mut *tx)
            .await
            .expect("扇出");
    tx.commit().await.expect("commit");

    assert_eq!(created, 0, "範本碼不存在就建不出通知");
    assert_eq!(
        no_template, 1,
        "打錯的範本碼必須被計數 —— 否則升級看起來成功而沒有人收到"
    );

    ctx.teardown().await;
}

// =============================================================================
// 收件匣是授權邊界
// =============================================================================

/// **`notifications` 的 RLS 只隔離租戶，沒有按收件人過濾。**
///
/// 因此 `recipient_user_id = 呼叫者` 這個條件不是方便性的過濾，是授權。
/// 少了它，任何登入者都能讀到同租戶每一個人的通知 —— 而那些內容包含
/// 工單標題、負責人姓名與地點。
#[tokio::test]
async fn the_inbox_only_shows_your_own_notifications() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id).await;
    sweep(ctx).await;
    relay_once(ctx).await;

    // fm.lin 收到了。
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (_, fm_inbox) = ctx.send(authed(get("/api/v1/notifications"), &fm)).await;
    let fm_ids: Vec<&str> = fm_inbox["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|n| n["id"].as_str())
        .collect();
    assert!(!fm_ids.is_empty(), "場域管理員應收到：{fm_inbox}");

    // user.huang 沒有 FACILITY_ADMIN 角色 → 收件匣不該有那些通知。
    let other = ctx.login_as(USERNAME_REQUESTER).await;
    let (_, other_inbox) = ctx.send(authed(get("/api/v1/notifications"), &other)).await;
    for n in other_inbox["data"].as_array().expect("data") {
        assert!(
            !fm_ids.contains(&n["id"].as_str().unwrap_or_default()),
            "不該看到別人的通知：{other_inbox}"
        );
    }

    // 也標記不了別人的 —— 而且回 404 而非 403（403 會確認 id 存在）。
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/notifications/{}/read", fm_ids[0]),
                json!({}),
            ),
            &other,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    ctx.teardown().await;
}

/// `unread_only` 與 `unread_count` 各自正確。
///
/// `unread_count` 與 `limit` 無關 —— 從 `data.len()` 推是錯的，而那是
/// 客戶端最容易犯的錯，所以端點自己回。
#[tokio::test]
async fn unread_filtering_and_count_are_independent_of_limit() {
    let ctx = &TestContext::setup().await;
    let admin_id = admin_user_id();

    // 直接種三筆站內通知（扇出的路徑已由前面的測試覆蓋）。
    {
        let mut tx = ctx.owner_tx().await;
        for i in 0..3 {
            sqlx::query(
                "INSERT INTO fms.notifications
                   (tenant_id, recipient_user_id, channel, body, subject)
                 VALUES ($1::uuid, $2, 'IN_APP', $3, $4)",
            )
            .bind(TENANT_ID)
            .bind(admin_id)
            .bind(format!("通知內容 {i}"))
            .bind(format!("主旨 {i}"))
            .execute(&mut *tx)
            .await
            .expect("種通知");
        }
        tx.commit().await.expect("commit");
    }

    let token = ctx.login().await;
    let (_, all) = ctx
        .send(authed(get("/api/v1/notifications?limit=1"), &token))
        .await;
    assert_eq!(all["data"].as_array().expect("data").len(), 1, "limit 生效");
    assert_eq!(
        all["meta"]["unread_count"], 3,
        "未讀數不受 limit 影響：{all}"
    );

    // 讀掉一筆。
    let (_, full) = ctx.send(authed(get("/api/v1/notifications"), &token)).await;
    let first = full["data"][0]["id"].as_str().expect("id");
    let (status, _) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/notifications/{first}/read"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, unread) = ctx
        .send(authed(
            get("/api/v1/notifications?unread_only=true"),
            &token,
        ))
        .await;
    assert_eq!(
        unread["data"].as_array().expect("data").len(),
        2,
        "{unread}"
    );
    assert_eq!(unread["meta"]["unread_count"], 2, "{unread}");

    ctx.teardown().await;
}

// =============================================================================
// 分片
// =============================================================================

/// 通知 relay 處理的事件型別**由目錄查出來**，不是寫死的。
///
/// 寫死一份清單會與目錄脫節，而脫節的症狀是靜默的：管理者為某個轉移加了
/// `notify`，事件照發，但 relay 不認識它 —— 那筆事件會被標成 `SKIPPED`，
/// 看起來像「沒有人要處理」。
#[tokio::test]
async fn the_shard_is_derived_from_the_catalogue() {
    let ctx = &TestContext::setup().await;
    let pool = ctx.owner_pool().await;

    let before = fms_worker::notifier::handled_event_types(&pool)
        .await
        .expect("查型別");
    assert!(
        before.contains(&"work_order.sla_breached".to_string()),
        "目錄裡 BREACH_SLA 宣告了 notify：{before:?}"
    );
    assert!(
        !before.contains(&"work_order.verified".to_string()),
        "VERIFY 沒有宣告 notify：{before:?}"
    );

    // 管理者為 VERIFY 加上 notify → 重新查就會納入，不必改程式。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_order_transitions_allowed
                SET side_effects = side_effects
                                   || '{\"emit\": \"work_order.verified\", \"notify\": [\"REQUESTER\"]}'::jsonb
              WHERE action = 'VERIFY'",
        )
        .execute(&mut *tx)
        .await
        .expect("加 notify");
        tx.commit().await.expect("commit");
    }

    let after = fms_worker::notifier::handled_event_types(&pool)
        .await
        .expect("再查");
    assert!(
        after.contains(&"work_order.verified".to_string()),
        "加了 notify 之後應自動納入分片：{after:?}"
    );

    pool.close().await;
    ctx.teardown().await;
}
