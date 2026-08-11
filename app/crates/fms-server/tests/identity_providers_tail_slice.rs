//! `PATCH /identity-providers/{id}` 與 `GET /directory-groups`。
//!
//! # 這一組的重點不在「能不能改」
//!
//! 能改是容易的部分。三件難的：
//!
//!   1. **不可變更的欄位被拒之後，值真的沒有被改**（`b_`）。只斷言 422 會漏掉
//!      「先寫再檢查」那種寫法，而那時 422 是一個謊。
//!   2. **必填欄位的檢查對合併後的值進行**（`d_`）。`{"issuer": null}` 送給
//!      一個 OIDC 來源，只看請求的話看起來沒問題。
//!   3. **「填了但沒有人讀」有被說出來**（`h_`）。`identity_providers` 有一部分
//!      欄位在 Phase 1 沒有消費者，填完 `client_secret_ref` 之後 SSO 登入
//!      仍然無法完成（callback 回 501）。回一個乾淨的 200 會讓管理者以為接好了。
//!
//! # 種子本身就是一個缺口的實例
//!
//! 009 種了兩個目錄群組，**兩個的 `last_synced_at` 都是 NULL** —— 因為
//! Phase 1 沒有同步客戶端（migration 058 檔頭）。`m_` 因此斷言
//! `groups_never_synced` 等於群組總數：那不是測試在遷就實作，那是這個系統
//! 目前真實的狀態，而它應該在 API 回應裡看得見。

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

/// 依 code 取回一個種子身分來源的 id 與目前狀態。
async fn provider(ctx: &TestContext, token: &str, code: &str) -> Value {
    let (status, body) = ctx
        .send(authed(get("/api/v1/identity-providers?limit=50"), token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["code"] == json!(code))
        .unwrap_or_else(|| panic!("種子裡沒有 code = {code} 的身分來源：{body}"))
        .clone()
}

async fn patch(ctx: &TestContext, token: &str, id: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request("PATCH", &format!("/api/v1/identity-providers/{id}"), body),
        token,
    ))
    .await
}

/// 直接讀資料庫，繞過 API 的投影 —— 「被拒之後值有沒有被改」不能靠同一支
/// 端點的回應來判斷。
async fn raw_column(ctx: &TestContext, id: &str, column: &str) -> Option<String> {
    let mut tx = ctx.owner_tx().await;
    let v: Option<String> = sqlx::query_scalar(&format!(
        "SELECT {column}::text FROM fms.identity_providers WHERE id = $1::uuid"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("read column");
    drop(tx);
    v
}

// =============================================================================

/// 改名成功，而且回應是**更新後**的值。
#[tokio::test]
async fn a_patch_returns_the_updated_row() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let p = provider(ctx, &admin, "entra-hq").await;
    let id = p["id"].as_str().unwrap();

    let (status, body) = patch(ctx, &admin, id, json!({"name": "總部 Entra ID（改名）"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // 資料修改型 CTE 會回傳**更新前**的 snapshot（PostgreSQL 手冊 7.8.2），
    // 症狀是「儲存成功但畫面是舊值」。這一格守的是那件事。
    assert_eq!(
        body["data"]["name"],
        json!("總部 Entra ID（改名）"),
        "回應不是更新後的值：{body}"
    );
    assert_eq!(
        raw_column(ctx, id, "name").await.as_deref(),
        Some("總部 Entra ID（改名）")
    );

    ctx.teardown().await;
}

/// `code` 與 `provider_type` 被拒，**而且值沒有被改**。
///
/// 只斷言 422 會漏掉「先寫再檢查」—— 那時 422 是一個謊，而症狀要等到
/// 有人發現 SSO 連結壞了才出現。
#[tokio::test]
async fn b_identity_fields_are_rejected_and_nothing_changes() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let p = provider(ctx, &admin, "entra-hq").await;
    let id = p["id"].as_str().unwrap();

    for (field, value) in [
        ("code", json!("entra-hq-renamed")),
        ("provider_type", json!("LDAP")),
    ] {
        let (status, body) = patch(ctx, &admin, id, json!({field: value})).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{field}` 被接受了：{body}"
        );
        assert_eq!(
            body["errors"][0]["code"],
            json!("NOT_PATCHABLE"),
            "`{field}` 的錯誤碼不對：{body}"
        );
    }

    // 兩個欄位都還是原值。
    assert_eq!(
        raw_column(ctx, id, "code").await.as_deref(),
        Some("entra-hq")
    );
    assert_eq!(
        raw_column(ctx, id, "provider_type").await.as_deref(),
        Some("OIDC")
    );

    // 一個合法欄位與一個不可改欄位混在同一個請求裡 —— 整個請求都要被拒，
    // 不是只忽略後者。回 200 而其中一半沒生效，管理者會以為都寫進去了。
    let (status, body) = patch(ctx, &admin, id, json!({"name": "不該被寫入", "code": "x"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_ne!(
        raw_column(ctx, id, "name").await.as_deref(),
        Some("不該被寫入"),
        "混合請求被拒，但合法的那個欄位已經寫進去了"
    );

    ctx.teardown().await;
}

/// 「沒有消費者」的欄位被拒，而**打錯字的欄位得到不同的錯誤碼**。
///
/// 兩者壓成同一個錯誤的話，客戶端只會知道「有個欄位不對」——
/// 而處置完全不同（一個是改用別的方式、一個是修拼字）。
#[tokio::test]
async fn c_unreadable_fields_and_typos_are_told_apart() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let p = provider(ctx, &admin, "ad-cinema").await;
    let id = p["id"].as_str().unwrap();

    // 真實存在但沒有讀者的欄位。
    //
    // **`scim_enabled` 不再列在這裡** —— SCIM 端點實作之後它成了那組端點的
    // 總開關（074 的 `authenticate_scim_token` 會檢查它），因此可以改。
    // 換成 `scim_token_ref`：它仍然沒有解析器，實際憑證在 `fms.scim_tokens`。
    for field in [
        "ldap_port",
        "scim_token_ref",
        "sync_cron",
        "attribute_mapping",
    ] {
        let (status, body) = patch(ctx, &admin, id, json!({field: json!(null)})).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{field}` 被接受了：{body}"
        );
        assert_eq!(
            body["errors"][0]["code"],
            json!("NOT_PATCHABLE"),
            "`{field}` 應該是 NOT_PATCHABLE：{body}"
        );
        // 理由必須說得出「為什麼」，不是只說「不行」。
        assert!(
            body["errors"][0]["message"].as_str().unwrap().len() > 20,
            "`{field}` 的理由太短，等於沒說：{body}"
        );
    }

    // 打錯字。
    let (status, body) = patch(ctx, &admin, id, json!({"ldapp_host": "x"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["errors"][0]["code"],
        json!("UNKNOWN_FIELD"),
        "打錯字的欄位與不可改的欄位得到同一個錯誤碼：{body}"
    );

    ctx.teardown().await;
}

/// 必填欄位的檢查對**合併後**的值進行，而且被拒之後原值還在。
#[tokio::test]
async fn d_required_fields_are_checked_against_the_merged_row() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // OIDC：issuer 與 client_id 兩者都必填（002 的 ck_idp_oidc_fields）。
    let oidc = provider(ctx, &admin, "entra-hq").await;
    let oidc_id = oidc["id"].as_str().unwrap();
    let before = raw_column(ctx, oidc_id, "issuer").await;
    assert!(before.is_some(), "種子的 entra-hq 應該有 issuer");

    let (status, body) = patch(ctx, &admin, oidc_id, json!({"issuer": null})).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "把 OIDC 來源的 issuer 清空被接受了：{body}"
    );
    assert!(
        body["detail"].as_str().unwrap().contains("清空"),
        "訊息沒說清楚是哪一種問題（只說約束被違反等於要人去查伺服器日誌）：{body}"
    );
    assert_eq!(
        raw_column(ctx, oidc_id, "issuer").await,
        before,
        "被拒之後 issuer 還是被清掉了"
    );

    // LDAP：ldap_host 與 ldap_base_dn。
    let ldap = provider(ctx, &admin, "ad-cinema").await;
    let ldap_id = ldap["id"].as_str().unwrap();
    let (status, body) = patch(ctx, &admin, ldap_id, json!({"ldap_base_dn": null})).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "把 LDAP 來源的 base_dn 清空被接受了：{body}"
    );
    assert!(
        raw_column(ctx, ldap_id, "ldap_base_dn").await.is_some(),
        "被拒之後 ldap_base_dn 還是被清掉了"
    );

    // 對照組：同時給兩個值是合法的（改，不是清空）。
    let (status, body) = patch(
        ctx,
        &admin,
        oidc_id,
        json!({"issuer": "https://login.example.com/v2.0", "client_id": "new-client"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "合法的成對修改被拒：{body}");
    assert_eq!(
        raw_column(ctx, oidc_id, "issuer").await.as_deref(),
        Some("https://login.example.com/v2.0")
    );

    ctx.teardown().await;
}

/// `null` 是清空、不提供是不動 —— 對不參與必填檢查的欄位也成立。
///
/// 少了這個區分，一個填錯的 `discovery_url` 就再也清不掉。
#[tokio::test]
async fn e_null_clears_and_absent_leaves_alone() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let p = provider(ctx, &admin, "entra-hq").await;
    let id = p["id"].as_str().unwrap();

    let (status, body) = patch(
        ctx,
        &admin,
        id,
        json!({"discovery_url": "https://login.example.com/.well-known/openid-configuration"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(raw_column(ctx, id, "discovery_url").await.is_some());

    // 不提供 → 不動。
    let (status, body) = patch(ctx, &admin, id, json!({"name": "還是那個來源"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        raw_column(ctx, id, "discovery_url").await.is_some(),
        "沒提供 discovery_url 卻被清空了 —— coalesce 分不出「不動」與「清空」"
    );

    // null → 清空。
    let (status, body) = patch(ctx, &admin, id, json!({"discovery_url": null})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        raw_column(ctx, id, "discovery_url").await.is_none(),
        "送 null 沒有清空 —— 填錯的值就再也清不掉了"
    );

    ctx.teardown().await;
}

/// 設第二個預設來源回 409，不是 500。
///
/// 002 的 `uq_identity_providers_default` 是部分唯一索引。少了對它的翻譯，
/// 呼叫端會拿到 500 然後去查伺服器日誌，而答案是「先把現在那個取消」。
#[tokio::test]
async fn f_second_default_provider_is_a_conflict_not_a_500() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 種子裡 entra-hq 已經是預設。
    let entra = provider(ctx, &admin, "entra-hq").await;
    assert_eq!(entra["is_default"], json!(true), "種子的前提變了");

    let ldap = provider(ctx, &admin, "ad-cinema").await;
    let (status, body) = patch(
        ctx,
        &admin,
        ldap["id"].as_str().unwrap(),
        json!({"is_default": true}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "設第二個預設來源不是 409：{body}"
    );
    assert!(
        body["detail"].as_str().unwrap().contains("is_default"),
        "訊息沒說出怎麼解（要先取消現有的那個）：{body}"
    );

    ctx.teardown().await;
}

/// status 只接受三個值；空 PATCH 與不存在的 id 各有自己的錯誤。
#[tokio::test]
async fn g_status_empty_patch_and_missing_id() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let p = provider(ctx, &admin, "ad-cinema").await;
    let id = p["id"].as_str().unwrap();

    let (status, body) = patch(ctx, &admin, id, json!({"status": "PAUSED"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = patch(ctx, &admin, id, json!({"status": "DISABLED"})).await;
    assert_eq!(status, StatusCode::OK, "合法的 status 被拒：{body}");
    assert_eq!(body["data"]["status"], json!("DISABLED"));

    // 空的 PATCH 不會有任何效果，回 200 會讓呼叫端以為做了什麼。
    let (status, body) = patch(ctx, &admin, id, json!({})).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "空 PATCH 回了 200：{body}"
    );

    let (status, _) = patch(
        ctx,
        &admin,
        "00000000-0000-4000-8000-000000000000",
        json!({"name": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 明文密鑰要擋掉 —— 它會進資料庫、備份與稽核紀錄。
    let (status, body) = patch(
        ctx,
        &admin,
        id,
        json!({"client_secret_ref": "kR9mZq2wX7pL4vN8sT1yB6cD3fG5hJ0aQ2eR4tY6uI8o"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "看起來像明文密鑰的值被寫進去了：{body}"
    );

    ctx.teardown().await;
}

/// 回應列出「這次改到、但目前沒有人讀」的欄位，而且**只列這次提到的**。
///
/// 全部列出來會變成一份沒有人看的免責聲明；一個都不列，管理者填完設定
/// 會合理地以為 SSO 登入可以用了 —— 而 callback 停在 token 交換之前回 501。
///
/// **`client_id` 刻意不再是這裡的例子。** `/authorize` 實作之後它有了讀者，
/// 而這一格用它當例子就會在「清單正確地縮小」時失敗 —— 那正是它應該
/// 失敗的方式，也是這次改用 `client_secret_ref` 的原因。
#[tokio::test]
async fn h_response_names_the_fields_that_nothing_reads_yet() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let p = provider(ctx, &admin, "entra-hq").await;
    let id = p["id"].as_str().unwrap();

    // 只改 name —— name 是有讀者的（列表、選單），所以清單應該是空的。
    let (status, body) = patch(ctx, &admin, id, json!({"name": "只改名"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["meta"]["fields_with_no_consumer_yet"],
        json!([]),
        "改一個有讀者的欄位卻回報了無消費者欄位：{body}"
    );

    // `client_id` 現在**有**讀者（/authorize 把它放進授權網址），
    // 因此改它不該再出現在這份清單裡。
    let (status, body) = patch(ctx, &admin, id, json!({"client_id": "another-client"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["meta"]["fields_with_no_consumer_yet"],
        json!([]),
        "client_id 已經被 /authorize 讀取，不該再被列為無消費者：{body}"
    );

    // 改 client_secret_ref —— 真的沒有讀者（沒有密鑰解析器）。
    let (status, body) = patch(
        ctx,
        &admin,
        id,
        json!({"client_secret_ref": "kv/fms/entra-hq"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let inert = body["meta"]["fields_with_no_consumer_yet"]
        .as_array()
        .unwrap();
    assert_eq!(
        inert.len(),
        1,
        "應該恰好列出 client_secret_ref 一個（只列這次提到的）：{body}"
    );
    assert_eq!(inert[0]["field"], json!("client_secret_ref"));
    assert!(
        inert[0]["reason"].as_str().unwrap().contains("501"),
        "理由沒指出它擋住的是哪一段：{body}"
    );

    // 不可變更欄位的清單是恆定的說明，永遠都在。
    assert!(!body["meta"]["not_patchable_fields"]
        .as_array()
        .unwrap()
        .is_empty());

    ctx.teardown().await;
}

/// 權限：`identity_provider:write` 是 TENANT 範圍，場域管理員沒有。
#[tokio::test]
async fn i_write_permission_is_required() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let p = provider(ctx, &admin, "ad-cinema").await;
    let id = p["id"].as_str().unwrap();

    let fm = ctx.login_as(USERNAME_FACILITY_ADMIN).await;
    let (status, body) = patch(ctx, &fm, id, json!({"name": "場域管理員改的"})).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "沒有 identity_provider:write 的人改成功了：{body}"
    );
    assert_ne!(
        raw_column(ctx, id, "name").await.as_deref(),
        Some("場域管理員改的")
    );

    ctx.teardown().await;
}

/// `GET /directory-groups`：清單、兩個缺口計數，以及計數跨整個租戶而非只算這一頁。
#[tokio::test]
async fn m_directory_groups_report_both_coverage_gaps() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, body) = ctx
        .send(authed(get("/api/v1/directory-groups"), &admin))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let total = body["meta"]["total_groups"].as_i64().unwrap();
    assert!(total >= 2, "種子應該有兩個目錄群組：{body}");

    // **009 種的兩個群組 last_synced_at 都是 NULL** —— 因為 Phase 1 沒有同步
    // 客戶端（migration 058 檔頭）。這不是測試在遷就實作：那是這個系統目前
    // 真實的狀態，而它必須在回應裡看得見。
    assert_eq!(
        body["meta"]["groups_never_synced"].as_i64().unwrap(),
        total,
        "種子的群組應該全部都是「從未同步」：{body}"
    );

    // 種子的兩個群組都有對應，所以「沒有對應」的計數是 0。
    assert_eq!(
        body["meta"]["groups_not_mapped_to_any_role"]
            .as_i64()
            .unwrap(),
        0,
        "{body}"
    );

    // 每一列都要說出它在 FMS 裡代表什麼權限。
    let first = &body["data"][0];
    assert!(first["role_mapping_count"].as_i64().unwrap() >= 1, "{body}");
    assert!(first["member_count_in_fms"].as_i64().is_some(), "{body}");

    // 回應要說出這些列是誰寫的 —— 「為什麼清單是空的」的答案不在這個系統裡。
    assert!(
        body["meta"]["populated_by"]
            .as_str()
            .unwrap()
            .contains("058"),
        "meta 沒指向那個限制的出處：{body}"
    );

    // 新增一個沒有任何對應的群組，缺口計數要跟著動 ——
    // 而且是在**第一頁只放一列**的情況下也要對（計數跨整個租戶）。
    let provider_id = provider(ctx, &admin, "entra-hq").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.directory_groups
                    (tenant_id, identity_provider_id, external_group_id, name, last_synced_at)
             VALUES ($1::uuid, $2::uuid, 'ext-zzz-unmapped', 'ZZZ-沒有對應的群組', now())",
        )
        .bind(TENANT_ID)
        .bind(&provider_id)
        .execute(&mut *tx)
        .await
        .expect("insert group");
        tx.commit().await.expect("commit");
    }

    let (status, body) = ctx
        .send(authed(get("/api/v1/directory-groups?limit=1"), &admin))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"].as_array().unwrap().len(), 1, "limit 沒生效");
    assert_eq!(
        body["meta"]["groups_not_mapped_to_any_role"]
            .as_i64()
            .unwrap(),
        1,
        "缺口計數只算了這一頁 —— 那一頁剛好沒問題不代表沒問題：{body}"
    );
    assert_eq!(
        body["meta"]["total_groups"].as_i64().unwrap(),
        total + 1,
        "{body}"
    );
    // 新群組有同步時間，所以「從未同步」的計數不變。
    assert_eq!(
        body["meta"]["groups_never_synced"].as_i64().unwrap(),
        total,
        "{body}"
    );

    ctx.teardown().await;
}

// 這裡原本有一支測試（`n_claim_value_only_mappings_do_not_count_as_coverage`）
// 驗證「只填 claim_value 的對應不算進 role_mapping_count」。
//
// migration 077（`ck_drm_group_required`）之後，這種列**在資料庫層就建不出來**
// 了——不只是不算覆蓋，而是不存在。原測試的 INSERT 因此會撞 23514。
// 這個保證本身已經由 077 的自我驗證與 `directory_mappings_slice.rs` 覆蓋
// （建立時擋下、mutation test 確認約束真的在擋），所以不需要在這裡重造一份
// 「假裝它存在」的情境。

/// `identity_provider_id` 過濾同時作用於清單與計數。
#[tokio::test]
async fn o_provider_filter_applies_to_the_counts_too() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let entra = provider(ctx, &admin, "entra-hq").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ldap = provider(ctx, &admin, "ad-cinema").await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (_, all) = ctx
        .send(authed(get("/api/v1/directory-groups?limit=50"), &admin))
        .await;
    let total = all["meta"]["total_groups"].as_i64().unwrap();

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/directory-groups?identity_provider_id={entra}"
            )),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entra_total = body["meta"]["total_groups"].as_i64().unwrap();

    let (status, body) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/directory-groups?identity_provider_id={ldap}"
            )),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ldap_total = body["meta"]["total_groups"].as_i64().unwrap();

    // **計數必須跟著過濾條件走。** 少了這一格，過濾只作用在清單上，
    // 而 meta 仍然回整個租戶的數字 —— 於是「這個來源有 0 個群組沒對應」
    // 這句話其實是在講另一個來源。
    assert_eq!(
        entra_total + ldap_total,
        total,
        "過濾後的計數加起來不等於全部 —— meta 沒有跟著過濾：{entra_total} + {ldap_total} != {total}"
    );

    ctx.teardown().await;
}

/// 讀取權限：`identity_provider:read`。一般請求者沒有。
#[tokio::test]
async fn p_read_permission_is_required() {
    let ctx = &TestContext::setup().await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    let (status, body) = ctx
        .send(authed(get("/api/v1/directory-groups"), &requester))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "一般請求者讀得到目錄群組：{body}"
    );

    ctx.teardown().await;
}
