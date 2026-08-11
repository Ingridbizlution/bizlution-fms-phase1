//! ADR-14：外部日曆同步進來的預約（`created_via IN ('OUTLOOK','GOOGLE')`）
//! 唯一的編輯入口是外部日曆本身——FMS 這邊取消或改期都會在下一輪拉入時被
//! 外部原生事件的現況覆蓋回去，看起來像「改了又自己復原」。
//!
//! `a_` 驗取消被擋；`b_` 驗改期被擋；`c_` 驗**只**擋這兩件事——標題等
//! FMS 本地中繼資料仍然可以照常改，不該被一併鎖死。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};
use uuid::Uuid;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

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

/// 直接以 SQL 建一筆 `created_via='OUTLOOK'` 的預約——沒有 API 路徑能建立
/// 這種來源的列（那正是重點，只有拉入 worker 才會寫這個值）。
async fn insert_outlook_reservation(ctx: &TestContext) -> Uuid {
    let mut tx = ctx.owner_tx().await;
    let bookable: Uuid = sqlx::query_scalar(
        "SELECT id FROM fms.bookable_resources
          WHERE facility_id = $1::uuid AND is_bookable LIMIT 1",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("找一個可預約資源");
    let resource_id: Uuid = sqlx::query_scalar(
        "SELECT coalesce(spatial_node_id, asset_id) FROM fms.bookable_resources WHERE id = $1",
    )
    .bind(bookable)
    .fetch_one(&mut *tx)
    .await
    .expect("查 resource_id");

    let start = chrono::DateTime::parse_from_rfc3339("2026-11-01T02:00:00Z")
        .unwrap()
        .to_utc();
    let end = start + chrono::Duration::hours(1);
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, title, start_at, end_at,
            status, created_via, external_event_id)
         VALUES ($1::uuid, $2::uuid, $3, fms.next_document_no($1::uuid, 'RESERVATION', 'RSV'),
                 'SPATIAL_NODE', $4, $5::uuid, '外部原生會議', $6, $7,
                 'CONFIRMED', 'OUTLOOK', 'edit-slice-ext-event')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(bookable)
    .bind(resource_id)
    .bind(ADMIN_USER_ID)
    .bind(start)
    .bind(end)
    .fetch_one(&mut *tx)
    .await
    .expect("建外部來源的預約");
    tx.commit().await.expect("commit");
    id
}

/// `a_`：取消被擋，回 409。
#[tokio::test]
async fn a_cancelling_an_outlook_sourced_reservation_is_rejected() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = insert_outlook_reservation(ctx).await;

    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/reservations/{id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (_, after) = ctx
        .send(authed(get(&format!("/api/v1/reservations/{id}")), &token))
        .await;
    assert_eq!(
        after["status"], "CONFIRMED",
        "取消被擋，狀態不該變：{after}"
    );

    ctx.teardown().await;
}

/// `b_`：改期被擋，回 409；不改時間的欄位（標題）仍然放行。
#[tokio::test]
async fn b_rescheduling_an_outlook_sourced_reservation_is_rejected_but_other_fields_are_not() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = insert_outlook_reservation(ctx).await;

    let (_, etag, _body) = ctx
        .send_with_headers(authed(get(&format!("/api/v1/reservations/{id}")), &token))
        .await;
    let etag = etag.expect("GET 單筆應回 ETag");

    let (status, body) = ctx
        .send(authed_if_match(
            json_request(
                "PATCH",
                &format!("/api/v1/reservations/{id}"),
                json!({ "start_at": "2026-11-01T04:00:00Z", "end_at": "2026-11-01T05:00:00Z" }),
            ),
            &token,
            &etag,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "改期應該被擋：{body}");

    // c_ 的核心：標題不是時間，外部日曆不管這件事，FMS 端仍然可以改。
    let (status, body) = ctx
        .send(authed_if_match(
            json_request(
                "PATCH",
                &format!("/api/v1/reservations/{id}"),
                json!({ "title": "本地備註：需要投影機" }),
            ),
            &token,
            &etag,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "改標題不該被擋：{body}");
    assert_eq!(body["title"], "本地備註：需要投影機");

    ctx.teardown().await;
}
