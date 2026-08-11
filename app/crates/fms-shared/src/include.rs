//! 關聯展開（`?include=children,relations`），對應契約各詳情端點的
//! `include` 參數。
//!
//! 與 [`crate::fields`] 是對稱的兩件事：`fields` 讓客戶端**收窄**回應，
//! `include` 讓客戶端**加寬**回應。兩者都以白名單驗證，未知值一律 422。
//!
//! # 為什麼未知值不能靜默忽略
//!
//! 靜默忽略時，`include=childrn`（拼錯）與 `include=children`（正確但該設備
//! 沒有子設備）在客戶端看來完全一樣 —— 都是「沒有 children」。前者是 bug，
//! 後者是正常結果，把兩者混為一談會讓前端花很久才發現拼錯。
//!
//! 同理，「契約列出但伺服器尚未實作」的值也回 422 並在訊息裡說明原因，
//! 而不是接受後回傳空陣列。空陣列是一個**斷言**（「這裡沒有資料」），
//! 對還沒實作的關聯而言那是謊。

use std::collections::BTreeSet;

use crate::problem::Problem;

/// 已驗證的展開集合。
#[derive(Debug, Default, Clone)]
pub struct Includes(BTreeSet<String>);

impl Includes {
    /// 是否要求展開某個關聯。
    pub fn has(&self, relation: &str) -> bool {
        self.0.contains(relation)
    }

    /// 是否什麼都沒要求。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 解析 `include` 參數。
///
/// `allowed` 是**本端點目前真能提供**的關聯；`deferred` 是契約列出但尚未
/// 實作的關聯 —— 後者回 422 並附上原因，讓客戶端知道是「還沒有」而不是
/// 「不存在」。
pub fn parse(
    raw: Option<&str>,
    allowed: &[&str],
    deferred: &[(&str, &str)],
) -> Result<Includes, Problem> {
    let Some(raw) = raw else {
        return Ok(Includes::default());
    };

    let mut out = BTreeSet::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if allowed.contains(&item) {
            out.insert(item.to_string());
            continue;
        }
        if let Some((_, why)) = deferred.iter().find(|(name, _)| *name == item) {
            return Err(Problem::validation(format!(
                "`include={item}` is defined in the API contract but not yet served: {why}"
            )));
        }
        return Err(Problem::validation(format!(
            "unknown value in `include`: `{item}`; available: {allowed:?}"
        )));
    }
    Ok(Includes(out))
}
