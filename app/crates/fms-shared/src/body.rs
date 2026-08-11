//! `OptionalJson<T>`：給「body 可有可無」的端點用（例如 `DELETE` 帶一個選填的
//! 取消原因）。
//!
//! 為什麼不能直接用 axum 內建的 `Option<Json<T>>`：那個組合只在完全沒有
//! `Content-Type` 標頭時才乾脆地變成 `None`；只要請求帶了
//! `Content-Type: application/json`（即使 body 是空的——常見於前端沒清掉上一個
//! 請求殘留的標頭），axum 仍然會嘗試把空 body 解析成 JSON，得到一個 EOF 錯誤，
//! 而這個錯誤會以 axum 預設的純文字 400 直接回應，不會被 handler 的
//! `Result<_, Problem>` 接住，前端因此拿到跟文件承諾的 `application/problem+json`
//! 不一致的錯誤格式。
//!
//! `OptionalJson<T>` 改成先看 body 是否為空——空的一律當 `None`（不管
//! `Content-Type` 寫什麼），非空但解析失敗才回 `Problem::bad_request`，跟其他
//! 端點的錯誤格式一致。

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use serde::de::DeserializeOwned;

use crate::problem::Problem;

pub struct OptionalJson<T>(pub Option<T>);

impl<T, S> FromRequest<S> for OptionalJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|e| Problem::bad_request(format!("failed to read request body: {e}")))?;

        if bytes.is_empty() {
            return Ok(OptionalJson(None));
        }

        let value = serde_json::from_slice(&bytes)
            .map_err(|e| Problem::bad_request(format!("invalid JSON body: {e}")))?;
        Ok(OptionalJson(Some(value)))
    }
}
