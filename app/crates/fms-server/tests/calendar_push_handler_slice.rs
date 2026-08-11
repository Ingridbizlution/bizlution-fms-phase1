//! `CalendarPushHandler`（ADR-14 決定 C 的推出側）對真的 Postgres 跑，掛在
//! 既有的 outbox relay 機制上（`fms_worker::run_once`）。Microsoft Graph 用
//! 本地假伺服器，跟 `calendar_sync_watchdog_slice.rs` 同一個做法。
//!
//! 三格：FMS 原生預約確認後推出並寫回 `external_event_id`、取消後推出
//! DELETE、以及防迴圈——`created_via='OUTLOOK'` 的合成預約不該被推回去。

mod common;

use axum::extract::{Path, State};
use axum::routing::{delete, post};
use axum::{Json, Router};
use common::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use uuid::Uuid;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

async fn setup_bookable_room(ctx: &TestContext) -> (Uuid, Uuid) {
    let mut tx = ctx.owner_tx().await;
    let code = format!("PUSH{}", Uuid::new_v4().simple());
    let node: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.spatial_nodes
           (tenant_id, facility_id, node_type_code, code, name)
         VALUES ($1::uuid, $2::uuid, 'ROOM', $3, '推出測試房')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(&code)
    .fetch_one(&mut *tx)
    .await
    .expect("建空間節點");

    let bookable: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.bookable_resources
           (tenant_id, facility_id, resource_type, spatial_node_id, display_name, is_bookable)
         VALUES ($1::uuid, $2::uuid, 'SPATIAL_NODE', $3, '推出測試房', true)
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(node)
    .fetch_one(&mut *tx)
    .await
    .expect("建可預約資源");

    tx.commit().await.expect("commit");
    (node, bookable)
}

async fn setup_integration(ctx: &TestContext, spatial_node_id: Uuid) {
    let mut tx = ctx.owner_tx().await;
    let integration: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.calendar_integrations
           (tenant_id, provider, status, ms_tenant_id, sync_cron)
         VALUES ($1::uuid, 'MS365', 'ACTIVE', 'test-ms-tenant', '* * * * *')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .fetch_one(&mut *tx)
    .await
    .expect("建整合");

    sqlx::query(
        "INSERT INTO fms.calendar_resource_mappings
           (tenant_id, calendar_integration_id, spatial_node_id,
            external_resource_id, status, sync_direction)
         VALUES ($1::uuid, $2, $3, 'room-a@contoso.com', 'ACTIVE', 'BIDIRECTIONAL')",
    )
    .bind(TENANT_ID)
    .bind(integration)
    .bind(spatial_node_id)
    .execute(&mut *tx)
    .await
    .expect("建資源對應");

    tx.commit().await.expect("commit");
}

#[derive(Clone, Default)]
struct MockState {
    create_called: Arc<AtomicBool>,
    delete_called: Arc<AtomicBool>,
    captured_subject: Arc<std::sync::Mutex<Option<String>>>,
}

async fn spawn_mock_graph(state: MockState) -> (String, String) {
    async fn token_handler() -> Json<Value> {
        Json(json!({ "token_type": "Bearer", "expires_in": 3599, "access_token": "fake-token" }))
    }

    async fn create_handler(
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state.create_called.store(true, Ordering::SeqCst);
        *state.captured_subject.lock().unwrap() = body["subject"].as_str().map(str::to_string);
        Json(json!({ "id": "pushed-event-id" }))
    }

    async fn delete_handler(
        State(state): State<MockState>,
        Path((_room, _event_id)): Path<(String, String)>,
    ) -> axum::http::StatusCode {
        state.delete_called.store(true, Ordering::SeqCst);
        axum::http::StatusCode::NO_CONTENT
    }

    let router = Router::new()
        .route("/token/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route("/graph/users/{room}/events", post(create_handler))
        .route(
            "/graph/users/{room}/events/{event_id}",
            delete(delete_handler),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock graph server");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let base = format!("http://{addr}");
    (format!("{base}/token"), format!("{base}/graph"))
}

fn handler(
    pool: sqlx::PgPool,
    token_base: String,
    graph_base: String,
) -> fms_calendar::CalendarPushHandler {
    fms_calendar::CalendarPushHandler::new(
        pool,
        admin_user_id(),
        "test-client-id".to_string(),
        "test-client-secret".to_string(),
    )
    .with_ms_bases(token_base, graph_base)
}

fn relay_config() -> fms_worker::RelayConfig {
    fms_worker::RelayConfig {
        event_types: Some(vec![
            "reservation.confirmed".to_string(),
            "reservation.cancelled".to_string(),
        ]),
        ..Default::default()
    }
}

async fn insert_reservation(
    ctx: &TestContext,
    bookable: Uuid,
    node: Uuid,
    created_via: &str,
    status: &str,
) -> Uuid {
    let mut tx = ctx.owner_tx().await;
    let start = chrono::DateTime::parse_from_rfc3339("2026-10-01T02:00:00Z")
        .unwrap()
        .to_utc();
    let end = start + chrono::Duration::hours(1);
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, title, start_at, end_at,
            status, created_via)
         VALUES ($1::uuid, $2::uuid, $3, fms.next_document_no($1::uuid, 'RESERVATION', 'RSV'),
                 'SPATIAL_NODE', $4, $5::uuid, '推出測試會議', $6, $7, $8, $9)
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(bookable)
    .bind(node)
    .bind(ADMIN_USER_ID)
    .bind(start)
    .bind(end)
    .bind(status)
    .bind(created_via)
    .fetch_one(&mut *tx)
    .await
    .expect("建預約");
    tx.commit().await.expect("commit");
    id
}

async fn external_event_id_of(ctx: &TestContext, reservation_id: Uuid) -> Option<String> {
    let mut tx = ctx.owner_tx().await;
    let v: Option<String> =
        sqlx::query_scalar("SELECT external_event_id FROM fms.reservations WHERE id = $1")
            .bind(reservation_id)
            .fetch_one(&mut *tx)
            .await
            .expect("查 external_event_id");
    tx.commit().await.expect("commit");
    v
}

/// 情境一：FMS 原生預約確認後推出到外部日曆，並把回傳的 id 寫回。
#[tokio::test]
async fn a_a_confirmed_native_reservation_is_pushed_out() {
    let ctx = &TestContext::setup().await;
    let (node, bookable) = setup_bookable_room(ctx).await;
    setup_integration(ctx, node).await;

    let reservation_id = insert_reservation(ctx, bookable, node, "WEB", "CONFIRMED").await;

    let state = MockState::default();
    let (token_base, graph_base) = spawn_mock_graph(state.clone()).await;

    let batch = fms_worker::run_once(
        &ctx.owner_pool().await,
        &handler(ctx.owner_pool().await, token_base, graph_base),
        &relay_config(),
    )
    .await
    .expect("run_once");
    assert_eq!(batch.published, 1, "{batch:?}");

    assert!(
        state.create_called.load(Ordering::SeqCst),
        "應該呼叫 Graph 建立事件"
    );
    assert_eq!(
        state.captured_subject.lock().unwrap().as_deref(),
        Some("推出測試會議")
    );
    assert_eq!(
        external_event_id_of(ctx, reservation_id).await.as_deref(),
        Some("pushed-event-id")
    );

    ctx.teardown().await;
}

/// 情境二：取消後推出 DELETE。
#[tokio::test]
async fn b_a_cancelled_reservation_with_an_external_id_is_deleted_upstream() {
    let ctx = &TestContext::setup().await;
    let (node, bookable) = setup_bookable_room(ctx).await;
    setup_integration(ctx, node).await;

    let reservation_id = insert_reservation(ctx, bookable, node, "WEB", "CONFIRMED").await;

    // 先跑一輪把它推出去，讓它有 external_event_id。
    let state = MockState::default();
    let (token_base, graph_base) = spawn_mock_graph(state.clone()).await;
    fms_worker::run_once(
        &ctx.owner_pool().await,
        &handler(
            ctx.owner_pool().await,
            token_base.clone(),
            graph_base.clone(),
        ),
        &relay_config(),
    )
    .await
    .expect("run_once（建立）");
    assert!(state.create_called.load(Ordering::SeqCst));

    // 取消它——觸發 reservation.cancelled。
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.reservations SET status = 'CANCELLED', cancelled_at = clock_timestamp()
          WHERE id = $1",
    )
    .bind(reservation_id)
    .execute(&mut *tx)
    .await
    .expect("取消預約");
    tx.commit().await.expect("commit");

    let batch = fms_worker::run_once(
        &ctx.owner_pool().await,
        &handler(ctx.owner_pool().await, token_base, graph_base),
        &relay_config(),
    )
    .await
    .expect("run_once（取消）");
    assert_eq!(batch.published, 1, "{batch:?}");
    assert!(
        state.delete_called.load(Ordering::SeqCst),
        "應該呼叫 Graph 刪除事件"
    );

    ctx.teardown().await;
}

/// 情境三：防迴圈——`created_via='OUTLOOK'` 的合成預約不該被推回外部日曆。
#[tokio::test]
async fn c_an_outlook_sourced_reservation_is_not_pushed_back() {
    let ctx = &TestContext::setup().await;
    let (node, bookable) = setup_bookable_room(ctx).await;
    setup_integration(ctx, node).await;

    insert_reservation(ctx, bookable, node, "OUTLOOK", "CONFIRMED").await;

    let state = MockState::default();
    let (token_base, graph_base) = spawn_mock_graph(state.clone()).await;

    let batch = fms_worker::run_once(
        &ctx.owner_pool().await,
        &handler(ctx.owner_pool().await, token_base, graph_base),
        &relay_config(),
    )
    .await
    .expect("run_once");
    // handler 認得這個事件型別（回 true），只是判斷後略過——因此仍然算 published，
    // 不是 skipped（skipped 是「沒有 handler 認得」的意思，見 fms-worker 檔頭）。
    assert_eq!(batch.published, 1, "{batch:?}");

    assert!(
        !state.create_called.load(Ordering::SeqCst),
        "OUTLOOK 來源的預約不該被推回外部日曆——這正是防迴圈的關鍵一格"
    );

    ctx.teardown().await;
}
