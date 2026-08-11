//! 租戶設定（`GET /tenant`、`PATCH /tenant`）。
//!
//! # `c_` 是這一組唯一真正重要的一格
//!
//! `fms.tenants` 有 18 個欄位，而 `quota_assets`、`plan_tier` 這些名字看起來
//! 就是「設定」。**可寫等於讓客戶自己解除配額上限、自己升級方案。**
//! `c_` 逐一嘗試每一個合約欄位，斷言 422 且值沒有被改。
//!
//! 少了它，一個「把白名單寫成黑名單的補集」的重構會安靜地打開那些欄位 ——
//! 而唯一的症狀是某個客戶的配額突然變成他自己填的數字。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

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

async fn patch(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(json_request("PATCH", "/api/v1/tenant", body), token))
        .await
}

/// GET 回全部欄位，含唯讀的合約那組，並列出改不了的欄位。
#[tokio::test]
async fn a_get_returns_every_field_and_says_which_are_read_only() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, body) = ctx.send(authed(get("/api/v1/tenant"), &token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let d = &body["data"];
    assert_eq!(d["code"], TENANT_CODE);
    // 合約那組也要回 —— 讀得到自己買了什麼是合理的，只是改不了。
    for f in [
        "plan_tier",
        "isolation_mode",
        "status",
        "quota_api_rps",
        "feature_flags",
    ] {
        assert!(!d[f].is_null(), "{f} 該有值：{d}");
    }
    // 租戶那組。
    for f in [
        "name",
        "default_timezone",
        "default_locale",
        "default_currency",
    ] {
        assert!(d[f].is_string(), "{f} 該是字串：{d}");
    }
    assert!(d["settings"].is_object(), "{d}");

    // meta 要列出改不了的欄位與理由 —— 前端不該自己維護一份會分歧的清單。
    let ro = body["meta"]["read_only_fields"]
        .as_array()
        .expect("read_only_fields");
    let names: Vec<&str> = ro.iter().filter_map(|x| x["field"].as_str()).collect();
    for f in [
        "plan_tier",
        "quota_assets",
        "feature_flags",
        "code",
        "industry",
    ] {
        assert!(names.contains(&f), "{f} 該在 read_only_fields：{names:?}");
    }
    assert!(
        ro.iter()
            .all(|x| x["reason"].as_str().is_some_and(|r| !r.is_empty())),
        "每一個唯讀欄位都要說出理由：{ro:?}"
    );

    ctx.teardown().await;
}

/// PATCH 改得動租戶擁有的欄位，而 `legal_name` 送 null 是清空。
#[tokio::test]
async fn b_patch_updates_the_fields_the_tenant_owns() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, updated) = patch(
        ctx,
        &token,
        json!({
            "name": "示範集團（改名後）",
            "legal_name": "示範集團股份有限公司",
            "default_locale": "en-US",
            "default_currency": "USD",
            "settings": { "satisfaction_editable_days": 21 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["data"]["name"], "示範集團（改名後）");
    assert_eq!(updated["data"]["legal_name"], "示範集團股份有限公司");
    assert_eq!(updated["data"]["default_locale"], "en-US");
    assert_eq!(updated["data"]["default_currency"], "USD");
    assert_eq!(
        updated["data"]["settings"]["satisfaction_editable_days"],
        21
    );

    // 沒送的欄位不動。
    let (_, again) = patch(ctx, &token, json!({ "default_locale": "zh-TW" })).await;
    assert_eq!(again["data"]["default_locale"], "zh-TW");
    assert_eq!(
        again["data"]["name"], "示範集團（改名後）",
        "沒送的欄位不該被清掉：{}",
        again["data"]
    );
    assert_eq!(
        again["data"]["settings"]["satisfaction_editable_days"], 21,
        "settings 也不該被沒送的請求清空"
    );

    // **送 null 是清空，不送是不動。** 兩者 serde 分不出來，所以
    // `legal_name` 用 `Option<Option<_>>` —— 少了那一層，清空是做不到的操作。
    let (status, cleared) = patch(ctx, &token, json!({ "legal_name": null })).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(
        cleared["data"]["legal_name"],
        Value::Null,
        "送 null 該把它清掉：{}",
        cleared["data"]
    );

    ctx.teardown().await;
}

/// **合約欄位一律拒絕，而且值不能被改。**
///
/// 這一格是那條界線的突變測試。
#[tokio::test]
async fn c_platform_managed_fields_are_rejected_not_silently_ignored() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 先記下原值，之後逐一比對。
    let (_, before) = ctx.send(authed(get("/api/v1/tenant"), &token)).await;
    let before = before["data"].clone();

    // 每一個都單獨送一次：混在一起的話，一個被擋下就看不出其他的行為。
    let attempts = [
        ("plan_tier", json!("ENTERPRISE")),
        ("isolation_mode", json!("DEDICATED")),
        ("status", json!("ACTIVE")),
        ("quota_api_rps", json!(999_999)),
        ("quota_assets", json!(999_999)),
        ("quota_users", json!(999_999)),
        ("contract_start_date", json!("2000-01-01")),
        ("contract_end_date", json!("2099-12-31")),
        ("feature_flags", json!({ "everything": true })),
        ("code", json!("HIJACKED")),
        ("industry", json!("HEALTHCARE")),
        ("id", json!("00000000-0000-4000-8000-000000000000")),
    ];

    for (field, value) in &attempts {
        let (status, p) = patch(ctx, &token, json!({ *field: value })).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "**`{field}` 不該可寫。** quota 可寫等於讓客戶自己解除上限，\
             plan_tier 可寫等於自己升級方案：{p}"
        );
        // 錯誤要指名欄位並說出理由 —— 靜默忽略會讓客戶以為改成功了。
        assert_eq!(
            p["errors"][0]["pointer"],
            format!("/{field}"),
            "要用 JSON Pointer 指名是哪個欄位：{p}"
        );
        assert_eq!(p["errors"][0]["code"], "PLATFORM_MANAGED", "{p}");
        assert!(
            p["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains(field) && m.len() > field.len() + 10),
            "訊息要說出為什麼，不只是「不可以」：{p}"
        );
    }

    // 值一個都沒變。
    let (_, after) = ctx.send(authed(get("/api/v1/tenant"), &token)).await;
    let after = after["data"].clone();
    for (field, _) in &attempts {
        assert_eq!(
            after[*field], before[*field],
            "`{field}` 被改掉了 —— 被拒的請求不該留下任何變更"
        );
    }

    // **混合請求整個被拒**：一個合法欄位配一個合約欄位，合法的那個也不該生效。
    // 部分套用會讓客戶端無法從 422 推斷實際狀態。
    let (status, mixed) = patch(
        ctx,
        &token,
        json!({ "name": "不該生效", "quota_assets": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{mixed}");
    let (_, check) = ctx.send(authed(get("/api/v1/tenant"), &token)).await;
    assert_ne!(
        check["data"]["name"], "不該生效",
        "混合請求要整個拒絕，不能部分套用：{}",
        check["data"]
    );

    ctx.teardown().await;
}

/// 打錯字的欄位、空請求、壞格式、壞 settings 各有自己的錯誤。
#[tokio::test]
async fn d_validation_distinguishes_typos_from_platform_fields() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 打錯字 → 422，而且 code 是 UNKNOWN_FIELD（不是 PLATFORM_MANAGED）。
    // 兩者是不同的事：一個是「沒有這個欄位」，一個是「有但不是你能改的」。
    let (status, p) = patch(ctx, &token, json!({ "default_locale2": "zh-TW" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(
        p["errors"][0]["code"], "UNKNOWN_FIELD",
        "打錯字與合約欄位要有不同的錯誤碼：{p}"
    );

    // 空的 PATCH → 422。回 200 會讓客戶端以為做了什麼。
    let (status, _) = patch(ctx, &token, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 不是物件 → 422。
    let (status, _) = patch(ctx, &token, json!([1, 2])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 貨幣格式。
    for bad in ["usd", "US", "USDD"] {
        let (status, p) = patch(ctx, &token, json!({ "default_currency": bad })).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{bad}` 不是合法的 ISO 4217：{p}"
        );
    }

    // **settings 的形狀由資料庫的約束擋，而錯誤要翻譯成 422 而不是 500。**
    let (status, p) = patch(
        ctx,
        &token,
        json!({ "settings": { "satisfaction_editable_days": "二十一" } }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "約束違反該翻譯成 422 —— 500 會讓客戶端以為是伺服器壞了：{p}"
    );
    assert_eq!(p["errors"][0]["pointer"], "/settings", "{p}");

    // 未知的鍵放行（那個欄位會長大）。
    let (status, ok) = patch(
        ctx,
        &token,
        json!({ "settings": { "some_future_key": [1, 2, 3] } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "未知的鍵該放行：{ok}");

    ctx.teardown().await;
}

/// 權限：`tenant:read`／`tenant:update` 是 TENANT 範圍。
#[tokio::test]
async fn e_both_endpoints_require_tenant_scoped_permissions() {
    let ctx = &TestContext::setup().await;

    // 場域級管理員沒有 tenant:read／tenant:update。
    for user in [USERNAME_FACILITY_ADMIN, USERNAME_REQUESTER] {
        let token = ctx.login_as(user).await;
        let (status, _) = ctx.send(authed(get("/api/v1/tenant"), &token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{user} 不該讀得到租戶設定");

        let (status, _) = patch(ctx, &token, json!({ "name": "x" })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{user} 不該改得動租戶設定");
    }

    ctx.teardown().await;
}
