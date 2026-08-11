//! SLA 政策的維護（`/sla-policies`、migration 037）。
//!
//! # 為什麼這一組測試存在
//!
//! 在這支端點之前，維護 SLA 政策的唯一方式是寫 migration —— 也就是把合約
//! 數字寫死在程式碼裡。後果具體可見：種子只覆蓋 `CRITICAL`／`HIGH`／
//! `MEDIUM`，於是 `LOW` 與 `URGENT` 的工單一律 `NOT_APPLICABLE`。
//!
//! 因此本檔的第一個測試就是**走完那個缺口的修補流程**：建立 `URGENT` 的
//! 政策，然後確認新開的 `URGENT` 工單真的有了目標時刻。那是這支端點
//! 存在的全部理由。
//!
//! 其餘的測試分兩類：
//!   * **範圍**：租戶通用政策需要 TENANT 範圍；搬移場域要兩端都有權限
//!   * **目錄不得含糊**：同一組 (場域, 優先度) 只能有一個生效的政策

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const FACILITY_CINEMA: &str = "cccccccc-0000-4000-8000-000000000002";
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";

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

async fn create_policy(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request("POST", "/api/v1/sla-policies", body),
        token,
    ))
    .await
}

async fn patch_policy(
    ctx: &TestContext,
    token: &str,
    id: &str,
    body: Value,
) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request("PATCH", &format!("/api/v1/sla-policies/{id}"), body),
        token,
    ))
    .await
}

// =============================================================================
// 這支端點存在的理由
// =============================================================================

/// **本檔最重要的測試**：補上 `URGENT` 的政策，然後 `URGENT` 的工單就有了
/// SLA 目標 —— 全程沒有寫任何 migration。
///
/// `URGENT` 在種子裡沒有政策，因此它的工單一律 `NOT_APPLICABLE`：不進報表
/// 分母、不被掃描、不會升級。名字比 `HIGH` 急，卻沒有任何時限。
#[tokio::test]
async fn defining_a_policy_closes_the_urgent_gap() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // ---- 前：URGENT 沒有目標 ----
    let (status, before) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "補政策之前的 URGENT 工單",
                    "priority": "URGENT"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{before}");
    assert_eq!(
        before["sla_state"], "NOT_APPLICABLE",
        "前提：URGENT 沒有政策，因此沒在量：{before}"
    );
    assert!(before["resolution_due_at"].is_null(), "{before}");

    // ---- 管理者定義政策 ----
    let (status, policy) = create_policy(
        ctx,
        &token,
        json!({
            "code": "SLA_URGENT",
            "name": "緊急",
            "applies_to_priority": "URGENT",
            "response_minutes": 15,
            "resolution_minutes": 90,
            "business_hours_only": false,
            "escalation_rules": [{ "at_pct": 60 }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");

    // ---- 後：新開的 URGENT 工單有目標了 ----
    let (status, after) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "補政策之後的 URGENT 工單",
                    "priority": "URGENT"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{after}");
    assert_eq!(after["sla_state"], "ON_TRACK", "{after}");

    let created: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(after["created_at"].clone()).expect("created_at");
    let resol: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(after["resolution_due_at"].clone())
            .unwrap_or_else(|_| panic!("應有 resolution_due_at：{after}"));
    assert_eq!((resol - created).num_minutes(), 90, "{after}");

    // ---- 而先前那張工單**沒有**被追溯套用 ----
    // 這是決定 F 的快照語意：合約報表不能因為今天補了政策而回溯改變。
    let id = before["id"].as_str().expect("id");
    let (_, refetched) = ctx
        .send(authed(get(&format!("/api/v1/work-orders/{id}")), &token))
        .await;
    assert_eq!(
        refetched["sla_state"], "NOT_APPLICABLE",
        "已開立的工單不該被追溯套用新政策：{refetched}"
    );

    ctx.teardown().await;
}

/// 改分鐘數不影響已經開立的工單。
///
/// `response_due_at`／`resolution_due_at` 在開單時就算成絕對時刻。這不是
/// 疏漏，是決定 F：今天調政策不該讓上個月的達成率跟著變。
#[tokio::test]
async fn changing_the_minutes_does_not_move_existing_targets() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "快照測試",
                    "priority": "CRITICAL"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    let before_due = wo["resolution_due_at"].clone();
    assert!(!before_due.is_null());

    // 找到 SLA_CRITICAL 並把解決時限從 120 改成 600。
    let (_, list) = ctx.send(authed(get("/api/v1/sla-policies"), &token)).await;
    let id = list["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|p| p["code"] == "SLA_CRITICAL")
        .and_then(|p| p["id"].as_str())
        .expect("種子應有 SLA_CRITICAL");

    let (status, patched) =
        patch_policy(ctx, &token, id, json!({ "resolution_minutes": 600 })).await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["resolution_minutes"], 600);

    let wo_id = wo["id"].as_str().expect("id");
    let (_, refetched) = ctx
        .send(authed(get(&format!("/api/v1/work-orders/{wo_id}")), &token))
        .await;
    assert_eq!(
        refetched["resolution_due_at"], before_due,
        "已開立工單的目標時刻是快照，不該被政策變更移動：{refetched}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 範圍
// =============================================================================

/// 場域管理員可以建自己場域的政策，但**建不了租戶通用的**。
///
/// 這是 037 兩個範圍規則的核心。`sla_policy:write` 宣告 `FACILITY`（否則
/// 「場域政策優先」那條設計沒有人走得到），而租戶通用的政策套用到每一個
/// 場域，因此額外要求 TENANT 範圍。
#[tokio::test]
async fn a_facility_admin_cannot_create_a_tenant_wide_policy() {
    let ctx = &TestContext::setup().await;
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    // 自己的場域 → 可以
    let (status, own) = create_policy(
        ctx,
        &fm,
        json!({
            "facility_id": FACILITY_HQ,
            "code": "SLA_HQ_LOW",
            "name": "總部低優先",
            "applies_to_priority": "LOW",
            "response_minutes": 240,
            "resolution_minutes": 2880
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "場域管理員應能建自己場域的政策：{own}"
    );

    // 租戶通用 → 不行
    let (status, wide) = create_policy(
        ctx,
        &fm,
        json!({
            "code": "SLA_ALL_LOW",
            "name": "全租戶低優先",
            "applies_to_priority": "LOW",
            "response_minutes": 240,
            "resolution_minutes": 2880
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "租戶通用政策影響每一個場域，需要 TENANT 範圍：{wide}"
    );
    assert_eq!(wide["code"], "PERMISSION_DENIED");

    // 別人的場域 → 也不行
    let (status, other) = create_policy(
        ctx,
        &fm,
        json!({
            "facility_id": FACILITY_CINEMA,
            "code": "SLA_CINEMA_LOW",
            "name": "影廳低優先",
            "applies_to_priority": "LOW",
            "response_minutes": 240,
            "resolution_minutes": 2880
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{other}");

    ctx.teardown().await;
}

/// 把場域政策改成 `facility_id: null` 是**一次 PATCH 完成的權限放大**。
///
/// 場域管理員對自己的政策有寫入權，但那不該讓他能把它變成套用全租戶的政策。
/// 因此 PATCH 對新舊兩端都檢查。
///
/// 這一格也是 `Option<Option<T>>` 存在的理由：若 DTO 用單層 `Option`，
/// 「沒有提供 facility_id」與「明確設為 null」在型別上分不出來，
/// 於是這個檢查也就無從觸發。
#[tokio::test]
async fn widening_a_policy_to_the_whole_tenant_needs_tenant_scope() {
    let ctx = &TestContext::setup().await;
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;

    let (status, own) = create_policy(
        ctx,
        &fm,
        json!({
            "facility_id": FACILITY_HQ,
            "code": "SLA_HQ_URGENT",
            "name": "總部緊急",
            "applies_to_priority": "URGENT",
            "response_minutes": 10,
            "resolution_minutes": 60
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{own}");
    let id = own["id"].as_str().expect("id");

    // 改名字（不動範圍）→ 可以
    let (status, renamed) = patch_policy(ctx, &fm, id, json!({ "name": "總部緊急（改名）" })).await;
    assert_eq!(status, StatusCode::OK, "{renamed}");

    // 改成租戶通用 → 不行
    let (status, widened) = patch_policy(ctx, &fm, id, json!({ "facility_id": null })).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "把場域政策放大到全租戶需要 TENANT 範圍：{widened}"
    );

    // 搬到別人的場域 → 也不行
    let (status, moved) =
        patch_policy(ctx, &fm, id, json!({ "facility_id": FACILITY_CINEMA })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{moved}");

    // 而租戶管理員兩者都可以。
    let admin = ctx.login().await;
    let (status, widened) = patch_policy(ctx, &admin, id, json!({ "facility_id": null })).await;
    assert_eq!(status, StatusCode::OK, "{widened}");
    assert!(widened["facility_id"].is_null(), "{widened}");

    ctx.teardown().await;
}

// =============================================================================
// 目錄不得含糊
// =============================================================================

/// 同一組 `(場域, 優先度)` 只能有一個生效的政策。
///
/// 沒有這個約束，`resolve_sla_policy` 的 `code` 決勝會讓第二個政策
/// **靜默沒有作用** —— 管理者建了 `SLA_CRITICAL_V2` 卻發現時限沒變，
/// 而系統不會給任何提示。037 把它變成寫入時的 409。
#[tokio::test]
async fn a_duplicate_scope_is_rejected_instead_of_silently_losing() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 種子已有 SLA_CRITICAL（租戶通用 + CRITICAL）。
    let (status, dup) = create_policy(
        ctx,
        &token,
        json!({
            "code": "SLA_CRITICAL_V2",
            "name": "緊急（第二版）",
            "applies_to_priority": "CRITICAL",
            "response_minutes": 5,
            "resolution_minutes": 30
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "重複的範圍應回 409，而不是建立一個永遠不會生效的政策：{dup}"
    );
    assert!(
        dup["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("is_active"),
        "訊息要說出可行動的修法（先停用舊的）：{dup}"
    );

    // 停用舊的之後就可以建了 —— 約束只管 is_active 的列。
    let (_, list) = ctx.send(authed(get("/api/v1/sla-policies"), &token)).await;
    let old = list["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|p| p["code"] == "SLA_CRITICAL")
        .and_then(|p| p["id"].as_str())
        .expect("SLA_CRITICAL");
    let (status, _) = patch_policy(ctx, &token, old, json!({ "is_active": false })).await;
    assert_eq!(status, StatusCode::OK);

    let (status, now_ok) = create_policy(
        ctx,
        &token,
        json!({
            "code": "SLA_CRITICAL_V2",
            "name": "緊急（第二版）",
            "applies_to_priority": "CRITICAL",
            "response_minutes": 5,
            "resolution_minutes": 30
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{now_ok}");

    // 停用的政策不出現在預設清單裡，但查得到 —— 已開立的工單快照了它的 id。
    let (_, default_list) = ctx.send(authed(get("/api/v1/sla-policies"), &token)).await;
    let codes: Vec<&str> = default_list["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|p| p["code"].as_str())
        .collect();
    assert!(!codes.contains(&"SLA_CRITICAL"), "{default_list}");

    let (_, all) = ctx
        .send(authed(
            get("/api/v1/sla-policies?include_inactive=true"),
            &token,
        ))
        .await;
    let codes: Vec<&str> = all["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|p| p["code"].as_str())
        .collect();
    assert!(codes.contains(&"SLA_CRITICAL"), "停用的政策要查得到：{all}");

    ctx.teardown().await;
}

/// 兩個「租戶通用 + 所有優先度」的政策也是重複 —— 而那是最含糊的一種。
///
/// 這一格驗的是索引的 `NULLS NOT DISTINCT`。預設 NULL 互不相等，
/// 少了那個關鍵字，這兩筆都會被放行，而它們**互相**都可能勝出。
#[tokio::test]
async fn two_catch_all_policies_are_also_a_duplicate() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let base = json!({
        "name": "全部適用",
        "response_minutes": 30,
        "resolution_minutes": 240
    });
    let mut first = base.clone();
    first["code"] = json!("SLA_CATCH_ALL");
    let (status, a) = create_policy(ctx, &token, first).await;
    assert_eq!(status, StatusCode::CREATED, "{a}");

    let mut second = base.clone();
    second["code"] = json!("SLA_CATCH_ALL_2");
    let (status, b) = create_policy(ctx, &token, second).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "兩個「通用 + 通用」的政策互相都可能勝出 —— NULLS NOT DISTINCT 擋的正是這個：{b}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 驗證與錯誤形狀
// =============================================================================

/// 壞掉的 `escalation_rules` 回 422，不是 500。
///
/// 036 的 CHECK 是 `23514`，而 `fms-shared` 的通用映射對不認識的 `23514`
/// 會落到 `Problem::internal` → 500。一個打錯字的 `at_pct` 不該是伺服器錯誤。
#[tokio::test]
async fn a_malformed_escalation_rule_is_a_validation_error_not_a_500() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    for bad in [
        json!([{ "at_pct": "80%" }]),
        json!([{ "at_pct": 150 }]),
        json!([{ "notify": ["FM"] }]),
        json!({ "at_pct": 80 }),
    ] {
        let (status, body) = create_policy(
            ctx,
            &token,
            json!({
                "code": "SLA_BAD",
                "name": "壞規則",
                "applies_to_priority": "LOW",
                "response_minutes": 30,
                "resolution_minutes": 240,
                "escalation_rules": bad
            }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad} 應回 422：{body}"
        );
        assert_eq!(body["errors"][0]["pointer"], "/escalation_rules", "{body}");
    }

    ctx.teardown().await;
}

/// 分鐘數與優先度的白名單。
#[tokio::test]
async fn invalid_field_values_are_rejected_with_pointers() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let cases = [
        (json!({ "response_minutes": 0 }), "/response_minutes"),
        (json!({ "resolution_minutes": -5 }), "/resolution_minutes"),
        (
            json!({ "applies_to_priority": "SUPER_URGENT" }),
            "/applies_to_priority",
        ),
    ];
    for (patch, pointer) in cases {
        let mut body = json!({
            "code": "SLA_X",
            "name": "測試",
            "response_minutes": 30,
            "resolution_minutes": 240
        });
        for (k, v) in patch.as_object().expect("物件") {
            body[k] = v.clone();
        }
        let (status, resp) = create_policy(ctx, &token, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{resp}");
        assert_eq!(resp["errors"][0]["pointer"], pointer, "{resp}");
    }

    ctx.teardown().await;
}

/// 代碼重複與範圍重複是**不同的** 409。
///
/// 兩者都是 `23505`，而通用映射給的是同一句「a conflicting record already
/// exists」。管理者需要知道是「這個代碼被用了」還是「這一組已經有政策了」——
/// 後者的修法完全不同（停用舊的）。
#[tokio::test]
async fn code_conflicts_and_scope_conflicts_say_different_things() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 代碼重複：用種子的 SLA_CLEANING，但範圍不衝突（綁 LOW）。
    let (status, code_dup) = create_policy(
        ctx,
        &token,
        json!({
            "code": "sla_cleaning",
            "name": "大小寫不同但仍重複",
            "applies_to_priority": "LOW",
            "response_minutes": 30,
            "resolution_minutes": 240
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{code_dup}");
    let detail = code_dup["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("代碼"), "應指出是代碼重複：{code_dup}");

    // 範圍重複：不同代碼，但撞到種子的 SLA_CLEANING（通用 + HIGH）。
    let (status, scope_dup) = create_policy(
        ctx,
        &token,
        json!({
            "code": "SLA_SOMETHING_ELSE",
            "name": "範圍撞到了",
            "applies_to_priority": "HIGH",
            "response_minutes": 30,
            "resolution_minutes": 240
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{scope_dup}");
    let detail = scope_dup["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("is_active"),
        "應指出修法是停用舊的：{scope_dup}"
    );

    ctx.teardown().await;
}

/// **ORG 管理員可以自己訂 LOW／URGENT 的分鐘數**（migration 048）。
///
/// 那些數字是合約條款，不是技術參數 —— 「LOW 幾分鐘內要回應」取決於那個客戶
/// 簽了什麼。037 因此刻意讓 `resolve_sla_policy` 沒有預設 fallback，
/// 而 048 補上缺的那一半：`ORG_MANAGER` 原本只有 `sla_policy:read`。
///
/// 三格一起驗，因為「能訂」跟「只能訂自己的」是同一件事的兩面：
///   1. 自己組織子樹底下的場域 → 可以
///   2. 租戶通用（`facility_id: null`）→ 不行（那會蓋到別的事業部）
///   3. 別的事業部的場域 → 不行
///
/// 少了 2 與 3，把 `require_scope` 整個拿掉也會讓 1 通過。
#[tokio::test]
async fn an_org_manager_sets_the_minutes_for_its_own_subtree_only() {
    let ctx = &TestContext::setup().await;

    // 種子裡沒有 ORG_MANAGER，所以自己造一個 —— 範圍綁在總部所屬的
    // 「不動產事業部」。信義影城屬於另一個事業部，正好是第 3 格的對照。
    {
        let mut tx = ctx.owner_tx().await;
        let hash = fms_identity::password::hash(TEST_PASSWORD).expect("hash");
        sqlx::query(
            "INSERT INTO fms.users
               (tenant_id, username, display_name, email, status, password_hash)
             VALUES ($1::uuid, 'org.mgr', '事業部經理', 'org.mgr@example.test',
                     'ACTIVE', $2)",
        )
        .bind(TENANT_ID)
        .bind(&hash)
        .execute(&mut *tx)
        .await
        .expect("建 ORG 經理");

        sqlx::query(
            "INSERT INTO fms.user_role_assignments
               (tenant_id, user_id, role_id, scope_type, scope_id, source)
             SELECT $1::uuid, u.id, r.id, 'ORG', f.org_id, 'MANUAL'
               FROM fms.users u, fms.roles r, fms.facilities f
              WHERE u.username = 'org.mgr' AND r.code = 'ORG_MANAGER'
                AND f.id = $2::uuid",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_HQ)
        .execute(&mut *tx)
        .await
        .expect("指派 ORG 範圍");
        tx.commit().await.expect("commit");
    }

    let mgr = ctx.login_as("org.mgr").await;

    // 1. 自己子樹底下的場域 —— 這是 048 的目的。
    let (status, own) = create_policy(
        ctx,
        &mgr,
        json!({
            "facility_id": FACILITY_HQ,
            "code": "SLA_HQ_URGENT_ORG",
            "name": "總部緊急（事業部訂）",
            "applies_to_priority": "URGENT",
            "response_minutes": 15,
            "resolution_minutes": 120
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "048 之後 ORG 經理應能訂自己子樹的政策：{own}"
    );

    // 2. 租戶通用 → 不行。ORG 範圍不是 TENANT 範圍。
    let (status, wide) = create_policy(
        ctx,
        &mgr,
        json!({
            "code": "SLA_ALL_URGENT_ORG",
            "name": "全租戶緊急（越權）",
            "applies_to_priority": "URGENT",
            "response_minutes": 5,
            "resolution_minutes": 30
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "租戶通用政策會蓋到別的事業部，需要 TENANT 範圍：{wide}"
    );

    // 3. 別的事業部的場域 → 不行。
    let (status, other) = create_policy(
        ctx,
        &mgr,
        json!({
            "facility_id": FACILITY_CINEMA,
            "code": "SLA_CINEMA_URGENT_ORG",
            "name": "影廳緊急（越權）",
            "applies_to_priority": "URGENT",
            "response_minutes": 5,
            "resolution_minutes": 30
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "影城事業部不在他的子樹裡：{other}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 資料庫層的縱深防禦（migration 050）
// =============================================================================

/// **RLS 自己就擋得住租戶通用列的寫入**，不靠 API 層的 `require_scope`。
///
/// 037 為 `sla_policies` 建的 `facility_scope` 政策沒有明寫 `WITH CHECK`，
/// 於是 PostgreSQL 對 `cmd = ALL` 的政策會拿 `USING` 當寫入檢查 —— 而那是
/// `facility_in_scope()`，對 NULL 一律放行。結果：**場域受限的連線可以建立
/// 套用到整個租戶的政策。**
///
/// 那個缺口先前沒有實際曝露（API 的 `require_scope` 擋住了），但這個 schema
/// 的原則是「RLS 要能自己站住 —— 一次 SQL injection 不該足以放大範圍」
/// （007／013／046）。因此這一組測試**繞過 HTTP 層**，直接用場域受限的
/// 交易情境操作，驗的是政策本身。
///
/// 上面 `a_facility_admin_cannot_create_a_tenant_wide_policy` 驗的是 API 層；
/// 兩者都要有，因為它們會各自獨立地壞掉。
#[tokio::test]
async fn rls_alone_blocks_a_facility_writer_from_tenant_wide_policies() {
    let ctx = &TestContext::setup().await;

    // `tenant_tx_as` 會照 begin_tenant_tx 的方式設好 app.facility_ids ——
    // 直接 set_context 不會設，那樣 facility_scope 政策全部惰性（見 helper 說明）。
    // **tx 必須在 teardown 之前釋放。** `ctx.teardown()` 會 DROP DATABASE，
    // 而那會卡在任何還開著的連線上 —— 症狀是測試整個掛住，不是失敗。
    // 因此把交易收進一個 block（本檔其他測試與 audit_trail_slice 都是這個寫法）。
    let (own, wide) = {
        let mut tx = ctx.tenant_tx_as(USERNAME_FACILITY_ADMIN).await;

        // 1. 自己場域的 → 可以。少了這一格，「政策收成永遠 false」也會讓後面通過。
        let own = sqlx::query(
            "INSERT INTO fms.sla_policies
           (tenant_id, facility_id, code, name, applies_to_priority,
            response_minutes, resolution_minutes, is_active)
         VALUES (fms.current_tenant_id(), $1::uuid, 'RLS_OWN', '自己場域',
                 'URGENT', 15, 120, true)",
        )
        .bind(FACILITY_HQ)
        .execute(tx.conn())
        .await;

        // 2. 租戶通用的 → RLS 要拒絕。
        let wide = sqlx::query(
            "INSERT INTO fms.sla_policies
           (tenant_id, facility_id, code, name, applies_to_priority,
            response_minutes, resolution_minutes, is_active)
         VALUES (fms.current_tenant_id(), NULL, 'RLS_WIDE', '全租戶',
                 'URGENT', 5, 30, true)",
        )
        .execute(tx.conn())
        .await;
        (own, wide)
    };

    assert!(own.is_ok(), "自己場域的政策該寫得進去：{own:?}");
    let err = wide
        .expect_err("RLS 必須拒絕場域受限者建立租戶通用政策")
        .to_string();
    assert!(
        err.contains("row-level security"),
        "應該是 RLS 擋下的，而不是別的錯：{err}"
    );

    ctx.teardown().await;
}

/// 場域受限者**不能修改或刪除**既有的租戶通用列，但**看得到**它們。
///
/// 這一段比我最初報的缺口更大。原本只說了 INSERT，但同一個不變量涵蓋四件事：
/// 建立、放大（UPDATE 成 NULL）、修改既有的 NULL 列、刪除 NULL 列。
/// 只修 INSERT 會留下「你建不了我們的租戶政策，但你刪得掉」。
///
/// **讀必須維持放行**：`resolve_sla_policy` 的 fallback 就是靠場域看得到套用
/// 在自己身上的租戶通用政策。把讀一起收緊，症狀會是 SLA 目標忽然變成
/// `NOT_APPLICABLE` —— 看起來像 SLA 的 bug，離這裡很遠。
///
/// 注意 UPDATE／DELETE 被 `USING` 擋下來是**靜默的**（0 列，不報錯）——
/// 那是 PostgreSQL 對 RLS 的語意。因此這裡數的是受影響列數，不是抓例外。
#[tokio::test]
async fn a_facility_writer_can_read_but_not_touch_tenant_wide_rows() {
    let ctx = &TestContext::setup().await;

    // 種一筆租戶通用政策（平台情境，代表租戶管理員先前建的）。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.sla_policies
               (tenant_id, facility_id, code, name, applies_to_priority,
                response_minutes, resolution_minutes, is_active)
             VALUES ($1::uuid, NULL, 'RLS_SEEDED_WIDE', '租戶管理員訂的',
                     'URGENT', 5, 30, true)",
        )
        .bind(TENANT_ID)
        .execute(&mut *tx)
        .await
        .expect("種租戶通用政策");
        tx.commit().await.expect("commit");
    }

    // tx 收進 block —— teardown 的 DROP DATABASE 會卡在開著的連線上。
    let (visible, updated, deleted) = {
        let mut tx = ctx.tenant_tx_as(USERNAME_FACILITY_ADMIN).await;

        // 讀：看得到。
        let visible: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM fms.sla_policies WHERE code = 'RLS_SEEDED_WIDE'",
        )
        .fetch_one(tx.conn())
        .await
        .expect("讀");

        // 改：0 列。
        let updated: i64 = sqlx::query_scalar(
            "WITH u AS (UPDATE fms.sla_policies SET response_minutes = 1
                     WHERE code = 'RLS_SEEDED_WIDE' RETURNING 1)
         SELECT count(*) FROM u",
        )
        .fetch_one(tx.conn())
        .await
        .expect("update");

        // 刪：0 列。
        let deleted: i64 = sqlx::query_scalar(
            "WITH d AS (DELETE FROM fms.sla_policies
                     WHERE code = 'RLS_SEEDED_WIDE' RETURNING 1)
         SELECT count(*) FROM d",
        )
        .fetch_one(tx.conn())
        .await
        .expect("delete");
        (visible, updated, deleted)
    };

    assert_eq!(
        visible, 1,
        "場域管理員必須看得到套用在自己身上的租戶通用政策 —— \
         resolve_sla_policy 的 fallback 靠這個"
    );
    assert_eq!(updated, 0, "不該改得動租戶通用政策");
    assert_eq!(deleted, 0, "更不該刪得掉租戶通用政策");

    ctx.teardown().await;
}

/// 反面：**租戶範圍的人仍然動得了。**
///
/// 這一格是整個 050 的安全帶。`current_facility_ids()` 在 `begin_tenant_tx`
/// 底下**永遠不是 NULL**（`set_facility_scope` 一律寫入具體清單，空清單也寫
/// 全零哨兵），所以「用 `current_facility_ids() IS NULL` 代表不受限」那種寫法
/// 會把租戶管理員一起擋掉 —— 而種子裡已經有 3 筆租戶通用政策。
///
/// 少了這一格，把述詞寫成恆假也會讓上面兩個測試通過。
#[tokio::test]
async fn a_tenant_scoped_writer_still_manages_tenant_wide_rows() {
    let ctx = &TestContext::setup().await;

    // tx 收進 block —— teardown 的 DROP DATABASE 會卡在開著的連線上。
    // admin.chen 是 TENANT 範圍 —— 他拿到的是全部場域的清單，不是 NULL。
    let (inserted, updated, deleted) = {
        let mut tx = ctx.tenant_tx_as(USERNAME).await;

        let inserted = sqlx::query(
            "INSERT INTO fms.sla_policies
           (tenant_id, facility_id, code, name, applies_to_priority,
            response_minutes, resolution_minutes, is_active)
         VALUES (fms.current_tenant_id(), NULL, 'RLS_TENANT_OK', '租戶通用',
                 'URGENT', 5, 30, true)",
        )
        .execute(tx.conn())
        .await;

        let updated: i64 = sqlx::query_scalar(
            "WITH u AS (UPDATE fms.sla_policies SET response_minutes = 7
                     WHERE code = 'RLS_TENANT_OK' RETURNING 1)
         SELECT count(*) FROM u",
        )
        .fetch_one(tx.conn())
        .await
        .expect("update");

        let deleted: i64 = sqlx::query_scalar(
            "WITH d AS (DELETE FROM fms.sla_policies
                     WHERE code = 'RLS_TENANT_OK' RETURNING 1)
         SELECT count(*) FROM d",
        )
        .fetch_one(tx.conn())
        .await
        .expect("delete");
        (inserted, updated, deleted)
    };

    assert!(
        inserted.is_ok(),
        "TENANT 範圍的人必須建得出租戶通用政策：{inserted:?}"
    );
    assert_eq!(updated, 1, "TENANT 範圍的人必須改得動");
    assert_eq!(deleted, 1, "TENANT 範圍的人必須刪得掉");

    ctx.teardown().await;
}
