//! 一條連續業務路徑：建人 → 指派角色 → 建工單 → 派工 → 執行 → 完工
//! → SLA → 稽核 → 匯出。**每一步用前一步的輸出當輸入。**
//!
//! # 這一格要抓的是接縫，不是功能
//!
//! 40 個測試檔各驗一段，每一格都**自己佈置資料**。因此「A 的輸出能不能當
//! B 的輸入」從來沒有被任何東西走過。這裡不重驗任何單一端點的行為
//!（那些切片已經做得比這裡細），只驗那些銜接處：
//!
//!   * 步驟 1 回的 `id` 能不能直接當步驟 2 的 path 參數
//!   * 步驟 2 建立的角色指派，能不能讓步驟 4–7 通過權限檢查
//!   * 步驟 7 完工之後，SLA 報表的分母**當下**就包含它，還是要等 worker
//!   * 步驟 9 的 `entity_id` 過濾能不能命中步驟 1、2 產生的那兩列
//!   * 步驟 10 匯出的檔案裡，找不找得到步驟 1、2 的那兩個 id
//!
//! 每個斷言的失敗訊息都說出**是哪一個接縫斷了**，不是只說 assert failed。
//!
//! # 執行者是場域級的人，而且他的權限全部來自這段旅程
//!
//! 步驟 4–7 的執行者是**步驟 1 建立、步驟 2 授權**的那個人，範圍只在
//! 台北總部（`MAINTENANCE_SUPERVISOR` @ FACILITY）。
//!
//! 刻意不用 `admin.chen`：租戶級授權涵蓋所有場域，它通過**不代表**場域級的
//! 會通過 —— 那正是 `sql/010` 的 T3 曾經掩蓋過的問題（見
//! 「fix(seed): 補上總部的技師」）。
//!
//! 也不用 `tech.liu`：他確實是場域級的，但 **TECHNICIAN 沒有
//! `work_order:assign`**（他有 `work_order:execute`，派工是排程者的動作）。
//! 更重要的是，用種子使用者的話，執行者的權限來自 009 而不是這段旅程 ——
//! 拿掉步驟 2 他照樣做得了事，接縫就沒有被驗到。
//!
//! `MAINTENANCE_SUPERVISOR` 是唯一同時具備 `work_order:create`／`:assign`／
//! `:execute`／`report:read` 的**非租戶級**角色，四項的 `min_scope_level`
//! 都是 FACILITY（026）。
//!
//! # 反面：`b_` 拿掉步驟 2，ASSIGN 必須被拒
//!
//! 少了它，這一格證明不了自己在驗接縫 —— 一個「權限一律放行」的實作
//! 也會讓 `a_` 全綠。
//!
//! # 稽核與匯出仍由租戶管理員做，那不是疏漏
//!
//! `audit:read` 與 `audit:export` 都是 `require_tenant_scoped_permission`，
//! 而 `MAINTENANCE_SUPERVISOR` 兩者都沒有。合規查詢本來就是租戶級職能，
//! 硬塞給場域主管才是錯的。步驟 9、10 因此換回 `admin.chen`。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
/// 4F 空調箱（總部）—— 有設備才滿足 `ck_wo_target`。
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";
/// 場域級的執行者角色。選它的理由見檔頭。
const ROLE_SUPERVISOR: &str = "MAINTENANCE_SUPERVISOR";
/// 有 SLA policy 的優先度（種子有 CRITICAL／HIGH／MEDIUM）——
/// 沒有 policy 的工單不會進 SLA 分母，步驟 8 就驗不到東西。
const PRIORITY: &str = "MEDIUM";

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

/// 讓步驟 1 建立的帳號真的能用（設密碼 + 轉 ACTIVE）。
///
/// **Phase 1 沒有任何端點做得到這件事**，而那是刻意的：
///   * `POST /users` 不收密碼 —— 管理員替別人設初始密碼，等於那個密碼
///     曾經被第三人知道（見 `users.rs` 檔頭）
///   * `PATCH /users/{id}` 明文不能改 `status`
///   * `POST /users/{id}/suspend` 只往 SUSPENDED／DEPROVISIONED 走
///   * 契約裡負責這段的 `POST /auth/password/change` 尚未實作
///
/// 因此走 `owner_tx` —— 與 `TestContext::setup()` 為 `TEST_USERS` 設定密碼
/// 的做法完全相同。這是**佈置資料**，不是繞過被驗證的端點：
/// 步驟 3 已經先證明了在這之前那個帳號登不進來，而下面緊接著的登入
/// 證明了它現在能用。兩者之間唯一的差別就是這個函式做的事。
async fn activate(ctx: &TestContext, user_id: &str) {
    let hash = fms_identity::password::hash(TEST_PASSWORD).expect("hash");
    let mut tx = ctx.owner_tx().await;
    let affected = sqlx::query(
        "UPDATE fms.users SET password_hash = $2, status = 'ACTIVE' WHERE id = $1::uuid",
    )
    .bind(user_id)
    .bind(&hash)
    .execute(&mut *tx)
    .await
    .expect("啟用帳號")
    .rows_affected();
    tx.commit().await.expect("commit");
    // 0 列代表 user_id 根本不是步驟 1 建立的那個人 —— 而後面每一步都會
    // 以 403 失敗，看起來像權限問題。
    assert_eq!(affected, 1, "啟用帳號時沒有命中任何列：user_id = {user_id}");
}

/// 送出一個轉換，並在失敗時說出是**哪一個動作**在哪一個工單上斷掉。
async fn transition(ctx: &TestContext, token: &str, wo_id: &str, body: Value) -> Value {
    let action = body["action"].as_str().unwrap_or("?").to_string();
    let (status, resp) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{wo_id}/transitions"),
                body,
            ),
            token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "接縫斷了：工單 {wo_id} 的 {action} 沒有成功。\
         這張工單與執行者都是這段旅程前面幾步產生的 —— \
         單獨的 work_order_slice 會通過，是銜接處出了問題：{resp}"
    );
    resp
}

/// 建立使用者並指派角色，回傳 `(user_id, assignment_id, username)`。
///
/// `a_` 與 `b_` 共用前兩步；`b_` 傳 `None` 表示**跳過步驟 2**。
async fn create_and_maybe_assign(
    ctx: &TestContext,
    admin: &str,
    username: &str,
    role_code: Option<&str>,
) -> (String, Option<String>) {
    // ---- 步驟 1：建立使用者 ----
    let (status, user) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/users",
                json!({
                    "username": username,
                    "display_name": "旅程測試 · 場域主管",
                    "email": format!("{username}@example.test"),
                    "job_title": "維護主管"
                }),
            ),
            admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "步驟 1 建立使用者失敗：{user}");
    assert_eq!(
        user["status"], "INVITED",
        "步驟 1 建立的帳號應為 INVITED（`POST /users` 刻意不設密碼）：{user}"
    );
    let user_id = user["id"]
        .as_str()
        .unwrap_or_else(|| panic!("步驟 1 的回應沒有 id，後面每一步都無從接起：{user}"))
        .to_string();

    let Some(role_code) = role_code else {
        return (user_id, None);
    };

    // ---- 步驟 2：把步驟 1 的 id 直接當 path 參數 ----
    let (status, assignment) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/users/{user_id}/role-assignments"),
                json!({
                    "role_code": role_code,
                    "scope_type": "FACILITY",
                    "scope_id": FACILITY_HQ
                }),
            ),
            admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "接縫斷了：步驟 1 回的 id（{user_id}）當成步驟 2 的 path 參數被拒絕：{assignment}"
    );
    assert_eq!(assignment["role_code"], role_code, "{assignment}");
    assert_eq!(
        assignment["scope_type"], "FACILITY",
        "執行者必須是**場域級**的 —— 租戶級通過不代表場域級會通過：{assignment}"
    );
    let assignment_id = assignment["id"]
        .as_str()
        .unwrap_or_else(|| panic!("步驟 2 的回應沒有 id，步驟 9 就查不到這一列：{assignment}"))
        .to_string();

    (user_id, Some(assignment_id))
}

/// 以指定帳號嘗試登入，回傳狀態碼（不斷言成功 —— 步驟 3 要的正是失敗）。
async fn try_login(ctx: &TestContext, username: &str) -> (StatusCode, Value) {
    ctx.send(json_request(
        "POST",
        "/api/v1/auth/token",
        json!({
            "grant_type": "password",
            "tenant_code": TENANT_CODE,
            "username": username,
            "password": TEST_PASSWORD
        }),
    ))
    .await
}

// =============================================================================
// a_：完整旅程
// =============================================================================

#[tokio::test]
async fn a_a_new_hire_is_onboarded_and_closes_a_work_order_end_to_end() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let username = "journey.supervisor";

    // ---- 步驟 1 + 2 ----
    let (user_id, assignment_id) =
        create_and_maybe_assign(ctx, &admin, username, Some(ROLE_SUPERVISOR)).await;
    let assignment_id = assignment_id.expect("a_ 一定有步驟 2");

    // ---- 步驟 3：那個人**還登不進來**（刻意插在中間的反面斷言）----
    //
    // 少了它，步驟 1 可能其實建出了一個可登入的帳號而沒有人發現 ——
    // 而回應本身看不出來（`UserDto` 沒有密碼欄位）。
    let (status, refused) = try_login(ctx, username).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "步驟 1 建立的帳號竟然登得進來 —— `POST /users` 不設密碼這件事沒有生效，\
         INVITED 只是一個標籤：{refused}"
    );

    // 讓他能用。這是這段旅程裡唯一不走端點的一步，理由見 `activate`。
    activate(ctx, &user_id).await;

    let (status, token_body) = try_login(ctx, username).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "啟用之後仍然登不進來 —— 那表示步驟 3 的失敗不是因為帳號還沒啟用，\
         這個反面斷言就沒有證明它想證明的事：{token_body}"
    );
    let supervisor = token_body["access_token"]
        .as_str()
        .expect("登入回應應有 access_token")
        .to_string();
    assert_eq!(
        token_body["user_id"], user_id,
        "接縫斷了：登入回的 user_id 與步驟 1 建立的不是同一個人：{token_body}"
    );

    // ---- 步驟 4：由**步驟 2 授權的那個人**建工單 ----
    //
    // 他的 `work_order:create` 完全來自步驟 2 的指派（`b_` 證明了這一點）。
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "4F 空調箱異音（端到端旅程）",
                    "priority": PRIORITY
                }),
            ),
            &supervisor,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "接縫斷了：步驟 2 指派的場域級角色沒有讓這個人在自己的場域建得了工單：{wo}"
    );
    assert_eq!(wo["status"], "SUBMITTED", "{wo}");
    let wo_id = wo["id"].as_str().expect("工單應有 id").to_string();
    assert_eq!(
        wo["requester"]["id"], user_id,
        "接縫斷了：requester 應該是步驟 1 建立的那個人：{wo}"
    );

    // ---- 步驟 5：ASSIGN，指派給步驟 1 的人（也就是他自己）----
    let assigned = transition(
        ctx,
        &supervisor,
        &wo_id,
        json!({ "action": "ASSIGN", "assignee_id": user_id }),
    )
    .await;
    assert_eq!(assigned["status"], "ASSIGNED", "{assigned}");
    assert_eq!(
        assigned["assignee"]["id"], user_id,
        "接縫斷了：步驟 1 的 id 當成 assignee_id 沒有被寫進工單：{assigned}"
    );

    // ---- 步驟 6：START_WORK ----
    let started = transition(ctx, &supervisor, &wo_id, json!({ "action": "START_WORK" })).await;
    assert_eq!(started["status"], "IN_PROGRESS", "{started}");
    assert!(
        started["actual_start_at"].as_str().is_some(),
        "START_WORK 的 set_actual_start 副作用沒有生效 —— \
         沒有實際開始時間，SLA 的回應量測就無從算起：{started}"
    );

    // ---- 步驟 7：COMPLETE ----
    let completed = transition(
        ctx,
        &supervisor,
        &wo_id,
        json!({
            "action": "COMPLETE",
            "resolution_notes": "更換軸承並測試，異音消失",
            "labor_minutes": 45
        }),
    )
    .await;
    assert_eq!(completed["status"], "COMPLETED", "{completed}");
    assert!(
        completed["completed_at"].as_str().is_some(),
        "COMPLETED 沒有 completed_at，步驟 8 的解決量測會判不出來：{completed}"
    );

    // ---- 步驟 8：SLA 報表**當下**就量得到這張工單 ----
    //
    // 這個接縫問的是時序：完工之後分母立刻就包含它，還是要等 worker 掃過？
    // 走 032 的量測鏈 —— `resolution_due_at` 由建立時解析 policy 寫入，
    // 判定在查詢當下做，因此不需要等任何背景作業。
    //
    // 用 `strict`：MEDIUM 的政策宣告 `business_hours_only`，而總部有班表，
    // 因此期限本來就是營業時間意義下算的，兩種口徑都納入（038／039）。
    //
    // 每個測試有自己的資料庫，而 009 **不建任何工單** —— 所以窗口裡
    // 只有這一張。分母因此可以精確斷言為 1，不是「至少 1」：
    // 後者在報表把別的東西也算進來時不會失敗。
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let from = today - chrono::Duration::days(1);
    let (status, report) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/sla-compliance?from={from}&to={today}\
                 &group_by=priority&strictness=strict"
            )),
            &supervisor,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "接縫斷了：場域主管讀不到 SLA 報表（report:read 的 min_scope_level 是 FACILITY）：{report}"
    );
    let row = report["data"]
        .as_array()
        .expect("報表應有 data")
        .iter()
        .find(|r| r["group_key"] == PRIORITY)
        .unwrap_or_else(|| {
            panic!(
                "接縫斷了：步驟 7 完工的 {PRIORITY} 工單完全沒有出現在 SLA 報表裡。\
                 分母是空的，代表報表看不到剛剛做完的事：{report}"
            )
        });
    assert_eq!(
        row["resolution_total"], 1,
        "接縫斷了：完工的工單沒有進 SLA 的**分母**。\
         032 之前這裡會是 0（`resolution_due_at` 從來沒有東西寫），\
         而 0 分母的達成率是 null —— 報表看起來「沒事」而不是「漏了」：{row}"
    );
    assert!(
        row["resolution_compliance_pct"].is_f64(),
        "分母是 1 卻算不出達成率：{row}"
    );

    // ---- 步驟 9：稽核查得到步驟 1、2 ----
    //
    // 兩次查詢，`entity_id` 分別是步驟 1 與步驟 2 **各自回的 id** ——
    // 029 的觸發器記的是那一列自己的 `id`，因此使用者的 id 查不到
    // 角色指派那一列。這正是這一格要驗的東西：前一步的輸出接得上。
    for (entity_type, entity_id, what) in [
        ("USERS", user_id.as_str(), "步驟 1 建立使用者"),
        (
            "USER_ROLE_ASSIGNMENTS",
            assignment_id.as_str(),
            "步驟 2 指派角色",
        ),
    ] {
        let (status, body) = ctx
            .send(authed(
                get(&format!(
                    "/api/v1/audit-log?entity_type={entity_type}&entity_id={entity_id}"
                )),
                &admin,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let rows = body["data"].as_array().cloned().unwrap_or_default();
        assert!(
            rows.iter()
                .any(|r| r["action"] == "CREATE" && r["entity_type"] == entity_type),
            "接縫斷了：{what}（{entity_type} {entity_id}）在稽核軌跡裡找不到。\
             029 的觸發器要嘛沒有記，要嘛記的 entity_id 不是那一列自己的 id —— \
             兩者都會讓「這個人是誰放進來的」變成查不出來：{body}"
        );
    }

    // ---- 步驟 10：匯出拿得到檔案，而且檔案裡有步驟 1、2 ----
    //
    // 過濾用 `actor_user_id` 而不是 `entity_id`：步驟 1、2 是**兩個不同的
    // entity**（見上面），單一 entity_id 撈不到兩者。而它們的共同點正是
    // 「都是這個管理員做的」。
    let (status, me) = ctx.send(authed(get("/api/v1/auth/me"), &admin)).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    let admin_id = me["user"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("/auth/me 沒有回 user.id：{me}"))
        .to_string();

    let (status, export) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/audit-log:export",
                json!({ "actor_user_id": admin_id }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "步驟 10 建立匯出作業失敗：{export}"
    );
    assert_eq!(export["status"], "PENDING", "{export}");
    let export_id = export["id"].as_str().expect("匯出作業應有 id").to_string();

    // worker 的 handler 直接呼叫 —— 不必等 relay 的 idle_interval。
    // 用 `test_storage()` 而不是 `build_storage()`：後者走
    // `StorageSettings::from_env()`，而測試刻意要能在沒有 .env 時跑。
    let storage = test_storage();
    let produced =
        fms_worker::audit_export::AuditExportHandler::new(ctx.owner_pool().await, storage.clone())
            .produce(export_id.parse().expect("匯出作業的 id 應為 uuid"))
            .await
            .expect("產檔");
    assert!(
        produced >= 2,
        "匯出只寫了 {produced} 列 —— 步驟 1、2 至少該有兩列"
    );

    let (status, done) = ctx
        .send(authed(
            get(&format!("/api/v1/audit-log/exports/{export_id}")),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(
        done["status"], "COMPLETED",
        "接縫斷了：worker 產完檔之後，狀態端點還沒看到它：{done}"
    );
    let download_url = done["download_url"]
        .as_str()
        .unwrap_or_else(|| panic!("COMPLETED 一定要有下載網址，否則匯出等於沒做：{done}"))
        .to_string();

    // 真的把它抓下來。只檢查資料庫欄位的話，「寫進物件儲存了嗎」
    // 完全沒被驗到 —— 而預簽網址簽錯的症狀正是「網址看起來對、下載回 403」。
    let csv = reqwest::get(&download_url)
        .await
        .expect("下載匯出檔")
        .text()
        .await
        .expect("讀取匯出檔");
    for (entity_id, what) in [
        (user_id.as_str(), "步驟 1 建立的使用者"),
        (assignment_id.as_str(), "步驟 2 建立的角色指派"),
    ] {
        assert!(
            csv.contains(entity_id),
            "接縫斷了：匯出的檔案裡找不到{what}（{entity_id}）。\
             端點回了 COMPLETED 與一個下載得到的網址，但內容不含這段旅程做的事 —— \
             這種失敗在稽核當下才會被發現：\n{}",
            csv.lines().take(5).collect::<Vec<_>>().join("\n")
        );
    }

    ctx.teardown().await;
}

// =============================================================================
// b_：反面 —— 拿掉步驟 2，ASSIGN 必須被拒
// =============================================================================

/// **這一格是 `a_` 的自我檢驗。**
///
/// 拿掉步驟 2 之後 ASSIGN 若還會過，就表示 `a_` 走完十步其實沒有驗到任何
/// 接縫 —— 執行者的權限來自別的地方（種子、或根本沒在檢查），而
/// 「步驟 2 的輸出讓步驟 5 通得過」這句話是假的。
///
/// 擋下 ASSIGN 的其實是**兩道獨立的閘**，順序是 RLS → 權限，而它們回的
/// 狀態碼不同。兩道都要驗，缺一不可：
///
///   完全沒有角色     → **404**，場域清單是空的，工單在 RLS 那層就消失了，
///                      handler 走不到權限檢查
///   TECHNICIAN@總部  → **403**，工單看得見，但缺 `work_order:assign`
///
/// 只驗第一道的話，一個「權限檢查整個拿掉」的實作照樣全綠 ——
/// 因為看不見的東西本來就派不了。
///
/// 工單由管理員建立：這一格要問的是「**這個人**能不能派工」，
/// 所以得先有一張派得了的工單。工單怎麼來的不是這一格的主題。
#[tokio::test]
async fn b_without_the_role_assignment_the_same_person_cannot_assign() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let username = "journey.noroles";

    // 步驟 1，**跳過步驟 2**。
    let (user_id, assignment_id) = create_and_maybe_assign(ctx, &admin, username, None).await;
    assert!(assignment_id.is_none(), "這一格刻意不指派角色");
    activate(ctx, &user_id).await;

    let (status, token_body) = try_login(ctx, username).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "沒有角色不影響登入 —— 認證與授權是兩件事：{token_body}"
    );
    let no_roles = token_body["access_token"]
        .as_str()
        .expect("token")
        .to_string();

    // 管理員建一張可派工的工單。
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "反面測試用工單",
                    "priority": PRIORITY
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    let wo_id = wo["id"].as_str().expect("id").to_string();

    // ---- ASSIGN 必須被拒 ----
    let (status, denied) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{wo_id}/transitions"),
                json!({ "action": "ASSIGN", "assignee_id": user_id }),
            ),
            &no_roles,
        ))
        .await;
    // **404，不是 403** —— 而這個差別本身值得記下來。
    //
    // 沒有任何角色指派的人，`user_accessible_facilities` 是空的，於是
    // `begin_tenant_tx` 填進 `app.facility_ids` 的清單是空的，工單的
    // facility_scope 政策直接讓那一列消失。handler 在讀取當下就找不到它，
    // **根本走不到權限檢查那一行**。
    //
    // 兩道閘的順序是 RLS → 權限，而這一格證明的是第一道。第二道由下面
    // 的 TECHNICIAN 案例單獨證明 —— 只驗這一個的話，一個「權限檢查整個
    // 拿掉」的實作也會通過。
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "**`a_` 沒有在驗接縫。** 一個沒有任何角色指派的人竟然派得了工 —— \
         那表示 `a_` 步驟 5 的成功與步驟 2 無關，十個步驟只是各自跑過一遍：{denied}"
    );

    // 同一個人在同一張工單上做 START_WORK 也該被拒 ——
    // 只驗 ASSIGN 的話，一個「只漏掉 work_order:assign 檢查」的實作會讓
    // 上面那句話成立，但整條授權鏈其實仍然是壞的。
    let (status, denied) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{wo_id}/transitions"),
                json!({ "action": "START_WORK" }),
            ),
            &no_roles,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "沒有角色的人也執行得了工單：{denied}"
    );

    // ---- 第二道閘：看得見，但仍然不准派工 ----
    //
    // `TECHNICIAN` @ 總部 —— 場域在範圍內（所以工單看得見，RLS 那道過了），
    // 但他沒有 `work_order:assign`。派工是排程者的動作，不是執行者的。
    //
    // 這一段同時釘住 `a_` 檔頭那句「不能用 tech.liu 當步驟 5 的執行者」：
    // 那不是風格偏好，是他真的做不到。角色的權限表哪天改了，這裡會失敗。
    let (tech_user_id, _) =
        create_and_maybe_assign(ctx, &admin, "journey.tech", Some("TECHNICIAN")).await;
    activate(ctx, &tech_user_id).await;
    let (_, tb) = try_login(ctx, "journey.tech").await;
    let tech = tb["access_token"].as_str().expect("token").to_string();

    let (status, denied) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{wo_id}/transitions"),
                json!({ "action": "ASSIGN", "assignee_id": tech_user_id }),
            ),
            &tech,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "TECHNICIAN 看得到這張工單，卻也派得了工 —— \
         `work_order:assign` 的檢查沒有作用：{denied}"
    );
    assert_eq!(
        denied["detail"], "missing permission: work_order:assign",
        "被擋的理由應該是缺 work_order:assign。理由不同代表擋下它的是**別的東西**，\
         而那樣這一格就沒有在證明權限閘有效：{denied}"
    );

    ctx.teardown().await;
}
