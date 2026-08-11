//! SLA 量測鏈的前三段（ADR-12、migration 032）。
//!
//! 這一組測試的核心不是「新功能會動」，而是**舊的假數字不再出現**。
//!
//! 032 之前 `resolution_due_at` 沒有任何東西會寫，而 004 的完成判定是
//! `resolution_due_at IS NULL OR now() <= resolution_due_at` —— 左邊恆為真，
//! 於是每一張完成的工單都被標成 `MET`。SLA 達成率報表做出來會是 100%，
//! 而看報表的人沒有辦法知道那是假的。
//!
//! 因此 `completing_without_a_policy_is_not_applicable_not_met` 是本檔最重要的
//! 一個測試：它斷言的是「沒在量」與「達成」分得開。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const FACILITY_CINEMA: &str = "cccccccc-0000-4000-8000-000000000002";
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";
const TECH_WANG: &str = "ffffffff-0000-4000-8000-000000000003";

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 開一張工單，回傳 (id, 回應主體)。
async fn create_wo(ctx: &TestContext, token: &str, extra: Value) -> (String, Value) {
    let mut body = json!({
        "work_order_type": "CORRECTIVE",
        "facility_id": FACILITY_HQ,
        "asset_id": SEED_AHU,
        "title": "SLA 量測測試",
    });
    for (k, v) in extra.as_object().expect("extra 應為物件") {
        body[k] = v.clone();
    }
    let (status, wo) = ctx
        .send(authed(
            json_request("POST", "/api/v1/work-orders", body),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "建立工單失敗：{wo}");
    (wo["id"].as_str().expect("id").to_string(), wo)
}

async fn transition(ctx: &TestContext, token: &str, id: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request(
            "POST",
            &format!("/api/v1/work-orders/{id}/transitions"),
            body,
        ),
        token,
    ))
    .await
}

/// `sla_basis`（038）也不在 DTO 裡 —— 它是「期限是怎麼算的」的快照。
async fn sla_basis(ctx: &TestContext, id: &str) -> Option<String> {
    let mut tx = ctx.owner_tx().await;
    sqlx::query_scalar("SELECT sla_basis FROM fms.work_orders WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .expect("讀 sla_basis")
}

/// 直接讀資料庫。`first_responded_at` 與 `actor_type` 不在 DTO 裡，
/// 而它們正是 032 決定 B 要改的兩個東西。
async fn wo_row(ctx: &TestContext, id: &str) -> (Option<String>, bool, i16) {
    let mut tx = ctx.owner_tx().await;
    let row: (
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<i16>,
    ) = sqlx::query_as(
        "SELECT sla_state, first_responded_at, reopened_count
               FROM fms.work_orders WHERE id = $1::uuid",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀工單");
    (row.0, row.1.is_some(), row.2.unwrap_or(0))
}

// =============================================================================
// 第 1 段：開單時解析 policy 並算出 due（決定 A、F）
// =============================================================================

/// 種子的三個 policy 綁在 priority 上（CRITICAL 15/120、HIGH 15/60、
/// MEDIUM 60/480），而 `applies_to_priority` 在 032 之前是零讀取點。
///
/// **CRITICAL 與 HIGH 的政策宣告 `business_hours_only = false`，MEDIUM 宣告
/// true。** 因此前兩者的期限就是牆鐘差值，而 MEDIUM 的期限落在下一個營業
/// 時段內（038）—— 那個差值取決於「現在是星期幾幾點」，不能寫成常數。
///
/// MEDIUM 改為斷言一個與日期無關的不變量：**營業時間的期限永遠不早於自然
/// 時間的期限**。營業時間只會把可用的分鐘往後推，不可能往前。
#[tokio::test]
async fn creation_resolves_the_policy_from_priority() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    for (priority, response_min, resolution_min) in [("CRITICAL", 15, 120), ("HIGH", 15, 60)] {
        let (_, wo) = create_wo(ctx, &token, json!({ "priority": priority })).await;

        let created: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(wo["created_at"].clone()).expect("created_at");
        let resp: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(wo["response_due_at"].clone())
                .unwrap_or_else(|_| panic!("{priority} 應有 response_due_at：{wo}"));
        let resol: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(wo["resolution_due_at"].clone())
                .unwrap_or_else(|_| panic!("{priority} 應有 resolution_due_at：{wo}"));

        assert_eq!(
            (resp - created).num_minutes(),
            response_min,
            "{priority} 的政策不看營業時間，因此期限就是牆鐘差值"
        );
        assert_eq!((resol - created).num_minutes(), resolution_min);
        assert_eq!(wo["sla_state"], "ON_TRACK", "有目標且未逾期：{wo}");
    }

    // MEDIUM → SLA_STANDARD，business_hours_only = true。
    let (id, wo) = create_wo(ctx, &token, json!({ "priority": "MEDIUM" })).await;
    let created: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["created_at"].clone()).expect("created_at");
    let resol: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["resolution_due_at"].clone()).expect("resolution_due_at");
    assert!(
        (resol - created).num_minutes() >= 480,
        "營業時間的期限不可能早於自然時間的（實際 {} 分）",
        (resol - created).num_minutes()
    );
    assert_eq!(
        sla_basis(ctx, &id).await.as_deref(),
        Some("BUSINESS_HOURS"),
        "總部有班表，因此 MEDIUM 應以營業時間計算"
    );

    ctx.teardown().await;
}

/// 種子只覆蓋 CRITICAL／HIGH／MEDIUM。`LOW` 解析不到 policy ——
/// 那是目錄的缺口，而缺口必須看得見。
///
/// **不能掉到一個「差不多的」policy 上**，也不能留在 `ON_TRACK`：
/// `ON_TRACK` 的意思是「有目標且還沒逾期」，而這裡根本沒有目標。
/// 兩者混在一起，報表就分不出「達成」與「沒在量」。
#[tokio::test]
async fn an_unmatched_priority_is_not_applicable() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (_, wo) = create_wo(ctx, &token, json!({ "priority": "LOW" })).await;

    assert!(
        wo["response_due_at"].is_null() && wo["resolution_due_at"].is_null(),
        "LOW 沒有對應 policy，不該有 due：{wo}"
    );
    assert_eq!(
        wo["sla_state"], "NOT_APPLICABLE",
        "沒有目標就是沒在量，不是 ON_TRACK：{wo}"
    );

    ctx.teardown().await;
}

/// 場域先於優先度。
///
/// 這是解析順序的關鍵一格：某棟樓的合約不該被另一條租戶通用規則蓋掉。
/// 種子的三個 policy 都是租戶通用，因此這裡插一個場域專屬的來驗。
#[tokio::test]
async fn a_facility_policy_beats_a_tenant_wide_one() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    {
        let mut tx = ctx.owner_tx().await;
        // 總部專屬、綁 HIGH。租戶通用的 SLA_CLEANING 也綁 HIGH（15/60），
        // 因此兩者都匹配 —— 順序決定誰贏。
        sqlx::query(
            // `business_hours_only = false` 是必要的：038 之後預設值 true 會讓
            // 期限落在下一個營業時段，於是這個測試量到的是營業時間計算而不是
            // **解析順序** —— 而後者才是它要驗的東西。
            "INSERT INTO fms.sla_policies
               (tenant_id, facility_id, code, name, applies_to_priority,
                response_minutes, resolution_minutes, business_hours_only)
             VALUES ($1::uuid, $2::uuid, 'SLA_HQ_HIGH', '總部高優先',
                     'HIGH', 5, 30, false)",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_HQ)
        .execute(&mut *tx)
        .await
        .expect("建立場域專屬 policy");
        tx.commit().await.expect("commit");
    }

    let (_, wo) = create_wo(ctx, &token, json!({ "priority": "HIGH" })).await;
    let created: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["created_at"].clone()).expect("created_at");
    let resol: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["resolution_due_at"].clone()).expect("resolution_due_at");
    assert_eq!(
        (resol - created).num_minutes(),
        30,
        "總部的工單應命中 SLA_HQ_HIGH（30 分），而不是租戶通用的 SLA_CLEANING（60 分）：{wo}"
    );

    // 反面：另一個場域不受影響，仍然吃租戶通用的規則。
    // 少了這一段，「場域優先」與「新 policy 蓋掉所有人」分不出來。
    let (_, other) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_CINEMA,
                    "spatial_node_id": "10000000-0000-4000-8000-000000000013",
                    "title": "影廳 SLA",
                    "priority": "HIGH"
                }),
            ),
            &token,
        ))
        .await;
    let created: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(other["created_at"].clone()).expect("created_at");
    let resol: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(other["resolution_due_at"].clone()).expect("resolution_due_at");
    assert_eq!(
        (resol - created).num_minutes(),
        60,
        "影廳沒有專屬 policy，應仍吃租戶通用的 60 分：{other}"
    );

    ctx.teardown().await;
}

/// 兩個維度衝突時，**場域的精確度贏過優先度的精確度**。
///
/// 前一個測試抓不到排序錯誤：那裡兩個候選都綁了 `HIGH`，因此
/// 「場域先」與「優先度先」會選到同一筆 —— 突變測試實測到把
/// `ORDER BY` 兩行對調，十個測試全部照過。
///
/// 真正有鑑別力的是這一格：
///
/// | policy | facility | priority | 解決分鐘 |
/// |---|---|---|---|
/// | `SLA_HQ_ANY`（本測試建立） | 總部 | （通用） | 45 |
/// | `SLA_CRITICAL`（種子） | （通用） | CRITICAL | 120 |
///
/// 兩者都匹配「總部的 CRITICAL 工單」，而且各自只精確一個維度。
/// 場域先 → 45；優先度先 → 120。
#[tokio::test]
async fn facility_specificity_outranks_priority_specificity() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            // 同樣關掉營業時間（見上一個測試的說明）。
            "INSERT INTO fms.sla_policies
               (tenant_id, facility_id, code, name, applies_to_priority,
                response_minutes, resolution_minutes, business_hours_only)
             VALUES ($1::uuid, $2::uuid, 'SLA_HQ_ANY', '總部通用', NULL, 10, 45, false)",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_HQ)
        .execute(&mut *tx)
        .await
        .expect("建立總部通用 policy");
        tx.commit().await.expect("commit");
    }

    let (_, wo) = create_wo(ctx, &token, json!({ "priority": "CRITICAL" })).await;
    let created: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["created_at"].clone()).expect("created_at");
    let resol: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["resolution_due_at"].clone()).expect("resolution_due_at");

    assert_eq!(
        (resol - created).num_minutes(),
        45,
        "總部的合約（45 分）應勝過租戶通用的 CRITICAL 規則（120 分）：{wo}"
    );

    ctx.teardown().await;
}

/// 決定 A：草稿不起算。
///
/// `DRAFT` 是還沒送出的東西，把它計入等於因為使用者慢慢填表而扣自己的分。
/// 時鐘在 SUBMIT 那一刻才開始。
#[tokio::test]
async fn the_clock_starts_at_submitted_not_at_draft() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (id, wo) = create_wo(ctx, &token, json!({ "priority": "HIGH", "as_draft": true })).await;
    assert_eq!(wo["status"], "DRAFT");
    assert!(
        wo["resolution_due_at"].is_null(),
        "草稿不該有 SLA 目標：{wo}"
    );

    let (status, submitted) = transition(ctx, &token, &id, json!({ "action": "SUBMIT" })).await;
    assert_eq!(status, StatusCode::OK, "{submitted}");
    assert_eq!(submitted["status"], "SUBMITTED");

    let resol: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(submitted["resolution_due_at"].clone())
            .unwrap_or_else(|_| panic!("SUBMIT 後應算出 due：{submitted}"));
    let created: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(submitted["created_at"].clone()).expect("created_at");

    // 從 SUBMIT 那一刻起算，而不是從 created_at。兩者在測試裡只差幾毫秒，
    // 因此斷言方向：due 必須嚴格晚於 created_at + 60 分。
    assert!(
        resol > created + chrono::Duration::minutes(60),
        "時鐘應從 SUBMIT 起算（晚於 created_at+60），實際 due={resol} created={created}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 第 2 段：回應時刻（決定 B）
// =============================================================================

/// 人為派工算回應，而且 transition 記成 `USER`。
#[tokio::test]
async fn a_human_assign_counts_as_a_response() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (id, _) = create_wo(ctx, &token, json!({ "priority": "HIGH" })).await;

    let (status, body) = transition(
        ctx,
        &token,
        &id,
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, responded, _) = wo_row(ctx, &id).await;
    assert!(responded, "人為 ASSIGN 應設 first_responded_at");

    let mut tx = ctx.owner_tx().await;
    let actor_type: String = sqlx::query_scalar(
        "SELECT actor_type FROM fms.work_order_transitions
          WHERE work_order_id = $1::uuid AND action = 'ASSIGN'",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀 transition");
    assert_eq!(actor_type, "USER");

    ctx.teardown().await;
}

/// `AUTO_ASSIGN` 不算回應 —— 這是 032 修掉的既有缺陷。
///
/// 自動派工把工單塞給某個人，而那個人可能還沒看過它。舊條件會把那一刻記成
/// 回應時刻，於是「平均回應時間」量到的是**系統派工多快**，不是人多快接手。
/// 一個看起來很漂亮而且完全沒有意義的數字。
///
/// 同時斷言 `actor_type` —— 目錄裡 `AUTO_ASSIGN` 的 `side_effects` 帶
/// `"actor": "SYSTEM"`，但 032 之前沒有任何地方讀它，於是每一筆 transition
/// 都吃 `DEFAULT 'USER'`。稽核軌跡上「誰做的」那一欄有一部分是假的。
///
/// 這個測試**不能走 HTTP**：`transition_work_order` 的 handler 在
/// `required_permission IS NULL` 時直接回 403（「系統驅動的動作不得經 API
/// 觸發」），那是刻意的 —— 沒有權限碼不等於任何人都能做。因此這裡照
/// 排程器的方式直接呼叫 SQL 函式。
#[tokio::test]
async fn auto_assign_is_not_a_human_response() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (id, _) = create_wo(ctx, &token, json!({ "priority": "HIGH" })).await;

    {
        let mut tx = ctx.owner_tx().await;
        // 排程器會先寫入負責人，再走轉移（handler 對人為動作也是這個順序）。
        sqlx::query("UPDATE fms.work_orders SET assignee_id = $2::uuid WHERE id = $1::uuid")
            .bind(&id)
            .bind(TECH_WANG)
            .execute(&mut *tx)
            .await
            .expect("設定負責人");
        sqlx::query(
            "SELECT fms.transition_work_order($1::uuid, 'AUTO_ASSIGN', NULL, NULL, '{}'::jsonb)",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("AUTO_ASSIGN");
        tx.commit().await.expect("commit");
    }

    let (state, responded, _) = wo_row(ctx, &id).await;
    assert_eq!(state.as_deref(), Some("ON_TRACK"), "前提：仍在量測中");
    assert!(
        !responded,
        "AUTO_ASSIGN 是系統動作，不該設 first_responded_at（032 之前會設）"
    );

    let mut tx = ctx.owner_tx().await;
    let actor_type: String = sqlx::query_scalar(
        "SELECT actor_type FROM fms.work_order_transitions
          WHERE work_order_id = $1::uuid AND action = 'AUTO_ASSIGN'",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀 transition");
    assert_eq!(
        actor_type, "SYSTEM",
        "side_effects.actor='SYSTEM' 應寫進 actor_type（032 之前吃 DEFAULT 'USER'）"
    );

    ctx.teardown().await;
}

// =============================================================================
// 那個恆真的 MET 判定（032 改動 5）
// =============================================================================

/// **本檔最重要的測試。**
///
/// 032 之前：`resolution_due_at IS NULL OR now() <= resolution_due_at`
/// → 左邊恆為真 → 完成即 `MET`。而 `resolution_due_at` 從來沒有東西會寫，
/// 所以「每一張完成的工單都達成」。
///
/// `LOW` 解析不到 policy，因此它是唯一能走到「沒有 due 卻完成」的路徑 ——
/// 也就是舊判定的那個分支。它必須是 `NOT_APPLICABLE`：
/// **沒在量不等於達成。**
#[tokio::test]
async fn completing_without_a_policy_is_not_applicable_not_met() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (id, wo) = create_wo(ctx, &token, json!({ "priority": "LOW" })).await;
    assert!(wo["resolution_due_at"].is_null(), "前提：沒有 due");

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "COMPLETE", "resolution_notes": "修好了" }),
    ] {
        let (status, body) = transition(ctx, &token, &id, action.clone()).await;
        assert_eq!(status, StatusCode::OK, "{action} 失敗：{body}");
    }

    let (sla_state, _, _) = wo_row(ctx, &id).await;
    assert_eq!(
        sla_state.as_deref(),
        Some("NOT_APPLICABLE"),
        "沒有 SLA 目標的工單完成後不該是 MET —— 那正是報表回 100% 的來源"
    );

    ctx.teardown().await;
}

/// 有目標且準時完成 → `MET`。
///
/// 這是反面：032 不能把「收斂假數字」做成「什麼都不算達成」。
#[tokio::test]
async fn completing_within_the_target_is_met() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (id, _) = create_wo(ctx, &token, json!({ "priority": "MEDIUM" })).await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "COMPLETE", "resolution_notes": "修好了" }),
    ] {
        let (status, body) = transition(ctx, &token, &id, action.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let (sla_state, _, _) = wo_row(ctx, &id).await;
    assert_eq!(sla_state.as_deref(), Some("MET"), "480 分內完成應是 MET");

    ctx.teardown().await;
}

/// 逾期完成 → `RESOLUTION_BREACHED`。
///
/// 測試不能等 480 分鐘，因此把 due 直接改到過去 —— 那正是狀態機看到的樣子。
#[tokio::test]
async fn completing_after_the_target_is_breached() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (id, _) = create_wo(ctx, &token, json!({ "priority": "MEDIUM" })).await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        let (status, body) = transition(ctx, &token, &id, action.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_orders
                SET resolution_due_at = clock_timestamp() - interval '1 hour'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("把 due 推到過去");
        tx.commit().await.expect("commit");
    }

    let (status, body) = transition(
        ctx,
        &token,
        &id,
        json!({ "action": "COMPLETE", "resolution_notes": "遲了" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (sla_state, _, _) = wo_row(ctx, &id).await;
    assert_eq!(
        sla_state.as_deref(),
        Some("RESOLUTION_BREACHED"),
        "逾期完成不是 MET"
    );

    ctx.teardown().await;
}

// =============================================================================
// 決定 E：重開是新的量測
// =============================================================================

/// 重開之後解決時鐘重新起算，而**前一輪的結果被保留在那筆轉移的 metadata 裡**。
///
/// 「第一次有沒有準時修好」與「重開後有沒有準時修好」是兩個事實。
/// 直接覆寫會讓兩者都看不見；另立一張表則是在既有答案（transitions 本來
/// 就是狀態機的歷史軌跡）旁邊再造一個。
#[tokio::test]
async fn reopening_starts_a_new_measurement_and_keeps_the_old_one() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (id, _) = create_wo(ctx, &token, json!({ "priority": "MEDIUM" })).await;

    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        let (status, body) = transition(ctx, &token, &id, action.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    // 讓第一輪逾期，這樣「前一輪的結果」是個有內容的事實而不是 MET。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_orders
                SET resolution_due_at = clock_timestamp() - interval '1 hour'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("推 due");
        tx.commit().await.expect("commit");
    }

    let (status, body) = transition(
        ctx,
        &token,
        &id,
        json!({ "action": "COMPLETE", "resolution_notes": "遲了" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (before, _, _) = wo_row(ctx, &id).await;
    assert_eq!(before.as_deref(), Some("RESOLUTION_BREACHED"), "前提");

    let (status, reopened) = transition(
        ctx,
        &token,
        &id,
        json!({ "action": "REOPEN", "reason": "還是有異音" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reopened}");

    let (sla_state, _, reopened_count) = wo_row(ctx, &id).await;
    assert_eq!(
        sla_state.as_deref(),
        Some("ON_TRACK"),
        "重開是新的量測，時鐘重新起算"
    );
    assert_eq!(reopened_count, 1);

    // 新的 due 必須在未來 —— 否則「重新起算」只是把狀態改回去而已。
    let resol: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(reopened["resolution_due_at"].clone()).expect("resolution_due_at");
    assert!(
        resol > chrono::Utc::now(),
        "重開後的 due 應在未來，實際 {resol}"
    );

    // 前一輪的事實被保留。
    let mut tx = ctx.owner_tx().await;
    let meta: Value = sqlx::query_scalar(
        "SELECT metadata FROM fms.work_order_transitions
          WHERE work_order_id = $1::uuid AND action = 'REOPEN'",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀 REOPEN 的 metadata");
    assert_eq!(
        meta["sla_cycle_closed"]["sla_state"], "RESOLUTION_BREACHED",
        "前一輪的達成與否必須保留下來，否則重開就是把逾期洗掉：{meta}"
    );

    // -----------------------------------------------------------------------
    // 044：重開之後不留下「已完成」的痕跡
    // -----------------------------------------------------------------------
    // 一個 `status = 'IN_PROGRESS'` 卻有 `completed_at` 的資料列會一直讓人
    // 寫出錯的查詢 —— 033 的第一版守衛（`completed_at IS NULL`）會把重開過的
    // 工單永久排除在逾期掃描之外，034 也得改綁狀態碼繞過它。
    //
    // **而清掉的前提是先保住。** 032 已經把 `completed_at` 放進上面那份
    // 快照，044 補上 `actual_end_at` —— 因此這裡兩件事一起斷言：
    // 欄位是空的，而值在 metadata 裡。
    assert!(
        !meta["sla_cycle_closed"]["completed_at"].is_null(),
        "前一輪的完成時刻要在快照裡：{meta}"
    );
    assert!(
        !meta["sla_cycle_closed"]["actual_end_at"].is_null(),
        "044 補上的：清掉 actual_end_at 之前必須先保住它：{meta}"
    );

    let (completed_cleared, ended_cleared): (bool, bool) = sqlx::query_as(
        "SELECT completed_at IS NULL, actual_end_at IS NULL
           FROM fms.work_orders WHERE id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀完成痕跡");
    assert!(
        completed_cleared,
        "重開之後工單正在進行中，不該還有 completed_at"
    );
    assert!(
        ended_cleared,
        "重開之後工作還沒結束，不該還有 actual_end_at"
    );

    // 反面：`actual_start_at` **保留**。`set_actual_start` 的
    // `coalesce(actual_start_at, ...)` 是刻意的 —— 它記的是「工作最早什麼
    // 時候開始」，而重開之後那個時刻仍然是真的。差別不在對稱，
    // 在於哪一句話還成立。
    let started: bool = sqlx::query_scalar(
        "SELECT actual_start_at IS NOT NULL FROM fms.work_orders WHERE id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀 actual_start_at");
    assert!(started, "actual_start_at 不該被清掉");

    assert!(
        !meta["sla_cycle_closed"]["resolution_due_at"].is_null(),
        "前一輪的目標時刻也要留，否則無法重算：{meta}"
    );

    ctx.teardown().await;
}
