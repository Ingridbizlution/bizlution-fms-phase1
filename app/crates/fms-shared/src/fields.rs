//! 稀疏欄位集合（`?fields=id,name,status`），對應契約的 `Fields` 參數。
//!
//! 以「序列化後投影」實作，而非動態組 SELECT 清單：
//! 後者會讓 `query_as!` 的編譯期驗證失效（第一個參數必須是字串字面值），
//! 而那是 ADR-09 選 Rust 的主要理由之一。網路傳輸量仍然減少，
//! 只是資料庫端仍讀完整列 —— 這個取捨對 Phase 1 的資料量是合理的，
//! 若日後成為瓶頸，正確解法是為熱門欄位組合寫專用查詢，而不是放棄驗證。

use serde_json::Value;

use crate::problem::Problem;

/// 解析 `fields` 參數。回傳 `None` 表示未指定（回傳完整物件）。
///
/// 未知欄位視為錯誤而非忽略：靜默忽略會讓前端以為拿得到某欄位，
/// 卻永遠收不到，且不會有任何訊號。
pub fn parse(raw: Option<&str>, allowed: &[&str]) -> Result<Option<Vec<String>>, Problem> {
    let Some(raw) = raw else { return Ok(None) };

    let requested: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if requested.is_empty() {
        return Ok(None);
    }

    let unknown: Vec<&String> = requested
        .iter()
        .filter(|f| !allowed.contains(&f.as_str()))
        .collect();
    if !unknown.is_empty() {
        return Err(Problem::validation(format!(
            "unknown field(s) in `fields`: {unknown:?}; allowed: {allowed:?}"
        )));
    }

    Ok(Some(requested))
}

/// 對單一物件投影。`id` 一律保留 —— 少了它前端無法辨識回傳的是哪一筆。
pub fn project(value: Value, fields: &Option<Vec<String>>) -> Value {
    let Some(fields) = fields else { return value };
    let Value::Object(map) = value else {
        return value;
    };

    Value::Object(
        map.into_iter()
            .filter(|(k, _)| k == "id" || fields.iter().any(|f| f == k))
            .collect(),
    )
}

/// 對陣列中每個元素投影。
pub fn project_all(values: Vec<Value>, fields: &Option<Vec<String>>) -> Vec<Value> {
    if fields.is_none() {
        return values;
    }
    values.into_iter().map(|v| project(v, fields)).collect()
}
