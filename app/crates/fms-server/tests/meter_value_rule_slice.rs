//! 讀數推進規則只有一份（migration 030）。
//!
//! WBS 4.1h(a) 記載的落差：`fms.ingest_telemetry` 一律 `last_value = value`，
//! 對 DELTA 型讀表把增量寫成總量。而 S4c 的人工登錄端點把規則實作對了 ——
//! 在 **Rust** 裡。於是同一條規則有兩份，其中一份是錯的。
//!
//! 這比「有一個 bug」更糟：同一支讀表，人工登錄與 IoT 上報會推進出不同的
//! `last_value`，而 PM 的門檻觸發讀的正是它。保養會不會被觸發，取決於讀數
//! 是誰送進來的。
//!
//! 因此這裡斷言的核心是**兩條路徑一致**，而不只是「IoT 路徑修好了」。
//! 既有的 `catalog_meter_slice` 已覆蓋人工登錄端點的行為；本檔補上 IoT 路徑，
//! 並直接比對兩者。

mod common;

use common::*;

/// 009 的 LAMP_HOURS 讀表（CUMULATIVE，last_value 4820）。
const METER_LAMP: &str = "30000000-0000-4000-8000-000000000001";
/// 4F 空調箱 —— 掛新讀表與遙測點位用。
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";
const DEVICE_AHU: &str = "a2000000-0000-4000-8000-000000000001";

async fn last_value(ctx: &TestContext, meter_id: &str) -> Option<f64> {
    let mut tx = ctx.owner_tx().await;
    sqlx::query_scalar("SELECT last_value::float8 FROM fms.asset_meters WHERE id = $1::uuid")
        .bind(meter_id)
        .fetch_one(&mut *tx)
        .await
        .expect("read last_value")
}

/// 建一支 DELTA 讀表並掛上遙測點位，回傳 (meter_id, point_id)。
///
/// 種子資料裡沒有 DELTA 型讀表，也沒有任何 `telemetry_points.asset_meter_id`
/// 有值的點位 —— 也就是說**那條有缺陷的路徑從來沒有被任何測試走過**，
/// 這正是它能長期存在的原因。
async fn delta_meter_with_point(ctx: &TestContext) -> (String, String) {
    let mut tx = ctx.owner_tx().await;
    let meter: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.asset_meters
           (tenant_id, asset_id, meter_code, name, unit, reading_type, last_value)
         SELECT a.tenant_id, a.id, 'PULSE_KWH', '脈衝電表', 'kWh', 'DELTA', 1000
           FROM fms.assets a WHERE a.id = $1::uuid
         RETURNING id",
    )
    .bind(SEED_AHU)
    .fetch_one(&mut *tx)
    .await
    .expect("create delta meter");

    let point: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.telemetry_points
           (tenant_id, device_id, point_code, name, data_type, asset_meter_id)
         SELECT d.tenant_id, d.id, 'PULSE', '脈衝', 'NUMBER', $2::uuid
           FROM fms.devices d WHERE d.id = $1::uuid
         RETURNING id",
    )
    .bind(DEVICE_AHU)
    .bind(meter)
    .fetch_one(&mut *tx)
    .await
    .expect("create telemetry point");

    tx.commit().await.expect("commit");
    (meter.to_string(), point.to_string())
}

/// 以平台情境執行一次 `ingest_telemetry`。
///
/// 不能用裸的 owner 連線：`telemetry_points` 有 FORCE RLS，沒有情境時
/// 函式裡的 `SELECT` 回 0 筆，`ingest_telemetry` 會回報「point not found」
/// —— 那是 RLS 生效的正確表現，不是測試設定錯誤。
async fn ingest(
    ctx: &TestContext,
    point: &str,
    offset_seconds: i64,
    value: f64,
) -> Result<(), sqlx::Error> {
    let mut tx = ctx.owner_tx().await;
    let r = sqlx::query(
        "SELECT fms.ingest_telemetry($1::uuid, now() + ($2 || ' seconds')::interval, $3::float8::numeric)",
    )
    .bind(point)
    .bind(offset_seconds.to_string())
    .bind(value)
    .execute(&mut *tx)
    .await
    .map(|_| ());
    if r.is_ok() {
        tx.commit().await?;
    }
    r
}

#[tokio::test]
async fn iot_ingest_accumulates_a_delta_meter_instead_of_overwriting_it() {
    let ctx = &TestContext::setup().await;
    let (meter, point) = delta_meter_with_point(ctx).await;

    assert_eq!(last_value(ctx, &meter).await, Some(1000.0), "起始值");

    // 上報兩筆各 25 的增量。修正前 last_value 會是 25（把增量寫成總量）；
    // 修正後應是 1000 + 25 + 25。
    for i in 0..2 {
        ingest(ctx, &point, i, 25.0).await.expect("ingest");
    }

    assert_eq!(
        last_value(ctx, &meter).await,
        Some(1050.0),
        "DELTA 型讀表的 IoT 上報應累加，而不是把增量寫成總量"
    );

    // 讀數列存的是**原始上報值**（增量），不是推進後的總量 ——
    // 那是這筆觀測的事實，改寫它等於偽造原始資料。
    {
        let mut tx = ctx.owner_tx().await;
        let values: Vec<f64> = sqlx::query_scalar(
            "SELECT value::float8 FROM fms.asset_meter_readings
              WHERE asset_meter_id = $1::uuid AND source = 'IOT' ORDER BY reading_at",
        )
        .bind(&meter)
        .fetch_all(&mut *tx)
        .await
        .expect("read readings");
        assert_eq!(values, vec![25.0, 25.0], "讀數列應保留原始增量");
    }

    ctx.teardown().await;
}

#[tokio::test]
async fn iot_ingest_rejects_a_cumulative_meter_going_backwards() {
    let ctx = &TestContext::setup().await;

    // LAMP_HOURS 是 CUMULATIVE、last_value 4820，且沒有 rollover_at。
    // 掛一個點位上去，然後上報一個更小的值。
    let point: uuid::Uuid = {
        let mut tx = ctx.owner_tx().await;
        let p = sqlx::query_scalar(
            "INSERT INTO fms.telemetry_points
               (tenant_id, device_id, point_code, name, data_type, asset_meter_id)
             SELECT d.tenant_id, d.id, 'LAMPH', '燈時', 'NUMBER', $2::uuid
               FROM fms.devices d WHERE d.id = $1::uuid
             RETURNING id",
        )
        .bind(DEVICE_AHU)
        .bind(uuid::Uuid::parse_str(METER_LAMP).unwrap())
        .fetch_one(&mut *tx)
        .await
        .expect("create point");
        tx.commit().await.expect("commit");
        p
    };

    let err = ingest(ctx, &point.to_string(), 0, 100.0)
        .await
        .expect_err("累計型讀表倒退應被拒絕");

    let msg = err.to_string();
    assert!(
        msg.contains("METER_VALUE_INVALID"),
        "錯誤應帶穩定標記讓應用層轉成 422，實際：{msg}"
    );
    assert!(
        msg.contains("rollover_at"),
        "訊息應指出可行動的修法（設 rollover_at），實際：{msg}"
    );

    // 值不該被改動 —— 整個 ingest 在同一個交易裡，失敗就全部回滾。
    assert_eq!(last_value(ctx, METER_LAMP).await, Some(4820.0));

    ctx.teardown().await;
}

/// 兩條路徑必須一致。
///
/// 這是本檔的重點：不是「IoT 修好了」，而是「同一支讀表不會因為讀數來源
/// 不同而推進出不同的值」。
#[tokio::test]
async fn the_manual_and_iot_paths_agree() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (meter, point) = delta_meter_with_point(ctx).await;

    // 人工登錄 30（走 HTTP 端點 → repo::next_meter_value）
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/assets/{SEED_AHU}/meters/PULSE_KWH/readings"
        ))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({ "value": 30, "reading_at": chrono::Utc::now().to_rfc3339() })
                .to_string(),
        ))
        .unwrap();
    let (status, body) = ctx.send(authed(req, &token)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    let after_manual = last_value(ctx, &meter).await.expect("有值");
    assert_eq!(after_manual, 1030.0, "人工登錄應累加");

    // IoT 上報同樣的 30（走 ingest_telemetry → fms.next_meter_value）
    ingest(ctx, &point, 1, 30.0).await.expect("ingest");
    let after_iot = last_value(ctx, &meter).await.expect("有值");

    assert_eq!(
        after_iot - after_manual,
        30.0,
        "IoT 上報的推進量必須與人工登錄相同（修正前 IoT 會把 last_value 設成 30）"
    );

    ctx.teardown().await;
}
