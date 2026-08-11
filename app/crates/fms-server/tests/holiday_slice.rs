//! 假日與補班日的維護（`/holiday-calendars`、migration 040）。
//!
//! 038 讓行事曆決定 SLA 期限，但只有 migration 能寫它 —— 一張只有工程師
//! 能填的假日表，等於把每年的行事曆變成一次部署。
//!
//! 第一個測試走完整條路：管理者加一個假日 → 之後開立的工單期限跳過那一天。
//! 那是這些端點存在的全部理由。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const FACILITY_CINEMA: &str = "cccccccc-0000-4000-8000-000000000002";

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

async fn create(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request("POST", "/api/v1/holiday-calendars", body),
        token,
    ))
    .await
}

async fn patch(ctx: &TestContext, token: &str, id: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request("PATCH", &format!("/api/v1/holiday-calendars/{id}"), body),
        token,
    ))
    .await
}

/// `fms.add_business_minutes` 對總部的結果，台北時間字串。
///
/// 2026-08-08 是週六（總部 09:00–17:00）、08-09 週日休、08-10 週一（08:00–21:00）。
async fn due_from_saturday(ctx: &TestContext) -> Option<String> {
    let mut tx = ctx.owner_tx().await;
    sqlx::query_scalar(
        "SELECT to_char(
                  fms.add_business_minutes($1::uuid, '2026-08-08 16:00+08'::timestamptz, 120)
                    AT TIME ZONE 'Asia/Taipei', 'YYYY-MM-DD HH24:MI')",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("add_business_minutes")
}

// =============================================================================
// 這些端點存在的理由
// =============================================================================

/// **本檔最重要的測試**：管理者透過 API 加一個假日，期限就跳過那一天。
#[tokio::test]
async fn adding_a_holiday_through_the_api_moves_the_deadline() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    assert_eq!(
        due_from_saturday(ctx).await.as_deref(),
        Some("2026-08-10 09:00"),
        "前提：週六剩 60 分、週日休、週一 08:00 起再 60 分"
    );

    let (status, holiday) = create(
        ctx,
        &token,
        json!({ "holiday_date": "2026-08-10", "name": "中元節" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{holiday}");
    assert_eq!(holiday["is_working_day"], false, "{holiday}");
    assert!(
        holiday["facility_id"].is_null(),
        "預設是租戶通用：{holiday}"
    );

    assert_eq!(
        due_from_saturday(ctx).await.as_deref(),
        Some("2026-08-11 09:00"),
        "週一放假 → 順延到週二"
    );

    // 刪掉之後恢復 —— 行事曆是真刪除，沒有任何東西參照它。
    let id = holiday["id"].as_str().expect("id");
    let (status, _) = ctx
        .send(authed(
            del(&format!("/api/v1/holiday-calendars/{id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        due_from_saturday(ctx).await.as_deref(),
        Some("2026-08-10 09:00")
    );

    ctx.teardown().await;
}

/// 改行事曆不影響已經開立的工單。
#[tokio::test]
async fn changing_the_calendar_does_not_move_existing_targets() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": "20000000-0000-4000-8000-000000000002",
                    "title": "行事曆快照",
                    "priority": "MEDIUM"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    let before = wo["resolution_due_at"].clone();
    assert!(!before.is_null());

    // 把往後兩週全部設成假日 —— 若期限不是快照，它會被推得很遠。
    for day in 1..=14 {
        let date = (chrono::Utc::now().date_naive() + chrono::Duration::days(day)).to_string();
        let (status, body) = create(
            ctx,
            &token,
            json!({ "holiday_date": date, "name": format!("測試假日 {day}") }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let id = wo["id"].as_str().expect("id");
    let (_, refetched) = ctx
        .send(authed(get(&format!("/api/v1/work-orders/{id}")), &token))
        .await;
    assert_eq!(
        refetched["resolution_due_at"], before,
        "期限在開單時就算成絕對時刻，行事曆變更不該移動它：{refetched}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 補班日沉默失效的組合
// =============================================================================

/// 補班日不給 `windows` 且該星期沒有常規班表 → 422。
///
/// 038 的 `business_windows` 對那個組合會沿用該星期的班表，而總部週日
/// 沒有班表 → 那一天可用 0 分鐘，整筆設定**沒有作用而且沒有任何提示**。
///
/// 這個檢查不能放在資料庫的 CHECK 裡：它要跨表看場域的 `operating_hours`，
/// 而 CHECK 不能含子查詢。因此在 handler 擋，並把理由說出來。
#[tokio::test]
async fn a_make_up_day_without_windows_is_rejected() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 2026-08-09 是週日，總部沒有 'sun' 班表。
    let (status, body) = create(
        ctx,
        &token,
        json!({
            "facility_id": FACILITY_HQ,
            "holiday_date": "2026-08-09",
            "name": "補班（沒給時段）",
            "is_working_day": true
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "沿用班表會讓這一天可用 0 分鐘 —— 一筆沉默失效的設定：{body}"
    );
    assert_eq!(body["errors"][0]["pointer"], "/windows", "{body}");

    // 給了時段就可以。
    let (status, ok) = create(
        ctx,
        &token,
        json!({
            "facility_id": FACILITY_HQ,
            "holiday_date": "2026-08-09",
            "name": "補班",
            "is_working_day": true,
            "windows": [["09:00", "18:00"]]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ok}");

    // 而且真的生效了。
    let mut tx = ctx.owner_tx().await;
    let sunday: Option<String> = sqlx::query_scalar(
        "SELECT to_char(
                  fms.add_business_minutes($1::uuid, '2026-08-09 12:00+08'::timestamptz, 60)
                    AT TIME ZONE 'Asia/Taipei', 'YYYY-MM-DD HH24:MI')",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("add_business_minutes");
    assert_eq!(
        sunday.as_deref(),
        Some("2026-08-09 13:00"),
        "補班日帶了 09:00–18:00，週日 12:00 + 60 分就在當天"
    );

    ctx.teardown().await;
}

/// PATCH 把 `windows` 清成 null 而保留 `is_working_day` → 同樣被擋。
///
/// 驗的是**合併之後的值**：只看請求主體會漏掉這個組合，因為請求裡
/// 只提供了 `windows: null`。
#[tokio::test]
async fn clearing_the_windows_of_a_make_up_day_is_rejected() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, made) = create(
        ctx,
        &token,
        json!({
            "facility_id": FACILITY_HQ,
            "holiday_date": "2026-08-09",
            "name": "補班",
            "is_working_day": true,
            "windows": [["09:00", "18:00"]]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let id = made["id"].as_str().expect("id");

    // 改名字沒問題。
    let (status, renamed) = patch(ctx, &token, id, json!({ "name": "補班（改名）" })).await;
    assert_eq!(status, StatusCode::OK, "{renamed}");

    // 清掉時段 → 422。
    let (status, cleared) = patch(ctx, &token, id, json!({ "windows": null })).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "合併之後仍是「補班日沒有時段」：{cleared}"
    );

    ctx.teardown().await;
}

/// 壞掉的 `windows` 回 422（`23514` 的通用映射會落到 500）。
#[tokio::test]
async fn malformed_windows_are_a_validation_error_not_a_500() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    for bad in [
        json!([["09:0", "18:00"]]),
        json!([["18:00", "09:00"]]),
        json!([["09:00", "09:00"]]),
        json!([["09:00"]]),
        json!("09:00-18:00"),
    ] {
        let (status, body) = create(
            ctx,
            &token,
            json!({
                "facility_id": FACILITY_HQ,
                "holiday_date": "2026-08-09",
                "name": "壞時段",
                "is_working_day": true,
                "windows": bad
            }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad} 應回 422：{body}"
        );
        assert_eq!(body["errors"][0]["pointer"], "/windows", "{body}");
    }

    ctx.teardown().await;
}

// =============================================================================
// 範圍
// =============================================================================

/// 場域管理員建不了租戶通用的假日 —— 它影響每一個場域的每一張工單。
#[tokio::test]
async fn a_facility_admin_cannot_create_a_tenant_wide_holiday() {
    let ctx = &TestContext::setup().await;
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, own) = create(
        ctx,
        &fm,
        json!({
            "facility_id": FACILITY_HQ,
            "holiday_date": "2026-08-10",
            "name": "總部歲修"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{own}");

    let (status, wide) = create(
        ctx,
        &fm,
        json!({ "holiday_date": "2026-08-11", "name": "全公司休假" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{wide}");

    let (status, other) = create(
        ctx,
        &fm,
        json!({
            "facility_id": FACILITY_CINEMA,
            "holiday_date": "2026-08-11",
            "name": "影廳休館"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{other}");

    ctx.teardown().await;
}

/// 把場域假日放大成租戶通用是一次 PATCH 完成的權限放大。
#[tokio::test]
async fn widening_a_holiday_to_the_whole_tenant_needs_tenant_scope() {
    let ctx = &TestContext::setup().await;
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, own) = create(
        ctx,
        &fm,
        json!({
            "facility_id": FACILITY_HQ,
            "holiday_date": "2026-08-10",
            "name": "總部歲修"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{own}");
    let id = own["id"].as_str().expect("id");

    let (status, widened) = patch(ctx, &fm, id, json!({ "facility_id": null })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{widened}");

    let admin = ctx.login().await;
    let (status, widened) = patch(ctx, &admin, id, json!({ "facility_id": null })).await;
    assert_eq!(status, StatusCode::OK, "{widened}");
    assert!(widened["facility_id"].is_null(), "{widened}");

    ctx.teardown().await;
}

/// 刪除也要有對應範圍的權限。
#[tokio::test]
async fn deleting_someone_elses_holiday_is_forbidden() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login().await;

    let (status, wide) = create(
        ctx,
        &admin,
        json!({ "holiday_date": "2026-08-10", "name": "全公司休假" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{wide}");
    let id = wide["id"].as_str().expect("id");

    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, body) = ctx
        .send(authed(del(&format!("/api/v1/holiday-calendars/{id}")), &fm))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "刪掉租戶通用的假日會改變每一個場域之後的期限：{body}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 目錄不得含糊
// =============================================================================

/// 同一個 (場域, 日期) 只能有一筆。
#[tokio::test]
async fn one_entry_per_facility_and_date() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, _) = create(
        ctx,
        &token,
        json!({ "holiday_date": "2026-08-10", "name": "第一筆" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, dup) = create(
        ctx,
        &token,
        json!({ "holiday_date": "2026-08-10", "name": "第二筆" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "兩筆會讓「這一天營不營業」有兩個答案：{dup}"
    );

    // 場域專屬的那一筆不衝突 —— 它是覆寫，不是重複。
    let (status, override_entry) = create(
        ctx,
        &token,
        json!({
            "facility_id": FACILITY_HQ,
            "holiday_date": "2026-08-10",
            "name": "總部照常營運",
            "is_working_day": true,
            "windows": [["08:00", "21:00"]]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "場域專屬的覆寫與租戶通用的並存：{override_entry}"
    );

    ctx.teardown().await;
}

/// 清單可依日期區間與場域過濾。
#[tokio::test]
async fn the_list_filters_by_range_and_facility() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    for (date, facility) in [
        ("2026-08-10", None),
        ("2026-09-10", None),
        ("2026-08-11", Some(FACILITY_HQ)),
    ] {
        let mut body = json!({ "holiday_date": date, "name": format!("假日 {date}") });
        if let Some(f) = facility {
            body["facility_id"] = json!(f);
        }
        let (status, resp) = create(ctx, &token, body).await;
        assert_eq!(status, StatusCode::CREATED, "{resp}");
    }

    let (_, august) = ctx
        .send(authed(
            get("/api/v1/holiday-calendars?from=2026-08-01&to=2026-08-31"),
            &token,
        ))
        .await;
    assert_eq!(
        august["data"].as_array().expect("data").len(),
        2,
        "{august}"
    );

    let (_, hq) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/holiday-calendars?facility_id={FACILITY_HQ}"
            )),
            &token,
        ))
        .await;
    let rows = hq["data"].as_array().expect("data");
    assert_eq!(rows.len(), 1, "{hq}");
    assert_eq!(rows[0]["holiday_date"], "2026-08-11", "{hq}");

    ctx.teardown().await;
}
