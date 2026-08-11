//! 遙測讀取（`/telemetry/latest`、`/telemetry/series`）與 060 的場域收斂。
//!
//! # `a_` 是這個檔案存在的主要理由
//!
//! `telemetry:read` 的 `min_scope_level` 是 `FACILITY`，而 006 建的三張遙測表
//! 只有 tenant-only 的政策。量過（060 檔頭記著數字）：場域管理員看不到別場域的
//! **裝置**，卻看得到那台裝置的**點位、讀數與最新值**。
//!
//! 在這兩支端點之前沒有人讀那三張表，所以那個洞沒有出口。這兩支就是出口。
//! `a_` 把它釘住 —— 060 被回退或政策被改鬆，這條就會紅。
//!
//! # `d_` 釘住的是「圖表會不會說謊」
//!
//! 降採樣只回 avg 會讓五分鐘桶裡衝到 31 度的三秒鐘消失，
//! 而告警門檻是「超過 28」。所以每個桶要回 min／max。
//!
//! # `e_` 釘住桶邊界的絕對性
//!
//! `date_bin` 的 origin 若用 `from`，`from=10:02` 與 `from=10:00` 會切出錯開的桶，
//! 兩次查詢的數列無法疊圖。origin 固定，所以不同的 `from` 必須給出
//! **相同的桶起點**。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::Value;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// 在**別的場域**建一台裝置、一個點位、一筆讀數與最新值。
///
/// 回傳 `(facility_id, device_id, point_id)`。
async fn seed_other_facility(ctx: &TestContext) -> (uuid::Uuid, uuid::Uuid, uuid::Uuid) {
    let mut tx = ctx.owner_tx().await;
    let facility: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM fms.facilities
          WHERE tenant_id = $1::uuid AND id <> $2::uuid LIMIT 1",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("示範資料該有第二個場域");

    tx.commit().await.expect("commit");
    let (device, point) = ctx
        .seed_device_with_point(&facility.to_string(), "LEAK_PROBE")
        .await;
    let mut tx = ctx.owner_tx().await;

    sqlx::query(
        "INSERT INTO fms.telemetry_readings
           (tenant_id, telemetry_point_id, observed_at, value_num)
         VALUES ($1::uuid, $2::uuid, now() - interval '5 minutes', 42)",
    )
    .bind(TENANT_ID)
    .bind(point)
    .execute(&mut *tx)
    .await
    .expect("建讀數");

    sqlx::query(
        "INSERT INTO fms.telemetry_latest
           (telemetry_point_id, tenant_id, device_id, observed_at, value_num)
         VALUES ($1::uuid, $2::uuid, $3::uuid, now() - interval '5 minutes', 42)",
    )
    .bind(point)
    .bind(TENANT_ID)
    .bind(device)
    .execute(&mut *tx)
    .await
    .expect("建最新值");

    tx.commit().await.expect("commit");
    (facility, device, point)
}

/// 把讀數灌進**上一個完整的 5 分鐘桶**裡，回傳 point_id。
///
/// 為什麼不能用「N 秒前」：桶邊界是絕對的（`date_bin` 的 origin 固定，
/// 見 `e_`）。「10/20/30/40 秒前」這種相對時刻會在「現在」剛好落在
/// 5 分鐘邊界後不久時**被切到兩個桶**，於是「找含四筆的桶」找不到。
///
/// 本機曾經靠運氣過，CI 上踩到了（四筆變成兩個桶各兩筆）。
/// 所以時刻由 SQL 的 `date_bin` 算出來，整批保證落在同一個桶內、
/// 而且整批都在過去（用**上一個**桶而不是當前桶 —— 當前桶的後半段
/// 可能還在未來，會被 `observed_at < to` 排掉）。
async fn seed_hq_readings_in_one_bucket(ctx: &TestContext, values: &[f64]) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let point: uuid::Uuid = sqlx::query_scalar(
        "SELECT p.id FROM fms.telemetry_points p
           JOIN fms.devices d ON d.id = p.device_id
          WHERE d.facility_id = $1::uuid ORDER BY p.point_code LIMIT 1",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("總部該有點位");

    for (i, v) in values.iter().enumerate() {
        sqlx::query(
            "INSERT INTO fms.telemetry_readings
               (tenant_id, telemetry_point_id, observed_at, value_num)
             VALUES ($1::uuid, $2::uuid,
                     date_bin('5 minutes', now(), timestamptz '2000-01-01 00:00:00+00')
                       - interval '5 minutes'
                       + make_interval(secs => $3::int),
                     $4)",
        )
        .bind(TENANT_ID)
        .bind(point)
        .bind((i as i32) * 10 + 5)
        .bind(v)
        .execute(&mut *tx)
        .await
        .expect("建讀數");
    }
    tx.commit().await.expect("commit");
    point
}

/// 在總部的既有點位灌讀數。回傳 point_id。
async fn seed_hq_readings(ctx: &TestContext, values: &[(i64, f64)]) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let point: uuid::Uuid = sqlx::query_scalar(
        "SELECT p.id FROM fms.telemetry_points p
           JOIN fms.devices d ON d.id = p.device_id
          WHERE d.facility_id = $1::uuid ORDER BY p.point_code LIMIT 1",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("總部該有點位");

    for (secs_ago, v) in values {
        sqlx::query(
            "INSERT INTO fms.telemetry_readings
               (tenant_id, telemetry_point_id, observed_at, value_num)
             VALUES ($1::uuid, $2::uuid, now() - ($3::bigint || ' seconds')::interval, $4)",
        )
        .bind(TENANT_ID)
        .bind(point)
        .bind(secs_ago)
        .bind(v)
        .execute(&mut *tx)
        .await
        .expect("建讀數");
    }
    tx.commit().await.expect("commit");
    point
}

/// **場域管理員讀不到別場域的遙測。** 060 的行為驗證。
///
/// 在 060 之前這三個斷言全都會失敗 —— 而失敗的方式是「多回了資料」，
/// 也就是沉默的洩漏。
#[tokio::test]
async fn a_a_facility_scoped_reader_cannot_read_another_facilitys_telemetry() {
    let ctx = TestContext::setup().await;
    let (other_facility, other_device, other_point) = seed_other_facility(&ctx).await;

    // fm.lin 是 FACILITY_ADMIN，範圍只在總部。
    let token = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    // (1) 最新值：明確指名那個點位也拿不到。
    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/telemetry/latest?point_ids={other_point}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["items"].as_array().map(Vec::len),
        Some(0),
        "別場域的最新值不該看得到；實際 {}",
        body["items"]
    );

    // (2) 時序：同樣指名點位。
    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/telemetry/series?point_ids={other_point}&interval=raw"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["items"].as_array().map(Vec::len),
        Some(0),
        "別場域的讀數不該看得到；實際 {}",
        body["items"]
    );

    // (3) 整個場域問也一樣 —— 而且這裡問的是**別人的** facility_id，
    //     所以權限檢查本身就該擋（403），不是靠 RLS 回空清單。
    let (status, _) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/telemetry/latest?facility_id={other_facility}"
            )),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "拿別場域的 facility_id 問，該在權限層就被擋掉"
    );

    // (4) 裝置也看不到 —— 這一條在 060 之前就已經是對的（devices 有場域政策），
    //     放在這裡是為了對比：問題從來不在 devices。
    let (status, body) = ctx.send(authed(get("/api/v1/devices"), &token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let codes: Vec<&str> = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|d| d["device_code"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !codes.contains(&"LEAK_PROBE"),
        "別場域的裝置不該出現：{codes:?}"
    );
    let _ = other_device;

    ctx.teardown().await;
}

/// 租戶管理員（範圍是整個租戶）**該**看得到兩個場域 ——
/// 否則上面那條就可能是「修成什麼都看不到」。
#[tokio::test]
async fn b_a_tenant_scoped_reader_still_sees_every_facility() {
    let ctx = TestContext::setup().await;
    let (_f, _d, other_point) = seed_other_facility(&ctx).await;
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/telemetry/latest?point_ids={other_point}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["items"].as_array().map(Vec::len),
        Some(1),
        "租戶級讀者該看得到；實際 {}",
        body["items"]
    );

    ctx.teardown().await;
}

/// 最新值要帶年齡與 stale 判定，而 stale 用**裝置自己的**門檻。
#[tokio::test]
async fn c_latest_reports_age_and_staleness_from_the_device_threshold() {
    let ctx = TestContext::setup().await;
    let point = seed_hq_readings(&ctx, &[(60, 24.0)]).await;

    // 讓最新值是 60 秒前，而門檻設 30 秒 → stale。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.telemetry_latest
               (telemetry_point_id, tenant_id, device_id, observed_at, value_num)
             SELECT p.id, p.tenant_id, p.device_id, now() - interval '60 seconds', 24
               FROM fms.telemetry_points p WHERE p.id = $1::uuid
             ON CONFLICT (telemetry_point_id) DO UPDATE
                SET observed_at = excluded.observed_at, value_num = excluded.value_num",
        )
        .bind(point)
        .execute(&mut *tx)
        .await
        .expect("最新值");
        sqlx::query(
            "UPDATE fms.devices SET offline_alarm_after_seconds = 30
              WHERE id = (SELECT device_id FROM fms.telemetry_points WHERE id = $1::uuid)",
        )
        .bind(point)
        .execute(&mut *tx)
        .await
        .expect("門檻");
        tx.commit().await.expect("commit");
    }

    let token = ctx.login().await;
    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/telemetry/latest?point_ids={point}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let item = &body["items"][0];
    assert!(
        item["age_seconds"].as_i64().unwrap_or(0) >= 59,
        "age_seconds 該約 60；實際 {}",
        item["age_seconds"]
    );
    assert_eq!(
        item["is_stale"], true,
        "60 秒前的讀數對 30 秒門檻是 stale；實際 {item}"
    );
    assert_eq!(body["meta"]["stale_count"], 1);

    // 把門檻放寬到 300 秒，同一筆讀數就不再 stale ——
    // 證明判定真的用了裝置的欄位，不是寫死的數字。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.devices SET offline_alarm_after_seconds = 300
              WHERE id = (SELECT device_id FROM fms.telemetry_points WHERE id = $1::uuid)",
        )
        .bind(point)
        .execute(&mut *tx)
        .await
        .expect("放寬門檻");
        tx.commit().await.expect("commit");
    }
    let (_s, body) = ctx
        .send(authed(
            get(&format!("/api/v1/telemetry/latest?point_ids={point}")),
            &token,
        ))
        .await;
    assert_eq!(
        body["items"][0]["is_stale"], false,
        "門檻放寬後同一筆讀數不該是 stale —— 否則門檻沒有被讀"
    );
    assert_eq!(body["meta"]["stale_count"], 0);

    ctx.teardown().await;
}

/// **降採樣不能把尖峰抹掉。** 桶裡有一筆 31 度，avg 會低於門檻而 max 不會。
#[tokio::test]
async fn d_a_downsampled_bucket_keeps_the_peak() {
    let ctx = TestContext::setup().await;
    // 同一個桶內四筆：三筆 22 度、一筆 31 度。avg = 24.25，max = 31。
    let point = seed_hq_readings_in_one_bucket(&ctx, &[22.0, 22.0, 22.0, 31.0]).await;
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/telemetry/series?point_ids={point}&interval=5m"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 找到含這四筆的那個桶（sample_count 至少 4）。
    let bucket = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|b| b["sample_count"].as_i64().unwrap_or(0) >= 4)
        .unwrap_or_else(|| panic!("找不到含四筆的桶：{}", body["items"]));

    let avg = bucket["avg_value"].as_f64().expect("avg");
    let max = bucket["max_value"].as_f64().expect("max");
    let min = bucket["min_value"].as_f64().expect("min");
    assert!(
        avg < 28.0,
        "這個桶的 avg 該低於門檻 28（尖峰被平均掉了）；實際 {avg}"
    );
    assert!(
        max >= 31.0,
        "max 必須留住 31 度那一筆 —— 只回 avg 的圖表看不到它；實際 {max}"
    );
    assert!(min <= 22.0, "min 該是 22；實際 {min}");
    assert_eq!(body["meta"]["bucket_seconds"], 300);
    assert_eq!(body["meta"]["truncated"], false);

    ctx.teardown().await;
}

/// **桶的邊界與 `from` 無關。** 不同的 `from` 必須落在同一個桶起點上。
#[tokio::test]
async fn e_bucket_boundaries_do_not_move_with_the_query_window() {
    let ctx = TestContext::setup().await;
    let point = seed_hq_readings(&ctx, &[(30, 25.0)]).await;
    let token = ctx.login().await;

    let mut starts = Vec::new();
    // 兩個錯開的視窗（差 137 秒，刻意不是 5 分鐘的倍數）。
    for offset in [600i64, 737i64] {
        let from = chrono::Utc::now() - chrono::Duration::seconds(offset);
        let (status, body) = ctx
            .send(authed(
                get(&format!(
                    "/api/v1/telemetry/series?point_ids={point}&interval=5m&from={}",
                    // `Z` 結尾而不是 `+00:00`：查詢字串裡的 `+` 會被解成空白，
                    // 而那個 bug 只在有時區偏移的時候出現。
                    from.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                )),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let s = body["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|b| b["bucket_start"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert!(
            !s.is_empty(),
            "offset={offset} 該有桶；實際 {}",
            body["items"]
        );
        starts.push(s);
    }

    // 兩次查詢共有的那一筆讀數，必須落在同一個桶起點。
    let shared: Vec<&String> = starts[0].iter().filter(|s| starts[1].contains(s)).collect();
    assert!(
        !shared.is_empty(),
        "兩個錯開的視窗切出的桶起點完全不重疊 —— origin 跟著 from 跑了：\n{:?}\n{:?}",
        starts[0],
        starts[1]
    );

    ctx.teardown().await;
}

/// 打錯的 `interval` 必須是 422，不能安靜地變成 raw（那會回幾萬筆）。
/// 桶數超上限也必須在送出查詢前被擋下來。
#[tokio::test]
async fn f_bad_interval_and_oversized_windows_are_rejected() {
    let ctx = TestContext::setup().await;
    let point = seed_hq_readings(&ctx, &[(30, 25.0)]).await;
    let token = ctx.login().await;

    for bad in ["5x", "0m", "-5m", "m"] {
        let (status, _) = ctx
            .send(authed(
                get(&format!(
                    "/api/v1/telemetry/series?point_ids={point}&interval={bad}"
                )),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "interval={bad} 該是 422"
        );
    }

    // 一年 + 1s → 3100 萬個桶。
    let from = chrono::Utc::now() - chrono::Duration::days(365);
    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/telemetry/series?point_ids={point}&interval=1s&from={}",
                from.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // 沒有範圍也該擋 —— 否則會是整個租戶的每一個點位。
    let (status, _) = ctx
        .send(authed(get("/api/v1/telemetry/latest"), &token))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// `raw` 與分桶的回應形狀相同 —— 前端不必為 raw 走另一條分支。
#[tokio::test]
async fn g_raw_and_bucketed_responses_have_the_same_shape() {
    let ctx = TestContext::setup().await;
    let point = seed_hq_readings(&ctx, &[(10, 22.0), (20, 26.0)]).await;
    let token = ctx.login().await;

    let (_s, raw) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/telemetry/series?point_ids={point}&interval=raw"
            )),
            &token,
        ))
        .await;
    let (_s, binned) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/telemetry/series?point_ids={point}&interval=1h"
            )),
            &token,
        ))
        .await;

    let keys = |v: &Value| -> Vec<String> {
        let mut k: Vec<String> = v["items"][0]
            .as_object()
            .expect("桶是物件")
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };
    assert_eq!(keys(&raw), keys(&binned), "raw 與分桶的欄位集合必須相同");
    // raw 的每一筆自己是一個桶。
    assert_eq!(raw["items"][0]["sample_count"], 1);
    assert_eq!(raw["meta"]["bucket_seconds"], Value::Null);

    ctx.teardown().await;
}
