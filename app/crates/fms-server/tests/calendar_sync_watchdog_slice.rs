//! `CalendarSyncWatchdog`（ADR-14）：拉入側的排程輪詢對真的 Postgres 跑，
//! Microsoft Graph 用本地假伺服器（跟 `fms-calendar` 自己的
//! `microsoft_provider.rs` 單元測試同一個做法，這裡驗的是拉回來之後
//! 「怎麼寫進 `reservations`」這一段，那段不碰 HTTP，也不該只靠 HTTP 層的
//! 單元測試涵蓋）。
//!
//! 五格對應 ADR-14 決定 I 的五種情境：新的外部事件、回音一致、回音分歧、
//! 外部刪除、雙系統真衝突（含 SAVEPOINT 隔離）。

mod common;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use common::*;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const TAG_PROPERTY_ID: &str = "String {c2a35d3a-6b9f-4b1a-9e2b-3f6a2b1d8f10} Name FmsReservationId";

/// 佈置一個 bookable 的空間節點，回傳 `(spatial_node_id, bookable_resource_id)`。
async fn setup_bookable_room(ctx: &TestContext) -> (Uuid, Uuid) {
    let mut tx = ctx.owner_tx().await;
    let code = format!("CAL{}", Uuid::new_v4().simple());
    let node: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.spatial_nodes
           (tenant_id, facility_id, node_type_code, code, name)
         VALUES ($1::uuid, $2::uuid, 'ROOM', $3, '日曆同步測試房')
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
         VALUES ($1::uuid, $2::uuid, 'SPATIAL_NODE', $3, '日曆同步測試房', true)
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

/// 佈置一個 ACTIVE 的 MS365 整合 + 對這個空間節點的資源對應。
async fn setup_integration(ctx: &TestContext, spatial_node_id: Uuid) -> (Uuid, Uuid) {
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

    let mapping: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.calendar_resource_mappings
           (tenant_id, calendar_integration_id, spatial_node_id,
            external_resource_id, status)
         VALUES ($1::uuid, $2, $3, 'room-a@contoso.com', 'ACTIVE')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(integration)
    .bind(spatial_node_id)
    .fetch_one(&mut *tx)
    .await
    .expect("建資源對應");

    tx.commit().await.expect("commit");
    (integration, mapping)
}

/// 起一個本地假 Graph 伺服器：token 端點固定回應，`calendarView/delta`
/// 回傳呼叫端傳進來的固定內容。回傳 `(token_base, graph_base)`。
async fn spawn_mock_graph(delta_response: Value) -> (String, String) {
    async fn token_handler() -> Json<Value> {
        Json(json!({ "token_type": "Bearer", "expires_in": 3599, "access_token": "fake-token" }))
    }

    #[derive(Clone)]
    struct DeltaState(Arc<Value>);

    async fn delta_handler(State(state): State<DeltaState>) -> Json<Value> {
        Json((*state.0).clone())
    }

    let router = Router::new()
        .route("/token/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route("/graph/users/{room}/calendarView/delta", get(delta_handler))
        .with_state(DeltaState(Arc::new(delta_response)));

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

fn watchdog(
    pool: sqlx::PgPool,
    token_base: String,
    graph_base: String,
) -> Arc<fms_calendar::CalendarSyncWatchdog> {
    fms_calendar::CalendarSyncWatchdog::with_ms_bases(pool, admin_user_id(), token_base, graph_base)
}

async fn reservations_for_resource(
    ctx: &TestContext,
    spatial_node_id: Uuid,
) -> Vec<(String, String, Option<String>)> {
    let mut tx = ctx.owner_tx().await;
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT r.status::text, r.created_via::text, r.external_event_id::text
           FROM fms.reservations r
           JOIN fms.bookable_resources b ON b.id = r.bookable_resource_id
          WHERE b.spatial_node_id = $1
          ORDER BY r.created_at",
    )
    .bind(spatial_node_id)
    .fetch_all(&mut *tx)
    .await
    .expect("查預約");
    tx.commit().await.expect("commit");
    rows
}

async fn conflict_count(ctx: &TestContext) -> i64 {
    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM fms.calendar_sync_conflicts")
        .fetch_one(&mut *tx)
        .await
        .expect("查衝突數");
    tx.commit().await.expect("commit");
    n
}

/// 情境一：全新的外部事件（沒有我們的標記）——應該合成一列 OUTLOOK 來源的預約。
#[tokio::test]
async fn a_a_new_external_event_creates_a_synthetic_reservation() {
    let ctx = &TestContext::setup().await;
    let (node, _bookable) = setup_bookable_room(ctx).await;
    setup_integration(ctx, node).await;

    let (token_base, graph_base) = spawn_mock_graph(json!({
        "value": [{
            "id": "ext-event-1",
            "subject": "外部原生會議",
            "start": { "dateTime": "2026-09-01T02:00:00.0000000" },
            "end": { "dateTime": "2026-09-01T03:00:00.0000000" }
        }],
        "@odata.deltaLink": "https://graph.example/delta-1"
    }))
    .await;

    let sweep = watchdog(ctx.owner_pool().await, token_base, graph_base)
        .run_once()
        .await
        .expect("run_once");
    assert_eq!(sweep.succeeded, 1, "{sweep:?}");
    assert_eq!(sweep.failed, 0, "{sweep:?}");

    let rows = reservations_for_resource(ctx, node).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "CONFIRMED");
    assert_eq!(rows[0].1, "OUTLOOK");
    assert_eq!(rows[0].2.as_deref(), Some("ext-event-1"));

    assert_eq!(conflict_count(ctx).await, 0);

    ctx.teardown().await;
}

/// 情境二：帶著我們自己標記、時間也一致的回音——什麼都不該發生。
#[tokio::test]
async fn b_a_matching_echo_does_nothing() {
    let ctx = &TestContext::setup().await;
    let (node, bookable) = setup_bookable_room(ctx).await;
    setup_integration(ctx, node).await;

    let start = chrono::DateTime::parse_from_rfc3339("2026-09-01T02:00:00Z")
        .unwrap()
        .to_utc();
    let end = start + chrono::Duration::hours(1);

    let mut tx = ctx.owner_tx().await;
    let reservation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, title, start_at, end_at,
            status, created_via, external_event_id)
         VALUES ($1::uuid, $2::uuid, $3, fms.next_document_no($1::uuid, 'RESERVATION', 'RSV'),
                 'SPATIAL_NODE', $4, $5::uuid, '我方推出去的會議', $6, $7,
                 'CONFIRMED', 'API', 'echoed-event-1')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(bookable)
    .bind(node)
    .bind(ADMIN_USER_ID)
    .bind(start)
    .bind(end)
    .fetch_one(&mut *tx)
    .await
    .expect("建既有預約");
    tx.commit().await.expect("commit");

    let (token_base, graph_base) = spawn_mock_graph(json!({
        "value": [{
            "id": "echoed-event-1",
            "subject": "我方推出去的會議",
            "start": { "dateTime": "2026-09-01T02:00:00.0000000" },
            "end": { "dateTime": "2026-09-01T03:00:00.0000000" },
            "singleValueExtendedProperties": [
                { "id": TAG_PROPERTY_ID, "value": reservation_id.to_string() }
            ]
        }],
        "@odata.deltaLink": "https://graph.example/delta-2"
    }))
    .await;

    let sweep = watchdog(ctx.owner_pool().await, token_base, graph_base)
        .run_once()
        .await
        .expect("run_once");
    assert_eq!(sweep.succeeded, 1, "{sweep:?}");

    let rows = reservations_for_resource(ctx, node).await;
    assert_eq!(rows.len(), 1, "回音一致不該多建任何一列：{rows:?}");
    assert_eq!(conflict_count(ctx).await, 0, "時間一致不該進待審佇列");

    ctx.teardown().await;
}

/// 情境三：帶著我們自己標記、但時間被使用者直接在外部日曆改掉的回音——
/// 進 `calendar_sync_conflicts`，不覆蓋 FMS 這邊。
#[tokio::test]
async fn c_a_drifted_echo_is_written_to_the_conflict_queue() {
    let ctx = &TestContext::setup().await;
    let (node, bookable) = setup_bookable_room(ctx).await;
    setup_integration(ctx, node).await;

    let start = chrono::DateTime::parse_from_rfc3339("2026-09-02T02:00:00Z")
        .unwrap()
        .to_utc();
    let end = start + chrono::Duration::hours(1);

    let mut tx = ctx.owner_tx().await;
    let reservation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, title, start_at, end_at,
            status, created_via, external_event_id)
         VALUES ($1::uuid, $2::uuid, $3, fms.next_document_no($1::uuid, 'RESERVATION', 'RSV'),
                 'SPATIAL_NODE', $4, $5::uuid, '我方推出去的會議', $6, $7,
                 'CONFIRMED', 'API', 'echoed-event-2')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(bookable)
    .bind(node)
    .bind(ADMIN_USER_ID)
    .bind(start)
    .bind(end)
    .fetch_one(&mut *tx)
    .await
    .expect("建既有預約");
    tx.commit().await.expect("commit");

    // 外部日曆上被改成晚一小時開始。
    let (token_base, graph_base) = spawn_mock_graph(json!({
        "value": [{
            "id": "echoed-event-2",
            "subject": "我方推出去的會議",
            "start": { "dateTime": "2026-09-02T03:00:00.0000000" },
            "end": { "dateTime": "2026-09-02T04:00:00.0000000" },
            "singleValueExtendedProperties": [
                { "id": TAG_PROPERTY_ID, "value": reservation_id.to_string() }
            ]
        }],
        "@odata.deltaLink": "https://graph.example/delta-3"
    }))
    .await;

    let sweep = watchdog(ctx.owner_pool().await, token_base, graph_base)
        .run_once()
        .await
        .expect("run_once");
    assert_eq!(sweep.succeeded, 1, "{sweep:?}");

    let rows = reservations_for_resource(ctx, node).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "CONFIRMED", "FMS 這邊的時間不該被靜默覆蓋");

    assert_eq!(conflict_count(ctx).await, 1, "時間分歧要進待審佇列");

    ctx.teardown().await;
}

/// 情境四：外部日曆刪除了一個先前拉進來的原生事件——對應的合成預約要被取消。
#[tokio::test]
async fn d_a_deleted_external_event_cancels_the_synthetic_reservation() {
    let ctx = &TestContext::setup().await;
    let (node, bookable) = setup_bookable_room(ctx).await;
    setup_integration(ctx, node).await;

    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, title, start_at, end_at,
            status, created_via, external_event_id)
         VALUES ($1::uuid, $2::uuid, $3, fms.next_document_no($1::uuid, 'RESERVATION', 'RSV'),
                 'SPATIAL_NODE', $4, $5::uuid, '外部原生會議', $6, $7,
                 'CONFIRMED', 'OUTLOOK', 'ext-event-to-delete')",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(bookable)
    .bind(node)
    .bind(ADMIN_USER_ID)
    .bind(
        chrono::DateTime::parse_from_rfc3339("2026-09-03T02:00:00Z")
            .unwrap()
            .to_utc(),
    )
    .bind(
        chrono::DateTime::parse_from_rfc3339("2026-09-03T03:00:00Z")
            .unwrap()
            .to_utc(),
    )
    .execute(&mut *tx)
    .await
    .expect("建既有的外部合成預約");
    tx.commit().await.expect("commit");

    let (token_base, graph_base) = spawn_mock_graph(json!({
        "value": [{
            "id": "ext-event-to-delete",
            "@removed": { "reason": "deleted" }
        }],
        "@odata.deltaLink": "https://graph.example/delta-4"
    }))
    .await;

    let sweep = watchdog(ctx.owner_pool().await, token_base, graph_base)
        .run_once()
        .await
        .expect("run_once");
    assert_eq!(sweep.succeeded, 1, "{sweep:?}");

    let rows = reservations_for_resource(ctx, node).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "CANCELLED", "外部刪除了事件，合成預約要被取消");

    ctx.teardown().await;
}

/// 情境五：一個全新的外部事件撞上既有的 FMS 預約（雙系統真衝突）——
/// 不該讓整輪同步失敗（SAVEPOINT 生效），要進待審佇列。
#[tokio::test]
async fn e_a_genuine_double_booking_is_isolated_by_a_savepoint_and_queued() {
    let ctx = &TestContext::setup().await;
    let (node, bookable) = setup_bookable_room(ctx).await;
    setup_integration(ctx, node).await;

    let start = chrono::DateTime::parse_from_rfc3339("2026-09-04T02:00:00Z")
        .unwrap()
        .to_utc();
    let end = start + chrono::Duration::hours(1);

    // FMS 這邊已經有一筆完全獨立建立的預約，佔住同一個房間同一個時段。
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, title, start_at, end_at,
            status, created_via)
         VALUES ($1::uuid, $2::uuid, $3, fms.next_document_no($1::uuid, 'RESERVATION', 'RSV'),
                 'SPATIAL_NODE', $4, $5::uuid, 'FMS 自己建立的預約', $6, $7,
                 'CONFIRMED', 'WEB')",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(bookable)
    .bind(node)
    .bind(ADMIN_USER_ID)
    .bind(start)
    .bind(end)
    .execute(&mut *tx)
    .await
    .expect("建 FMS 端既有預約");
    tx.commit().await.expect("commit");

    // 外部日曆上，同一個房間同一個時段被獨立訂了一個沒有標記的原生事件。
    let (token_base, graph_base) = spawn_mock_graph(json!({
        "value": [{
            "id": "double-booked-event",
            "subject": "外部端獨立訂的會議",
            "start": { "dateTime": "2026-09-04T02:00:00.0000000" },
            "end": { "dateTime": "2026-09-04T03:00:00.0000000" }
        }],
        "@odata.deltaLink": "https://graph.example/delta-5"
    }))
    .await;

    let sweep = watchdog(ctx.owner_pool().await, token_base, graph_base)
        .run_once()
        .await
        .expect("run_once 不該因為一個事件撞上排他約束就整個失敗（SAVEPOINT）");
    assert_eq!(
        sweep.succeeded, 1,
        "SAVEPOINT 應該讓這一輪同步仍然算成功：{sweep:?}"
    );
    assert_eq!(sweep.failed, 0, "{sweep:?}");

    let rows = reservations_for_resource(ctx, node).await;
    assert_eq!(
        rows.len(),
        1,
        "衝突的外部事件不該被插入成第二列預約：{rows:?}"
    );

    assert_eq!(
        conflict_count(ctx).await,
        1,
        "雙系統衝突要進待審佇列，附掛在既有的那一列預約上"
    );

    ctx.teardown().await;
}
