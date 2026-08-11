//! 服務型錄與 `POST /auth/token/refresh`（第 0 批：收尾已出貨的功能）。
//!
//! 這兩支的價值不在自己：
//!   * `GET /facilities/{id}/service-items` 是**附加服務的前置條件** ——
//!     在它存在之前，客戶端拿不到 `service_item_id` 與 `form_schema`，
//!     `POST /reservations` 的 `services[]` 是一個填不出來的欄位。
//!   * `POST /auth/token/refresh` 是契約定義的獨立路徑，先前是 404 ——
//!     照契約產生的 client 打不到它。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::json;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const FACILITY_CINEMA: &str = "cccccccc-0000-4000-8000-000000000002";
/// 會議茶水佈置：HQ、可附加、chargeable、form_schema 要求 headcount
const SVC_TEA: &str = "60000000-0000-4000-8000-000000000001";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn service_items_are_listed_per_facility_with_their_form_schema() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_HQ}/service-items")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let items = body["data"].as_array().expect("data 應為陣列");
    assert!(!items.is_empty(), "總部應有可申請的服務：{body}");

    let tea = items
        .iter()
        .find(|i| i["id"] == SVC_TEA)
        .unwrap_or_else(|| panic!("應包含會議茶水佈置：{body}"));

    // form_schema 是這支端點存在的理由：前端要靠它渲染表單。
    assert!(
        tea["form_schema"]["properties"]["headcount"].is_object(),
        "form_schema 要能讓前端渲染表單，實際：{}",
        tea["form_schema"]
    );
    assert_eq!(tea["chargeable"], true);
    assert_eq!(tea["unit_price"], 60.0);
    // 契約的 relative_offset_minutes：-15 表示會議前 15 分鐘執行
    assert_eq!(tea["relative_offset_minutes"], -15);

    // 分頁外殼與其他列表端點一致
    assert!(
        body["page"]["limit"].is_number(),
        "應有 PagedEnvelope: {body}"
    );

    ctx.teardown().await;
}

#[tokio::test]
async fn the_catalogue_is_scoped_to_the_facility_but_includes_tenant_wide_items() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 先把茶水佈置改成「不限場域」（facility_id = NULL）——
    // 那代表全租戶適用，因此在**影廳**也該查得到。
    // 少了 `OR facility_id IS NULL` 這一半，共用服務在任何場域都會消失。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.service_items SET facility_id = NULL WHERE id = $1::uuid")
            .bind(SVC_TEA)
            .execute(&mut *tx)
            .await
            .expect("make tenant-wide");
        tx.commit().await.expect("commit");
    }

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/facilities/{FACILITY_CINEMA}/service-items"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == SVC_TEA),
        "facility_id 為 NULL 的服務應在任何場域都出現：{body}"
    );

    ctx.teardown().await;
}

#[tokio::test]
async fn inactive_and_filtered_items_are_excluded() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // ---- category 過濾 ----
    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/facilities/{FACILITY_HQ}/service-items?category=CATERING"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for item in body["data"].as_array().unwrap() {
        assert_eq!(item["category"], "CATERING", "過濾後不該有其他分類");
    }

    // ---- attachable_to_reservation=false 只回不可附加的 ----
    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/facilities/{FACILITY_HQ}/service-items?attachable_to_reservation=false"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for item in body["data"].as_array().unwrap() {
        assert_eq!(item["is_attachable_to_reservation"], false);
    }

    // ---- 停用的項目不該出現 ----
    // 型錄是給人挑的；停用項目留在清單上只會產生一次注定失敗的請求。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.service_items SET is_active = false WHERE id = $1::uuid")
            .bind(SVC_TEA)
            .execute(&mut *tx)
            .await
            .expect("deactivate");
        tx.commit().await.expect("commit");
    }
    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_HQ}/service-items")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == SVC_TEA),
        "停用的項目不該出現在型錄：{body}"
    );

    ctx.teardown().await;
}

/// 契約把 `/auth/token/refresh` 定義成獨立路徑，body 只有 `refresh_token`
/// （沒有 `grant_type`）。先前只有 `POST /auth/token` 支援 refresh，
/// 因此照契約寫的 client 會拿到 404。
#[tokio::test]
async fn the_contract_refresh_path_works_and_shares_one_implementation() {
    let ctx = &TestContext::setup().await;

    // 先取得一組 token
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/token")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "grant_type": "password",
                "tenant_code": TENANT_CODE,
                "username": USERNAME,
                "password": TEST_PASSWORD
            })
            .to_string(),
        ))
        .unwrap();
    let (status, issued) = ctx.send(req).await;
    assert_eq!(status, StatusCode::OK, "{issued}");
    let refresh_token = issued["refresh_token"].as_str().unwrap().to_string();

    // ---- 契約的獨立路徑：body 不帶 grant_type ----
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/token/refresh")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "refresh_token": refresh_token }).to_string(),
        ))
        .unwrap();
    let (status, refreshed) = ctx.send(req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "契約定義的獨立路徑必須可用（先前是 404）: {refreshed}"
    );
    for field in ["access_token", "token_type", "expires_in", "refresh_token"] {
        assert!(
            !refreshed[field].is_null(),
            "TokenResponse 缺少 {field}: {refreshed}"
        );
    }

    // ---- access token 不得當成 refresh token 用 ----
    // 兩條路徑共用同一個 refresh_grant，因此這個保護也自動適用於新路徑。
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/token/refresh")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "refresh_token": issued["access_token"].as_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let (status, _) = ctx.send(req).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "access token 不得用於 refresh"
    );

    ctx.teardown().await;
}
