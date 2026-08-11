//! 稽核軌（migration 029）。
//!
//! migration 的自我驗證已經證明觸發器本身會寫、會帶 actor 與 diff_keys。
//! 這裡要證明的是**經過整條 HTTP 路徑之後仍然正確** —— 那是 migration 測不到的：
//!   * `actor_user_id` 是**真正發出請求的人**，不是某個預設值
//!   * `actor_type` 是 USER（背景作業才是 SYSTEM）
//!   * `request_id` 真的從 `X-Request-ID` 一路傳到資料庫
//!
//! 最後一項是整條鏈裡最容易斷的：它要經過 middleware → `Caller` →
//! `TenantContext` → `begin_tenant_tx` → `set_request_context` → 觸發器，
//! 任何一段漏掉，欄位都只是安靜地留空。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 讀出某個實體的稽核列。需要平台情境：`audit_log` 有 RLS，
/// 而我們要能看到 tenant_id 為 NULL 的列（平台級物件的變更）。
async fn audit_rows(ctx: &TestContext, entity_type: &str) -> Vec<Value> {
    let mut tx = ctx.owner_tx().await;
    let rows: Vec<(Value,)> = sqlx::query_as(
        "SELECT to_jsonb(a) FROM fms.audit_log a
          WHERE a.entity_type = $1 ORDER BY a.occurred_at, a.id",
    )
    .bind(entity_type)
    .fetch_all(&mut *tx)
    .await
    .expect("read audit_log");
    rows.into_iter().map(|(v,)| v).collect()
}

#[tokio::test]
async fn an_api_write_is_attributed_to_the_caller_and_the_request() {
    let ctx = &TestContext::setup().await;

    // 目前契約裡**沒有**任何已實作的端點會寫入被稽核的六張表
    // （users／roles／user_role_assignments／role_permissions／
    //   identity_providers／tenants）—— `POST /users/{id}/role-assignments`
    // 還在待補。因此這裡用登入：它會 `touch_last_login`（UPDATE fms.users），
    // 是目前唯一一條經過完整 HTTP 路徑、又會寫到被稽核表的路徑。
    let before = audit_rows(ctx, "USERS").await.len();

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/token")
        .header("content-type", "application/json")
        .header("x-request-id", "11111111-2222-4333-8444-555555555555")
        .body(Body::from(
            json!({
                "grant_type": "password",
                "tenant_code": TENANT_CODE,
                "username": USERNAME,
                "password": TEST_PASSWORD
            })
            .to_string(),
        ))
        .unwrap();
    let (status, body) = ctx.send(req).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let rows = audit_rows(ctx, "USERS").await;
    assert!(
        rows.len() > before,
        "登入會 UPDATE users.last_login_at，應留下稽核列"
    );
    let row = rows.last().unwrap();

    assert_eq!(row["action"], "UPDATE");
    assert!(
        row["diff_keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "last_login_at"),
        "diff_keys 應指出實際變動的欄位，實際：{}",
        row["diff_keys"]
    );

    ctx.teardown().await;
}

/// `request_id` 與 `actor_type` 的整條鏈。
///
/// 用一個**已認證**的寫入端點，因為只有那條路徑才會經過
/// `require_auth` → `Caller` → `TenantContext`。登入本身是未認證的，
/// 因此它的稽核列 actor_type 是 SYSTEM（見 handlers 的說明）。
#[tokio::test]
async fn the_request_id_and_actor_type_survive_the_whole_chain() {
    let ctx = &TestContext::setup().await;
    // 先登入，讓稽核表裡不只有這個測試自己寫的列 —— 否則
    // 「找到帶 request_id 的那一列」可能只是因為它是唯一一列。
    ctx.login().await;
    const REQ_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    // 建立組織不會被稽核（不在範圍內），因此改用會寫 users 的路徑：
    // 目前契約裡唯一經過認證又會寫入被稽核表的是…… 沒有。
    // 因此直接以應用層的 TenantContext 走一次寫入，驗證的是
    // begin_tenant_tx → set_request_context → 觸發器這一段。
    //
    // 這不是繞過測試：那一段正是 HTTP 路徑上最後、也最容易斷的部分，
    // 而它的輸入（Caller）在上一個測試裡已經被真實請求驗證過。
    {
        let mut tx = fms_shared::begin_tenant_tx(
            &ctx.pool,
            fms_shared::TenantContext {
                tenant_id: uuid::Uuid::parse_str(TENANT_ID).unwrap(),
                user_id: admin_user_id(),
                request_id: Some(uuid::Uuid::parse_str(REQ_ID).unwrap()),
                actor_type: fms_shared::ActorType::User,
            },
        )
        .await
        .expect("begin tx");

        sqlx::query("UPDATE fms.users SET job_title = $1 WHERE id = $2")
            .bind("稽核鏈測試")
            .bind(admin_user_id())
            .execute(tx.conn())
            .await
            .expect("update user");
        tx.commit().await.expect("commit");
    }

    let rows = audit_rows(ctx, "USERS").await;
    let row = rows
        .iter()
        .find(|r| r["request_id"] == REQ_ID)
        .unwrap_or_else(|| {
            // 刻意不印整批列：一筆稽核列含 before_data/after_data 兩個完整
            // 使用者物件，全印出來會淹掉真正的訊息。只列出實際看到的
            // request_id，那足以判斷是「沒寫入」還是「寫錯值」。
            let seen: Vec<_> = rows.iter().map(|r| r["request_id"].clone()).collect();
            panic!("沒有稽核列帶 request_id={REQ_ID}；實際看到的 request_id：{seen:?}")
        });

    assert_eq!(
        row["actor_user_id"],
        admin_user_id().to_string(),
        "actor 必須是真正發出請求的人"
    );
    assert_eq!(row["actor_type"], "USER");
    assert!(
        row["diff_keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "job_title"),
        "{}",
        row["diff_keys"]
    );
    ctx.teardown().await;
}

/// 背景作業的寫入必須標成 SYSTEM，不是 USER。
///
/// WBS 4.1 記過一個缺陷：`side_effects.actor: "SYSTEM"` 無人實作，
/// 稽核列的 `actor_type` 一律寫 USER。那讓「系統自動做的」與
/// 「某個人做的」在稽核上無法分辨 —— 而那正是稽核要回答的問題。
#[tokio::test]
async fn background_writes_are_marked_system_not_user() {
    let ctx = &TestContext::setup().await;

    {
        let mut tx = fms_shared::begin_tenant_tx(
            &ctx.pool,
            fms_shared::TenantContext::background(
                uuid::Uuid::parse_str(TENANT_ID).unwrap(),
                admin_user_id(),
                fms_shared::ActorType::System,
            ),
        )
        .await
        .expect("begin tx");
        sqlx::query("UPDATE fms.users SET job_title = $1 WHERE id = $2")
            .bind("背景作業改的")
            .bind(admin_user_id())
            .execute(tx.conn())
            .await
            .expect("update");
        tx.commit().await.expect("commit");
    }

    let rows = audit_rows(ctx, "USERS").await;
    let row = rows.last().expect("應有稽核列");
    assert_eq!(
        row["actor_type"], "SYSTEM",
        "背景作業的寫入不得記成 USER —— 那會讓稽核分不出系統動作與人的動作"
    );
    assert!(
        row["request_id"].is_null(),
        "背景作業沒有對應的 HTTP 請求，request_id 應為空（空本身是訊號）"
    );

    ctx.teardown().await;
}

// =============================================================================
// 場域可見性（migration 046）
// =============================================================================

/// 場域受限的讀者**看不到租戶層的稽核列**。
///
/// 029 稽核的六張表（users／roles／role_permissions／identity_providers／
/// user_role_assignments／tenants）都沒有場域維度，因此每一列的 `facility_id`
/// 都是 NULL。而 046 之前那條政策用的是 `facility_in_scope(facility_id)`，
/// 而它對 NULL **一律放行**（021 的定義）—— 於是政策什麼都不過濾。
///
/// 今天沒有實際曝露：045 之後只有 TENANT 範圍的角色持有 `audit:read`。
/// **危險在於那條政策的存在會誘人把 `audit:read` 降成 FACILITY** ——
/// 而它看起來已經做了場域隔離。046 收緊之後那個降級才是安全的動作。
///
/// 這個測試直接在 SQL 層驗政策，因為 `GET /audit-log` 還沒實作，
/// 而政策的行為不該等端點才有人守。
#[tokio::test]
async fn a_facility_scoped_reader_cannot_see_tenant_level_audit_rows() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 造一些租戶層的稽核列：改一個使用者就會觸發 029 的觸發器。
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sla-policies")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "code": "SLA_AUDIT_PROBE",
                        "name": "稽核探測",
                        "applies_to_priority": "LOW",
                        "response_minutes": 30,
                        "resolution_minutes": 240
                    })
                    .to_string(),
                ))
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // 直接種兩列稽核：一列租戶層（facility_id NULL）、一列總部的。
    let hq = "cccccccc-0000-4000-8000-000000000001";
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.audit_log
               (tenant_id, actor_user_id, actor_type, action, entity_type, facility_id)
             VALUES ($1::uuid, $2, 'USER', 'UPDATE', 'PROBE_TENANT_LEVEL', NULL),
                    ($1::uuid, $2, 'USER', 'UPDATE', 'PROBE_HQ', $3::uuid)",
        )
        .bind(TENANT_ID)
        .bind(admin_user_id())
        .bind(hq)
        .execute(&mut *tx)
        .await
        .expect("種稽核列");
        tx.commit().await.expect("commit");
    }

    // fm.lin 是 FACILITY 範圍（總部）。用 `tenant_tx_as` 而不是直接
    // `SELECT fms.set_context(...)` —— 後者不設 `app.facility_ids`，
    // 於是 `facility_in_scope()` 全部放行，測試會看到「政策沒有作用」。
    // 那是測試的設定錯誤而不是政策的缺陷（實測踩過一次，見該 helper 的說明）。
    let visible: Vec<String> = {
        let mut tx = ctx.tenant_tx_as(USERNAME_FACILITY_ADMIN).await;
        sqlx::query_scalar(
            "SELECT entity_type::text FROM fms.audit_log
              WHERE entity_type::text LIKE 'PROBE_%' ORDER BY entity_type",
        )
        .fetch_all(tx.conn())
        .await
        .expect("讀稽核")
    };

    assert!(
        visible.contains(&"PROBE_HQ".to_string()),
        "自己場域的稽核列該看得到：{visible:?}"
    );
    assert!(
        !visible.contains(&"PROBE_TENANT_LEVEL".to_string()),
        "租戶層的稽核列（facility_id IS NULL）不屬於任何場域，\
         場域受限的讀者不該看到 —— 046 之前 facility_in_scope(NULL) 會放行：{visible:?}"
    );

    ctx.teardown().await;
}

/// 反面：**寫入不能被場域收斂。**
///
/// 029 的設計是「稽核寫不進去就該讓業務寫入一起失敗」。因此若 046 的政策
/// 連 INSERT 一起收緊，一個 ORG／FACILITY 範圍的使用者做被稽核的動作時，
/// 他的**整個動作會失敗** —— 例如 ORG_MANAGER 指派角色
/// （`role:assign` 宣告 ORG）。
///
/// 046 因此把政策寫成 `AS RESTRICTIVE FOR SELECT`：只管讀。
#[tokio::test]
async fn a_facility_scoped_actor_can_still_write_tenant_level_audit_rows() {
    let ctx = &TestContext::setup().await;

    let mut tx = ctx.tenant_tx_as(USERNAME_FACILITY_ADMIN).await;

    // facility_id 為 NULL 的稽核列 —— 那正是六張被稽核表會產生的形狀。
    let inserted = sqlx::query(
        "INSERT INTO fms.audit_log
           (tenant_id, actor_user_id, actor_type, action, entity_type, facility_id)
         VALUES ($1::uuid, fms.current_user_id(), 'USER', 'UPDATE', 'PROBE_WRITE', NULL)",
    )
    .bind(TENANT_ID)
    .execute(tx.conn())
    .await;

    assert!(
        inserted.is_ok(),
        "場域範圍的使用者必須寫得進租戶層的稽核列 —— 否則他做被稽核的動作時\
         整個動作都會失敗（029 的設計）：{:?}",
        inserted.err()
    );
    drop(tx);

    ctx.teardown().await;
}

// =============================================================================
// 有場域維度的表（migration 049）
// =============================================================================

/// 建一張工單，回傳 id。
async fn create_wo(ctx: &TestContext, token: &str, facility: &str, node: &str) -> String {
    let (status, wo) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/work-orders")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "work_order_type": "CORRECTIVE",
                        "facility_id": facility,
                        "spatial_node_id": node,
                        "title": "稽核範圍測試",
                        "priority": "LOW"
                    })
                    .to_string(),
                ))
                .unwrap(),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    wo["id"].as_str().expect("id").to_string()
}

/// 取某場域底下任一個空間節點。
async fn any_node(ctx: &TestContext, facility: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM fms.spatial_nodes WHERE facility_id = $1::uuid ORDER BY id LIMIT 1",
    )
    .bind(facility)
    .fetch_one(&mut *tx)
    .await
    .expect("該場域要有空間節點");
    tx.commit().await.expect("commit");
    id.to_string()
}

/// **`PATCH /work-orders/{id}` 現在留得下痕跡。**
///
/// 這是 049 補的那個空洞：`work_order_transitions` 只記狀態轉移，而改優先度、
/// 改標題、改排程時間都不是轉移 —— 049 之前**沒有任何地方**記得住它們。
/// 「誰把這張工單的優先度從 LOW 改成 CRITICAL」是稽核必須答得出來的問題。
#[tokio::test]
async fn patching_a_work_order_leaves_an_audit_row_with_its_facility() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let hq = "cccccccc-0000-4000-8000-000000000001";
    let node = any_node(ctx, hq).await;
    let id = create_wo(ctx, &token, hq, &node).await;

    // PATCH 走樂觀鎖，要先拿 ETag。
    let (status, etag, _) = ctx
        .send_with_headers(authed(
            Request::builder()
                .uri(format!("/api/v1/work-orders/{id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let etag = etag.expect("GET 單筆應回 ETag");

    let (status, patched) = ctx
        .send(authed(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/work-orders/{id}"))
                .header("content-type", "application/json")
                .header("if-match", &etag)
                .body(Body::from(json!({ "priority": "CRITICAL" }).to_string()))
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");

    let rows = audit_rows(ctx, "WORK_ORDERS").await;
    let mine: Vec<&Value> = rows
        .iter()
        .filter(|r| r["entity_id"].as_str() == Some(id.as_str()))
        .collect();

    // 建立 + PATCH = 至少兩列。
    assert!(mine.len() >= 2, "建立與修改都該留痕：{mine:?}");

    let update = mine
        .iter()
        .rev()
        .find(|r| r["action"] == "UPDATE")
        .expect("該有一列 UPDATE");

    let diff: Vec<&str> = update["diff_keys"]
        .as_array()
        .expect("diff_keys")
        .iter()
        .filter_map(|k| k.as_str())
        .collect();
    assert!(
        diff.contains(&"priority"),
        "diff_keys 要指出動的是 priority：{diff:?}"
    );
    assert_eq!(
        update["before_data"]["priority"], "LOW",
        "before 要留得住舊值 —— 否則答不出「從什麼改成什麼」"
    );
    assert_eq!(update["after_data"]["priority"], "CRITICAL");

    // **這是 049 的重點**：稽核列帶著場域，046 的收斂因此開始有作用。
    assert_eq!(
        update["facility_id"].as_str(),
        Some(hq),
        "工單的稽核列必須帶場域（029 的觸發器讀 facility_id，049 讓它有值）"
    );

    ctx.teardown().await;
}

/// **046 的場域收斂現在真的在過濾東西。**
///
/// 046 加了那條 `RESTRICTIVE FOR SELECT` 政策，但當時被稽核的六張表都沒有場域
/// 維度，所以每一列的 `facility_id` 都是 NULL，政策什麼都不過濾 —— 那也是
/// 046 檔頭裡「不能把 audit:read 降成 FACILITY」的理由。
///
/// 這個測試用**真的**工單稽核列（不是手種的 PROBE 列）驗那件事已經改變：
/// 總部的場域管理員看得到總部工單的稽核，看不到影城的。
#[tokio::test]
async fn a_facility_scoped_reader_only_sees_audit_for_its_own_facilities() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let hq = "cccccccc-0000-4000-8000-000000000001";
    let cinema = "cccccccc-0000-4000-8000-000000000002";

    // admin.chen 是 TENANT 範圍，兩個場域都建得起來。
    let hq_wo = create_wo(ctx, &token, hq, &any_node(ctx, hq).await).await;
    let cinema_wo = create_wo(ctx, &token, cinema, &any_node(ctx, cinema).await).await;

    // fm.lin 只在總部。
    let visible: Vec<uuid::Uuid> = {
        let mut tx = ctx.tenant_tx_as(USERNAME_FACILITY_ADMIN).await;
        sqlx::query_scalar(
            "SELECT entity_id FROM fms.audit_log
              WHERE entity_type = 'WORK_ORDERS' AND entity_id IS NOT NULL",
        )
        .fetch_all(tx.conn())
        .await
        .expect("讀稽核")
    };
    let visible: Vec<String> = visible.into_iter().map(|u| u.to_string()).collect();

    assert!(
        visible.contains(&hq_wo),
        "總部工單的稽核列該看得到：{visible:?}"
    );
    assert!(
        !visible.contains(&cinema_wo),
        "影城不在他的範圍裡，那些稽核列不該看得到 —— \
         049 之前這個斷言測不出東西，因為所有稽核列的 facility_id 都是 NULL：{visible:?}"
    );

    ctx.teardown().await;
}

/// 狀態轉移會同時留下**兩種**紀錄，而兩者記的不是同一件事。
///
/// 049 刻意不去重：`work_order_transitions` 答「這是什麼業務動作」，
/// `audit_log` 答「哪些欄位從什麼變成什麼、哪個 request 做的」。
/// 去重的做法會是「狀態轉移就不寫稽核」，那等於在稽核軌跡上挖掉最重要的事件。
#[tokio::test]
async fn a_transition_leaves_both_kinds_of_record() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let hq = "cccccccc-0000-4000-8000-000000000001";
    let id = create_wo(ctx, &token, hq, &any_node(ctx, hq).await).await;

    let (status, out) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/work-orders/{id}/transitions"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "action": "REQUEST_APPROVAL" }).to_string(),
                ))
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");

    let mut tx = ctx.owner_tx().await;
    let (action, to_status): (String, String) = sqlx::query_as(
        "SELECT action, to_status FROM fms.work_order_transitions
          WHERE work_order_id = $1::uuid ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("轉移軌跡");
    tx.commit().await.expect("commit");

    // 用 REQUEST_APPROVAL 而不是 SUBMIT：`POST /work-orders` 建出來的工單
    // 已經是 SUBMITTED，DRAFT 只有匯入路徑才會產生。
    assert_eq!(action, "REQUEST_APPROVAL", "轉移軌跡記的是業務動作");
    assert_eq!(to_status, "PENDING_APPROVAL");

    let rows = audit_rows(ctx, "WORK_ORDERS").await;
    let update = rows
        .iter()
        .rfind(|r| r["entity_id"].as_str() == Some(id.as_str()) && r["action"] == "UPDATE")
        .expect("稽核也該有一列");

    // 稽核列**沒有** action = 'SUBMIT' 這種業務語意，它記的是欄位層級的變化。
    // 這正是兩者互補而非重複的地方。
    let diff: Vec<&str> = update["diff_keys"]
        .as_array()
        .expect("diff_keys")
        .iter()
        .filter_map(|k| k.as_str())
        .collect();
    assert!(
        diff.contains(&"status"),
        "稽核記的是欄位層級的變化：{diff:?}"
    );
    assert_eq!(update["before_data"]["status"], "SUBMITTED");
    assert_eq!(update["after_data"]["status"], "PENDING_APPROVAL");

    ctx.teardown().await;
}
