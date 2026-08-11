//! 可觀測性：每筆請求的 log 必須帶已驗證的 `tenant_id`。
//!
//! 這個測試的價值在於它驗證的是**運維時真正需要的東西**：
//! 「這個客戶這一小時的錯誤率是多少」——沒有租戶標籤就答不出來。
//!
//! 用一個自訂的 tracing writer 攔截 JSON log，然後斷言 span 欄位真的出現。
//! 只斷言「middleware 有被掛上」是不夠的：欄位可能被記錄卻沒有被
//! 格式器輸出（`with_current_span(false)` 就會這樣），而那在生產才會發現。

mod common;

use axum::body::Body;
use axum::http::Request;
use common::*;
use std::io;
use std::sync::{Arc, Mutex};

/// 把 log 收進記憶體的 writer。
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn requests_are_logged_with_the_verified_tenant_id() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;

    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        // span 欄位要進到每一筆 log —— 這正是「不必在每個 log! 手動帶
        // tenant_id」的機制，也是這個測試要守住的行為。
        .with_current_span(true)
        .with_span_list(true)
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::INFO)
        .finish();

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let (status, _) = ctx
            .send(authed(
                Request::builder()
                    .uri("/api/v1/reservations?limit=1")
                    .body(Body::empty())
                    .unwrap(),
                &token,
            ))
            .await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    let logs = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
    assert!(!logs.is_empty(), "應該要有 log 輸出");
    assert!(
        logs.contains(TENANT_ID),
        "每筆請求的 log 都必須帶已驗證的 tenant_id，否則無法按租戶切分。\n實際輸出：\n{logs}"
    );
    assert!(
        logs.contains("http_request"),
        "應有 http_request span：\n{logs}"
    );
    assert!(
        logs.contains(ADMIN_USER_ID),
        "user_id 也應在 span 上（追查是誰觸發的）：\n{logs}"
    );

    ctx.teardown().await;
}

/// 未認證的路徑沒有租戶可記 —— 不該因此壞掉，也不該記下客戶端自稱的租戶。
#[tokio::test]
async fn unauthenticated_paths_carry_no_tenant_label() {
    let ctx = TestContext::setup().await;
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::INFO)
        .finish();

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        // 帶一個**偽造的** X-Tenant-ID 但沒有 token
        let req = Request::builder()
            .uri("/api/v1/health")
            .header("x-tenant-id", "aaaaaaaa-0000-4000-8000-999999999999")
            .body(Body::empty())
            .unwrap();
        let (status, _) = ctx.send(req).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    let logs = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
    assert!(
        !logs.contains("999999999999"),
        "log 不該記下客戶端自稱、未經驗證的租戶 —— 那會讓 log 可以被偽造。\n{logs}"
    );

    ctx.teardown().await;
}
