//! 四支報表（group-rollup、asset-reliability、space-utilization、service-volume）。
//!
//! # 每一條都釘住一個「看起來對但會說謊」的分母
//!
//! 這四支報表的數字會拿去談合約、決定要不要再隔一間會議室、對客戶收費。
//! 所以測的不是「有沒有回資料」，是**分母有沒有選對**：
//!
//!   * `a_`：子樹彙總 —— 父組織的數字必須含孫節點的設施。
//!     逐層爬 `parent_id` 會讓集團那一列比底下分公司的總和還小。
//!   * `c_`：MTBF 一次故障回 null 而不是 0，而 `failure_count` 讓「還不知道」
//!     與「很可靠」分得開。
//!   * `d_`：使用率的分母是可預約時數，而 `hours_basis` 說出它是哪來的。
//!   * `e_`：no-show 的分母只含需要報到的預約。
//!   * `f_`：費率未知的工單被單獨計數，而金額被標記為下限。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::Value;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

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

/// **子樹彙總**：父組織的數字含孫節點的設施。
#[tokio::test]
async fn a_rollup_counts_the_whole_subtree_not_just_direct_facilities() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();

    // 在總部建一張工單 —— 它該出現在總部所屬組織**以及所有上層組織**的數字裡。
    ctx.seed_work_order(FACILITY_HQ, "彙總測試").await;

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/reports/group-rollup?from={from}&to={to}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert!(!rows.is_empty(), "示範資料該有 3 個組織：{}", body["data"]);

    // 根組織（depth 0）的工單數必須 >= 任何子組織的。
    let root = rows.iter().find(|r| r["depth"] == 0).expect("該有根組織");
    let root_total = root["work_orders_total"].as_i64().expect("total");
    assert!(
        root_total >= 1,
        "根組織該含底下設施的工單 —— 這一條就是「逐層爬會漏掉孫節點」的\
         突變測試：{root}"
    );
    for r in rows {
        if r["depth"].as_i64().unwrap_or(0) > 0 {
            assert!(
                root_total >= r["work_orders_total"].as_i64().unwrap_or(0),
                "根組織的數字不該小於子組織的：root={root_total}, child={r}"
            );
        }
    }

    // 各列會重疊，必須說出來。
    assert_eq!(
        body["meta"]["rows_are_cumulative"], true,
        "父組織含子組織 —— 前端相加會重複計算，這件事必須在 meta 裡"
    );
    assert_eq!(
        body["meta"]["subtree_basis"],
        "organizations.org_path (ltree)"
    );

    // 日期填反 → 422，不是一份看起來合理的空報表。
    let (status, _) = ctx
        .send(authed(
            get(&format!("/api/v1/reports/group-rollup?from={to}&to={from}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// `subtree_of` 只回那個組織的子樹。
#[tokio::test]
async fn b_subtree_of_narrows_to_that_branch() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();

    let (_s, all) = ctx
        .send(authed(
            get(&format!("/api/v1/reports/group-rollup?from={from}&to={to}")),
            &token,
        ))
        .await;
    let leaf = all["data"]
        .as_array()
        .expect("data")
        .iter()
        .max_by_key(|r| r["depth"].as_i64().unwrap_or(0))
        .expect("該有最深的一列")
        .clone();
    let leaf_id = leaf["org_id"].as_str().expect("org_id");

    let (status, sub) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/group-rollup?from={from}&to={to}&subtree_of={leaf_id}"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    let n = sub["data"].as_array().map(Vec::len).unwrap_or(0);
    assert!(
        n >= 1 && n <= all["data"].as_array().map(Vec::len).unwrap_or(0),
        "子樹該是整棵樹的子集且至少含自己；{n}"
    );
    assert_eq!(sub["meta"]["subtree_of"], leaf_id);

    ctx.teardown().await;
}

/// 改設備狀態，並把 064 的觸發器剛寫下的那一列挪到 N 天前。
///
/// 觸發器用 `clock_timestamp()`，所以「兩次故障相隔多久」只能事後調時刻做出來。
async fn set_status(ctx: &TestContext, asset: uuid::Uuid, status: &str, days_ago: i32) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("UPDATE fms.assets SET status = $2 WHERE id = $1")
        .bind(asset)
        .bind(status)
        .execute(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("改狀態成 {status} 失敗：{e}"));
    sqlx::query(
        "UPDATE fms.asset_status_history
            SET changed_at = clock_timestamp() - make_interval(days => $2)
          WHERE asset_id = $1 AND changed_at > clock_timestamp() - interval '1 minute'",
    )
    .bind(asset)
    .bind(days_ago)
    .execute(&mut *tx)
    .await
    .expect("調時刻");
    tx.commit().await.expect("commit");
}

/// **MTBF：一次故障回 null 而不是 0，兩次才算得出來。**
#[tokio::test]
async fn c_mtbf_needs_two_failures_and_null_is_not_zero() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();
    let asset = ctx.seed_asset(FACILITY_HQ, "REL-MTBF").await;

    // 第一次故障（20 天前）。
    set_status(&ctx, asset, "DOWN", 20).await;
    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/asset-reliability?from={from}&to={to}"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|r| r["asset_code"] == "REL-MTBF")
        .expect("該找得到")
        .clone();
    assert_eq!(row["failure_count"], 1);
    assert_eq!(
        row["mtbf_hours"],
        Value::Null,
        "**一次故障算不出 MTBF** —— 回 0 會看起來像設備一直在壞：{row}"
    );

    // 修好再壞（10 天前）→ 兩次故障，MTBF ≈ 10 天 = 240 小時。
    set_status(&ctx, asset, "OPERATIONAL", 15).await;
    set_status(&ctx, asset, "DOWN", 10).await;
    let (_s, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/asset-reliability?from={from}&to={to}"
            )),
            &token,
        ))
        .await;
    let row = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|r| r["asset_code"] == "REL-MTBF")
        .expect("該找得到")
        .clone();
    assert_eq!(row["failure_count"], 2, "{row}");
    let mtbf = row["mtbf_hours"].as_f64().expect("mtbf");
    assert!(
        (mtbf - 240.0).abs() < 24.0,
        "兩次故障相隔 10 天 → MTBF 約 240 小時；實際 {mtbf}"
    );

    // `history_since` 與涵蓋範圍旗標。
    assert!(row["history_since"].as_str().is_some(), "{row}");
    assert_eq!(
        body["meta"]["mtbf_source"],
        "asset_status_history (migration 064)"
    );
    assert!(body["meta"]["history_covers_full_range"].is_boolean());

    ctx.teardown().await;
}

/// **使用率的分母是可預約時數，而 `hours_basis` 說出來源。**
///
/// 這一條抓的是「宣告了沒人讀」：第一版把時數形狀寫成 `{"hours_per_day": n}`，
/// 但 038 定下的形狀是星期鍵 → 時段陣列，於是每一列都落到 `assumed_24h`，
/// 整個決定變成沒有效果的程式碼 —— 而報表看起來完全正常。
///
/// 種子的總部是週一到週五 08:00–21:00、週六 09:00–17:00、週日不開，
/// 一週 73 小時。所以分母**必須嚴格小於** 24 × 天數。
#[tokio::test]
async fn d_utilization_denominator_is_real_opening_hours_not_24h() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();
    let days = (chrono::NaiveDate::parse_from_str(&to, "%Y-%m-%d").unwrap()
        - chrono::NaiveDate::parse_from_str(&from, "%Y-%m-%d").unwrap())
    .num_days() as f64
        + 1.0;

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/space-utilization?from={from}&to={to}&facility_id={FACILITY_HQ}"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert!(!rows.is_empty(), "示範資料該有可預約資源：{body}");

    for r in rows {
        let basis = r["hours_basis"].as_str().expect("hours_basis");
        assert_eq!(
            basis, "facility.operating_hours",
            "總部設了 operating_hours（週一到五 13 小時、週六 8 小時），\
             所以基準該是場域的。`assumed_24h` 代表解析根本沒讀到那個欄位：{r}"
        );
        let available = r["available_hours"].as_f64().expect("available_hours");
        assert!(available > 0.0, "可預約時數該 > 0，否則使用率算不出來：{r}");
        assert!(
            available < 24.0 * days,
            "分母是**營業時數**而不是 24 小時 × {days} 天 = {} —— \
             實際 {available}。相等代表落回了 assumed_24h。",
            24.0 * days
        );
        // 一週 73 小時 → 每日平均約 10.4 小時。抓「解析出一星期只有一天」之類的錯。
        let per_day = available / days;
        assert!(
            (8.0..14.0).contains(&per_day),
            "每日平均營業時數該在 8–14 之間（73/7 ≈ 10.4）；實際 {per_day}：{r}"
        );
    }

    assert_eq!(
        body["meta"]["resources_with_assumed_hours"], 0,
        "總部的資源都解析得到時數，沒有一列該用猜的：{}",
        body["meta"]
    );

    ctx.teardown().await;
}

/// 資源自己的 `opening_hours` 蓋過場域的，而它的形狀在寫入時就被擋。
#[tokio::test]
async fn d2_resource_hours_override_the_facility_and_bad_shapes_are_rejected() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();

    let mut tx = ctx.owner_tx().await;
    // 壞形狀要在寫入時被擋 —— 不是在讀報表時讓 `::time` 炸掉。
    let bad = sqlx::query(
        "UPDATE fms.bookable_resources SET opening_hours = '{\"mon\":[[\"8:0\",\"21:00\"]]}'
          WHERE facility_id = $1::uuid AND is_bookable",
    )
    .bind(FACILITY_HQ)
    .execute(&mut *tx)
    .await;
    assert!(
        bad.is_err(),
        "`\"8:0\"` 不合 038 的形狀，該被 ck_bookable_opening_hours 擋下"
    );
    tx.rollback().await.expect("rollback");

    // 每天 24 小時的資源 → 基準變成資源自己的，分母等於 24 × 天數。
    let mut tx = ctx.owner_tx().await;
    let resource: uuid::Uuid = sqlx::query_scalar(
        "UPDATE fms.bookable_resources
            SET opening_hours = '{\"mon\":[[\"00:00\",\"24:00\"]],\"tue\":[[\"00:00\",\"24:00\"]],
                                  \"wed\":[[\"00:00\",\"24:00\"]],\"thu\":[[\"00:00\",\"24:00\"]],
                                  \"fri\":[[\"00:00\",\"24:00\"]],\"sat\":[[\"00:00\",\"24:00\"]],
                                  \"sun\":[[\"00:00\",\"24:00\"]]}'::jsonb
          WHERE id = (SELECT id FROM fms.bookable_resources
                       WHERE facility_id = $1::uuid AND is_bookable ORDER BY id LIMIT 1)
         RETURNING id",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("設資源時數");
    tx.commit().await.expect("commit");

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/space-utilization?from={from}&to={to}&facility_id={FACILITY_HQ}"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|r| r["resource_id"].as_str() == Some(&resource.to_string()))
        .expect("該找得到那個資源")
        .clone();
    assert_eq!(
        row["hours_basis"], "resource.opening_hours",
        "資源自己設了時數，該蓋過場域的：{row}"
    );

    ctx.teardown().await;
}

/// **no-show 的分母只含需要報到的預約，而且讀預約自己的那一份旗標。**
///
/// 兩個突變都該被這一條殺掉：
///   * 分母換成「所有預約」→ 比率被不需報到的那一筆稀釋成 1/3。
///   * 分母改讀 `bookable_resources.requires_check_in`（資源上的現值）→
///     那筆刻意設成 false 的預約會被算進去，而它永遠不可能變成 NO_SHOW，
///     因為 `no_show.rs` 的掃描器讀的是 `r.requires_check_in`。
#[tokio::test]
async fn e_the_no_show_denominator_reads_the_reservations_own_flag() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();

    let mut tx = ctx.owner_tx().await;
    // 資源上的旗標一律打開 —— 這樣「讀資源」的錯誤版本會把三筆全算進分母。
    let resource: uuid::Uuid = sqlx::query_scalar(
        "UPDATE fms.bookable_resources SET requires_check_in = true
          WHERE id = (SELECT id FROM fms.bookable_resources
                       WHERE facility_id = $1::uuid AND is_bookable
                       ORDER BY id LIMIT 1)
         RETURNING id",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("該有可預約資源");

    // 三筆預約，全在同一個資源上：
    //   NO_SHOW（需報到）／COMPLETED（需報到）→ 分母 2、分子 1 → 0.5
    //   CONFIRMED（**不需**報到）→ 不進分母
    for (status_val, needs_checkin, hours_ago) in [
        ("NO_SHOW", true, 30_i32),
        ("COMPLETED", true, 26),
        ("CONFIRMED", false, 22),
    ] {
        sqlx::query(
            "INSERT INTO fms.reservations
               (tenant_id, facility_id, bookable_resource_id, reservation_no,
                resource_type, resource_id, organizer_id, title,
                start_at, end_at, status, requires_check_in)
             SELECT $1::uuid, br.facility_id, br.id,
                    'RSV-T-' || substr(md5(random()::text), 1, 10),
                    br.resource_type,
                    coalesce(br.spatial_node_id, br.asset_id),
                    $2::uuid, '分母測試',
                    clock_timestamp() - make_interval(hours => $3),
                    clock_timestamp() - make_interval(hours => $3) + interval '1 hour',
                    $4, $5
               FROM fms.bookable_resources br WHERE br.id = $6",
        )
        .bind(TENANT_ID)
        .bind(ADMIN_USER_ID)
        .bind(hours_ago)
        .bind(status_val)
        .bind(needs_checkin)
        .bind(resource)
        .execute(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("建 {status_val} 預約失敗：{e}"));
    }
    tx.commit().await.expect("commit");

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/space-utilization?from={from}&to={to}"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|r| r["resource_id"].as_str() == Some(&resource.to_string()))
        .expect("該找得到那個資源")
        .clone();

    assert_eq!(row["reservations_total"], 3, "三筆都該算進總數：{row}");
    assert_eq!(row["no_shows"], 1, "{row}");
    assert_eq!(
        row["checkin_required"], 2,
        "分母是**預約自己說需要報到**的兩筆。3 代表讀了資源上的旗標，\
         而那一筆 CONFIRMED 永遠不可能變成 NO_SHOW：{row}"
    );
    let rate = row["no_show_rate"].as_f64().expect("no_show_rate");
    assert!(
        (rate - 0.5).abs() < 0.001,
        "1/2 = 0.5；實際 {rate} —— 分母含不需報到的預約時會被稀釋成 0.333"
    );
    assert_eq!(
        body["meta"]["no_show_denominator"],
        "reservations.requires_check_in (same column no_show.rs sweeps)"
    );

    ctx.teardown().await;
}

/// **費率未知的工單被單獨計數，而金額被標記為下限。**
#[tokio::test]
async fn f_service_volume_counts_work_orders_whose_rate_is_unknown() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let (from, to) = window();

    // 一張 SERVICE 工單，登了工時但**沒有費率**。
    let mut tx = ctx.owner_tx().await;
    let wo: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.work_orders
           (tenant_id, facility_id, wo_no, work_order_type, source, title,
            status, priority, spatial_node_id, service_item_id, labor_minutes)
         SELECT $1::uuid, $2::uuid,
                'WO-SV-' || substr(md5(random()::text), 1, 8),
                'SERVICE', 'MANUAL', '軟性服務', 'IN_PROGRESS', 'MEDIUM',
                (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                (SELECT id FROM fms.service_items LIMIT 1),
                120
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("建服務工單");
    let _ = wo;
    tx.commit().await.expect("commit");

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/service-volume?from={from}&to={to}"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert!(!rows.is_empty(), "該有一列：{body}");

    let total_without: i64 = rows
        .iter()
        .map(|r| r["work_orders_without_rate"].as_i64().unwrap_or(0))
        .sum();
    assert!(
        total_without >= 1,
        "有工時（120 分）但工時成本為 0 → 費率未知，必須被計數：{}",
        body["data"]
    );
    assert_eq!(
        body["meta"]["cost_is_lower_bound"], true,
        "**金額是下限而不是實際值** —— 把費率未知當免費會讓帳單安靜偏低：{}",
        body["meta"]
    );
    // 三種成本分開回傳。
    let r = &rows[0];
    for k in ["labor_cost", "parts_cost", "other_cost", "total_cost"] {
        assert!(r.get(k).is_some(), "{k} 該存在：{r}");
    }

    // group_by 白名單。
    let (status, _) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/service-volume?from={from}&to={to}&group_by=nope"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// 四支都需要 `report:read`。
#[tokio::test]
async fn g_all_four_require_report_read() {
    let ctx = TestContext::setup().await;
    let token = ctx.login_as(USERNAME_REQUESTER).await;
    let (from, to) = window();

    for path in [
        "group-rollup",
        "asset-reliability",
        "space-utilization",
        "service-volume",
    ] {
        let (status, _) = ctx
            .send(authed(
                get(&format!("/api/v1/reports/{path}?from={from}&to={to}")),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} 該需要 report:read");
    }

    ctx.teardown().await;
}
