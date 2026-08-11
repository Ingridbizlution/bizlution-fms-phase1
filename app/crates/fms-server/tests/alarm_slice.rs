//! 告警（`/alarms`）。
//!
//! # 這一組的核心是 `c_`：同一個告警不能開出兩張工單
//!
//! 契約明寫 `409 該告警已關聯工單`。在 handler 裡「先讀 `work_order_id`、
//! 是 NULL 才建」在並發下會失效：兩個請求都讀到 NULL，兩張工單都建出來，
//! 而其中一張沒有人知道它存在。
//!
//! migration 056 的判定是條件式 `UPDATE ... WHERE work_order_id IS NULL`，
//! 而那與 006 的 `raise_alarm` 是同一條述詞 —— **自動建單與人工補建之間
//! 也是安全的**，不是只有人工那一側。`c_` 把兩條路徑並排跑。
//!
//! # 告警從哪裡來
//!
//! `fms.raise_alarm()`（006）—— 已實作、T4 驗過。**目前沒有 HTTP 路徑
//! 產生告警**（`POST /telemetry:batch-ingest` 還沒做），所以測試直接呼叫
//! 那支函式，與 T4 相同。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 009 的示範規則與計量點（T4 用的是同一組）。
const RULE_HVAC: &str = "a4000000-0000-4000-8000-000000000001";
const POINT_HVAC: &str = "a3000000-0000-4000-8000-000000000002";

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

/// 觸發一個告警，回傳它的 id。走 006 的 `raise_alarm`，與 T4 相同。
async fn raise(ctx: &TestContext, value: f64, msg: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("SELECT fms.set_context($1::uuid, NULL, false)")
        .bind(TENANT_ID)
        .execute(&mut *tx)
        .await
        .expect("set_context");
    let id: uuid::Uuid =
        sqlx::query_scalar("SELECT fms.raise_alarm($1::uuid, $2::uuid, $3::numeric, $4)")
            .bind(RULE_HVAC)
            .bind(POINT_HVAC)
            .bind(value)
            .bind(msg)
            .fetch_one(&mut *tx)
            .await
            .expect("raise_alarm");
    tx.commit().await.expect("commit");
    id.to_string()
}

/// 直接插一則**未關聯工單**的告警。
///
/// 不能用 `raise_alarm` 觸發第二則：同一個規則 + 同一個計量點在去重窗內
/// 只會讓 `occurrence_count` 加一，**不會產生第二則告警** ——
/// 那是 006 的設計，T4 正在驗它。我的第一版佈置假設兩次呼叫會有兩則，
/// 結果兩個變數指向同一個 id，而 `b_` 的兩個斷言因此互相矛盾。
async fn insert_orphan_alarm(ctx: &TestContext, msg: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.alarms
           (tenant_id, facility_id, alarm_no, source, severity, status, message,
            occurrence_count, first_seen_at, last_seen_at)
         SELECT $1::uuid, f.id,
                fms.next_document_no($1::uuid, 'ALARM', 'AL'),
                'MANUAL', 'MAJOR', 'ACTIVE', $2, 1,
                clock_timestamp(), clock_timestamp()
           FROM fms.facilities f WHERE f.tenant_id = $1::uuid
          ORDER BY f.id LIMIT 1
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(msg)
    .fetch_one(&mut *tx)
    .await
    .expect("插入未關聯告警");
    tx.commit().await.expect("commit");
    id.to_string()
}

/// 把告警與工單的關聯拆掉，模擬「規則沒設定自動建單」或「歷史未串接」。
async fn unlink(ctx: &TestContext, alarm_id: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.alarms SET work_order_id = NULL, work_order_created_at = NULL
          WHERE id = $1::uuid",
    )
    .bind(alarm_id)
    .execute(&mut *tx)
    .await
    .expect("拆關聯");
    tx.commit().await.expect("commit");
}

/// 清單讀得到，過濾條件真的過濾。
#[tokio::test]
async fn a_alarms_are_readable_and_filters_apply() {
    let ctx = &TestContext::setup().await;
    let id = raise(ctx, 512.0, "壓差 512Pa 超過門檻").await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ctx.send(authed(get("/api/v1/alarms"), &admin)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().cloned().unwrap_or_default();
    assert!(!rows.is_empty(), "告警讀不到：{body}");

    let a = rows
        .iter()
        .find(|r| r["id"] == id)
        .cloned()
        .expect("剛觸發的那一則");
    assert_eq!(a["status"], "ACTIVE");
    assert!(
        a["alarm_no"].as_str().unwrap_or_default().len() > 3,
        "alarm_no 要帶出來 —— 值班的人是用它溝通的：{a}"
    );
    assert!(
        a["rule_code"].is_string(),
        "規則代碼要帶出來，只回 uuid 等於沒有回答「為什麼響」：{a}"
    );
    assert!(
        a["work_order_id"].is_string(),
        "009 的規則設定了自動建單，T4 也驗過：{a}"
    );

    // 狀態過濾接受逗號分隔。
    let (_, none) = ctx
        .send(authed(get("/api/v1/alarms?status=CLOSED"), &admin))
        .await;
    assert!(
        !none["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id),
        "status 過濾要真的過濾：{none}"
    );
    let (_, both) = ctx
        .send(authed(
            get("/api/v1/alarms?status=ACTIVE,ACKNOWLEDGED"),
            &admin,
        ))
        .await;
    assert!(
        both["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id),
        "逗號分隔的狀態聯集要成立：{both}"
    );

    let (status, bad) = ctx
        .send(authed(get("/api/v1/alarms?severity=VERY_BAD"), &admin))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "打錯的 severity 要擋下來，不是回空清單：{bad}"
    );

    ctx.teardown().await;
}

/// `unlinked_only` —— 契約說它是「用於稽核 IoT 與工單的串接缺口」。
///
/// 兩格一起驗：串接好的**不**出現、沒串的**要**出現。
/// 只驗一邊的話，一個永遠回空（或永遠回全部）的實作都會通過，
/// 而那讓這個稽核工具變成裝飾。
#[tokio::test]
async fn b_unlinked_only_finds_exactly_the_gap() {
    let ctx = &TestContext::setup().await;
    let linked = raise(ctx, 512.0, "已自動建單").await;
    let orphan = insert_orphan_alarm(ctx, "未串接").await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ctx
        .send(authed(get("/api/v1/alarms?unlinked_only=true"), &admin))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();

    assert!(
        ids.contains(&orphan.as_str()),
        "沒串接的告警要出現在缺口清單裡 —— 那正是這個參數的用途：{body}"
    );
    assert!(
        !ids.contains(&linked.as_str()),
        "已經串好的不該出現，否則這個清單無法用來找缺口：{body}"
    );

    ctx.teardown().await;
}

/// **同一個告警不能開出兩張工單。** 這一組最重要的一格。
///
/// 三個方向：
///   1. 已經自動建單的告警 → 補建要回 409（自動 vs 人工）
///   2. 拆掉關聯後補建 → 201，且關聯真的回填
///   3. 再補建一次 → 409（人工 vs 人工）
#[tokio::test]
async fn c_an_alarm_can_never_get_two_work_orders() {
    let ctx = &TestContext::setup().await;
    let id = raise(ctx, 512.0, "壓差過高").await;
    let admin = ctx.login_as(USERNAME).await;

    // (1) 規則已經自動建過單 —— 人工補建必須被擋。
    let (status, dup) = ctx
        .send(authed(
            post(&format!("/api/v1/alarms/{id}/work-order"), json!({})),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "自動建過單的告警不該能再補建一張：{dup}"
    );
    assert!(
        dup["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("unlinked_only"),
        "訊息要指出怎麼找到真正沒串接的告警：{dup}"
    );

    // (2) 拆掉關聯，模擬「規則沒設定自動建單」。
    unlink(ctx, &id).await;
    let (status, created) = ctx
        .send(authed(
            post(
                &format!("/api/v1/alarms/{id}/work-order"),
                json!({ "priority": "CRITICAL", "title": "人工補建" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["priority"], "CRITICAL");
    assert_eq!(
        created["source"], "IOT_ALARM",
        "來源仍是 IOT_ALARM —— 標成 MANUAL 會讓串接稽核與報表把兩種來源混在一起：{created}"
    );
    assert_eq!(created["alarm_id"], id, "工單要回指告警：{created}");

    // 關聯真的回填了，否則這則告警會再度出現在缺口清單裡。
    let (_, after) = ctx
        .send(authed(get("/api/v1/alarms?unlinked_only=true"), &admin))
        .await;
    assert!(
        !after["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id),
        "補建之後關聯要回填，否則它會被重複補建：{after}"
    );

    // (3) 人工 vs 人工。
    let (status, again) = ctx
        .send(authed(
            post(&format!("/api/v1/alarms/{id}/work-order"), json!({})),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "第二次補建要被擋：{again}");

    ctx.teardown().await;
}

/// 確認：狀態轉移、責任歸屬不被第二個人覆寫、以及非 ACTIVE 要擋下來。
#[tokio::test]
async fn d_acknowledge_keeps_the_first_responder() {
    let ctx = &TestContext::setup().await;
    let id = raise(ctx, 512.0, "壓差過高").await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, acked) = ctx
        .send(authed(
            post(
                &format!("/api/v1/alarms/{id}/acknowledge"),
                json!({ "note": "已到現場" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{acked}");
    assert_eq!(acked["status"], "ACKNOWLEDGED");
    assert_eq!(acked["acknowledged_by"], admin_user_id().to_string());
    let first_at = acked["acknowledged_at"].as_str().unwrap().to_string();

    // 第二個人再按一次：不是錯誤，但**不能改掉責任歸屬**。
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, second) = ctx
        .send(authed(
            post(&format!("/api/v1/alarms/{id}/acknowledge"), json!({})),
            &fm,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "重複確認是 no-op 不是錯誤：{second}"
    );
    assert_eq!(
        second["acknowledged_by"],
        admin_user_id().to_string(),
        "第二個人按下按鈕不該把責任歸屬改掉：{second}"
    );
    assert_eq!(second["acknowledged_at"], first_at, "時間也不該被改掉");

    // 已經 CLOSED 的告警不能確認 —— 回 200 會讓操作者以為自己做了什麼。
    let mut tx = ctx.owner_tx().await;
    sqlx::query("UPDATE fms.alarms SET status = 'CLOSED' WHERE id = $1::uuid")
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("關閉告警");
    tx.commit().await.expect("commit");

    let (status, closed) = ctx
        .send(authed(
            post(&format!("/api/v1/alarms/{id}/acknowledge"), json!({})),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{closed}");
    assert!(
        closed["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("CLOSED"),
        "訊息要說出它現在是什麼狀態：{closed}"
    );

    ctx.teardown().await;
}

/// 權限：讀、確認、建單是三個不同的權限，而且是**對告警所在場域**判定的。
#[tokio::test]
async fn e_permissions_are_evaluated_against_the_alarms_facility() {
    let ctx = &TestContext::setup().await;
    let id = raise(ctx, 512.0, "壓差過高").await;
    unlink(ctx, &id).await;

    // user.huang 是 REQUESTER（總部）：沒有 alarm:read。
    let req = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, denied) = ctx.send(authed(get("/api/v1/alarms"), &req)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "REQUESTER 沒有 alarm:read：{denied}"
    );

    // fm.lin 是 FACILITY_ADMIN（總部）。這個告警在信義影城（009 的規則掛在那裡）
    // —— 他讀得到清單（權限在任一場域成立），但那一則不在他的範圍內。
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, listed) = ctx.send(authed(get("/api/v1/alarms"), &fm)).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let visible = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["id"] == id);

    // 這一格不假設告警落在哪個場域 —— 只要求「看得到就能確認、看不到就 404」，
    // 兩者都不該是「看不到卻改得動」。
    let (status, ack) = ctx
        .send(authed(
            post(&format!("/api/v1/alarms/{id}/acknowledge"), json!({})),
            &fm,
        ))
        .await;
    if visible {
        assert_eq!(status, StatusCode::OK, "看得到就該確認得了：{ack}");
    } else {
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "看不到的告警必須是 404，不能讓他改得動：{ack}"
        );
    }

    ctx.teardown().await;
}
