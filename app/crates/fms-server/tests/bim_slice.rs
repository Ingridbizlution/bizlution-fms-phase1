//! BIM 模型與直傳上傳（`/facilities/{id}/bim-models`、`/uploads/presign`）。
//!
//! # 核心是 `c_`：空陣列有兩種意思，而它們長得一模一樣
//!
//! `unresolved_elements` 是空的，可能代表：
//!
//!   * 「解析完了，全部都對應好了」
//!   * 「還沒解析」
//!
//! 模型在被 `bim-worker` 輪到之前（`status = UPLOADED`／`PARSING`）永遠是
//! 後者 —— 而若回應不說，看的人會讀成前者，然後以為 BIM 對映已經完成。
//!
//! `c_` 把同一個模型從 `UPLOADED` 改成 `PARSED` 再問一次，
//! 斷言那句說明真的跟著狀態變 —— 否則它只是一句寫死的裝飾。
//!
//! # `b_`：直傳的鍵屬於租戶
//!
//! 註冊端點只收 `POST /uploads/presign` 回傳的 `storage_key`。
//! 少了那個比對，猜到鍵的人可以把**別的租戶的檔案**掛進自己的場域。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

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

/// 取預簽網址 → 註冊 → 列出。回傳 `(storage_key, model_id)`。
async fn presign_and_register(ctx: &TestContext, token: &str) -> (String, String) {
    let (status, pre) = ctx
        .send(authed(
            post(
                "/api/v1/uploads/presign",
                json!({ "file_name": "tower-a.ifc", "content_type": "application/octet-stream", "content_length": 12345 }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{pre}");
    let key = pre["storage_key"]
        .as_str()
        .expect("storage_key")
        .to_string();

    let (status, model) = ctx
        .send(authed(
            post(
                &format!("/api/v1/facilities/{FACILITY_HQ}/bim-models"),
                json!({ "name": "A 棟結構", "source_format": "IFC", "storage_key": key }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{model}");
    (key, model["id"].as_str().unwrap().to_string())
}

/// 預簽 → 註冊 → 列得到，而且**上傳網址真的能用**。
#[tokio::test]
async fn a_a_model_can_be_uploaded_registered_and_listed() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    let (status, pre) = ctx
        .send(authed(
            post(
                "/api/v1/uploads/presign",
                json!({ "file_name": "tower-a.ifc", "content_type": "application/octet-stream", "content_length": 12345 }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{pre}");
    let url = pre["upload_url"].as_str().expect("upload_url").to_string();
    let key = pre["storage_key"]
        .as_str()
        .expect("storage_key")
        .to_string();
    assert!(
        key.starts_with(TENANT_ID),
        "物件鍵要含租戶前綴，否則跨租戶猜鍵猜得到：{key}"
    );
    assert_eq!(
        pre["content_type"], "application/octet-stream",
        "content_type 被簽進網址，必須回傳給客戶端 —— 只寫在文件裡不可靠：{pre}"
    );

    // **真的用那個網址上傳。** 只檢查回傳了一個 http 開頭的字串不算驗證 ——
    // 「網址看起來對但 PUT 回 403」正是預簽最典型的失敗方式。
    let resp = reqwest::Client::new()
        .put(&url)
        .header("content-type", "application/octet-stream")
        .body(b"IFC-FAKE-CONTENT".to_vec())
        .send()
        .await
        .expect("上傳");
    assert!(
        resp.status().is_success(),
        "預簽的 PUT 應該成功，拿到 {}",
        resp.status()
    );

    let (status, model) = ctx
        .send(authed(
            post(
                &format!("/api/v1/facilities/{FACILITY_HQ}/bim-models"),
                json!({ "name": "A 棟結構", "source_format": "IFC", "storage_key": key }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{model}");
    assert_eq!(model["status"], "UPLOADED");
    assert_eq!(
        model["element_count"], 0,
        "還沒被 bim-worker 輪到，元件數是 0"
    );

    let (status, listed) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_HQ}/bim-models")),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "A 棟結構"),
        "{listed}"
    );

    ctx.teardown().await;
}

/// **註冊只收屬於這個租戶的鍵。**
///
/// 少了那個比對，猜到鍵的人可以把別的租戶的檔案掛進自己的場域 ——
/// 而那個模型會出現在他的清單裡、下載得到。
#[tokio::test]
async fn b_a_foreign_storage_key_is_refused() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    for (label, key) in [
        (
            "別的租戶",
            "aaaaaaaa-0000-4000-8000-0000000000ff/bim-model/x/y.ifc",
        ),
        ("沒有前綴", "bim-model/x/y.ifc"),
        (
            "想跳出前綴",
            "../aaaaaaaa-0000-4000-8000-0000000000ff/y.ifc",
        ),
    ] {
        let (status, body) = ctx
            .send(authed(
                post(
                    &format!("/api/v1/facilities/{FACILITY_HQ}/bim-models"),
                    json!({ "name": "偷別人的", "storage_key": key }),
                ),
                &admin,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{label} 的 storage_key 必須被拒絕：{body}"
        );
    }

    // 缺 storage_key 時訊息要指出正確的流程。
    let (status, body) = ctx
        .send(authed(
            post(
                &format!("/api/v1/facilities/{FACILITY_HQ}/bim-models"),
                json!({ "name": "沒有鍵" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("uploads/presign"),
        "訊息要說出該先去哪裡取鍵：{body}"
    );

    ctx.teardown().await;
}

/// **空陣列的兩種意思必須分得開。** 這一組最重要的一格。
///
/// 同一個模型從 `UPLOADED` 改成 `PARSED`，那句說明必須跟著變 ——
/// 否則它只是一句寫死的裝飾。
#[tokio::test]
async fn c_an_empty_element_list_says_which_kind_of_empty_it_is() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (_, model_id) = presign_and_register(ctx, &admin).await;

    // --- UPLOADED：空的意思是「還沒解析」---
    let (status, un) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/bim-models/{model_id}/unresolved-elements"
            )),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{un}");
    assert_eq!(un["data"], json!([]), "還沒被 bim-worker 輪到，清單是空的");
    assert_eq!(un["meta"]["status"], "UPLOADED");
    let note = un["meta"]["parsing"].as_str().unwrap_or_default();
    assert!(
        note.contains("排隊"),
        "**空陣列必須說出它是哪一種空的** —— 少了這句，看的人會讀成\
         「全部都對應好了」然後以為 BIM 對映完成了：{un}"
    );

    // --- 改成 PARSED：同一個空陣列，意思完全不同 ---
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.bim_models SET status = 'PARSED' WHERE id = $1::uuid")
            .bind(&model_id)
            .execute(&mut *tx)
            .await
            .expect("改狀態");
        tx.commit().await.expect("commit");
    }

    let (_, parsed) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/bim-models/{model_id}/unresolved-elements"
            )),
            &admin,
        ))
        .await;
    let parsed_note = parsed["meta"]["parsing"].as_str().unwrap_or_default();
    assert_eq!(parsed["data"], json!([]), "陣列還是空的");
    assert_ne!(
        parsed_note, note,
        "狀態變了說明卻沒變 —— 那句話是寫死的裝飾，不是真的在說明狀態"
    );
    assert!(
        parsed_note.contains("已解析"),
        "PARSED 的說明要說出「這是真實的對應缺口」：{parsed}"
    );

    // 清單端點的每一列也要帶說明（同一個場域可能同時有兩種狀態的模型）。
    let (_, listed) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_HQ}/bim-models")),
            &admin,
        ))
        .await;
    assert!(
        listed["data"][0]["parsing"].is_string(),
        "清單的每一列都要帶解析說明：{listed}"
    );

    ctx.teardown().await;
}

/// 真的有未解析元件時，資料要如實回傳（不是永遠空陣列）。
///
/// 少了這一格，一個「一律回 `[]`」的實作會讓 `c_` 通過 ——
/// 而那時人工補正介面永遠是空的。
#[tokio::test]
async fn d_real_unresolved_elements_are_returned() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (_, model_id) = presign_and_register(ctx, &admin).await;

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "UPDATE fms.bim_models
                SET status = 'PARSED', element_count = 2,
                    unresolved_elements = $2::jsonb
              WHERE id = $1::uuid",
        )
        .bind(&model_id)
        .bind(json!([
            { "guid": "2O2Fr$t4X7Zf8NOew3FLOH", "type": "IfcPump", "name": "B1 泵浦 1" },
            { "guid": "3P3Gs$u5Y8Ag9OPfx4GMPI", "type": "IfcPump", "name": "B1 泵浦 2" }
        ]))
        .execute(&mut *tx)
        .await
        .expect("塞未解析元件");
        tx.commit().await.expect("commit");
    }

    let (status, un) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/bim-models/{model_id}/unresolved-elements"
            )),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{un}");
    let items = un["data"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2, "元件要如實回傳，不是永遠空陣列：{un}");
    assert_eq!(
        items[0]["name"], "B1 泵浦 1",
        "契約說這支端點是給「B1 有 2 台未識別設備」那種補正介面用的：{un}"
    );

    ctx.teardown().await;
}

/// 權限與範圍：讀寫分開，而且是對**模型所在場域**判定的。
#[tokio::test]
async fn e_permissions_are_scoped_to_the_models_facility() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;
    let (_, model_id) = presign_and_register(ctx, &admin).await;

    // REQUESTER 沒有 bim_model:read。
    let req = ctx.login_as(USERNAME_REQUESTER).await;
    let (status, denied) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_HQ}/bim-models")),
            &req,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");

    let (status, denied) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/bim-models/{model_id}/unresolved-elements"
            )),
            &req,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "權限是對模型所在場域判定的：{denied}"
    );

    // 但**預簽只要已登入** —— 契約如此，而預簽網址不洩漏任何資料，
    // 也不會讓物件出現在任何清單裡。真正的守衛是註冊端點。
    let (status, pre) = ctx
        .send(authed(
            post(
                "/api/v1/uploads/presign",
                json!({ "file_name": "x.ifc", "content_type": "application/octet-stream", "content_length": 12345 }),
            ),
            &req,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "預簽只要已登入（一個上傳了但沒註冊的物件是孤兒，不是資料洩漏）：{pre}"
    );

    // 而他註冊不了。
    let (status, cannot) = ctx
        .send(authed(
            post(
                &format!("/api/v1/facilities/{FACILITY_HQ}/bim-models"),
                json!({ "name": "越權", "storage_key": pre["storage_key"] }),
            ),
            &req,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "註冊要 bim_model:write —— 那才是真正的守衛：{cannot}"
    );

    // 不存在的模型是 404。
    let (status, missing) = ctx
        .send(authed(
            get("/api/v1/bim-models/00000000-0000-4000-8000-0000000000ff/unresolved-elements"),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");

    ctx.teardown().await;
}

/// 上傳當下就擋下：格式與容量都不必等 `bim-worker` 非同步輪詢才知道錯了。
#[tokio::test]
async fn f_bad_format_or_size_is_rejected_at_upload_time_not_async() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login_as(USERNAME).await;

    // 容量：presign 沒帶 content_length 就該擋，不能讓客戶端跳過大小回報。
    let (status, missing_len) = ctx
        .send(authed(
            post(
                "/api/v1/uploads/presign",
                json!({ "file_name": "tower.ifc", "content_type": "application/octet-stream" }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{missing_len}");

    // 超過上限（1 GiB）的檔案在 presign 這一步就該被擋下，不必等直傳完成。
    let (status, too_big) = ctx
        .send(authed(
            post(
                "/api/v1/uploads/presign",
                json!({ "file_name": "tower.ifc", "content_type": "application/octet-stream",
                        "content_length": 1024_i64 * 1024 * 1024 + 1 }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{too_big}");

    // 格式：DB CHECK 允許的 6 種格式裡，`bim-worker` 只有 IFC 有解析器
    // ——RVT 通過 CHECK 卻應該在註冊當下就被擋下，不是上傳後排隊、
    // 等 worker 非同步輪到才發現「不支援」。
    let (status, pre) = ctx
        .send(authed(
            post(
                "/api/v1/uploads/presign",
                json!({ "file_name": "tower.rvt", "content_type": "application/octet-stream",
                        "content_length": 12345 }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{pre}");
    let key = pre["storage_key"].as_str().expect("storage_key");

    let (status, rejected) = ctx
        .send(authed(
            post(
                &format!("/api/v1/facilities/{FACILITY_HQ}/bim-models"),
                json!({ "name": "B 棟結構", "source_format": "RVT", "storage_key": key }),
            ),
            &admin,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "RVT 通過 DB CHECK 但沒有解析器，註冊當下就該擋下：{rejected}"
    );

    ctx.teardown().await;
}
