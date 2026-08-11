//! 設備型錄與計量讀數（WBS 4.8／4.9）。
//!
//! 重點：
//!   * `GET /asset-models` 的 `scope=all|platform|tenant` 真的可分辨
//!     （017 同時種了平台與租戶型號，否則三種過濾看起來一樣）
//!   * 型錄的 keyset 翻頁（游標鍵是 `manufacturer` + `model_no` 兩段）
//!   * 累計型讀表：門檻是**週期**，每跨一個倍數觸發一次
//!   * 累計型讀表不得倒退，除非設了 `rollover_at`
//!   * 遲到的讀數寫入歷史但不推進當前值，也不觸發保養
//!   * 觸發時寫入 outbox 事件（與讀數同一個交易）

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 1 廳投影機 —— 009 唯一有讀表（LAMP_HOURS，CUMULATIVE，last_value 4820）的設備
const SEED_PROJECTOR: &str = "20000000-0000-4000-8000-000000000003";
/// 009 的計量型保養計畫：LAMP_HOURS 門檻 5000
const PLAN_LAMP: &str = "90000000-0000-4000-8000-000000000002";

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// 單一入口：兩個場景共用一個 ctx。分成兩個 `#[tokio::test]` 會平行執行，
/// 而 setup 的清理會刪掉對方寫入的讀數。
#[tokio::test]
async fn catalog_and_meter_slice() {
    let ctx = TestContext::setup().await;
    asset_model_catalogue(&ctx).await;
    meter_readings_and_threshold_rule(&ctx).await;
    ctx.teardown().await;
}

async fn asset_model_catalogue(ctx: &TestContext) {
    let token = ctx.login().await;

    // ---- scope=all：平台與租戶型號都要在 ----
    let (status, all) = ctx
        .send(authed(
            get_request("/api/v1/asset-models?limit=200"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{all}");
    let rows = all["data"].as_array().expect("應有 data");
    assert!(
        rows.iter().any(|m| m["is_platform"] == true),
        "應看得到平台共用型號：{all}"
    );
    assert!(
        rows.iter().any(|m| m["is_platform"] == false),
        "應看得到租戶自建型號（017 種下的）：{all}"
    );
    // 排序固定為 manufacturer, model_no
    let makers: Vec<&str> = rows
        .iter()
        .map(|m| m["manufacturer"].as_str().unwrap())
        .collect();
    let mut sorted = makers.clone();
    sorted.sort_unstable();
    assert_eq!(makers, sorted, "型錄應依製造商字母序：{all}");

    // ---- scope=platform／tenant 必須真的不同 ----
    let (_, platform) = ctx
        .send(authed(
            get_request("/api/v1/asset-models?scope=platform&limit=200"),
            &token,
        ))
        .await;
    assert!(
        platform["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["is_platform"] == true),
        "scope=platform 不該混入租戶型號：{platform}"
    );
    let (_, tenant) = ctx
        .send(authed(
            get_request("/api/v1/asset-models?scope=tenant&limit=200"),
            &token,
        ))
        .await;
    let tenant_rows = tenant["data"].as_array().unwrap();
    assert!(
        !tenant_rows.is_empty() && tenant_rows.iter().all(|m| m["is_platform"] == false),
        "scope=tenant 應只回租戶型號且不為空：{tenant}"
    );
    assert_ne!(
        platform["data"].as_array().unwrap().len(),
        all["data"].as_array().unwrap().len(),
        "platform 與 all 的筆數相同代表過濾沒生效"
    );

    // ---- 未定義的 scope → 422 ----
    let (status, body) = ctx
        .send(authed(
            get_request("/api/v1/asset-models?scope=everything"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- category_code 過濾 ----
    let (_, ups) = ctx
        .send(authed(
            get_request("/api/v1/asset-models?category_code=UPS&limit=200"),
            &token,
        ))
        .await;
    assert!(
        ups["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["category_code"] == "UPS"),
        "{ups}"
    );

    // ---- keyset 翻頁：兩段游標鍵（manufacturer + model_no）要能繼續 ----
    let (_, p1) = ctx
        .send(authed(get_request("/api/v1/asset-models?limit=1"), &token))
        .await;
    let cursor = p1["page"]["next_cursor"]
        .as_str()
        .expect("應有 next_cursor")
        .to_string();
    let (_, p2) = ctx
        .send(authed(
            get_request(&format!("/api/v1/asset-models?limit=1&cursor={cursor}")),
            &token,
        ))
        .await;
    let first = p1["data"][0]["manufacturer"].as_str().unwrap();
    let second = p2["data"][0]["manufacturer"].as_str().unwrap();
    assert_ne!(
        p1["data"][0]["id"], p2["data"][0]["id"],
        "第二頁重複了第一頁：{p2}"
    );
    assert!(second >= first, "翻頁應往後走：{first} → {second}");

    // ---- 供相容性檢查用的欄位要真的帶出來 ----
    let barco = rows
        .iter()
        .find(|m| m["manufacturer"] == "Barco")
        .unwrap_or_else(|| panic!("017 應種下 Barco 投影機型號：{all}"));
    assert!(
        barco["supported_protocols"]
            .as_array()
            .is_some_and(|p| !p.is_empty()),
        "supported_protocols 是相容性檢查的輸入，不該是空的：{barco}"
    );
    assert!(barco["specifications"]["lumens"].as_i64().is_some());
}

async fn meter_readings_and_threshold_rule(ctx: &TestContext) {
    let token = ctx.login().await;
    let now = chrono::Utc::now();

    // 009 的投影機讀表：LAMP_HOURS，CUMULATIVE，last_value = 4820，門檻 5000
    // ---- 未達門檻：只更新當前值，不觸發 ----
    let (status, r1) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                json!({ "value": 4900, "reading_at": now.to_rfc3339() }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{r1}");
    assert_eq!(r1["meter_code"], "LAMP_HOURS");
    assert_eq!(r1["last_value"].as_f64(), Some(4900.0));
    assert_eq!(
        r1["triggered_maintenance_plan_ids"],
        json!([]),
        "4900 還沒到 5000，不該觸發：{r1}"
    );

    // ---- 路徑參數大小寫不敏感（唯一索引是 lower(meter_code)）----
    let (status, lower) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/lamp_hours/readings"),
                json!({ "value": 4950, "reading_at": (now + chrono::Duration::minutes(1)).to_rfc3339() }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "讀表查詢應與唯一索引一樣不分大小寫：{lower}"
    );

    // ---- 跨過 5000：計量型保養計畫應被回報 ----
    let (status, r2) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                json!({ "value": 5010, "reading_at": (now + chrono::Duration::minutes(2)).to_rfc3339() }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{r2}");
    assert_eq!(r2["last_value"].as_f64(), Some(5010.0));
    let triggered: Vec<&str> = r2["triggered_maintenance_plan_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        triggered.contains(&PLAN_LAMP),
        "跨過 5000 應回報光源更換計畫：{r2}"
    );

    // ---- 同一個週期內再讀一次不該重複觸發 ----
    let (_, r3) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                json!({ "value": 5200, "reading_at": (now + chrono::Duration::minutes(3)).to_rfc3339() }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        r3["triggered_maintenance_plan_ids"],
        json!([]),
        "5010 → 5200 沒有跨過新的 5000 倍數，不該再觸發：{r3}"
    );

    // ---- 門檻是週期而非一次性：跨過 10000 要再觸發 ----
    let (_, r4) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                json!({ "value": 10050, "reading_at": (now + chrono::Duration::minutes(4)).to_rfc3339() }),
            ),
            &token,
        ))
        .await;
    assert!(
        r4["triggered_maintenance_plan_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == PLAN_LAMP),
        "累計型讀表的門檻是週期，跨 10000 應再觸發一次：{r4}"
    );

    // ---- 觸發時要有 outbox 事件（與讀數同一個交易）----
    let mut probe = ctx.tenant_tx().await;
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.event_outbox
          WHERE event_type = 'maintenance.meter_threshold_reached'
            AND aggregate_id = $1::uuid",
    )
    .bind(SEED_PROJECTOR)
    .fetch_one(&mut *probe)
    .await
    .expect("count events");
    assert!(
        events >= 2,
        "兩次觸發應各留下一筆 outbox 事件，實際 {events} 筆"
    );
    drop(probe);

    // ---- 累計型不得倒退 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                json!({ "value": 100, "reading_at": (now + chrono::Duration::minutes(5)).to_rfc3339() }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "累計型讀表倒退應回 422 並說明 rollover_at：{body}"
    );

    // ---- 遲到的讀數：寫入歷史但不推進當前值，也不觸發 ----
    let (status, late) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                json!({
                    "value": 20000,
                    "reading_at": (now - chrono::Duration::days(30)).to_rfc3339(),
                    "source": "IMPORT"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{late}");
    assert_eq!(
        late["last_value"].as_f64(),
        Some(10050.0),
        "遲到的讀數不該推進當前值：{late}"
    );
    assert_eq!(
        late["triggered_maintenance_plan_ids"],
        json!([]),
        "補登舊讀數不該產生今天的保養：{late}"
    );

    // ---- 不存在的讀表代碼 → 404 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/NO_SUCH_METER/readings"),
                json!({ "value": 1, "reading_at": now.to_rfc3339() }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // ---- 契約未列的 source（IOT 走 ingest_telemetry）→ 422 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                json!({ "value": 11000, "reading_at": now.to_rfc3339(), "source": "IOT" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 缺必填欄位 → 422 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                json!({ "value": 11000 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 冪等：同一個鍵重送應回原本的回應而不是再寫一筆 ----
    let key = format!("meter-{}", uuid::Uuid::new_v4());
    let payload = json!({
        "value": 11000,
        "reading_at": (now + chrono::Duration::minutes(10)).to_rfc3339()
    });
    let (s1, first) = ctx
        .send(authed_idem(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                payload.clone(),
            ),
            &token,
            &key,
        ))
        .await;
    assert_eq!(s1, StatusCode::CREATED, "{first}");
    let (s2, replay) = ctx
        .send(authed_idem(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                payload,
            ),
            &token,
            &key,
        ))
        .await;
    assert_eq!(s2, StatusCode::CREATED, "重放應回原本的狀態碼");
    assert_eq!(replay, first, "重放的回應必須逐欄位相同：{replay}");
}
