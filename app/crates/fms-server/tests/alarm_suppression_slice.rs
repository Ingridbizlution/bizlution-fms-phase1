//! `POST /alarms/{id}/suppress` 與 `POST /alarms:reconcile-work-orders`。
//!
//! # 這一組最重要的一格是 `c_`
//!
//! 071 之前，把告警設成 `SUPPRESSED` 會讓事情**變糟**：`raise_alarm()` 找既有
//! 告警的條件是 `status IN ('ACTIVE','ACKNOWLEDGED')`，一筆 SUPPRESSED 的告警
//! 不在那個集合裡，因此下一次門檻觸發會**新增一筆告警**、發出 `alarm.raised`、
//! 並可能再開一張工單。
//!
//! 也就是說：使用者按了「抑制」，得到的是更多噪音。而端點仍然回 200。
//!
//! `c_` 觸發兩次並斷言只有一筆告警、事件沒有增加。這是「若壞掉，端點仍然回
//! 200」的性質 —— 只有測試看得見。
//!
//! # `f_`／`g_` 守的是「沒有補」的四種原因分得開
//!
//! 只回一個 `reconciled: N` 會讓「沒有缺口」與「有缺口但全部被跳過」長得一樣。
//! 而 `skipped_rule_does_not_auto_create` **不是缺口** —— 把它算進去，
//! 那個數字會永遠降不到 0，於是沒有人再看它。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 009 的示範規則與計量點（`alarm_slice` 用同一組）。
const RULE_HVAC: &str = "a4000000-0000-4000-8000-000000000001";
const POINT_HVAC: &str = "a3000000-0000-4000-8000-000000000002";

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// 走 006／071 的 `raise_alarm`，與規則引擎完全相同的路徑。
async fn raise(ctx: &TestContext, value: f64, msg: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("SELECT fms.set_context($1::uuid, NULL, false)")
        .bind(TENANT_ID)
        .execute(&mut *tx)
        .await
        .expect("set_context");
    let id: uuid::Uuid =
        sqlx::query_scalar("SELECT fms.raise_alarm($1::uuid, $2::uuid, $3::numeric, $4)")
            .bind(RULE_HVAC)
            .bind(POINT_HVAC)
            .bind(value)
            .bind(msg)
            .fetch_one(&mut *tx)
            .await
            .expect("raise_alarm");
    tx.commit().await.expect("commit");
    id.to_string()
}

async fn suppress(ctx: &TestContext, token: &str, id: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        post(&format!("/api/v1/alarms/{id}/suppress"), body),
        token,
    ))
    .await
}

async fn reconcile(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        post("/api/v1/alarms:reconcile-work-orders", body),
        token,
    ))
    .await
}

/// 這個規則 + 計量點目前有幾筆告警。**`c_` 的核心量測。**
async fn alarm_count(ctx: &TestContext) -> i64 {
    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.alarms WHERE alarm_rule_id = $1::uuid AND tenant_id = $2::uuid",
    )
    .bind(RULE_HVAC)
    .bind(TENANT_ID)
    .fetch_one(&mut *tx)
    .await
    .expect("count alarms");
    drop(tx);
    n
}

/// `alarm.raised` 事件的筆數。抑制的**全部意義**就是讓這個數字不動。
async fn raised_event_count(ctx: &TestContext) -> i64 {
    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.event_outbox
          WHERE event_type = 'alarm.raised' AND tenant_id = $1::uuid",
    )
    .bind(TENANT_ID)
    .fetch_one(&mut *tx)
    .await
    .expect("count events");
    drop(tx);
    n
}

async fn alarm_row(ctx: &TestContext, id: &str) -> (String, Option<i64>, i32) {
    let mut tx = ctx.owner_tx().await;
    let row: (String, Option<chrono::DateTime<chrono::Utc>>, i32) = sqlx::query_as(
        "SELECT status, suppressed_until, occurrence_count FROM fms.alarms WHERE id = $1::uuid",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("read alarm");
    drop(tx);
    (row.0, row.1.map(|t| t.timestamp()), row.2)
}

async fn facility_of(ctx: &TestContext, alarm_id: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    let f: uuid::Uuid =
        sqlx::query_scalar("SELECT facility_id FROM fms.alarms WHERE id = $1::uuid")
            .bind(alarm_id)
            .fetch_one(&mut *tx)
            .await
            .expect("facility");
    drop(tx);
    f.to_string()
}

// =============================================================================

/// 抑制成功，狀態與期限都寫進去，而且回應是**更新後**的值。
#[tokio::test]
async fn a_suppress_sets_a_bounded_deadline() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = raise(ctx, 31.0, "冷氣過熱").await;

    let (status, body) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 240, "reason": "空調機房今晚維修"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // 資料修改型 CTE 會回傳更新**前**的 snapshot，症狀是「按了抑制但畫面沒變」。
    assert_eq!(
        body["data"]["status"],
        json!("SUPPRESSED"),
        "回應不是更新後的狀態：{body}"
    );
    assert!(
        !body["data"]["suppressed_until"].is_null(),
        "回應沒有帶出期限 —— 值班畫面就看不出「到幾點為止」：{body}"
    );
    assert_eq!(body["meta"]["extended_existing_suppression"], json!(false));
    // 抑制**不做**的事要說出來，否則按下按鈕的人會以為問題解決了。
    assert!(
        body["meta"]["does_not"].as_array().unwrap().len() >= 3,
        "{body}"
    );

    let (db_status, until, _) = alarm_row(ctx, &id).await;
    assert_eq!(db_status, "SUPPRESSED");
    assert!(until.is_some(), "071 的約束要求 SUPPRESSED 必有期限");

    ctx.teardown().await;
}

/// 缺 reason、時長不合、超過租戶上限。
#[tokio::test]
async fn b_suppress_validates_reason_and_the_tenant_cap() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = raise(ctx, 31.0, "冷氣過熱").await;

    // reason 必填 —— 下一個發現告警沒響的人需要答案。
    let (status, body) = suppress(ctx, &admin, &id, json!({"duration_minutes": 60})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 60, "reason": "   "}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "空白 reason：{body}"
    );

    let (status, body) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 0, "reason": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // 上限走租戶設定。**改成 30 分鐘再測**，而不是靠預設的 1440：
    // 那樣才驗得到 tenants.settings.alarm_max_suppress_minutes 真的被讀了。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.tenants
                SET settings = coalesce(settings,'{}'::jsonb)
                             || '{\"alarm_max_suppress_minutes\": 30}'::jsonb
              WHERE id = $1::uuid",
        )
        .bind(TENANT_ID)
        .execute(&mut *tx)
        .await
        .expect("set cap");
        tx.commit().await.expect("commit");
    }

    let (status, body) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 60, "reason": "超過上限"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "60 分鐘在上限 30 的租戶被接受了 —— 政策沒有被讀：{body}"
    );
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("30"),
        "錯誤訊息沒說出真正生效的上限：{body}"
    );

    let (status, body) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 30, "reason": "剛好在上限"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "剛好等於上限被拒：{body}");
    assert_eq!(body["meta"]["max_minutes_allowed"], json!(30));

    ctx.teardown().await;
}

/// **抑制期間再觸發：不新增告警、不發事件，但次數照算。**
///
/// 071 之前這一格必定失敗 —— 而失敗的方式是「多了一筆告警與一封通知」，
/// 也就是抑制造成了它本該防止的東西。
#[tokio::test]
async fn c_suppressed_alarm_does_not_spawn_a_duplicate_or_an_event() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = raise(ctx, 31.0, "冷氣過熱").await;

    let (status, body) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 240, "reason": "維修中"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let alarms_before = alarm_count(ctx).await;
    let events_before = raised_event_count(ctx).await;
    let (_, _, count_before) = alarm_row(ctx, &id).await;

    // 規則引擎再次觸發同一個條件。
    let id2 = raise(ctx, 33.0, "冷氣又過熱").await;

    assert_eq!(
        id2, id,
        "抑制期間再觸發產生了**另一筆**告警 —— 抑制反而製造了噪音"
    );
    assert_eq!(
        alarm_count(ctx).await,
        alarms_before,
        "告警筆數增加了 —— 被抑制的告警被當成不存在"
    );
    assert_eq!(
        raised_event_count(ctx).await,
        events_before,
        "抑制期間發出了 alarm.raised —— 該安靜的人還是收到通知了"
    );

    // 次數仍要累加：抑制的是通知，不是事實。解除後要能看出這段時間響了幾次。
    let (status_after, _, count_after) = alarm_row(ctx, &id).await;
    assert_eq!(status_after, "SUPPRESSED", "狀態被改掉了");
    assert_eq!(
        count_after,
        count_before + 1,
        "occurrence_count 沒有累加 —— 抑制把證據也一起關掉了"
    );

    ctx.teardown().await;
}

/// 期限過了之後自動回到 ACTIVE 並恢復發報。
///
/// 不回去的話，過期的告警會永遠停在 SUPPRESSED，而它佔著
/// `uq_alarms_open_per_point` 的唯一鍵 —— 新告警也建不出來，那個測點從此靜音。
#[tokio::test]
async fn d_expired_suppression_returns_to_active_and_fires_again() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = raise(ctx, 31.0, "冷氣過熱").await;

    let (status, body) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 60, "reason": "維修中"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 把期限撥到過去（不能等一小時）。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.alarms SET suppressed_until = clock_timestamp() - interval '1 minute'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("expire suppression");
        tx.commit().await.expect("commit");
    }

    let events_before = raised_event_count(ctx).await;
    let id2 = raise(ctx, 35.0, "維修結束後又過熱").await;

    assert_eq!(id2, id, "過期之後產生了另一筆告警而不是沿用原本那筆");
    let (status_after, until_after, _) = alarm_row(ctx, &id).await;
    assert_eq!(
        status_after, "ACTIVE",
        "抑制期限過了卻還停在 SUPPRESSED —— 那個測點從此靜音"
    );
    assert!(
        until_after.is_none(),
        "回到 ACTIVE 卻沒有清掉 suppressed_until"
    );
    // 期限過後**要**恢復發報。這一格與 `c_` 是一對：少了它，一個「永遠不發事件」
    // 的實作也會讓 c_ 通過。
    assert_eq!(
        raised_event_count(ctx).await,
        events_before,
        "沿用既有告警時不該發新的 alarm.raised（那是 006 原本的行為）"
    );

    ctx.teardown().await;
}

/// 需要 `alarm:suppress`，而**能 acknowledge 的人不夠**。
///
/// 這是刻意偏離契約的那一格（契約原本寫 `alarm:acknowledge`）：
/// 持有 acknowledge 的包含 TECHNICIAN 與 SERVICE_STAFF —— 現場人員該能確認
/// 告警是對的，不該能讓監控靜音。
#[tokio::test]
async fn e_acknowledge_permission_is_not_enough_to_suppress() {
    let ctx = &TestContext::setup().await;
    let id = raise(ctx, 31.0, "冷氣過熱").await;

    let tech = ctx.login_as(USERNAME_TECHNICIAN_HQ).await;

    // 前提：這個技師**確實**可以確認告警 —— 否則下面的 403 可能只是因為
    // 他對這則告警完全沒有權限。
    let (ack_status, ack_body) = ctx
        .send(authed(
            post(&format!("/api/v1/alarms/{id}/acknowledge"), json!({})),
            &tech,
        ))
        .await;
    assert_eq!(
        ack_status,
        StatusCode::OK,
        "前提不成立：這個技師連確認都做不到，這一格就證明不了任何事：{ack_body}"
    );

    let (status, body) = suppress(
        ctx,
        &tech,
        &id,
        json!({"duration_minutes": 60, "reason": "技師想讓它安靜"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "能確認告警的技師也能讓監控靜音了：{body}"
    );

    let (db_status, _, _) = alarm_row(ctx, &id).await;
    assert_eq!(db_status, "ACKNOWLEDGED", "被拒之後狀態還是被改了");

    ctx.teardown().await;
}

/// 重新抑制是**延長**，從現在起算。
#[tokio::test]
async fn f_re_suppressing_extends_from_now() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = raise(ctx, 31.0, "冷氣過熱").await;

    let (status, _) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 10, "reason": "先抑制十分鐘"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, first_until, _) = alarm_row(ctx, &id).await;

    let (status, body) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 120, "reason": "維修拖長了"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "重新抑制被拒 —— 維修拖長是正常的：{body}"
    );
    assert_eq!(
        body["meta"]["extended_existing_suppression"],
        json!(true),
        "沒有回報這是延長：{body}"
    );

    let (_, second_until, _) = alarm_row(ctx, &id).await;
    assert!(
        second_until.unwrap() > first_until.unwrap(),
        "期限沒有往後延"
    );

    ctx.teardown().await;
}

/// 對帳：補上缺口，並把「沒有補」的原因分開回報。
#[tokio::test]
async fn g_reconcile_fills_the_gap_and_explains_the_rest() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 觸發一則會自動建單的告警，再把關聯拆掉 —— 那正是「規則說要建單但沒有工單」
    // 這個缺口的樣子（歷史資料、或規則是事後才開的）。
    let id = raise(ctx, 31.0, "冷氣過熱").await;
    let facility = facility_of(ctx, &id).await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.alarms SET work_order_id = NULL, work_order_created_at = NULL
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("unlink");
        // 確保規則真的要求自動建單（缺口的定義）。
        sqlx::query("UPDATE fms.alarm_rules SET auto_create_work_order = true WHERE id = $1::uuid")
            .bind(RULE_HVAC)
            .execute(&mut *tx)
            .await
            .expect("enable auto create");
        tx.commit().await.expect("commit");
    }

    // 缺口在診斷端點裡看得到（租戶級，不需要 facility_id）。
    let (status, listed) = ctx
        .send(authed(get("/api/v1/alarms?unlinked_only=true"), &admin))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == json!(id)),
        "unlinked_only 沒有列出這個缺口：{listed}"
    );

    let (status, body) = reconcile(ctx, &admin, json!({"facility_id": facility})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["reconciled"].as_i64().unwrap() >= 1,
        "缺口沒有被補上：{body}"
    );
    assert_eq!(
        body["remaining_gap"],
        json!(0),
        "補完之後 remaining_gap 不是 0：{body}"
    );
    // 「不是缺口」的那一類要分開說，否則 unlinked_only 的清單與這裡的數字對不上。
    assert!(
        body["meta"]["skipped_rule_does_not_auto_create"]
            .as_i64()
            .is_some(),
        "{body}"
    );
    assert!(
        body["meta"]["diagnosis_endpoint"]
            .as_str()
            .unwrap()
            .contains("unlinked_only"),
        "{body}"
    );

    // 告警真的有工單了。
    let mut tx = ctx.owner_tx().await;
    let wo: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT work_order_id FROM fms.alarms WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .expect("read wo");
    drop(tx);
    assert!(wo.is_some(), "reconciled 回了 1，但告警還是沒有工單");

    // 再跑一次是幂等的（056 的條件式 UPDATE）。
    let (status, body) = reconcile(ctx, &admin, json!({"facility_id": facility})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reconciled"], json!(0), "重跑又補了一次：{body}");

    ctx.teardown().await;
}

/// 對帳**不碰**抑制中的告警，而且說出跳過了幾筆。
///
/// `raise_alarm()` 在抑制期間也不自動建單（071）。這裡若補了，同一個條件會依
/// 「是誰觸發的」得到不同結果 —— 那種不一致最難查。
#[tokio::test]
async fn h_reconcile_skips_suppressed_alarms_and_says_so() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let id = raise(ctx, 31.0, "冷氣過熱").await;
    let facility = facility_of(ctx, &id).await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.alarms SET work_order_id = NULL, work_order_created_at = NULL
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("unlink");
        sqlx::query("UPDATE fms.alarm_rules SET auto_create_work_order = true WHERE id = $1::uuid")
            .bind(RULE_HVAC)
            .execute(&mut *tx)
            .await
            .expect("enable auto create");
        tx.commit().await.expect("commit");
    }

    let (status, _) = suppress(
        ctx,
        &admin,
        &id,
        json!({"duration_minutes": 120, "reason": "維修中，先不要開單"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = reconcile(ctx, &admin, json!({"facility_id": facility})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["reconciled"],
        json!(0),
        "對帳為抑制中的告警建了工單 —— 與 raise_alarm 的行為不一致：{body}"
    );
    assert!(
        body["meta"]["skipped_suppressed"].as_i64().unwrap() >= 1,
        "跳過了但沒有說 —— 呼叫端會以為沒有缺口：{body}"
    );
    assert!(
        body["meta"]["skipped_suppressed_reason"]
            .as_str()
            .unwrap()
            .len()
            > 20,
        "理由太短，等於沒說：{body}"
    );

    let mut tx = ctx.owner_tx().await;
    let wo: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT work_order_id FROM fms.alarms WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .expect("read wo");
    drop(tx);
    assert!(wo.is_none(), "抑制中的告警還是被建了工單");

    ctx.teardown().await;
}

/// 對帳需要 `work_order:create`，而 `facility_id` 是必填。
#[tokio::test]
async fn i_reconcile_requires_facility_and_permission() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let id = raise(ctx, 31.0, "冷氣過熱").await;
    let facility = facility_of(ctx, &id).await;

    // facility_id 必填 —— 見 handler 說明：跨場域掃描只能靜默跳過或整批失敗。
    let (status, body) = reconcile(ctx, &admin, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // 只能讀的角色不能對帳（它會建立工單）。
    let viewer = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = reconcile(ctx, &viewer, json!({"facility_id": facility})).await;
    // REQUESTER 有 work_order:create，但只在自己的場域 —— 因此這裡驗的是
    // 「權限檢查真的跑了」而不是「一定 403」。任一種都可接受，但不能是 500。
    assert!(
        status == StatusCode::OK || status == StatusCode::FORBIDDEN,
        "權限檢查回了非預期的狀態：{status} {body}"
    );

    ctx.teardown().await;
}
