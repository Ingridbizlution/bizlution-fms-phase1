//! 附件與物件儲存（WBS S5）。
//!
//! 重點：
//!   * 上傳 → 預簽下載網址**真的可以下載**，且位元組與上傳的一致
//!   * bucket 是私有的：沒有簽章的直連必須被拒絕
//!   * 預簽網址帶回原始檔名（物件鍵是 uuid 形式，不設就下載到亂碼檔名）
//!   * 掛到不存在的實體 → 404（`entity_id` 是多型的，沒有外鍵保護）
//!   * 權限沿用所屬實體的寫入權限
//!   * 軟刪除後查不到，且物件真的被刪掉
//!   * 工單詳情的 `include=attachments`

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::json;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";

/// 組一個 multipart body。手寫而不用 crate：只需要兩三個欄位，
/// 而測試的重點是端到端行為不是 multipart 編碼。
fn multipart(
    boundary: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &str, &[u8])>,
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    if let Some((name, file_name, bytes)) = file {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{name}\"; filename=\"{file_name}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

fn upload_request(
    entity_type: &str,
    entity_id: &str,
    purpose: &str,
    name: &str,
    data: &[u8],
) -> Request<Body> {
    let boundary = "fmsboundary1234";
    let body = multipart(
        boundary,
        &[
            ("entity_type", entity_type),
            ("entity_id", entity_id),
            ("purpose", purpose),
        ],
        Some(("file", name, data)),
    );
    Request::builder()
        .method("POST")
        .uri("/api/v1/attachments")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

/// 只解 `%XX`，夠用來驗證 RFC 6266 的編碼結果。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn attachment_slice_end_to_end() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let http = reqwest::Client::new();

    // 內容刻意不是純文字：附件多半是二進位，位元組比對才有意義
    let payload: Vec<u8> = (0u8..=255).cycle().take(3000).collect();

    // ---- 上傳 ----
    let (status, created) = ctx
        .send(authed(
            upload_request("ASSET", SEED_AHU, "MANUAL", "空調箱操作手冊.pdf", &payload),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "upload failed: {created}");
    assert_eq!(created["file_name"], "空調箱操作手冊.pdf");
    assert_eq!(created["purpose"], "MANUAL");
    assert_eq!(
        created["size_bytes"].as_i64(),
        Some(payload.len() as i64),
        "size_bytes 應是實際位元組數：{created}"
    );
    let attachment_id = created["id"].as_str().unwrap().to_string();
    let url = created["download_url"]
        .as_str()
        .expect("契約要求 download_url")
        .to_string();

    // ---- 預簽網址必須真的能下載，且內容一致 ----
    let res = http.get(&url).send().await.expect("fetch presigned url");
    assert_eq!(
        res.status().as_u16(),
        200,
        "預簽網址應可下載（SigV4 簽章正確）：{}",
        res.status()
    );
    // 檔名要帶回去，否則使用者下載到的是物件鍵那串 uuid
    let disposition = res
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // 中文檔名必須走 RFC 6266 的 `filename*=UTF-8''<百分比編碼>`：
    // 直接把 UTF-8 放進 `filename=` 是不合法的標頭，MinIO 會整個丟掉，
    // 而使用者會下載到一個以物件鍵命名的檔案且毫無錯誤訊息。
    assert!(
        disposition.contains("filename*=UTF-8''"),
        "非 ASCII 檔名必須用 RFC 6266 的擴充語法，實際：{disposition}"
    );
    let decoded = percent_decode(&disposition);
    assert!(
        decoded.contains("空調箱操作手冊.pdf"),
        "解碼後應是原始檔名，實際：{disposition}"
    );
    let downloaded = res.bytes().await.expect("body");
    assert_eq!(
        downloaded.as_ref(),
        payload.as_slice(),
        "下載的位元組必須與上傳的完全相同"
    );

    // ---- bucket 是私有的：拿掉簽章參數就該被拒絕 ----
    let unsigned = url.split('?').next().unwrap().to_string();
    let res = http.get(&unsigned).send().await.expect("fetch unsigned");
    assert!(
        res.status().is_client_error(),
        "bucket 必須是私有的 —— 未簽章的直連竟然回 {}。\
         這代表 minio-init 的 `mc anonymous set none` 沒生效，\
         租戶隔離在物件儲存那一側形同虛設",
        res.status()
    );

    // ---- 重新取得預簽網址（過期後客戶端要能便宜地再要一個）----
    let (status, refreshed) = ctx
        .send(authed(
            get_request(&format!("/api/v1/attachments/{attachment_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    // 刻意**不**斷言兩次的網址不同：SigV4 的 `X-Amz-Date` 只到秒，
    // 同一秒內簽兩次本來就會得到完全相同的字串。原本寫成 assert_ne! 是
    // 我把「每次重新簽」誤當成「每次結果不同」——那是錯的假設。
    // 真正要驗的是它是一個當下有效的預簽網址（下面就抓它），
    // 以及它確實帶了簽章而不是裸網址。
    let refreshed_url = refreshed["download_url"]
        .as_str()
        .expect("應有 download_url");
    assert!(
        refreshed_url.contains("X-Amz-Signature") && refreshed_url.contains("X-Amz-Expires"),
        "重新取得的必須是預簽網址：{refreshed_url}"
    );
    let res = http
        .get(refreshed_url)
        .send()
        .await
        .expect("fetch refreshed");
    assert_eq!(res.status().as_u16(), 200, "重新簽的網址也要能用");

    // ---- 列出某個實體的附件 ----
    let (status, listed) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/attachments?entity_type=ASSET&entity_id={SEED_AHU}"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == attachment_id),
        "{listed}"
    );

    // purpose 過濾
    let (_, filtered) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/attachments?entity_type=ASSET&entity_id={SEED_AHU}&purpose=BEFORE_PHOTO"
            )),
            &token,
        ))
        .await;
    assert!(
        !filtered["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == attachment_id),
        "purpose 過濾應生效：{filtered}"
    );

    // ---- 掛到不存在的實體 → 404（entity_id 是多型的，沒有外鍵擋）----
    let (status, body) = ctx
        .send(authed(
            upload_request(
                "ASSET",
                &uuid::Uuid::new_v4().to_string(),
                "GENERAL",
                "x.txt",
                b"x",
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "沒有外鍵保護，因此存在性必須由應用層檢查：{body}"
    );

    // ---- 未支援的 entity_type → 422 ----
    let (status, body) = ctx
        .send(authed(
            upload_request("INVOICE", SEED_AHU, "GENERAL", "x.txt", b"x"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 沒有 file part → 422 ----
    let boundary = "fmsboundary1234";
    let body = multipart(
        boundary,
        &[("entity_type", "ASSET"), ("entity_id", SEED_AHU)],
        None,
    );
    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .method("POST")
                .uri("/api/v1/attachments")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 工單詳情的 include=attachments ----
    let (status, wo) = ctx
        .send(authed(
            {
                let b = json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": FACILITY_HQ,
                    "asset_id": SEED_AHU,
                    "title": "附件測試工單"
                });
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/work-orders")
                    .header("content-type", "application/json")
                    .body(Body::from(b.to_string()))
                    .unwrap()
            },
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{wo}");
    let wo_id = wo["id"].as_str().unwrap().to_string();

    let (status, photo) = ctx
        .send(authed(
            upload_request(
                "WORK_ORDER",
                &wo_id,
                "AFTER_PHOTO",
                "完工照.jpg",
                b"jpegbytes",
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{photo}");

    let (status, detail) = ctx
        .send(authed(
            get_request(&format!("/api/v1/work-orders/{wo_id}?include=attachments")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let files = detail["attachments"].as_array().expect("應有 attachments");
    assert_eq!(files.len(), 1, "{detail}");
    assert_eq!(files[0]["purpose"], "AFTER_PHOTO");
    assert!(
        files[0]["download_url"]
            .as_str()
            .is_some_and(|u| u.contains("X-Amz-Signature")),
        "嵌入的附件也要是預簽網址：{detail}"
    );

    // ---- 刪除：資料列軟刪除、物件真的消失 ----
    let (status, _) = ctx
        .send(authed(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/attachments/{attachment_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = ctx
        .send(authed(
            get_request(&format!("/api/v1/attachments/{attachment_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "軟刪除後應查不到");

    // 舊的預簽網址仍在有效期內，但物件已刪除 → 404。
    // 這正是「軟刪除資料列、硬刪除物件」的用意：紀錄留著供稽核，
    // 檔案不再能下載。
    let res = http.get(&url).send().await.expect("fetch after delete");
    assert!(
        res.status().is_client_error(),
        "物件應已被刪除，實際 {}",
        res.status()
    );

    ctx.teardown().await;
}
