//! `MicrosoftGraphProvider` 對著一個本地假伺服器測——跟 `fms-server` 既有
//! 測試（`idp_test_connection_slice.rs`）同一個做法：真的起一個伺服器綁在
//! `127.0.0.1:0`，不用 wiremock 之類的套件。這裡多一層是因為要模擬 JSON
//! API（token 端點 + Graph API），所以用 axum 起一個真的 router，而不是
//! 裸的 TCP 監聽。
//!
//! 這些測試驗證的是「我們對 Graph 回應的解析邏輯是否正確」與「我們送出的
//! 請求形狀是否符合文件記載的 Graph API 慣例」——不是「真的 Microsoft
//! 365 租戶會怎麼回應」，那件事沒有可對接的租戶就驗不到（同一個限制在
//! ADR-13 的決定 B 也記過一次）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use fms_calendar::microsoft::MicrosoftGraphProvider;
use fms_calendar::provider::CalendarProvider;
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct MockState {
    token_requests: Arc<AtomicUsize>,
}

async fn token_handler(State(state): State<MockState>) -> Json<Value> {
    state.token_requests.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "token_type": "Bearer", "expires_in": 3599, "access_token": "fake-token" }))
}

async fn spawn_mock(router: Router<MockState>, state: MockState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().unwrap();
    let app = router.with_state(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn provider(base: &str, ms_tenant_id: &str) -> MicrosoftGraphProvider {
    MicrosoftGraphProvider::new(
        ms_tenant_id.to_string(),
        "app-client-id".to_string(),
        "app-client-secret".to_string(),
    )
    .with_bases(format!("{base}/token"), format!("{base}/graph"))
}

#[tokio::test]
async fn list_resources_parses_rooms_and_follows_pagination() {
    let state = MockState::default();
    let router = Router::new()
        .route("/token/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route(
            "/graph/places/microsoft.graph.room",
            get(|| async {
                Json(json!({
                    "value": [
                        { "emailAddress": "room-a@contoso.com", "displayName": "會議室 A" }
                    ],
                    "@odata.nextLink": "PLACEHOLDER_NEXT"
                }))
            }),
        )
        .route(
            "/graph/next-page",
            get(|| async {
                Json(json!({
                    "value": [
                        { "emailAddress": "room-b@contoso.com", "displayName": "會議室 B" }
                    ]
                }))
            }),
        );
    let base = spawn_mock(router, state.clone()).await;

    // `@odata.nextLink` 在真的 Graph 回應裡是絕對 URL；這裡用假伺服器自己的
    // base 組出第二頁的網址，因為 base 要等伺服器起來才知道埠號。
    let router2 = Router::new()
        .route("/token/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route(
            "/graph/places/microsoft.graph.room",
            get({
                let base = base.clone();
                move || {
                    let base = base.clone();
                    async move {
                        Json(json!({
                            "value": [
                                { "emailAddress": "room-a@contoso.com", "displayName": "會議室 A" }
                            ],
                            "@odata.nextLink": format!("{base}/graph/next-page")
                        }))
                    }
                }
            }),
        )
        .route(
            "/graph/next-page",
            get(|| async {
                Json(json!({
                    "value": [
                        { "emailAddress": "room-b@contoso.com", "displayName": "會議室 B" }
                    ]
                }))
            }),
        );
    let base2 = spawn_mock(router2, state.clone()).await;

    let p = provider(&base2, "tenant-guid");
    let resources = p.list_resources().await.expect("list_resources");

    assert_eq!(
        resources.len(),
        2,
        "應該跟著 @odata.nextLink 撈到第二頁：{resources:?}"
    );
    assert_eq!(resources[0].external_id, "room-a@contoso.com");
    assert_eq!(resources[0].display_name, "會議室 A");
    assert_eq!(resources[1].external_id, "room-b@contoso.com");
}

#[tokio::test]
async fn token_is_cached_across_calls() {
    let state = MockState::default();
    let router = Router::new()
        .route("/token/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route(
            "/graph/places/microsoft.graph.room",
            get(|| async { Json(json!({ "value": [] })) }),
        );
    let base = spawn_mock(router, state.clone()).await;
    let p = provider(&base, "tenant-guid");

    p.list_resources().await.expect("first call");
    p.list_resources().await.expect("second call");

    assert_eq!(
        state.token_requests.load(Ordering::SeqCst),
        1,
        "第二次呼叫不該重新換 token——快取還沒過期"
    );
}

#[tokio::test]
async fn fetch_events_delta_parses_events_and_tag_and_returns_delta_link() {
    let state = MockState::default();
    let tag_id = "String {c2a35d3a-6b9f-4b1a-9e2b-3f6a2b1d8f10} Name FmsReservationId";
    let reservation_id = uuid::Uuid::new_v4();
    let router = Router::new()
        .route("/token/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route(
            "/graph/users/{room}/calendarView/delta",
            get({
                let tag_id = tag_id.to_string();
                let reservation_id = reservation_id.to_string();
                move || {
                    let tag_id = tag_id.clone();
                    let reservation_id = reservation_id.clone();
                    async move {
                        Json(json!({
                            "value": [
                                {
                                    "id": "event-1",
                                    "subject": "週會",
                                    "start": { "dateTime": "2026-08-10T02:00:00.0000000" },
                                    "end": { "dateTime": "2026-08-10T03:00:00.0000000" },
                                    "singleValueExtendedProperties": [
                                        { "id": tag_id, "value": reservation_id }
                                    ]
                                },
                                {
                                    "id": "event-2",
                                    "@removed": { "reason": "deleted" }
                                }
                            ],
                            "@odata.deltaLink": "https://graph.example/delta-token-abc"
                        }))
                    }
                }
            }),
        );
    let base = spawn_mock(router, state.clone()).await;
    let p = provider(&base, "tenant-guid");

    let page = p
        .fetch_events_delta("room-a@contoso.com", None)
        .await
        .expect("fetch_events_delta");

    assert_eq!(page.events.len(), 2);
    let first = &page.events[0];
    assert_eq!(first.external_event_id, "event-1");
    assert_eq!(first.subject.as_deref(), Some("週會"));
    assert!(!first.is_deleted);
    assert_eq!(
        first.fms_reservation_id,
        Some(reservation_id),
        "應該從 singleValueExtendedProperties 解出我們自己打的標記"
    );

    let second = &page.events[1];
    assert!(second.is_deleted, "@removed 的事件要標成 is_deleted");

    assert_eq!(
        page.next_delta_token,
        "https://graph.example/delta-token-abc"
    );
}

#[tokio::test]
async fn create_event_sends_the_fms_tag_and_returns_external_id() {
    let state = MockState::default();
    let captured_body: Arc<tokio::sync::Mutex<Option<Value>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let router = Router::new()
        .route("/token/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route(
            "/graph/users/{room}/events",
            post({
                let captured_body = captured_body.clone();
                move |Json(body): Json<Value>| {
                    let captured_body = captured_body.clone();
                    async move {
                        *captured_body.lock().await = Some(body);
                        Json(json!({ "id": "created-event-id" }))
                    }
                }
            }),
        );
    let base = spawn_mock(router, state.clone()).await;
    let p = provider(&base, "tenant-guid");

    let reservation_id = uuid::Uuid::new_v4();
    let event = fms_calendar::provider::OutboundEvent {
        reservation_id,
        subject: "測試會議".to_string(),
        start_at: chrono::Utc::now(),
        end_at: chrono::Utc::now() + chrono::Duration::hours(1),
    };

    let external_id = p
        .create_event("room-a@contoso.com", &event)
        .await
        .expect("create_event");
    assert_eq!(external_id, "created-event-id");

    let body = captured_body
        .lock()
        .await
        .clone()
        .expect("request captured");
    assert_eq!(body["subject"], "測試會議");
    let props = body["singleValueExtendedProperties"]
        .as_array()
        .expect("extended properties array");
    assert_eq!(props.len(), 1);
    assert_eq!(props[0]["value"], reservation_id.to_string());
}

#[tokio::test]
async fn cancel_event_issues_delete_to_the_right_path() {
    let state = MockState::default();
    let deleted_path: Arc<tokio::sync::Mutex<Option<String>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let router = Router::new()
        .route("/token/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route(
            "/graph/users/{room}/events/{event_id}",
            delete({
                let deleted_path = deleted_path.clone();
                move |axum::extract::Path((room, event_id)): axum::extract::Path<(
                    String,
                    String,
                )>| {
                    let deleted_path = deleted_path.clone();
                    async move {
                        *deleted_path.lock().await = Some(format!("{room}/{event_id}"));
                        axum::http::StatusCode::NO_CONTENT
                    }
                }
            }),
        );
    let base = spawn_mock(router, state.clone()).await;
    let p = provider(&base, "tenant-guid");

    p.cancel_event("room-a@contoso.com", "event-to-cancel")
        .await
        .expect("cancel_event");

    assert_eq!(
        deleted_path.lock().await.clone(),
        Some("room-a@contoso.com/event-to-cancel".to_string())
    );
}
