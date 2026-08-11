//! Assets 收尾五支：動態欄位定義 ×2、依賴關係 ×2、批次匯入。
//!
//! # 三條核心，各對應一個設計決定
//!
//! **`a_`：動態欄位在 API 層驗，而定義終於有讀者。** `attribute_definitions`
//! 在此之前是 0 列 0 讀者。這一條同時驗「定義生效」與「既有值不被回溯拒絕」
//! —— 後者是那個決定的代價，必須看得見。
//!
//! **`c_`：循環偵測。** `ck_asset_relations_distinct` 只擋 A → A。
//! A → B → A 會讓 `/assets/{id}/dependency-graph` 無限展開，所以寫入前擋。
//!
//! **`e_`：dry-run 真的沒有寫入。** 那是這個決定唯一重要的性質 ——
//! 若試跑會留下資料，它就不是試跑。而 `f_` 驗更難的一半：試跑抓得到
//! **批次內部的重複**，那種衝突只在第二列真的寫進去時才出現。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn json_req(method: &str, uri: &str, body: Value) -> Request<Body> {
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

/// 某個既有分類的代碼（`bulk-import` 用）。
async fn a_category_code(ctx: &TestContext) -> String {
    let mut tx = ctx.owner_tx().await;
    let code: String = sqlx::query_scalar("SELECT code::text FROM fms.asset_categories LIMIT 1")
        .fetch_one(&mut *tx)
        .await
        .expect("該有分類");
    tx.commit().await.expect("commit");
    code
}

/// **動態欄位定義終於有讀者，而既有值不被回溯拒絕。**
#[tokio::test]
async fn a_definitions_gate_new_writes_but_not_existing_rows() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    // 先建一台設備，帶一個**還沒有定義**的 attributes 值。
    let category = a_category_code(&ctx).await;
    let (status, before) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/assets",
                json!({
                    "facility_id": FACILITY_HQ, "asset_code": "ATTR-OLD",
                    "name": "定義之前建立的", "category_code": category,
                    "attributes": { "voltage": "這是字串" }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "定義之前該收得下任何值：{before}"
    );

    // 現在加一個要求 voltage 必須是數字的定義。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/attribute-definitions",
                json!({
                    "attribute_key": "voltage", "label": "電壓",
                    "data_type": "NUMBER",
                    "validation_schema": { "type": "number", "minimum": 0 }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // 新的寫入被擋 —— 定義有讀者了。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/assets",
                json!({
                    "facility_id": FACILITY_HQ, "asset_code": "ATTR-NEW-BAD",
                    "name": "字串電壓", "category_code": category,
                    "attributes": { "voltage": "還是字串" }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "**定義沒有讀者** —— 這是這一輪要修的那個缺陷：{body}"
    );

    // 合法的值通過。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/assets",
                json!({
                    "facility_id": FACILITY_HQ, "asset_code": "ATTR-NEW-OK",
                    "name": "數字電壓", "category_code": category,
                    "attributes": { "voltage": 220 }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // **既有那一列仍然讀得出來** —— 加定義不該讓歷史資料變成無法存取的東西。
    let (status, body) = ctx
        .send(authed(get("/api/v1/assets?limit=100"), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]
            .as_array()
            .expect("data")
            .iter()
            .any(|a| a["asset_code"] == "ATTR-OLD"),
        "定義之前建立的那一列該還在 —— 那是 API 層驗證的代價與好處"
    );

    // 定義清單要說出驗證發生在哪一層。
    let (_s, defs) = ctx
        .send(authed(get("/api/v1/attribute-definitions"), &token))
        .await;
    assert_eq!(defs["meta"]["validated_at"], "api");
    assert_eq!(defs["meta"]["existing_values_revalidated"], false);

    ctx.teardown().await;
}

/// 壞掉的 `validation_schema` 與矛盾的 `default_value` 在建立時就被擋。
#[tokio::test]
async fn b_a_broken_schema_is_rejected_where_it_can_be_fixed() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    // `type` 不是合法的 JSON Schema 型別 → 422（而不是等到 POST /assets 才 500）。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/attribute-definitions",
                json!({
                    "attribute_key": "bad_schema", "label": "壞 schema",
                    "validation_schema": { "type": "not_a_json_schema_type" }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or("").contains("500"),
        "訊息要說出後果（別人的請求會收到 500）：{body}"
    );

    // `default_value` 不符合自己的 schema → 422。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/attribute-definitions",
                json!({
                    "attribute_key": "bad_default", "label": "矛盾預設值",
                    "validation_schema": { "type": "number" },
                    "default_value": "字串"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // key 帶點 → 422（會讓錯誤路徑無法解析）。
    let (status, _) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/attribute-definitions",
                json!({ "attribute_key": "a.b", "label": "帶點" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// **循環偵測。** A → B 之後，B → A 必須被拒。
#[tokio::test]
async fn c_a_cycle_is_refused_before_it_can_break_the_graph() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let a = ctx.seed_asset(FACILITY_HQ, "REL-A").await;
    let b = ctx.seed_asset(FACILITY_HQ, "REL-B").await;
    let c = ctx.seed_asset(FACILITY_HQ, "REL-C").await;

    // A → B。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/assets/{a}/relations"),
                json!({ "to_asset_id": b }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["meta"]["chain_depth"], 1);
    assert_eq!(body["meta"]["max_depth"], 32);

    // B → C。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/assets/{b}/relations"),
                json!({ "to_asset_id": c }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // **C → A 會形成循環** → 409，訊息說出走幾步回到起點。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/assets/{c}/relations"),
                json!({ "to_asset_id": a }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "C → A 會形成 A→B→C→A 的循環，dependency-graph 會無限展開：{body}"
    );
    assert!(
        body["detail"].as_str().unwrap_or("").contains("循環"),
        "{body}"
    );

    // 自我參照 → 422（在 handler 就擋，訊息指名那個 CHECK）。
    let (status, _) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/assets/{a}/relations"),
                json!({ "to_asset_id": a }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// 刪除是硬刪除，而不存在的回 404。
#[tokio::test]
async fn d_deleting_a_relation_removes_the_row() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let a = ctx.seed_asset(FACILITY_HQ, "DEL-A").await;
    let b = ctx.seed_asset(FACILITY_HQ, "DEL-B").await;

    let (_s, body) = ctx
        .send(authed(
            json_req(
                "POST",
                &format!("/api/v1/assets/{a}/relations"),
                json!({ "to_asset_id": b }),
            ),
            &token,
        ))
        .await;
    let id = body["id"].as_str().expect("id");

    let (status, _) = ctx
        .send(authed(
            json_req(
                "DELETE",
                &format!("/api/v1/asset-relations/{id}"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // 硬刪除 → 資料庫裡真的沒有了。
    let mut tx = ctx.owner_tx().await;
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fms.asset_relations WHERE id = $1::uuid")
            .bind(uuid::Uuid::parse_str(id).unwrap())
            .fetch_one(&mut *tx)
            .await
            .expect("count");
    tx.commit().await.expect("commit");
    assert_eq!(left, 0, "硬刪除 —— 拓樸是當前狀態，不是歷史");

    // 再刪一次 → 404。
    let (status, _) = ctx
        .send(authed(
            json_req(
                "DELETE",
                &format!("/api/v1/asset-relations/{id}"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// **dry-run 真的沒有寫入。** 那是這個決定唯一重要的性質。
#[tokio::test]
async fn e_a_dry_run_writes_nothing() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let category = a_category_code(&ctx).await;

    let rows = json!([
        { "asset_code": "IMP-1", "name": "匯入一", "facility_id": FACILITY_HQ, "category_code": category },
        { "asset_code": "IMP-2", "name": "匯入二", "facility_id": FACILITY_HQ, "category_code": category },
    ]);

    // 不帶 dry_run → **預設是試跑**。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/assets:bulk-import",
                json!({ "rows": rows }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["dry_run"], true,
        "**預設該是試跑** —— 匯入不可逆，預設值要站在安全那一邊：{body}"
    );
    assert_eq!(body["accepted"], 2);
    assert_eq!(body["rows"][0]["outcome"], "WOULD_CREATE");
    assert_eq!(
        body["meta"]["ids_are_provisional"], true,
        "試跑的 id 是回捲掉的，必須說清楚"
    );

    // 資料庫裡什麼都沒有。
    let mut tx = ctx.owner_tx().await;
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fms.assets WHERE asset_code IN ('IMP-1','IMP-2')")
            .fetch_one(&mut *tx)
            .await
            .expect("count");
    tx.commit().await.expect("commit");
    assert_eq!(n, 0, "**試跑寫入了資料** —— 那它就不是試跑");

    // 明說 dry_run: false → 真的寫入。
    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/assets:bulk-import",
                json!({ "dry_run": false, "rows": rows }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"][0]["outcome"], "CREATED");
    assert_eq!(body["meta"]["ids_are_provisional"], false);

    let mut tx = ctx.owner_tx().await;
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fms.assets WHERE asset_code IN ('IMP-1','IMP-2')")
            .fetch_one(&mut *tx)
            .await
            .expect("count");
    tx.commit().await.expect("commit");
    assert_eq!(n, 2);

    ctx.teardown().await;
}

/// **試跑抓得到批次內部的重複。** 那種衝突只在第二列真的寫進去時才出現，
/// 所以逐列驗證抓不到 —— 而這正是「走同一條路再回捲」的理由。
#[tokio::test]
async fn f_a_dry_run_catches_duplicates_inside_the_batch() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let category = a_category_code(&ctx).await;

    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/assets:bulk-import",
                json!({ "rows": [
                    { "asset_code": "DUP-X", "name": "第一列", "facility_id": FACILITY_HQ, "category_code": category },
                    { "asset_code": "DUP-X", "name": "第二列（重複）", "facility_id": FACILITY_HQ, "category_code": category },
                    { "asset_code": "GOOD-1", "name": "好的", "facility_id": FACILITY_HQ, "category_code": category },
                ]}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["accepted"], 2,
        "第一列與第三列可以，第二列重複：{body}"
    );
    assert_eq!(body["rejected"], 1);
    assert_eq!(body["rows"][1]["outcome"], "REJECTED");
    assert_eq!(
        body["rows"][1]["error_code"], "DUPLICATE_ASSET_CODE",
        "**批次內部的重複** —— 逐列驗證抓不到，只有走真的寫入路徑才會出現：{}",
        body["rows"][1]
    );
    // 第三列仍然通過 —— savepoint 的作用：一列失敗不該讓後面陪葬。
    assert_eq!(
        body["rows"][2]["outcome"], "WOULD_CREATE",
        "第二列失敗之後第三列該還能處理 —— 那是逐列 savepoint 的理由：{}",
        body["rows"][2]
    );

    ctx.teardown().await;
}

/// 逐列錯誤要能自動分類，而分類代碼查不到的訊息要指向正確的地方。
#[tokio::test]
async fn g_row_errors_are_classified_for_the_caller() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let category = a_category_code(&ctx).await;

    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/assets:bulk-import",
                json!({ "rows": [
                    { "name": "沒有編號", "facility_id": FACILITY_HQ, "category_code": category },
                    { "asset_code": "BAD-CAT", "name": "分類打錯", "facility_id": FACILITY_HQ, "category_code": "NO_SUCH_CATEGORY" },
                    { "asset_code": "BAD-ST", "name": "狀態錯", "facility_id": FACILITY_HQ, "category_code": category, "status": "NOT_A_STATUS" },
                ]}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rejected"], 3, "{body}");
    assert_eq!(body["rows"][0]["error_code"], "MISSING_ASSET_CODE");
    assert_eq!(
        body["rows"][1]["error_code"], "CATEGORY_NOT_FOUND",
        "not-null 違反其實是「分類代碼查不到」—— 回原始 SQLSTATE 會讓使用者\
         以為自己漏填了 category_id：{}",
        body["rows"][1]
    );
    assert!(
        body["rows"][1]["error"]
            .as_str()
            .unwrap_or("")
            .contains("NO_SUCH_CATEGORY"),
        "訊息要點名那個打錯的代碼：{}",
        body["rows"][1]
    );
    assert_eq!(body["rows"][2]["error_code"], "BAD_STATUS");

    // 超過上限 → 422（在送出查詢之前擋）。
    let many: Vec<Value> = (0..501)
        .map(|i| {
            json!({ "asset_code": format!("BULK-{i}"), "name": "多",
                    "facility_id": FACILITY_HQ, "category_code": category })
        })
        .collect();
    let (status, _) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/assets:bulk-import",
                json!({ "rows": many }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// 匯入也會驗動態欄位 —— 那是定義的第二個讀者。
#[tokio::test]
async fn h_bulk_import_validates_dynamic_attributes_too() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let category = a_category_code(&ctx).await;

    ctx.send(authed(
        json_req(
            "POST",
            "/api/v1/attribute-definitions",
            json!({
                "attribute_key": "capacity_kw", "label": "容量",
                "data_type": "NUMBER", "is_required": true,
                "validation_schema": { "type": "number", "exclusiveMinimum": 0 }
            }),
        ),
        &token,
    ))
    .await;

    let (status, body) = ctx
        .send(authed(
            json_req(
                "POST",
                "/api/v1/assets:bulk-import",
                json!({ "rows": [
                    { "asset_code": "ATT-OK", "name": "合法", "facility_id": FACILITY_HQ,
                      "category_code": category, "attributes": { "capacity_kw": 12.5 } },
                    { "asset_code": "ATT-BAD", "name": "型別錯", "facility_id": FACILITY_HQ,
                      "category_code": category, "attributes": { "capacity_kw": "十二" } },
                    { "asset_code": "ATT-MISS", "name": "缺必填", "facility_id": FACILITY_HQ,
                      "category_code": category },
                ]}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"][0]["outcome"], "WOULD_CREATE", "{body}");
    assert_eq!(
        body["rows"][1]["error_code"], "ATTRIBUTE_SCHEMA_VIOLATION",
        "{}",
        body["rows"][1]
    );
    assert_eq!(
        body["rows"][2]["error_code"], "MISSING_REQUIRED_ATTRIBUTE",
        "{}",
        body["rows"][2]
    );

    ctx.teardown().await;
}
