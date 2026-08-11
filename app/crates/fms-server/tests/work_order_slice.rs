//! 工單狀態機端到端測試（WBS S4）。
//!
//! 重點不在 CRUD，而在**狀態機的三關**與**規則來自資料而非程式**：
//!   * 完整生命週期 SUBMITTED → ASSIGNED → IN_PROGRESS → COMPLETED → CLOSED
//!   * 當前狀態下不存在的動作回 409 `WORK_ORDER_ILLEGAL_TRANSITION`
//!   * `required_fields` 缺欄位回 422 並以 JSON Pointer 指出
//!   * `required_fields` 也接受「工單上已有值」（SUBMIT 的 `title`）
//!   * `required_permission` 由應用層執行（資料庫函式完全忽略那一欄）
//!   * `available-actions` 的 `permitted` 反映真實權限，且動作仍然列出
//!   * `PATCH` 無法改狀態（契約明訂），也無法繞過狀態機
//!   * `side_effects.increment_reopen` 真的加一（那是契約看不到的欄位，
//!     因此直接查資料庫驗證）
//!   * SERVICE 類工單的 `payload` 以 `service_items.form_schema` 驗證
//!   * `work_order:read_own` 的列級範圍（WBS 3.9）：只有 `_own` 的角色
//!     看得到自己的、看不到別人的，且無法用查詢參數放寬
//!   * 場域範圍的角色能用列表端點（先前一律 403），且只看得到自己場域的列
//!     —— 授權放寬與 007 場域級 RLS 啟用必須成對出現（WBS 3.9）

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
/// 4F 空調箱（HQ）—— 有這台設備才滿足 `ck_wo_target`
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";
const SEED_AHU_CODE: &str = "HQ-AHU-4F-01";
/// 王技師，派工對象
const TECH_WANG: &str = "ffffffff-0000-4000-8000-000000000003";
/// 影廳緊急清潔服務項目（required: severity，enum 限定三值）
const SERVICE_SPILL: &str = "60000000-0000-4000-8000-000000000004";
const FACILITY_CINEMA: &str = "cccccccc-0000-4000-8000-000000000002";
const NODE_CINEMA_HALL: &str = "10000000-0000-4000-8000-000000000013";

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

/// 五個場景各自獨立 `#[tokio::test]`，因此**平行執行**。
///
/// 在測試隔離之前這是做不到的：所有測試共用一個資料庫，而 setup 的清理
/// 是全域的（刪除本租戶所有 `source='API'` 的工單），第二個測試會刪掉
/// 第一個測試正在用的資料。當時的解法是把五個場景塞進同一個函式依序跑。
///
/// 現在每個測試從 template 複製一份自己的資料庫，互相看不見對方，
/// 因此可以平行 —— 這正是隔離要換來的東西。
#[tokio::test]
async fn state_machine() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // ---- 建立：預設 SUBMITTED，關聯物件要帶名稱 ----
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "4F 空調箱異音（狀態機測試）",
                    "description": "下午起持續低頻異音",
                    "priority": "HIGH"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {wo}");
    assert_eq!(wo["status"], "SUBMITTED", "契約：預設狀態為 SUBMITTED");
    assert_eq!(
        wo["status_category"], "OPEN",
        "status_category 應由 work_order_statuses 帶出：{wo}"
    );
    assert_eq!(wo["source"], "API");
    assert!(
        wo["wo_no"].as_str().is_some_and(|s| s.starts_with("WO-")),
        "wo_no 應由 next_document_no 產生：{wo}"
    );
    assert_eq!(
        wo["asset"]["asset_code"], SEED_AHU_CODE,
        "asset 應嵌入物件而非只給 id：{wo}"
    );
    assert!(
        wo["requester"]["display_name"].as_str().is_some(),
        "requester 應是目前使用者：{wo}"
    );
    assert_eq!(wo["version"], 1);
    let id = wo["id"].as_str().unwrap().to_string();

    // ---- ck_wo_target：沒有 asset 也沒有 spatial_node → 422 而非 500 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "title": "沒有目標"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- ck_wo_service_item：SERVICE 但沒帶 service_item_id → 422 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "SERVICE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "缺服務項目"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 不在 CHECK 清單內的 work_order_type → 422（不是 500）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "REPAIR",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "錯的型別"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- available-actions：規則來自資料庫，label 來自 015 ----
    let (status, actions) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{id}/available-actions")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{actions}");
    let list = actions["data"].as_array().expect("應有 data");
    let assign = list
        .iter()
        .find(|a| a["action"] == "ASSIGN")
        .unwrap_or_else(|| panic!("SUBMITTED 下應可 ASSIGN：{actions}"));
    assert_eq!(assign["to_status"], "ASSIGNED");
    assert_eq!(
        assign["label_zh"], "派工",
        "label_zh 應來自 work_order_actions catalog（015）：{assign}"
    );
    assert_eq!(
        assign["required_fields"],
        json!(["assignee_id"]),
        "required_fields 應原樣來自狀態機規則：{assign}"
    );
    assert_eq!(
        assign["permitted"], true,
        "TENANT_ADMIN 有 work_order:assign"
    );

    let auto = list
        .iter()
        .find(|a| a["action"] == "AUTO_ASSIGN")
        .unwrap_or_else(|| panic!("AUTO_ASSIGN 也該被列出：{actions}"));
    assert_eq!(
        auto["permitted"], false,
        "required_permission 為 NULL 的系統動作應列出但不可執行：{auto}"
    );
    // APPROVE 只存在於 PENDING_APPROVAL，此刻不該出現
    assert!(
        !list.iter().any(|a| a["action"] == "APPROVE"),
        "SUBMITTED 下不該出現 APPROVE：{actions}"
    );

    // ---- 當前狀態不存在的動作 → 409（不是 404 也不是 422）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "APPROVE" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["code"], "WORK_ORDER_ILLEGAL_TRANSITION",
        "契約定義的錯誤碼：{body}"
    );

    // ---- required_fields 缺欄位 → 422，且以 JSON Pointer 指出 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "ASSIGN" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["errors"][0]["pointer"], "/assignee_id",
        "應以 JSON Pointer 指出缺哪個欄位：{body}"
    );

    // ---- ASSIGN：帶了必填欄位就成功，且欄位被寫入 ----
    let (status, assigned) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{assigned}");
    assert_eq!(assigned["status"], "ASSIGNED");
    assert_eq!(
        assigned["assignee"]["id"], TECH_WANG,
        "body 帶來的 assignee_id 應在轉換前寫入：{assigned}"
    );

    // ---- PATCH 不能改狀態（契約明訂），但可以改其他欄位 ----
    let (_, etag, _) = ctx
        .send_with_headers(authed(
            get_request(&format!("/api/v1/work-orders/{id}")),
            &token,
        ))
        .await;
    let etag = etag.expect("GET 單筆應回 ETag");
    let (status, patched) = ctx
        .send(authed_if_match(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{id}"),
                json!({ "title": "改過的標題", "status": "CLOSED", "priority": "URGENT" }),
            ),
            &token,
            &etag,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["title"], "改過的標題");
    assert_eq!(patched["priority"], "URGENT");
    assert_eq!(
        patched["status"], "ASSIGNED",
        "PATCH 不該能改狀態，狀態變更只能走 transitions：{patched}"
    );

    // ---- PATCH 缺 If-Match → 428；過期 → 412 ----
    let (status, _) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{id}"),
                json!({ "title": "x" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    let (status, _) = ctx
        .send(authed_if_match(
            json_request(
                "PATCH",
                &format!("/api/v1/work-orders/{id}"),
                json!({ "title": "x" }),
            ),
            &token,
            "999",
        ))
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    // ---- START_WORK：set_actual_start 副作用（由資料庫函式執行）----
    let (status, started) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "START_WORK" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["status"], "IN_PROGRESS");
    assert_eq!(started["status_category"], "IN_PROGRESS");
    assert!(
        started["actual_start_at"].as_str().is_some(),
        "side_effects.set_actual_start 應已生效：{started}"
    );

    // ---- COMPLETE 需要 resolution_notes ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "COMPLETE" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["pointer"], "/resolution_notes");

    // 領用濾網：018 在總部備了 24 片，單價 850
    let mut probe = ctx.tenant_tx().await;
    let (filter_id, stock_before): (uuid::Uuid, f64) = sqlx::query_as(
        "SELECT p.id, s.quantity_on_hand::float8
           FROM fms.parts p JOIN fms.part_stock s ON s.part_id = p.id
          WHERE lower(p.part_code) = 'filt-merv13-24x24'
            AND s.facility_id = $1::uuid",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *probe)
    .await
    .expect("018 應已種下濾網與總部庫存");
    drop(probe);

    let (status, completed) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({
                    "action": "COMPLETE",
                    "resolution_notes": "更換軸承並測試，異音消失",
                    "labor_minutes": 75,
                    "parts_used": [{ "part_id": filter_id, "quantity": 2 }]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["status"], "COMPLETED");
    assert!(
        completed["completed_at"].as_str().is_some(),
        "COMPLETED 應設 completed_at：{completed}"
    );
    assert!(
        completed["actual_end_at"].as_str().is_some(),
        "side_effects.set_actual_end 應已生效：{completed}"
    );
    assert_eq!(
        completed["labor_minutes"], 75,
        "labor_minutes 現在由 work_order_labor 的明細列 rollup 而來：{completed}"
    );
    // 料件成本：2 × 850 = 1700，且在領用時快照（日後調價不改寫已完工單）
    assert_eq!(
        completed["total_cost"].as_f64(),
        Some(1700.0),
        "total_cost 應由明細 rollup（2 片 × 850）：{completed}"
    );

    // 明細與庫存扣帳
    let (status, detail) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{id}?include=parts,labor")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let parts = detail["parts"].as_array().expect("應有 parts");
    assert_eq!(parts.len(), 1, "{detail}");
    assert_eq!(parts[0]["part_code"], "FILT-MERV13-24X24");
    assert_eq!(parts[0]["quantity_used"].as_f64(), Some(2.0));
    assert_eq!(parts[0]["total_cost"].as_f64(), Some(1700.0));

    let labor = detail["labor"].as_array().expect("應有 labor");
    assert_eq!(labor.len(), 1, "{detail}");
    assert_eq!(labor[0]["minutes"], 75);
    assert!(
        labor[0]["cost"].is_null(),
        "工時成本應為 null —— 全 schema 沒有費率來源，填一個數字會是憑空的：{detail}"
    );

    let mut probe = ctx.tenant_tx().await;
    let stock_after: f64 = sqlx::query_scalar(
        "SELECT s.quantity_on_hand::float8 FROM fms.part_stock s
           JOIN fms.parts p ON p.id = s.part_id
          WHERE lower(p.part_code) = 'filt-merv13-24x24' AND s.facility_id = $1::uuid",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *probe)
    .await
    .expect("read stock");
    assert_eq!(
        stock_after,
        stock_before - 2.0,
        "領用應原子性扣減庫存：{stock_before} → {stock_after}"
    );

    // ---- 庫存不足 → 409（請求合法，是當前庫存讓它不可行）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({
                    "action": "REOPEN",
                    "reason": "測試庫存不足",
                    "parts_used": [{ "part_id": filter_id, "quantity": 9999 }]
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "庫存不足應回 409 而非 500（ck_part_stock_nonneg 不該是錯誤路徑）：{body}"
    );

    // 失敗的交易不該留下任何痕跡：庫存與明細都要維持原樣
    let stock_still: f64 = sqlx::query_scalar(
        "SELECT s.quantity_on_hand::float8 FROM fms.part_stock s
           JOIN fms.parts p ON p.id = s.part_id
          WHERE lower(p.part_code) = 'filt-merv13-24x24' AND s.facility_id = $1::uuid",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *probe)
    .await
    .expect("read stock");
    assert_eq!(stock_still, stock_after, "409 之後交易回滾，庫存不該有變化");
    drop(probe);

    // ---- REOPEN：需要 reason，且 increment_reopen 副作用要真的加一 ----
    // reopened_count 不在契約的 WorkOrder 裡，因此直接查資料庫驗證。
    let mut probe = ctx.tenant_tx().await;
    let before: i16 =
        sqlx::query_scalar("SELECT reopened_count FROM fms.work_orders WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&id).unwrap())
            .fetch_one(&mut *probe)
            .await
            .expect("read reopened_count");

    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "REOPEN" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "REOPEN 需要 reason: {body}"
    );

    let (status, reopened) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "REOPEN", "reason": "使用者回報異音再次出現" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{reopened}");
    assert_eq!(reopened["status"], "IN_PROGRESS");

    let after: i16 = sqlx::query_scalar("SELECT reopened_count FROM fms.work_orders WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&id).unwrap())
        .fetch_one(&mut *probe)
        .await
        .expect("read reopened_count");
    assert_eq!(
        after,
        before + 1,
        "side_effects.increment_reopen 應由服務層執行（資料庫函式沒有實作它）"
    );
    // 這個 drop 不是整潔而已 —— 漏掉它會讓 teardown 的 `pool.close()` 永久卡住
    // （成因見 common/mod.rs 的 `teardown`）。前兩個 probe 都有 drop，這一個沒有，
    // 於是 CI 上這個測試把整個 app job 撞到 30 分鐘的 timeout。
    drop(probe);

    // ---- 再完成一次然後結案 ----
    let (_, _) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "COMPLETE", "resolution_notes": "重新更換並延長觀察" }),
            ),
            &token,
        ))
        .await;
    let (status, closed) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "CLOSE" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["status"], "CLOSED");
    assert_eq!(closed["status_category"], "TERMINAL");

    // ---- 稽核軌跡：每一步都要在 work_order_transitions 裡 ----
    let (status, detail) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{id}?include=transitions")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let log = detail["transitions"].as_array().expect("應有 transitions");
    let chain: Vec<&str> = log.iter().map(|t| t["action"].as_str().unwrap()).collect();
    assert_eq!(
        chain,
        vec![
            "ASSIGN",
            "START_WORK",
            "COMPLETE",
            "REOPEN",
            "COMPLETE",
            "CLOSE"
        ],
        "稽核軌跡應按序記下每一步：{detail}"
    );
    assert!(
        log[0]["actor_name"].as_str().is_some(),
        "actor 應解析為使用者名稱：{detail}"
    );
    assert_eq!(
        log[3]["reason"], "使用者回報異音再次出現",
        "reason 應寫進稽核列：{detail}"
    );

    // ---- include=tasks 已實作：非 PM 工單的檢查表確實是空的 ----
    // 空陣列在這裡是**正確的斷言**（「查過了，這張工單沒有檢查項目」），
    // 與先前「功能還沒做」的 422 語意不同。檢查項目只由保養範本展開而來。
    let (status, with_tasks) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{id}?include=tasks")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{with_tasks}");
    assert_eq!(
        with_tasks["tasks"],
        serde_json::json!([]),
        "API 建立的工單沒有範本，因此沒有檢查項目：{with_tasks}"
    );

    // ---- 未知的 include 值仍然回 422（機制沒有因為值變多而失效）----
    let (status, body) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{id}?include=labour")),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "`labour` 是 `labor` 的英式拼法，不在白名單內：{body}"
    );

    // ---- 終態之後仍可 REOPEN（狀態機允許），但 START_WORK 不行 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "START_WORK" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "CLOSED 下不能 START_WORK: {body}"
    );

    // ---- 列表過濾：status_category 與 asset_id ----
    let (status, page) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/work-orders?asset_id={SEED_AHU}&status_category=TERMINAL&limit=50"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == id),
        "已結案的工單應出現在 status_category=TERMINAL：{page}"
    );
    let (_, open_page) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/work-orders?asset_id={SEED_AHU}&status_category=OPEN&limit=50"
            )),
            &token,
        ))
        .await;
    assert!(
        !open_page["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == id),
        "已結案的工單不該出現在 status_category=OPEN：{open_page}"
    );

    // ---- 不合法的 status_category → 422 ----
    let (status, body) = ctx
        .send(authed(
            get_request("/api/v1/work-orders?status_category=NOPE"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    ctx.teardown().await;
}

#[tokio::test]
async fn service_payload_is_validated_against_form_schema() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // form_schema 要求 severity ∈ {MINOR, MODERATE, MAJOR}
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "SERVICE",
                    "facility_id": FACILITY_CINEMA,
                    "service_item_id": SERVICE_SPILL,
                    "spatial_node_id": NODE_CINEMA_HALL,
                    "title": "1 廳飲料傾倒（schema 測試）",
                    "payload": { "severity": "CATASTROPHIC" }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["pointer"] == "/payload/severity"),
        "契約要求以 JSON Pointer 指出違規欄位：{body}"
    );

    // 缺 required 欄位也要被抓到
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "SERVICE",
                    "facility_id": FACILITY_CINEMA,
                    "service_item_id": SERVICE_SPILL,
                    "spatial_node_id": NODE_CINEMA_HALL,
                    "title": "1 廳飲料傾倒（缺 severity）",
                    "payload": { "seat_range": "G8-G12" }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // 合法 payload 應通過
    let (status, ok) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "SERVICE",
                    "facility_id": FACILITY_CINEMA,
                    "service_item_id": SERVICE_SPILL,
                    "spatial_node_id": NODE_CINEMA_HALL,
                    "title": "1 廳飲料傾倒（合法）",
                    "payload": { "severity": "MODERATE", "blocking_show": true }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ok}");
    assert_eq!(ok["payload"]["severity"], "MODERATE");

    ctx.teardown().await;
}

#[tokio::test]
async fn required_permission_is_enforced_by_the_application() {
    let ctx = &TestContext::setup().await;
    // 這個場景存在的理由：`fms.transition_work_order` 查出了規則列卻**沒有**
    // 讀取 `required_permission`。那一關完全由應用層負責，
    // 因此必須有測試證明它真的有擋，否則哪天被拿掉不會有人發現。
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    // REQUESTER 有 work_order:create
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "申請人建立的報修"
                }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "REQUESTER 應能建立工單: {wo}");
    let id = wo["id"].as_str().unwrap().to_string();

    // 但沒有 work_order:assign → 403，而且是在動作合法的情況下被權限擋下
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ASSIGN 在 SUBMITTED 下是合法動作，該被權限而非狀態機擋下: {body}"
    );
    assert_eq!(body["code"], "PERMISSION_DENIED");

    // 同一個工單，TENANT_ADMIN 就做得到 —— 證明擋的是權限不是資料
    let admin = ctx.login().await;
    let (status, assigned) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{assigned}");
    assert_eq!(assigned["status"], "ASSIGNED");

    ctx.teardown().await;
}

/// `work_order:read_own` 的列級範圍（WBS 3.9）。
///
/// 這是三個角色（REQUESTER／TECHNICIAN／SERVICE_STAFF）唯一的讀權限。
/// 在本次之前列表與詳情只檢查完整的 `work_order:read`，
/// 也就是這三個角色連自己報修的工單都看不到 —— 對絕大多數實際使用者
/// 系統是不可用的。
#[tokio::test]
async fn read_own_scopes_rows_not_just_facilities() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login().await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    // 管理員建立的工單：申請人是管理員，與 REQUESTER 無關
    let (status, others) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "INSPECTION",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "別人的工單（read_own 測試）"
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{others}");
    let others_id = others["id"].as_str().unwrap().to_string();

    // REQUESTER 自己建立的工單
    let (status, mine) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "我自己的工單（read_own 測試）"
                }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{mine}");
    let mine_id = mine["id"].as_str().unwrap().to_string();

    // ---- 自己的看得到 ----
    let (status, got) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{mine_id}")),
            &requester,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "只有 read_own 的角色應看得到自己申請的工單: {got}"
    );

    // ---- 別人的看不到，且回 404 而非 403（不洩漏存在性）----
    let (status, body) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{others_id}")),
            &requester,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "別人的工單應與「不存在」不可分辨: {body}"
    );

    // ---- 列表：即使沒帶 mine=true 也只回自己的 ----
    let (status, page) = ctx
        .send(authed(
            get_request("/api/v1/work-orders?limit=200"),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let ids: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&mine_id.as_str()),
        "自己的工單應在列表裡: {page}"
    );
    assert!(
        !ids.contains(&others_id.as_str()),
        "列表不該漏出別人的工單，即使沒帶 mine=true: {page}"
    );

    // ---- 用查詢參數指定別人也放寬不了 ----
    let (_, spoofed) = ctx
        .send(authed(
            get_request("/api/v1/work-orders?mine=false&limit=200"),
            &requester,
        ))
        .await;
    assert!(
        !spoofed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == others_id),
        "mine=false 不該能繞過 read_own: {spoofed}"
    );

    // ---- available-actions 也套同一個範圍 ----
    let (status, _) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/work-orders/{others_id}/available-actions"
            )),
            &requester,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "別人的工單連可用動作都不該列出"
    );

    // 自己的可以，但 ASSIGN 應顯示為不可執行（有列出、但 permitted=false）
    let (status, actions) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{mine_id}/available-actions")),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{actions}");
    let assign = actions["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["action"] == "ASSIGN")
        .unwrap_or_else(|| panic!("動作仍應列出: {actions}"));
    assert_eq!(
        assign["permitted"], false,
        "REQUESTER 沒有 work_order:assign，應列出但不可執行: {assign}"
    );

    // ---- 管理員仍然看得到兩筆（證明擋的是權限不是資料）----
    let (_, all) = ctx
        .send(authed(get_request("/api/v1/work-orders?limit=200"), &admin))
        .await;
    let admin_ids: Vec<&str> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert!(
        admin_ids.contains(&mine_id.as_str()) && admin_ids.contains(&others_id.as_str()),
        "有完整 read 的角色應看得到兩筆: {all}"
    );

    ctx.teardown().await;
}

/// 場域範圍的角色（WBS 3.9）。
///
/// 兩件事必須同時成立，因此寫在同一個場景裡：
///   1. **能用列表端點**。先前以 `facility_id = NULL` 檢查權限，
///      而 `user_permission_codes` 的 FACILITY 分支比對 `scope_id = NULL`
///      永遠不成立 —— `FACILITY_ADMIN` 連 `GET /work-orders` 都是 403。
///   2. **只看得到自己場域的列**。放寬第 1 點而不做第 2 點就是權限擴大：
///      007 的 `facility_scope` RESTRICTIVE 政策讀 `app.facility_ids`，
///      而該 GUC 在本次之前從來沒有人設定，於是那 15 個政策全部放行。
#[tokio::test]
async fn facility_scoped_roles_can_list_and_only_see_their_facility() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login().await;
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    // 影廳（非總部）的工單，由管理員建立
    let (status, cinema) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "SERVICE",
                    "facility_id": FACILITY_CINEMA,
                    "service_item_id": SERVICE_SPILL,
                    "spatial_node_id": NODE_CINEMA_HALL,
                    "title": "影廳工單（場域範圍測試）",
                    "payload": { "severity": "MINOR" }
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{cinema}");
    let cinema_id = cinema["id"].as_str().unwrap().to_string();

    // 總部的工單
    let (status, hq) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "INSPECTION",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "總部工單（場域範圍測試）"
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{hq}");
    let hq_id = hq["id"].as_str().unwrap().to_string();

    // ---- 1. 場域範圍的角色能用不帶 facility_id 的列表端點 ----
    let (status, page) = ctx
        .send(authed(get_request("/api/v1/work-orders?limit=200"), &fm))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "FACILITY_ADMIN 應能使用列表端點（授權判定必須是「任一範圍持有」）: {page}"
    );

    // ---- 2. 但只看得到自己場域的列 ----
    let ids: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&hq_id.as_str()),
        "應看得到自己場域（總部）的工單: {page}"
    );
    assert!(
        !ids.contains(&cinema_id.as_str()),
        "不該看得到其他場域（影廳）的工單 —— 007 的 facility_scope 政策必須生效: {page}"
    );

    // 單筆讀取同樣被 RLS 擋在外面（回 404，因為那一列對它不存在）
    let (status, body) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{cinema_id}")),
            &fm,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "其他場域的工單對它應不存在: {body}"
    );

    // ---- 租戶管理員（TENANT 範圍）仍然看得到兩個場域 ----
    let (_, all) = ctx
        .send(authed(get_request("/api/v1/work-orders?limit=200"), &admin))
        .await;
    let admin_ids: Vec<&str> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert!(
        admin_ids.contains(&hq_id.as_str()) && admin_ids.contains(&cinema_id.as_str()),
        "TENANT 範圍的角色可存取全部場域，不該被 app.facility_ids 誤擋: {all}"
    );

    ctx.teardown().await;
}

/// 資料庫層自行執行 `required_permission`（022）。
///
/// S4 時權限檢查只在應用層，並記下風險：任何**不經 REST API** 的呼叫者
/// （PM 產生器、SLA 排程器）都繞得過去。022 把它下移進
/// `fms.transition_work_order()`，讓「唯一入口」名副其實。
///
/// 這個測試刻意**繞過 API**，直接呼叫 SQL 函式 —— 那正是先前沒有防護的路徑。
#[tokio::test]
async fn transition_function_enforces_permission_without_the_api() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login().await;

    // 建立一張工單（SUBMITTED），ASSIGN 需要 work_order:assign
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "資料庫層權限測試"
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    let id = uuid::Uuid::parse_str(wo["id"].as_str().unwrap()).unwrap();

    // user.huang（REQUESTER）沒有 work_order:assign。
    // 直接呼叫函式 —— 這條路徑在 022 之前完全沒有檢查。
    let requester = uuid::Uuid::parse_str("ffffffff-0000-4000-8000-000000000004").unwrap();
    let mut tx = ctx.tenant_tx().await;
    let denied = sqlx::query("SELECT fms.transition_work_order($1, 'ASSIGN', $2)")
        .bind(id)
        .bind(requester)
        .execute(&mut *tx)
        .await;

    let err = denied.expect_err(
        "沒有 work_order:assign 的身分直接呼叫函式也必須被擋 —— \
         這正是 S4 記錄的風險：非 API 呼叫者繞過權限檢查",
    );
    let db_err = err.as_database_error().expect("應是資料庫錯誤");
    assert_eq!(
        db_err.code().as_deref(),
        Some("42501"),
        "應以 insufficient_privilege 拋出，讓既有的 SQLSTATE 映射轉成 403：{db_err}"
    );
    drop(tx);

    // 有權限的身分走同一條路徑則成功 —— 證明擋的是權限不是函式壞了
    let mut tx = ctx.tenant_tx().await;
    sqlx::query("UPDATE fms.work_orders SET assignee_id = $2 WHERE id = $1")
        .bind(id)
        .bind(uuid::Uuid::parse_str("ffffffff-0000-4000-8000-000000000003").unwrap())
        .execute(&mut *tx)
        .await
        .expect("set assignee");
    sqlx::query("SELECT fms.transition_work_order($1, 'ASSIGN', $2)")
        .bind(id)
        .bind(admin_user_id())
        .execute(&mut *tx)
        .await
        .expect("TENANT_ADMIN 有 work_order:assign，同一條路徑應成功");
    drop(tx);

    ctx.teardown().await;
}
