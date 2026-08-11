//! `GET /facilities/{facilityId}/availability`：082 給了 REQUESTER 這支端點，
//! 也補了它一直沒補的私人預約遮罩。
//!
//! # 這一組要守住的是哪個決定
//!
//! 負載測試量到的（docs/perf-baseline.md 發現二）：老師能
//! `reservation:create` 卻查不到任何教室的空檔，因為 availability 要
//! `reservation:read`，而 REQUESTER 只有 `read_own`。產品決定是開一支新的
//! 窄權限 `reservation:read_availability` 給 REQUESTER（見 082），
//! 不是把整支 `reservation:read` 發給他 —— 那會連 `GET /reservations` 與
//! `/occupancy`（別人預約的完整內容、牆面板）都一起打開，曝光面完全不同。
//!
//! # `c_` 是這一組最重要的一格
//!
//! 在寫這個測試之前查證：`repo::busy_blocks` 把 `reservations.title` 原樣
//! 塞進 `busy[].reason`，完全沒有讀 011 的 `is_private`。011 私人預約遮罩
//! 只補了 `GET /reservations`、`GET /reservations/{id}` 與
//! `GET /facilities/{id}/occupancy`（見 `private_reservation_slice.rs`），
//! availability 不在那張表裡 —— 在此之前沒人踩到，因為 availability 只有
//! 持有 `reservation:view_private` 的角色能叫。把它開給 REQUESTER 之後，
//! 那個洞就是活的：一個老師會在 `busy[].reason` 看到別人私人會議的完整
//! 標題。`c_` 守住這個洞已經補上；`d_` 守住補洞的實作沒有變成「一律回
//! null」（那樣 `c_` 也會過，但整份行事曆會變成空白）。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Timelike;
use common::*;
use serde_json::{json, Value};

/// 台北總部。與 `private_reservation_slice.rs`／`blackouts_slice.rs` 同一個
/// 常數（`common/mod.rs` 沒有匯出它）。
const FACILITY_A: &str = "cccccccc-0000-4000-8000-000000000001";

fn tomorrow(hour: u32, hours: i64) -> (String, String) {
    let base = (chrono::Utc::now() + chrono::Duration::days(1))
        .date_naive()
        .and_hms_opt(hour, 0, 0)
        .expect("valid time")
        .and_utc();
    (
        base.to_rfc3339(),
        (base + chrono::Duration::hours(hours)).to_rfc3339(),
    )
}

/// 涵蓋整個「明天」的查詢範圍。刻意用 `Z` 而非 `to_rfc3339()` 的
/// `+00:00` —— 後者的 `+` 在 query string 裡會被解成空白
/// （負載測試量到的，見 docs/perf-baseline.md 發現二表格倒數第二列）。
fn tomorrow_day_bounds() -> (String, String) {
    let d = (chrono::Utc::now() + chrono::Duration::days(1)).date_naive();
    let next = d.succ_opt().expect("valid date");
    (format!("{d}T00:00:00Z"), format!("{next}T00:00:00Z"))
}

/// 直接以 SQL 建一筆預約，主辦人可以是任意使用者（不必登入得起來）。
/// 與 `private_reservation_slice.rs` 的同名函式相同手法：API 的建立者必然是
/// 主辦人，而這裡要驗的正是「讀取者不是主辦人」。
async fn seed_reservation(
    ctx: &TestContext,
    organizer_username: &str,
    is_private: bool,
) -> uuid::Uuid {
    let (start, end) = tomorrow(if is_private { 9 } else { 14 }, 1);
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, resource_type, resource_id, bookable_resource_id,
            reservation_no, organizer_id, title, purpose, party_size, start_at, end_at,
            status, is_private, created_via)
         SELECT $1::uuid, $2::uuid, br.resource_type,
                coalesce(br.spatial_node_id, br.asset_id), br.id,
                fms.next_document_no($1::uuid, 'RESERVATION'),
                u.id, $4, $5, 4, $6::timestamptz, $7::timestamptz, 'CONFIRMED', $8, 'WEB'
           FROM fms.bookable_resources br
           CROSS JOIN fms.users u
          WHERE br.facility_id = $2::uuid AND br.is_bookable
            AND u.username = $3::citext AND u.tenant_id = $1::uuid
          ORDER BY br.display_name
          LIMIT 1
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_A)
    .bind(organizer_username)
    .bind(if is_private {
        "併購案討論"
    } else {
        "週會"
    })
    .bind(if is_private {
        "對象：目標公司財務"
    } else {
        "例行事項"
    })
    .bind(&start)
    .bind(&end)
    .bind(is_private)
    .fetch_one(&mut *tx)
    .await
    .expect("seed reservation");
    tx.commit().await.expect("commit");
    id
}

async fn availability(ctx: &TestContext, token: &str) -> (StatusCode, Value) {
    let (from, to) = tomorrow_day_bounds();
    ctx.send(authed(
        Request::builder()
            .uri(format!(
                "/api/v1/facilities/{FACILITY_A}/availability?from={from}&to={to}"
            ))
            .body(Body::empty())
            .unwrap(),
        token,
    ))
    .await
}

/// 在回應的所有資源裡找開始時刻是指定 UTC 小時的忙碌區塊。
/// `seed_reservation` 私人用 9 點、非私人用 14 點，兩者不會撞在一起。
fn find_busy_by_hour(body: &Value, hour: u32) -> Value {
    body["data"]
        .as_array()
        .expect("應有 data")
        .iter()
        .find_map(|room| {
            room["busy"].as_array()?.iter().find(|b| {
                b["start_at"]
                    .as_str()
                    .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok())
                    .is_some_and(|dt| dt.hour() == hour)
            })
        })
        .unwrap_or_else(|| panic!("找不到 {hour} 點開始的忙碌區塊：{body}"))
        .clone()
}

// =============================================================================

/// 核心：082 之前 REQUESTER 打這支端點是 403（沒有 `reservation:read`）。
/// 這一格是權限資料改動本身的憑證。
#[tokio::test]
async fn a_requester_can_now_query_availability() {
    let ctx = &TestContext::setup().await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    let (status, body) = availability(ctx, &requester).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "REQUESTER 應該持有 reservation:read_availability：{body}"
    );
    assert!(
        body["data"].as_array().is_some_and(|a| !a.is_empty()),
        "應回傳該設施的可預約資源：{body}"
    );

    ctx.teardown().await;
}

/// 反面控制：這不是把 availability 對所有人開放 —— 沒有任何 reservation
/// 權限的角色仍然是 403。TECHNICIAN 只有 `work_order:*`，009 的 seed 沒有
/// 給它任何 `reservation:*`。
#[tokio::test]
async fn b_a_role_without_any_reservation_permission_is_still_denied() {
    let ctx = &TestContext::setup().await;
    let technician = ctx.login_as(USERNAME_TECHNICIAN_HQ).await;

    let (status, body) = availability(ctx, &technician).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "TECHNICIAN 沒有任何 reservation:* 權限，應該 403：{body}"
    );

    ctx.teardown().await;
}

/// 這一組最重要的一格：`busy[].reason` 在 082 之前直接回傳
/// `reservations.title`，完全不看 `is_private`。REQUESTER（有
/// `read_availability`，沒有 `view_private`）看別人的私人預約，
/// 標題必須被遮成 null —— 時段與種類仍要看得到，那才是可用性查詢
/// 存在的理由。
#[tokio::test]
async fn c_a_private_reservation_masks_its_title_for_a_requester() {
    let ctx = &TestContext::setup().await;
    // 主辦人是 tech.wang：不是讀取者本人，也不是管理員。
    seed_reservation(ctx, "tech.wang", true).await;

    let requester = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = availability(ctx, &requester).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let block = find_busy_by_hour(&body, 9);
    assert_eq!(
        block["kind"],
        json!("RESERVATION"),
        "種類不該被遮 —— 那正是規格說看得到的部分：{block}"
    );
    assert!(
        block["reason"].is_null(),
        "私人預約的標題洩漏到 availability 的 busy[].reason 了：{block}"
    );
    assert!(!block["start_at"].is_null(), "時段被遮掉了：{block}");
    assert!(!block["end_at"].is_null(), "時段被遮掉了：{block}");

    ctx.teardown().await;
}

/// 反面：**非私人**的預約不能被遮。少了這一格，一個把 `reason` 一律回
/// null 的實作也會讓 `c_` 全綠 —— 那個實作會讓整份行事曆的忙碌原因都是
/// 空白，而 `c_` 說不出差別。
#[tokio::test]
async fn d_a_normal_reservation_is_not_masked() {
    let ctx = &TestContext::setup().await;
    seed_reservation(ctx, "tech.wang", false).await;

    let requester = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = availability(ctx, &requester).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let block = find_busy_by_hour(&body, 14);
    assert_eq!(
        block["reason"],
        json!("週會"),
        "非私人預約被遮罩了：{block}"
    );

    ctx.teardown().await;
}

/// 主辦人看得到自己訂的私人會議標題，即使他沒有 `view_private`。
/// 與 `private_reservation_slice.rs` 的 `c_` 同一個「本人例外」，
/// 這裡驗的是 availability 這條路徑也套用了同一套判定，不是另一套
/// 「一律遮到底」的規則。
#[tokio::test]
async fn e_the_organizer_sees_their_own_private_title() {
    let ctx = &TestContext::setup().await;
    seed_reservation(ctx, USERNAME_REQUESTER, true).await;

    let requester = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = availability(ctx, &requester).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let block = find_busy_by_hour(&body, 9);
    assert_eq!(
        block["reason"],
        json!("併購案討論"),
        "主辦人看不到自己訂的私人會議標題：{block}"
    );

    ctx.teardown().await;
}
