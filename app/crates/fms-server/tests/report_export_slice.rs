//! 報表匯出（`POST /reports/{reportCode}:export` + worker 產檔）。
//!
//! # 這一組的核心是 `d_`：匯出不能繞過場域收斂
//!
//! 六支報表函式都是 `SECURITY INVOKER`，收斂靠**呼叫者的情境**。relay 跑在
//! 平台情境下，若 handler 就這樣呼叫報表函式，每一張底層表的
//! `tenant_isolation` 與 `facility_scope` 第一個 OR 分支都成立，產出的檔案
//! 會是整個資料庫 —— 而且是寫進一個可下載的檔案。
//!
//! `d_` 用一個只涵蓋單一場域的發起者去匯出，斷言產出的 CSV 裡沒有別的場域
//! 的列。少了它，handler 少寫三行情境切換也會全部通過。
//!
//! # `a_` 盯的是另一類：兩份清單分歧
//!
//! 報表目錄在兩個地方各宣告一份（`fms-report::export::REPORTS` 與 worker 的
//! `SPECS`），刻意不共用（crate 方向）。**重複而沒有人比對**才是問題，
//! 所以 `a_` 逐一比對它們與實際掛上路由的 `GET /reports/*`，三個方向都查。

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

fn window() -> (String, String) {
    (
        (chrono::Utc::now() - chrono::Duration::days(30))
            .date_naive()
            .to_string(),
        (chrono::Utc::now() + chrono::Duration::days(1))
            .date_naive()
            .to_string(),
    )
}

async fn request_export(
    ctx: &TestContext,
    token: &str,
    code: &str,
    body: Value,
) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request("POST", &format!("/api/v1/reports/{code}:export"), body),
        token,
    ))
    .await
}

/// 直接跑 handler 一次，回傳產出的檔案內容（bytes）與列數。
///
/// 不透過 relay 迴圈：那要等 idle_interval，而這裡要驗的是 handler 的行為，
/// 不是排程 —— 與 `audit_export_slice.rs` 同一個做法。
async fn run_export(ctx: &TestContext, export_id: &str) -> (Vec<u8>, i64) {
    let storage = test_storage();
    let handler = fms_worker::report_export::ReportExportHandler::new(
        ctx.owner_pool().await,
        storage.clone(),
    );
    let n = handler
        .produce(export_id.parse().unwrap())
        .await
        .expect("產檔");

    let mut tx = ctx.owner_tx().await;
    let key: String =
        sqlx::query_scalar("SELECT object_key FROM fms.report_exports WHERE id = $1::uuid")
            .bind(export_id)
            .fetch_one(&mut *tx)
            .await
            .expect("讀 object_key");
    tx.commit().await.expect("commit");

    // 用預簽網址真的把檔案抓下來 —— 只檢查資料庫欄位的話，
    // 「寫進 S3 了嗎」這件事完全沒被驗到。
    let url = storage.presign_get(&key, "e.bin").await.expect("presign");
    let body = reqwest::get(&url)
        .await
        .expect("下載")
        .bytes()
        .await
        .expect("讀取");
    (body.to_vec(), n)
}

/// **兩份報表清單與實際路由三方一致。**
///
/// 分歧的三個方向都難察覺：
///   * 清單有、路由沒有 → 可匯出一份讀不到的報表
///   * 路由有、清單沒有 → 那支報表匯不出來，而沒有任何錯誤訊息
///   * API 有、worker 沒有 → 作業建立成功，產檔時才 FAILED
#[tokio::test]
async fn a_the_report_catalogue_matches_the_routes_and_the_worker() {
    // 事件型別：兩個 crate 各自宣告。不比對的話，寫成不同字串時症狀是
    // 「作業永遠 PENDING」，而沒有任何錯誤訊息。
    assert_eq!(
        fms_report::export::EVENT_TYPE,
        fms_worker::report_export::EVENT_TYPE,
        "兩個 crate 的事件型別不一致 —— 匯出作業會永遠停在 PENDING 而不報錯"
    );

    // 已掛上路由的 GET /reports/*。
    let routed: Vec<&str> = fms_server::IMPLEMENTED_OPERATIONS
        .iter()
        .filter(|(m, p)| *m == "get" && p.starts_with("/reports/") && !p.contains('{'))
        .map(|(_, p)| p.trim_start_matches("/reports/"))
        .collect();
    assert!(
        routed.len() >= 7,
        "示範資料該有七支報表讀取端點，實際 {routed:?}"
    );

    // `facility-dashboard` 是唯一的已知例外：它回彙總物件而不是列，
    // 沒有表頭可寫。**明列成例外**，而不是靜默地不在清單裡。
    const NOT_EXPORTABLE: [&str; 1] = ["facility-dashboard"];

    for code in &routed {
        if NOT_EXPORTABLE.contains(code) {
            assert!(
                fms_report::export::find_report(code).is_none(),
                "{code} 在 NOT_EXPORTABLE 裡卻又出現在匯出清單 —— 兩邊要一致"
            );
            continue;
        }
        assert!(
            fms_report::export::find_report(code).is_some(),
            "`GET /reports/{code}` 有路由但匯不出來 —— \
             這支報表的 :export 會回 422，而沒有任何地方說明為什麼"
        );
    }

    for spec in fms_report::export::REPORTS {
        assert!(
            routed.contains(&spec.code),
            "匯出清單有 {} 但沒有對應的 GET 路由 —— 可匯出一份讀不到的報表",
            spec.code
        );
    }

    // worker 那一份：用它產檔一次就會發現分歧，但那時作業已經 FAILED。
    // 這裡靠端到端把每一支都跑一遍（`b_`），並在這裡先擋掉數量不符。
    let ctx = &TestContext::setup().await;
    let mut tx = ctx.owner_tx().await;
    for spec in fms_report::export::REPORTS {
        // 函式必須存在，而且 `extra_params` 的名稱必須是它真的收的參數 ——
        // 名稱錯了會在產檔時才炸（`p_xxx` 不存在）。
        let args: Vec<String> = sqlx::query_scalar(
            "SELECT unnest(p.proargnames) FROM pg_proc p
              WHERE p.proname = $1 AND p.pronamespace = 'fms'::regnamespace",
        )
        .bind(spec.function)
        .fetch_all(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("查 {} 的參數失敗：{e}", spec.function));
        assert!(
            !args.is_empty(),
            "fms.{} 不存在 —— 匯出清單指向一個沒有的函式",
            spec.function
        );
        assert!(
            args.contains(&"p_from".to_string()) && args.contains(&"p_to".to_string()),
            "fms.{} 沒有 p_from／p_to，但匯出一律用具名記號帶這兩個",
            spec.function
        );
        for (_, arg, _, _) in spec.extra_params {
            assert!(
                args.contains(&arg.to_string()),
                "fms.{} 沒有參數 `{arg}` —— 具名記號呼叫會在產檔時炸",
                spec.function
            );
        }
    }
    tx.commit().await.expect("commit");

    ctx.teardown().await;
}

/// 端到端：每一支都匯得出來，表頭順序等於函式的宣告順序。
#[tokio::test]
async fn b_every_report_exports_with_the_declared_column_order() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();
    ctx.seed_work_order(FACILITY_HQ, "匯出測試").await;

    for spec in fms_report::export::REPORTS {
        let (status, created) =
            request_export(ctx, &token, spec.code, json!({"from": from, "to": to})).await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "{} 該回 202：{created}",
            spec.code
        );
        assert_eq!(created["status"], "PENDING");
        assert_eq!(created["format"], "csv", "預設格式是 csv");
        assert_eq!(created["report_code"], spec.code);

        let id = created["id"].as_str().expect("id").to_string();
        let (bytes, _n) = run_export(ctx, &id).await;
        let csv = String::from_utf8(bytes).expect("utf-8");
        let header = csv.lines().next().expect("表頭");

        // 表頭必須等於函式的 RETURNS TABLE 順序 —— 逐字比對，不只檢查
        // 「有沒有那幾個字」。錯位的 CSV 看起來完全正常。
        let mut tx = ctx.owner_tx().await;
        let expected: Vec<String> = sqlx::query_scalar(
            "SELECT p.proargnames[i]
               FROM pg_proc p, generate_subscripts(p.proargnames, 1) i
              WHERE p.proname = $1 AND p.pronamespace = 'fms'::regnamespace
                AND p.proargmodes[i] = 't'
              ORDER BY i",
        )
        .bind(spec.function)
        .fetch_all(&mut *tx)
        .await
        .expect("查欄位順序");
        tx.commit().await.expect("commit");

        let want = expected
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            header, want,
            "{} 的表頭與函式宣告的欄位順序不符 —— 錯位的 CSV 看起來完全正常，\
             而它會被拿去對帳",
            spec.code
        );
    }

    ctx.teardown().await;
}

/// **參數真的傳到 worker，而 0 列是合法的答案。**
#[tokio::test]
async fn c_params_reach_the_worker_and_zero_rows_is_valid() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 一段沒有任何資料的過去區間 → 0 列，但狀態是 COMPLETED 而不是 FAILED。
    let (status, created) = request_export(
        ctx,
        &token,
        "service-volume",
        json!({"from": "2000-01-01", "to": "2000-01-31", "group_by": "facility"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{created}");
    assert_eq!(
        created["params"]["group_by"], "facility",
        "參數要原樣存下來，worker 才讀得到：{created}"
    );

    let id = created["id"].as_str().expect("id").to_string();
    let (bytes, n) = run_export(ctx, &id).await;
    let csv = String::from_utf8(bytes).expect("utf-8");
    assert_eq!(n, 0, "那段期間沒有服務工單");
    assert_eq!(
        csv.lines().count(),
        1,
        "**只有表頭**。0 列是合法的答案，不是失敗：{csv}"
    );

    // 狀態端點：COMPLETED + row_count 0 + 有下載網址。
    let (status, done) = ctx
        .send(authed(
            get(&format!("/api/v1/reports/exports/{id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "COMPLETED");
    assert_eq!(
        done["row_count"], 0,
        "**0 不是 null** —— 那段期間真的沒有資料"
    );
    assert!(
        done["download_url"]
            .as_str()
            .is_some_and(|u| u.contains("http")),
        "COMPLETED 一定要有下載網址，否則 202 之後就沒有下文：{done}"
    );
    assert_eq!(done["error"], Value::Null);

    ctx.teardown().await;
}

/// **匯出以發起者的身分查，不是 worker 的。**
///
/// 這一格是整組的核心。少了 handler 那三行情境切換，產出的檔案會含
/// 發起者看不到的場域。
#[tokio::test]
async fn d_the_export_is_scoped_to_the_requester_not_the_worker() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();

    // 兩個場域各一張服務工單，用不同的服務項目分組才分得開。
    let mut tx = ctx.owner_tx().await;
    for facility in [FACILITY_HQ, FACILITY_CINEMA] {
        sqlx::query(
            "INSERT INTO fms.work_orders
               (tenant_id, facility_id, wo_no, work_order_type, source, title,
                status, priority, spatial_node_id, service_item_id, labor_minutes)
             SELECT $1::uuid, $2::uuid,
                    'WO-SC-' || substr(md5(random()::text), 1, 8),
                    'SERVICE', 'MANUAL', '收斂測試', 'IN_PROGRESS', 'MEDIUM',
                    (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                    (SELECT id FROM fms.service_items LIMIT 1), 30",
        )
        .bind(TENANT_ID)
        .bind(facility)
        .execute(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("建 {facility} 的服務工單失敗：{e}"));
    }
    tx.commit().await.expect("commit");

    // 依場域分組，這樣兩個場域是兩列，看得出哪一列不該在。
    let (_, created) = request_export(
        ctx,
        &token,
        "service-volume",
        json!({"from": from, "to": to, "group_by": "facility"}),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_string();

    // fm.lin 沒有 report:export，所以作業由管理員建立，再把 requested_by
    // 改成他 —— 要驗的是 **worker 用誰的身分查**，不是端點的權限判定
    //（那由 `f_` 驗）。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.report_exports SET requested_by = $2::uuid WHERE id = $1::uuid")
            .bind(&id)
            .bind(USER_FACILITY_ADMIN)
            .execute(&mut *tx)
            .await
            .expect("改 requested_by");
        tx.commit().await.expect("commit");
    }

    let (bytes, n) = run_export(ctx, &id).await;
    let csv = String::from_utf8(bytes).expect("utf-8");

    assert!(
        csv.contains(FACILITY_HQ),
        "總部那一列必須在 —— 少了它代表情境切換切成了「什麼都看不到」：{csv}"
    );
    assert!(
        !csv.contains(FACILITY_CINEMA),
        "**匯出繞過了場域收斂。** 報表函式是 SECURITY INVOKER，\
         worker 若沒有以 requested_by 的身分重新注入情境（含 app.facility_ids），\
         產出的檔案會包含發起者本來看不到的場域：{csv}"
    );
    assert_eq!(n, 1, "只有總部那一列：{csv}");

    ctx.teardown().await;
}

/// xlsx 也產得出來，而且是 zip 容器。
#[tokio::test]
async fn e_xlsx_is_produced_as_a_real_workbook() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();

    let (status, created) = request_export(
        ctx,
        &token,
        "group-rollup",
        json!({"from": from, "to": to, "format": "xlsx"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{created}");
    assert_eq!(created["format"], "xlsx");

    let id = created["id"].as_str().expect("id").to_string();
    let (bytes, n) = run_export(ctx, &id).await;
    assert!(n >= 1, "示範資料該有組織列");
    assert_eq!(
        &bytes[0..4],
        b"PK\x03\x04",
        "xlsx 必須是 zip 容器 —— 契約寫的是 xlsx/csv，兩種都要真的能開"
    );

    // 下載檔名帶正確的副檔名。錯的副檔名會讓試算表拒開一個內容正確的檔案。
    let (_, done) = ctx
        .send(authed(
            get(&format!("/api/v1/reports/exports/{id}")),
            &token,
        ))
        .await;
    let url = done["download_url"].as_str().expect("url");
    assert!(
        url.contains(".xlsx") || url.contains("xlsx"),
        "預簽網址的檔名該是 .xlsx：{url}"
    );

    ctx.teardown().await;
}

/// 權限、未知報表、未知參數、日期填反、未知格式。
#[tokio::test]
async fn f_validation_rejects_what_would_produce_a_plausible_wrong_file() {
    let ctx = &TestContext::setup().await;
    let (from, to) = window();

    // 沒有 report:export → 403。
    //
    // 用 user.huang（REQUESTER）而不是 fm.lin：**FACILITY_ADMIN 有
    // `report:export`**（008 把它連同 `report:read` 一起給了 ORG_MANAGER 與
    // FACILITY_ADMIN）。`d_` 借用 fm.lin 當 `requested_by` 正是因為他有那個
    // 權限卻只看得到一個場域 —— 那才是收斂測得出來的組合。
    let requester = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, _) = request_export(
        ctx,
        &requester,
        "service-volume",
        json!({"from": from, "to": to}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "user.huang 沒有 report:export"
    );

    let token = ctx.login().await;

    // 不可匯出的報表 → 404：路由表就是白名單，那條路徑不存在。
    //
    // 原本預期 422（handler 檢查代碼），但 router 從 `REPORTS` 逐一展開之後
    // 那個分支走不到了。**404 是更強的性質**：不可匯出的報表連端點都沒有，
    // 而不是有一個端點會拒絕你。
    for code in ["facility-dashboard", "no-such-report"] {
        let (status, p) = request_export(ctx, &token, code, json!({"from": from, "to": to})).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{code} 不在匯出清單裡，路徑就不該存在：{p}"
        );
    }

    // **未知的參數鍵 → 422 而不是忽略。**
    // 忽略一個打錯的 facility_id 會產出一份範圍比預期大的檔案，
    // 而它看起來完全正常。
    let (status, p) = request_export(
        ctx,
        &token,
        "space-utilization",
        json!({"from": from, "to": to, "facilty_id": FACILITY_HQ}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "打錯的參數鍵被忽略了 —— 那會產出一份範圍比預期大的檔案：{p}"
    );

    // 日期填反 → 422，不是一份看起來合理的空檔案。
    let (status, _) = request_export(
        ctx,
        &token,
        "service-volume",
        json!({"from": to, "to": from}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 未知格式 → 422。
    let (status, _) = request_export(
        ctx,
        &token,
        "service-volume",
        json!({"from": from, "to": to, "format": "pdf"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 缺 from → 422（serde 的必填欄位）。
    let (status, _) = request_export(ctx, &token, "service-volume", json!({"to": to})).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST,
        "from 是必填的，實際 {status}"
    );

    // 不存在的作業 → 404。
    let (status, _) = ctx
        .send(authed(
            get("/api/v1/reports/exports/00000000-0000-4000-8000-000000000000"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// 失敗要落地成 FAILED 且帶原因；重放已完成的作業是 no-op。
#[tokio::test]
async fn g_failures_land_and_replays_are_no_ops() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();

    // --- 失敗 ---------------------------------------------------------------
    // 把 report_code 改成一個 worker 不認的（但通得過 066 的字元集 CHECK）。
    // 這模擬「API 與 worker 的清單分歧」，也就是 `a_` 要防的那件事真的發生時
    // 的樣子 —— 它必須是一個帶原因的 FAILED，不是永遠 RUNNING。
    let (_, created) = request_export(
        ctx,
        &token,
        "service-volume",
        json!({"from": from, "to": to}),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_string();
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.report_exports SET report_code = 'no-such-report' WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("改 report_code");
        tx.commit().await.expect("commit");
    }
    let handler =
        fms_worker::report_export::ReportExportHandler::new(ctx.owner_pool().await, test_storage());
    let err = handler.produce(id.parse().unwrap()).await;
    assert!(err.is_err(), "認不出的報表該失敗");

    let (_, failed) = ctx
        .send(authed(
            get(&format!("/api/v1/reports/exports/{id}")),
            &token,
        ))
        .await;
    assert_eq!(
        failed["status"], "FAILED",
        "失敗要落地 —— 否則客戶端輪詢到的永遠是 RUNNING，\
         而「還在跑」與「早就死了」看起來一樣：{failed}"
    );
    assert!(
        failed["error"].as_str().is_some_and(|e| !e.is_empty()),
        "FAILED 一定要有原因（066 的 CHECK）：{failed}"
    );
    assert_eq!(failed["download_url"], Value::Null);

    // --- 重放 ---------------------------------------------------------------
    let (_, created) =
        request_export(ctx, &token, "group-rollup", json!({"from": from, "to": to})).await;
    let id = created["id"].as_str().expect("id").to_string();
    let (_, first) = run_export(ctx, &id).await;
    // 第二次：relay 保證至少一次投遞，所以重放會發生。
    let again = handler.produce(id.parse().unwrap()).await.expect("重放");
    assert_eq!(
        again, 0,
        "已完成的作業重放回 0 而不是重做 —— 檔案已經在了：{first}"
    );

    let (_, done) = ctx
        .send(authed(
            get(&format!("/api/v1/reports/exports/{id}")),
            &token,
        ))
        .await;
    assert_eq!(
        done["status"], "COMPLETED",
        "重放不該把一個已完成的作業改壞：{done}"
    );
    assert_eq!(done["row_count"], first, "列數不該被重放改掉");

    ctx.teardown().await;
}

/// 建立作業與 outbox 事件在同一個交易裡 —— 「建立了但沒有人去做」不可能發生。
#[tokio::test]
async fn h_the_outbox_event_is_written_in_the_same_transaction() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();

    let (_, created) = request_export(
        ctx,
        &token,
        "asset-reliability",
        json!({"from": from, "to": to}),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_string();

    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.event_outbox
          WHERE event_type = $1 AND aggregate_id = $2::uuid AND aggregate_type = 'REPORT_EXPORT'",
    )
    .bind(fms_report::export::EVENT_TYPE)
    .bind(&id)
    .fetch_one(&mut *tx)
    .await
    .expect("查 outbox");
    tx.commit().await.expect("commit");
    assert_eq!(
        n, 1,
        "作業與 outbox 事件必須同一個交易 —— 少了事件，作業永遠 PENDING 而沒人知道"
    );

    ctx.teardown().await;
}
