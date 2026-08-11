//! Assets 補完（分類樹、型錄寫入、相容性、維修履歷、狀態歷程、讀數時序）。
//!
//! # `a_` 是這個檔案存在的主要理由
//!
//! `asset_status_history` 在 migration 064 之前是 **0 列、0 寫入者、0 讀者**。
//! 設備狀態改變發生在多條路徑上（人工 `PATCH`、`sql/030` 的計量規則、
//! 未來的告警降級），而沒有一條寫歷程。
//!
//! 所以照契約做 `GET /assets/{id}/status-history` 會交付一支永遠回空清單的
//! 端點 —— 而它看起來會像「這台設備從來沒有故障過」。
//!
//! `a_` 釘住那個觸發器：狀態一變就長出一列，而**相同狀態的更新不長**
//! （否則歷程被雜訊淹沒等於沒有歷程）。
//!
//! # `c_` 釘住 compatibility 找得到真問題
//!
//! `asset_models.spare_part_codes` 是無外鍵的字串陣列。示範資料裡
//! **4 個型號有 2 個宣告了不存在的料件代碼** —— 而技師是在要叫料時才發現。
//! 這支端點的價值就在把那件事變成一個查得到的清單。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn json_req(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 直接改設備狀態（模擬 030 的計量規則或人工 PATCH 之外的路徑）。
async fn set_asset_status(ctx: &TestContext, asset: uuid::Uuid, status: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("UPDATE fms.assets SET status = $2 WHERE id = $1::uuid")
        .bind(asset)
        .bind(status)
        .execute(&mut *tx)
        .await
        .expect("改狀態");
    tx.commit().await.expect("commit");
}

/// **狀態一變就記歷程，相同狀態不記。** 064 的觸發器。
///
/// 在 064 之前這條會失敗，而失敗的方式是「清單永遠是空的」——
/// 看起來像這台設備從來沒有故障過。
#[tokio::test]
async fn a_status_changes_are_recorded_and_no_ops_are_not() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let asset = ctx.seed_asset(FACILITY_HQ, "HIST-1").await;

    // helper 建的是 OPERATIONAL。
    set_asset_status(&ctx, asset, "DEGRADED").await;
    set_asset_status(&ctx, asset, "DOWN").await;
    // 同一個狀態再寫一次 —— 不該長出第三列。
    set_asset_status(&ctx, asset, "DOWN").await;

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/assets/{asset}/status-history")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert_eq!(
        rows.len(),
        2,
        "該有兩列（OPERATIONAL→DEGRADED、DEGRADED→DOWN）——\
         相同狀態的更新不該長出第三列，否則歷程被雜訊淹沒：{}",
        body["data"]
    );
    // 最新在前。
    assert_eq!(rows[0]["from_status"], "DEGRADED");
    assert_eq!(rows[0]["to_status"], "DOWN");
    assert_eq!(rows[1]["from_status"], "OPERATIONAL");
    assert_eq!(rows[1]["to_status"], "DEGRADED");
    // owner_tx 沒有使用者情境 → changed_by 是 null，那代表「系統改的」。
    assert_eq!(
        rows[0]["changed_by"],
        Value::Null,
        "背景路徑改的該是 null —— 那與「某個人改的」是不同的事實：{}",
        rows[0]
    );

    // 看不到的設備 → 404，不是空清單。
    let (status, _) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/assets/{}/status-history",
                uuid::Uuid::new_v4()
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// 維修履歷帶成本，而成本來自 labor/parts 的明細列 rollup。
#[tokio::test]
async fn b_work_order_history_carries_the_rolled_up_cost() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let asset = ctx.seed_asset(FACILITY_HQ, "HIST-2").await;

    // 兩張掛在這台設備上的工單。
    let mut tx = ctx.owner_tx().await;
    for title in ["第一次維修", "第二次維修"] {
        sqlx::query(
            "INSERT INTO fms.work_orders
               (tenant_id, facility_id, wo_no, work_order_type, source, title,
                status, priority, asset_id)
             VALUES ($1::uuid, $2::uuid,
                     'WO-H-' || substr(md5(random()::text), 1, 10),
                     'CORRECTIVE', 'MANUAL', $3, 'IN_PROGRESS', 'MEDIUM', $4::uuid)",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_HQ)
        .bind(title)
        .bind(asset)
        .execute(&mut *tx)
        .await
        .expect("建工單");
    }
    tx.commit().await.expect("commit");

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/assets/{asset}/work-orders")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert_eq!(rows.len(), 2, "{}", body["data"]);
    // 成本欄位存在（值是 0 或 null —— 還沒登工時）。
    assert!(rows[0].get("labor_cost").is_some());
    assert!(rows[0].get("parts_cost").is_some());
    assert_eq!(rows[0]["labor_minutes"], 0);

    ctx.teardown().await;
}

/// **`compatibility` 找得到示範資料裡對不上的備品代碼。**
#[tokio::test]
async fn c_compatibility_finds_declarations_that_point_at_nothing() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    // 示範資料的四個型號。至少一個該有對不上的備品代碼 ——
    // 那正是這支端點存在的理由。
    let mut tx = ctx.owner_tx().await;
    let models: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, model_no::text FROM fms.asset_models ORDER BY model_no")
            .fetch_all(&mut *tx)
            .await
            .expect("讀型號");
    tx.commit().await.expect("commit");
    assert!(!models.is_empty(), "示範資料該有型號");

    let mut total_missing = 0i64;
    for (id, model_no) in &models {
        let (status, body) = ctx
            .send(authed(
                get(&format!("/api/v1/asset-models/{id}/compatibility")),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{model_no}：{body}");
        assert_eq!(body["model"]["model_no"], model_no.as_str());
        let missing = body["meta"]["spare_parts_missing"]
            .as_i64()
            .expect("spare_parts_missing");
        total_missing += missing;
        // `complete` 與 missing 必須一致 —— 兩個欄位說不同的話就有一個在騙人。
        assert_eq!(
            body["meta"]["complete"],
            missing == 0,
            "{model_no}：complete 與 spare_parts_missing 不一致：{}",
            body["meta"]
        );
        // 每個宣告的代碼都要出現在清單裡，附上 exists。
        let declared = body["meta"]["spare_parts_declared"]
            .as_i64()
            .expect("declared");
        assert_eq!(
            body["spare_parts"].as_array().map(Vec::len).unwrap_or(0) as i64,
            declared,
            "{model_no}：清單長度該等於宣告數"
        );
    }

    assert!(
        total_missing > 0,
        "示範資料裡該有對不上的備品代碼 —— 這支端點的價值就在把那件事變成\
         一個查得到的清單。若真的全部對得上，這條測試需要自己造一筆。"
    );

    ctx.teardown().await;
}

/// `POST /asset-models` 擋下指向不存在料件的備品清單。
#[tokio::test]
async fn d_creating_a_model_rejects_parts_that_do_not_exist() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    let mut tx = ctx.owner_tx().await;
    let (category, real_part): (uuid::Uuid, String) = sqlx::query_as(
        "SELECT (SELECT id FROM fms.asset_categories LIMIT 1),
                (SELECT part_code::text FROM fms.parts LIMIT 1)",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("讀分類與料件");
    tx.commit().await.expect("commit");

    // 不存在的料件代碼 → 422，訊息點名它。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/asset-models",
                json!({
                    "category_id": category,
                    "manufacturer": "測試廠", "model_no": "GHOST-1", "name": "幽靈備品",
                    "spare_part_codes": ["NO_SUCH_PART"]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["detail"]
        .as_str()
        .unwrap_or("")
        .contains("NO_SUCH_PART"));

    // 存在的 → 201。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/asset-models",
                json!({
                    "category_id": category,
                    "manufacturer": "測試廠", "model_no": "REAL-1", "name": "正常型號",
                    "spare_part_codes": [real_part],
                    "supported_protocols": ["MQTT"]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().expect("id");

    // 新建的型號 compatibility 該是完整的。
    let (_s, comp) = ctx
        .send(authed(
            get(&format!("/api/v1/asset-models/{id}/compatibility")),
            &token,
        ))
        .await;
    assert_eq!(comp["meta"]["spare_parts_missing"], 0, "{comp}");
    assert_eq!(comp["meta"]["complete"], true);

    // 不認得的協定 → 422。
    let (status, _) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/asset-models",
                json!({
                    "category_id": category,
                    "manufacturer": "測試廠", "model_no": "BAD-PROTO", "name": "協定錯",
                    "supported_protocols": ["CARRIER_PIGEON"]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// 分類樹回扁平清單 + 路徑，而 `asset_count` 含子分類。
#[tokio::test]
async fn e_the_category_tree_counts_subtree_assets() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(get("/api/v1/asset-categories"), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().expect("items");
    assert!(
        items.len() >= 20,
        "示範資料該有 28 個分類；實際 {}",
        items.len()
    );

    // 每一列都要有路徑與深度 —— 前端靠它們畫樹。
    for c in items {
        assert!(c["category_path"].as_str().is_some(), "{c}");
        assert!(c["depth"].as_i64().is_some(), "{c}");
    }
    // 依 category_path 排序 → 第一列的深度該是最小的。
    assert_eq!(items[0]["depth"], 0, "第一列該是根分類：{}", items[0]);

    // 建一台設備在某個葉分類下，它的**父分類**的計數也要增加。
    let mut tx = ctx.owner_tx().await;
    let (leaf, parent): (uuid::Uuid, Option<uuid::Uuid>) = sqlx::query_as(
        "SELECT id, parent_id FROM fms.asset_categories
          WHERE parent_id IS NOT NULL LIMIT 1",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("該有子分類");
    let before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.assets a
           JOIN fms.asset_categories c ON c.id = a.category_id
           JOIN fms.asset_categories p ON c.category_path <@ p.category_path
          WHERE p.id = $1::uuid",
    )
    .bind(parent.expect("parent"))
    .fetch_one(&mut *tx)
    .await
    .expect("計數");
    sqlx::query(
        "INSERT INTO fms.assets (tenant_id, facility_id, spatial_node_id,
                                 category_id, asset_code, name, status)
         VALUES ($1::uuid, $2::uuid,
                 (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                 $3::uuid, 'CAT-COUNT-1', '計數測試', 'OPERATIONAL')",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(leaf)
    .execute(&mut *tx)
    .await
    .expect("建設備");
    tx.commit().await.expect("commit");

    let (_s, body) = ctx
        .send(authed(get("/api/v1/asset-categories"), &token))
        .await;
    let parent_row = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|c| c["id"].as_str() == Some(&parent.expect("parent").to_string()))
        .expect("該找得到父分類");
    assert_eq!(
        parent_row["asset_count"].as_i64(),
        Some(before + 1),
        "父分類的計數要含子分類的設備 —— 否則中間層永遠是 0，看起來像沒人用：{parent_row}"
    );

    ctx.teardown().await;
}

/// 讀數時序：`delta` 由視窗函式算，計量表必須屬於那台設備。
#[tokio::test]
async fn f_meter_readings_expose_the_delta_and_check_ownership() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let asset = ctx.seed_asset(FACILITY_HQ, "METER-1").await;

    // 掛一個計量表並灌三筆累計讀數。
    let mut tx = ctx.owner_tx().await;
    let meter: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.asset_meters (tenant_id, asset_id, meter_code, name, unit)
         VALUES ($1::uuid, $2::uuid, 'RUN_HOURS', '運轉時數', 'h')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(asset)
    .fetch_one(&mut *tx)
    .await
    .expect("建計量表");
    for (days_ago, value) in [(3, 1000.0), (2, 1024.0), (1, 1050.0)] {
        sqlx::query(
            "INSERT INTO fms.asset_meter_readings
               (tenant_id, asset_meter_id, reading_at, value, source)
             VALUES ($1::uuid, $2::uuid, now() - make_interval(days => $3::int),
                     $4::float8::numeric, 'MANUAL')",
        )
        .bind(TENANT_ID)
        .bind(meter)
        .bind(days_ago)
        .bind(value)
        .execute(&mut *tx)
        .await
        .expect("建讀數");
    }
    tx.commit().await.expect("commit");

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/assets/{asset}/meters/RUN_HOURS/readings")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert_eq!(rows.len(), 3, "{}", body["data"]);
    // 最新在前：1050，delta = 1050 - 1024 = 26。
    assert_eq!(rows[0]["value"], 1050.0);
    let delta = rows[0]["delta"].as_f64().expect("delta");
    assert!(
        (delta - 26.0).abs() < 0.01,
        "delta 該是 26（1050 - 1024）；實際 {delta} —— 累計值的意義在增量"
    );
    // 最早那一筆沒有前一筆 → delta 是 null。
    assert_eq!(rows[2]["delta"], Value::Null);
    assert_eq!(rows[0]["source"], "MANUAL");
    assert_eq!(body["meta"]["meter_code"], "RUN_HOURS");

    // 大小寫不敏感。
    let (status, _) = ctx
        .send(authed(
            get(&format!("/api/v1/assets/{asset}/meters/run_hours/readings")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    // **別台設備的 id + 這個 meter_code → 404。** 只用 meter_code 查的話
    // 這裡會成功，而路徑就變成謊言。
    let other = ctx.seed_asset(FACILITY_HQ, "METER-2").await;
    let (status, _) = ctx
        .send(authed(
            get(&format!("/api/v1/assets/{other}/meters/RUN_HOURS/readings")),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "計量表必須屬於路徑上那台設備"
    );

    ctx.teardown().await;
}
