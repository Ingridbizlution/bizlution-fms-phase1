//! `GET /resource-blackouts`、`POST /resource-blackouts`，以及被它們帶出來的
//! 一個潛伏缺陷。
//!
//! # `e_` 是這一組最重要的一格
//!
//! 可用性查詢對封鎖時段用的是 `JOIN fms.bookable_resources`（**內連接**），
//! 因此 `bookable_resource_id IS NULL` 的**全場域封鎖**在可用性視圖裡看不到 ——
//! 而 011 的 `check_resource_availability()` 會擋這種列
//! （`OR (b.bookable_resource_id IS NULL AND b.facility_id = ...)`）。
//!
//! 症狀：日曆顯示可預約 → 使用者選了 → 送出得到衝突，而衝突指向一個他在畫面上
//! 看不到的封鎖時段。
//!
//! 這個缺陷在 `POST /resource-blackouts` 之前是**潛伏的**（沒有端點建得出那種
//! 列），所以補寫入端點的同一刀必須修它，`e_` 是那個修正的守門人。
//!
//! # `c_` 守的是「不能默默建立」
//!
//! 封鎖時段不會取消既有預約。蓋在三筆已確認預約上而回 201，等於讓那三個人
//! 當天到一間關著的房間前面 —— 而系統從頭到尾沒說過任何話。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 009 的示範可預約資源（HQ 的兩間會議室）。
const RESOURCE_A: &str = "70000000-0000-4000-8000-000000000001";
const FACILITY_A: &str = "cccccccc-0000-4000-8000-000000000001";

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

/// 明天的一段時間，避開「不能訂過去」之類的規則。
fn tomorrow(hour: u32, plus_hours: i64) -> (String, String) {
    let base = (chrono::Utc::now() + chrono::Duration::days(1))
        .date_naive()
        .and_hms_opt(hour, 0, 0)
        .unwrap()
        .and_utc();
    (
        base.to_rfc3339(),
        (base + chrono::Duration::hours(plus_hours)).to_rfc3339(),
    )
}

async fn create_blackout(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(post("/api/v1/resource-blackouts", body), token))
        .await
}

async fn list_blackouts(ctx: &TestContext, token: &str, query: &str) -> (StatusCode, Value) {
    ctx.send(authed(
        get(&format!("/api/v1/resource-blackouts{query}")),
        token,
    ))
    .await
}

/// 直接插一筆預約 —— 走 API 要過審核、通知門檻等一堆規則，而這一組測的是封鎖，
/// 不是預約流程。狀態集合抄 005 的排除約束：只有這幾個真的佔著時段。
async fn insert_reservation(ctx: &TestContext, start: &str, end: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, reservation_no, bookable_resource_id, resource_id,
            resource_type, title, start_at, end_at, status, organizer_id, created_via)
         SELECT $1::uuid, br.facility_id,
                fms.next_document_no($1::uuid, 'RESERVATION', 'RS'),
                br.id, coalesce(br.spatial_node_id, br.asset_id), br.resource_type,
                '既有會議', $2::timestamptz, $3::timestamptz, 'CONFIRMED',
                (SELECT id FROM fms.users WHERE tenant_id = $1::uuid ORDER BY id LIMIT 1),
                'WEB'
           FROM fms.bookable_resources br WHERE br.id = $4::uuid
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(start)
    .bind(end)
    .bind(RESOURCE_A)
    .fetch_one(&mut *tx)
    .await
    .expect("插入既有預約");
    tx.commit().await.expect("commit");
    id.to_string()
}

/// 可用性查詢在這段時間回報的忙碌區間種類。
async fn busy_kinds(ctx: &TestContext, token: &str, from: &str, to: &str) -> Vec<String> {
    let uri = format!(
        "/api/v1/facilities/{FACILITY_A}/availability?from={}&to={}",
        urlencoding(from),
        urlencoding(to)
    );
    let (status, body) = ctx.send(authed(get(&uri), token)).await;
    assert_eq!(status, StatusCode::OK, "availability 失敗：{body}");
    // 回應的形狀由 availability 端點決定；這裡只把所有出現過的 kind 收集起來。
    let mut kinds = Vec::new();
    collect_kinds(&body, &mut kinds);
    kinds
}

fn collect_kinds(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                if k == "kind" {
                    if let Some(s) = val.as_str() {
                        out.push(s.to_string());
                    }
                }
                collect_kinds(val, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_kinds(x, out)),
        _ => {}
    }
}

fn urlencoding(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}

// =============================================================================

/// 建立與列出，而且 `resource_name` 帶得出來。
#[tokio::test]
async fn a_create_and_list() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (start, end) = tomorrow(9, 3);

    let (status, body) = create_blackout(
        ctx,
        &admin,
        json!({
            "facility_id": FACILITY_A,
            "bookable_resource_id": RESOURCE_A,
            "start_at": start,
            "end_at": end,
            "reason": "空調濾網更換",
            "blackout_type": "MAINTENANCE"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // INSERT 版的資料修改型 CTE 會回**零筆**（外層 SELECT 讀不到剛插入的列）——
    // 這一格會以 500 失敗。
    assert_eq!(body["data"]["reason"], json!("空調濾網更換"), "{body}");
    assert!(
        !body["data"]["resource_name"].is_null(),
        "指定資源的封鎖應該帶出資源名稱：{body}"
    );
    assert_eq!(body["meta"]["conflicting_reservation_count"], json!(0));
    assert!(body["meta"]["affects"].as_str().unwrap().contains("資源"));

    let (status, listed) = list_blackouts(ctx, &admin, "").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["reason"] == json!("空調濾網更換")),
        "剛建的封鎖沒有出現在清單裡：{listed}"
    );
    // 預設視窗要說出來，否則被濾掉的結果看起來像「沒有封鎖時段」。
    assert_eq!(listed["meta"]["window_default_applied"], json!(true));

    ctx.teardown().await;
}

/// 驗證：時間顛倒、缺 reason、型別不合、跨場域的資源與工單。
#[tokio::test]
async fn b_validation_covers_cross_facility_references() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (start, end) = tomorrow(9, 3);

    for (label, body) in [
        (
            "end 早於 start",
            json!({"facility_id": FACILITY_A, "start_at": end.clone(), "end_at": start.clone(),
                   "reason": "x"}),
        ),
        (
            "空白 reason",
            json!({"facility_id": FACILITY_A, "start_at": start.clone(), "end_at": end.clone(),
                   "reason": "   "}),
        ),
        (
            "非法 blackout_type",
            json!({"facility_id": FACILITY_A, "start_at": start.clone(), "end_at": end.clone(),
                   "reason": "x", "blackout_type": "COFFEE_BREAK"}),
        ),
        (
            // **跨場域的資源。** 少了這一格，一筆「掛在 A 場域、擋著 B 場域資源」
            // 的封鎖建得起來 —— 而 011 以 bookable_resource_id 比對，所以它真的
            // 會擋，只是沒有人找得到原因。
            "資源屬於別的場域",
            json!({"facility_id": FACILITY_A,
                   "bookable_resource_id": "70000000-0000-4000-8000-000000000004",
                   "start_at": start.clone(), "end_at": end.clone(), "reason": "x"}),
        ),
        (
            "不存在的工單",
            json!({"facility_id": FACILITY_A, "start_at": start.clone(), "end_at": end.clone(),
                   "reason": "x",
                   "work_order_id": "00000000-0000-4000-8000-000000000000"}),
        ),
    ] {
        let (status, body) = create_blackout(ctx, &admin, body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "「{label}」被接受了：{body}"
        );
    }

    ctx.teardown().await;
}

/// 視窗內有既有預約 → **409**，清單帶回去；明確 acknowledge 之後才建得起來。
#[tokio::test]
async fn c_existing_reservations_block_the_blackout_until_acknowledged() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (start, end) = tomorrow(10, 2);
    let reservation = insert_reservation(ctx, &start, &end).await;

    let body_base = json!({
        "facility_id": FACILITY_A,
        "bookable_resource_id": RESOURCE_A,
        "start_at": start,
        "end_at": end,
        "reason": "緊急維修"
    });

    let (status, body) = create_blackout(ctx, &admin, body_base.clone()).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "蓋在既有預約上卻直接建立了 —— 那三個人會白跑一趟而系統沒說話：{body}"
    );
    // 清單要帶回去，操作者才知道要通知誰。
    let listed = body["errors"][0]["message"].as_str().unwrap();
    assert!(
        listed.contains(&reservation),
        "409 沒有列出衝突的預約：{body}"
    );
    assert!(
        listed.contains("organizer_id"),
        "沒有帶出預約人 —— 操作者不知道要通知誰：{body}"
    );

    // 沒有建立任何東西。
    let (_, before) = list_blackouts(ctx, &admin, "").await;
    assert!(
        !before["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["reason"] == json!("緊急維修")),
        "409 之後封鎖還是被建了：{before}"
    );

    // 明確確認之後才建得起來，而回應仍然把清單帶回去。
    let mut acked = body_base.clone();
    acked["acknowledge_conflicting_reservations"] = json!(true);
    let (status, body) = create_blackout(ctx, &admin, acked).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["meta"]["conflicting_reservation_count"],
        json!(1),
        "201 的回應沒有帶出衝突清單：{body}"
    );
    assert!(
        body["meta"]["does_not"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap().contains("不會取消")),
        "沒有說出「不會取消既有預約」：{body}"
    );

    // **既有預約真的沒有被動到。** 取消別人的預約是獨立的決定。
    let mut tx = ctx.owner_tx().await;
    let status_after: String =
        sqlx::query_scalar("SELECT status FROM fms.reservations WHERE id = $1::uuid")
            .bind(&reservation)
            .fetch_one(&mut *tx)
            .await
            .expect("read reservation");
    drop(tx);
    assert_eq!(
        status_after, "CONFIRMED",
        "建立封鎖時段把既有預約取消了 —— 那需要 reservation:cancel_any"
    );

    ctx.teardown().await;
}

/// 清單用**重疊**過濾，不是「起點落在視窗內」。
///
/// 一段昨天開始、明天結束的封鎖現在正在生效；用起點過濾會把它濾掉，
/// 而那正是使用者最需要看到的那一筆。
#[tokio::test]
async fn d_list_filters_by_overlap_not_by_start() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 直接插一段「昨天開始、明天結束」的封鎖（API 不禁止過去的起點，
    // 但用 API 建會讓這一格依賴那個決定）。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.resource_blackouts
               (tenant_id, facility_id, bookable_resource_id, start_at, end_at, reason)
             VALUES ($1::uuid, $2::uuid, $3::uuid,
                     clock_timestamp() - interval '1 day',
                     clock_timestamp() + interval '1 day',
                     '長期整修中')",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_A)
        .bind(RESOURCE_A)
        .execute(&mut *tx)
        .await
        .expect("插入跨越現在的封鎖");
        tx.commit().await.expect("commit");
    }

    let (status, body) = list_blackouts(ctx, &admin, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["reason"] == json!("長期整修中")),
        "正在生效的封鎖被預設視窗濾掉了 —— 那是最需要看到的那一筆：{body}"
    );

    ctx.teardown().await;
}

/// **全場域封鎖必須出現在可用性視圖裡。**
///
/// 修正前：可用性查詢對封鎖時段用內連接 `bookable_resources`，
/// 所以 `bookable_resource_id IS NULL` 的列看不到 —— 而 011 的衝突檢查會擋。
/// 日曆說可以訂，送出說衝突。
#[tokio::test]
async fn e_facility_wide_blackout_shows_up_in_availability() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (start, end) = tomorrow(14, 2);

    // 先確認這段時間本來是乾淨的 —— 否則下面的斷言可能是別的東西造成的。
    let before = busy_kinds(ctx, &admin, &start, &end).await;
    assert!(
        !before.iter().any(|k| k == "BLACKOUT"),
        "測試前這段時間就已經有封鎖了：{before:?}"
    );

    let (status, body) = create_blackout(
        ctx,
        &admin,
        json!({
            "facility_id": FACILITY_A,
            // 不給 bookable_resource_id → 整個場域。
            "start_at": start,
            "end_at": end,
            "reason": "全館年度消防檢查",
            "blackout_type": "EMERGENCY"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["data"]["bookable_resource_id"].is_null(), "{body}");
    assert!(
        body["meta"]["affects"]
            .as_str()
            .unwrap()
            .contains("整個場域"),
        "沒有說出這會影響整個場域：{body}"
    );

    let after = busy_kinds(ctx, &admin, &start, &end).await;
    assert!(
        after.iter().any(|k| k == "BLACKOUT"),
        "全場域封鎖沒有出現在可用性視圖裡 —— 日曆會顯示可預約，\
         而送出預約時 011 會擋，錯誤指向一個畫面上看不到的封鎖：{after:?}"
    );

    ctx.teardown().await;
}

/// GET 用 `reservation:read`、POST 用 `blackout:write`，而且兩者真的不同。
#[tokio::test]
async fn f_read_and_write_are_gated_differently() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (start, end) = tomorrow(16, 1);

    // 需要一個「有 reservation:read、但沒有 blackout:write」的人 —— 那是唯一能
    // 區分兩道閘門的組合。009 沒有這種使用者（TENANT_ADMIN 與 FACILITY_ADMIN
    // 兩者都有，TECHNICIAN／REQUESTER 兩者都沒有），因此在測試裡把平台的
    // VIEWER 角色指派給既有的 REQUESTER：VIEWER 有 reservation:read，
    // 而 blackout:write 只給 FACILITY_ADMIN／ORG_MANAGER／TENANT_ADMIN／
    // PLATFORM_ADMIN。
    {
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
        .bind(USERNAME_REQUESTER)
        .execute(&mut *tx)
        .await
        .expect("assign VIEWER");
        tx.commit().await.expect("commit");
    }

    let requester = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, body) = list_blackouts(ctx, &requester, "").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "看得到日曆的人讀不到封鎖時段 —— 那他就問不到「為什麼這格是灰的」：{body}"
    );

    let (status, body) = create_blackout(
        ctx,
        &requester,
        json!({"facility_id": FACILITY_A, "start_at": start, "end_at": end,
               "reason": "請求者想封鎖"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "沒有 blackout:write 的人建立成功了：{body}"
    );

    // 對照：管理員兩者都可以（否則上面的 403 可能只是這個端點壞了）。
    let (status, _) = list_blackouts(ctx, &admin, "").await;
    assert_eq!(status, StatusCode::OK);

    ctx.teardown().await;
}
