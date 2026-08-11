//! 工單執行面（`/work-orders/{id}/tasks`、`/labor`、`/parts`）。
//!
//! # 這三支之前，技師在現場做的事只有一件記錄得下來
//!
//! `PATCH .../tasks/{taskId}`（回填檢查結果）早就有了，而**登工時與領備品
//! 沒有任何端點** —— 表在、`repo::record_labor` 與 `repo::record_part_usage`
//! 都在（而且有 handler 呼叫，不是孤兒），只缺 HTTP 入口。
//!
//! 而 `/reports/service-volume`（可 chargeback）與 `asset-reliability`
//! 需要的正是這些明細列。
//!
//! # `b_` 是這個檔案最重要的一條
//!
//! **沒有費率時 `cost` 必須是 null，不是 0。** schema 裡沒有任何費率來源，
//! 所以「這筆工時的成本未知」是常態。0 會讓它安靜地變成「免費」，
//! 而那個數字會被加總進 `work_orders.labor_cost` 並出現在 chargeback 上。
//!
//! # `e_` 釘住「兩種缺料是不同的事」
//!
//! 該場域沒有庫存列 → 允許（廠商當場帶料是真實情境）。
//! 有庫存列但不足 → 409。把兩者混成同一個回應會讓系統無法記錄真正發生的事。

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

/// 總部有庫存的第一個料件，回傳 `(part_id, quantity_on_hand)`。
async fn stocked_part(ctx: &TestContext) -> (uuid::Uuid, f64) {
    let mut tx = ctx.owner_tx().await;
    let row: (uuid::Uuid, f64) = sqlx::query_as(
        "SELECT s.part_id, s.quantity_on_hand::float8
           FROM fms.part_stock s
          WHERE s.facility_id = $1::uuid AND s.quantity_on_hand > 0
          ORDER BY s.quantity_on_hand DESC LIMIT 1",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("018 的備品種子該有總部庫存");
    tx.commit().await.expect("commit");
    row
}

/// **列出檢查項，並回報還有幾個必要項目沒填。**
#[tokio::test]
async fn a_tasks_are_listable_with_the_outstanding_count() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let wo = ctx.seed_work_order(FACILITY_HQ, "檢查表測試").await;

    // 掛兩個檢查項：一個必要、一個非必要。
    ctx.seed_work_order_task(wo, 1, "必要項", true).await;
    ctx.seed_work_order_task(wo, 2, "選填項", false).await;

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/work-orders/{wo}/tasks")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        body["meta"]["required_outstanding"], 1,
        "必要且未填的只有一個 —— 這個數字就是「還不能結案」的答案：{body}"
    );

    // 不存在的工單 → 404，不是空清單。
    let (status, _) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/work-orders/{}/tasks",
                uuid::Uuid::new_v4()
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// **沒有費率時 `cost` 是 null，不是 0。** 這個檔案最重要的一條。
#[tokio::test]
async fn b_missing_rate_yields_null_cost_not_zero() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let wo = ctx.seed_work_order(FACILITY_HQ, "工時成本測試").await;

    // 不給費率。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/labor"),
                json!({ "minutes": 90 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["minutes"], 90);
    assert_eq!(
        body["cost"],
        Value::Null,
        "**沒有費率時成本未知，不是 0** —— 0 會讓它變成「免費」並壓低 chargeback：{body}"
    );
    assert_eq!(
        body["work_order_labor_minutes"], 90,
        "分鐘數要 rollup 到工單上"
    );

    // 給費率 → cost = 90/60 × 800 = 1200。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/labor"),
                json!({ "minutes": 90, "hourly_rate": 800 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let cost = body["cost"].as_f64().expect("cost");
    assert!(
        (cost - 1200.0).abs() < 0.01,
        "90 分鐘 × 800/時 = 1200；實際 {cost}"
    );
    // 工單的 labor_cost 只加總有成本的那一筆 —— null 不會被當 0 加進去。
    let total = body["work_order_labor_cost"].as_f64().expect("total");
    assert!(
        (total - 1200.0).abs() < 0.01,
        "工單成本該只有 1200（另一筆是 null）；實際 {total}"
    );
    assert_eq!(body["work_order_labor_minutes"], 180);

    ctx.teardown().await;
}

/// `minutes` 與 `started_at`/`ended_at` 不一致時必須拒絕，不能挑一個。
#[tokio::test]
async fn c_inconsistent_time_inputs_are_rejected() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let wo = ctx.seed_work_order(FACILITY_HQ, "時間一致性").await;

    let start = chrono::Utc::now() - chrono::Duration::hours(2);
    let end = start + chrono::Duration::minutes(60);
    let fmt =
        |t: chrono::DateTime<chrono::Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // 區間是 60 分鐘，卻聲稱 90 → 422。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/labor"),
                json!({ "started_at": fmt(start), "ended_at": fmt(end), "minutes": 90 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or("").contains("不一致"),
        "訊息要說出是哪兩個對不上：{body}"
    );

    // 一致 → 201，而分鐘數由區間推導。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/labor"),
                json!({ "started_at": fmt(start), "ended_at": fmt(end) }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["minutes"], 60);

    // 結束早於開始 → 422。
    let (status, _) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/labor"),
                json!({ "started_at": fmt(end), "ended_at": fmt(start) }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 兩者都沒有 → 422。
    let (status, _) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/labor"),
                json!({ "hourly_rate": 500 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// 替**別人**登記工時需要 `work_order:assign`。
///
/// 少了這道檢查，任何技師都能把工時掛到同事身上 ——
/// 而那會影響團隊負載與 chargeback 的歸屬。
#[tokio::test]
async fn d_logging_labor_for_someone_else_needs_assign() {
    let ctx = TestContext::setup().await;
    let wo = ctx.seed_work_order(FACILITY_HQ, "代登工時").await;

    // tech.liu 是總部的 TECHNICIAN：有 execute，沒有 assign。
    let tech = ctx.login_as(USERNAME_TECHNICIAN_HQ).await;

    // 記在自己身上 → 201。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/labor"),
                json!({ "minutes": 30 }),
            ),
            &tech,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "技師該登記得了自己的工時：{body}"
    );

    // 掛到別人身上 → 403。
    let (status, _) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/labor"),
                json!({ "minutes": 30, "user_id": ADMIN_USER_ID }),
            ),
            &tech,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "沒有 work_order:assign 不該能把工時掛到同事身上"
    );

    ctx.teardown().await;
}

/// **兩種缺料是不同的事。** 有庫存但不足 → 409；沒有庫存列 → 允許。
#[tokio::test]
async fn e_the_two_kinds_of_missing_stock_are_different() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let wo = ctx.seed_work_order(FACILITY_HQ, "備品領用").await;
    let (part, on_hand) = stocked_part(&ctx).await;

    // 正常領用 1 個 → 201，庫存減 1，成本從 parts.unit_cost 快照。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/parts"),
                json!({ "part_id": part, "quantity": 1 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let after = body["stock_on_hand_after"].as_f64().expect("stock");
    assert!(
        (after - (on_hand - 1.0)).abs() < 0.001,
        "庫存該從 {on_hand} 減到 {}；實際 {after}",
        on_hand - 1.0
    );
    assert!(
        body["total_cost"].as_f64().is_some(),
        "成本該從 parts.unit_cost 快照下來：{body}"
    );

    // 領超過庫存 → 409（有庫存列但不足）。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/parts"),
                json!({ "part_id": part, "quantity": on_hand + 100.0 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // 沒有庫存列的料件 → 允許（廠商當場帶料），而 stock_on_hand_after 是 null。
    let mut tx = ctx.owner_tx().await;
    let untracked: uuid::Uuid = sqlx::query_scalar(
        "SELECT p.id FROM fms.parts p
          WHERE NOT EXISTS (SELECT 1 FROM fms.part_stock s
                             WHERE s.part_id = p.id AND s.facility_id = $1::uuid)
          LIMIT 1",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("該有一個總部沒庫存的料件");
    tx.commit().await.expect("commit");

    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/parts"),
                json!({ "part_id": untracked, "quantity": 2 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "沒有庫存列該允許 —— 廠商當場帶料是真實情境，拒絕它會讓系統無法記錄真正發生的事：{body}"
    );
    assert_eq!(
        body["stock_on_hand_after"],
        Value::Null,
        "null = 沒有庫存列，不是 0 庫存：{body}"
    );

    ctx.teardown().await;
}

/// 領用會 rollup 到 `work_orders.parts_cost`。
#[tokio::test]
async fn f_part_usage_rolls_up_to_the_work_order() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let wo = ctx.seed_work_order(FACILITY_HQ, "成本彙總").await;
    let (part, _) = stocked_part(&ctx).await;

    let (_s, first) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/parts"),
                json!({ "part_id": part, "quantity": 1 }),
            ),
            &token,
        ))
        .await;
    let one = first["work_order_parts_cost"].as_f64().expect("cost");

    let (_s, second) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/work-orders/{wo}/parts"),
                json!({ "part_id": part, "quantity": 1 }),
            ),
            &token,
        ))
        .await;
    let two = second["work_order_parts_cost"].as_f64().expect("cost");

    assert!(
        two > one,
        "第二次領用之後工單的 parts_cost 該變大（rollup 是重算而非累加，\
         但兩筆明細的總和仍該增加）；{one} → {two}"
    );

    ctx.teardown().await;
}

/// 邊界輸入：分鐘數與數量的範圍。
#[tokio::test]
async fn g_out_of_range_inputs_are_rejected() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let wo = ctx.seed_work_order(FACILITY_HQ, "邊界").await;
    let (part, _) = stocked_part(&ctx).await;

    for bad in [0, -30, 24 * 60 + 1] {
        let (status, _) = ctx
            .send(authed(
                json_req(
                    "POST",
                    &format!("/api/v1/work-orders/{wo}/labor"),
                    json!({ "minutes": bad }),
                ),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "minutes={bad} 該是 422"
        );
    }

    for bad in [0.0, -1.0] {
        let (status, _) = ctx
            .send(authed(
                json_req(
                    "POST",
                    &format!("/api/v1/work-orders/{wo}/parts"),
                    json!({ "part_id": part, "quantity": bad }),
                ),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "quantity={bad} 該是 422"
        );
    }

    ctx.teardown().await;
}
