//! PM 計畫端點與產生器（WBS 5.x）。
//!
//! 重點：
//!   * `preview-schedule` 與產生器用同一份展開邏輯與同一份瞄準規則
//!   * RRULE 展開在**場域當地時區**（`BYMONTHDAY=5` 必須是當地 5 號）
//!   * 建立 CALENDAR 計畫時就寫入 `next_due_at`，否則產生器永遠不動
//!   * 產生器冪等：重跑／事件重放不會產生第二張工單
//!     （由 `uq_maintenance_occurrences` 仲裁，非應用層去重）
//!   * 產生的工單 `source = 'PM_PLAN'` 且回連計畫與占位
//!   * 計量門檻事件 → 工單，整條鏈路（4.9 發事件 → 5.x 消費）

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
/// 4F 空調箱
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";
/// 1 廳投影機（有 LAMP_HOURS 讀表）
const SEED_PROJECTOR: &str = "20000000-0000-4000-8000-000000000003";
/// 009 的季保養計畫（CALENDAR，FREQ=MONTHLY;INTERVAL=3;BYMONTHDAY=5）
const PLAN_AHU: &str = "90000000-0000-4000-8000-000000000001";
/// 009 的光源更換計畫（METER，LAMP_HOURS 門檻 5000）
const PLAN_LAMP: &str = "90000000-0000-4000-8000-000000000002";
/// 009 的保養範本
const TEMPLATE_QUARTERLY: &str = "80000000-0000-4000-8000-000000000001";

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

#[tokio::test]
async fn pm_plans_and_generator() {
    let ctx = TestContext::setup().await;
    plan_endpoints(&ctx).await;
    calendar_generator_is_idempotent(&ctx).await;
    meter_event_produces_work_order(&ctx).await;
    checklist_and_comments(&ctx).await;
    ctx.teardown().await;
}

async fn plan_endpoints(ctx: &TestContext) {
    let token = ctx.login().await;

    // ---- 列表與 trigger_type 過濾 ----
    let (status, all) = ctx
        .send(authed(
            get_request("/api/v1/maintenance-plans?limit=200"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert!(
        all["data"].as_array().unwrap().len() >= 2,
        "009 種了兩個計畫：{all}"
    );
    let ahu = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == PLAN_AHU)
        .unwrap_or_else(|| panic!("應找到季保養計畫：{all}"));
    assert_eq!(ahu["trigger_type"], "CALENDAR");
    assert_eq!(ahu["target"]["type"], "ASSET");
    assert_eq!(ahu["target"]["id"], SEED_AHU);
    assert!(
        ahu["template_name"].as_str().is_some_and(|s| !s.is_empty()),
        "template_name 應由 maintenance_templates 帶出：{ahu}"
    );

    let (_, meters) = ctx
        .send(authed(
            get_request("/api/v1/maintenance-plans?trigger_type=METER&limit=200"),
            &token,
        ))
        .await;
    assert!(
        meters["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["trigger_type"] == "METER"),
        "{meters}"
    );

    // ---- 不合法的 trigger_type → 422 ----
    let (status, body) = ctx
        .send(authed(
            get_request("/api/v1/maintenance-plans?trigger_type=WHENEVER"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- preview-schedule：BYMONTHDAY=5 必須落在場域當地的 5 號 ----
    let (status, preview) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/maintenance-plans/{PLAN_AHU}/preview-schedule?limit=4"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let items = preview["data"].as_array().expect("應有 data");
    assert!(!items.is_empty(), "季保養應展開出排程：{preview}");
    for item in items {
        let at = item["scheduled_for"].as_str().unwrap();
        let utc: chrono::DateTime<chrono::Utc> = at.parse().expect("rfc3339");
        // 場域時區是 Asia/Taipei（009 的預設值）
        let local = utc.with_timezone(&chrono_tz::Asia::Taipei);
        assert_eq!(
            chrono::Datelike::day(&local),
            5,
            "BYMONTHDAY=5 應是**當地**的 5 號，實際 {local}（UTC {utc}）"
        );
        assert_eq!(
            item["asset_id"], SEED_AHU,
            "瞄準單一設備的計畫只該預覽那一台：{item}"
        );
        assert!(item["asset_code"].as_str().is_some_and(|s| !s.is_empty()));
    }
    // INTERVAL=3：相鄰兩期應相隔約一季
    if items.len() >= 2 {
        let a: chrono::DateTime<chrono::Utc> =
            items[0]["scheduled_for"].as_str().unwrap().parse().unwrap();
        let b: chrono::DateTime<chrono::Utc> =
            items[1]["scheduled_for"].as_str().unwrap().parse().unwrap();
        let days = (b - a).num_days();
        assert!(
            (85..=95).contains(&days),
            "INTERVAL=3 的相鄰兩期應相隔約 90 天，實際 {days} 天"
        );
    }

    // ---- 建立 CALENDAR 計畫：next_due_at 必須被算出來 ----
    let code = format!("PMTEST-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/maintenance-plans",
                json!({
                    "facility_id": FACILITY_HQ,
                    "template_id": TEMPLATE_QUARTERLY,
                    "code": code,
                    "name": "測試用月保養",
                    "asset_id": SEED_AHU,
                    "trigger_type": "CALENDAR",
                    "rrule": "FREQ=MONTHLY;BYMONTHDAY=15",
                    "generate_lead_days": 3
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(
        created["next_due_at"].as_str().is_some(),
        "建立時就該算出首次到期，否則產生器永遠不動：{created}"
    );
    let created_id = created["id"].as_str().unwrap().to_string();

    // 首次到期應是未來，且是當地 15 號
    let due: chrono::DateTime<chrono::Utc> =
        created["next_due_at"].as_str().unwrap().parse().unwrap();
    assert!(due > chrono::Utc::now(), "首次到期不該是過去：{due}");
    assert_eq!(
        chrono::Datelike::day(&due.with_timezone(&chrono_tz::Asia::Taipei)),
        15
    );

    // ---- CALENDAR 缺 rrule → 422（ck_plan_trigger 不該變成 500）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/maintenance-plans",
                json!({
                    "facility_id": FACILITY_HQ,
                    "template_id": TEMPLATE_QUARTERLY,
                    "code": format!("PMTEST-NORRULE-{}", &uuid::Uuid::new_v4().to_string()[..6]),
                    "name": "缺 rrule",
                    "asset_id": SEED_AHU,
                    "trigger_type": "CALENDAR"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 語法錯誤的 RRULE → 422，而不是 500 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/maintenance-plans",
                json!({
                    "facility_id": FACILITY_HQ,
                    "template_id": TEMPLATE_QUARTERLY,
                    "code": format!("PMTEST-BAD-{}", &uuid::Uuid::new_v4().to_string()[..6]),
                    "name": "壞的 rrule",
                    "asset_id": SEED_AHU,
                    "trigger_type": "CALENDAR",
                    "rrule": "FREQ=WHENEVER_I_FEEL_LIKE_IT"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "RRULE 是管理員輸入的，語法錯誤是 422：{body}"
    );

    // ---- ck_plan_target：三種瞄準模式必須恰好一種 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/maintenance-plans",
                json!({
                    "facility_id": FACILITY_HQ,
                    "template_id": TEMPLATE_QUARTERLY,
                    "code": format!("PMTEST-TWO-{}", &uuid::Uuid::new_v4().to_string()[..6]),
                    "name": "兩個目標",
                    "asset_id": SEED_AHU,
                    "category_code": "AHU",
                    "trigger_type": "CALENDAR",
                    "rrule": "FREQ=MONTHLY"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 新建計畫的 preview 也要能展開 ----
    let (status, p2) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/maintenance-plans/{created_id}/preview-schedule?limit=2"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{p2}");
    assert_eq!(
        p2["data"].as_array().unwrap().len(),
        2,
        "瞄準一台設備、預覽兩期 = 兩項：{p2}"
    );

    // ---- 時區：展開必須在場域當地時區，而不是 UTC ----
    //
    // 前面那組斷言（BYMONTHDAY=5、當地 09:00）其實**分辨不出** UTC 與台北：
    // 09:00+08 就是同日的 01:00 UTC，日期一樣。要能分辨，必須挑一個
    // 「當地日期與 UTC 日期不同」的時刻 —— 22:00Z 是台北隔天的 06:00。
    //
    // 這個測試是在 mutation test 發現前一組斷言擋不住「改成 UTC 展開」之後補的。
    let mut arm = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.maintenance_plans
            SET rrule = 'FREQ=MONTHLY;BYMONTHDAY=16',
                next_due_at = '2026-09-15T22:00:00Z'
          WHERE id = $1::uuid",
    )
    .bind(&created_id)
    .execute(&mut *arm)
    .await
    .expect("arm timezone case");
    arm.commit().await.expect("commit");

    let (status, tzp) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/maintenance-plans/{created_id}/preview-schedule?limit=2"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{tzp}");
    let first: chrono::DateTime<chrono::Utc> = tzp["data"][0]["scheduled_for"]
        .as_str()
        .expect("應有排程")
        .parse()
        .unwrap();
    let local = first.with_timezone(&chrono_tz::Asia::Taipei);
    assert_eq!(
        chrono::Datelike::day(&local),
        16,
        "BYMONTHDAY=16 是**當地**的 16 號。UTC 展開會得到當地 17 號 \
         （22:00Z 是台北隔天 06:00），實際 {local}（UTC {first}）"
    );

    // ---- 非日曆型計畫的 preview 回空陣列而非 422 ----
    let (status, p3) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/maintenance-plans/{PLAN_LAMP}/preview-schedule"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{p3}");
    assert_eq!(
        p3["data"],
        json!([]),
        "計量型計畫沒有日曆排程，這是正常狀態不是錯誤：{p3}"
    );
}

async fn calendar_generator_is_idempotent(ctx: &TestContext) {
    // 把季保養計畫的 next_due_at 拉到現在，讓它落入 plans_due 的視窗。
    // 直接改資料庫而非透過 API：契約沒有「立刻執行」的端點，
    // 而產生器的觸發條件就是 next_due_at。
    let mut setup = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.maintenance_plans
            SET next_due_at = date_trunc('second', clock_timestamp()),
                last_generated_at = NULL
          WHERE id = $1::uuid",
    )
    .bind(PLAN_AHU)
    .execute(&mut *setup)
    .await
    .expect("arm plan");
    setup.commit().await.expect("commit");

    let generator =
        fms_maintenance::pm_worker::PmGenerator::new(ctx.owner_pool().await, admin_user_id());

    // ---- 第一輪：應產生一張工單 ----
    let handled = generator.run_calendar_scan(10).await.expect("first scan");
    assert!(handled >= 1, "到期的計畫應被處理，實際 {handled}");

    let mut probe = ctx.tenant_tx().await;
    let wos: Vec<(uuid::Uuid, String, String)> = sqlx::query_as(
        "SELECT id, source::text, status::text FROM fms.work_orders
          WHERE maintenance_plan_id = $1::uuid ORDER BY created_at",
    )
    .bind(PLAN_AHU)
    .fetch_all(&mut *probe)
    .await
    .expect("read work orders");
    assert_eq!(wos.len(), 1, "應產生恰好一張工單，實際 {}", wos.len());
    assert_eq!(
        wos[0].1, "PM_PLAN",
        "provenance 必須是 PM_PLAN，否則計畫性與反應性維護的報表分不開"
    );
    assert_eq!(
        wos[0].2, "SUBMITTED",
        "產生的工單從狀態機起點進入，不直接跳到 ASSIGNED"
    );

    // 占位要回連工單
    let linked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.maintenance_occurrences
          WHERE plan_id = $1::uuid AND status = 'GENERATED' AND work_order_id IS NOT NULL",
    )
    .bind(PLAN_AHU)
    .fetch_one(&mut *probe)
    .await
    .expect("count occurrences");
    assert_eq!(linked, 1, "占位應標記 GENERATED 並回連工單");

    // next_due_at 應被推進到下一季
    let advanced: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT next_due_at FROM fms.maintenance_plans WHERE id = $1::uuid")
            .bind(PLAN_AHU)
            .fetch_one(&mut *probe)
            .await
            .expect("read next_due_at");
    let advanced = advanced.expect("next_due_at 不該被清空");
    assert!(
        advanced > chrono::Utc::now(),
        "next_due_at 應被推到未來，否則每一輪掃描都會白做：{advanced}"
    );
    drop(probe);

    // ---- 第二輪：同一個時刻不該再產生工單 ----
    // 重新把 next_due_at 拉回剛才那個時刻，模擬「產生器重跑」。
    let mut rearm = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.maintenance_plans
            SET next_due_at = (SELECT scheduled_for FROM fms.maintenance_occurrences
                                WHERE plan_id = $1::uuid ORDER BY scheduled_for LIMIT 1)
          WHERE id = $1::uuid",
    )
    .bind(PLAN_AHU)
    .execute(&mut *rearm)
    .await
    .expect("re-arm");
    rearm.commit().await.expect("commit");

    generator.run_calendar_scan(10).await.expect("second scan");

    let mut probe = ctx.tenant_tx().await;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.work_orders WHERE maintenance_plan_id = $1::uuid",
    )
    .bind(PLAN_AHU)
    .fetch_one(&mut *probe)
    .await
    .expect("recount");
    assert_eq!(
        count, 1,
        "重跑不該產生第二張工單 —— 冪等由 uq_maintenance_occurrences 仲裁"
    );
}

async fn meter_event_produces_work_order(ctx: &TestContext) {
    let token = ctx.login().await;
    let now = chrono::Utc::now();

    // 4.9 的端點跨過 5000 門檻，寫下 outbox 事件
    let (status, reading) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/assets/{SEED_PROJECTOR}/meters/LAMP_HOURS/readings"),
                json!({ "value": 5100, "reading_at": now.to_rfc3339() }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{reading}");
    let triggered = reading["triggered_maintenance_plan_ids"]
        .as_array()
        .unwrap();
    assert!(
        triggered.iter().any(|v| v == PLAN_LAMP),
        "應觸發光源更換計畫：{reading}"
    );

    // 產生器消費那筆事件
    let generator =
        fms_maintenance::pm_worker::PmGenerator::new(ctx.owner_pool().await, admin_user_id());
    // reading_at 必須在 payload 裡：它是消費端的冪等鍵（見 pm_worker 的說明）。
    let payload = json!({
        "asset_id": SEED_PROJECTOR,
        "meter_code": "LAMP_HOURS",
        "value": 5100,
        "reading_at": now.to_rfc3339(),
        "maintenance_plan_ids": [PLAN_LAMP],
    });
    let tenant = uuid::Uuid::parse_str(TENANT_ID).unwrap();
    let first = generator
        .on_meter_threshold(tenant, &payload)
        .await
        .expect("handle event");
    assert_eq!(
        first.work_order_ids.len(),
        1,
        "計量觸發應產生一張工單：{first:?}"
    );

    // ---- 事件重放（outbox 是 at-least-once）不該產生第二張 ----
    // 重放刻意在「不同的秒」發生：冪等必須來自事件內容（reading_at），
    // 不能來自兩次呼叫剛好落在同一秒。這正是先前隱藏了 bug 的地方。
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let replay = generator
        .on_meter_threshold(tenant, &payload)
        .await
        .expect("replay");
    assert!(
        replay.work_order_ids.is_empty() && replay.skipped >= 1,
        "重放應被占位擋下並回報 skipped，實際 {replay:?}"
    );

    let mut probe = ctx.tenant_tx().await;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.work_orders WHERE maintenance_plan_id = $1::uuid",
    )
    .bind(PLAN_LAMP)
    .fetch_one(&mut *probe)
    .await
    .expect("count");
    assert_eq!(count, 1, "重放後仍只有一張工單");

    // 產生的工單要指向讀表所屬的設備
    let asset: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT asset_id FROM fms.work_orders WHERE maintenance_plan_id = $1::uuid",
    )
    .bind(PLAN_LAMP)
    .fetch_one(&mut *probe)
    .await
    .expect("asset");
    assert_eq!(
        asset,
        Some(uuid::Uuid::parse_str(SEED_PROJECTOR).unwrap()),
        "工單應開在那台投影機上"
    );

    // ---- 指向不存在計畫的事件：略過而不是失敗（事件過期不是暫時性錯誤）----
    let stale = json!({
        "asset_id": SEED_PROJECTOR,
        "meter_code": "LAMP_HOURS",
        "reading_at": now.to_rfc3339(),
        "maintenance_plan_ids": [uuid::Uuid::new_v4().to_string()],
    });
    let result = generator
        .on_meter_threshold(tenant, &stale)
        .await
        .expect("過期事件不該回 Err，否則會被無限重試");
    assert!(result.work_order_ids.is_empty());
}

/// 工單子資源：範本檢查表展開與回填、留言。
///
/// 放在 PM 切片裡是刻意的：檢查項目只由保養範本展開而來，
/// 沒有產生器就沒有檢查表可以回填 —— 兩件事無法分開測。
async fn checklist_and_comments(ctx: &TestContext) {
    let token = ctx.login().await;

    // 產生器已在前一個場景開好一張 PM 工單；取它來回填檢查表。
    let mut probe = ctx.tenant_tx().await;
    let wo_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM fms.work_orders WHERE maintenance_plan_id = $1::uuid LIMIT 1",
    )
    .bind(PLAN_AHU)
    .fetch_one(&mut *probe)
    .await
    .expect("前一個場景應已產生工單");
    drop(probe);

    // ---- include=tasks：範本的五個檢查項目應已展開 ----
    let (status, detail) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{wo_id}?include=tasks")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let tasks = detail["tasks"].as_array().expect("應有 tasks");
    assert_eq!(
        tasks.len(),
        5,
        "AHU_QUARTERLY 範本有五個檢查項目，PM 工單應照樣展開：{detail}"
    );
    assert_eq!(tasks[0]["seq"], 1);
    assert_eq!(tasks[0]["title"], "更換初級濾網");
    assert_eq!(tasks[0]["input_type"], "CHECKBOX");
    // 範本的 min/max 必須跟著帶下來，否則回填時無從驗證
    let temp = tasks
        .iter()
        .find(|t| t["title"] == "進風溫度")
        .unwrap_or_else(|| panic!("{detail}"));
    assert_eq!(temp["input_type"], "NUMBER");
    assert_eq!(temp["unit"], "°C");
    assert_eq!(temp["min_value"].as_f64(), Some(10.0));
    assert_eq!(temp["max_value"].as_f64(), Some(40.0));
    assert!(temp["result_value"].is_null(), "尚未回填");

    let checkbox_id = tasks[0]["id"].as_str().unwrap().to_string();
    let number_id = temp["id"].as_str().unwrap().to_string();

    // ---- 回填 CHECKBOX ----
    let (status, filled) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{wo_id}/tasks/{checkbox_id}"),
                json!({ "result_value": true, "is_pass": true, "notes": "已更換" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{filled}");
    assert_eq!(filled["result_value"], json!(true));
    assert_eq!(filled["is_pass"], json!(true));
    assert!(
        filled["completed_at"].as_str().is_some(),
        "填入結果時應設 completed_at：{filled}"
    );

    // ---- 型別不符 → 422（jsonb 不會擋，這一層必須存在）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{wo_id}/tasks/{checkbox_id}"),
                json!({ "result_value": "yes" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "CHECKBOX 收到字串應回 422：{body}"
    );

    // ---- 超出範本 min/max → 422（不是靜默收下）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{wo_id}/tasks/{number_id}"),
                json!({ "result_value": 55 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "55 超過範本上限 40，應回 422 而非污染趨勢資料：{body}"
    );
    let (status, body) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{wo_id}/tasks/{number_id}"),
                json!({ "result_value": 5 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "低於下限也要擋：{body}"
    );

    // 範圍內就成功
    let (status, ok) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{wo_id}/tasks/{number_id}"),
                json!({ "result_value": 24.5, "is_pass": true }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{ok}");
    assert_eq!(ok["result_value"].as_f64(), Some(24.5));

    // ---- 空的 PATCH → 422 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{wo_id}/tasks/{number_id}"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 別的工單的 task id → 404（路徑含 workOrderId，必須一起比對）----
    let other_wo: uuid::Uuid = {
        let mut probe = ctx.tenant_tx().await;
        sqlx::query_scalar(
            "SELECT id FROM fms.work_orders WHERE maintenance_plan_id = $1::uuid LIMIT 1",
        )
        .bind(PLAN_LAMP)
        .fetch_one(&mut *probe)
        .await
        .expect("計量觸發的工單")
    };
    let (status, body) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{other_wo}/tasks/{number_id}"),
                json!({ "result_value": 20 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "task 不屬於這張工單，路徑不該說謊：{body}"
    );

    // ---- 留言 ----
    let (status, comment) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{wo_id}/comments"),
                json!({ "body": "濾網比預期髒，建議縮短週期", "visibility": "INTERNAL" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    assert_eq!(comment["visibility"], "INTERNAL");
    assert!(
        comment["author_name"].as_str().is_some(),
        "作者應解析為使用者名稱：{comment}"
    );

    // 不合法的 visibility → 422（CHECK 約束不該變成 500）
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{wo_id}/comments"),
                json!({ "body": "x", "visibility": "SHOUT" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // 空 body → 422
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{wo_id}/comments"),
                json!({ "body": "   " }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- include=comments ----
    let (status, detail) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/work-orders/{wo_id}?include=comments,tasks"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["comments"].as_array().unwrap().len(), 1, "{detail}");
    assert_eq!(
        detail["tasks"].as_array().unwrap().len(),
        5,
        "同時展開兩個關聯都要正確：{detail}"
    );

    // ---- include=labor,parts 已實作：剛產生的工單兩者都是空的 ----
    // 這裡的空陣列是正確斷言（「還沒領料、還沒記工時」）。
    let (status, detail) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{wo_id}?include=labor,parts")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["labor"], json!([]), "{detail}");
    assert_eq!(detail["parts"], json!([]), "{detail}");
}
