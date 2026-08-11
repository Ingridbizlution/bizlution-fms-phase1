//! `/facilities/{id}/calendar-integrations` 與 `/calendar-integrations/{id}/
//! resource-mappings`（ADR-14 §8）。呼叫 Microsoft Graph 的那一格
//! （`unresolved-resources` 真的列外部資源）沒有在這裡測——那需要把測試
//! 端點的 base URL 換成本地假伺服器，目前只有 `fms-calendar` crate 自己的
//! `CalendarSyncWatchdog`／`CalendarPushHandler` 有那個測試鉤子（見
//! `calendar_sync_watchdog_slice.rs`／`calendar_push_handler_slice.rs`）。
//! 這裡驗的是不需要真的打外部 API 的部分：註冊、列出、權限、手動對應、
//! 重複衝突、找不到資源時的 404。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};
use uuid::Uuid;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn register_ms365(ctx: &TestContext, token: &str, ms_tenant_id: Option<&str>) -> Value {
    let mut body = json!({ "provider": "MS365" });
    if let Some(t) = ms_tenant_id {
        body["ms_tenant_id"] = json!(t);
    }
    let (status, resp) = ctx
        .send(authed(
            post(
                &format!("/api/v1/facilities/{FACILITY_HQ}/calendar-integrations"),
                body,
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{resp}");
    resp
}

async fn a_bookable_room(ctx: &TestContext) -> Uuid {
    let mut tx = ctx.owner_tx().await;
    let code = format!("APIROOM{}", Uuid::new_v4().simple());
    let node: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.spatial_nodes
           (tenant_id, facility_id, node_type_code, code, name)
         VALUES ($1::uuid, $2::uuid, 'ROOM', $3, 'API 測試房')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(&code)
    .fetch_one(&mut *tx)
    .await
    .expect("建空間節點");
    tx.commit().await.expect("commit");
    node
}

/// `a_`：註冊時沒給 `ms_tenant_id` → `PENDING_CONSENT`，附一個 admin consent URL；
/// 給了 → 直接 `ACTIVE`。
#[tokio::test]
async fn a_registering_without_ms_tenant_id_stays_pending_consent() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let pending = register_ms365(ctx, &token, None).await;
    assert_eq!(pending["status"], "PENDING_CONSENT");
    assert!(
        pending["admin_consent_url"]
            .as_str()
            .unwrap_or_default()
            .contains("adminconsent"),
        "{pending}"
    );

    ctx.teardown().await;
}

#[tokio::test]
async fn a2_registering_with_ms_tenant_id_is_active() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let active = register_ms365(ctx, &token, Some("customer-tenant-guid")).await;
    assert_eq!(active["status"], "ACTIVE");
    assert_eq!(active["ms_tenant_id"], "customer-tenant-guid");

    ctx.teardown().await;
}

/// `b_`：同一個場域＋provider 重複註冊 → 409。
#[tokio::test]
async fn b_a_duplicate_integration_for_the_same_facility_and_provider_is_rejected() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    register_ms365(ctx, &token, None).await;

    let (status, body) = ctx
        .send(authed(
            post(
                &format!("/api/v1/facilities/{FACILITY_HQ}/calendar-integrations"),
                json!({ "provider": "MS365" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    ctx.teardown().await;
}

/// `c_`：沒有 `calendar_integration:write` 的人（一般 REQUESTER）註冊不了。
#[tokio::test]
async fn c_a_requester_without_the_permission_cannot_register() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login_as(USERNAME_REQUESTER).await;

    let (status, body) = ctx
        .send(authed(
            post(
                &format!("/api/v1/facilities/{FACILITY_HQ}/calendar-integrations"),
                json!({ "provider": "MS365" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    ctx.teardown().await;
}

/// `d_`：列出剛註冊的整合。
#[tokio::test]
async fn d_listing_shows_the_registered_integration() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let created = register_ms365(ctx, &token, Some("t1")).await;

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/facilities/{FACILITY_HQ}/calendar-integrations"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&created["id"].as_str().unwrap()), "{body}");

    ctx.teardown().await;
}

/// `e_`：手動建立資源對應，成功寫進 `calendar_resource_mappings`。
#[tokio::test]
async fn e_creating_a_resource_mapping_writes_an_active_row() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let integration = register_ms365(ctx, &token, Some("t1")).await;
    let integration_id = integration["id"].as_str().unwrap();
    let node = a_bookable_room(ctx).await;

    let (status, body) = ctx
        .send(authed(
            post(
                &format!("/api/v1/calendar-integrations/{integration_id}/resource-mappings"),
                json!({ "mappings": [{ "external_resource_id": "room-x@contoso.com", "spatial_node_id": node.to_string() }] }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["created"].as_array().unwrap().len(), 1);

    let mut tx = ctx.owner_tx().await;
    let (status, spatial_node_id): (String, Uuid) = sqlx::query_as(
        "SELECT status, spatial_node_id FROM fms.calendar_resource_mappings
          WHERE external_resource_id = 'room-x@contoso.com'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("查對應");
    tx.commit().await.expect("commit");
    assert_eq!(status, "ACTIVE");
    assert_eq!(spatial_node_id, node);

    ctx.teardown().await;
}

/// `f_`：同一個整合裡同一個外部資源重複對應 → 409。
#[tokio::test]
async fn f_a_duplicate_external_resource_mapping_is_rejected() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let integration = register_ms365(ctx, &token, Some("t1")).await;
    let integration_id = integration["id"].as_str().unwrap();
    let node_a = a_bookable_room(ctx).await;
    let node_b = a_bookable_room(ctx).await;

    let (status, body) = ctx
        .send(authed(
            post(
                &format!("/api/v1/calendar-integrations/{integration_id}/resource-mappings"),
                json!({ "mappings": [{ "external_resource_id": "room-dup@contoso.com", "spatial_node_id": node_a.to_string() }] }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = ctx
        .send(authed(
            post(
                &format!("/api/v1/calendar-integrations/{integration_id}/resource-mappings"),
                json!({ "mappings": [{ "external_resource_id": "room-dup@contoso.com", "spatial_node_id": node_b.to_string() }] }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    ctx.teardown().await;
}

/// `g_`：整合還沒 `ACTIVE`（`PENDING_CONSENT`）時，`unresolved-resources`
/// 回空陣列並說明原因——不需要真的打外部 API。
#[tokio::test]
async fn g_unresolved_resources_on_a_pending_integration_is_empty_with_a_reason() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let integration = register_ms365(ctx, &token, None).await;
    let integration_id = integration["id"].as_str().unwrap();

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/calendar-integrations/{integration_id}/unresolved-resources"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"].as_array().unwrap().len(), 0);
    assert!(body["meta"]["reason"].is_string(), "{body}");

    ctx.teardown().await;
}

/// `h_`：找不到整合 → 404。
#[tokio::test]
async fn h_resource_mappings_for_a_missing_integration_is_404() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let node = a_bookable_room(ctx).await;

    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/calendar-integrations/00000000-0000-4000-8000-000000000000/resource-mappings",
                json!({ "mappings": [{ "external_resource_id": "room-x@contoso.com", "spatial_node_id": node.to_string() }] }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    ctx.teardown().await;
}
