//! PM 合規鏈（migration 063 + `/maintenance-occurrences` + `/reports/pm-compliance`）。
//!
//! # `a_` 是這個檔案存在的主要理由
//!
//! 在 063 之前 `maintenance_occurrences` 的寫入者只有兩個：`claim_occurrence`
//! （`PLANNED`）與 `mark_generated`（`GENERATED`）。**沒有任何東西寫
//! COMPLETED，也沒有任何東西設 `completed_at`。**
//!
//! 所以「PM 準時完成率」若照原樣寫，會對每個租戶永遠回 0% ——
//! 而它看起來會像一支正常的報表。`a_` 把「工單完工 → occurrence 終結」
//! 這條鏈釘住。
//!
//! # `b_` 釘住反方向
//!
//! `sql/044` 在工單重開時會清掉 `completed_at`。063 的觸發器綁在那一欄上，
//! 所以 occurrence 會自動退回 `GENERATED` —— 一件重做中的保養不該被算成
//! 已完成。綁狀態名稱（`CLOSED`）做不到這件事。
//!
//! # `d_` 是分母的三個定義
//!
//! 主分母只含**已經有結果**的期次。`in_window`（還有機會）與 `skipped`
//! （被排除）各自具名。兩種極端都是謊：把 skip 算進分母，「全部跳過」
//! 得到 0%；完全不計算，得到 100%。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn json_req(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 佈置一筆 occurrence（含其計畫），回傳 `(plan_id, occurrence_id)`。
///
/// `scheduled_days_ago`：排定時刻在幾天前（負數 = 未來）。
/// `grace_days`：計畫的完工容許窗。
async fn seed_occurrence(
    ctx: &TestContext,
    code: &str,
    scheduled_days_ago: i32,
    grace_days: i32,
) -> (uuid::Uuid, uuid::Uuid) {
    let plan = ctx
        .seed_maintenance_plan(FACILITY_HQ, code, grace_days)
        .await;

    let mut tx = ctx.owner_tx().await;
    let occ: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.maintenance_occurrences
           (tenant_id, plan_id, scheduled_for, status)
         VALUES ($1::uuid, $2::uuid,
                 now() - make_interval(days => $3::int), 'PLANNED')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(plan)
    .bind(scheduled_days_ago)
    .fetch_one(&mut *tx)
    .await
    .expect("建 occurrence");

    tx.commit().await.expect("commit");
    (plan, occ)
}

/// 建一張掛著這筆 occurrence 的工單，回傳 work_order_id。
async fn work_order_for(ctx: &TestContext, occurrence: uuid::Uuid) -> uuid::Uuid {
    let id = ctx.seed_work_order(FACILITY_HQ, "PM 工單").await;

    let mut tx = ctx.owner_tx().await;
    // helper 建的是 CORRECTIVE／MANUAL 的通用工單；PM 的兩個欄位在這裡補上
    // —— `maintenance_occurrence_id` 是 063 觸發器的觸發條件。
    sqlx::query(
        "UPDATE fms.work_orders
            SET work_order_type = 'MAINTENANCE', source = 'PM_PLAN',
                maintenance_occurrence_id = $2::uuid
          WHERE id = $1::uuid",
    )
    .bind(id)
    .bind(occurrence)
    .execute(&mut *tx)
    .await
    .expect("掛上 occurrence");
    sqlx::query(
        "UPDATE fms.maintenance_occurrences
            SET status = 'GENERATED', work_order_id = $2::uuid,
                generated_at = clock_timestamp()
          WHERE id = $1::uuid",
    )
    .bind(occurrence)
    .bind(id)
    .execute(&mut *tx)
    .await
    .expect("標記 generated");
    tx.commit().await.expect("commit");
    id
}

/// 設定工單的 `completed_at`（`None` = 清掉，模擬 044 的重開）。
async fn set_wo_completed(ctx: &TestContext, wo: uuid::Uuid, days_after_schedule: Option<i32>) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.work_orders w
            SET completed_at = CASE WHEN $2::int IS NULL THEN NULL
                                    ELSE (SELECT o.scheduled_for
                                            + make_interval(days => $2::int)
                                            FROM fms.maintenance_occurrences o
                                           WHERE o.id = w.maintenance_occurrence_id) END
          WHERE w.id = $1::uuid",
    )
    .bind(wo)
    .bind(days_after_schedule)
    .execute(&mut *tx)
    .await
    .expect("設定完工時刻");
    tx.commit().await.expect("commit");
}

async fn occurrence_state(ctx: &TestContext, occ: uuid::Uuid) -> (String, Option<String>) {
    let mut tx = ctx.owner_tx().await;
    let row: (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT status, completed_at FROM fms.maintenance_occurrences WHERE id = $1::uuid",
    )
    .bind(occ)
    .fetch_one(&mut *tx)
    .await
    .expect("讀 occurrence");
    tx.commit().await.expect("commit");
    (row.0, row.1.map(|t| t.to_rfc3339()))
}

/// **工單完工 → occurrence 終結。** 063 的鏈，這個檔案的主要理由。
///
/// 在 063 之前這條會失敗，而失敗的方式是「狀態停在 GENERATED」——
/// 於是合規率永遠是 0%，看起來像一支正常的報表。
#[tokio::test]
async fn a_completing_the_work_order_closes_the_occurrence() {
    let ctx = TestContext::setup().await;
    let (_plan, occ) = seed_occurrence(&ctx, "PM_CHAIN", 10, 0).await;

    let (status, completed) = occurrence_state(&ctx, occ).await;
    assert_eq!(status, "PLANNED");
    assert_eq!(completed, None);

    let wo = work_order_for(&ctx, occ).await;
    let (status, _) = occurrence_state(&ctx, occ).await;
    assert_eq!(status, "GENERATED", "開了工單之後該是 GENERATED");

    // 工單完工（排定日當天）。
    set_wo_completed(&ctx, wo, Some(0)).await;
    let (status, completed) = occurrence_state(&ctx, occ).await;
    assert_eq!(
        status, "COMPLETED",
        "**鏈斷了** —— 工單完工之後 occurrence 該終結，否則合規率永遠是 0%"
    );
    assert!(
        completed.is_some(),
        "completed_at 必須被設 —— 準時判定要用它"
    );

    ctx.teardown().await;
}

/// **重開工單 → occurrence 退回。** 044 清掉 `completed_at`，063 跟著退回。
///
/// 綁狀態名稱（`CLOSED`）的實作抓不到這件事。
#[tokio::test]
async fn b_reopening_the_work_order_reverts_the_occurrence() {
    let ctx = TestContext::setup().await;
    let (_plan, occ) = seed_occurrence(&ctx, "PM_REOPEN", 10, 0).await;
    let wo = work_order_for(&ctx, occ).await;

    set_wo_completed(&ctx, wo, Some(0)).await;
    assert_eq!(occurrence_state(&ctx, occ).await.0, "COMPLETED");

    // 044 的重開：清掉 completed_at。
    set_wo_completed(&ctx, wo, None).await;
    let (status, completed) = occurrence_state(&ctx, occ).await;
    assert_eq!(
        status, "GENERATED",
        "重開之後該退回 GENERATED —— 留著 COMPLETED 會把一件重做中的事算成已完成"
    );
    assert_eq!(completed, None, "completed_at 也要清掉");

    ctx.teardown().await;
}

/// 容許窗**來自計畫**，而 `is_late` / `is_missed` 跟著它變。
#[tokio::test]
async fn c_the_grace_window_comes_from_the_plan() {
    let ctx = TestContext::setup().await;
    // 排定 10 天前、容許 0 天 → 3 天後完工 = 逾時。
    let (plan, occ) = seed_occurrence(&ctx, "PM_GRACE", 10, 0).await;
    let wo = work_order_for(&ctx, occ).await;
    set_wo_completed(&ctx, wo, Some(3)).await;

    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/maintenance-occurrences?plan_id={plan}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = &body["data"][0];
    assert_eq!(row["grace_days"], 0, "容許窗要回傳，否則無法解釋判定");
    assert_eq!(row["is_late"], true, "容許 0 天而 3 天後完工 = 逾時：{row}");
    assert_eq!(row["is_missed"], false, "已完成的不是漏做");
    let days_late = row["days_late"].as_f64().expect("days_late");
    assert!(
        (days_late - 3.0).abs() < 0.01,
        "逾時 3 天；實際 {days_late}"
    );

    // 把容許窗放寬到 7 天 → 同一筆變準時。證明判定真的讀計畫的欄位。
    let (status, _) = ctx
        .send(authed(
            json_req(
                "PATCH",
                &format!("/api/v1/maintenance-plans/{plan}"),
                json!({ "completion_grace_days": 7 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (_s, body) = ctx
        .send(authed(
            get(&format!("/api/v1/maintenance-occurrences?plan_id={plan}")),
            &token,
        ))
        .await;
    let row = &body["data"][0];
    assert_eq!(row["grace_days"], 7);
    assert_eq!(
        row["is_late"], false,
        "容許窗放寬後同一筆不該是逾時 —— 否則容許窗沒有被讀：{row}"
    );

    ctx.teardown().await;
}

/// **三個分母是分開的。** 這是報表最重要的性質。
#[tokio::test]
async fn d_the_denominators_are_separate() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    // 準時一筆。
    let (_p1, o1) = seed_occurrence(&ctx, "PM_D_ONTIME", 10, 0).await;
    let w1 = work_order_for(&ctx, o1).await;
    set_wo_completed(&ctx, w1, Some(0)).await;
    // 逾時一筆。
    let (_p2, o2) = seed_occurrence(&ctx, "PM_D_LATE", 10, 0).await;
    let w2 = work_order_for(&ctx, o2).await;
    set_wo_completed(&ctx, w2, Some(5)).await;
    // 漏做一筆（排定 10 天前、沒完成）。
    let (_p3, _o3) = seed_occurrence(&ctx, "PM_D_MISSED", 10, 0).await;
    // 還在窗內一筆（排定在**未來**）。
    let (_p4, _o4) = seed_occurrence(&ctx, "PM_D_WINDOW", -5, 0).await;
    // 被跳過一筆。
    let (_p5, o5) = seed_occurrence(&ctx, "PM_D_SKIP", 10, 0).await;
    let (status, _) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/maintenance-occurrences/{o5}/skip"),
                json!({ "reason": "設備已拆除" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let from = (chrono::Utc::now() - chrono::Duration::days(30))
        .date_naive()
        .to_string();
    let to = (chrono::Utc::now() + chrono::Duration::days(30))
        .date_naive()
        .to_string();
    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/pm-compliance?group_by=none&from={from}&to={to}"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let r = &body["data"][0];

    assert_eq!(r["completed_on_time"], 1, "{r}");
    assert_eq!(r["completed_late"], 1, "{r}");
    assert_eq!(r["missed"], 1, "{r}");
    assert_eq!(
        r["scheduled_total"], 3,
        "主分母只含已有結果的三筆（準時／逾時／漏做）；實際 {r}"
    );
    assert_eq!(
        r["excluded_in_window"], 1,
        "未來的那筆不進分母 —— 它還有機會"
    );
    assert_eq!(r["excluded_skipped"], 1, "跳過的不進分母");
    assert_eq!(
        r["skip_reasons"]["設備已拆除"], 1,
        "理由分佈是「全部跳過卻 100%」時唯一能解釋的東西：{r}"
    );
    // 1 / 3
    let rate = r["on_time_rate"].as_f64().expect("on_time_rate");
    assert!(
        (rate - 1.0 / 3.0).abs() < 0.001,
        "準時率該是 1/3；實際 {rate}"
    );
    assert_eq!(
        body["meta"]["grace_source"],
        "maintenance_plans.completion_grace_days"
    );
    assert_eq!(body["meta"]["missed_is_derived"], true);

    ctx.teardown().await;
}

/// 分母為 0 時 `on_time_rate` 是 **null 而不是 0** ——
/// 「沒有任何期次」與「全部沒做」是完全不同的兩件事。
#[tokio::test]
async fn e_an_empty_denominator_is_null_not_zero() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    // 只有一筆在未來（不進分母）。
    seed_occurrence(&ctx, "PM_E_ONLY_FUTURE", -5, 0).await;

    let from = (chrono::Utc::now() - chrono::Duration::days(1))
        .date_naive()
        .to_string();
    let to = (chrono::Utc::now() + chrono::Duration::days(30))
        .date_naive()
        .to_string();
    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/pm-compliance?group_by=none&from={from}&to={to}"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let r = &body["data"][0];
    assert_eq!(r["scheduled_total"], 0);
    assert_eq!(
        r["on_time_rate"],
        Value::Null,
        "分母 0 時該是 null —— 回 0 會看起來像「全部沒做」：{r}"
    );

    ctx.teardown().await;
}

/// `missed_only` 找得到漏做的；用 `status=MISSED` 問會是空的。
#[tokio::test]
async fn f_missed_is_derived_not_stored() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (plan, _o) = seed_occurrence(&ctx, "PM_F_MISSED", 10, 0).await;

    let (_s, body) = ctx
        .send(authed(
            get("/api/v1/maintenance-occurrences?missed_only=true"),
            &token,
        ))
        .await;
    let codes: Vec<&str> = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|r| r["plan_code"].as_str().unwrap_or(""))
        .collect();
    assert!(codes.contains(&"PM_F_MISSED"), "{codes:?}");

    // 對照：問儲存的狀態會漏掉它 —— 沒有人寫 MISSED。
    let (_s, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/maintenance-occurrences?plan_id={plan}&status=MISSED"
            )),
            &token,
        ))
        .await;
    assert_eq!(
        body["data"].as_array().map(Vec::len),
        Some(0),
        "這是對照組：status=MISSED 問不到，所以那不是正確的問法"
    );

    ctx.teardown().await;
}

/// `skip` 的三個規則：理由必填、已完成的擋下、狀態衝突分開回報。
#[tokio::test]
async fn g_skip_requires_a_reason_and_refuses_completed_work() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    let (_p, occ) = seed_occurrence(&ctx, "PM_G_SKIP", 5, 0).await;

    // 沒有理由 → 422。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/maintenance-occurrences/{occ}/skip"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or("").contains("分母"),
        "訊息要說出後果（會從分母移除）：{body}"
    );

    // 已完成的不能跳過 → 409。
    let (_p2, occ2) = seed_occurrence(&ctx, "PM_G_DONE", 5, 0).await;
    let wo = work_order_for(&ctx, occ2).await;
    set_wo_completed(&ctx, wo, Some(0)).await;
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/maintenance-occurrences/{occ2}/skip"),
                json!({ "reason": "想調整數字" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or("").contains("已經完成"),
        "「已完成」不能看起來像「不存在」：{body}"
    );

    // 不存在的 → 404（與上面那個 409 分開）。
    let (status, _) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!(
                    "/api/v1/maintenance-occurrences/{}/skip",
                    uuid::Uuid::new_v4()
                ),
                json!({ "reason": "x" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// `POST /maintenance-templates` 會擋下空的 checklist 與不存在的技能代碼。
#[tokio::test]
async fn h_templates_reject_declarations_nobody_can_check() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    // 空 checklist → 422。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/maintenance-templates",
                json!({ "code": "T_EMPTY", "name": "空清單", "checklist": [] }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // 不存在的技能代碼 → 422，而且訊息點名它。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/maintenance-templates",
                json!({
                    "code": "T_GHOST", "name": "幽靈技能",
                    "checklist": [{ "title": "檢查" }],
                    "required_skill_codes": ["NO_SUCH_SKILL"]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["detail"]
        .as_str()
        .unwrap_or("")
        .contains("NO_SUCH_SKILL"));

    // 存在的技能（055 的平台目錄）→ 201。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/maintenance-templates",
                json!({
                    "code": "T_OK", "name": "正常範本",
                    "checklist": [{ "title": "檢查絕緣" }],
                    "required_skill_codes": ["ELECTRICAL"]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["plan_count"], 0, "新範本還沒有計畫在用");

    ctx.teardown().await;
}
