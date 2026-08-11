//! 遙測批次寫入（`/telemetry:batch-ingest`）+ 即時門檻評估（migration 057）。
//!
//! # 這一組守兩件事
//!
//! **`b_`：一批裡壞掉幾筆，好的那些要進得去。** 契約寫「逐筆處理，回應中
//! 列出失敗項目而不整批退回」。一個交易裡某一筆 SQL 失敗會讓整個交易
//! aborted，後面每一筆都拿到 `current transaction is aborted` ——
//! 少了 savepoint，「1000 筆有 3 筆點位打錯」會變成 997 筆好資料一起被丟掉。
//!
//! **`c_`：057 的行為驗證。** 那個 migration 的自我驗證只能做結構檢查
//! （它沒有租戶情境，看不到 `alarm_rules`；加平台情境又會真的建出告警
//! 留在資料庫裡）。行為在這裡驗，用 009 的三條規則各自的分支。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 009 的計量點。BATTERY_SOC 掛簡單門檻、FILTER_DP 掛持續型。
const POINT_SOC: &str = "a3000000-0000-4000-8000-000000000003";
const POINT_DP: &str = "a3000000-0000-4000-8000-000000000002";
const POINT_TEMP: &str = "a3000000-0000-4000-8000-000000000001"; // 沒有規則

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

async fn ingest(ctx: &TestContext, token: &str, readings: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        post(
            "/api/v1/telemetry:batch-ingest",
            json!({ "readings": readings }),
        ),
        token,
    ))
    .await
}

/// 正常路徑：寫得進去、讀數真的落地。
#[tokio::test]
async fn a_readings_are_written() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ingest(
        ctx,
        &admin,
        json!([
            { "telemetry_point_id": POINT_TEMP, "observed_at": now(), "value_num": 22.5 },
            { "telemetry_point_id": POINT_TEMP, "observed_at": now(), "value_num": 23.0 }
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["accepted"], 2);
    assert_eq!(body["rejected"], 0);
    assert_eq!(body["errors"].as_array().map(|a| a.len()), Some(0));

    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.telemetry_readings WHERE telemetry_point_id = $1::uuid",
    )
    .bind(POINT_TEMP)
    .fetch_one(&mut *tx)
    .await
    .expect("查讀數");
    tx.commit().await.expect("commit");
    assert!(n >= 2, "讀數要真的落地，不是只回一個好看的數字：{n}");

    ctx.teardown().await;
}

/// **壞掉的那幾筆不能拖垮整批。** 這一組最重要的一格。
///
/// 少了逐筆 savepoint，第一筆失敗之後整個交易就 aborted，
/// 後面每一筆都會拿到 `current transaction is aborted` —— 好資料全部陪葬。
#[tokio::test]
async fn b_bad_items_do_not_take_the_good_ones_down() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ingest(
        ctx,
        &admin,
        json!([
            // 壞的排在**最前面**：若沒有 savepoint，後面兩筆一定失敗。
            { "telemetry_point_id": "00000000-0000-4000-8000-0000000000ff",
              "observed_at": now(), "value_num": 1.0 },
            { "telemetry_point_id": POINT_TEMP, "observed_at": now(), "value_num": 21.0 },
            { "observed_at": now(), "value_num": 2.0 },
            { "telemetry_point_id": POINT_TEMP, "observed_at": now(), "value_num": 22.0 }
        ]),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED, "整批不該退回：{body}");
    assert_eq!(
        body["accepted"], 2,
        "壞的排在最前面，後面的好資料仍要進得去（少了 savepoint 這裡會是 0）：{body}"
    );
    assert_eq!(body["rejected"], 2);

    let errs = body["errors"].as_array().cloned().unwrap_or_default();
    assert_eq!(errs.len(), 2, "{body}");
    assert_eq!(errs[0]["index"], 0, "錯誤要指出是第幾筆：{errs:?}");
    assert_eq!(errs[0]["code"], "POINT_NOT_FOUND");
    assert_eq!(errs[1]["index"], 2);
    assert_eq!(errs[1]["code"], "MISSING_POINT");

    ctx.teardown().await;
}

/// **057 的行為驗證。** 三條規則、三個分支。
#[tokio::test]
async fn c_threshold_rules_fire_and_the_unevaluable_ones_are_counted() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // (1) 簡單門檻：BATTERY_SOC 的規則是 {"op":"<","value":40}。
    let (status, low) = ingest(
        ctx,
        &admin,
        json!([{ "telemetry_point_id": POINT_SOC, "observed_at": now(), "value_num": 12.0 }]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{low}");
    assert_eq!(
        low["alarms_raised"], 1,
        "SOC 12% 低於門檻 40% 要觸發 —— 在 057 之前這裡永遠是 0：{low}"
    );

    // 告警真的建出來了，而且看得到（不是只回一個數字）。
    let (_, alarms) = ctx
        .send(authed(
            Request::builder()
                .uri("/api/v1/alarms?status=ACTIVE")
                .body(Body::empty())
                .unwrap(),
            &admin,
        ))
        .await;
    assert!(
        alarms["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["rule_code"] == "UPS_SOC_LOW"),
        "觸發的告警要查得到：{alarms}"
    );

    // (2) 反面：門檻之上不觸發。少了這一格，「一律觸發」也會讓 (1) 通過。
    let (_, high) = ingest(
        ctx,
        &admin,
        json!([{ "telemetry_point_id": POINT_SOC, "observed_at": now(), "value_num": 88.0 }]),
    )
    .await;
    assert_eq!(high["alarms_raised"], 0, "SOC 88% 不該觸發：{high}");

    // (3) 持續型：FILTER_DP 的規則帶 for_seconds:600 —— 不觸發，但**要被計數**。
    let (_, sustained) = ingest(
        ctx,
        &admin,
        json!([{ "telemetry_point_id": POINT_DP, "observed_at": now(), "value_num": 9999.0 }]),
    )
    .await;
    assert_eq!(
        sustained["alarms_raised"], 0,
        "持續型規則不該在單筆讀數上觸發：{sustained}"
    );
    assert!(
        sustained["meta"]["rules_skipped_not_evaluable_per_reading"]
            .as_i64()
            .unwrap_or(0)
            >= 1,
        "跳過的規則要被計數 —— 「設定了但永遠不會響」必須看得見：{sustained}"
    );

    ctx.teardown().await;
}

/// 上限、空批次、權限。
#[tokio::test]
async fn d_limits_and_permission() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, empty) = ingest(ctx, &admin, json!([])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{empty}");

    let too_many: Vec<Value> = (0..1001)
        .map(
            |_| json!({ "telemetry_point_id": POINT_TEMP, "observed_at": now(), "value_num": 1.0 }),
        )
        .collect();
    let (status, over) = ingest(ctx, &admin, json!(too_many)).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "上限要在端點擋，不是讓資料庫慢慢吃：{over}"
    );
    assert!(
        over["detail"].as_str().unwrap_or_default().contains("1000"),
        "訊息要說出上限是多少：{over}"
    );

    // REQUESTER 沒有 telemetry:ingest。
    let user = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, denied) = ingest(
        ctx,
        &user,
        json!([{ "telemetry_point_id": POINT_TEMP, "observed_at": now(), "value_num": 1.0 }]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");

    ctx.teardown().await;
}
