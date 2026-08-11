//! 技能與證照（`/skills`、`/users/{id}/skills`）。
//!
//! # 這一組守的是「到期狀態是算出來的」
//!
//! `status` 由 `expires_at` 與**資料庫的今天**比較得出，不是存的欄位。
//! 存下來的話它會在沒有人更新的那一天開始說謊 —— 而證照過期正是那種
//! 沒有人會主動去更新的事實。
//!
//! `b_` 用四張到期日不同的證照一次驗四種狀態，並且驗門檻真的可調。
//!
//! # 這一輪**沒有**做到期提醒
//!
//! ENDPOINTS.md 寫「技能與證照（含到期提醒）」。這裡只做到「查得到現在的
//! 狀態」；沒有任何東西會主動告訴管理員下個月有幾張證照到期。
//! `e_` 把那個缺口釘成一格會說話的測試，而不是留一句註解。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// migration 055 的平台技能。
const SKILL_ELECTRICAL: &str = "50000000-0000-4000-8000-000000000001"; // 需證照
const SKILL_HVAC: &str = "50000000-0000-4000-8000-000000000006"; // 不需證照
const USER_TECH_HQ: &str = "ffffffff-0000-4000-8000-000000000006"; // tech.liu

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

async fn put_skill(
    ctx: &TestContext,
    token: &str,
    skill: &str,
    body: Value,
) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request(
            "PUT",
            &format!("/api/v1/users/{USER_TECH_HQ}/skills/{skill}"),
            body,
        ),
        token,
    ))
    .await
}

/// 目錄查得到，而且平台技能對租戶可見。
///
/// **這一格會抓到「055 沒跑」**：在它之前這張表是 0 列，
/// 而一支回空清單的端點看起來跟正常的一樣。
#[tokio::test]
async fn a_the_platform_catalogue_is_visible_and_filterable() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ctx.send(authed(get("/api/v1/skills"), &admin)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().cloned().unwrap_or_default();
    assert!(
        items.len() >= 9,
        "平台技能目錄至少 9 項（migration 055）—— 拿到 {}：{body}",
        items.len()
    );
    assert!(
        items.iter().all(|s| s["tenant_id"].is_null()),
        "目前只有平台技能，全部的 tenant_id 應為 null"
    );

    let (_, certed) = ctx
        .send(authed(
            get("/api/v1/skills?requires_certification=true"),
            &admin,
        ))
        .await;
    let n_cert = certed["items"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(
        (1..items.len()).contains(&n_cert),
        "requires_certification 要有判別力（{n_cert} / {}）",
        items.len()
    );

    let (_, mep) = ctx
        .send(authed(get("/api/v1/skills?domain=mep"), &admin))
        .await;
    assert!(
        mep["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["domain"] == "MEP"),
        "domain 過濾不分大小寫且要真的過濾：{mep}"
    );

    ctx.teardown().await;
}

/// **到期狀態是算出來的。** 四種狀態一次驗，並且驗門檻可調。
#[tokio::test]
async fn b_expiry_status_is_derived_from_the_date_not_stored() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 需要證照的技能：三張不同到期日。
    let (status, expired) = put_skill(
        ctx,
        &admin,
        SKILL_ELECTRICAL,
        json!({ "level": 3, "expires_at": "2020-01-01", "certificate_no": "E-OLD" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{expired}");
    assert_eq!(
        expired["status"], "EXPIRED",
        "2020 年的證照早就過期：{expired}"
    );
    assert!(
        expired["days_until_expiry"].as_i64().unwrap() < 0,
        "過期的天數要是負的：{expired}"
    );

    // 不需要證照的技能：沒有到期日 → NOT_APPLICABLE。
    let (status, na) = put_skill(ctx, &admin, SKILL_HVAC, json!({ "level": 2 })).await;
    assert_eq!(status, StatusCode::OK, "{na}");
    assert_eq!(na["status"], "NOT_APPLICABLE");
    assert!(na["days_until_expiry"].is_null());

    // 20 天後到期：預設門檻 30 天內算 EXPIRING，改成 7 天就該是 VALID。
    let soon = (chrono::Utc::now() + chrono::Duration::days(20))
        .format("%Y-%m-%d")
        .to_string();
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.user_skills SET expires_at = $3::date
          WHERE user_id = $1::uuid AND skill_id = $2::uuid",
    )
    .bind(USER_TECH_HQ)
    .bind(SKILL_ELECTRICAL)
    .bind(&soon)
    .execute(&mut *tx)
    .await
    .expect("改到期日");
    tx.commit().await.expect("commit");

    let (_, default_window) = ctx
        .send(authed(
            get(&format!("/api/v1/users/{USER_TECH_HQ}/skills")),
            &admin,
        ))
        .await;
    let e = default_window["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["skill_code"] == "ELECTRICAL")
        .cloned()
        .expect("電氣那一列");
    assert_eq!(e["status"], "EXPIRING", "20 天後到期，預設 30 天窗內：{e}");

    let (_, narrow) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/users/{USER_TECH_HQ}/skills?expiring_within_days=7"
            )),
            &admin,
        ))
        .await;
    let e = narrow["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["skill_code"] == "ELECTRICAL")
        .cloned()
        .expect("電氣那一列");
    assert_eq!(
        e["status"], "VALID",
        "門檻是呼叫端的條件，改成 7 天之後 20 天後到期就不算 EXPIRING：{e}"
    );
    assert_eq!(
        narrow["meta"]["expiring_within_days"], 7,
        "回應要說出用了哪個門檻"
    );

    ctx.teardown().await;
}

/// 需要證照的技能一定要有到期日。
///
/// 而「需要證照卻沒有到期日」被算成 **EXPIRED 而不是 NOT_APPLICABLE** ——
/// 那是資料缺漏，不是「不適用」。混成同一個值的話，缺漏永遠不會有人發現。
#[tokio::test]
async fn c_a_certified_skill_without_an_expiry_is_refused_and_never_reads_as_fine() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = put_skill(ctx, &admin, SKILL_ELECTRICAL, json!({ "level": 3 })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("expires_at"),
        "訊息要說得出缺什麼：{body}"
    );

    // 繞過端點直接塞一列缺到期日的證照（模擬歷史資料或目錄同步匯入）。
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.user_skills (user_id, skill_id, tenant_id, level)
         VALUES ($1::uuid, $2::uuid, $3::uuid, 3)",
    )
    .bind(USER_TECH_HQ)
    .bind(SKILL_ELECTRICAL)
    .bind(TENANT_ID)
    .execute(&mut *tx)
    .await
    .expect("塞缺漏資料");
    tx.commit().await.expect("commit");

    let (_, listed) = ctx
        .send(authed(
            get(&format!("/api/v1/users/{USER_TECH_HQ}/skills")),
            &admin,
        ))
        .await;
    let e = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["skill_code"] == "ELECTRICAL")
        .cloned()
        .expect("電氣那一列");
    assert_eq!(
        e["status"], "EXPIRED",
        "需要證照卻沒有到期日是**資料缺漏**，不是「不適用」—— \
         算成 NOT_APPLICABLE 的話那個缺漏永遠不會有人發現：{e}"
    );

    ctx.teardown().await;
}

/// upsert 是真的 upsert，而權限分讀寫。
#[tokio::test]
async fn d_upsert_replaces_and_permissions_split_read_from_write() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    put_skill(
        ctx,
        &admin,
        SKILL_ELECTRICAL,
        json!({ "level": 1, "expires_at": "2030-01-01", "certificate_no": "A" }),
    )
    .await;
    let (_, second) = put_skill(
        ctx,
        &admin,
        SKILL_ELECTRICAL,
        json!({ "level": 5, "expires_at": "2031-06-30", "certificate_no": "B" }),
    )
    .await;
    assert_eq!(second["level"], 5);
    assert_eq!(second["certificate_no"], "B");

    let (_, listed) = ctx
        .send(authed(
            get(&format!("/api/v1/users/{USER_TECH_HQ}/skills")),
            &admin,
        ))
        .await;
    let n = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["skill_code"] == "ELECTRICAL")
        .count();
    assert_eq!(
        n, 1,
        "主鍵是 (user_id, skill_id)，upsert 不該產生第二列：{listed}"
    );

    // VIEWER 有 team:read 但沒有 team:write。
    let viewer = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, denied) = put_skill(ctx, &viewer, SKILL_HVAC, json!({ "level": 1 })).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "REQUESTER 沒有 team:write：{denied}"
    );

    let (status, bad_level) = put_skill(ctx, &admin, SKILL_HVAC, json!({ "level": 9 })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad_level}");

    ctx.teardown().await;
}

/// **這一格是一個沒有做完的功能的記號，不是一個通過的測試。**
///
/// ENDPOINTS.md 寫「技能與證照（含到期提醒）」。提醒的部分沒有做：
/// 沒有掃描迴圈、沒有通知範本，也沒有任何東西會主動告訴管理員
/// 「下個月有三張證照到期」。
///
/// 004 建了 `idx_user_skills_expiring`（`(tenant_id, expires_at)` 部分索引）
/// 就是為那件事 —— 本模組的查詢都是**單一使用者**，走不到它。
///
/// 這一格斷言那個索引存在且仍然只服務未來的掃描。
/// 等提醒真的做出來時，這一格會變成「它現在有讀者了」的提示。
#[tokio::test]
async fn e_the_expiry_reminder_is_not_built_and_its_index_has_no_reader() {
    let ctx = &TestContext::setup().await;

    let mut tx = ctx.owner_tx().await;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes
                         WHERE schemaname = 'fms' AND indexname = 'idx_user_skills_expiring')",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("查索引");
    tx.commit().await.expect("commit");

    assert!(
        exists,
        "idx_user_skills_expiring 不見了 —— 它是到期掃描要用的，\
         現在還沒有讀者，但拿掉它等於把那件事的準備工作也刪了"
    );

    // 這裡刻意**不**斷言「掃描不存在」：那種測試會在功能做出來的那天
    // 變成阻礙。這一格的作用是讓「還沒做」寫在一個會被執行的地方，
    // 而不是只寫在註解裡。

    ctx.teardown().await;
}
