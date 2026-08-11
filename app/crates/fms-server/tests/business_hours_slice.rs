//! 營業時間與假日行事曆（migration 038／039）。
//!
//! ADR-12 決定 C 當時寫「一律以自然時間計」，理由是營業時間內的經過時間
//! 算不出來。後果是 `strict` 報表必須整批排除宣告 `business_hours_only` 的
//! 政策 —— 而種子的 `SLA_STANDARD`（MEDIUM，多數工單）就宣告了它。
//! **也就是一份把大多數工單靜默排除掉的合約報表。**
//!
//! 038 的關鍵決定是**在算 due 的時候**把營業時間算進去，而不是在報表算
//! 經過時間。算出來仍然是絕對時刻，因此掃描與報表的比較都不需要任何
//! 營業時間邏輯，決定 F 的快照語意也保留。
//!
//! 這些測試的時間都用**固定日期**而不是 `now()`：營業時間的答案取決於
//! 星期幾，而一個「只在週末失敗」的測試比沒有測試更糟。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";

/// 2026-08-07 是**週五**、08-08 週六、08-09 週日、08-10 週一。
/// 總部班表：週一至五 08:00–21:00、週六 09:00–17:00、週日休。
const FRI_2000: &str = "2026-08-07 20:00+08";
const SAT_1600: &str = "2026-08-08 16:00+08";
const SUN_1200: &str = "2026-08-09 12:00+08";
/// 週五 22:00 —— **當日班表（到 21:00）已經打烊**。
const FRI_2200: &str = "2026-08-07 22:00+08";

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 直接呼叫 `fms.add_business_minutes`，回傳台北時間的字串。
async fn add_minutes(ctx: &TestContext, from: &str, minutes: i32) -> Option<String> {
    let mut tx = ctx.owner_tx().await;
    sqlx::query_scalar(
        "SELECT to_char(
                  fms.add_business_minutes($1::uuid, $2::timestamptz, $3)
                    AT TIME ZONE 'Asia/Taipei',
                  'YYYY-MM-DD HH24:MI')
           FROM fms.facilities WHERE id = $1::uuid",
    )
    .bind(FACILITY_HQ)
    .bind(from)
    .bind(minutes)
    .fetch_one(&mut *tx)
    .await
    .expect("add_business_minutes")
}

async fn add_holiday(
    ctx: &TestContext,
    date: &str,
    name: &str,
    working: bool,
    windows: Option<&str>,
) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.holiday_calendars
           (tenant_id, facility_id, holiday_date, name, is_working_day, windows)
         VALUES ($1::uuid, NULL, $2::date, $3, $4, $5::jsonb)",
    )
    .bind(TENANT_ID)
    .bind(date)
    .bind(name)
    .bind(working)
    .bind(windows)
    .execute(&mut *tx)
    .await
    .expect("建立假日");
    tx.commit().await.expect("commit");
}

// =============================================================================
// 算術
// =============================================================================

/// 跨過關門時間 → 順延到下一個營業時段。
#[tokio::test]
async fn minutes_spill_into_the_next_open_window() {
    let ctx = &TestContext::setup().await;

    // 週五 20:00，班表到 21:00 → 剩 60 分；再 60 分要到週六 09:00 之後。
    assert_eq!(
        add_minutes(ctx, FRI_2000, 120).await.as_deref(),
        Some("2026-08-08 10:00"),
        "週五剩 60 分，餘下 60 分從週六 09:00 起算"
    );

    // 同一個時段內就夠 → 不換日。
    assert_eq!(
        add_minutes(ctx, FRI_2000, 30).await.as_deref(),
        Some("2026-08-07 20:30")
    );

    // 週六 09:00–17:00 是 480 分鐘。週五 20:00 + 480：週五吃 60、
    // 週六從 09:00 起再 420 → 16:00。
    assert_eq!(
        add_minutes(ctx, FRI_2000, 480).await.as_deref(),
        Some("2026-08-08 16:00")
    );

    // **打烊之後才開單** —— 很常見（晚上 22:00 報修），而第一版沒有測到。
    //
    // 這一格守的是「整段在過去的時段要跳過」那個判斷：少了它，
    // 可用分鐘會算成**負數**，而負數會讓剩餘分鐘反而變大 ——
    // 期限往後跑一個小時（突變實測 11 個測試全數通過，因為所有既有案例的
    // 起算時刻都還在當日時段之內）。
    assert_eq!(
        add_minutes(ctx, FRI_2200, 60).await.as_deref(),
        Some("2026-08-08 10:00"),
        "週五已打烊 → 整段跳過，週六 09:00 起算 60 分"
    );

    ctx.teardown().await;
}

/// 週日不營業 → 整天跳過。
#[tokio::test]
async fn a_closed_weekday_is_skipped_entirely() {
    let ctx = &TestContext::setup().await;

    // 週六 16:00，班表到 17:00 → 剩 60；週日休；週一 08:00 起再 60 → 09:00。
    assert_eq!(
        add_minutes(ctx, SAT_1600, 120).await.as_deref(),
        Some("2026-08-10 09:00")
    );

    // 起算時刻本身就在不營業的日子 → 從下一個營業日開門起算。
    assert_eq!(
        add_minutes(ctx, SUN_1200, 60).await.as_deref(),
        Some("2026-08-10 09:00")
    );

    ctx.teardown().await;
}

/// 假日跟週末一樣被跳過。
#[tokio::test]
async fn a_holiday_is_skipped() {
    let ctx = &TestContext::setup().await;

    assert_eq!(
        add_minutes(ctx, SAT_1600, 120).await.as_deref(),
        Some("2026-08-10 09:00"),
        "前提：沒有假日時落在週一"
    );

    add_holiday(ctx, "2026-08-10", "中元節（測試）", false, None).await;

    assert_eq!(
        add_minutes(ctx, SAT_1600, 120).await.as_deref(),
        Some("2026-08-11 09:00"),
        "週一放假 → 順延到週二 08:00 起算 60 分"
    );

    ctx.teardown().await;
}

/// **補班日必須自帶時段。**
///
/// 這一格是驗算時逼出來的設計修正。台灣的補班日是週六，而多數辦公場域
/// 只排週一至五 —— 若補班日沿用「那個星期的班表」，那個星期沒有班表，
/// 一天能做 0 分鐘，於是 `is_working_day = true` 對它**唯一的用途**無效。
///
/// 因此 `holiday_calendars.windows` 是必要的，不是選配。
#[tokio::test]
async fn a_make_up_working_day_needs_its_own_windows() {
    let ctx = &TestContext::setup().await;

    // 先把週日變成補班日，但**不給時段** —— 總部沒有 'sun' 班表，
    // 因此這一天仍然沒有可用的分鐘。
    add_holiday(ctx, "2026-08-09", "補班（沒給時段）", true, None).await;
    assert_eq!(
        add_minutes(ctx, SUN_1200, 60).await.as_deref(),
        Some("2026-08-10 09:00"),
        "沒給時段的補班日沿用該星期的班表，而週日沒有班表 → 仍然跳過"
    );

    // 給時段之後才真的能上班。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.holiday_calendars
                SET windows = '[[\"09:00\",\"18:00\"]]'::jsonb
              WHERE holiday_date = '2026-08-09'::date",
        )
        .execute(&mut *tx)
        .await
        .expect("補上時段");
        tx.commit().await.expect("commit");
    }
    assert_eq!(
        add_minutes(ctx, SUN_1200, 60).await.as_deref(),
        Some("2026-08-09 13:00"),
        "補班日帶了 09:00–18:00，因此週日 12:00 + 60 分就在當天"
    );

    ctx.teardown().await;
}

/// 場域專屬的假日勝過租戶通用的。
#[tokio::test]
async fn a_facility_holiday_overrides_the_tenant_wide_one() {
    let ctx = &TestContext::setup().await;

    // 租戶通用：週一放假。
    add_holiday(ctx, "2026-08-10", "全公司休假", false, None).await;
    assert_eq!(
        add_minutes(ctx, SAT_1600, 120).await.as_deref(),
        Some("2026-08-11 09:00"),
        "前提：租戶通用的假日生效"
    );

    // 總部專屬：那天照上班（補班，帶時段）。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.holiday_calendars
               (tenant_id, facility_id, holiday_date, name, is_working_day, windows)
             VALUES ($1::uuid, $2::uuid, '2026-08-10'::date, '總部照常營運', true,
                     '[[\"08:00\",\"21:00\"]]'::jsonb)",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_HQ)
        .execute(&mut *tx)
        .await
        .expect("建立場域專屬覆寫");
        tx.commit().await.expect("commit");
    }

    assert_eq!(
        add_minutes(ctx, SAT_1600, 120).await.as_deref(),
        Some("2026-08-10 09:00"),
        "場域專屬的覆寫應勝過租戶通用的假日"
    );

    ctx.teardown().await;
}

/// 沒有班表的場域回 `NULL` —— 呼叫端據此退回自然時間。
///
/// **不能偷偷當成 24/7**：那會讓期限比預期緊得多（週五晚上的單變成週六
/// 早上到期而不是週一），而且沒有任何人知道。
#[tokio::test]
async fn a_facility_without_a_calendar_returns_null() {
    let ctx = &TestContext::setup().await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.facilities SET operating_hours = '{}'::jsonb WHERE id = $1::uuid")
            .bind(FACILITY_HQ)
            .execute(&mut *tx)
            .await
            .expect("清班表");
        tx.commit().await.expect("commit");
    }

    assert_eq!(
        add_minutes(ctx, FRI_2000, 120).await,
        None,
        "算不出來就要說算不出來"
    );

    ctx.teardown().await;
}

// =============================================================================
// 接到工單上
// =============================================================================

/// 開單時期限就用營業時間算，並記下 `sla_basis`。
#[tokio::test]
async fn work_order_targets_use_business_hours_and_record_the_basis() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // MEDIUM → SLA_STANDARD（business_hours_only = true，480 分鐘）。
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "營業時間期限",
                    "priority": "MEDIUM"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    let id = wo["id"].as_str().expect("id");

    let mut tx = ctx.owner_tx().await;
    let (basis, due, created): (
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as(
        "SELECT sla_basis, resolution_due_at, created_at
               FROM fms.work_orders WHERE id = $1::uuid",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("讀工單");

    assert_eq!(basis.as_deref(), Some("BUSINESS_HOURS"), "{wo}");
    let due = due.expect("應有 due");
    assert!(
        (due - created).num_minutes() >= 480,
        "營業時間的期限不可能早於自然時間的（實際 {} 分）",
        (due - created).num_minutes()
    );

    ctx.teardown().await;
}

/// 政策要營業時間、場域沒班表 → `NATURAL_FALLBACK`，而且**期限還是有的**。
///
/// 這是剩下的那個真實缺口。給期限（有目標比沒目標好）但標記它是退路 ——
/// 報表的 `strict` 模式據此排除，`operational` 納入並計數。
#[tokio::test]
async fn a_missing_calendar_falls_back_and_says_so() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.facilities SET operating_hours = '{}'::jsonb WHERE id = $1::uuid")
            .bind(FACILITY_HQ)
            .execute(&mut *tx)
            .await
            .expect("清班表");
        tx.commit().await.expect("commit");
    }

    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "沒有班表",
                    "priority": "MEDIUM"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");

    let created: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["created_at"].clone()).expect("created_at");
    let resol: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["resolution_due_at"].clone())
            .unwrap_or_else(|_| panic!("退路也要給期限：{wo}"));
    assert_eq!(
        (resol - created).num_minutes(),
        480,
        "退路是自然時間，因此就是牆鐘差值"
    );

    let mut tx = ctx.owner_tx().await;
    let basis: Option<String> =
        sqlx::query_scalar("SELECT sla_basis FROM fms.work_orders WHERE id = $1::uuid")
            .bind(wo["id"].as_str().expect("id"))
            .fetch_one(&mut *tx)
            .await
            .expect("讀 sla_basis");
    assert_eq!(
        basis.as_deref(),
        Some("NATURAL_FALLBACK"),
        "必須看得出這個期限不是營業時間算的：{wo}"
    );

    ctx.teardown().await;
}

/// 不看營業時間的政策仍然是自然時間，`sla_basis = 'NATURAL'`。
///
/// 反面：038 不能把所有政策都變成營業時間 —— `SLA_CRITICAL` 明確宣告
/// `business_hours_only = false`（緊急事件不等上班）。
#[tokio::test]
async fn a_247_policy_stays_on_natural_time() {
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
                    "title": "緊急不等上班",
                    "priority": "CRITICAL"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");

    let created: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["created_at"].clone()).expect("created_at");
    let resol: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(wo["resolution_due_at"].clone()).expect("resolution_due_at");
    assert_eq!((resol - created).num_minutes(), 120);

    let mut tx = ctx.owner_tx().await;
    let basis: Option<String> =
        sqlx::query_scalar("SELECT sla_basis FROM fms.work_orders WHERE id = $1::uuid")
            .bind(wo["id"].as_str().expect("id"))
            .fetch_one(&mut *tx)
            .await
            .expect("讀 sla_basis");
    assert_eq!(basis.as_deref(), Some("NATURAL"));

    ctx.teardown().await;
}

// =============================================================================
// 形狀約束
// =============================================================================

/// 壞掉的 `operating_hours` 在寫入時被擋。
///
/// 這個欄位從「存了沒人看」變成「決定合約期限」，因此它的形狀開始有後果。
/// 一個 `"08:0"` 若能寫進去，`::time` 轉型會在**開單時**失敗 ——
/// 那時使用者只看到一個失敗的請求，而原因在三層之外的場域設定裡。
#[tokio::test]
async fn a_malformed_schedule_is_rejected_at_write_time() {
    let ctx = &TestContext::setup().await;

    for bad in [
        r#"{"monday": [["08:00","21:00"]]}"#, // 星期鍵要三字母小寫
        r#"{"mon": [["08:0","21:00"]]}"#,     // 壞掉的時刻
        r#"{"mon": [["21:00","08:00"]]}"#,    // 結束早於開始
        r#"{"mon": [["09:00","09:00"]]}"#,    // 零長度
        r#"{"mon": [["08:00"]]}"#,            // 不是兩個元素
        r#"{"mon": "08:00-21:00"}"#,          // 不是陣列
    ] {
        let mut tx = ctx.owner_tx().await;
        let r = sqlx::query(
            "UPDATE fms.facilities SET operating_hours = $1::jsonb WHERE id = $2::uuid",
        )
        .bind(bad)
        .bind(FACILITY_HQ)
        .execute(&mut *tx)
        .await;
        assert!(r.is_err(), "{bad} 應被 CHECK 擋下");
    }

    ctx.teardown().await;
}

/// 同一個 (場域, 日期) 只能有一筆行事曆列。
#[tokio::test]
async fn one_calendar_entry_per_facility_and_date() {
    let ctx = &TestContext::setup().await;
    add_holiday(ctx, "2026-08-10", "第一筆", false, None).await;

    let mut tx = ctx.owner_tx().await;
    let r = sqlx::query(
        "INSERT INTO fms.holiday_calendars
           (tenant_id, facility_id, holiday_date, name, is_working_day)
         VALUES ($1::uuid, NULL, '2026-08-10'::date, '第二筆', true)",
    )
    .bind(TENANT_ID)
    .execute(&mut *tx)
    .await;
    assert!(
        r.is_err(),
        "兩筆租戶通用的同日行事曆會讓「這一天營不營業」有兩個答案 —— \
         NULLS NOT DISTINCT 擋的正是這個"
    );

    ctx.teardown().await;
}
