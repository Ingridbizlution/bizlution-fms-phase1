//! 2.5D 樓層平面圖的設備標點（`/spatial-nodes/{id}/floor-plan-markers`、
//! `/floor-plan-markers/{id}`）。
//!
//! `b_` 是這組測試的核心：`floorNodeId` 必須是一列 `FLOOR` 節點，不是任意
//! 空間節點——資料庫層驗證不到（`spatial_node_types` 是租戶可擴充目錄，
//! 見 migration 086 檔頭），只能靠 handler 自己查一次再擋。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 009 種的 FL04（四樓），HQ 大樓，`node_type_code = 'FLOOR'`。
const FLOOR_FL04: &str = "10000000-0000-4000-8000-000000000003";
/// 009 種的 401 會議室——不是 FLOOR，用來測 node_type 驗證。
const MEETING_ROOM_401: &str = "10000000-0000-4000-8000-000000000005";
/// 009 種的「4F 空調箱 #1」，本來就掛在 FL04 底下。
const ASSET_AHU_4F: &str = "20000000-0000-4000-8000-000000000002";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn create_marker(
    ctx: &TestContext,
    token: &str,
    floor_id: &str,
    body: Value,
) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request(
            "POST",
            &format!("/api/v1/spatial-nodes/{floor_id}/floor-plan-markers"),
            body,
        ),
        token,
    ))
    .await
}

async fn list_markers(ctx: &TestContext, token: &str, floor_id: &str) -> (StatusCode, Value) {
    ctx.send(authed(
        get(&format!(
            "/api/v1/spatial-nodes/{floor_id}/floor-plan-markers"
        )),
        token,
    ))
    .await
}

/// 正常路徑：建得起來、列得到、`entity_label`／`entity_status` 解析得出來，
/// 刪掉之後就不在清單裡了。
#[tokio::test]
async fn a_a_marker_can_be_created_listed_and_deleted() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, created) = create_marker(
        ctx,
        &admin,
        FLOOR_FL04,
        json!({
            "entity_type": "ASSET",
            "entity_id": ASSET_AHU_4F,
            "x_ratio": 0.42,
            "y_ratio": 0.61,
            "z_offset": 0.3
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["floor_node_id"], FLOOR_FL04);
    assert_eq!(created["entity_type"], "ASSET");
    assert_eq!(created["x_ratio"], 0.42);
    assert_eq!(created["y_ratio"], 0.61);
    assert_eq!(
        created["entity_label"], "4F 空調箱 #1",
        "只回 entity_id 的話 UI 得為每一列再查一次那個 uuid：{created}"
    );
    assert_eq!(
        created["entity_status"], "OPERATIONAL",
        "前端要靠這個欄位決定標點顏色，不用另外查一次資產：{created}"
    );

    let (status, listed) = list_markers(ctx, &admin, FLOOR_FL04).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    assert!(
        items.iter().any(|m| m["id"] == created["id"]),
        "新增的標點要在清單裡：{listed}"
    );

    let (status, deleted) = ctx
        .send(authed(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/api/v1/floor-plan-markers/{}",
                    created["id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");

    let (_, listed_after) = list_markers(ctx, &admin, FLOOR_FL04).await;
    let items_after = listed_after["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !items_after.iter().any(|m| m["id"] == created["id"]),
        "刪掉之後不該還在清單裡：{listed_after}"
    );

    ctx.teardown().await;
}

/// `floorNodeId` 必須是 FLOOR 節點——401 會議室不是，GET／POST 都要 422。
#[tokio::test]
async fn b_floor_node_id_must_actually_be_a_floor() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = list_markers(ctx, &admin, MEETING_ROOM_401).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("FLOOR"),
        "訊息要說得出為什麼——不是任意空間節點都能掛平面圖標點：{body}"
    );

    let (status, body) = create_marker(
        ctx,
        &admin,
        MEETING_ROOM_401,
        json!({
            "entity_type": "ASSET",
            "entity_id": ASSET_AHU_4F,
            "x_ratio": 0.5,
            "y_ratio": 0.5
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    ctx.teardown().await;
}

/// 輸入驗證：entity_type 不對、比例超出範圍、entity_id 不存在，都要 422
/// 且訊息說得出原因——這張表沒有外鍵，insert 失敗不會自動說清楚。
#[tokio::test]
async fn c_input_is_validated_with_a_readable_reason() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = create_marker(
        ctx,
        &admin,
        FLOOR_FL04,
        json!({ "entity_type": "SPACESHIP", "entity_id": ASSET_AHU_4F, "x_ratio": 0.5, "y_ratio": 0.5 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = create_marker(
        ctx,
        &admin,
        FLOOR_FL04,
        json!({ "entity_type": "ASSET", "entity_id": ASSET_AHU_4F, "x_ratio": 1.5, "y_ratio": 0.5 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = create_marker(
        ctx,
        &admin,
        FLOOR_FL04,
        json!({
            "entity_type": "ASSET",
            "entity_id": "99999999-0000-4000-8000-000000000099",
            "x_ratio": 0.5,
            "y_ratio": 0.5
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("entity_id"),
        "訊息要說得出是哪個 id 找不到：{body}"
    );

    ctx.teardown().await;
}

/// REQUESTER 有 `spatial_node:read` 但沒有 `spatial_node:write`——
/// 讀得到、寫不進去。
#[tokio::test]
async fn d_write_requires_spatial_node_write_not_just_read() {
    let ctx = &TestContext::setup().await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    let (status, body) = list_markers(ctx, &requester, FLOOR_FL04).await;
    assert_eq!(status, StatusCode::OK, "read-only 角色也該看得到：{body}");

    let (status, body) = create_marker(
        ctx,
        &requester,
        FLOOR_FL04,
        json!({ "entity_type": "ASSET", "entity_id": ASSET_AHU_4F, "x_ratio": 0.5, "y_ratio": 0.5 }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    ctx.teardown().await;
}
