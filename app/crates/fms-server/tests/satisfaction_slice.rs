//! 工單滿意度（`POST /work-orders/{id}/satisfaction` + 結案時的評分邀請）。
//!
//! # 這一組要證明的是「那條鏈真的接起來了」
//!
//! 004 有欄位、DTO 有讀路徑、008 的狀態機宣告 `request_satisfaction` ——
//! 但在 067 之前中間兩段是斷的：沒有寫入者，而 `apply_side_effects` 不執行
//! 那個宣告。所以 `a_` 走完整條鏈：結案 → 邀請通知真的出現 → 申請人評分 →
//! 分數在工單上讀得到。少了任何一段，那一格就會失敗。
//!
//! # `b_` 是這一組最重要的一格
//!
//! 「申請人本人」不能用權限碼表達。`b_` 讓一個**有 `work_order:read` 的
//! 管理員**去評別人的工單，斷言 403 —— 若條件寫成權限碼，那一格會通過，
//! 而客戶收到的滿意度報告裡會有管理員自己打的分數。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
/// `user.huang` —— REQUESTER。這一組裡他是**申請人**。
const USER_REQUESTER: &str = "ffffffff-0000-4000-8000-000000000004";

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

/// 建一張**有申請人**的 IN_PROGRESS 工單。
///
/// `seed_work_order` 不設 `created_by`（那一欄可為 NULL —— 背景產生的 PM 工單
/// 就是那樣）。而 `created_by` 是這一組的授權依據，所以這裡自己建。
async fn seed_requested_work_order(ctx: &TestContext, title: &str) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.work_orders
           (tenant_id, facility_id, wo_no, work_order_type, source, title,
            status, priority, spatial_node_id, actual_start_at, created_by)
         VALUES ($1::uuid, $2::uuid,
                 'WO-ST-' || substr(md5(random()::text), 1, 9),
                 'CORRECTIVE', 'MANUAL', $3, 'IN_PROGRESS', 'MEDIUM',
                 (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                 clock_timestamp() - interval '2 hours', $4::uuid)
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(title)
    .bind(USER_REQUESTER)
    .fetch_one(&mut *tx)
    .await
    .unwrap_or_else(|e| panic!("建工單失敗：{e}"));
    tx.commit().await.expect("commit");
    id
}

/// 走 transitions 端點結案。用 `tech.liu`（總部的執行者）—— 他有
/// `work_order:execute`，而 009 補他進來正是為了讓這條路徑走得通。
async fn complete(ctx: &TestContext, id: uuid::Uuid) -> (StatusCode, Value) {
    let tech = ctx.login_as(USERNAME_TECHNICIAN_HQ).await;
    ctx.send(authed(
        json_request(
            "POST",
            &format!("/api/v1/work-orders/{id}/transitions"),
            json!({ "action": "COMPLETE", "resolution_notes": "已處理" }),
        ),
        &tech,
    ))
    .await
}

async fn invitation_count(ctx: &TestContext, id: uuid::Uuid) -> i64 {
    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.notifications
          WHERE template_code = 'SATISFACTION_REQUEST'
            AND entity_type = 'WORK_ORDER' AND entity_id = $1::uuid",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("查邀請");
    tx.commit().await.expect("commit");
    n
}

async fn set_setting(ctx: &TestContext, value: Option<i32>) {
    let mut tx = ctx.owner_tx().await;
    let sql = match value {
        Some(_) => {
            "UPDATE fms.tenants SET settings = \
                    jsonb_set(settings, '{satisfaction_editable_days}', to_jsonb($2::int)) \
                    WHERE id = $1::uuid"
        }
        None => {
            "UPDATE fms.tenants SET settings = settings - 'satisfaction_editable_days' \
                 WHERE id = $1::uuid"
        }
    };
    let q = sqlx::query(sql).bind(TENANT_ID);
    let q = match value {
        Some(v) => q.bind(v),
        None => q,
    };
    q.execute(&mut *tx).await.expect("設定");
    tx.commit().await.expect("commit");
}

/// **整條鏈**：結案 → 邀請通知出現（兩個管道）→ 申請人評分 → 工單上讀得到。
#[tokio::test]
async fn a_completing_invites_the_requester_and_the_score_lands() {
    let ctx = &TestContext::setup().await;
    let id = seed_requested_work_order(ctx, "滿意度鏈").await;

    assert_eq!(invitation_count(ctx, id).await, 0, "結案之前不該有邀請");

    let (status, done) = complete(ctx, id).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "COMPLETED");

    // **這是 067 之前不會發生的事。** 狀態機宣告了 request_satisfaction，
    // 而 apply_side_effects 從來不執行它。
    assert_eq!(
        invitation_count(ctx, id).await,
        2,
        "結案該發出 EMAIL 與 IN_APP 兩則邀請 —— 少了它，端點會存在但永遠沒有流量"
    );

    // 邀請內容要帶得出工單編號與可修改天數，否則收信的人不知道在說哪一張。
    let mut tx = ctx.owner_tx().await;
    let (subject, body): (Option<String>, String) = sqlx::query_as(
        "SELECT subject, body FROM fms.notifications
          WHERE template_code = 'SATISFACTION_REQUEST' AND entity_id = $1::uuid
            AND channel = 'EMAIL'",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀 EMAIL 邀請");
    let recipient: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT recipient_user_id FROM fms.notifications
          WHERE template_code = 'SATISFACTION_REQUEST' AND entity_id = $1::uuid LIMIT 1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀收件人");
    tx.commit().await.expect("commit");

    assert!(
        subject.as_deref().is_some_and(|s| s.contains("WO-ST-")),
        "主旨要帶工單編號：{subject:?}"
    );
    assert!(
        body.contains("14 天"),
        "信裡要寫出可修改天數（預設 14）—— 承諾的期限與實際擋人的期限必須一致：{body}"
    );
    assert_eq!(
        recipient.map(|u| u.to_string()).as_deref(),
        Some(USER_REQUESTER),
        "邀請要寄給申請人，不是負責人"
    );

    // ---- 申請人評分 ----
    let requester = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, rated) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/satisfaction"),
                json!({ "score": 4, "comment": "處理很快" }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{rated}");
    assert_eq!(rated["data"]["score"], 4);
    assert_eq!(rated["meta"]["was_first_submission"], true);
    assert_eq!(rated["meta"]["editable_days"], 14);
    assert_eq!(
        rated["meta"]["editable_days_source"], "platform_default",
        "租戶沒設就要說是平台預設 —— 那個數字會隨版本改：{}",
        rated["meta"]
    );

    // ---- 分數在工單上讀得到（DTO 從 004 起就有這一欄，只是一直是 NULL）----
    let admin = ctx.login().await;
    let (_, wo) = ctx
        .send(authed(get(&format!("/api/v1/work-orders/{id}")), &admin))
        .await;
    assert_eq!(
        wo["satisfaction_score"], 4,
        "DTO 的 satisfaction_score 該讀得到 —— 它從 004 到現在一直是 null：{wo}"
    );

    ctx.teardown().await;
}

/// **有 `work_order:read` 但不是申請人 → 403。**
///
/// 這一格是「申請人本人」那個條件的突變測試：改成權限碼會讓它通過。
#[tokio::test]
async fn b_only_the_requester_can_rate_not_anyone_who_can_read() {
    let ctx = &TestContext::setup().await;
    let id = seed_requested_work_order(ctx, "非申請人").await;
    complete(ctx, id).await;

    // 租戶管理員 —— 有 work_order:read（甚至更多），但不是申請人。
    let admin = ctx.login().await;
    let (status, denied) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/satisfaction"),
                json!({ "score": 5 }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "**管理員不該能替客戶打分。** 條件若寫成權限碼，這一格會通過，\
         而客戶收到的滿意度報告裡會有管理員自己打的分數：{denied}"
    );

    // 403 而不是 404：404 會讓申請人以為自己的工單不見了。
    // 而分數確實沒有被寫入。
    let mut tx = ctx.owner_tx().await;
    let score: Option<i16> =
        sqlx::query_scalar("SELECT satisfaction_score FROM fms.work_orders WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .expect("讀分數");
    tx.commit().await.expect("commit");
    assert_eq!(score, None, "被拒的請求不該留下任何分數");

    ctx.teardown().await;
}

/// 還沒完成 → 409；不存在 → 404；分數超出範圍 → 422。
#[tokio::test]
async fn c_the_four_failures_are_distinguishable() {
    let ctx = &TestContext::setup().await;
    let id = seed_requested_work_order(ctx, "尚未完成").await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    // 還在 IN_PROGRESS → 409（狀態衝突，不是輸入錯誤）。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/satisfaction"),
                json!({ "score": 3 }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "還沒完成該是 409 而不是 422 —— 這是狀態衝突：{p}"
    );

    // 不存在 → 404。
    let (status, _) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders/00000000-0000-4000-8000-000000000000/satisfaction",
                json!({ "score": 3 }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 分數超出 1–5 → 422（而且在應用層擋，不是讓 004 的 CHECK 變成 500）。
    complete(ctx, id).await;
    for bad in [0, 6, -1] {
        let (status, p) = ctx
            .send(authed(
                json_request(
                    "POST",
                    &format!("/api/v1/work-orders/{id}/satisfaction"),
                    json!({ "score": bad }),
                ),
                &requester,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "score={bad} 該是 422 而不是 500：{p}"
        );
        assert_eq!(p["errors"][0]["pointer"], "/score", "{p}");
    }

    // 過長的評論 → 422。
    let (status, _) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/satisfaction"),
                json!({ "score": 3, "comment": "字".repeat(2001) }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// **可修改期限由租戶定義，而 Rust 與 SQL 的預設值必須一致。**
#[tokio::test]
async fn d_the_editable_window_is_tenant_defined_and_zero_means_final() {
    let ctx = &TestContext::setup().await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    // 兩邊的預設值必須是同一個數字：067 的邀請信寫「{{editable_days}} 天內可
    // 修改」，而擋人的是 Rust 的常數。不一致的話信裡承諾的期限與實際不同，
    // 而使用者只會看到信。
    let mut tx = ctx.owner_tx().await;
    let sql_default: Option<i32> =
        sqlx::query_scalar("SELECT fms.tenant_setting_int('a_key_that_does_not_exist', 14)")
            .fetch_one(&mut *tx)
            .await
            .expect("讀預設");
    tx.commit().await.expect("commit");
    assert_eq!(
        sql_default,
        Some(fms_workorder::satisfaction::DEFAULT_EDITABLE_DAYS),
        "SQL 與 Rust 的預設天數必須一致"
    );

    // ---- 租戶設 30 天 → meta 說是租戶設的 ----
    set_setting(ctx, Some(30)).await;
    let id = seed_requested_work_order(ctx, "期限 30").await;
    complete(ctx, id).await;
    let (_, rated) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/satisfaction"),
                json!({ "score": 3 }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(rated["meta"]["editable_days"], 30, "{rated}");
    assert_eq!(rated["meta"]["editable_days_source"], "tenant_setting");

    // 期限內可以改，而 `was_first_submission` 變成 false。
    let (status, again) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/satisfaction"),
                json!({ "score": 5, "comment": "改成滿分" }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(
        again["meta"]["was_first_submission"], false,
        "第二次不是第一次 —— 前端要據此顯示「已更新」而不是「感謝評分」：{again}"
    );

    // ---- 0 天 = 一經送出即定案 ----
    set_setting(ctx, Some(0)).await;
    let id2 = seed_requested_work_order(ctx, "定案").await;
    complete(ctx, id2).await;
    let (status, first) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id2}/satisfaction"),
                json!({ "score": 2 }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "第一次一定要成功：{first}");
    let (status, blocked) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id2}/satisfaction"),
                json!({ "score": 5 }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "**0 天是一個合法的政策（一經送出即定案），不是「沒設定」**：{blocked}"
    );
    assert!(
        blocked["detail"]
            .as_str()
            .is_some_and(|d| d.contains("0") && d.contains("satisfaction_editable_days")),
        "detail 要說出期限幾天與它從哪裡來，否則使用者只知道被拒絕：{blocked}"
    );

    // 分數仍是第一次那個。
    let mut tx = ctx.owner_tx().await;
    let score: Option<i16> =
        sqlx::query_scalar("SELECT satisfaction_score FROM fms.work_orders WHERE id = $1::uuid")
            .bind(id2)
            .fetch_one(&mut *tx)
            .await
            .expect("讀分數");
    tx.commit().await.expect("commit");
    assert_eq!(score, Some(2), "被拒的修改不該改掉已定案的分數");

    ctx.teardown().await;
}

/// 同一張工單只邀請一次 —— 重開再結案不會再發。
#[tokio::test]
async fn e_reopening_and_completing_again_does_not_re_invite() {
    let ctx = &TestContext::setup().await;
    let id = seed_requested_work_order(ctx, "重開").await;
    complete(ctx, id).await;
    assert_eq!(invitation_count(ctx, id).await, 2);

    // 重開再結案。走狀態機而不是直接改狀態 —— 044 的觸發器會清 completed_at，
    // 而那正是 `within_window` 要退回 updated_at 的原因。
    //
    // 必填欄位是 `reason`（008 第 251 行的 `{reason}`），不是 `reopen_reason`；
    // 權限是 `work_order:reopen`，示範資料裡只有 TENANT_ADMIN 有。
    // 第一版把這兩個都寫錯，而測試用 `if status == OK` 包住後續斷言 ——
    // 於是整格通過而**最重要的那一句從來沒有執行**。
    let admin = ctx.login().await;
    let (status, reopened) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                json!({ "action": "REOPEN", "reason": "沒修好" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "REOPEN 該成功：{reopened}");
    assert_eq!(reopened["status"], "IN_PROGRESS", "{reopened}");

    complete(ctx, id).await;
    assert_eq!(
        invitation_count(ctx, id).await,
        2,
        "**只邀請一次。** 判斷依據是通知本身而不是旗標欄位 —— \
         旗標會與通知不同步，而通知是那件事發生過的證據"
    );

    ctx.teardown().await;
}

/// `tenants.settings` 的形狀在寫入時就被擋（它現在決定一個期限）。
#[tokio::test]
async fn f_the_settings_shape_is_enforced_on_write() {
    let ctx = &TestContext::setup().await;

    for bad in [
        r#"{"satisfaction_editable_days": "十四"}"#,
        r#"{"satisfaction_editable_days": 1.5}"#,
        r#"{"satisfaction_editable_days": 400}"#,
        r#"{"satisfaction_editable_days": -1}"#,
    ] {
        let mut tx = ctx.owner_tx().await;
        let r = sqlx::query("UPDATE fms.tenants SET settings = $2::jsonb WHERE id = $1::uuid")
            .bind(TENANT_ID)
            .bind(bad)
            .execute(&mut *tx)
            .await;
        assert!(
            r.is_err(),
            "`{bad}` 該被 ck_tenants_settings 擋下 —— \
             壞掉的值要在寫入時擋，而不是在評分時炸（那離設定它的人三層之外）"
        );
        drop(tx);
    }

    // 未知的鍵放行 —— 這個欄位會長大，每加一個設定都改 migration 沒有意義。
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        r#"UPDATE fms.tenants SET settings = '{"future_key": {"a": 1}}'::jsonb
            WHERE id = $1::uuid"#,
    )
    .bind(TENANT_ID)
    .execute(&mut *tx)
    .await
    .expect("未知的鍵該放行");
    tx.commit().await.expect("commit");

    ctx.teardown().await;
}

/// 沒有申請人的工單（PM 產生的）結案時不發邀請，也不會失敗。
#[tokio::test]
async fn g_a_work_order_without_a_requester_completes_without_inviting() {
    let ctx = &TestContext::setup().await;
    // `seed_work_order` 不設 created_by —— 那正是 PM 產生的工單的樣子。
    let id = ctx.seed_work_order(FACILITY_HQ, "無申請人").await;

    let (status, done) = complete(ctx, id).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "**沒有申請人不該讓結案失敗。** 一封邀請信發不出去是通知的問題，\
         不是工單的問題：{done}"
    );
    assert_eq!(
        invitation_count(ctx, id).await,
        0,
        "沒有申請人就沒有人可以評分，不該產生一則寄不出去的通知"
    );

    ctx.teardown().await;
}
