//! 併發下的**正確性**，不是效能。
//!
//! # 這個切片問的四個問題
//!
//! | 不變量 | 失敗的樣子 |
//! |---|---|
//! | `a_` 同時搶同一時段 → **恰好一筆成立，落敗者全部 409** | 兩筆都成立（雙重預約）或落敗者拿到 **500** |
//! | `b_` 同時用同一個 refresh token 換發 → 恰好一次成功 | 兩個都拿到可用的 token pair（撤銷鏈分岔） |
//! | `c_` 同時用同一個 `Idempotency-Key` 建立 → 只建立一筆 | 建立兩筆（冪等在併發下失效） |
//! | `d_` 同時以同一個 `If-Match` 版本更新 → 恰好一次成功 | 兩次都 200（後寫的默默覆蓋前一個，lost update） |
//!
//! # 為什麼 T11 不夠
//!
//! `docker/scripts/concurrency-test.sh`（T11）已經在 **SQL 層**驗過 100 路搶訂
//! 恰好一筆落地。那證明了 005 的排除約束成立。
//!
//! **但它沒有經過 HTTP 層。** 而 ADR-09 實作紀律 5 說的是「搶輸不可以是 500」——
//! 那是 `Problem::from(sqlx::Error)` 的對應規則，T11 完全碰不到它。
//!
//! 那條對應規則有一個很容易漏的細節：**PostgreSQL 在高併發下會擇一犧牲，
//! 落敗者的錯誤碼在 `23P01`（排除約束）與 `40P01`（偵測到死鎖）之間隨機分佈**
//! （problem.rs 的註解引用了 T11 的觀測）。只對應 `23P01` 的實作在低併發時
//! 看起來完全正確，而在真實負載下開始間歇地回 500。
//!
//! `a_` 因此**斷言的是「沒有任何一個落敗者是 5xx」**，而不是「錯誤碼是某個
//! 特定值」—— 後者會讓這一格在 PostgreSQL 換一種犧牲策略時假性失敗。
//!
//! # 為什麼用 `tokio::spawn` 而不是 `join!`
//!
//! `join!` 在同一個 task 上輪詢，因此併發只發生在 `await` 點。那足以讓兩個
//! INSERT 同時在資料庫裡飛，但**不會**產生跨執行緒的競爭。
//!
//! 這裡把 router clone 出 N 份（它是 `Clone` 且不借用 `TestContext`），
//! 然後 `spawn` 到 multi-thread runtime 上 —— 那是真的並行。
//! 每個測試都標 `flavor = "multi_thread"`，否則 spawn 出來的 task 仍然
//! 在單一執行緒上排隊。
//!
//! # 屏障：不然這些測試會偶然通過
//!
//! `tokio::spawn` 只保證「可以並行」，不保證「同時開始」。實測：資產那一格
//! 單獨跑時穩定抓到 lost update，整個檔案一起跑時六路偶然被序列化而變綠。
//! 於是突變測試回報「沒有被抓到」，而缺陷確實在那裡。
//!
//! `race()` 因此用 `tokio::sync::Barrier` 把所有請求的「開始」對齊。
//!
//! **屏障仍然不夠。** 加了屏障之後資產那一格穩定抓到，而預約那一格反而
//! 開始偶然放過 —— 不穩定性只是搬家了。預約的 PATCH 跳過權限檢查
//!（主辦人就是呼叫者），路徑更短，讀取到寫入的窗口更窄。
//!
//! 因此每個不變量跑 `ROUNDS` 輪，任何一輪破功就失敗。
//!
//! # 連線池要夠大
//!
//! `test_settings` 的 `max_connections` 是 5。**六路併發會在池子上排隊**，
//! 於是請求被序列化，而序列化的請求不會競爭 —— 這一格會變成一個
//! 什麼都沒驗到的綠燈。因此每個測試都用 `setup_with` 把池子開大。
//!
//! # 沒有被覆蓋的
//!
//! **真實負載下的分佈。** 這裡是 6–8 路，而 T11 用 100 路。目的不同：
//! T11 問「約束擋不擋得住」，這裡問「HTTP 層怎麼回報」。兩者都需要。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_A: &str = "cccccccc-0000-4000-8000-000000000001";

/// 併發路數。**要大於 1 但小於連線池** —— 見檔頭。
const RACERS: usize = 10;

/// 每個不變量重複幾輪。
///
/// **一輪不夠。** 即使有屏障對齊起點，「讀取版本」到「UPDATE」之間的窗口是
/// 微秒級的，六路是否真的重疊仍然是隨機的 —— 實測到的具體症狀是：
/// 拿掉預約的列鎖之後，`d_` 有時抓到（2 個贏家）、有時放過（1 個贏家）。
///
/// 多輪把「至少觀察到一次真實重疊」的機率推高。任何一輪出現第二個贏家就失敗，
/// 因此多輪只會讓偵測更強，不會讓它更寬鬆。
const ROUNDS: usize = 4;

async fn wide_pool_ctx() -> TestContext {
    TestContext::setup_with(|s| {
        // 6 路併發 + 每個請求一條連線 + 測試自己的 owner_tx 還要一條。
        s.database.max_connections = 16;
    })
    .await
}

/// 挑一個可預約資源的 `resource_id`（不是 `bookable_resource_id`）。
async fn pick_resource(ctx: &TestContext) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "SELECT coalesce(br.spatial_node_id, br.asset_id)
           FROM fms.bookable_resources br
          WHERE br.facility_id = $1::uuid AND br.is_bookable
          ORDER BY br.display_name LIMIT 1",
    )
    .bind(FACILITY_A)
    .fetch_one(&mut *tx)
    .await
    .expect("pick resource");
    drop(tx);
    id
}

fn tomorrow(hour: u32) -> (String, String) {
    let base = (chrono::Utc::now() + chrono::Duration::days(1))
        .date_naive()
        .and_hms_opt(hour, 0, 0)
        .expect("valid")
        .and_utc();
    (
        base.to_rfc3339(),
        (base + chrono::Duration::hours(1)).to_rfc3339(),
    )
}

/// 把 N 個請求同時送出去，回傳 `(status, body)` 的清單。
///
/// **router 先 clone 出 N 份**：那讓每個 task 都是 `'static`，
/// 因此可以 `spawn` 到 runtime 的多個執行緒上（真並行，不是輪詢併發）。
async fn race(ctx: &TestContext, reqs: Vec<Request<Body>>) -> Vec<(StatusCode, Value)> {
    use tower::ServiceExt;

    // **屏障：所有請求在同一刻才發出。**
    //
    // 少了它這些測試會**偶然通過**。實測過程中，資產那一格單獨跑時穩定地
    // 抓到 lost update（`[(200, 2), (412, 4)]`），而整個檔案一起跑時
    // 六路偶然被序列化 → 恰好一個贏家 → 綠燈。也就是說突變測試回報
    // 「沒有被抓到」，而缺陷其實在那裡。
    //
    // **一個會偶然通過的併發測試比沒有更糟**：它給出的是假保證，而且
    // 那個假保證只在機器忙的時候出現 —— 也就是 CI 上。
    //
    // `tokio::spawn` 只保證「可以並行」，不保證「同時開始」：先 spawn 的
    // 那個可能已經跑完整個請求，後面的才被排程。屏障把「開始」對齊，
    // 讓重疊窗口最大化。
    let gate = std::sync::Arc::new(tokio::sync::Barrier::new(reqs.len()));

    let mut handles = Vec::with_capacity(reqs.len());
    for req in reqs {
        let router = ctx.router_for_race();
        let gate = gate.clone();
        handles.push(tokio::spawn(async move {
            gate.wait().await;
            let res = router.oneshot(req).await.expect("router call");
            let status = res.status();
            let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
                .await
                .expect("body");
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }));
    }

    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.expect("task panicked"));
    }
    out
}

/// 統計每個狀態碼出現幾次，失敗訊息用得到。
fn tally(results: &[(StatusCode, Value)]) -> Vec<(u16, usize)> {
    let mut m = std::collections::BTreeMap::new();
    for (s, _) in results {
        *m.entry(s.as_u16()).or_insert(0) += 1;
    }
    m.into_iter().collect()
}

// =============================================================================

/// **同時搶同一時段：恰好一筆成立，落敗者沒有一個是 5xx。**
///
/// 這是這個切片的核心。雙重預約是這個系統最嚴重的功能性缺陷（兩群人走到
/// 同一間會議室），而「落敗者拿到 500」是最嚴重的介面缺陷 ——
/// 前端無法分辨「時段被搶走，請選別的」與「後端壞了，請重試」。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_only_one_of_six_simultaneous_bookings_wins_and_no_loser_gets_a_500() {
    let ctx = &wide_pool_ctx().await;
    let token = ctx.login_as(USERNAME).await;
    let resource_id = pick_resource(ctx).await;
    let (start, end) = tomorrow(10);

    // 六個**完全相同**的請求，刻意不帶 Idempotency-Key ——
    // 帶了的話冪等機制會先擋下來，而那是 `c_` 要驗的另一件事。
    let reqs: Vec<_> = (0..RACERS)
        .map(|_| {
            authed(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/reservations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "resource_id": resource_id,
                            "title": "搶同一個時段",
                            "start_at": start,
                            "end_at": end
                        })
                        .to_string(),
                    ))
                    .unwrap(),
                &token,
            )
        })
        .collect();

    let results = race(ctx, reqs).await;

    // **先驗這一格。** 「沒有 5xx」是介面契約，而它比「恰好一筆」更容易壞
    // （只對應 23P01 而漏掉 40P01 的實作，在低併發時完全正常）。
    let server_errors: Vec<_> = results
        .iter()
        .filter(|(s, _)| s.is_server_error())
        .collect();
    assert!(
        server_errors.is_empty(),
        "有 {} 個落敗者拿到 5xx —— 搶輸時段不是伺服器錯誤，\
         前端因此無法分辨「請選別的時段」與「後端壞了」。\
         狀態分佈：{:?}，第一個錯誤：{}",
        server_errors.len(),
        tally(&results),
        server_errors[0].1
    );

    let created: Vec<_> = results
        .iter()
        .filter(|(s, _)| *s == StatusCode::CREATED)
        .collect();
    assert_eq!(
        created.len(),
        1,
        "{RACERS} 路同時搶訂，成立了 {} 筆 —— 雙重預約。狀態分佈：{:?}",
        created.len(),
        tally(&results)
    );

    // 落敗者全部是 409，而語意必須是「時段衝突」而不是一般性衝突 ——
    // 前端要據此顯示「這個時段剛剛被訂走了」。
    for (status, body) in results.iter().filter(|(s, _)| *s != StatusCode::CREATED) {
        assert_eq!(*status, StatusCode::CONFLICT, "落敗者不是 409：{body}");
        assert_eq!(
            body["code"],
            json!("RESERVATION_CONFLICT"),
            "落敗者的 code 不是 RESERVATION_CONFLICT —— \
             前端無法區分「時段被搶」與其他 409：{body}"
        );
    }

    // 資料庫裡真的只有一筆。狀態碼對而資料庫有兩筆是可能的
    // （例如回應被誤判），因此這一格獨立驗。
    let mut tx = ctx.tenant_tx().await;
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.reservations
          WHERE resource_id = $1 AND start_at = $2::timestamptz
            AND status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN')",
    )
    .bind(resource_id)
    .bind(&start)
    .fetch_one(&mut *tx)
    .await
    .expect("count");
    drop(tx);
    assert_eq!(rows, 1, "資料庫裡有 {rows} 筆佔著同一時段");

    ctx.teardown().await;
}

/// **同時用同一個 refresh token 換發：恰好一次成功。**
///
/// 070 的換發是「撤銷舊的 + 發新的」。若那兩步不是原子的，兩個並發的
/// 換發都會成功 —— 於是撤銷鏈分岔成兩條，而 logout 只殺得掉其中一條。
///
/// 這一格驗的是 `refresh_grant` 裡「`!consumed` 代表有人先到」那條分支
/// **在真的併發下**成立，而不只是在循序測試裡成立。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn b_simultaneous_refresh_of_the_same_token_succeeds_exactly_once() {
    let ctx = &wide_pool_ctx().await;

    // 先拿一個 refresh token。
    let (status, body) = ctx
        .send(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/token")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"grant_type":"password","tenant_code":TENANT_CODE,
                           "username":USERNAME,"password":TEST_PASSWORD})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let refresh_token = body["refresh_token"].as_str().unwrap().to_string();

    let reqs: Vec<_> = (0..RACERS)
        .map(|_| {
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/token")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"grant_type":"refresh_token","refresh_token":refresh_token}).to_string(),
                ))
                .unwrap()
        })
        .collect();

    let results = race(ctx, reqs).await;

    let ok: Vec<_> = results
        .iter()
        .filter(|(s, _)| *s == StatusCode::OK)
        .collect();
    assert_eq!(
        ok.len(),
        1,
        "同一個 refresh token 被換發成功 {} 次 —— 撤銷鏈分岔了，\
         而 logout 只會殺掉其中一條。狀態分佈：{:?}",
        ok.len(),
        tally(&results)
    );

    // 落敗者是 401，不是 500。並發換發是**正常**的客戶端行為
    // （兩個分頁同時醒來），因此它不該產生伺服器錯誤。
    for (status, body) in results.iter().filter(|(s, _)| *s != StatusCode::OK) {
        assert_eq!(
            *status,
            StatusCode::UNAUTHORIZED,
            "並發換發的落敗者不是 401 —— 兩個分頁同時醒來是正常行為：{body}"
        );
    }

    // 而唯一成功的那個換出來的 token 真的可用。
    let new_refresh = ok[0].1["refresh_token"].as_str().unwrap();
    let (status, body) = ctx
        .send(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/token")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"grant_type":"refresh_token","refresh_token":new_refresh}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "併發換發之後，唯一成功的那個 token 竟然不能用：{body}"
    );

    ctx.teardown().await;
}

/// **同時用同一個 `Idempotency-Key` 建立：只建立一筆。**
///
/// 這是冪等機制存在的**唯一理由**：網路超時後客戶端重送，而那兩個請求可能
/// 同時在飛。若冪等只在循序重送時有效，它守不住它要守的那個情境。
///
/// 允許的回應有兩種：拿到第一次的回應（201），或 409 `IDEMPOTENCY_IN_PROGRESS`
/// （前一次還在處理中）。**不允許的是建立第二筆。**
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn c_the_same_idempotency_key_in_flight_creates_exactly_one_row() {
    let ctx = &wide_pool_ctx().await;
    let token = ctx.login_as(USERNAME).await;
    let resource_id = pick_resource(ctx).await;
    let (start, end) = tomorrow(13);
    let key = uuid::Uuid::new_v4().to_string();

    let reqs: Vec<_> = (0..RACERS)
        .map(|_| {
            authed_idem(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/reservations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "resource_id": resource_id,
                            "title": "同一個冪等鍵",
                            "start_at": start,
                            "end_at": end
                        })
                        .to_string(),
                    ))
                    .unwrap(),
                &token,
                &key,
            )
        })
        .collect();

    let results = race(ctx, reqs).await;

    assert!(
        results.iter().all(|(s, _)| !s.is_server_error()),
        "併發的冪等重送產生了 5xx：{:?}",
        tally(&results)
    );

    // 每一個回應都必須是「成立」或「前一次還在處理」，不能有其他。
    for (status, body) in &results {
        let acceptable = *status == StatusCode::CREATED
            || (*status == StatusCode::CONFLICT
                && body["code"] == json!("IDEMPOTENCY_IN_PROGRESS"));
        assert!(
            acceptable,
            "併發冪等重送拿到了預期外的回應 {status}：{body}"
        );
    }

    // **這才是重點：資料庫裡只有一筆。**
    let mut tx = ctx.tenant_tx().await;
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.reservations
          WHERE resource_id = $1 AND start_at = $2::timestamptz",
    )
    .bind(resource_id)
    .bind(&start)
    .fetch_one(&mut *tx)
    .await
    .expect("count");
    drop(tx);
    assert_eq!(
        rows,
        1,
        "同一個 Idempotency-Key 併發重送建立了 {rows} 筆 —— 冪等在併發下失效，\
         而那正是它唯一要守的情境。狀態分佈：{:?}",
        tally(&results)
    );

    ctx.teardown().await;
}

/// **同時以同一個 `If-Match` 版本更新：恰好一次成功。**
///
/// 樂觀鎖若不是原子的，兩個並發的 PATCH 都會看到 `version = 3` 而都寫入 ——
/// 後寫的那個默默覆蓋前一個（lost update），而**兩邊都收到 200**。
/// 那是最難察覺的一類缺陷：沒有錯誤、沒有日誌，只有一筆消失的修改。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn d_concurrent_patches_with_the_same_version_succeed_exactly_once() {
    let ctx = &wide_pool_ctx().await;
    let token = ctx.login_as(USERNAME).await;
    let resource_id = pick_resource(ctx).await;
    let (start, end) = tomorrow(16);

    let (status, created) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/reservations")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"resource_id": resource_id, "title": "原始標題",
                           "start_at": start, "end_at": end})
                    .to_string(),
                ))
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    let version = created["version"].as_i64().unwrap().to_string();

    // 六個請求，**同一個版本號**，各寫入不同的 party_size ——
    // 值不同才看得出「誰贏了」。
    // 多輪：每一輪重讀當下的版本再競爭。見 `ROUNDS` 的說明。
    let mut version = version;
    let mut results = Vec::new();
    for round in 0..ROUNDS {
        let reqs: Vec<_> = (0..RACERS)
            .map(|i| {
                authed_if_match(
                    Request::builder()
                        .method("PATCH")
                        .uri(format!("/api/v1/reservations/{id}"))
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"party_size": 10 + i + round * 100}).to_string(),
                        ))
                        .unwrap(),
                    &token,
                    &version,
                )
            })
            .collect();
        results = race(ctx, reqs).await;
        assert_exactly_one_winner(&results, &format!("reservations 第 {round} 輪"));
        version = current_version(ctx, &token, &format!("/api/v1/reservations/{id}"))
            .await
            .to_string();
    }

    // 最後一輪的贏家寫的值，就是資料庫裡的值。
    let winner_size = results
        .iter()
        .find(|(s, _)| *s == StatusCode::OK)
        .expect("每一輪都該有一個贏家")
        .1["party_size"]
        .as_i64()
        .unwrap();
    let mut tx = ctx.tenant_tx().await;
    let stored: i32 = sqlx::query_scalar("SELECT party_size FROM fms.reservations WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&id).unwrap())
        .fetch_one(&mut *tx)
        .await
        .expect("read");
    drop(tx);
    assert_eq!(
        stored as i64, winner_size,
        "資料庫裡的值不是回 200 的那個請求寫的 —— 有人在它之後寫進去了"
    );

    ctx.teardown().await;
}

/// 讀出某個資源目前的 `version`。
async fn current_version(ctx: &TestContext, token: &str, uri: &str) -> i64 {
    let (status, body) = ctx
        .send(authed(
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "讀不到 {uri}：{body}");
    body["version"]
        .as_i64()
        .unwrap_or_else(|| panic!("{uri} 的回應沒有 version：{body}"))
}

/// 對任一資源的 PATCH 做同版本併發，回傳成功次數與狀態分佈。
///
/// 抽出來是因為**列鎖加在三個 repo 上，就需要三個守衛** ——
/// 只驗預約的話，資產與工單那兩處的鎖是兩個未經驗證的宣稱。
/// 突變測試 N4（拿掉資產的鎖）在補這一格之前**沒有被任何測試抓到**。
/// 跑 `ROUNDS` 輪，每一輪都斷言「恰好一個贏家」。
///
/// 每輪之間重讀版本 —— 上一輪的贏家已經把版本推進了，用舊的會讓整輪
/// 全部 412 而什麼都驗不到。
async fn race_patches_repeatedly(
    ctx: &TestContext,
    token: &str,
    uri: &str,
    first_version: String,
    what: &str,
    body_of: impl Fn(usize, usize) -> Value,
) {
    let mut version = first_version;
    for round in 0..ROUNDS {
        let reqs: Vec<_> = (0..RACERS)
            .map(|i| {
                authed_if_match(
                    Request::builder()
                        .method("PATCH")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(body_of(i, round).to_string()))
                        .unwrap(),
                    token,
                    &version,
                )
            })
            .collect();
        let results = race(ctx, reqs).await;
        assert_exactly_one_winner(&results, &format!("{what} 第 {round} 輪"));
        version = current_version(ctx, token, uri).await.to_string();
    }
}

fn assert_exactly_one_winner(results: &[(StatusCode, Value)], what: &str) {
    assert!(
        results.iter().all(|(s, _)| !s.is_server_error()),
        "{what}：併發的樂觀鎖衝突產生了 5xx：{:?}",
        tally(results)
    );
    let ok = results.iter().filter(|(s, _)| *s == StatusCode::OK).count();
    assert_eq!(
        ok,
        1,
        "{what}：同一個版本號的 {RACERS} 個 PATCH 成功了 {ok} 次 —— lost update。\
         狀態分佈：{:?}",
        tally(results)
    );
    for (status, body) in results.iter().filter(|(s, _)| *s != StatusCode::OK) {
        assert_eq!(
            *status,
            StatusCode::PRECONDITION_FAILED,
            "{what}：落敗者不是 412：{body}"
        );
        assert_eq!(body["code"], json!("STALE_VERSION"), "{what}：{body}");
    }
}

/// **資產**的樂觀鎖在併發下也只能成功一次。
///
/// 與 `d_` 是同一個不變量、不同的 repo。分開寫而不是併進 `d_`：
/// 一格失敗時要能立刻看出是哪個資源少了鎖。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn e_assets_optimistic_lock_holds_under_concurrency() {
    let ctx = &wide_pool_ctx().await;
    let token = ctx.login_as(USERNAME).await;
    let asset_id = ctx.seed_asset(FACILITY_A, "CONC-AST-1").await;
    let uri = format!("/api/v1/assets/{asset_id}");
    let version = current_version(ctx, &token, &uri).await.to_string();

    race_patches_repeatedly(
        ctx,
        &token,
        &uri,
        version,
        "assets",
        |i, r| json!({ "name": format!("併發改名 {r}-{i}") }),
    )
    .await;

    ctx.teardown().await;
}

/// **工單**的樂觀鎖在併發下也只能成功一次。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn f_work_orders_optimistic_lock_holds_under_concurrency() {
    let ctx = &wide_pool_ctx().await;
    let token = ctx.login_as(USERNAME).await;
    let wo_id = ctx.seed_work_order(FACILITY_A, "併發測試用工單").await;
    let uri = format!("/api/v1/work-orders/{wo_id}");
    let version = current_version(ctx, &token, &uri).await.to_string();

    race_patches_repeatedly(
        ctx,
        &token,
        &uri,
        version,
        "work_orders",
        |i, r| json!({ "title": format!("併發改標題 {r}-{i}") }),
    )
    .await;

    ctx.teardown().await;
}
