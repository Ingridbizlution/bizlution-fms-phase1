//! Service Catalogue 的管理面四支。
//!
//! # `d_` 是這一組最重要的一格
//!
//! `availability` 的鍵打錯字**很危險**：`blackout_date`（少一個 s）在寬鬆的
//! 形狀驗證下會被靜默忽略，於是那些停止服務日一天都沒有生效 —— 而設定畫面上
//! 看起來是對的。migration 068 因此對這個欄位用**嚴格**的形狀（與
//! `tenants.settings` 相反，理由見 068 的檔頭），而 `d_` 釘住那個決定。
//!
//! # `c_` 盯的是 PATCH 的後門
//!
//! POST 擋掉「可收費但沒有單價」。若 PATCH 只驗送來的欄位，那麼
//! `PATCH {"chargeable": true}` 就繞過了同一條規則 —— 而結果是一個會產出
//! 金額不明帳單的服務項目。`c_` 走那條路徑。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Datelike;
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn req(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn create(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request(
            "POST",
            &format!("/api/v1/facilities/{FACILITY_HQ}/service-items"),
            body,
        ),
        token,
    ))
    .await
}

fn minimal(code: &str) -> Value {
    json!({ "category": "CLEANING", "code": code, "name": "測試服務" })
}

/// 建立、更新、停用一輪走完，而停用會說出還有幾張未結工單。
#[tokio::test]
async fn a_create_patch_and_deactivate_round_trip() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, created) = create(
        ctx,
        &token,
        json!({
            "category": "CATERING",
            "code": "COFFEE_SVC",
            "name": "會議咖啡",
            "lead_time_minutes": 120,
            "default_duration_minutes": 15,
            "chargeable": true,
            "unit_price": 60.0,
            "currency": "TWD",
            "unit_label": "per cup",
            "max_quantity": 50,
            "availability": { "mon": [["08:00", "17:00"]] }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let d = &created["data"];
    assert_eq!(d["code"], "COFFEE_SVC");
    assert_eq!(d["lead_time_minutes"], 120);
    assert_eq!(d["unit_price"], 60.0);
    assert_eq!(d["is_active"], true);
    // 管理面才看得到的欄位。
    assert!(d["availability"].is_object(), "{d}");
    assert_eq!(created["meta"]["applies_to_all_facilities"], false);
    assert_eq!(created["meta"]["authorized_via_facility"], FACILITY_HQ);
    let id = d["id"].as_str().expect("id").to_string();

    // 出現在型錄清單裡。
    let (_, listed) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_HQ}/service-items")),
            &token,
        ))
        .await;
    assert!(
        listed["data"]
            .as_array()
            .unwrap_or_else(|| panic!(
                "型錄清單回了 500。**這是一個既有缺陷**：`sla_policies` 的\
                 兩欄來自 LEFT JOIN 而 schema 是 NOT NULL，sqlx 因此推論成非空，\
                 於是任何沒有 sla_policy_id 的服務項目都會讓整個清單炸掉。\
                 示範資料的三個項目剛好都有 SLA 政策，所以在 POST 存在之前\
                 這條路徑不可達：{listed}"
            ))
            .iter()
            .any(|r| r["code"] == "COFFEE_SVC"),
        "剛建的項目該出現在型錄裡：{}",
        listed["data"]
    );

    // PATCH：改幾個欄位，沒送的不動，送 null 清空。
    let (status, updated) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/service-items/{id}"),
                json!({ "name": "會議咖啡（大杯）", "unit_label": null }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["data"]["name"], "會議咖啡（大杯）");
    assert_eq!(
        updated["data"]["unit_label"],
        Value::Null,
        "送 null 該清空：{}",
        updated["data"]
    );
    assert_eq!(
        updated["data"]["lead_time_minutes"], 120,
        "沒送的欄位不該被重設：{}",
        updated["data"]
    );

    // 掛一張未結的 SERVICE 工單，讓停用時的計數有東西可數。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.work_orders
               (tenant_id, facility_id, wo_no, work_order_type, source, title,
                status, priority, spatial_node_id, service_item_id)
             SELECT $1::uuid, $2::uuid, 'WO-SVC-' || substr(md5(random()::text), 1, 8),
                    'SERVICE', 'MANUAL', '咖啡', 'IN_PROGRESS', 'MEDIUM',
                    (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                    $3::uuid",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_HQ)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("建服務工單");
        tx.commit().await.expect("commit");
    }

    // 停用：軟刪除，而且**說出還有幾張未結工單**。
    let (status, deleted) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/service-items/{id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["data"]["deleted"], true);
    assert_eq!(
        deleted["data"]["open_work_orders"], 1,
        "**停用要說出還有幾張未結工單** —— 20 張與 0 張的處理方式完全不同，\
         而 204 不帶任何資訊：{}",
        deleted["data"]
    );
    assert_eq!(deleted["meta"]["soft_delete"], true);

    // 停用之後型錄看不到，但工單還在（軟刪除的重點）。
    let (_, after) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/facilities/{FACILITY_HQ}/service-items?limit=100"
            )),
            &token,
        ))
        .await;
    assert!(
        !after["data"]
            .as_array()
            .expect("data")
            .iter()
            .any(|r| r["code"] == "COFFEE_SVC"),
        "停用後不該出現在型錄裡"
    );
    let mut tx = ctx.owner_tx().await;
    let still: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fms.work_orders WHERE service_item_id = $1::uuid")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .expect("查工單");
    tx.commit().await.expect("commit");
    assert_eq!(still, 1, "**軟刪除的重點**：既有工單仍然指得到它是什麼服務");

    // 停用之後 code 釋放了（唯一索引帶 WHERE deleted_at IS NULL）。
    let (status, again) = create(ctx, &token, minimal("COFFEE_SVC")).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "停用的項目不該佔用 code：{again}"
    );

    // 已停用的再停用 → 404。
    let (status, _) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/service-items/{id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// 建立時的兩條實質驗證，加上 code 重複與 category 白名單。
#[tokio::test]
async fn b_creation_rejects_items_that_could_not_work() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // **可收費但沒有單價** → 422。
    let (status, p) = create(
        ctx,
        &token,
        json!({ "category": "CLEANING", "code": "NO_PRICE", "name": "x",
                "chargeable": true }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "可收費但沒有單價會產出一張金額不明的帳單：{p}"
    );
    assert_eq!(p["errors"][0]["pointer"], "/unit_price", "{p}");

    // 有單價但沒有幣別 → 422。
    let (status, _) = create(
        ctx,
        &token,
        json!({ "category": "CLEANING", "code": "NO_CCY", "name": "x",
                "chargeable": true, "unit_price": 10.0 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // **兩個入口都關掉** → 422（那樣的項目沒有任何入口可以申請）。
    let (status, p) = create(
        ctx,
        &token,
        json!({ "category": "CLEANING", "code": "NO_ENTRY", "name": "x",
                "is_attachable_to_reservation": false,
                "is_standalone_requestable": false }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "兩個入口都 false 建出來的是一個沒有用的列：{p}"
    );

    // category 白名單。
    let (status, _) = create(
        ctx,
        &token,
        json!({ "category": "COFFEE", "code": "BAD_CAT", "name": "x" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 未知欄位 → 422（deny_unknown_fields）。
    let (status, _) = create(
        ctx,
        &token,
        json!({ "category": "CLEANING", "code": "TYPO", "name": "x",
                "lead_time_minute": 30 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // code 重複 → 409（不是 422 —— 請求本身沒問題，是狀態衝突）。
    let (status, _) = create(ctx, &token, minimal("DUP_CODE")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, p) = create(ctx, &token, minimal("DUP_CODE")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{p}");

    // 全場域項目：facility_id 存 NULL，而權限用路徑上的場域。
    let (status, all) = create(
        ctx,
        &token,
        json!({ "category": "IT_SUPPORT", "code": "GLOBAL_IT", "name": "全租戶 IT",
                "applies_to_all_facilities": true }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{all}");
    assert_eq!(all["data"]["facility_id"], Value::Null);
    assert_eq!(all["meta"]["applies_to_all_facilities"], true);

    ctx.teardown().await;
}

/// **PATCH 不能是價格規則的後門，`code`／`facility_id` 不可變更。**
#[tokio::test]
async fn c_patch_validates_the_merged_value_not_just_the_payload() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 一個不可收費、沒有單價的項目。
    let (_, created) = create(ctx, &token, minimal("MERGE_TEST")).await;
    let id = created["data"]["id"].as_str().expect("id").to_string();
    let uri = format!("/api/v1/service-items/{id}");

    // **只送 chargeable: true → 422。** 資料庫裡沒有單價，合併後就是
    // 「可收費但金額不明」—— 那正是 POST 擋掉的情況。
    let (status, p) = ctx
        .send(authed(
            json_request("PATCH", &uri, json!({ "chargeable": true })),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "**PATCH 不該是 POST 那條規則的後門。** 只驗送來的欄位會讓\
         `{{\"chargeable\": true}}` 建出一個會產出金額不明帳單的項目：{p}"
    );

    // 一起送就可以。
    let (status, ok) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &uri,
                json!({ "chargeable": true, "unit_price": 25.0, "currency": "TWD" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{ok}");

    // **把單價清空但保持可收費 → 422**（反方向的同一條規則）。
    let (status, p) = ctx
        .send(authed(
            json_request("PATCH", &uri, json!({ "unit_price": null })),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "清空單價而項目仍可收費，結果一樣是金額不明：{p}"
    );

    // 入口旗標的合併值也不能都 false：先關一個，再關另一個。
    let (status, _) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &uri,
                json!({ "is_attachable_to_reservation": false }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "關一個是合法的");
    let (status, p) = ctx
        .send(authed(
            json_request("PATCH", &uri, json!({ "is_standalone_requestable": false })),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "合併後兩個入口都關掉，該擋：{p}"
    );

    // `code`／`facility_id` 不可變更。
    for f in ["code", "facility_id", "id", "tenant_id"] {
        let (status, p) = ctx
            .send(authed(
                json_request("PATCH", &uri, json!({ f: "x" })),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{f}` 不該可改：{p}"
        );
        assert_eq!(p["errors"][0]["code"], "IMMUTABLE", "{p}");
    }

    // 不存在 → 404。
    let (status, _) = ctx
        .send(authed(
            json_request(
                "PATCH",
                "/api/v1/service-items/00000000-0000-4000-8000-000000000000",
                json!({ "name": "x" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// **`availability` 的打錯字會被擋，而空陣列的三種原因分得開。**
#[tokio::test]
async fn d_availability_typos_are_rejected_and_empty_days_say_why() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 打錯字的鍵 → 422。這是 068 用嚴格形狀的理由：`blackout_date` 少一個 s
    // 在寬鬆的版本下會被靜默忽略，於是停止服務日一天都沒有生效。
    // 用索引產生 code：原本用 `bad.to_string().len()`，而其中兩個剛好一樣長
    // —— 那會讓第二個得到 409（code 重複）而不是 422，於是這一格測到的是
    // 唯一約束而不是形狀約束。
    for (i, bad) in [
        json!({ "blackout_date": ["2026-01-01"] }),
        json!({ "monday": [["08:00", "17:00"]] }),
        json!({ "mon": [["8:0", "17:00"]] }),
        json!({ "mon": [["17:00", "08:00"]] }),
        json!({ "blackout_dates": ["2026-02-30"] }),
        json!({ "blackout_dates": "2026-01-01" }),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, p) = create(
            ctx,
            &token,
            json!({ "category": "CLEANING", "code": format!("BAD_SHAPE_{i}"),
                    "name": "x", "availability": bad }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{bad}` 該被 ck_service_items_availability 擋下 —— \
             放行它會讓設定看起來生效而實際沒有：{p}"
        );
        assert_eq!(p["errors"][0]["pointer"], "/availability", "{p}");
    }

    // ---- 合法的設定，逐日解析 ----
    // 週一 08:00–17:00，其餘星期不設；下週一放假。
    let next_monday = {
        let today = chrono::Utc::now().date_naive();
        let days_ahead = (8 - today.weekday().num_days_from_monday() as i64) % 7;
        today + chrono::Duration::days(if days_ahead == 0 { 7 } else { days_ahead })
    };
    let (status, created) = create(
        ctx,
        &token,
        json!({
            "category": "CLEANING", "code": "AVAIL_TEST", "name": "清潔",
            "lead_time_minutes": 2880,
            "availability": {
                "mon": [["08:00", "17:00"]],
                "blackout_dates": [next_monday.to_string()]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["data"]["id"].as_str().expect("id").to_string();

    let (status, avail) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/service-items/{id}/availability?from={}&days=14",
                chrono::Utc::now().date_naive()
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{avail}");
    let days = avail["data"].as_array().expect("data");
    assert_eq!(days.len(), 14, "要回 14 天");

    // 下週一：**blackout 蓋過星期表**（星期表說週一有時段）。
    let blackout_day = days
        .iter()
        .find(|d| d["date"].as_str() == Some(&next_monday.to_string()))
        .expect("該有下週一那一天");
    assert_eq!(
        blackout_day["basis"], "blackout_date",
        "停止服務日要蓋過星期表 —— 例外的意義就是蓋過常規：{blackout_day}"
    );
    assert_eq!(blackout_day["is_blackout"], true);
    assert_eq!(
        blackout_day["windows"].as_array().map(Vec::len),
        Some(0),
        "{blackout_day}"
    );

    // 其他的週一：走服務自己的星期表。
    let other_monday = days
        .iter()
        .find(|d| d["basis"] == "service_item.availability" && d["is_blackout"] == false);
    assert!(
        other_monday.is_some(),
        "該有至少一個走服務自己星期表的日子：{}",
        avail["data"]
    );

    // 非週一：服務沒設定那個星期 → 退回場域的營運時間。
    let fell_back = days
        .iter()
        .find(|d| d["basis"] == "facility.operating_hours");
    assert!(
        fell_back.is_some(),
        "**沒設定的星期要退回場域的營運時間**，而 basis 要說出來 —— \
         少了它，一個空陣列與「今天停止服務」長得一樣：{}",
        avail["data"]
    );

    // 提前量與時段是兩件事。
    let m = &avail["meta"];
    assert_eq!(m["lead_time_minutes"], 2880, "{m}");
    assert!(
        m["earliest_requestable_at"].as_str().is_some(),
        "**要算出最早可申請的時刻** —— 時段告訴使用者哪幾小時開放，\
         這個告訴他最早能訂到什麼時候：{m}"
    );
    assert!(m["open_days"].as_i64().is_some_and(|v| v > 0), "{m}");

    // days 超出範圍 → 422。
    for bad in [0, 32] {
        let (status, _) = ctx
            .send(authed(
                get(&format!(
                    "/api/v1/service-items/{id}/availability?days={bad}"
                )),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "days={bad}");
    }

    // 不存在 → 404。
    let (status, _) = ctx
        .send(authed(
            get("/api/v1/service-items/00000000-0000-4000-8000-000000000000/availability"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// 全場域項目沒有場域可退，而那個原因要具名。
#[tokio::test]
async fn e_a_tenant_wide_item_has_no_facility_to_fall_back_to() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (_, created) = create(
        ctx,
        &token,
        json!({ "category": "IT_SUPPORT", "code": "GLOBAL_NO_HOURS",
                "name": "全租戶 IT", "applies_to_all_facilities": true }),
    )
    .await;
    let id = created["data"]["id"].as_str().expect("id").to_string();

    let (status, avail) = ctx
        .send(authed(
            get(&format!("/api/v1/service-items/{id}/availability?days=3")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{avail}");
    for d in avail["data"].as_array().expect("data") {
        assert_eq!(
            d["basis"], "no_facility_to_fall_back_to",
            "**這個原因要具名。** 一個沒設定的全租戶項目回空陣列，\
             與「今天停止服務」是完全不同的事 —— 前者要去問管理員：{d}"
        );
        assert_eq!(d["is_blackout"], false, "沒設定不等於停止服務：{d}");
    }
    assert_eq!(avail["meta"]["open_days"], 0, "0 天是有意義的答案");

    ctx.teardown().await;
}

/// 權限：`service_item:write` 有擋。
#[tokio::test]
async fn f_writing_requires_the_write_permission() {
    let ctx = &TestContext::setup().await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    let (status, _) = create(ctx, &requester, minimal("NO_PERM")).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "REQUESTER 沒有 service_item:write"
    );

    // 讀得到型錄（那是 service_item:read）—— 兩者是不同的權限。
    let admin = ctx.login().await;
    let (_, created) = create(ctx, &admin, minimal("READ_ONLY_TEST")).await;
    let id = created["data"]["id"].as_str().expect("id").to_string();

    let (status, _) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/service-items/{id}"),
                json!({ "name": "x" }),
            ),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/service-items/{id}")),
            &requester,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    ctx.teardown().await;
}
