//! Work Orders 補完八支。
//!
//! # 兩格盯的是同一類事：讓「不知道」與「不會發生」看得見
//!
//! `b_`：狀態機端點必須把每個 side effect 標上 `executed`。008 宣告了七個 key，
//! 應用層只執行三個 —— 一份把惰性宣告畫成實際行為的流程圖，會讓看圖的人以為
//! 結案時系統會自動改設備狀態。
//!
//! `g_`：`part_stock.available` 的分母裡有 `quantity_reserved`，而那一欄沒有
//! 任何寫入者。回應必須說出它是死的，否則看到 `available` 的人會以為預留機制
//! 在運作。
//!
//! # 斷言全部相對於基線，因為示範資料**不是空的**
//!
//! 009 與 018 已經種了 3 個團隊、5 個備品與 5 筆庫存。
//! （我第一次量的時候拿到 0，那是因為 `psql` 沒有設 `app.is_platform='on'`
//!  —— FORCE RLS 讓沒有情境的查詢回零列。那個 0 不是「表是空的」，
//!  是「你看不到」。所以這裡不寫絕對數字。）
//!
//! # `c_` 盯的是「批次是不是繞過個別權限的後門」
//!
//! 單筆與批次共用 `transition_one`，所以那件事在結構上不可能分歧 ——
//! 而 `c_` 從 HTTP 這一層確認：一個沒有那條轉換所需權限的人，批次也做不到。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
/// `tech.liu` —— 總部的 TECHNICIAN。
const USER_TECH_LIU: &str = "ffffffff-0000-4000-8000-000000000006";

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
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

/// 建一個團隊並加一個成員。（示範資料已經有三個團隊 —— 見檔頭的基線說明。）
async fn seed_team(ctx: &TestContext, code: &str) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let team: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.teams (tenant_id, facility_id, code, name, team_type)
         VALUES ($1::uuid, $2::uuid, $3, $3 || ' 團隊', 'MAINTENANCE')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(code)
    .fetch_one(&mut *tx)
    .await
    .unwrap_or_else(|e| panic!("建團隊失敗：{e}"));
    sqlx::query(
        "INSERT INTO fms.team_members (team_id, user_id, tenant_id, role_in_team)
         VALUES ($1::uuid, $2::uuid, $3::uuid, 'MEMBER')",
    )
    .bind(team)
    .bind(USER_TECH_LIU)
    .bind(TENANT_ID)
    .execute(&mut *tx)
    .await
    .expect("加成員");
    tx.commit().await.expect("commit");
    team
}

/// 狀態字典：16 個狀態，而 `is_terminal` 是終態的唯一定義。
#[tokio::test]
async fn a_the_status_dictionary_is_the_single_source_of_terminality() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(get("/api/v1/work-order-statuses"), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert!(rows.len() >= 10, "008 該種了十幾個狀態：{}", rows.len());

    // 每一列都要有中英文與分類 —— 少了任何一個，客戶端就得自己硬編一份翻譯。
    for r in rows {
        assert!(r["name_zh"].as_str().is_some_and(|s| !s.is_empty()), "{r}");
        assert!(r["name_en"].as_str().is_some_and(|s| !s.is_empty()), "{r}");
        assert!(r["is_terminal"].is_boolean(), "{r}");
        assert!(
            ["OPEN", "IN_PROGRESS", "WAITING", "TERMINAL"]
                .contains(&r["category"].as_str().unwrap_or("")),
            "category 必須是四者之一：{r}"
        );
    }
    // 至少有一個終態與一個非終態 —— 否則那一欄是死的。
    assert!(rows.iter().any(|r| r["is_terminal"] == true), "該有終態");
    assert!(rows.iter().any(|r| r["is_terminal"] == false), "該有非終態");
    assert_eq!(
        body["meta"]["terminal_source"], "work_order_statuses.is_terminal",
        "要說出終態的來源 —— 客戶端不該自己硬編一份清單：{}",
        body["meta"]
    );

    ctx.teardown().await;
}

/// **狀態機端點要分得出「宣告了」與「真的會做」。**
#[tokio::test]
async fn b_the_state_machine_marks_which_side_effects_actually_run() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(get("/api/v1/work-order-state-machine"), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert!(rows.len() >= 20, "008 種了 40 條規則：{}", rows.len());

    // 權威清單 = `apply_side_effects` 真的執行的三個。
    let executed: Vec<&str> = body["meta"]["executed_side_effects"]
        .as_array()
        .expect("executed_side_effects")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        executed,
        vec![
            "increment_reopen",
            "release_assignee",
            "request_satisfaction"
        ],
        "這份清單是 `EXECUTED_SIDE_EFFECTS`，也就是 apply_side_effects 真的會做的"
    );

    // **惰性宣告要被列出來。** 008 宣告了 notify／compute_sla／
    // update_asset_status／release_reservation_step，而系統不做那四件事。
    let inert: Vec<&str> = body["meta"]["declared_but_inert"]
        .as_array()
        .expect("declared_but_inert")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for k in ["notify", "compute_sla", "update_asset_status"] {
        assert!(
            inert.contains(&k),
            "**`{k}` 是宣告了但不會發生的**，一定要出現在 declared_but_inert —— \
             少了它，前端會把它畫成實際行為：{inert:?}"
        );
    }
    assert!(
        !inert.iter().any(|k| executed.contains(k)),
        "同一個 key 不能同時在兩份清單裡：executed={executed:?} inert={inert:?}"
    );

    // 逐條的 side effect 也要標。
    let with_effects = rows
        .iter()
        .find(|r| {
            r["side_effects"]
                .as_array()
                .is_some_and(|a| a.iter().any(|e| e["key"] == "notify"))
        })
        .expect("該有一條宣告 notify 的規則");
    let notify = with_effects["side_effects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "notify")
        .unwrap();
    assert_eq!(
        notify["executed"], false,
        "`notify` 該標成 executed=false：{with_effects}"
    );

    // 系統驅動的動作（required_permission 為 null）要看得出來。
    assert!(
        rows.iter().any(|r| r["required_permission"].is_null()),
        "該有系統驅動的轉換（AUTO_ASSIGN／BREACH_SLA）：{}",
        body["data"]
    );

    // 依型別過濾：SERVICE 專屬的規則 + 通用規則。
    let (_, service_only) = ctx
        .send(authed(
            get("/api/v1/work-order-state-machine?work_order_type=SERVICE"),
            &token,
        ))
        .await;
    let n_service = service_only["data"].as_array().map(Vec::len).unwrap_or(0);
    assert!(
        n_service > 0 && n_service <= rows.len(),
        "型別過濾該是子集且非空：{n_service} vs {}",
        rows.len()
    );

    ctx.teardown().await;
}

/// **批次：部分成功、逐筆結果、而且不繞過個別權限。**
#[tokio::test]
async fn c_bulk_transition_reports_each_item_and_still_checks_permission() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login().await;

    // 兩張可 ASSIGN 的工單（IN_PROGRESS 不能 ASSIGN，所以先造 SUBMITTED 的）。
    let mut ids = Vec::new();
    let mut tx = ctx.owner_tx().await;
    for i in 0..2 {
        let id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fms.work_orders
               (tenant_id, facility_id, wo_no, work_order_type, source, title,
                status, priority, spatial_node_id)
             VALUES ($1::uuid, $2::uuid,
                     'WO-BK-' || $3 || substr(md5(random()::text), 1, 6),
                     'CORRECTIVE', 'MANUAL', '批次測試', 'SUBMITTED', 'MEDIUM',
                     (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1))
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_HQ)
        .bind(i.to_string())
        .fetch_one(&mut *tx)
        .await
        .expect("建工單");
        ids.push(id);
    }
    // 第三張已經結案 —— ASSIGN 對它不合法，所以它會是那個「失敗的一筆」。
    let closed: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.work_orders
           (tenant_id, facility_id, wo_no, work_order_type, source, title,
            status, priority, spatial_node_id, completed_at)
         VALUES ($1::uuid, $2::uuid,
                 'WO-BK-CLOSED-' || substr(md5(random()::text), 1, 6),
                 'CORRECTIVE', 'MANUAL', '已結案', 'CLOSED', 'MEDIUM',
                 (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                 clock_timestamp())
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("建已結案工單");
    tx.commit().await.expect("commit");

    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders:bulk-transition",
                json!({
                    "work_order_ids": [ids[0], ids[1], closed],
                    "action": "ASSIGN",
                    "fields": { "assignee_id": USER_TECH_LIU }
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let outcomes = body["data"].as_array().expect("data");
    assert_eq!(outcomes.len(), 3, "逐筆結果要三筆：{body}");
    assert_eq!(body["meta"]["succeeded"], 2, "{}", body["meta"]);
    assert_eq!(body["meta"]["failed"], 1, "{}", body["meta"]);
    assert_eq!(
        body["meta"]["partial_success"], true,
        "**部分成功要說出來** —— 只回「成功 2 筆」會讓那一筆失敗的消失：{}",
        body["meta"]
    );

    // 失敗那一筆要有自己的原因。
    let failed = outcomes
        .iter()
        .find(|o| o["ok"] == false)
        .expect("該有一筆失敗");
    assert_eq!(failed["work_order_id"], closed.to_string());
    assert!(
        failed["error"].as_str().is_some_and(|e| !e.is_empty()),
        "每一筆失敗要有自己的原因，不是一個整批的錯誤：{failed}"
    );
    // 成功那兩筆的新狀態要在。
    for o in outcomes.iter().filter(|o| o["ok"] == true) {
        assert_eq!(o["to_status"], "ASSIGNED", "{o}");
    }

    // 成功的真的寫進去了，失敗的沒被動到（savepoint 各自回捲）。
    let mut tx = ctx.owner_tx().await;
    let assigned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.work_orders
          WHERE id = ANY($1::uuid[]) AND status = 'ASSIGNED' AND assignee_id = $2::uuid",
    )
    .bind(&ids)
    .bind(USER_TECH_LIU)
    .fetch_one(&mut *tx)
    .await
    .expect("查");
    let closed_status: String =
        sqlx::query_scalar("SELECT status FROM fms.work_orders WHERE id = $1::uuid")
            .bind(closed)
            .fetch_one(&mut *tx)
            .await
            .expect("查");
    tx.commit().await.expect("commit");
    assert_eq!(assigned, 2, "成功的兩筆要真的寫進去");
    assert_eq!(
        closed_status, "CLOSED",
        "**失敗的那一筆不該被動到** —— 每一筆一個 savepoint"
    );

    // **不繞過個別權限。** REQUESTER 沒有 work_order:assign → 整批 403。
    let requester = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, _) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders:bulk-transition",
                json!({ "work_order_ids": [ids[0]], "action": "ASSIGN",
                        "fields": { "assignee_id": USER_TECH_LIU } }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "**批次不是繞過權限的後門**");

    ctx.teardown().await;
}

/// 批次的輸入驗證：空批次、重複 id、超量。
#[tokio::test]
async fn d_bulk_input_validation() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let id = ctx.seed_work_order(FACILITY_HQ, "驗證").await;

    for (body, why) in [
        (
            json!({ "work_order_ids": [], "action": "ASSIGN" }),
            "空批次不會有任何效果",
        ),
        (
            json!({ "work_order_ids": [id, id], "action": "ASSIGN" }),
            "重複的 id 第二次一定失敗，而那個失敗看起來像真的錯誤",
        ),
        (
            json!({ "work_order_ids": [id], "action": "" }),
            "action 不得為空",
        ),
        (
            json!({ "work_order_ids": [id], "action": "ASSIGN", "extra": 1 }),
            "未知欄位",
        ),
    ] {
        let (status, p) = ctx
            .send(authed(
                json_request("POST", "/api/v1/work-orders:bulk-transition", body),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{why}：{p}");
    }

    // 超量。
    let many: Vec<String> = (0..201).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let (status, p) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders:bulk-transition",
                json!({ "work_order_ids": many, "action": "ASSIGN" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(p["errors"][0]["code"], "TOO_MANY", "{p}");

    ctx.teardown().await;
}

/// 團隊清單與負載：空團隊看得見，未指派的不混進成員的數字。
#[tokio::test]
async fn e_team_list_and_workload_separate_the_queue_from_the_members() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // **基線不是零**（009 種了團隊），所以所有斷言都相對於它。
    let (_, before) = ctx.send(authed(get("/api/v1/teams"), &token)).await;
    let baseline_count = before["meta"]["count"].as_i64().expect("count");
    let baseline_without = before["meta"]["teams_without_members"]
        .as_i64()
        .expect("teams_without_members");

    let team = seed_team(ctx, "TM_A").await;

    // 再建一個沒有成員的團隊。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.teams (tenant_id, facility_id, code, name, team_type)
             VALUES ($1::uuid, $2::uuid, 'TM_EMPTY', '空團隊', 'CLEANING')",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_HQ)
        .execute(&mut *tx)
        .await
        .expect("建空團隊");
        tx.commit().await.expect("commit");
    }

    let (status, body) = ctx.send(authed(get("/api/v1/teams"), &token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["meta"]["count"].as_i64().unwrap_or(0),
        baseline_count + 2,
        "剛建的兩個團隊要出現（基線 {baseline_count}）：{}",
        body["meta"]
    );
    assert_eq!(
        body["meta"]["teams_without_members"].as_i64().unwrap_or(0),
        baseline_without + 1,
        "**空團隊派不了工，那件事要看得見** —— 它在畫面上與有人的團隊長得一樣：{}",
        body["meta"]
    );
    assert_eq!(
        body["meta"]["dispatch_rule_is_not_yet_applied"], true,
        "dispatch_rule.strategy 沒有讀者，不該被當成行為"
    );

    let with_member = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|t| t["code"] == "TM_A")
        .expect("該有 TM_A");
    assert_eq!(
        with_member["members"].as_array().map(Vec::len),
        Some(1),
        "{with_member}"
    );

    // ---- workload ----
    // 成員手上一張未結、團隊佇列裡一張還沒指到人。
    {
        let mut tx = ctx.owner_tx().await;
        for (assignee, team_col) in [(Some(USER_TECH_LIU), None), (None, Some(team))] {
            sqlx::query(
                "INSERT INTO fms.work_orders
                   (tenant_id, facility_id, wo_no, work_order_type, source, title,
                    status, priority, spatial_node_id, assignee_id, team_id)
                 VALUES ($1::uuid, $2::uuid,
                         'WO-WL-' || substr(md5(random()::text), 1, 8),
                         'CORRECTIVE', 'MANUAL', '負載', 'IN_PROGRESS', 'URGENT',
                         (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                         $3::uuid, $4::uuid)",
            )
            .bind(TENANT_ID)
            .bind(FACILITY_HQ)
            .bind(assignee)
            .bind(team_col)
            .execute(&mut *tx)
            .await
            .expect("建負載工單");
        }
        tx.commit().await.expect("commit");
    }

    let (status, wl) = ctx
        .send(authed(
            get(&format!("/api/v1/teams/{team}/workload")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{wl}");
    let member = wl["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|m| m["user_id"] == USER_TECH_LIU)
        .expect("該有 tech.liu");
    assert_eq!(member["open_work_orders"], 1, "{member}");
    assert_eq!(member["urgent_or_critical"], 1, "{member}");
    assert_eq!(
        wl["meta"]["unassigned_in_team_queue"], 1,
        "**指派給團隊但還沒指到人的要分開回報** —— 混進成員的數字會讓\
         「還沒有人接」看不見：{}",
        wl["meta"]
    );
    assert_eq!(
        wl["meta"]["denominator"],
        "未結工單（work_order_statuses.is_terminal IS NOT TRUE）"
    );

    // 不存在的團隊 → 404。
    let (status, _) = ctx
        .send(authed(
            get("/api/v1/teams/00000000-0000-4000-8000-000000000000/workload"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// **班表：同型別重疊擋、跨型別回報；非成員不能排班。**
#[tokio::test]
async fn f_same_type_shift_overlap_is_rejected_but_leave_over_regular_is_not() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let team = seed_team(ctx, "TM_SHIFT").await;
    let uri = format!("/api/v1/teams/{team}/shifts");

    let start = chrono::Utc::now() + chrono::Duration::days(1);
    let end = start + chrono::Duration::hours(8);

    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                &uri,
                json!({ "user_id": USER_TECH_LIU,
                        "shift_start": start, "shift_end": end }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["data"]["shift_type"], "REGULAR");
    assert_eq!(created["meta"]["overlaps_other_types"], 0);

    // **同型別重疊 → 409。**
    let (status, p) = ctx
        .send(authed(
            json_request(
                "POST",
                &uri,
                json!({ "user_id": USER_TECH_LIU,
                        "shift_start": start + chrono::Duration::hours(2),
                        "shift_end": end }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "同一個人同一段時間兩筆 REGULAR 是重複資料：{p}"
    );

    // **LEAVE 蓋在 REGULAR 上 → 允許**（那正是請假的記法），而且要回報重疊數。
    let (status, leave) = ctx
        .send(authed(
            json_request(
                "POST",
                &uri,
                json!({ "user_id": USER_TECH_LIU,
                        "shift_start": start, "shift_end": end,
                        "shift_type": "LEAVE" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "**LEAVE 蓋在 REGULAR 上是請假的記法，不是錯誤**：{leave}"
    );
    assert_eq!(
        leave["meta"]["overlaps_other_types"], 1,
        "跨型別的重疊不擋但要回報 —— 讓排班的人看得到：{}",
        leave["meta"]
    );

    // 班表查詢：兩筆都在，而請假那筆要看得出來。
    let (status, list) = ctx.send(authed(get(&uri), &token)).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["meta"]["count"], 2, "{}", list["meta"]);
    assert_eq!(
        list["meta"]["leave_entries"], 1,
        "**請假也是一筆班次** —— 派工的人要看得出「有排班」與「排的是休假」的差別：{}",
        list["meta"]
    );

    // 非成員不能排班。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "POST",
                &uri,
                json!({ "user_id": ADMIN_USER_ID,
                        "shift_start": start, "shift_end": end }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(p["errors"][0]["code"], "NOT_A_MEMBER", "{p}");

    // 反向區間、超過 24 小時、未知型別。
    for (body, why) in [
        (
            json!({ "user_id": USER_TECH_LIU, "shift_start": end, "shift_end": start }),
            "結束早於開始",
        ),
        (
            json!({ "user_id": USER_TECH_LIU, "shift_start": start,
                    "shift_end": start + chrono::Duration::hours(30) }),
            "超過 24 小時",
        ),
        (
            json!({ "user_id": USER_TECH_LIU, "shift_start": start, "shift_end": end,
                    "shift_type": "NIGHT" }),
            "未知型別",
        ),
    ] {
        let (status, p) = ctx
            .send(authed(json_request("POST", &uri, body), &token))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{why}：{p}");
    }

    ctx.teardown().await;
}

/// **備品庫存的 `available` 要說出 `quantity_reserved` 沒有寫入者。**
#[tokio::test]
async fn g_part_stock_says_that_reserved_is_never_written() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 基線不是零：018 種了備品與庫存。
    let (_, pb) = ctx.send(authed(get("/api/v1/parts"), &token)).await;
    let base_parts = pb["meta"]["count"].as_i64().expect("count");
    let base_no_cost = pb["meta"]["parts_without_unit_cost"].as_i64().expect("n");
    let (_, sb) = ctx.send(authed(get("/api/v1/part-stock"), &token)).await;
    let base_stock = sb["meta"]["count"].as_i64().expect("count");
    let base_no_rp = sb["meta"]["rows_without_reorder_point"]
        .as_i64()
        .expect("n");
    let base_below = sb["meta"]["below_reorder_point"].as_i64().expect("n");

    // 兩個備品：一個有單價、一個沒有；一筆有再訂購點、一筆沒有。
    let mut tx = ctx.owner_tx().await;
    for (code, cost) in [("PT_WITH", Some(120.0_f64)), ("PT_NO_COST", None)] {
        let part: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fms.parts (tenant_id, part_code, name, unit, unit_cost, currency)
             VALUES ($1::uuid, $2, $2 || ' 備品', 'PCS', $3::float8::numeric,
                     CASE WHEN $3 IS NULL THEN NULL ELSE 'TWD' END)
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(code)
        .bind(cost)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("建備品 {code} 失敗：{e}"));
        sqlx::query(
            "INSERT INTO fms.part_stock
               (tenant_id, part_id, facility_id, quantity_on_hand, reorder_point)
             VALUES ($1::uuid, $2::uuid, $3::uuid, 5,
                     CASE WHEN $4 THEN 10 ELSE NULL END)",
        )
        .bind(TENANT_ID)
        .bind(part)
        .bind(FACILITY_HQ)
        .bind(code == "PT_WITH")
        .execute(&mut *tx)
        .await
        .expect("建庫存");
    }
    tx.commit().await.expect("commit");

    // ---- /parts ----
    let (status, parts) = ctx.send(authed(get("/api/v1/parts"), &token)).await;
    assert_eq!(status, StatusCode::OK, "{parts}");
    assert_eq!(
        parts["meta"]["count"].as_i64().unwrap_or(0),
        base_parts + 2,
        "{}",
        parts["meta"]
    );
    assert_eq!(
        parts["meta"]["parts_without_unit_cost"]
            .as_i64()
            .unwrap_or(0),
        base_no_cost + 1,
        "**沒有單價的備品領用後算不出成本** —— report_service_volume 的 \
         parts_cost 會少算，而帳單會安靜地偏低：{}",
        parts["meta"]
    );
    let with_cost = parts["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|p| p["part_code"] == "PT_WITH")
        .expect("該有 PT_WITH");
    assert_eq!(with_cost["unit_cost"], 120.0, "{with_cost}");
    assert_eq!(with_cost["total_on_hand"], 5.0, "{with_cost}");

    // 搜尋。
    let (_, searched) = ctx
        .send(authed(get("/api/v1/parts?q=pt_no_cost"), &token))
        .await;
    assert_eq!(
        searched["meta"]["count"], 1,
        "搜尋要精確到我們剛建的那一個：{}",
        searched["data"]
    );

    // ---- /part-stock ----
    let (status, stock) = ctx.send(authed(get("/api/v1/part-stock"), &token)).await;
    assert_eq!(status, StatusCode::OK, "{stock}");
    assert_eq!(
        stock["meta"]["count"].as_i64().unwrap_or(0),
        base_stock + 2,
        "{}",
        stock["meta"]
    );
    assert_eq!(
        stock["meta"]["reserved_is_never_written"], true,
        "**`quantity_reserved` 沒有任何寫入者**，所以 available 恆等於 on_hand。\
         少了這句，看到 available 的人會以為預留機制在運作：{}",
        stock["meta"]
    );
    assert_eq!(
        stock["meta"]["rows_without_reorder_point"]
            .as_i64()
            .unwrap_or(0),
        base_no_rp + 1,
        "**沒設再訂購點的列永遠不會出現在補貨清單裡** —— 那不是庫存充足，\
         是沒有人設過門檻：{}",
        stock["meta"]
    );
    assert_eq!(
        stock["meta"]["below_reorder_point"].as_i64().unwrap_or(0),
        base_below + 1,
        "{}",
        stock["meta"]
    );

    let row = stock["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|r| r["part_code"] == "PT_WITH")
        .expect("該有 PT_WITH");
    assert_eq!(row["quantity_on_hand"], 5.0, "{row}");
    assert_eq!(row["quantity_reserved"], 0.0, "{row}");
    assert_eq!(
        row["available"], 5.0,
        "available = on_hand - reserved，而 reserved 恆為 0：{row}"
    );
    assert_eq!(row["needs_reorder"], true, "5 <= 10：{row}");

    // 只看低於再訂購點的。
    let (_, below) = ctx
        .send(authed(
            get("/api/v1/part-stock?below_reorder_point=true"),
            &token,
        ))
        .await;
    assert_eq!(
        below["meta"]["count"].as_i64().unwrap_or(0),
        base_below + 1,
        "{}",
        below["data"]
    );

    ctx.teardown().await;
}

/// 權限：`team:read`／`team:write`／`part:read` 各自有擋。
#[tokio::test]
async fn h_permissions_are_enforced() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login().await;
    let team = seed_team(ctx, "TM_PERM").await;
    // user.huang 是 REQUESTER：沒有 team:read／part:read。
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    for uri in [
        "/api/v1/teams".to_string(),
        format!("/api/v1/teams/{team}/workload"),
        format!("/api/v1/teams/{team}/shifts"),
        "/api/v1/parts".to_string(),
        "/api/v1/part-stock".to_string(),
    ] {
        let (status, _) = ctx.send(authed(get(&uri), &requester)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} 該擋下 REQUESTER");
    }

    // 狀態字典只需要登入 —— REQUESTER 讀得到。
    let (status, _) = ctx
        .send(authed(get("/api/v1/work-order-statuses"), &requester))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "狀態字典是字典，不該擋權限（與 GET /permissions 同一個判斷）"
    );

    // 而狀態機需要 work_order:read —— REQUESTER 只有 read_own。
    let (status, _) = ctx
        .send(authed(get("/api/v1/work-order-state-machine"), &requester))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "狀態機定義需要 work_order:read"
    );

    let _ = admin;
    ctx.teardown().await;
}
