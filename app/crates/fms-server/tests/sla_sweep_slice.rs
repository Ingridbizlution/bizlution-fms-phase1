//! 逾期掃描（ADR-12 量測鏈第 3 段、migration 033）。
//!
//! 逾期是「時間到了而某事沒有發生」，沒有觸發點 —— 沒有人動的工單逾期了
//! 也不會有任何地方知道。因此這一段是掃描，而掃描要驗的東西和轉移不同：
//!
//!   * **會標的要標**（逾回應、逾解決、有風險）
//!   * **不該標的不能標** —— 這一半更重要。一個把已完成、已取消、
//!     或根本沒有 SLA 目標的工單也標成逾期的掃描，會讓報表比沒有掃描更糟。

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
                    "title": "掃描測試",
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

/// 把 due 時刻整體往過去推 —— 掃描看到的就是這個樣子。
async fn age_due(ctx: &TestContext, id: &str, past: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(&format!(
        "UPDATE fms.work_orders
            SET response_due_at   = clock_timestamp() - interval '{past}',
                resolution_due_at = clock_timestamp() - interval '{past}'
          WHERE id = $1::uuid"
    ))
    .bind(id)
    .execute(&mut *tx)
    .await
    .expect("推 due");
    tx.commit().await.expect("commit");
}

async fn sla_state(ctx: &TestContext, id: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    sqlx::query_scalar("SELECT sla_state FROM fms.work_orders WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .expect("讀 sla_state")
}

/// 執行掃描，回傳 (at_risk, response_breached, resolution_breached)。
///
/// 035 之後函式多了 `escalated` 與 `escalation_failed` 兩欄，
/// 升級的斷言集中在 `escalation_*` 那幾個測試裡。
async fn sweep(ctx: &TestContext) -> (i64, i64, i64) {
    let (a, b, c, ..) = sweep_full(ctx).await;
    (a, b, c)
}

/// (at_risk, response, resolution, escalated, not_escalatable, failed)
async fn sweep_full(ctx: &TestContext) -> (i64, i64, i64, i64, i64, i64) {
    let mut tx = ctx.owner_tx().await;
    let row: (i64, i64, i64, i64, i64, i64) =
        sqlx::query_as("SELECT * FROM fms.sweep_sla_states()")
            .fetch_one(&mut *tx)
            .await
            .expect("sweep");
    tx.commit().await.expect("commit");
    row
}

async fn status_of(ctx: &TestContext, id: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    sqlx::query_scalar("SELECT status FROM fms.work_orders WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .expect("讀 status")
}

// =============================================================================
// 會標的要標
// =============================================================================

/// 沒有人接下、回應時限已過 → `RESPONSE_BREACHED`。
#[tokio::test]
async fn an_unanswered_work_order_is_response_breached() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;
    age_due(ctx, &id, "2 hours").await;

    let (_, response, _) = sweep(ctx).await;
    assert_eq!(response, 1, "應標記一張逾回應的工單");
    assert_eq!(sla_state(ctx, &id).await, "RESPONSE_BREACHED");

    // 掃描要**幂等**：排程器每分鐘跑一次，同一張單不該被反覆計數，
    // 否則任何以計數為基礎的告警都會持續尖叫。
    let (_, response, _) = sweep(ctx).await;
    assert_eq!(response, 0, "第二次掃描不該再標同一張");

    ctx.teardown().await;
}

/// 有人接下了但沒做完、解決時限已過 → `RESOLUTION_BREACHED`。
#[tokio::test]
async fn an_unfinished_work_order_is_resolution_breached() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    transition(
        ctx,
        &token,
        &id,
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
    )
    .await;
    transition(ctx, &token, &id, json!({ "action": "START_WORK" })).await;
    age_due(ctx, &id, "2 hours").await;

    let (_, response, resolution) = sweep(ctx).await;
    assert_eq!(
        response, 0,
        "已經有人接下（first_responded_at 有值），不該算逾回應"
    );
    assert_eq!(resolution, 1);
    assert_eq!(sla_state(ctx, &id).await, "RESOLUTION_BREACHED");

    ctx.teardown().await;
}

/// 同時逾兩者時，標的是**先發生的那一個**。
///
/// 理由是批次補跑的結果要等於連續運行的結果：掃描每分鐘跑時
/// `response_due_at` 必然先到，那一刻就標成 `RESPONSE_BREACHED` 並離開
/// `idx_wo_sla_watch` 的部分索引。若補跑改標解決，同一批資料會依
/// 「掃描有沒有中斷過」得到不同答案。
///
/// 這也是為什麼**報表不讀 `sla_state`** —— 它只放得下一個逾期。
#[tokio::test]
async fn breaching_both_records_the_first_one() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;
    age_due(ctx, &id, "2 hours").await;

    let (_, response, resolution) = sweep(ctx).await;
    assert_eq!(
        (response, resolution),
        (1, 0),
        "同時逾兩者應記回應（先發生的），且不重複計入解決"
    );
    assert_eq!(sla_state(ctx, &id).await, "RESPONSE_BREACHED");

    // 事實仍然完整：兩個時刻都在過去，報表算得出兩個逾期。
    let mut tx = ctx.owner_tx().await;
    let (resp_past, resol_past): (bool, bool) = sqlx::query_as(
        "SELECT response_due_at < clock_timestamp(), resolution_due_at < clock_timestamp()
           FROM fms.work_orders WHERE id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀時刻");
    assert!(
        resp_past && resol_past,
        "sla_state 只放得下一個，但兩個時刻都還在 —— 報表要從時刻算"
    );

    ctx.teardown().await;
}

/// 窗口用掉 80% 之後 → `AT_RISK`，且還沒逾期。
#[tokio::test]
async fn a_work_order_near_its_target_is_at_risk() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    // MEDIUM → SLA_STANDARD，resolution_minutes = 480。
    // 剩 20% 是 96 分鐘，因此把 due 調到 30 分鐘後就落在風險區內。
    let id = create_wo(ctx, &token, "MEDIUM").await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_orders
                SET resolution_due_at = clock_timestamp() + interval '30 minutes',
                    response_due_at   = clock_timestamp() + interval '30 minutes'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("調整 due");
        tx.commit().await.expect("commit");
    }

    let (at_risk, response, resolution) = sweep(ctx).await;
    assert_eq!(
        (at_risk, response, resolution),
        (1, 0, 0),
        "還沒逾期，只該標 AT_RISK"
    );
    assert_eq!(sla_state(ctx, &id).await, "AT_RISK");

    ctx.teardown().await;
}

/// 還很早 → 什麼都不標。
///
/// 這是 `AT_RISK` 門檻的另一邊。少了它，「門檻算對了」與
/// 「只要還沒逾期就一律標 AT_RISK」分不出來。
#[tokio::test]
async fn a_work_order_with_plenty_of_time_is_untouched() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    // 剛建的 MEDIUM 工單有 480 分鐘，離 96 分鐘的風險區還很遠。
    let id = create_wo(ctx, &token, "MEDIUM").await;

    let (at_risk, response, resolution) = sweep(ctx).await;
    assert_eq!(
        (at_risk, response, resolution),
        (0, 0, 0),
        "480 分鐘的窗口才剛開始，不該有任何標記"
    );
    assert_eq!(sla_state(ctx, &id).await, "ON_TRACK");

    ctx.teardown().await;
}

// =============================================================================
// 自動升級（migration 035）
// =============================================================================

/// `IN_PROGRESS` 的逾期工單會被自動升級成 `SLA_BREACHED`，
/// 而 `sla_state` 的標記**不被轉移蓋掉**。
///
/// 順序是必要的：035 先標 `sla_state`、再呼叫 `BREACH_SLA`。反過來做會被
/// 032 的 `sla_state` CASE 覆寫 —— `BREACH_SLA` 的 side_effects 沒有
/// `compute_sla`，所以那個 CASE 會走 `ELSE sla_state`，保留先標好的值。
#[tokio::test]
async fn an_in_progress_breach_is_escalated() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id, "2 hours").await;

    let (_, _, resolution, escalated, skipped, failed) = sweep_full(ctx).await;
    assert_eq!((resolution, escalated, skipped, failed), (1, 1, 0, 0));
    assert_eq!(status_of(ctx, &id).await, "SLA_BREACHED");
    assert_eq!(
        sla_state(ctx, &id).await,
        "RESOLUTION_BREACHED",
        "轉移不該蓋掉剛標好的 sla_state"
    );

    // 升級留下稽核軌跡，而且是系統做的（032 的 actor_type 修正）。
    let mut tx = ctx.owner_tx().await;
    let (actor_type, reason): (String, Option<String>) = sqlx::query_as(
        "SELECT actor_type, reason FROM fms.work_order_transitions
          WHERE work_order_id = $1::uuid AND action = 'BREACH_SLA'",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("應有 BREACH_SLA 轉移");
    assert_eq!(actor_type, "SYSTEM", "沒有人做這件事");
    assert!(reason.is_some(), "升級要留下理由");

    ctx.teardown().await;
}

/// `AUTO_ASSIGN` 派了但沒有人接手 → 逾回應，而且會被升級。
///
/// 這一格是決定 B 與 035 的交會點：`AUTO_ASSIGN` 不算人為回應（032），
/// 因此這張工單會逾回應；而它的狀態是 `ASSIGNED`，正好在目錄允許
/// `BREACH_SLA` 的範圍內。**一張系統塞給某人、而那個人從未看過的工單，
/// 現在會自己浮出來。**
#[tokio::test]
async fn an_auto_assigned_but_untouched_work_order_is_escalated() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.work_orders SET assignee_id = $2::uuid WHERE id = $1::uuid")
            .bind(&id)
            .bind(TECH_WANG)
            .execute(&mut *tx)
            .await
            .expect("設定負責人");
        sqlx::query(
            "SELECT fms.transition_work_order($1::uuid, 'AUTO_ASSIGN', NULL, NULL, '{}'::jsonb)",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("AUTO_ASSIGN");
        tx.commit().await.expect("commit");
    }
    age_due(ctx, &id, "2 hours").await;

    let (_, response, _, escalated, ..) = sweep_full(ctx).await;
    assert_eq!(
        (response, escalated),
        (1, 1),
        "AUTO_ASSIGN 不算回應，因此逾回應；狀態是 ASSIGNED，因此可升級"
    );
    assert_eq!(status_of(ctx, &id).await, "SLA_BREACHED");

    ctx.teardown().await;
}

/// **還停在 `SUBMITTED` 的逾期工單只標記、不升級。**
///
/// 這是 035 覆蓋範圍的邊界，而且是最反直覺的一格：沒有人接手的工單
/// 最該被升級，卻是唯一升不了的。
///
/// 原因在目錄：`BREACH_SLA` 只能從 `ASSIGNED`／`IN_PROGRESS` 進入，
/// 而補上 `SUBMITTED → SLA_BREACHED` 會把工單困死 —— `SLA_BREACHED`
/// 出去的路只有 `CANCEL`／`COMPLETE`／`RESUME`，**沒有 `ASSIGN`**。
///
/// 這個測試在這裡是為了讓那個缺口**有名字**：它是被記錄的取捨，
/// 不是沒人發現的漏洞。逾期仍然標了、仍然進報表分母。
#[tokio::test]
async fn a_submitted_breach_is_marked_but_not_escalated() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;
    age_due(ctx, &id, "2 hours").await;

    let (_, response, _, escalated, skipped, failed) = sweep_full(ctx).await;
    assert_eq!(response, 1, "逾期照標");
    assert_eq!(
        (escalated, skipped, failed),
        (0, 1, 0),
        "目錄不允許從 SUBMITTED 升級 —— 記在 not_escalatable，不是 escalation_failed。\
         兩者混在一起的話，覆蓋缺口會長得像每分鐘一次的系統故障"
    );
    assert_eq!(status_of(ctx, &id).await, "SUBMITTED", "狀態不動");
    assert_eq!(sla_state(ctx, &id).await, "RESPONSE_BREACHED");

    ctx.teardown().await;
}

/// 等待中的逾期工單也只標記、不升級 —— 理由與上面不同。
///
/// 改成 `SLA_BREACHED` 會**抹掉「為什麼卡住」**（等料／等廠商／等核准），
/// 而那正是決定 D 要讓人看見的資訊。逾期已經記在 `sla_state` 與報表的
/// `avg_waiting_minutes` 裡；再犧牲卡住的原因換一個狀態碼是淨損失。
#[tokio::test]
async fn a_waiting_breach_keeps_its_status() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "WAIT_PARTS", "reason": "等壓縮機" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id, "2 hours").await;

    let (_, _, resolution, escalated, skipped, failed) = sweep_full(ctx).await;
    assert_eq!(resolution, 1, "決定 D：等待不停錶，因此照樣逾期");
    assert_eq!((escalated, skipped, failed), (0, 1, 0));
    assert_eq!(
        status_of(ctx, &id).await,
        "WAITING_PARTS",
        "「為什麼卡住」比一個 SLA_BREACHED 狀態碼有用"
    );

    ctx.teardown().await;
}

/// 升級是幂等的 —— 第二輪掃描不會再動它。
///
/// `BREACH_SLA` 從 `SLA_BREACHED` 出發是不合法的（目錄沒有那條規則），
/// 因此若第二輪還把它選進升級清單，會得到 `escalation_failed = 1`
/// 而不是 0。這個測試同時守住「不重複升級」與「不把正常情況記成失敗」。
#[tokio::test]
async fn escalation_does_not_repeat() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id, "2 hours").await;

    let (_, _, _, escalated, ..) = sweep_full(ctx).await;
    assert_eq!(escalated, 1);

    let (a, b, c, escalated, skipped, failed) = sweep_full(ctx).await;
    assert_eq!(
        (a, b, c, escalated, skipped, failed),
        (0, 0, 0, 0, 0, 0),
        "第二輪應完全安靜 —— 尤其 escalation_failed 必須是 0，\
         否則每分鐘都會有一筆假的失敗告警"
    );

    // 只有一筆 BREACH_SLA 轉移。
    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.work_order_transitions
          WHERE work_order_id = $1::uuid AND action = 'BREACH_SLA'",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("數轉移");
    assert_eq!(n, 1, "稽核軌跡不該有重複的升級");

    ctx.teardown().await;
}

/// 升級失敗時，**逾期標記仍然留下來**。
///
/// 035 逐筆包 EXCEPTION，理由是掃描每分鐘對線上系統跑一次：從標記到轉移
/// 之間有人推進了同一張工單，`BREACH_SLA` 就可能不再合法。若讓它整批失敗，
/// worker 的單一交易會把**這一輪全部的標記都回滾**，下一輪再撞同一張 →
/// 永久停擺。
///
/// 那個競態沒辦法在測試裡穩定重現，因此這裡用一個**同樣真實的錯誤設定**
/// 製造失敗：給 `BREACH_SLA` 加上 `required_permission`。掃描沒有 actor
/// （它不是任何人做的），因此 022 的權限檢查會以 42501 拋出。
///
/// 那是管理者真的可能做的事 —— 目錄可編輯，而「系統驅動的動作不該有權限碼」
/// 這件事只寫在 handler 的註解裡。
///
/// 不能用「停用規則」來製造失敗：036 之後那會被歸類成 `not_escalatable`
/// （目錄不允許＝不在範圍內），而不是失敗 —— 那正是那兩個計數要分開的理由。
///
/// 沒有這個測試，那段 EXCEPTION 就是一段從來沒被執行過的防禦程式碼。
#[tokio::test]
async fn a_failed_escalation_still_leaves_the_breach_marked() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id, "2 hours").await;

    {
        let mut tx = ctx.owner_tx().await;
        let n = sqlx::query(
            "UPDATE fms.work_order_transitions_allowed
                SET required_permission = 'work_order:execute'
              WHERE action = 'BREACH_SLA'",
        )
        .execute(&mut *tx)
        .await
        .expect("給 BREACH_SLA 加權限要求")
        .rows_affected();
        assert_eq!(n, 2, "目錄應有兩條 BREACH_SLA 規則");
        tx.commit().await.expect("commit");
    }

    let (_, _, resolution, escalated, skipped, failed) = sweep_full(ctx).await;
    assert_eq!(resolution, 1, "標記照做");
    assert_eq!(
        (escalated, skipped, failed),
        (0, 0, 1),
        "這是真的失敗（42501），不是「不在範圍內」—— 兩者必須分得開"
    );

    // **重點**：標記活下來了。整批失敗的話這裡會是 ON_TRACK。
    assert_eq!(
        sla_state(ctx, &id).await,
        "RESOLUTION_BREACHED",
        "一張工單升級失敗不該讓整輪掃描的標記回滾 —— 那會永久停擺"
    );
    assert_eq!(status_of(ctx, &id).await, "IN_PROGRESS", "狀態沒變");

    ctx.teardown().await;
}

/// 升級會發出 `work_order.sla_breached` 事件 —— **但沒有人消費它**。
///
/// 目錄裡那條規則宣告了 `notify: [FACILITY_ADMIN, MAINTENANCE_SUPERVISOR]`，
/// 而全 repo 沒有任何 `INSERT INTO fms.notifications`，`fms-jobs` 的 relay
/// 也只處理 `maintenance.meter_threshold_reached`。
///
/// 這個測試斷言的是**目前的真實狀態**：事件進了 outbox，通知沒有發出。
/// 它存在的理由是防止「已經自動升級了」被讀成「有人會被通知」——
/// 而當通知真的被實作時，這個測試會失敗，逼人回來更新這段說明。
#[tokio::test]
async fn escalation_emits_an_event_that_nobody_consumes_yet() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id, "2 hours").await;
    let (_, _, _, escalated, ..) = sweep_full(ctx).await;
    assert_eq!(escalated, 1);

    let mut tx = ctx.owner_tx().await;
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.event_outbox
          WHERE event_type = 'work_order.sla_breached'
            AND aggregate_id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("數事件");
    assert_eq!(events, 1, "升級應發出事件");

    let notifications: i64 = sqlx::query_scalar("SELECT count(*) FROM fms.notifications")
        .fetch_one(&mut *tx)
        .await
        .expect("數通知");
    assert_eq!(
        notifications, 0,
        "目前沒有任何東西寫 fms.notifications —— 若這裡開始失敗，\
         代表通知已經實作了，請一併更新 035 檔頭與本測試的說明"
    );

    ctx.teardown().await;
}

// =============================================================================
// 門檻由管理者定義（migration 036）
// =============================================================================

/// 改 policy 的 `escalation_rules`，預警時點就跟著變 —— 不改任何程式。
///
/// 033 用一個全域的 0.8 蓋掉了三個 policy 各自宣告的 `at_pct`
/// （SLA_CRITICAL 是 75、SLA_STANDARD 是 80）。036 改成讀目錄。
///
/// 這個測試把 SLA_STANDARD 從 80 改成 20，然後斷言一張才剛開始的工單
/// 就進入預警 —— 若門檻還是寫死的 0.8，它會什麼都不標。
#[tokio::test]
async fn the_at_risk_threshold_comes_from_the_policy() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.sla_policies
                SET escalation_rules = '[{\"at_pct\": 20}]'::jsonb
              WHERE code = 'SLA_STANDARD'",
        )
        .execute(&mut *tx)
        .await
        .expect("改門檻");
        tx.commit().await.expect("commit");
    }

    // MEDIUM → SLA_STANDARD，480 分鐘。門檻 20% ⇒ 用掉 96 分鐘就預警。
    // 把 due 調到 300 分鐘後（等於已用掉 180 分＝37.5%）。
    let id = create_wo(ctx, &token, "MEDIUM").await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_orders
                SET resolution_due_at = clock_timestamp() + interval '300 minutes',
                    response_due_at   = clock_timestamp() + interval '300 minutes'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("調 due");
        tx.commit().await.expect("commit");
    }

    let (at_risk, ..) = sweep_full(ctx).await;
    assert_eq!(
        at_risk, 1,
        "門檻改成 20% 之後，用掉 37.5% 的工單就該預警（寫死 0.8 的話不會）"
    );
    assert_eq!(sla_state(ctx, &id).await, "AT_RISK");

    ctx.teardown().await;
}

/// 沒有宣告 `at_pct < 100` 的 policy **不預警** —— 那是管理者的選擇。
///
/// 種子裡 SLA_CLEANING 就是這樣：只有 `at_pct 100`（逾期本身）。
/// 033 的全域 0.8 會硬給它一個預警點；036 尊重那個宣告。
///
/// 這一格是「沒有預設後備」的立場，與 032 的 `resolve_sla_policy`
/// 刻意不設 default policy 同源：沒有宣告就是沒有，不要代為決定。
#[tokio::test]
async fn a_policy_without_a_warning_rule_never_goes_at_risk() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // HIGH → SLA_CLEANING（60 分鐘，escalation_rules 只有 at_pct 100）。
    let id = create_wo(ctx, &token, "HIGH").await;
    {
        let mut tx = ctx.owner_tx().await;
        // 只剩 1 分鐘 —— 任何合理的預警門檻都會觸發，除了「不預警」。
        sqlx::query(
            "UPDATE fms.work_orders
                SET resolution_due_at = clock_timestamp() + interval '1 minute',
                    response_due_at   = clock_timestamp() + interval '1 minute'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("調 due");
        tx.commit().await.expect("commit");
    }

    let (at_risk, ..) = sweep_full(ctx).await;
    assert_eq!(
        at_risk, 0,
        "SLA_CLEANING 沒有宣告預警規則，就不該有預警 —— 即使只剩一分鐘"
    );
    assert_eq!(sla_state(ctx, &id).await, "ON_TRACK");

    ctx.teardown().await;
}

/// 管理者在目錄補一條 `BREACH_SLA` 規則，升級範圍就跟著擴 —— 不改程式。
///
/// 035 把可升級的狀態寫死成 `('ASSIGNED','IN_PROGRESS')`，還加了一條
/// 自我驗證去擋目錄新增規則 —— 那條驗證等於**禁止管理者設定**。
/// 036 改成問目錄。
///
/// 這裡補上 035 檔頭說的那組配套（`SUBMITTED → SLA_BREACHED` 加上
/// `ASSIGN: SLA_BREACHED → ASSIGNED`，後者是為了工單不被困死），
/// 然後斷言原本升不了的那一類現在升得了。
#[tokio::test]
async fn adding_a_catalogue_rule_widens_escalation_without_code_changes() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.work_order_transitions_allowed
               (from_status, action, to_status, required_permission, side_effects, is_active)
             VALUES
               ('SUBMITTED', 'BREACH_SLA', 'SLA_BREACHED', NULL,
                '{\"emit\": \"work_order.sla_breached\", \"actor\": \"SYSTEM\"}'::jsonb, true),
               ('SLA_BREACHED', 'ASSIGN', 'ASSIGNED', 'work_order:assign',
                '{\"emit\": \"work_order.assigned\", \"set_responded\": true}'::jsonb, true)",
        )
        .execute(&mut *tx)
        .await
        .expect("補目錄規則");
        tx.commit().await.expect("commit");
    }

    let id = create_wo(ctx, &token, "HIGH").await;
    age_due(ctx, &id, "2 hours").await;

    let (_, response, _, escalated, skipped, failed) = sweep_full(ctx).await;
    assert_eq!(
        (response, escalated, skipped, failed),
        (1, 1, 0, 0),
        "目錄允許之後，SUBMITTED 的逾期工單就會被升級 —— 035 的硬編清單做不到"
    );
    assert_eq!(status_of(ctx, &id).await, "SLA_BREACHED");

    // 而且沒有被困死：配套的 ASSIGN 讓它還能派工。
    transition(
        ctx,
        &token,
        &id,
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
    )
    .await;
    assert_eq!(status_of(ctx, &id).await, "ASSIGNED");

    ctx.teardown().await;
}

/// 形狀壞掉的 `escalation_rules` 在**寫入時**就被擋下。
///
/// 一旦管理者能編輯這個欄位，掃描就會去讀他打的字。`"at_pct": "80%"`
/// 會讓 `::numeric` 轉型失敗 —— 而那個失敗發生在每分鐘跑的跨租戶掃描裡，
/// 會讓所有租戶的那一輪標記一起回滾。
///
/// 「讓管理者定義」的另一半是「資料庫定義那個定義的形狀」。
#[tokio::test]
async fn a_malformed_escalation_rule_is_rejected_at_write_time() {
    let ctx = &TestContext::setup().await;

    for bad in [
        r#"[{"at_pct": "80%"}]"#,  // 字串而非數字
        r#"[{"at_pct": 0}]"#,      // 0 沒有意義（一開始就預警）
        r#"[{"at_pct": 150}]"#,    // 超過 100
        r#"{"at_pct": 80}"#,       // 物件而非陣列
        r#"[{"notify": ["FM"]}]"#, // 缺 at_pct
    ] {
        let mut tx = ctx.owner_tx().await;
        let r = sqlx::query(
            "UPDATE fms.sla_policies SET escalation_rules = $1::jsonb WHERE code = 'SLA_STANDARD'",
        )
        .bind(bad)
        .execute(&mut *tx)
        .await;
        assert!(
            r.is_err(),
            "{bad} 應被 CHECK 擋下 —— 否則它會在掃描裡炸，拖垮整輪"
        );
    }

    ctx.teardown().await;
}

// =============================================================================
// 不該標的不能標
// =============================================================================

/// 已完成的工單不再被掃描改動。
///
/// 它的 `sla_state` 在 COMPLETE 那一刻就定了（032 改動 5）。掃描再去碰它
/// 等於讓「達成」在完成之後還會變成「逾期」—— 一個已經結案的數字被
/// 事後改寫，是報表最不能有的性質。
#[tokio::test]
async fn a_completed_work_order_is_never_re_judged() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "MEDIUM").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "COMPLETE", "resolution_notes": "修好了" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    assert_eq!(sla_state(ctx, &id).await, "MET", "前提：準時完成");

    // 把 due 推到過去 —— 若掃描不看 completed_at，這裡就會被改成逾期。
    age_due(ctx, &id, "2 hours").await;

    let (at_risk, response, resolution) = sweep(ctx).await;
    assert_eq!((at_risk, response, resolution), (0, 0, 0));
    assert_eq!(
        sla_state(ctx, &id).await,
        "MET",
        "已完成的判定不該被掃描事後改寫"
    );

    ctx.teardown().await;
}

/// 已取消的工單不算逾期。
///
/// `CANCELLED` 是 `TERMINAL` 類別。沒有做完不代表逾期 —— 決定 G 把它排除在
/// 分母外，而掃描若先把它標成逾期，那個排除就得在報表裡再做一次
/// （於是同一條規則又有兩份）。
#[tokio::test]
async fn a_cancelled_work_order_is_not_breached() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    transition(
        ctx,
        &token,
        &id,
        json!({ "action": "CANCEL", "reason": "誤報" }),
    )
    .await;
    age_due(ctx, &id, "2 hours").await;

    let (at_risk, response, resolution) = sweep(ctx).await;
    assert_eq!(
        (at_risk, response, resolution),
        (0, 0, 0),
        "TERMINAL 類別的工單不該被標記"
    );

    ctx.teardown().await;
}

/// 沒有 SLA 目標的工單（`NOT_APPLICABLE`）不會被掃描拉回量測。
#[tokio::test]
async fn a_work_order_without_a_policy_stays_not_applicable() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    // LOW 解析不到 policy（種子只有 CRITICAL/HIGH/MEDIUM）。
    let id = create_wo(ctx, &token, "LOW").await;
    assert_eq!(sla_state(ctx, &id).await, "NOT_APPLICABLE", "前提");

    let (at_risk, response, resolution) = sweep(ctx).await;
    assert_eq!((at_risk, response, resolution), (0, 0, 0));
    assert_eq!(
        sla_state(ctx, &id).await,
        "NOT_APPLICABLE",
        "沒有目標就是沒在量，掃描不該把它變成逾期"
    );

    ctx.teardown().await;
}

/// **重開後的第二輪一樣會被掃描標記。**
///
/// 這個測試是突變測試逼出來的。033 第一版的三個掃描都帶
/// `completed_at IS NULL`，而突變（拿掉它）十個測試全部照過 ——
/// 那個條件對「已完成的工單」是多餘的（`sla_state` 已經不是 ON_TRACK）。
///
/// 但它不只是多餘：032 的 REOPEN 把 `sla_state` 重設成 `ON_TRACK`，
/// 卻**沒有清掉 `completed_at`**（那個欄位只在進入 COMPLETED 時寫）。
/// 於是 `completed_at IS NULL` 會把重開過的工單**永久排除在掃描之外**，
/// 決定 E（重開是新的量測）靜默失效 —— 第二輪逾期多久都不會被標。
///
/// 一個測試通不過任何突變，通常不是實作特別穩，而是**測試沒有覆蓋
/// 那條路徑**。
#[tokio::test]
async fn a_reopened_work_order_is_swept_again() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "MEDIUM").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "COMPLETE", "resolution_notes": "修好了" }),
        json!({ "action": "REOPEN", "reason": "又壞了" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    assert_eq!(
        sla_state(ctx, &id).await,
        "ON_TRACK",
        "前提：重開後重新起算（決定 E）"
    );

    // **044 之後 `completed_at` 被清掉了。**
    //
    // 這個斷言原本是反過來的：它把「留著上一輪的值」記成前提，並註明
    // 「若日後改了，本測試的理由要一起更新」—— 044 就是那個改動。
    //
    // 而這個測試的價值沒有變：它守的是「重開後的第二輪仍然會被掃描標記」。
    // 033 的守衛已經不看 `completed_at`（用 `sla_state` + 狀態類別），
    // 因此那個欄位是 NULL 或不是都不影響掃描 —— 但它現在說的是真話。
    {
        let mut tx = ctx.owner_tx().await;
        let (completed, ended): (bool, bool) = sqlx::query_as(
            "SELECT completed_at IS NULL, actual_end_at IS NULL
               FROM fms.work_orders WHERE id = $1::uuid",
        )
        .bind(&id)
        .fetch_one(&mut *tx)
        .await
        .expect("讀完成痕跡");
        assert!(
            completed && ended,
            "044：重開之後不該還留著 completed_at / actual_end_at"
        );
    }

    age_due(ctx, &id, "2 hours").await;

    let (_, _, resolution) = sweep(ctx).await;
    assert_eq!(
        resolution, 1,
        "重開後的第二輪也要被掃描標記，否則決定 E 只是把狀態改回去而已"
    );
    assert_eq!(sla_state(ctx, &id).await, "RESOLUTION_BREACHED");

    ctx.teardown().await;
}

/// Worker 層：`SlaWatchdog::run_once` 自己取得平台情境。
///
/// 033 刻意不用 SECURITY DEFINER，代價是**呼叫端必須自己開平台情境**。
/// 那個責任在 `fms-worker`，而它是靜默失敗的類型：沒有平台情境時
/// `work_orders` 的 FORCE RLS 讓 UPDATE 影響 0 列 ——
/// 掃描會回報 (0,0,0)「沒事」，而實際上它什麼都看不到。
///
/// 因此這個測試繞過 `ctx.owner_tx()`（那個 helper 自己設了情境），
/// 直接用 worker 的進入點。
#[tokio::test]
async fn the_worker_entry_point_acquires_platform_context() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;
    age_due(ctx, &id, "2 hours").await;

    let pool = ctx.owner_pool().await;
    let watchdog = fms_worker::sla_watchdog::SlaWatchdog::new(pool.clone());
    let swept = watchdog.run_once().await.expect("run_once");

    assert_eq!(
        swept.response_breached, 1,
        "worker 進入點應看得到工單 —— 回 0 代表平台情境沒設，RLS 把它濾光了：{swept:?}"
    );
    assert_eq!(sla_state(ctx, &id).await, "RESPONSE_BREACHED");

    // 幂等：排程器每分鐘呼叫一次。
    let again = watchdog.run_once().await.expect("run_once 第二次");
    assert!(again.is_quiet(), "第二輪應無變化：{again:?}");

    pool.close().await;
    ctx.teardown().await;
}

/// 等待中的工單**仍然在計時**（決定 D：不停錶）。
///
/// 這一格是決定 D 的唯一執行點。`WAITING` 類別有四個狀態，
/// 若掃描把它們排除，決定 D 就變成了「有停錶、只是沒寫在文件裡」。
#[tokio::test]
async fn a_waiting_work_order_still_breaches() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = create_wo(ctx, &token, "HIGH").await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "WAIT_PARTS", "reason": "等壓縮機" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }
    age_due(ctx, &id, "2 hours").await;

    let (_, _, resolution) = sweep(ctx).await;
    assert_eq!(
        resolution, 1,
        "ADR-12 決定 D：等待狀態不停錶，因此等料的工單一樣會逾期"
    );
    assert_eq!(sla_state(ctx, &id).await, "RESOLUTION_BREACHED");

    ctx.teardown().await;
}
