//! 以 `service_items.form_schema` 驗證 payload。
//!
//! # 為什麼放在 shared
//!
//! 同一個 `form_schema` 有兩個消費者：`POST /work-orders`（SERVICE 類工單的
//! `payload`）與 `POST /reservations`（附加服務的 `payload`）。這兩處驗的是
//! **同一份 schema、同一個語意**，因此不能各寫一份 —— 兩份驗證遲早會出現
//! 「同樣的 payload 在工單被接受、在預約被拒」這種無法解釋的行為。
//!
//! 這與 016 把 scope 述詞收斂成一份是同一個理由。
//!
//! # 第三個消費者：`attribute_definitions.validation_schema`
//!
//! 動態欄位的驗證（`assets.attributes`）用的是同一套機制 ——
//! 一份 JSON Schema、一個 payload、指向請求 body 的錯誤路徑。
//!
//! 那件事**在 API 層做，不用資料庫觸發器**（這是明確的設計決定）。
//! 理由：schema 可以隨時被管理者改，而觸發器只能驗「當下」那一版；
//! 既有資料不會、也不該被回溯拒絕 —— 那會讓一次設定變更把歷史資料變成
//! 無法儲存的東西。API 層驗證讓新寫入符合現行定義，而舊資料照原樣留著。
//!
//! 代價要說清楚：**既有的 `attributes` 不會被回溯驗證**。要找出不符合
//! 現行定義的舊資料需要另一支稽核端點，而那不是這一層的事。

use crate::problem::{FieldError, Problem};

/// 驗證 payload 是否符合 `form_schema`。
///
/// `pointer_prefix` 讓錯誤指向請求 body 裡的正確位置：工單是 `/payload`，
/// 預約的附加服務是 `/services/0/payload`。少了它，前端只知道「payload 有錯」
/// 卻不知道是哪一個服務的。
pub fn validate(
    schema: &serde_json::Value,
    payload: &serde_json::Value,
    pointer_prefix: &str,
) -> Result<(), Problem> {
    validate_named(schema, payload, pointer_prefix, "service item form_schema")
}

/// 與 [`validate`] 相同，但可以指名 schema 的來源。
///
/// 訊息裡指名來源不是修辭：一份壞掉的 schema 回 500 時，運維要知道去改
/// **哪一筆設定** —— 是某個服務項目的 `form_schema`，還是某個動態欄位的
/// `validation_schema`。寫死「service item」會把第二種指向錯的地方。
pub fn validate_named(
    schema: &serde_json::Value,
    payload: &serde_json::Value,
    pointer_prefix: &str,
    schema_label: &str,
) -> Result<(), Problem> {
    let validator = jsonschema::validator_for(schema).map_err(|e| {
        // schema 本身壞掉是**設定**問題，不是客戶端的錯：回 500 才誠實，
        // 回 422 會讓客戶端不斷修改自己完全正確的請求。
        Problem::internal(std::io::Error::other(format!(
            "{schema_label} is not a valid JSON Schema: {e}"
        )))
    })?;

    let errors: Vec<FieldError> = validator
        .iter_errors(payload)
        .map(|e| FieldError {
            pointer: format!("{pointer_prefix}{}", e.instance_path()),
            code: "SCHEMA_VIOLATION".to_string(),
            message: e.to_string(),
        })
        .collect();

    if errors.is_empty() {
        return Ok(());
    }
    Err(Problem::validation(format!("payload does not match {schema_label}")).with_errors(errors))
}
