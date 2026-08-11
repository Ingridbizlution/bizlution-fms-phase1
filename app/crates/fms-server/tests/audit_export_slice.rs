//! 稽核匯出（`/audit-log:export` + worker 產檔）。
//!
//! # 這一組的核心是 `c_`：匯出不能繞過場域收斂
//!
//! relay 跑在**平台情境**下 —— 它必須跨租戶取用 `event_outbox`。
//! 若 handler 就這樣執行匯出查詢，`is_platform_context()` 為真，
//! `audit_log` 兩條政策的第一個 OR 分支都成立，產出的檔案會是
//! **整個資料庫的稽核紀錄**。
//!
//! 那比 053 修的問題更嚴重：053 是「該看到的看不到」，
//! 這裡的失效是「**不該看到的全看到**」，而且是寫進一個可下載的檔案。
//!
//! `c_` 用一個只涵蓋單一場域的發起者去匯出，斷言產出的 CSV 裡沒有
//! 別的場域的列。少了它，handler 少寫三行情境切換也會全部通過。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const FACILITY_CINEMA: &str = "cccccccc-0000-4000-8000-000000000002";
/// `fm.lin` —— FACILITY_ADMIN，範圍只在總部。
const USER_FACILITY_ADMIN: &str = "ffffffff-0000-4000-8000-000000000002";

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

/// 種兩列稽核：一列在總部、一列在影城，外加一列租戶級（facility_id NULL）。
async fn seed_probes(ctx: &TestContext) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.audit_log
           (tenant_id, occurred_at, actor_type, action, entity_type, facility_id)
         VALUES ($1::uuid, clock_timestamp(), 'SYSTEM', 'HQ',     'EXPORT_PROBE', $2::uuid),
                ($1::uuid, clock_timestamp(), 'SYSTEM', 'CINEMA', 'EXPORT_PROBE', $3::uuid),
                ($1::uuid, clock_timestamp(), 'SYSTEM', 'WIDE',   'EXPORT_PROBE', NULL)",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(FACILITY_CINEMA)
    .execute(&mut *tx)
    .await
    .expect("種探針");
    tx.commit().await.expect("commit");
}

/// 直接跑 handler 一次，回傳產出的 CSV 內容。
///
/// 不透過 relay 迴圈：那要等 idle_interval，而這裡要驗的是 handler 的行為，
/// 不是排程。relay 本身由 `notification_template_slice` 的 `run_once` 驗過。
async fn run_export(ctx: &TestContext, export_id: &str) -> (String, i64) {
    // 用測試框架的 storage 而不是 `build_storage()`：後者走
    // `StorageSettings::from_env()`，而測試刻意要能在沒有 .env 的環境下跑
    //（common/mod.rs 的 test_settings 有同樣的說明）。
    let storage = test_storage();
    let handler =
        fms_worker::audit_export::AuditExportHandler::new(ctx.owner_pool().await, storage.clone());
    let n = handler
        .produce(export_id.parse().unwrap())
        .await
        .expect("產檔");

    let mut tx = ctx.owner_tx().await;
    let key: String =
        sqlx::query_scalar("SELECT object_key FROM fms.audit_exports WHERE id = $1::uuid")
            .bind(export_id)
            .fetch_one(&mut *tx)
            .await
            .expect("讀 object_key");
    tx.commit().await.expect("commit");

    // 用預簽網址真的把檔案抓下來 —— 只檢查資料庫欄位的話，
    // 「寫進 S3 了嗎」這件事完全沒被驗到。
    let url = storage.presign_get(&key, "e.csv").await.expect("presign");
    let body = reqwest::get(&url)
        .await
        .expect("下載")
        .text()
        .await
        .expect("讀取");
    (body, n)
}

async fn request_export(ctx: &TestContext, token: &str, filters: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request("POST", "/api/v1/audit-log:export", filters),
        token,
    ))
    .await
}

/// 端到端：建立作業 → outbox 有事件 → 產檔 → 狀態端點回得出下載網址。
#[tokio::test]
async fn a_an_export_runs_end_to_end_and_becomes_downloadable() {
    let ctx = &TestContext::setup().await;
    seed_probes(ctx).await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, created) =
        request_export(ctx, &admin, json!({ "entity_type": "EXPORT_PROBE" })).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{created}");
    assert_eq!(created["status"], "PENDING");
    let id = created["id"].as_str().unwrap().to_string();

    // 同一個交易裡寫了 outbox —— 「作業建立了但沒有人去做」不可能發生。
    let mut tx = ctx.owner_tx().await;
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.event_outbox
          WHERE event_type = $1 AND payload->>'export_id' = $2",
    )
    .bind(fms_worker::audit_export::EVENT_TYPE)
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("查 outbox");
    tx.commit().await.expect("commit");
    assert_eq!(queued, 1, "建立作業必須同時入列，否則沒有人會去做它");

    // 產檔前：狀態端點不該給出下載網址。
    let (_, pending) = ctx
        .send(authed(
            get(&format!("/api/v1/audit-log/exports/{id}")),
            &admin,
        ))
        .await;
    assert!(
        pending["download_url"].is_null(),
        "還沒產檔就給下載網址等於給一個 404：{pending}"
    );

    let (csv, n) = run_export(ctx, &id).await;
    assert_eq!(n, 3, "租戶管理員涵蓋兩個場域 + 租戶級列");
    assert!(
        csv.starts_with("id,occurred_at,"),
        "要有表頭：{}",
        &csv[..60.min(csv.len())]
    );
    assert!(
        !csv.contains("before_data") && !csv.contains("after_data"),
        "整列快照不該出現在匯出裡"
    );

    let (status, done) = ctx
        .send(authed(
            get(&format!("/api/v1/audit-log/exports/{id}")),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "COMPLETED");
    assert_eq!(done["row_count"], 3);
    assert!(
        done["download_url"]
            .as_str()
            .unwrap_or_default()
            .starts_with("http"),
        "COMPLETED 一定要有下載網址（054 的 CHECK 保證有 object_key）：{done}"
    );

    ctx.teardown().await;
}

/// 過濾條件真的傳到 worker，而且 **0 列是合法的答案**。
///
/// 回 0 列與作業失敗必須分得出來 —— 前者代表「那段時間沒有事件」，
/// 而把它當成錯誤會讓人去找不存在的問題。
#[tokio::test]
async fn b_filters_reach_the_worker_and_zero_rows_is_a_valid_answer() {
    let ctx = &TestContext::setup().await;
    seed_probes(ctx).await;
    let admin = ctx.login_as(USERNAME).await;

    let (_, created) = request_export(
        ctx,
        &admin,
        json!({ "entity_type": "EXPORT_PROBE", "action": "HQ" }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    let (csv, n) = run_export(ctx, &id).await;
    assert_eq!(n, 1, "action 過濾要真的傳到 worker：{csv}");
    assert!(csv.contains("\"HQ\"") && !csv.contains("\"CINEMA\""));

    // 一個不可能命中的條件。
    let (_, empty) = request_export(
        ctx,
        &admin,
        json!({ "entity_type": "NOTHING_MATCHES_THIS" }),
    )
    .await;
    let empty_id = empty["id"].as_str().unwrap().to_string();
    let (csv, n) = run_export(ctx, &empty_id).await;
    assert_eq!(n, 0);
    assert_eq!(csv.lines().count(), 1, "只有表頭：{csv}");

    let (_, done) = ctx
        .send(authed(
            get(&format!("/api/v1/audit-log/exports/{empty_id}")),
            &admin,
        ))
        .await;
    assert_eq!(
        done["status"], "COMPLETED",
        "0 列是答案不是錯誤 —— 把它當失敗會讓人去找不存在的問題：{done}"
    );
    assert_eq!(done["row_count"], 0);

    ctx.teardown().await;
}

/// **匯出不能繞過場域收斂。** 這一組最重要的一格。
///
/// worker 跑在平台情境下。少了 `write_csv` 開頭那三步情境切換，
/// 產出的檔案會包含發起者本來看不到的列 —— 而且是寫進一個可下載的檔案。
///
/// 發起者用 `fm.lin`（範圍只在總部）。他匯出的 CSV：
///   * 必須有總部那一列
///   * **不能**有影城那一列
///
/// 反面（總部那一列必須在）不可省：若情境切換寫成「一律看不到」，
/// 只驗「影城不在」也會通過，而那時匯出永遠是空的。
#[tokio::test]
async fn c_the_export_is_scoped_to_the_requester_not_the_worker() {
    let ctx = &TestContext::setup().await;
    seed_probes(ctx).await;
    let admin = ctx.login_as(USERNAME).await;

    // fm.lin 沒有 audit:export，所以作業由管理員建立，再把 requested_by
    // 改成他 —— 要驗的是 **worker 用誰的身分查**，不是端點的權限判定
    //（那由 `d_` 驗）。
    let (_, created) = request_export(ctx, &admin, json!({ "entity_type": "EXPORT_PROBE" })).await;
    let id = created["id"].as_str().unwrap().to_string();
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.audit_exports SET requested_by = $2::uuid WHERE id = $1::uuid")
            .bind(&id)
            .bind(USER_FACILITY_ADMIN)
            .execute(&mut *tx)
            .await
            .expect("改 requested_by");
        tx.commit().await.expect("commit");
    }

    let (csv, n) = run_export(ctx, &id).await;

    assert!(
        csv.contains("\"HQ\""),
        "總部那一列必須在 —— 少了它代表情境切換切成了「什麼都看不到」：{csv}"
    );
    assert!(
        !csv.contains("\"CINEMA\""),
        "**匯出繞過了場域收斂。** worker 跑在平台情境下，\
         若沒有以 requested_by 的身分重新注入情境，產出的檔案會包含\
         發起者本來看不到的列：{csv}"
    );
    // 租戶級那一列（facility_id IS NULL）也不在 —— 那是 053 的**另一半**：
    // 租戶級稽核列只有具 TENANT 範圍的人看得到，而 fm.lin 是 FACILITY 範圍。
    //
    // 我原本預期 2 列（總部 + 租戶級），跑出來是 1。錯的是預期：
    // 匯出正確地繼承了 053 的兩半語意，而不是只繼承了場域那一半。
    assert!(
        !csv.contains("\"WIDE\""),
        "租戶級列只有 TENANT 範圍的人看得到（053），FACILITY 範圍的發起者不該拿到：{csv}"
    );
    assert_eq!(n, 1, "只有總部那一列：{csv}");

    ctx.teardown().await;
}

/// 權限與輸入。
///
/// 也順帶釘住**兩個 crate 各自宣告的事件型別必須相等** ——
/// 發送端在 `fms-identity`、接收端在 `fms-worker`，刻意不互相 import
///（那會讓 worker 依賴 HTTP 層的 crate）。不比對的話，兩邊寫成不同字串時
/// 症狀是「作業永遠 PENDING」，而沒有任何錯誤訊息。
#[tokio::test]
async fn d_permission_input_and_the_event_type_agree() {
    assert_eq!(
        fms_identity::audit_export::EVENT_TYPE,
        fms_worker::audit_export::EVENT_TYPE,
        "兩個 crate 的事件型別不一致 —— 匯出作業會永遠停在 PENDING 而不報錯"
    );

    let ctx = &TestContext::setup().await;

    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, denied) = request_export(ctx, &fm, json!({})).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "FACILITY_ADMIN 沒有 audit:export：{denied}"
    );

    let admin = ctx.login_as(USERNAME).await;
    let (status, bad) = request_export(
        ctx,
        &admin,
        json!({ "from": "2026-08-01T00:00:00Z", "to": "2026-07-01T00:00:00Z" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");

    // 不帶條件是合法的，但要說出它會匯出全部。
    let (status, all) = request_export(ctx, &admin, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{all}");
    assert!(
        all["meta"]["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("全部"),
        "空條件會匯出整個租戶的稽核史，回應要說出來：{all}"
    );

    ctx.teardown().await;
}

/// 失敗必須落地成 `FAILED`，不能停在 `RUNNING`。
///
/// 「還在跑」與「早就死了」看起來一樣的話，輪詢的人永遠等不到答案 ——
/// 而這正是這個專案反覆出現的缺陷類型：失敗了但沒有人知道。
///
/// 用一個不存在的 bucket 讓上傳失敗。這是真的失敗路徑，不是模擬：
/// handler 的 `write_csv` 會在 `storage.put` 那一步回 Err。
#[tokio::test]
async fn e_a_failed_export_lands_as_failed_with_a_reason() {
    let ctx = &TestContext::setup().await;
    seed_probes(ctx).await;
    let admin = ctx.login_as(USERNAME).await;

    let (_, created) = request_export(ctx, &admin, json!({})).await;
    let id = created["id"].as_str().unwrap().to_string();

    let bad = fms_shared::StorageSettings {
        endpoint: std::env::var("S3_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:9000".to_string()),
        public_endpoint: None,
        access_key: std::env::var("S3_ACCESS_KEY").unwrap_or_else(|_| "fmsminio".to_string()),
        secret_key: std::env::var("S3_SECRET_KEY")
            .unwrap_or_else(|_| "change_me_minio".to_string()),
        region: "us-east-1".to_string(),
        bucket_attachments: "this-bucket-does-not-exist-054".to_string(),
        download_ttl: std::time::Duration::from_secs(300),
    };
    let handler = fms_worker::audit_export::AuditExportHandler::new(
        ctx.owner_pool().await,
        fms_shared::Storage::new(&bad),
    );

    let err = handler.produce(id.parse().unwrap()).await;
    assert!(err.is_err(), "上傳到不存在的 bucket 應該失敗：{err:?}");

    let (status, failed) = ctx
        .send(authed(
            get(&format!("/api/v1/audit-log/exports/{id}")),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{failed}");
    assert_eq!(
        failed["status"], "FAILED",
        "失敗要落地 —— 停在 RUNNING 的話輪詢的人永遠等不到答案：{failed}"
    );
    assert!(
        !failed["error"].as_str().unwrap_or_default().is_empty(),
        "054 的 CHECK 要求 FAILED 一定有 error，而它要說得出原因：{failed}"
    );
    assert!(
        failed["download_url"].is_null(),
        "失敗的作業不該有下載網址：{failed}"
    );

    ctx.teardown().await;
}

/// 重放不會重做，也不會把已完成的結果改壞。
///
/// relay 保證**至少一次**投遞，因此重放一定會發生。
/// `produce` 只處理 PENDING／RUNNING；已完成的直接回成功。
///
/// 這一格也順帶擋住一個具體的壞法：若重放把 `row_count` 清成 NULL 再更新，
/// 054 的 `ck_audit_exports_result` 會擋下那個 UPDATE，而 handler 只會
/// 回一個難懂的 23514。
#[tokio::test]
async fn f_replaying_a_completed_export_is_a_no_op() {
    let ctx = &TestContext::setup().await;
    seed_probes(ctx).await;
    let admin = ctx.login_as(USERNAME).await;

    let (_, created) = request_export(ctx, &admin, json!({ "entity_type": "EXPORT_PROBE" })).await;
    let id = created["id"].as_str().unwrap().to_string();

    let (_, n1) = run_export(ctx, &id).await;
    assert_eq!(n1, 3);

    let handler =
        fms_worker::audit_export::AuditExportHandler::new(ctx.owner_pool().await, test_storage());
    let n2 = handler
        .produce(id.parse().unwrap())
        .await
        .expect("重放不該失敗");
    assert_eq!(n2, 0, "已完成的作業重放要直接回成功而不重做");

    let (_, done) = ctx
        .send(authed(
            get(&format!("/api/v1/audit-log/exports/{id}")),
            &admin,
        ))
        .await;
    assert_eq!(done["status"], "COMPLETED", "重放不該把狀態改壞：{done}");
    assert_eq!(done["row_count"], n1, "列數不該被重放清掉");

    ctx.teardown().await;
}
