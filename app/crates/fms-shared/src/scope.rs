//! 列級讀取範圍（WBS 3.9）。
//!
//! 權限目錄裡有兩個 `_own` 權限：`work_order:read_own` 與
//! `reservation:read_own`。它們**不是**「較弱的 read」，而是換了一個維度 ——
//! read 限制的是「哪些場域」，read_own 限制的是「哪些列」。
//!
//! 這件事非做不可的理由不是完整度：`REQUESTER`、`TECHNICIAN`、
//! `SERVICE_STAFF` 三個角色**只有** `_own`，若端點只檢查完整的 read，
//! 這三個角色連自己報修的工單都看不到。也就是說少了這一層，
//! 系統對絕大多數實際使用者是不可用的。

use uuid::Uuid;

use crate::db::{permission_codes, TenantTx};
use crate::problem::Problem;

/// 這個使用者在此端點上看得到哪些列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadScope {
    /// 場域內全部。
    All,
    /// 只有與自己相關的列（申請人／負責人／主辦人，各模組自行定義）。
    Own(Uuid),
}

impl ReadScope {
    /// `Own` 時回傳使用者 id，供列表查詢當成過濾條件。
    pub fn own_user_id(self) -> Option<Uuid> {
        match self {
            Self::All => None,
            Self::Own(id) => Some(id),
        }
    }
}

/// 判定讀取範圍。
///
/// 順序刻意是「先看完整權限」：同時擁有 `read` 與 `read_own` 的角色
/// （`TENANT_ADMIN`、`VIEWER`、`PLATFORM_ADMIN`）應該看到全部，
/// 而不是被較窄的那個綁住。
///
/// 這個「聯集」語意是明確的架構決定，見 ADR-11：角色是純加法，系統不提供
/// deny。要收窄某個人的可見範圍，正確操作是不要給他那個較寬的角色，
/// 而不是額外給他一個較窄的。
///
/// 兩者都沒有才是 403 —— 這與「有 `read_own` 但這一列不是你的」是
/// 不同的情況，後者由各模組自行處理（見 `deny_unless_own`）。
pub async fn read_scope(
    tx: &mut TenantTx,
    full: &str,
    own: &str,
    facility_id: Option<Uuid>,
) -> Result<ReadScope, Problem> {
    let user_id = tx.context().user_id;
    // 一次取回整組權限，因此問兩個權限碼只有一次往返。
    let codes = permission_codes(tx, facility_id, None).await?;

    if codes.contains(full) {
        return Ok(ReadScope::All);
    }
    if codes.contains(own) {
        return Ok(ReadScope::Own(user_id));
    }
    Err(Problem::permission_denied(format!(
        "missing permission: {full} (or {own})"
    )))
}

/// 單筆讀取時的擁有權檢查。
///
/// # 為什麼回 404 而不是 403
///
/// 只有 `read_own` 的使用者拿到別人的資源 id 時，「不是你的」與「不存在」
/// 刻意**不可分辨**。工單編號是租戶內連號（`WO-2026-000482`），
/// 403 與 404 若可區分，一個 `REQUESTER` 只要逐號試探就能量出整個租戶的
/// 工單量與編號分布。這類推論在同租戶內雖然不致命，但沒有任何理由送給他。
///
/// 契約在這些端點上也只定義了 404，因此回 404 同時是契約相符的。
pub fn deny_unless_own(
    scope: ReadScope,
    owners: &[Option<Uuid>],
    what: &str,
) -> Result<(), Problem> {
    let Some(me) = scope.own_user_id() else {
        return Ok(());
    };
    if owners.contains(&Some(me)) {
        return Ok(());
    }
    Err(Problem::not_found(format!("{what} not found")))
}
