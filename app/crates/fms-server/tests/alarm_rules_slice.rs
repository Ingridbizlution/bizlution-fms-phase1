//! 告警規則（`/alarm-rules`）。
//!
//! # `b_` 是這個檔案存在的主要理由
//!
//! 試跑與真跑必須給出**同一個數字**。`POST /alarm-rules/{id}/test` 不能呼叫
//! `evaluate_telemetry_rules`（那會真的建告警），所以它得自己判斷 ——
//! 而那就是同一套語意的第二份實作。
//!
//! migration 061 把兩個述詞抽成 `fms.telemetry_rule_fires` 與
//! `fms.alarm_rule_covers_point`，並改寫 057 的評估器去呼叫它們。
//! `b_` 拿真跑的 `alarms_raised` 與試跑的 `would_fire` 對比，
//! 把那件事釘住 —— 兩邊漂移的症狀是「試跑說會響 3 次，上線後響 0 次」，
//! 而使用者對這個系統的信任正建立在那個預覽上。
//!
//! # 「設了等於沒設」必須查得出來
//!
//! `c_`：`point_code` 打錯 → `covered_point_count = 0`。
//! `d_`：持續型與非 THRESHOLD → `evaluable = false`，而試跑回 `reason`
//! 而不是 `would_fire: 0`（回 0 會讓人以為門檻很安全）。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 總部第一個點位的 (id, point_code, device_code)。
async fn hq_point(ctx: &TestContext) -> (uuid::Uuid, String, String) {
    let mut tx = ctx.owner_tx().await;
    let row: (uuid::Uuid, String, String) = sqlx::query_as(
        "SELECT p.id, p.point_code::text, d.device_code::text
           FROM fms.telemetry_points p
           JOIN fms.devices d ON d.id = p.device_id
          WHERE d.facility_id = $1::uuid AND p.data_type = 'NUMBER'
          ORDER BY p.point_code LIMIT 1",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("總部該有數值型點位");
    tx.commit().await.expect("commit");
    row
}

/// 建一條規則，回傳 id。
async fn create_rule(ctx: &TestContext, token: &str, body: Value) -> uuid::Uuid {
    let (status, resp) = ctx
        .send(authed(post("/api/v1/alarm-rules", body), token))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{resp}");
    uuid::Uuid::parse_str(resp["id"].as_str().expect("id")).expect("uuid")
}

/// 停用示範資料的規則，讓測試只看自己建的那一條。
///
/// 009 的 seed 已經有告警規則，而 `evaluate_telemetry_rules` 會評估**所有**
/// 符合的規則 —— 不停掉它們，`alarms_raised` 會混進別人的數字。
async fn silence_seed_rules(ctx: &TestContext) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("UPDATE fms.alarm_rules SET is_active = false")
        .execute(&mut *tx)
        .await
        .expect("停用既有規則");
    tx.commit().await.expect("commit");
}

/// 建立時就擋掉「永遠不會響」的 condition。
#[tokio::test]
async fn a_conditions_that_could_never_fire_are_rejected_at_creation() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (_id, point_code, _dev) = hq_point(&ctx).await;

    // op 打錯：061 的 telemetry_rule_fires 會回 NULL，真跑時進 bad_rule_codes
    // —— 也就是設了卻永遠不觸發，而且沒有錯誤訊息。
    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/alarm-rules",
                json!({
                    "code": "BAD_OP", "name": "運算子打錯",
                    "point_code": point_code,
                    "condition": { "op": "=>", "value": 28 },
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"].as_str().unwrap_or("").contains("永遠不觸發"),
        "訊息要說出後果，否則使用者不知道為什麼被擋：{body}"
    );

    // 缺 value。
    let (status, _) = ctx
        .send(authed(
            post(
                "/api/v1/alarm-rules",
                json!({
                    "code": "NO_VALUE", "name": "缺門檻值",
                    "point_code": point_code,
                    "condition": { "op": ">" },
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 沒有任何點位範圍。
    let (status, _) = ctx
        .send(authed(
            post(
                "/api/v1/alarm-rules",
                json!({
                    "code": "NO_SCOPE", "name": "沒有範圍",
                    "condition": { "op": ">", "value": 28 },
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // notify_role_codes 指向不存在的角色 → 設了通知而沒有人會收到。
    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/alarm-rules",
                json!({
                    "code": "GHOST_ROLE", "name": "通知不存在的角色",
                    "point_code": point_code,
                    "condition": { "op": ">", "value": 28 },
                    "notify_role_codes": ["NOBODY_HOME"],
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["detail"]
        .as_str()
        .unwrap_or("")
        .contains("NOBODY_HOME"));

    ctx.teardown().await;
}

/// **試跑與真跑給同一個數字。** 061 唯一的理由。
#[tokio::test]
async fn b_the_dry_run_agrees_with_the_real_evaluator() {
    let ctx = TestContext::setup().await;
    silence_seed_rules(&ctx).await;
    let token = ctx.login().await;
    let (point_id, _code, device_code) = hq_point(&ctx).await;

    let rule = create_rule(
        &ctx,
        &token,
        json!({
            "code": "AGREE", "name": "一致性測試",
            "telemetry_point_id": point_id,
            "condition": { "op": ">", "value": 28 },
        }),
    )
    .await;

    // 走**真的**寫入路徑：五筆讀數，其中兩筆越界（29、31）。
    //
    // **28.0 那一筆是刻意的邊界值。** 少了它，把 `>` 寫成 `>=` 的漂移
    // 完全看不出來 —— 兩者只在剛好等於門檻時有差。突變測試證實過：
    // 沒有這一筆的版本，評估器改用 `>=` 之後這條測試仍然全綠。
    let now = chrono::Utc::now();
    let readings: Vec<Value> = [
        (-50i64, 25.0),
        (-40, 28.0),
        (-30, 29.0),
        (-20, 31.0),
        (-10, 27.0),
    ]
    .iter()
    .map(|(secs, v)| {
        json!({
            "device_code": device_code,
            "point_code": _code,
            "observed_at": (now + chrono::Duration::seconds(*secs))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "value_num": v,
        })
    })
    .collect();

    let (status, body) = ctx
        .send(authed(
            post(
                "/api/v1/telemetry:batch-ingest",
                json!({ "readings": readings }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["accepted"], 5, "五筆都該寫進去；{body}");
    let real_fires = body["alarms_raised"].as_i64().expect("alarms_raised");
    assert_eq!(
        real_fires, 2,
        "29 與 31 越過門檻 28；25、27 與**剛好 28** 那筆沒有（`>` 不含等於）；實際 {body}"
    );

    // 現在試跑同一個視窗。
    let from =
        (now - chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, dry) = ctx
        .send(authed(
            post(
                &format!("/api/v1/alarm-rules/{rule}/test?from={from}"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{dry}");
    assert_eq!(dry["evaluable"], true, "{dry}");
    assert_eq!(
        dry["would_fire"].as_i64(),
        Some(real_fires),
        "**試跑與真跑不一致** —— 這正是 061 要防的漂移。\n真跑 {real_fires} 次，試跑 {}",
        dry["would_fire"]
    );
    assert_eq!(dry["readings_scanned"], 5);
    assert_eq!(dry["not_evaluable_readings"], 0);
    assert_eq!(
        dry["peak_value"].as_f64(),
        Some(31.0),
        "越界時最極端的值該是 31；{dry}"
    );
    assert_eq!(dry["meta"]["truncated"], false);

    // 覆寫門檻試跑：改成 30 只有 31 那筆越界，而且**不會寫入**。
    let (status, dry30) = ctx
        .send(authed(
            post(
                &format!("/api/v1/alarm-rules/{rule}/test?from={from}"),
                json!({ "condition": { "op": ">", "value": 30 } }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{dry30}");
    assert_eq!(dry30["would_fire"], 1, "門檻改 30 只剩 31 越界；{dry30}");

    let (_s, listed) = ctx
        .send(authed(get("/api/v1/alarm-rules?is_active=true"), &token))
        .await;
    let stored = listed["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|r| r["code"] == "AGREE")
        .expect("該找得到");
    assert_eq!(
        stored["condition"]["value"], 28,
        "覆寫試跑不能寫回資料庫；實際 {stored}"
    );

    ctx.teardown().await;
}

/// `point_code` 打錯 → 管不到任何點位。`ineffective_only` 要找得到它。
#[tokio::test]
async fn c_a_rule_that_covers_no_points_is_findable() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (point_id, good_code, _dev) = hq_point(&ctx).await;

    create_rule(
        &ctx,
        &token,
        json!({
            "code": "TYPO", "name": "點位代碼打錯",
            "point_code": "TEMP_SUPPLZ",
            "condition": { "op": ">", "value": 28 },
        }),
    )
    .await;
    create_rule(
        &ctx,
        &token,
        json!({
            "code": "GOOD", "name": "正常規則",
            "telemetry_point_id": point_id,
            "condition": { "op": ">", "value": 28 },
        }),
    )
    .await;

    let (status, body) = ctx.send(authed(get("/api/v1/alarm-rules"), &token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let find = |code: &str| -> Value {
        body["data"]
            .as_array()
            .expect("data")
            .iter()
            .find(|r| r["code"] == code)
            .cloned()
            .unwrap_or_else(|| panic!("找不到 {code}"))
    };
    assert_eq!(
        find("TYPO")["covered_point_count"],
        0,
        "打錯的 point_code 該管到 0 個點位"
    );
    assert!(
        find("GOOD")["covered_point_count"].as_i64().unwrap_or(0) >= 1,
        "指名點位的規則該至少管到 1 個"
    );
    let _ = good_code;

    // ineffective_only 一次問出「哪些規則設了等於沒設」。
    let (_s, ineff) = ctx
        .send(authed(
            get("/api/v1/alarm-rules?ineffective_only=true"),
            &token,
        ))
        .await;
    let codes: Vec<&str> = ineff["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|r| r["code"].as_str().unwrap_or(""))
        .collect();
    assert!(codes.contains(&"TYPO"), "{codes:?}");
    assert!(!codes.contains(&"GOOD"), "正常規則不該在裡面：{codes:?}");

    ctx.teardown().await;
}

/// 持續型與非 THRESHOLD：`evaluable = false`，而試跑回原因而不是 0。
#[tokio::test]
async fn d_rules_the_evaluator_skips_say_so_instead_of_reporting_zero() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (point_id, _c, _d) = hq_point(&ctx).await;

    let sustained = create_rule(
        &ctx,
        &token,
        json!({
            "code": "SUSTAINED", "name": "持續型",
            "telemetry_point_id": point_id,
            "condition": { "op": ">", "value": 28, "for_seconds": 300 },
        }),
    )
    .await;
    let offline = create_rule(
        &ctx,
        &token,
        json!({
            "code": "OFFLINE_RULE", "name": "裝置離線",
            "rule_type": "DEVICE_OFFLINE",
            "condition": { "grace_seconds": 900 },
        }),
    )
    .await;

    for (id, needle) in [(sustained, "for_seconds"), (offline, "THRESHOLD")] {
        let (status, dry) = ctx
            .send(authed(
                post(&format!("/api/v1/alarm-rules/{id}/test"), json!({})),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{dry}");
        assert_eq!(
            dry["evaluable"], false,
            "這種規則現在不會被評估；實際 {dry}"
        );
        assert!(
            dry["reason"].as_str().unwrap_or("").contains(needle),
            "原因要說出是哪一種情況（找 {needle}）：{dry}"
        );
        assert!(
            dry.get("would_fire").is_none(),
            "**不能回 would_fire: 0** —— 那會讓人以為門檻很安全；實際 {dry}"
        );
    }

    // 清單也要標出來。
    let (_s, body) = ctx.send(authed(get("/api/v1/alarm-rules"), &token)).await;
    for code in ["SUSTAINED", "OFFLINE_RULE"] {
        let r = body["data"]
            .as_array()
            .expect("data")
            .iter()
            .find(|r| r["code"] == code)
            .unwrap_or_else(|| panic!("找不到 {code}"));
        assert_eq!(r["evaluable"], false, "{code} 該標為評估不了：{r}");
    }

    ctx.teardown().await;
}

/// `alarm_rule:read`（060 補的）足夠讀清單 —— 不必有 write。
///
/// 這條驗的是 060 的授予：在它之前只有 `alarm_rule:write`，
/// 於是技師看得到告警卻看不到產生它的門檻。
#[tokio::test]
async fn e_reading_rules_needs_only_the_new_read_permission() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (point_id, _c, _d) = hq_point(&ctx).await;
    create_rule(
        &ctx,
        &token,
        json!({
            "code": "VISIBLE", "name": "技師該看得到",
            "telemetry_point_id": point_id,
            "condition": { "op": ">", "value": 28 },
        }),
    )
    .await;

    // TECHNICIAN 有 alarm:read（因此 060 給了他 alarm_rule:read），沒有 write。
    let tech = ctx.login_as(USERNAME_TECHNICIAN_HQ).await;
    let (status, body) = ctx.send(authed(get("/api/v1/alarm-rules"), &tech)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "技師該讀得到規則（060 的 alarm_rule:read）；實際 {body}"
    );
    assert!(
        body["data"]
            .as_array()
            .expect("data")
            .iter()
            .any(|r| r["code"] == "VISIBLE"),
        "{}",
        body["data"]
    );

    // 但建立與試跑要 write。
    let (status, _) = ctx
        .send(authed(
            post(
                "/api/v1/alarm-rules",
                json!({
                    "code": "TECH_RULE", "name": "技師不該建得起來",
                    "telemetry_point_id": point_id,
                    "condition": { "op": ">", "value": 28 },
                }),
            ),
            &tech,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    ctx.teardown().await;
}

/// 這份 Rust 端的 `OPS` 清單與 061 的 `telemetry_rule_fires` 認得同一組運算子。
///
/// 不一致的方向只有一種會出事：Rust 放行、SQL 回 NULL ——
/// 於是一條「通過驗證」的規則永遠不會響。
#[tokio::test]
async fn f_the_accepted_operators_match_the_sql_predicate() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (point_id, _c, _d) = hq_point(&ctx).await;

    // 每一個被 API 接受的 op，SQL 都必須給出非 NULL 的答案。
    for (i, op) in [">", ">=", "<", "<=", "=", "!="].iter().enumerate() {
        let (status, body) = ctx
            .send(authed(
                post(
                    "/api/v1/alarm-rules",
                    json!({
                        "code": format!("OP_{i}"), "name": format!("運算子 {op}"),
                        "telemetry_point_id": point_id,
                        "condition": { "op": op, "value": 28 },
                    }),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "op={op} 該被接受；{body}");

        let mut tx = ctx.owner_tx().await;
        let fires: Option<bool> =
            sqlx::query_scalar("SELECT fms.telemetry_rule_fires($1::jsonb, 29::numeric)")
                .bind(json!({ "op": op, "value": 28 }))
                .fetch_one(&mut *tx)
                .await
                .expect("查述詞");
        tx.commit().await.expect("commit");
        assert!(
            fires.is_some(),
            "API 接受了 op={op}，但 SQL 的述詞回 NULL —— \
             這條規則會通過驗證然後永遠不響"
        );
    }

    ctx.teardown().await;
}
