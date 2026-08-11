//! `GET /reports/sla-compliance`（ADR-12 量測鏈第 4 段、migration 034）。
//!
//! 這支端點在 032 之前做出來會**每一列都是 100%** —— 004 的完成判定在
//! `resolution_due_at IS NULL` 時恆為真，而那個欄位從來沒有東西會寫。
//! 因此本檔的核心不是「數字算得出來」，而是**那些不該被算成達成的東西
//! 沒有被算成達成**：
//!
//!   * 沒有 policy 的工單 → `excluded_no_policy`，不進分母
//!   * 還沒可判定的工單 → `excluded_in_flight`，不進分母
//!   * 已取消的工單 → `excluded_abandoned`，不進分母
//!   * 分母為 0 → `compliance_pct` 是 `null`，不是 0 也不是 100

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";
const TECH_WANG: &str = "ffffffff-0000-4000-8000-000000000003";

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

async fn create_wo(ctx: &TestContext, token: &str, priority: &str) -> String {
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "報表測試",
                    "priority": priority
                }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    wo["id"].as_str().expect("id").to_string()
}

async fn transition(ctx: &TestContext, token: &str, id: &str, body: Value) {
    let (status, resp) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/work-orders/{id}/transitions"),
                body.clone(),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body} 失敗：{resp}");
}

/// 昨天到今天的報表。每個測試有自己的資料庫、而 009 不建工單，
/// 因此窗口裡只有本測試建的工單。
///
/// 刻意涵蓋兩天而不是只有今天：有些測試把 `created_at` 往前挪幾小時來造
/// 情境，而「今天」在 Asia/Taipei 的日界附近會讓那些工單掉出窗口 ——
/// 一個只在凌晨失敗的測試。
async fn report(ctx: &TestContext, token: &str, extra: &str) -> Value {
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let from = today - chrono::Duration::days(1);
    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/sla-compliance?from={from}&to={today}{extra}"
            )),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// 取出 `group_by=priority` 下某個優先度的那一列。
fn row<'a>(body: &'a Value, key: &str) -> &'a Value {
    body["data"]
        .as_array()
        .expect("data 應為陣列")
        .iter()
        .find(|r| r["group_key"] == key)
        .unwrap_or_else(|| panic!("找不到 group_key = {key}：{body}"))
}

/// 指定期間的報表（給需要把 `created_at` 往前挪的測試用）。
async fn report_since(ctx: &TestContext, token: &str, days_back: i64, extra: &str) -> Value {
    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let from = today - chrono::Duration::days(days_back);
    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/reports/sla-compliance?from={from}&to={today}{extra}"
            )),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// 清掉場域的班表，讓宣告 `business_hours_only` 的政策退回自然時間
/// （`sla_basis = 'NATURAL_FALLBACK'`）。
///
/// 038 之後這是 `strict` 與 `operational` **唯一**的行為差異來源：
/// 有班表的場域兩種模式都納入（期限本來就是營業時間意義下算的），
/// 只有設定不完整的場域才需要取捨。
async fn clear_operating_hours(ctx: &TestContext, facility_id: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("UPDATE fms.facilities SET operating_hours = '{}'::jsonb WHERE id = $1::uuid")
        .bind(facility_id)
        .execute(&mut *tx)
        .await
        .expect("清班表");
    tx.commit().await.expect("commit");
}

/// 把 due 推到過去。
async fn age_due(ctx: &TestContext, id: &str, past: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(&format!(
        "UPDATE fms.work_orders
            SET response_due_at   = clock_timestamp() - interval '{past}',
                resolution_due_at = clock_timestamp() - interval '{past}'
          WHERE id = $1::uuid"
    ))
    .bind(id)
    .execute(&mut *tx)
    .await
    .expect("推 due");
    tx.commit().await.expect("commit");
}

// =============================================================================
// 核心：假數字不再出現
// =============================================================================

/// **本檔最重要的測試。**
///
/// 三種不該進分母的工單各建一張，然後斷言分母是 0 且達成率是 `null`。
///
/// 032 之前這三張裡的第一張（完成但沒有 policy）會是 `MET`，
/// 於是報表回 100%。而 100% 與「沒有可判定的工單」在一份 PDF 上
/// 看起來完全一樣。
#[tokio::test]
async fn nothing_decidable_yields_null_not_a_hundred_percent() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // (1) LOW：解析不到 policy（種子只有 CRITICAL/HIGH/MEDIUM）。做完它。
    let no_policy = create_wo(ctx, &token, "LOW").await;
    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "COMPLETE", "resolution_notes": "修好了" }),
    ] {
        transition(ctx, &token, &no_policy, action).await;
    }

    // (2) CRITICAL：剛開，還沒逾期也還沒做完 → 尚不可判定。
    create_wo(ctx, &token, "CRITICAL").await;

    // (3) HIGH：取消，**而且時限已過**。
    //
    // 把 due 推到過去是必要的：第一版沒推，於是那張工單本來就還沒逾期，
    // 「排除已取消」這個守衛根本沒被測到 —— 突變測試（拿掉 NOT abandoned）
    // 九個測試全部照過。沒有做完不代表逾期，而這才是能證明它的資料。
    let cancelled = create_wo(ctx, &token, "HIGH").await;
    transition(
        ctx,
        &token,
        &cancelled,
        json!({ "action": "CANCEL", "reason": "誤報" }),
    )
    .await;
    age_due(ctx, &cancelled, "3 hours").await;

    let body = report(ctx, &token, "&group_by=priority").await;

    let low = row(&body, "LOW");
    assert_eq!(low["excluded_no_policy"], 1, "{low}");
    assert_eq!(low["resolution_total"], 0, "沒有目標就不進分母：{low}");
    assert!(
        low["resolution_compliance_pct"].is_null(),
        "分母為 0 應回 null —— 0 會像災難、100 會像完美，兩者都會被拿去做決定：{low}"
    );

    let critical = row(&body, "CRITICAL");
    assert_eq!(critical["excluded_in_flight"], 1, "{critical}");
    assert_eq!(critical["resolution_total"], 0, "{critical}");

    let high = row(&body, "HIGH");
    assert_eq!(high["excluded_abandoned"], 1, "{high}");
    assert_eq!(high["resolution_total"], 0, "{high}");
    assert_eq!(
        high["response_total"], 0,
        "取消的工單也不進回應分母：{high}"
    );

    ctx.teardown().await;
}

/// 準時與逾期各一，達成率是 50%。
#[tokio::test]
async fn met_and_breached_are_counted_separately() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 準時完成
    let ok = create_wo(ctx, &token, "MEDIUM").await;
    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "COMPLETE", "resolution_notes": "準時" }),
    ] {
        transition(ctx, &token, &ok, action).await;
    }

    // 逾期完成
    let late = create_wo(ctx, &token, "MEDIUM").await;
    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
    ] {
        transition(ctx, &token, &late, action).await;
    }
    age_due(ctx, &late, "3 hours").await;
    transition(
        ctx,
        &token,
        &late,
        json!({ "action": "COMPLETE", "resolution_notes": "遲了" }),
    )
    .await;

    // **用 strict**。MEDIUM 的政策宣告 business_hours_only，而 034 當時會
    // 整批排除它 —— 也就是說多數工單從來不出現在合約報表裡。038／039 之後
    // 期限本身就是營業時間意義下算的，因此它們正常進入 strict。
    let body = report(ctx, &token, "&group_by=priority&strictness=strict").await;
    let medium = row(&body, "MEDIUM");

    assert_eq!(medium["resolution_total"], 2, "{medium}");
    assert_eq!(medium["resolution_met"], 1, "{medium}");
    assert_eq!(medium["resolution_breached"], 1, "{medium}");
    assert_eq!(medium["resolution_compliance_pct"], 50.0, "{medium}");
    assert_eq!(
        medium["excluded_business_hours"], 0,
        "總部有班表，因此不該被排除：{medium}"
    );
    assert_eq!(medium["substituted_business_hours"], 0, "{medium}");

    ctx.teardown().await;
}

/// `strict` 排除 `business_hours_only` 的 policy，`operational` 納入 ——
/// 而**兩者都必須說出有幾張被這樣處理**。
///
/// 一個沒有附上排除數的達成率，無法判斷它是不是被挑選過的。
#[tokio::test]
async fn strictness_changes_the_denominator_and_says_so() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 038 之後 strict 不再排除「宣告了 business_hours_only」的政策 ——
    // 那些期限現在算得出來。剩下的缺口是「政策要營業時間、場域沒班表」，
    // 因此這個測試要先製造那個缺口。
    clear_operating_hours(ctx, FACILITY_HQ).await;

    let id = create_wo(ctx, &token, "MEDIUM").await;
    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "COMPLETE", "resolution_notes": "好了" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }

    let strict = report(ctx, &token, "&group_by=priority&strictness=strict").await;
    let m = row(&strict, "MEDIUM");
    assert_eq!(
        m["resolution_total"], 0,
        "政策要營業時間但場域沒班表 → strict 排除：{m}"
    );
    assert_eq!(m["excluded_business_hours"], 1, "而且要說出來：{m}");
    assert_eq!(m["substituted_business_hours"], 0, "{m}");
    assert!(m["resolution_compliance_pct"].is_null(), "{m}");

    let lenient = report(ctx, &token, "&group_by=priority&strictness=operational").await;
    let m = row(&lenient, "MEDIUM");
    assert_eq!(m["resolution_total"], 1, "operational 模式納入：{m}");
    assert_eq!(m["excluded_business_hours"], 0, "{m}");
    assert_eq!(m["substituted_business_hours"], 1, "並標示代算：{m}");
    assert_eq!(m["resolution_compliance_pct"], 100.0, "{m}");

    assert_eq!(strict["meta"]["strictness"], "strict");
    assert_eq!(lenient["meta"]["strictness"], "operational");

    ctx.teardown().await;
}

/// 兩個平均值是**牆鐘時間**，而回應會這樣標示自己。
///
/// 038 之後 MEDIUM 的期限是用營業時間算的，於是一張週五晚上開、週一上午
/// 修好的工單：**達成率是達成，平均解決時間是 2296 分鐘**。看起來像系統
/// 很慢，實際上那包含了兩個晚上和一個週末。
///
/// 這個測試做兩件事：
///   1. 斷言 `meta.minutes_basis` 說出了單位
///   2. **把那個標籤與實際計算釘在一起** —— 斷言平均值精確等於
///      `completed_at - created_at` 的牆鐘分鐘數
///
/// 第 2 點是重點。標籤住在 Rust（`MINUTES_BASIS_WALLCLOCK`），而計算住在
/// SQL（034 的兩個 `avg`）—— 兩個檔案。少了這個斷言，日後有人把 avg 改成
/// 營業分鐘而忘了改標籤，回應就會**自稱牆鐘而其實不是** ——
/// 那正是這整個功能要避免的那種誤讀，只是換了個方向。
#[tokio::test]
async fn the_averages_are_wallclock_and_labelled_as_such() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // MEDIUM → SLA_STANDARD（business_hours_only = true）。
    let id = create_wo(ctx, &token, "MEDIUM").await;
    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "COMPLETE", "resolution_notes": "修好了" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }

    // 把開單時刻往前挪 30 小時。牆鐘差是 1800 分鐘；營業分鐘無論今天星期幾
    // 都遠小於它（總部一天最多 13 小時 = 780 分鐘）。因此這兩種單位
    // **在數值上分得開**，測試不會因為兩者剛好相同而變成空的。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_orders
                SET created_at = completed_at - interval '30 hours'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("挪 created_at");
        tx.commit().await.expect("commit");
    }

    // 期望值直接從那一列算出來 —— 不寫死 1800，那樣連「挪了幾小時」
    // 改動時測試也還是對的。
    let expected: f64 = {
        let mut tx = ctx.owner_tx().await;
        sqlx::query_scalar(
            "SELECT round(extract(epoch FROM (completed_at - created_at)) / 60.0, 1)::float8
               FROM fms.work_orders WHERE id = $1::uuid",
        )
        .bind(&id)
        .fetch_one(&mut *tx)
        .await
        .expect("算牆鐘差")
    };
    assert!(
        expected > 1000.0,
        "前提：牆鐘差要大到與營業分鐘分得開（{expected}）"
    );

    let body = report_since(ctx, &token, 3, "&group_by=priority").await;
    assert_eq!(
        body["meta"]["minutes_basis"], "WALLCLOCK",
        "回應要說出平均值的單位：{}",
        body["meta"]
    );

    let m = row(&body, "MEDIUM");
    assert_eq!(
        m["avg_resolution_minutes"], expected,
        "平均值必須正好是牆鐘差 —— 若改成營業分鐘，meta.minutes_basis 也要跟著改：{m}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 決定 G：回應與解決不同分母
// =============================================================================

/// PM 產生的工單計入解決、不計入回應。
///
/// 「多久有人回應」對一張三個月前就排好的保養單沒有意義，但「準不準時
/// 做完」有意義。因此兩個指標的分母不同 —— 這也是為什麼契約刻意
/// **沒有**單一的 `compliance_pct`。
#[tokio::test]
async fn pm_work_orders_count_for_resolution_but_not_response() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // API 無法直接建 PM_PLAN 來源的工單（那是產生器的事），因此改寫 source。
    // 觸發器已經在 INSERT 時算好 due，改 source 不影響它們。
    let id = create_wo(ctx, &token, "CRITICAL").await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.work_orders SET source = 'PM_PLAN' WHERE id = $1::uuid")
            .bind(&id)
            .execute(&mut *tx)
            .await
            .expect("改 source");
        tx.commit().await.expect("commit");
    }
    age_due(ctx, &id, "3 hours").await;

    let body = report(ctx, &token, "&group_by=priority").await;
    let c = row(&body, "CRITICAL");

    assert_eq!(
        c["resolution_total"], 1,
        "PM 工單仍計入解決指標（準不準時做完是有意義的）：{c}"
    );
    assert_eq!(c["resolution_breached"], 1, "{c}");
    assert_eq!(
        c["response_total"], 0,
        "PM 工單不計入回應指標（決定 G）：{c}"
    );
    assert_eq!(
        c["excluded_pm_response"], 1,
        "而且要說出被排除幾張，否則兩個分母的差異看不出來：{c}"
    );

    ctx.teardown().await;
}

/// 從未有人接下、且已過回應時限 → 算逾回應，**但不進平均**。
///
/// 它沒有回應時長可以平均。把它當 0 會讓平均值變好看，
/// 當「窗口長度」則是憑空發明一個數字。分母裡有它、平均裡沒有它。
#[tokio::test]
async fn a_never_answered_breach_counts_in_the_rate_but_not_the_average() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 從未派工的 CRITICAL，時限已過。
    let never = create_wo(ctx, &token, "CRITICAL").await;
    age_due(ctx, &never, "3 hours").await;

    // 另一張立刻派工的。**這張是關鍵**：只有從未回應的那一張時，
    // 平均值是 NULL，而「把 NULL 當 0」與「忽略 NULL」都會得到 NULL ——
    // 兩種實作分不出來。有一張真實的回應時長之後，
    // 把從未回應者當 0 會讓平均掉一半，測得出來。
    let answered = create_wo(ctx, &token, "CRITICAL").await;
    transition(
        ctx,
        &token,
        &answered,
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
    )
    .await;
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_orders
                SET created_at = clock_timestamp() - interval '10 minutes'
              WHERE id = $1::uuid",
        )
        .bind(&answered)
        .execute(&mut *tx)
        .await
        .expect("推 created_at");
        tx.commit().await.expect("commit");
    }

    let body = report(ctx, &token, "&group_by=priority").await;
    let c = row(&body, "CRITICAL");

    assert_eq!(c["response_total"], 2, "兩張都可判定：{c}");
    assert_eq!(c["response_breached"], 1, "{c}");
    assert_eq!(c["response_met"], 1, "{c}");
    assert_eq!(c["response_compliance_pct"], 50.0, "{c}");

    let avg = c["avg_response_minutes"]
        .as_f64()
        .unwrap_or_else(|| panic!("有一張已回應，應有平均：{c}"));
    assert!(
        avg >= 9.0,
        "平均只該算已回應的那一張（約 10 分）；把從未回應者當 0 會掉到約 5 分（實際 {avg}）：{c}"
    );

    ctx.teardown().await;
}

// =============================================================================
// 決定 D：等待不停錶，但看得見
// =============================================================================

/// 等待時長要出現在報表裡，即使它已經計入了達成率。
///
/// 決定 D 選了「不停錶」，代價是等料／等廠商的時間會算在維修方頭上。
/// 那個取捨只有在等待時長看得見的時候才是誠實的 ——
/// 否則就是「有停錶、只是沒寫在文件裡」的反面：沒停錶，也沒人知道。
#[tokio::test]
async fn waiting_time_is_reported_even_though_the_clock_kept_running() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let id = create_wo(ctx, &token, "MEDIUM").await;
    for action in [
        json!({ "action": "ASSIGN", "assignee_id": TECH_WANG }),
        json!({ "action": "START_WORK" }),
        json!({ "action": "WAIT_PARTS", "reason": "等壓縮機" }),
    ] {
        transition(ctx, &token, &id, action).await;
    }

    // 造出「已經等了兩小時」。
    //
    // 只推 WAIT_PARTS 那一筆是不夠的 —— 第一版就是這樣寫的，結果
    // `ASSIGN` 與 `START_WORK` 仍停在 now，成了 `max(occurred_at)`，
    // 於是「還在等多久」算出來趨近 0。整段歷史必須一起挪，
    // 而且進入 WAITING 的那一筆要仍然是最後一筆。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.work_orders
                SET created_at = clock_timestamp() - interval '3 hours'
              WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("推 created_at");
        sqlx::query(
            "UPDATE fms.work_order_transitions
                SET occurred_at = CASE
                      WHEN action = 'WAIT_PARTS' THEN clock_timestamp() - interval '2 hours'
                      ELSE clock_timestamp() - interval '3 hours' END
              WHERE work_order_id = $1::uuid",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("推轉移時刻");
        tx.commit().await.expect("commit");
    }

    let body = report(ctx, &token, "&group_by=priority&strictness=operational").await;
    let m = row(&body, "MEDIUM");

    let waited = m["avg_waiting_minutes"]
        .as_f64()
        .unwrap_or_else(|| panic!("avg_waiting_minutes 應有值：{m}"));
    assert!(
        waited >= 115.0,
        "還停在 WAITING_PARTS 的那一段也要算進去，否則卡三天的工單會顯示等待 0 分鐘（實際 {waited}）"
    );

    ctx.teardown().await;
}

// =============================================================================
// 參數與範圍
// =============================================================================

/// 未知的 `group_by` 回 422，不是一份看起來合理的報表。
///
/// 034 的 `CASE p_group_by` 沒有 ELSE —— 未知值會讓 `group_key` 整欄變成
/// NULL，也就是「一個叫做『全部』的分組」。那是個靜默的錯誤答案。
#[tokio::test]
async fn an_unknown_group_by_is_rejected() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let today = chrono::Utc::now().date_naive();

    for (param, field) in [
        ("&group_by=assignee", "group_by"),
        ("&strictness=lenient", "strictness"),
    ] {
        let (status, body) = ctx
            .send(authed(
                get(&format!(
                    "/api/v1/reports/sla-compliance?from={today}&to={today}{param}"
                )),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{param} 應被拒絕：{body}"
        );
        assert_eq!(body["errors"][0]["pointer"], format!("/{field}"), "{body}");
    }

    ctx.teardown().await;
}

/// `from > to` 回 422 而不是一份空報表。
///
/// 空集合與「這段期間真的沒有工單」長得一模一樣。
#[tokio::test]
async fn a_reversed_date_range_is_rejected() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(
            get("/api/v1/reports/sla-compliance?from=2026-08-31&to=2026-08-01"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["errors"][0]["code"], "RANGE", "{body}");

    ctx.teardown().await;
}

/// 場域範圍的使用者只算到自己看得見的工單 —— 由 RLS 完成，不是應用層。
///
/// 034 的函式是 `SECURITY INVOKER`。若日後有人為了「效能」把它改成
/// `SECURITY DEFINER`，這個測試會變成跨租戶／跨場域的洩漏偵測器。
#[tokio::test]
async fn the_report_is_scoped_by_rls_not_by_the_handler() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login().await;

    // 影廳（fm.lin 的範圍是總部）的工單。
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": "cccccccc-0000-4000-8000-000000000002",
                    "spatial_node_id": "10000000-0000-4000-8000-000000000013",
                    "title": "影廳的工單",
                    "priority": "CRITICAL"
                }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");

    // 總部的工單。
    create_wo(ctx, &admin, "CRITICAL").await;

    // 租戶管理員兩個場域都看得到。
    let all = report(ctx, &admin, "&group_by=facility").await;
    let facilities = all["data"].as_array().expect("data").len();
    assert!(facilities >= 2, "TENANT_ADMIN 應看到兩個場域的分組：{all}");

    // 場域管理員只看得到總部。
    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let scoped = report(ctx, &fm, "&group_by=facility").await;
    let keys: Vec<&str> = scoped["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|r| r["group_key"].as_str())
        .collect();
    assert!(keys.contains(&FACILITY_HQ), "應看到自己的場域：{scoped}");
    assert!(
        !keys.contains(&"cccccccc-0000-4000-8000-000000000002"),
        "不該看到範圍外的場域 —— RLS 應在函式內就濾掉：{scoped}"
    );

    ctx.teardown().await;
}
