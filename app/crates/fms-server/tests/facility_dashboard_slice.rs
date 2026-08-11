//! 設施儀表板（`/reports/facility-dashboard`）。
//!
//! # 核心是 `b_`：儀表板的 SLA 數字必須與報表一致
//!
//! `sla.compliance_pct` 與 `work_orders.avg_resolution_minutes` 來自
//! `/reports/sla-compliance` 呼叫的**同一支** 034 函式。
//!
//! 若儀表板自己算一遍，兩個畫面會給出兩個達成率，而**沒有人知道哪一個
//! 是對的** —— 因為兩邊看起來都「有在算」。`b_` 把兩支端點並排打，
//! 斷言數字相同。
//!
//! # `null` 與 `0` 是不同的答案
//!
//! `c_` 釘住這件事：期間內沒有納入 SLA 的工單時 `compliance_pct` 是 `null`，
//! 不是 `0`。混用會讓前端把「沒資料」畫成一條貼底的線，而那看起來像系統壞了。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const FACILITY_CINEMA: &str = "cccccccc-0000-4000-8000-000000000002";
const ASSET_HQ: &str = "20000000-0000-4000-8000-000000000002";

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn dashboard(ctx: &TestContext, token: &str, facility: &str) -> (StatusCode, Value) {
    ctx.send(authed(
        get(&format!(
            "/api/v1/reports/facility-dashboard?facility_id={facility}"
        )),
        token,
    ))
    .await
}

/// 七個區塊都在，而且數字對得上實際資料。
#[tokio::test]
async fn a_every_section_is_present_and_counts_are_real() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, d) = dashboard(ctx, &admin, FACILITY_HQ).await;
    assert_eq!(status, StatusCode::OK, "{d}");

    for section in [
        "facility",
        "work_orders",
        "sla",
        "assets",
        "maintenance",
        "alarms",
        "space",
        "devices",
    ] {
        assert!(
            d.get(section).is_some(),
            "缺少區塊 {section} —— 契約說「前端首頁所需的**全部**彙總」：{d}"
        );
    }
    assert_eq!(d["facility"]["name"], "台北總部大樓");

    // 資產數要對得上實際的列數，不是一個好看的 0。
    let mut tx = ctx.owner_tx().await;
    let real_assets: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.assets
          WHERE facility_id = $1::uuid AND deleted_at IS NULL AND status <> 'DECOMMISSIONED'",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("查資產");
    tx.commit().await.expect("commit");
    assert_eq!(
        d["assets"]["total"].as_i64(),
        Some(real_assets),
        "資產總數要對得上實際列數：{d}"
    );
    assert!(real_assets > 0, "示範租戶總部應該有資產，否則這一格是空的");

    ctx.teardown().await;
}

/// **儀表板的 SLA 數字必須與 `/reports/sla-compliance` 相同。**
///
/// 這一組最重要的一格。兩支端點並排打，斷言同一個場域的達成率一致。
#[tokio::test]
async fn b_sla_numbers_match_the_report_exactly() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 造一張已完工的工單，讓 SLA 有東西可算。
    let (status, wo) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/work-orders")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "facility_id": FACILITY_HQ, "asset_id": ASSET_HQ,
                        "work_order_type": "CORRECTIVE", "priority": "HIGH",
                        "title": "儀表板測試"
                    })
                    .to_string(),
                ))
                .unwrap(),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");

    let (_, d) = dashboard(ctx, &admin, FACILITY_HQ).await;
    // 報表的 `from`／`to` 是必填（它刻意不給預設 —— 一份沒有指定期間的
    // 合規報表是沒有意義的）。用與儀表板預設 period=30d 相同的窗，
    // 否則兩邊在比不同期間的數字。
    let to = chrono::Utc::now().date_naive();
    let from = to - chrono::Duration::days(29);
    let (status, report) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/sla-compliance?group_by=facility&strictness=strict&from={from}&to={to}"
            )),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");

    let row = report["data"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["group_key"].as_str() == Some(FACILITY_HQ))
        })
        .cloned();

    match row {
        Some(r) => {
            let total = r["resolution_total"].as_i64().unwrap_or(0);
            let met = r["resolution_met"].as_i64().unwrap_or(0);
            let expected = if total > 0 {
                Some((met as f64) * 100.0 / (total as f64))
            } else {
                None
            };
            let actual = d["sla"]["compliance_pct"].as_f64();
            match (expected, actual) {
                (None, None) => {}
                (Some(e), Some(a)) => assert!(
                    (e - a).abs() < 0.001,
                    "**儀表板與報表的達成率不一致**（報表 {e}，儀表板 {a}）—— \
                     兩個畫面給兩個數字時沒有人知道哪一個是對的"
                ),
                _ => panic!(
                    "一邊是 null 一邊有值：報表 {expected:?}，儀表板 {actual:?}。\
                     `null` 與 `0` 是不同的答案，兩支端點必須一致"
                ),
            }
            assert_eq!(
                d["work_orders"]["avg_resolution_minutes"].as_f64(),
                r["avg_resolution_minutes"].as_f64(),
                "avg_resolution_minutes 也來自同一支函式，不該有第二個算法"
            );
        }
        None => {
            // 報表沒有這個場域的列 → 期間內沒有納入 SLA 的工單。
            // 儀表板必須也是 null，不是 0。
            assert!(
                d["sla"]["compliance_pct"].is_null(),
                "報表沒有這個場域的資料，儀表板不該憑空給一個數字：{d}"
            );
        }
    }

    // 口徑要說出來，否則沒有人知道這個數字與報表是不是同一個。
    //
    // **而口徑本身要被釘住。** 上面比對「兩支端點的數字相同」並不足夠：
    // 突變測試把儀表板改成 `loose` 之後 5 格全過，因為示範資料裡
    // strict 與 loose 算出來一樣（沒有跨營業時間的工單）。
    // **數字相同不代表口徑相同**，而口徑一旦分歧，兩個畫面遲早會分歧。
    //
    // `meta.sla_source` 由 handler 的 `SLA_STRICTNESS` 常數產生，
    // 所以這一格等於直接釘住那個政策決定。
    let src = d["meta"]["sla_source"].as_str().unwrap_or_default();
    assert!(
        src.contains("report_sla_compliance"),
        "meta 要說出 SLA 的來源：{d}"
    );
    assert!(
        src.contains("strictness=strict"),
        "**口徑必須與報表的預設（strict）相同** —— 拿到「{src}」。\
         口徑分歧時兩個畫面會給兩個達成率，而數字碰巧相同時沒有人會發現"
    );

    ctx.teardown().await;
}

/// 沒有資料時回 `null`，不是 `0`。
///
/// 信義影城在示範資料裡幾乎是空的 —— 正好用來驗這件事。
#[tokio::test]
async fn c_no_data_means_null_not_zero() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, d) = dashboard(ctx, &admin, FACILITY_CINEMA).await;
    assert_eq!(status, StatusCode::OK, "{d}");

    // 計數型指標是 0（那是算出來的）；比率型在沒有分母時必須是 null。
    assert_eq!(d["work_orders"]["open"], 0, "計數沒有東西就是 0：{d}");

    for path in [
        ("sla", "compliance_pct"),
        ("maintenance", "pm_compliance_pct"),
    ] {
        let v = &d[path.0][path.1];
        assert!(
            v.is_null() || v.is_number(),
            "{}.{} 應該是 null 或數字：{v}",
            path.0,
            path.1
        );
        // 若真的沒有分母，必須是 null 而不是 0.0 —— 那是不同的事實。
        if v.as_f64() == Some(0.0) {
            panic!(
                "{}.{} 是 0.0 —— 若分母是 0 就該回 null，\
                 否則前端會把「沒資料」畫成一條貼底的線：{d}",
                path.0, path.1
            );
        }
    }

    ctx.teardown().await;
}

/// 場域收斂：每個子查詢都要限縮在請求的場域。
///
/// 兩個場域的儀表板不能給出同一組數字 —— 那代表有子查詢忘了加
/// `facility_id` 條件。
#[tokio::test]
async fn d_every_subquery_is_scoped_to_the_requested_facility() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (_, hq) = dashboard(ctx, &admin, FACILITY_HQ).await;
    let (_, cinema) = dashboard(ctx, &admin, FACILITY_CINEMA).await;

    assert_ne!(
        hq["assets"]["total"], cinema["assets"]["total"],
        "兩個場域的資產數相同 —— 某個子查詢漏了 facility_id 條件。\
         HQ={} 影城={}",
        hq["assets"]["total"], cinema["assets"]["total"]
    );
    assert_ne!(hq["facility"]["id"], cinema["facility"]["id"]);

    // 看不到的場域是 404，不是一整組 0（後者看起來像「這個場域很閒」）。
    let (status, missing) = dashboard(ctx, &admin, "cccccccc-0000-4000-8000-0000000000ff").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "不存在的場域要回 404，回一整組 0 會被讀成「這個場域很閒」：{missing}"
    );

    ctx.teardown().await;
}

/// 參數驗證與權限。
#[tokio::test]
async fn e_parameters_and_permission() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // facility_id 是必填 —— 契約寫 required: true。
    let (status, body) = ctx
        .send(authed(get("/api/v1/reports/facility-dashboard"), &admin))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, bad) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/facility-dashboard?facility_id={FACILITY_HQ}&period=forever"
            )),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "打錯的 period 要擋下來，不是靜默用預設值：{bad}"
    );

    // period 真的影響結果：today 的完工數不會多於 30d 的。
    let (_, today) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/facility-dashboard?facility_id={FACILITY_HQ}&period=today"
            )),
            &admin,
        ))
        .await;
    let (_, month) = dashboard(ctx, &admin, FACILITY_HQ).await;
    assert!(
        today["work_orders"]["completed_in_period"]
            .as_i64()
            .unwrap_or(0)
            <= month["work_orders"]["completed_in_period"]
                .as_i64()
                .unwrap_or(0),
        "today 的完工數不該多於 30d —— period 沒有真的傳進查詢：today={} 30d={}",
        today["work_orders"]["completed_in_period"],
        month["work_orders"]["completed_in_period"]
    );
    assert_eq!(today["meta"]["period"], "today", "回應要說出用了哪個期間");

    // 場域管理員看得到自己的場域（儀表板是場域級的畫面）。
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, own) = dashboard(ctx, &fm, FACILITY_HQ).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "場域管理員要看得到自己場域的儀表板：{own}"
    );

    // 但看不到別的場域。
    let (status, other) = dashboard(ctx, &fm, FACILITY_CINEMA).await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
        "總部的管理員不該看到影城的儀表板（得到 {status}）：{other}"
    );

    ctx.teardown().await;
}
