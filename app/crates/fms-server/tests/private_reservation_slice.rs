//! 私人預約的遮罩。
//!
//! # 這個切片修的是什麼
//!
//! `sql/011` 為 `reservations.is_private` 寫下的規格是：
//!
//! > 私人預約：非本人／非管理員只看得到「已預約」與時段，看不到標題、
//! > 與會者與備註。**由 API 層遮罩**，並在 audit_log 記錄任何越權查看嘗試。
//!
//! 而在此之前 **API 層完全沒有讀那一欄** —— `is_private` 在整個 Rust 樹裡
//! 只出現在 `safe_http.rs` 的 `Ipv4Addr::is_private()`（完全無關）。
//! 也就是說使用者把預約標記為私人之後，標題、備註與主辦人姓名對所有持有
//! `reservation:read` 的人照樣全開，包含**牆面板上的佔用地圖**。
//! `reservation:view_private` 是一個已經被四個管理角色持有、卻守不住任何
//! 東西的權限。
//!
//! 這個缺陷不是任何測試找到的 —— 446 格都沒抓到它，因為測試驗的是
//! 「我們想到要做的事」，而不是「schema 承諾過的事」。它是在盤查
//! 「宣告了但沒有人讀」時掉出來的。
//!
//! # 三條洩漏路徑，因此三組斷言
//!
//! | 端點 | 洩漏的欄位 |
//! |---|---|
//! | `GET /reservations`、`GET /reservations/{id}` | `title`、`purpose`、`organizer` |
//! | `GET /facilities/{id}/occupancy` | `title`、`organizer_name` |
//! | `POST /resource-blackouts` 的 409 衝突清單 | `title` |
//!
//! 只修第一條等於把洩漏搬家：牆面板才是最多人看得到的那一面。
//!
//! # 測試用的權限組合是構造出來的
//!
//! 需要「有 `reservation:read`、但沒有 `reservation:view_private`」的人。
//! 009 沒有這種使用者 —— 四個管理角色兩者都有，REQUESTER 只有 `read_own`。
//! 平台的 **VIEWER** 角色正好是那個組合（讀全部、不含私人），因此測試把它
//! 指派給既有的 `user.huang`。與 `blackouts_slice.rs` 的 `f_` 同一個手法。
//!
//! # 沒有實作的那一半，寫在這裡而不是假裝沒說過
//!
//! 011 還說「在 audit_log 記錄任何越權查看嘗試」。**刻意沒做。**
//! 遮罩發生在每一次清單讀取的每一列上 —— 一個看板每 30 秒輪詢一次
//! 佔用地圖，一天就是數十萬列稽核。那會讓 audit_log 對真正該查的事沉默，
//! 而「稽核軌被噪音淹沒」與「沒有稽核軌」的實際效果相同。
//!
//! 客戶端拿到的訊號是每一列的 `is_private: true` —— 它知道自己看到的是
//! 遮罩後的內容，這是誠實的，只是不進稽核軌。
//!
//! # 寫入面：`f_` 與 `g_`
//!
//! 遮罩上線的第一版**沒有寫入面** —— `ReservationCreate` 沒有 `is_private`，
//! 於是那段遮罩程式碼在生產環境沒有觸發路徑。`f_` 守住「使用者真的可以從
//! API 把預約設成私人」。
//!
//! `g_` 守的是補寫入面時開出來的洞：PATCH 對非主辦人只要求
//! `reservation:update`，而把旗標關掉就等於揭露內容 —— 一條完整的提權路徑。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 台北總部。與 `blackouts_slice.rs` 同一個常數（`common/mod.rs` 沒有匯出它）。
const FACILITY_A: &str = "cccccccc-0000-4000-8000-000000000001";

/// 把平台的 VIEWER 角色指派給 `user.huang`（場域範圍）。
///
/// VIEWER = `reservation:read` + `read_own`，**沒有** `view_private`。
async fn grant_viewer(ctx: &TestContext, username: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.user_role_assignments
                (tenant_id, user_id, role_id, scope_type, scope_id, granted_by)
         SELECT $1::uuid, u.id, r.id, 'FACILITY', $2::uuid, u.id
           FROM fms.users u
           CROSS JOIN fms.roles r
          WHERE u.tenant_id = $1::uuid AND u.username = $3
            AND r.code = 'VIEWER' AND r.tenant_id IS NULL
         ON CONFLICT DO NOTHING",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_A)
    .bind(username)
    .execute(&mut *tx)
    .await
    .expect("assign VIEWER");
    tx.commit().await.expect("commit");
}

fn tomorrow(hour: u32, hours: i64) -> (String, String) {
    let base = (chrono::Utc::now() + chrono::Duration::days(1))
        .date_naive()
        .and_hms_opt(hour, 0, 0)
        .expect("valid time")
        .and_utc();
    (
        base.to_rfc3339(),
        (base + chrono::Duration::hours(hours)).to_rfc3339(),
    )
}

/// 直接以 SQL 建一筆預約。
///
/// **不是因為 API 做不到** —— `POST /reservations` 已經接受 `is_private`
/// （見 `f_`）。用 SQL 是為了讓 `a_`～`e_` 能指定**任意主辦人**：
/// API 的建立者必然是主辦人，而那幾格要驗的正是「讀取者不是主辦人」。
async fn seed_private_reservation(
    ctx: &TestContext,
    organizer_username: &str,
    is_private: bool,
    status: &str,
) -> uuid::Uuid {
    let (start, end) = tomorrow(if is_private { 9 } else { 14 }, 1);
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        // `reservation_no` 沒有預設值 —— 正式路徑走 `fms.next_document_no()`
        // （repo.rs 的檔頭記著「不在應用層編號」），這裡照樣用它，
        // 否則造出來的列與真實資料形狀不同。
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, resource_type, resource_id, bookable_resource_id,
            reservation_no, organizer_id, title, purpose, party_size, start_at, end_at,
            status, is_private, created_via)
         SELECT $1::uuid, $2::uuid, br.resource_type,
                coalesce(br.spatial_node_id, br.asset_id), br.id,
                fms.next_document_no($1::uuid, 'RESERVATION'),
                u.id, $4, $5, 4, $6::timestamptz, $7::timestamptz, $8, $9, 'WEB'
           FROM fms.bookable_resources br
           CROSS JOIN fms.users u
          WHERE br.facility_id = $2::uuid AND br.is_bookable
            AND u.username = $3::citext AND u.tenant_id = $1::uuid
          ORDER BY br.display_name
          LIMIT 1
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_A)
    .bind(organizer_username)
    .bind(if is_private {
        "併購案討論"
    } else {
        "週會"
    })
    .bind(if is_private {
        "對象：目標公司財務"
    } else {
        "例行事項"
    })
    .bind(&start)
    .bind(&end)
    .bind(status)
    .bind(is_private)
    .fetch_one(&mut *tx)
    .await
    .expect("seed reservation");
    tx.commit().await.expect("commit");
    id
}

async fn get_reservation(ctx: &TestContext, token: &str, id: uuid::Uuid) -> (StatusCode, Value) {
    ctx.send(authed(
        Request::builder()
            .uri(format!("/api/v1/reservations/{id}"))
            .body(Body::empty())
            .unwrap(),
        token,
    ))
    .await
}

async fn list_reservations(ctx: &TestContext, token: &str) -> (StatusCode, Value) {
    ctx.send(authed(
        Request::builder()
            .uri(format!(
                "/api/v1/reservations?facility_id={FACILITY_A}&limit=200"
            ))
            .body(Body::empty())
            .unwrap(),
        token,
    ))
    .await
}

async fn occupancy(ctx: &TestContext, token: &str) -> (StatusCode, Value) {
    ctx.send(authed(
        Request::builder()
            .uri(format!("/api/v1/facilities/{FACILITY_A}/occupancy"))
            .body(Body::empty())
            .unwrap(),
        token,
    ))
    .await
}

// =============================================================================

/// 核心：有 `reservation:read` 但沒有 `view_private` 的人看不到標題、備註與主辦人。
#[tokio::test]
async fn a_a_reader_without_view_private_sees_only_the_time_slot() {
    let ctx = &TestContext::setup().await;
    grant_viewer(ctx, USERNAME_REQUESTER).await;

    // 主辦人是 tech.wang（不是讀取者本人，也不是管理員）。
    let id = seed_private_reservation(ctx, "tech.wang", true, "CONFIRMED").await;

    let viewer = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = get_reservation(ctx, &viewer, id).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(
        body["is_private"],
        json!(true),
        "回應沒有 is_private —— 客戶端無從知道該渲染「已預約」：{body}"
    );
    assert!(
        body["title"].is_null(),
        "私人預約的標題洩漏了：{}",
        body["title"]
    );
    assert!(
        body["purpose"].is_null(),
        "私人預約的備註洩漏了：{}",
        body["purpose"]
    );
    assert!(
        body["organizer"].is_null(),
        "私人預約的主辦人洩漏了 —— 保留 id 也不行，那可以拿去 GET /users/{{id}} \
         換回姓名：{}",
        body["organizer"]
    );

    // **時段與狀態必須留著。** 那正是 011 說「看得到」的部分，
    // 也是這個遮罩可用的前提：看不到時段的行事曆沒有意義。
    assert!(!body["start_at"].is_null(), "時段被遮掉了：{body}");
    assert!(!body["end_at"].is_null(), "時段被遮掉了：{body}");
    assert_eq!(body["status"], json!("CONFIRMED"), "{body}");
    assert!(
        !body["resource_name"].as_str().unwrap_or("").is_empty(),
        "資源名稱被遮掉了 —— 那樣連「哪一間會議室被訂了」都看不到：{body}"
    );

    // 清單路徑要一致。兩條路徑各自遮罩，其中一條漏掉就等於沒遮。
    let (status, body) = list_reservations(ctx, &viewer).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(id.to_string()))
        .unwrap_or_else(|| panic!("清單裡找不到那筆預約：{body}"));
    assert!(row["title"].is_null(), "清單路徑沒有遮罩：{row}");
    assert!(row["organizer"].is_null(), "清單路徑沒有遮罩主辦人：{row}");
    assert_eq!(row["is_private"], json!(true), "{row}");

    ctx.teardown().await;
}

/// 反面：**非私人**的預約不能被遮。
///
/// 少了這一格，一個「一律回 null」的實作也會讓 `a_` 全綠 ——
/// 那個實作會讓整個行事曆變成空白，而 `a_` 說不出差別。
#[tokio::test]
async fn b_a_normal_reservation_is_not_masked() {
    let ctx = &TestContext::setup().await;
    grant_viewer(ctx, USERNAME_REQUESTER).await;
    let id = seed_private_reservation(ctx, "tech.wang", false, "CONFIRMED").await;

    let viewer = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = get_reservation(ctx, &viewer, id).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(body["is_private"], json!(false), "{body}");
    assert_eq!(body["title"], json!("週會"), "非私人預約被遮罩了：{body}");
    assert_eq!(body["purpose"], json!("例行事項"), "{body}");
    assert!(
        !body["organizer"].is_null() && !body["organizer"]["display_name"].is_null(),
        "非私人預約的主辦人被遮罩了：{body}"
    );

    ctx.teardown().await;
}

/// 持有 `reservation:view_private` 的人看得到全部；主辦人本人也看得到。
///
/// 這一格守的是「遮罩不是一刀切」。只驗 `a_` 的話，一個把所有私人預約
/// 對所有人遮掉的實作也會過 —— 而那會讓管理者無法處理衝突，
/// 也讓使用者看不到自己訂的會議。
#[tokio::test]
async fn c_admins_and_the_organizer_see_everything() {
    let ctx = &TestContext::setup().await;
    // 主辦人用 `user.huang`（REQUESTER：只有 `reservation:read_own`，**沒有**
    // `view_private`）。這個選擇讓後半段的斷言更強：他看得到不是因為權限，
    // 而是因為那筆預約是他的。
    //
    // 不用 `tech.wang`：他不在 `TEST_USERS` 裡，密碼沒有被設成測試密碼，
    // 登入會回 401（第一版就是那樣寫的）。
    let id = seed_private_reservation(ctx, USERNAME_REQUESTER, true, "CONFIRMED").await;

    // TENANT_ADMIN 持有 view_private。
    let admin = ctx.login_as(USERNAME).await;
    let (status, body) = get_reservation(ctx, &admin, id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["title"],
        json!("併購案討論"),
        "持有 view_private 的人被遮罩了 —— 那讓這個權限碼變成裝飾：{body}"
    );
    assert_eq!(body["purpose"], json!("對象：目標公司財務"), "{body}");
    assert!(
        !body["organizer"]["display_name"].is_null(),
        "持有 view_private 的人看不到主辦人姓名：{body}"
    );
    // 即使看得到內容，旗標仍要回傳 —— UI 要能標示「這是私人預約」。
    assert_eq!(body["is_private"], json!(true), "{body}");

    // 主辦人本人：REQUESTER 沒有 view_private，但這筆是他的。
    let owner = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = get_reservation(ctx, &owner, id).await;
    assert_eq!(status, StatusCode::OK, "主辦人讀不到自己的預約：{body}");
    assert_eq!(
        body["title"],
        json!("併購案討論"),
        "主辦人看不到自己訂的會議標題：{body}"
    );

    ctx.teardown().await;
}

/// 佔用地圖 —— 這是缺陷最顯眼的一面：牆面板與樓層圖的資料來源。
#[tokio::test]
async fn d_the_occupancy_map_masks_too() {
    let ctx = &TestContext::setup().await;
    grant_viewer(ctx, USERNAME_REQUESTER).await;

    // 佔用地圖只看**此刻**正在進行的預約，因此時段要涵蓋現在。
    let now = chrono::Utc::now();
    let id = seed_private_reservation(ctx, "tech.wang", true, "CHECKED_IN").await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.reservations
                SET start_at = $2::timestamptz - interval '30 minutes',
                    end_at   = $2::timestamptz + interval '30 minutes'
              WHERE id = $1",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await
        .expect("shift到現在");
        tx.commit().await.expect("commit");
    }

    let viewer = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = occupancy(ctx, &viewer).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cell = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["reservation_id"] == json!(id.to_string()))
        .unwrap_or_else(|| panic!("佔用地圖沒有那一格 —— 這一格因此驗不到任何東西：{body}"));

    assert_eq!(
        cell["state"],
        json!("OCCUPIED"),
        "狀態被遮掉了 —— 那正是規格說看得到的部分：{cell}"
    );
    assert!(
        cell["title"].is_null(),
        "牆面板顯示了私人會議的標題：{cell}"
    );
    assert!(
        cell["organizer_name"].is_null(),
        "牆面板顯示了私人會議的主辦人姓名：{cell}"
    );
    assert_eq!(cell["is_private"], json!(true), "{cell}");
    assert!(!cell["start_at"].is_null(), "時段被遮掉了：{cell}");

    // 管理員看得到。
    let admin = ctx.login_as(USERNAME).await;
    let (status, body) = occupancy(ctx, &admin).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cell = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["reservation_id"] == json!(id.to_string()))
        .unwrap();
    assert_eq!(
        cell["title"],
        json!("併購案討論"),
        "持有 view_private 的人在地圖上也被遮罩了：{cell}"
    );

    ctx.teardown().await;
}

/// 封鎖時段的 409 衝突清單。
///
/// **「能建封鎖」不等於「能看私人會議的標題」。** 這一格驗的是那個判定
/// 走的是權限資料，而不是「blackout 寫入者一律看得見」的硬編碼例外 ——
/// 需要看的人把 `reservation:view_private` 加進角色即可，那是設定的事。
#[tokio::test]
async fn e_the_blackout_conflict_list_masks_the_title() {
    let ctx = &TestContext::setup().await;

    // 需要「有 blackout:write、但沒有 view_private」的人。009 沒有這種組合
    // （blackout:write 的四個持有者全部也持有 view_private），因此在測試裡
    // 建一個租戶角色：blackout:write + reservation:read，不含 view_private。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "WITH r AS (
               INSERT INTO fms.roles (tenant_id, code, name, is_system)
               VALUES ($1::uuid, 'BLACKOUT_ONLY', '只能封鎖時段', false)
               RETURNING id
             ), p AS (
               INSERT INTO fms.role_permissions (role_id, permission_code)
               SELECT r.id, c FROM r
                 CROSS JOIN unnest(ARRAY['blackout:write','reservation:read']) AS c
               RETURNING role_id
             )
             INSERT INTO fms.user_role_assignments
                    (tenant_id, user_id, role_id, scope_type, scope_id, granted_by)
             SELECT $1::uuid, u.id, (SELECT id FROM r), 'FACILITY', $2::uuid, u.id
               FROM fms.users u
              WHERE u.tenant_id = $1::uuid AND u.username = $3::citext",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_A)
        .bind(USERNAME_REQUESTER)
        .execute(&mut *tx)
        .await
        .expect("建 BLACKOUT_ONLY 角色");
        tx.commit().await.expect("commit");
    }

    let id = seed_private_reservation(ctx, "tech.wang", true, "CONFIRMED").await;
    let (start, end) = tomorrow(9, 1); // 與 seed 的私人預約同一時段

    let caller = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/resource-blackouts")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "facility_id": FACILITY_A,
                        "start_at": start,
                        "end_at": end,
                        "reason": "設備維護"
                    })
                    .to_string(),
                ))
                .unwrap(),
            &caller,
        ))
        .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "應該回 409 並列出衝突的預約：{body}"
    );
    let listed = body["errors"]
        .as_array()
        .map(|a| serde_json::to_string(a).unwrap())
        .unwrap_or_else(|| body.to_string());
    assert!(
        listed.contains(&id.to_string()),
        "409 沒有列出那筆衝突的預約，這一格因此驗不到遮罩：{body}"
    );
    assert!(
        !listed.contains("併購案討論"),
        "封鎖時段的 409 洩漏了私人會議的標題：{body}"
    );
    // organizer_id **刻意沒有遮** —— 409 的用途是「你要去通知這些人」，
    // 少了它整個回應就沒有可行動的資訊。
    assert!(
        listed.contains("organizer_id"),
        "409 連 organizer_id 都遮掉了 —— 那讓操作者不知道要通知誰：{body}"
    );

    ctx.teardown().await;
}

/// 寫入面：使用者真的可以從 API 把預約設成私人，而別人看到的是遮罩。
///
/// 遮罩上線的第一版沒有這一段 —— `ReservationCreate` 沒有 `is_private`，
/// 於是遮罩在生產環境**沒有觸發路徑**：程式碼在，但沒有任何請求能讓它生效。
/// 這一格是那個缺口的守衛。
#[tokio::test]
async fn f_a_user_can_mark_their_own_reservation_private_through_the_api() {
    let ctx = &TestContext::setup().await;
    grant_viewer(ctx, USERNAME_REQUESTER).await;

    // 主辦人：admin.chen（TENANT_ADMIN，可以建立）。
    let owner = ctx.login_as(USERNAME).await;
    let (start, end) = tomorrow(11, 1);
    let resource_id = {
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
        id
    };

    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/reservations")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "resource_id": resource_id,
                        "title": "薪酬檢討",
                        "purpose": "個別討論",
                        "start_at": start,
                        "end_at": end,
                        "is_private": true
                    })
                    .to_string(),
                ))
                .unwrap(),
            &owner,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "`POST /reservations` 不接受 is_private —— 遮罩因此沒有觸發路徑：{body}"
    );
    assert_eq!(
        body["is_private"],
        json!(true),
        "建立時帶了 is_private:true 卻沒有寫進去：{body}"
    );
    // 建立者是主辦人，因此他自己看得到內容。
    assert_eq!(body["title"], json!("薪酬檢討"), "主辦人被遮罩了：{body}");

    let id: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();

    // 而 VIEWER（有 read、無 view_private、不是主辦人）看到的是遮罩。
    let viewer = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, seen) = get_reservation(ctx, &viewer, id).await;
    assert_eq!(status, StatusCode::OK, "{seen}");
    assert!(
        seen["title"].is_null() && seen["purpose"].is_null() && seen["organizer"].is_null(),
        "從 API 建立的私人預約沒有被遮罩 —— 寫入面與讀取面沒有接上：{seen}"
    );

    // 未帶 is_private 時預設不私人。
    let (start2, end2) = tomorrow(15, 1);
    let (status, plain) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/reservations")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "resource_id": resource_id,
                        "title": "公開會議",
                        "start_at": start2,
                        "end_at": end2
                    })
                    .to_string(),
                ))
                .unwrap(),
            &owner,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{plain}");
    assert_eq!(
        plain["is_private"],
        json!(false),
        "沒帶 is_private 卻變成私人 —— 預設值錯了：{plain}"
    );

    // **事後把公開的預約改成私人。** 這一格是突變測試 W4 補上的：
    // 把 repo 的寫入改成 `coalesce($7, is_private) AND is_private`（只能關、
    // 不能開）時，前面所有斷言都還是綠的 —— 因為 `g_` 只驗了 true→false，
    // 而使用者「訂完才想到要設為私人」是完全正常的流程。
    // 那個突變的症狀是 **200 而旗標沒變**，沒有任何錯誤。
    let plain_id = plain["id"].as_str().unwrap();
    let (status, body) = ctx
        .send(authed_if_match(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/reservations/{plain_id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"is_private": true}).to_string()))
                .unwrap(),
            &owner,
            &plain["version"].as_i64().unwrap().to_string(),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["is_private"],
        json!(true),
        "PATCH 沒有把旗標打開 —— 回了 200 但什麼都沒改：{body}"
    );

    // 而它現在對 VIEWER 是遮罩的（讀取面立刻跟上，不需要重建任何東西）。
    let (status, seen) = get_reservation(ctx, &viewer, plain_id.parse().unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{seen}");
    assert!(
        seen["title"].is_null(),
        "事後改成私人之後仍然看得到標題：{seen}"
    );

    ctx.teardown().await;
}

/// **提權防護**：有 `reservation:update` 但沒有 `view_private` 的人
/// 不能把別人的私人預約改成公開。
///
/// 這是補寫入面時開出來的洞。PATCH 對非主辦人只要求 `reservation:update`，
/// 而把 `is_private` 關掉之後再讀一次就拿到標題 —— 整個遮罩被繞過。
///
/// 少了這一格，`f_` 全綠而系統仍然是可以被繞過的。
#[tokio::test]
async fn g_flipping_the_flag_off_requires_more_than_update_permission() {
    let ctx = &TestContext::setup().await;

    // `user.huang` 拿到一個「有 reservation:read + update、但沒有 view_private」
    // 的租戶角色。009 沒有這種組合（四個管理角色都持有 view_private）。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "WITH r AS (
               INSERT INTO fms.roles (tenant_id, code, name, is_system)
               VALUES ($1::uuid, 'RSV_EDITOR', '可改預約但看不到私人', false)
               RETURNING id
             ), p AS (
               INSERT INTO fms.role_permissions (role_id, permission_code)
               SELECT r.id, c FROM r
                 CROSS JOIN unnest(ARRAY['reservation:read','reservation:update']) AS c
               RETURNING role_id
             )
             INSERT INTO fms.user_role_assignments
                    (tenant_id, user_id, role_id, scope_type, scope_id, granted_by)
             SELECT $1::uuid, u.id, (SELECT id FROM r), 'FACILITY', $2::uuid, u.id
               FROM fms.users u
              WHERE u.tenant_id = $1::uuid AND u.username = $3::citext",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_A)
        .bind(USERNAME_REQUESTER)
        .execute(&mut *tx)
        .await
        .expect("建 RSV_EDITOR 角色");
        tx.commit().await.expect("commit");
    }

    let id = seed_private_reservation(ctx, "tech.wang", true, "CONFIRMED").await;
    let editor = ctx.login_as(USERNAME_REQUESTER).await;

    // 先確認他真的看得到那筆（遮罩後），否則下面驗不到 PATCH 這條路。
    let (status, seen) = get_reservation(ctx, &editor, id).await;
    assert_eq!(status, StatusCode::OK, "{seen}");
    assert!(
        seen["title"].is_null(),
        "前提不成立：他不該看到標題：{seen}"
    );
    let version = seen["version"].as_i64().unwrap();

    // 試著把旗標關掉。
    let (status, body) = ctx
        .send(authed_if_match(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/reservations/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"is_private": false}).to_string()))
                .unwrap(),
            &editor,
            &version.to_string(),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "有 update 權限就能把別人的私人預約公開 —— 那是一條繞過遮罩的路：{body}"
    );

    // 資料庫裡的旗標沒有被改動。
    let mut tx = ctx.tenant_tx().await;
    let still_private: bool =
        sqlx::query_scalar("SELECT is_private FROM fms.reservations WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .expect("read flag");
    drop(tx);
    assert!(still_private, "被拒絕的 PATCH 仍然改掉了旗標");

    // 反面：**同一個人改標題是可以的**（他有 update 權限）。
    // 少了這一格，一個「PATCH 一律 403」的實作也會讓上面全綠。
    let (status, body) = ctx
        .send(authed_if_match(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/reservations/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"party_size": 9}).to_string()))
                .unwrap(),
            &editor,
            &version.to_string(),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "擋過頭了 —— 只有 is_private 該受這道額外的閘門：{body}"
    );
    assert_eq!(body["party_size"], json!(9), "{body}");
    // 而回應仍然是遮罩後的（寫入路徑同一條規則）。
    assert!(
        body["title"].is_null(),
        "PATCH 的回應洩漏了私人預約的標題：{body}"
    );

    // 主辦人本人可以關掉它。
    let owner_admin = ctx.login_as(USERNAME).await; // TENANT_ADMIN 持有 view_private
    let (status, latest) = get_reservation(ctx, &owner_admin, id).await;
    assert_eq!(status, StatusCode::OK, "{latest}");
    let (status, body) = ctx
        .send(authed_if_match(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/reservations/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"is_private": false}).to_string()))
                .unwrap(),
            &owner_admin,
            &latest["version"].as_i64().unwrap().to_string(),
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "持有 view_private 的人也改不動旗標 —— 那道閘門擋過頭了：{body}"
    );
    assert_eq!(body["is_private"], json!(false), "{body}");

    ctx.teardown().await;
}
