//! 兩條連續業務路徑：**預約鏈**與 **IoT 鏈**。每一步用前一步的輸出當輸入。
//!
//! # 這裡驗的是接縫，不是功能
//!
//! `e2e_journey_slice.rs` 已經走過工單鏈（建人 → 指派角色 → 建工單 → 派工 →
//! 執行 → 完工 → SLA → 稽核 → 匯出）。**另外兩條主鏈沒有任何接縫測試。**
//!
//! 五十幾個切片各驗一段，而每一格都自己佈置資料 —— 因此「A 的輸出能不能當
//! B 的輸入」在這兩條路徑上從來沒有被走過。這個檔案不重驗單一端點的行為
//!（那些切片做得比這裡細），只驗銜接處。
//!
//! # 鏈一：預約 → 報到 → 佔用地圖 → 逾時釋放
//!
//! | 接縫 | 斷了會怎樣 |
//! |---|---|
//! | 建立回的 `id` 能當 check-in 的 path 參數 | 前端拿到 id 卻報不了到 |
//! | 報到之後**佔用地圖立刻**變 OCCUPIED | 牆面板顯示空房而裡面有人 |
//! | `auto_release_at` 真的被填 | no-show 掃描沒有東西可掃，整條機制是斷的 |
//! | 掃描把它標成 NO_SHOW **且時段被釋放** | 那個時段永遠訂不到 |
//! | 釋放之後同一時段**訂得起來** | 這是「釋放」的唯一可觀察定義 |
//!
//! 最後一項是這條鏈的重點。「狀態變成 NO_SHOW」是實作細節；
//! **「別人現在訂得到了」才是使用者感受到的那件事**，而它跨越了
//! reservations 的狀態機與 005 的排除約束兩個機制。
//!
//! # 鏈二：遙測 → 告警 → 工單 → 對帳
//!
//! | 接縫 | 斷了會怎樣 |
//! |---|---|
//! | 一筆超標讀數產生一筆告警（057 的即時評估） | IoT 接進來了但沒有人被通知 |
//! | 告警的 `asset_id` 從點位推導出來 | 工單開在錯的設備上，或開不出來 |
//! | 從告警開工單，兩邊互相指得到 | 工單看不出來源、告警看不出處置 |
//! | `reconcile-work-orders` 找得到「已開單但沒連上」的 | 那支端點永遠回 0，等於裝飾 |
//!
//! # 為什麼用 009 的規則而不是自己建
//!
//! `UPS_SOC_LOW`（`{"op": "<", "value": 40}`，`auto_create_work_order = true`）
//! 是**種子裡就有的**規則。自己在測試裡建一條規則會驗到「我建的規則能用」，
//! 而不是「示範環境開箱就能跑」—— 後者才是客戶第一天會遇到的。
//!
//! # 沒有被覆蓋的
//!
//! * **持續型門檻**（`FILTER_DP` 的 `for_seconds: 600`）—— 需要跨十分鐘的
//!   讀數序列。`telemetry_ingest_slice` 已經驗過那個分支，這裡不重做。
//! * **通知真的寄出** —— dispatcher 只寫資料庫與 Mailpit。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_A: &str = "cccccccc-0000-4000-8000-000000000001";
/// 009 的 UPS 電池電量點位。規則 `UPS_SOC_LOW`：`< 40` 且自動開工單。
const POINT_SOC: &str = "a3000000-0000-4000-8000-000000000003";

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// 把一筆預約的時段移到涵蓋此刻。
///
/// **只動 `start_at`／`end_at`**，狀態與其他欄位一律不碰 —— 這是 fixture 操作，
/// 不是繞過任何邏輯。需要它的原因見步驟 1 的說明（建立規則不接受過去的時段，
/// 而佔用地圖只看正在進行的預約）。
async fn shift_to_now(ctx: &TestContext, id: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.reservations
            SET start_at = clock_timestamp() - interval '5 minutes',
                end_at   = clock_timestamp() + interval '55 minutes'
          WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(id).expect("valid uuid"))
    .execute(&mut *tx)
    .await
    .expect("把時段移到涵蓋此刻");
    tx.commit().await.expect("commit");
}

// =============================================================================
// 鏈一：預約 → 報到 → 佔用地圖 → 逾時釋放
// =============================================================================

/// 一路走完，每一步用前一步的輸出。
#[tokio::test]
async fn a_reservation_chain_from_booking_to_no_show_release() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login_as(USERNAME).await;

    // ---- 前置：找一個需要報到的資源 ----
    //
    // `requires_check_in` 與 `auto_release_minutes` 都必須有值，否則
    // `auto_release_at` 不會被填，而 no-show 那一段就沒有東西可驗。
    // **這個前提由 009 提供**；不成立的話是種子的問題，因此明說。
    let (resource_id, release_minutes) = {
        let mut tx = ctx.owner_tx().await;
        let row: Option<(uuid::Uuid, i32)> = sqlx::query_as(
            "SELECT coalesce(br.spatial_node_id, br.asset_id), br.auto_release_minutes
               FROM fms.bookable_resources br
              WHERE br.facility_id = $1::uuid AND br.is_bookable
                AND br.requires_check_in AND br.auto_release_minutes IS NOT NULL
              ORDER BY br.display_name LIMIT 1",
        )
        .bind(FACILITY_A)
        .fetch_optional(&mut *tx)
        .await
        .expect("query");
        drop(tx);
        row.expect(
            "009 沒有任何『需要報到且設了釋放分鐘數』的資源 —— \
             no-show 那一段因此驗不到。這是種子的缺口，不是測試的",
        )
    };

    // ---- 步驟 1：建立預約 ----
    //
    // **必須訂在未來。** 011 的 `min_notice_minutes` 判定是
    // `start_at < now + min_notice`（第 1036 行），而 009 的資源設的是 0 分鐘 ——
    // 也就是「不能訂過去」。訂在過去會拿到 422 `TOO_LATE`
    //（訊息是「需提前 0 分鐘預約」，讀起來像沒有限制，實際上是「不能是過去」）。
    //
    // 而佔用地圖只看**正在進行**的預約 —— 兩個要求互相衝突。解法是照真實路徑
    // 建立（規則因此有被走過），再用 SQL 把時段移到涵蓋此刻。移動的是時間，
    // 不是狀態：步驟 2、3 的邏輯完全沒有被繞過。
    let now = chrono::Utc::now();
    let start = (now + chrono::Duration::minutes(30)).to_rfc3339();
    let end = (now + chrono::Duration::minutes(90)).to_rfc3339();

    let (status, created) = ctx
        .send(authed(
            post(
                "/api/v1/reservations",
                json!({"resource_id": resource_id, "title": "旅程測試",
                       "start_at": start, "end_at": end}),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "步驟 1 建立預約失敗：{created}"
    );
    let id = created["id"].as_str().expect("回應沒有 id").to_string();

    // **`auto_release_at` 必須已經被填。** 這是接縫：建立的那一刻要算出
    // no-show 的判定時點，而先前那一欄從未被填入 —— 等於整條機制是斷的
    // （repo.rs 的註解記著這件事）。
    assert!(
        !created["auto_release_at"].is_null(),
        "步驟 1：`auto_release_at` 是 null —— no-show 掃描會沒有東西可掃，\
         而那個斷裂完全不可觀察（資源的釋放分鐘數是 {release_minutes}）：{created}"
    );
    assert_eq!(
        created["requires_check_in"],
        json!(true),
        "步驟 1：資源要求報到，但預約沒有標記：{created}"
    );

    // 把時段往前移到涵蓋此刻，讓步驟 3 的佔用地圖看得到。只動兩個時間欄位。
    shift_to_now(ctx, &id).await;

    // ---- 步驟 2：報到 ----
    let (status, checked_in) = ctx
        .send(authed(
            post(
                &format!("/api/v1/reservations/{id}/check-in"),
                json!({"method": "QR"}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "步驟 2 報到失敗：{checked_in}");
    assert_eq!(checked_in["status"], json!("CHECKED_IN"), "{checked_in}");
    assert!(
        !checked_in["checked_in_at"].is_null(),
        "步驟 2：狀態變了但 `checked_in_at` 沒填 —— 「幾點報到的」查不到：{checked_in}"
    );

    // ---- 步驟 3：佔用地圖立刻反映 ----
    //
    // **這是最容易斷的接縫**：地圖是另一支查詢（不同的 JOIN、不同的狀態集合），
    // 而它與 check-in 之間沒有任何共用程式碼。
    let (status, occ) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_A}/occupancy")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "步驟 3：{occ}");
    let cell = occ["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["reservation_id"] == json!(id))
        .unwrap_or_else(|| {
            panic!("步驟 3：報到之後佔用地圖上找不到這筆預約 —— 牆面板會顯示空房：{occ}")
        });
    assert_eq!(
        cell["state"],
        json!("OCCUPIED"),
        "步驟 3：已報到卻不是 OCCUPIED（RESERVED 是「已訂未報到」）：{cell}"
    );

    // ---- 步驟 4：把時間推過釋放時點，讓掃描器有東西可掃 ----
    //
    // 直接改 `auto_release_at` 而不是等 —— 等 15 分鐘的測試沒有人會跑。
    // 改的是**判定時點本身**，不是狀態：掃描器的邏輯完全沒有被繞過。
    //
    // 同時把狀態退回 CONFIRMED：no-show 的定義是「該報到卻沒報到」，
    // 而步驟 2 已經報到了。這一步模擬的是**另一種結局**。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.reservations
                SET status = 'CONFIRMED', checked_in_at = NULL,
                    auto_release_at = clock_timestamp() - interval '1 minute'
              WHERE id = $1",
        )
        .bind(uuid::Uuid::parse_str(&id).unwrap())
        .execute(&mut *tx)
        .await
        .expect("推進時間");
        tx.commit().await.expect("commit");
    }

    // ---- 步驟 5：掃描器把它標成 NO_SHOW ----
    let scanner =
        fms_reservation::no_show::NoShowScanner::new(ctx.owner_pool().await, admin_user_id());
    let marked = scanner.run_once(100).await.expect("no-show 掃描");
    assert!(
        marked >= 1,
        "步驟 5：掃描器標記了 {marked} 筆 —— 逾時的那筆沒有被掃到"
    );

    let (status, after) = ctx
        .send(authed(get(&format!("/api/v1/reservations/{id}")), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(
        after["status"],
        json!("NO_SHOW"),
        "步驟 5：掃描器回報標記了，但那筆預約的狀態沒變：{after}"
    );

    // ---- 步驟 6：時段真的被釋放了 ----
    //
    // **這才是這條鏈的重點。** 「狀態是 NO_SHOW」是實作細節；
    // 「別人現在訂得到了」是使用者感受到的那件事，而它跨越了狀態機與
    // 005 的排除約束兩個機制 —— 只有 NO_SHOW 不在排除約束的狀態集合裡，
    // 這一步才會成功。
    //
    // 先把那筆移回原本的未來時段：步驟 1 之後它被移到涵蓋此刻，而
    // 「同一時段重訂」要真的是同一時段才驗得到排除約束。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.reservations
                SET start_at = $2::timestamptz, end_at = $3::timestamptz
              WHERE id = $1",
        )
        .bind(uuid::Uuid::parse_str(&id).unwrap())
        .bind(&start)
        .bind(&end)
        .execute(&mut *tx)
        .await
        .expect("移回原時段");
        tx.commit().await.expect("commit");
    }

    let (status, rebooked) = ctx
        .send(authed(
            post(
                "/api/v1/reservations",
                json!({"resource_id": resource_id, "title": "釋放後重訂",
                       "start_at": start, "end_at": end}),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "步驟 6：標成 NO_SHOW 之後同一時段還是訂不到 —— \
         「釋放」沒有真的發生，那個時段永遠被佔著：{rebooked}"
    );
    assert_ne!(
        rebooked["id"], created["id"],
        "步驟 6：回的是同一筆，不是新的預約"
    );

    ctx.teardown().await;
}

// =============================================================================
// 鏈二：遙測 → 告警 → 工單 → 對帳
// =============================================================================

#[tokio::test]
async fn b_iot_chain_from_a_reading_to_a_reconciled_work_order() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login_as(USERNAME).await;

    // ---- 步驟 1：送一筆超標讀數 ----
    //
    // `UPS_SOC_LOW` 是 `< 40`。送 18 —— 明確低於門檻，而不是壓在邊界上：
    // 邊界行為是 `telemetry_ingest_slice` 的事，這裡要的是「一定會觸發」。
    let (status, ingested) = ctx
        .send(authed(
            post(
                "/api/v1/telemetry:batch-ingest",
                // 欄位名是 `observed_at` 與 `value_num`（見 `telemetry.rs`
                // 的 `Reading`）—— 不是 `recorded_at`／`value`。
                // 打錯會拿到 422，而那個 422 是對的：契約要求逐筆處理，
                // 而一筆連時間都沒有的讀數無法歸屬到任何時序。
                json!({"readings": [{
                    "telemetry_point_id": POINT_SOC,
                    "value_num": 18.0,
                    "observed_at": chrono::Utc::now().to_rfc3339()
                }]}),
            ),
            &token,
        ))
        .await;
    assert!(
        status.is_success(),
        "步驟 1 遙測寫入失敗：{status} {ingested}"
    );

    // ---- 步驟 2：告警被建立 ----
    //
    // 057 的即時評估在寫入的同一個交易裡跑，因此這裡**不需要等** ——
    // 若要等，那本身就是一個缺陷（前端會看到「資料進去了但沒有告警」）。
    let (status, alarms) = ctx
        .send(authed(get("/api/v1/alarms?status=ACTIVE&limit=50"), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "步驟 2：{alarms}");
    // 取最新的那一筆 —— 剛才那次寫入產生的就是它。測試資料庫是這個測試獨占的，
    // 因此「最新」足夠精確；用 code 或 severity 篩會綁死 009 的規則細節。
    let alarm = alarms["data"]
        .as_array()
        .unwrap()
        .iter()
        .max_by_key(|a| a["first_seen_at"].as_str().unwrap_or("").to_string())
        .unwrap_or_else(|| {
            panic!("步驟 2：超標讀數沒有產生任何告警 —— IoT 接進來了但沒有人被通知：{alarms}")
        });

    let alarm_id = alarm["id"].as_str().expect("告警沒有 id").to_string();

    // **`asset_id` 從點位推導。** 告警規則綁的是點位，而工單要開在設備上 ——
    // 這一段推導斷了的話工單開不出來，或開在錯的設備上。
    assert!(
        !alarm["asset_id"].is_null(),
        "步驟 2：告警沒有 `asset_id` —— 從點位到設備的推導斷了，\
         下一步的工單會開在空氣上：{alarm}"
    );

    // ---- 步驟 3：規則**自動**開了工單 ----
    //
    // `UPS_SOC_LOW` 的 `auto_create_work_order = true`，因此工單在告警產生的
    // 同一刻就開好了 —— 不需要人按任何按鈕。**這才是 IoT 鏈的主線**：
    // 「設備出問題 → 有人被派去修」之間沒有人工步驟。
    //
    // 第一版把這一步寫成手動 `POST /alarms/{id}/work-order`，而它回了 409
    //「這個告警已經關聯了工單 —— 規則可能已經自動建過」。那個 409 的訊息
    // 本身就指出了真正的鏈長什麼樣子。
    //
    // 這一格順帶讓 `auto_create_work_order` 這個旗標有了行為驗證 ——
    // 在此之前沒有任何測試證明它真的被讀。
    let wo_id = alarm["work_order_id"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "步驟 3：告警沒有自動開工單 —— `auto_create_work_order` 沒有生效，\
                 於是設備壞了而沒有人被派去修：{alarm}"
            )
        })
        .to_string();

    // ---- 步驟 3b：手動再開會被擋，而且訊息說得出下一步 ----
    //
    // 重複開單的防護。**訊息裡要有可行動的指引** —— 值班的人看到 409 之後
    // 需要知道去哪裡找真正還沒串接的告警。
    let (status, dup) = ctx
        .send(authed(
            post(
                &format!("/api/v1/alarms/{alarm_id}/work-order"),
                json!({"priority": "HIGH", "title": "重複開單"}),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "步驟 3b：已經有工單的告警還能再開一張 —— 同一個問題會有兩張單：{dup}"
    );
    assert!(
        dup["detail"]
            .as_str()
            .unwrap_or("")
            .contains("unlinked_only"),
        "步驟 3b：409 沒有告訴值班的人去哪裡找真正沒串接的告警：{dup}"
    );

    // ---- 步驟 4：兩邊互相指得到 ----
    //
    // **雙向**都要驗。只驗一邊的話「工單看不出來源」或「告警看不出處置」
    // 其中一個會漏 —— 而那兩件事在現場是不同的人在問。
    let (status, alarm_after) = ctx
        .send(authed(get("/api/v1/alarms?limit=50"), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{alarm_after}");
    let linked = alarm_after["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == json!(alarm_id))
        .expect("找不到那筆告警");
    assert_eq!(
        linked["work_order_id"],
        json!(wo_id),
        "步驟 4：告警沒有指回工單 —— 值班的人看不出這個告警處置了沒有：{linked}"
    );

    let (status, wo_detail) = ctx
        .send(authed(get(&format!("/api/v1/work-orders/{wo_id}")), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{wo_detail}");
    assert_eq!(
        wo_detail["alarm_id"],
        json!(alarm_id),
        "步驟 4：工單沒有指回告警 —— 技師看不出這張單是為什麼開的：{wo_detail}"
    );
    assert_eq!(
        wo_detail["source"],
        json!("IOT_ALARM"),
        "步驟 4：來源不是 IOT_ALARM —— 報表分不出「反應性 vs IoT 驅動」：{wo_detail}"
    );

    // ---- 步驟 5：診斷 → 修復 ----
    //
    // 先人工把連結拆掉，模擬「規則說要自動建單，但那張單沒有建出來」
    // （自動建單失敗、或工單事後被刪）。那正是這一組端點存在的理由。
    //
    // **少了這一步，`unlinked_only=true` 永遠回空、對帳永遠回 0，
    // 而沒有人會發現它們壞了** —— 這是這個 repo 反覆出現的那一類缺陷。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("SELECT set_config('app.is_platform','on',true)")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE fms.alarms SET work_order_id = NULL WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&alarm_id).unwrap())
            .execute(&mut *tx)
            .await
            .expect("拆掉連結");
        tx.commit().await.expect("commit");
    }

    // 5a：**診斷** —— `unlinked_only=true` 要找得到它。
    let (status, unlinked) = ctx
        .send(authed(
            get("/api/v1/alarms?unlinked_only=true&limit=50"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "步驟 5a：{unlinked}");
    assert!(
        unlinked["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == json!(alarm_id)),
        "步驟 5a：`unlinked_only=true` 找不到那筆沒有工單的告警 —— \
         診斷面是空的，於是沒有人知道有缺口：{unlinked}"
    );

    // 5b：**修復** —— 對帳補一張工單上去。
    //
    // `facility_id` 是**必填**（見 `ReconcileBody` 的說明：跨場域批次補單
    // 沒有合理的權限上限）。第一版送了空物件，拿到 422 而 body 是 null。
    let (status, recon) = ctx
        .send(authed(
            post(
                "/api/v1/alarms:reconcile-work-orders",
                json!({"facility_id": FACILITY_A}),
            ),
            &token,
        ))
        .await;
    assert!(status.is_success(), "步驟 5b 對帳失敗：{status} {recon}");

    let (status, final_list) = ctx
        .send(authed(get("/api/v1/alarms?limit=50"), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{final_list}");
    let relinked = final_list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == json!(alarm_id))
        .expect("找不到那筆告警");

    // **對帳是「補開一張新的」，不是「接回舊的」**（見該端點的說明：它修的是
    // 「規則說要自動建單、但沒有工單」）。因此斷言的是「現在有工單了」，
    // 而不是「等於原來那一張」—— 後者會讓這一格在正確的實作上失敗。
    assert!(
        relinked["work_order_id"].is_string(),
        "步驟 5b：對帳回了成功，但那筆告警仍然沒有工單 —— \
         那支端點什麼都沒做：{recon} / {relinked}"
    );
    assert_ne!(
        relinked["work_order_id"],
        json!(wo_id),
        "步驟 5b：補的是原來那一張 —— 那張已經不存在於連結裡了，\
         對帳應該建一張新的"
    );

    // 5c：修復之後診斷面要變乾淨 —— 否則同一筆會被反覆補單。
    let (status, after_recon) = ctx
        .send(authed(
            get("/api/v1/alarms?unlinked_only=true&limit=50"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "步驟 5c：{after_recon}");
    assert!(
        !after_recon["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == json!(alarm_id)),
        "步驟 5c：補完單之後它還在 unlinked 清單裡 —— \
         下一輪對帳會再補一張，同一個問題會累積出很多工單：{after_recon}"
    );

    ctx.teardown().await;
}
