//! 契約符合性檢查 —— ADR-09 實作紀律 1 的機械化落實。
//!
//! `api/openapi.yaml` 是手寫的權威契約，前端由它產生 client。
//! 因此方向是「程式碼去符合契約」，不是「從程式碼產生契約」。
//! （早期原型用 utoipa 反向產生 OpenAPI，結果與真正的契約不相容。）
//!
//! 本檔擋的是三類實際發生過的漂移：
//!   A. 路徑／方法對不上契約（`/auth/login` vs 契約的 `/auth/token`、
//!      `/me` vs `/auth/me`）
//!   B. 發明契約裡沒有的端點（原型曾憑空加出 `POST /resources`）
//!   C. 回應形狀與契約 schema 不符（原型的 `TokenResponse` 少了
//!      `tenant_id`／`user_id`／`must_change_password`）
//!
//! A、B 靠路徑比對；C 靠把真實回應丟進契約的 JSON Schema 驗證 ——
//! OpenAPI 3.1 的 schema 就是 JSON Schema 2020-12，可以直接拿來驗。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// 以 `CARGO_MANIFEST_DIR` 組出絕對路徑：測試的工作目錄是 crate 根，
/// 用相對路徑會隨執行方式而變。
const CONTRACT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../api/openapi.yaml");

fn load_contract() -> Value {
    let raw =
        std::fs::read_to_string(CONTRACT).unwrap_or_else(|e| panic!("讀不到契約 {CONTRACT}: {e}"));
    serde_yaml::from_str(&raw).expect("openapi.yaml 不是合法 YAML")
}

/// 依 `#/components/schemas/X` 取出 schema，並把整份契約當作 root
/// 以便解析內部 `$ref`。
fn schema_validator(contract: &Value, name: &str) -> jsonschema::Validator {
    let mut schema = contract["components"]["schemas"][name].clone();
    assert!(
        !schema.is_null(),
        "契約中找不到 schema `{name}` —— 若已改名，程式碼要跟著改"
    );
    // 把 components 併進 schema，讓相對 $ref 解得開
    schema["components"] = contract["components"].clone();
    jsonschema::validator_for(&schema)
        .unwrap_or_else(|e| panic!("schema `{name}` 無法編譯為 JSON Schema: {e}"))
}

fn assert_valid(validator: &jsonschema::Validator, instance: &Value, label: &str) {
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("  {} @ {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{label} 的回應不符契約 schema：\n{}\n實際回應：{}",
        errors.join("\n"),
        serde_json::to_string_pretty(instance).unwrap_or_default()
    );
}

/// 補足 JSON Schema 驗證的盲點：**契約的回應 schema 幾乎都沒有 `required`**，
/// 因此純 schema 驗證只抓型別與可空性錯誤，抓不到「少回傳欄位」。
/// 而少回傳欄位正是早期原型實際犯的錯（`TokenResponse` 漏了
/// `tenant_id`／`user_id`／`must_change_password`）。
///
/// 這裡對「契約認為應完整回傳」的 schema 額外要求：
/// schema 宣告的每個 property 都必須出現在回應中（值可為 null）。
///
/// `Problem` 刻意不套用此檢查 —— RFC 9457 的多數成員本就是選用的，
/// 例如沒有欄位級錯誤時就不該出現 `errors`。
fn assert_no_missing_declared_properties(contract: &Value, schema_name: &str, instance: &Value) {
    let props = contract["components"]["schemas"][schema_name]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema `{schema_name}` 沒有 properties"));
    let obj = instance
        .as_object()
        .unwrap_or_else(|| panic!("{schema_name} 的回應不是物件"));

    let missing: Vec<&String> = props.keys().filter(|k| !obj.contains_key(*k)).collect();
    assert!(
        missing.is_empty(),
        "{schema_name} 的回應少了契約宣告的欄位：{missing:?}\n\
         （契約未標 required，所以 JSON Schema 驗證不會抓到這類漏欄位）"
    );
}

/// A + B：已實作的每一支端點都必須存在於契約，且路徑／方法完全一致。
#[test]
fn every_implemented_operation_is_declared_in_the_contract() {
    let contract = load_contract();
    let paths = contract["paths"].as_object().expect("契約缺少 paths");

    let mut missing = Vec::new();
    for (method, path) in fms_server::IMPLEMENTED_OPERATIONS {
        match paths.get(*path) {
            None => missing.push(format!("契約沒有這個路徑：{path}")),
            Some(item) => {
                if item.get(*method).is_none() {
                    missing.push(format!("契約的 {path} 沒有 {} 方法", method.to_uppercase()));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "實作偏離契約：\n{}\n\
         這代表發明了契約沒有的端點，或路徑／方法拼錯。\
         正確做法是先改 openapi.yaml（它是權威），不是改這份測試。",
        missing.join("\n")
    );
}

/// 進度指標（不斷言，只報告）：已實作覆蓋了多少契約 operation。
#[test]
fn report_contract_coverage() {
    let contract = load_contract();
    let mut declared = BTreeSet::new();
    for (path, item) in contract["paths"].as_object().unwrap() {
        for method in ["get", "post", "put", "patch", "delete"] {
            if item.get(method).is_some() {
                declared.insert(format!("{} {}", method.to_uppercase(), path));
            }
        }
    }
    let implemented: BTreeSet<String> = fms_server::IMPLEMENTED_OPERATIONS
        .iter()
        .map(|(m, p)| format!("{} {}", m.to_uppercase(), p))
        .collect();

    println!(
        "契約覆蓋率：{}/{} operations（{:.1}%）",
        implemented.len(),
        declared.len(),
        implemented.len() as f64 / declared.len() as f64 * 100.0
    );
    for op in declared.difference(&implemented) {
        println!("  尚未實作：{op}");
    }
}

/// B + C：實際呼叫每支端點，確認可路由（非 404）且回應符合契約 schema。
#[tokio::test]
async fn responses_conform_to_contract_schemas() {
    let contract = load_contract();
    let ctx = TestContext::setup().await;

    // ---- POST /auth/token → TokenResponse ----
    let token_res = ctx
        .send(
            Request::builder()
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
                .unwrap(),
        )
        .await;
    assert_eq!(token_res.0, StatusCode::OK, "{:?}", token_res.1);
    assert_valid(
        &schema_validator(&contract, "TokenResponse"),
        &token_res.1,
        "POST /auth/token",
    );
    assert_no_missing_declared_properties(&contract, "TokenResponse", &token_res.1);
    let token = token_res.1["access_token"].as_str().unwrap().to_string();

    // ---- GET /auth/me → CurrentUser ----
    let (status, me) = ctx
        .send(authed(
            Request::builder()
                .uri("/api/v1/auth/me")
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_valid(
        &schema_validator(&contract, "CurrentUser"),
        &me,
        "GET /auth/me",
    );
    assert_no_missing_declared_properties(&contract, "CurrentUser", &me);

    // ---- POST /reservations → Reservation ----
    let start = (chrono::Utc::now() + chrono::Duration::days(6))
        .date_naive()
        .and_hms_opt(9, 0, 0)
        .unwrap()
        .and_utc();
    let (status, created) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/reservations")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "resource_id": "10000000-0000-4000-8000-000000000005",
                        "title": "契約符合性測試",
                        "start_at": start.to_rfc3339(),
                        "end_at": (start + chrono::Duration::hours(1)).to_rfc3339()
                    })
                    .to_string(),
                ))
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_valid(
        &schema_validator(&contract, "Reservation"),
        &created,
        "POST /reservations",
    );
    assert_no_missing_declared_properties(&contract, "Reservation", &created);
    let id = created["id"].as_str().unwrap().to_string();

    // ---- GET /reservations → PagedEnvelope（data 內每一項須為 Reservation）----
    let (status, listed) = ctx
        .send(authed(
            Request::builder()
                .uri("/api/v1/reservations?mine=true")
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_valid(
        &schema_validator(&contract, "PagedEnvelope"),
        &listed,
        "GET /reservations",
    );
    let item_validator = schema_validator(&contract, "Reservation");
    for (i, item) in listed["data"].as_array().unwrap().iter().enumerate() {
        assert_valid(
            &item_validator,
            item,
            &format!("GET /reservations data[{i}]"),
        );
    }

    // ---- GET /reservations/{id} → Reservation ----
    let (status, etag, one) = ctx
        .send_with_headers(authed(
            Request::builder()
                .uri(format!("/api/v1/reservations/{id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_valid(
        &schema_validator(&contract, "Reservation"),
        &one,
        "GET /reservations/{reservationId}",
    );

    // ---- PATCH /reservations/{id} → Reservation ----
    let (status, patched) = ctx
        .send(authed_if_match(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/reservations/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "party_size": 6 }).to_string()))
                .unwrap(),
            &token,
            &etag.expect("GET 應回 ETag"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_valid(
        &schema_validator(&contract, "Reservation"),
        &patched,
        "PATCH /reservations/{reservationId}",
    );

    // ---- POST /assets → Asset ----
    let asset_code = format!("TEST-CONF-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let (status, asset) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/assets")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "facility_id": "cccccccc-0000-4000-8000-000000000001",
                        "spatial_node_id": "10000000-0000-4000-8000-000000000003",
                        "category_code": "AHU",
                        "asset_code": asset_code,
                        "name": "契約符合性測試設備"
                    })
                    .to_string(),
                ))
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{asset}");
    assert_valid(
        &schema_validator(&contract, "Asset"),
        &asset,
        "POST /assets",
    );
    assert_no_missing_declared_properties(&contract, "Asset", &asset);
    let asset_id = asset["id"].as_str().unwrap().to_string();

    // ---- GET /assets → PagedEnvelope + 每項符合 Asset ----
    let (status, assets) = ctx
        .send(authed(
            Request::builder()
                .uri("/api/v1/assets?limit=5")
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{assets}");
    assert_valid(
        &schema_validator(&contract, "PagedEnvelope"),
        &assets,
        "GET /assets",
    );
    let asset_validator = schema_validator(&contract, "Asset");
    for (i, item) in assets["data"].as_array().unwrap().iter().enumerate() {
        assert_valid(&asset_validator, item, &format!("GET /assets data[{i}]"));
    }

    // ---- GET /assets/{id} → AssetDetail（allOf: Asset + 選用陣列）----
    let (status, etag, one_asset) = ctx
        .send_with_headers(authed(
            Request::builder()
                .uri(format!("/api/v1/assets/{asset_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{one_asset}");
    assert_valid(
        &schema_validator(&contract, "AssetDetail"),
        &one_asset,
        "GET /assets/{assetId}",
    );
    // AssetDetail 用 allOf，沒有頂層 properties；基底欄位的完整性對 Asset 檢查
    assert_no_missing_declared_properties(&contract, "Asset", &one_asset);

    // ---- PATCH /assets/{id} → Asset ----
    let (status, patched_asset) = ctx
        .send(authed_if_match(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/assets/{asset_id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "criticality": "CRITICAL" }).to_string()))
                .unwrap(),
            &token,
            &etag.expect("GET /assets/{id} 應回 ETag"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched_asset}");
    assert_valid(
        &schema_validator(&contract, "Asset"),
        &patched_asset,
        "PATCH /assets/{assetId}",
    );

    // ---- DELETE /assets/{id} → 204，無 body ----
    let (status, _, _) = ctx
        .send_with_headers(authed(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/assets/{asset_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // ---- 錯誤回應也要符合契約的 Problem schema ----
    let (status, problem) = ctx
        .send(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/token")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "grant_type": "password",
                        "tenant_code": TENANT_CODE,
                        "username": USERNAME,
                        "password": "wrong"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_valid(&schema_validator(&contract, "Problem"), &problem, "Problem");

    ctx.teardown().await;
}
