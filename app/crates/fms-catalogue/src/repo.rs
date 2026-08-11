//! 型錄查詢。
//!
//! RLS 已限定租戶，因此查詢不寫 `tenant_id` 條件。

use uuid::Uuid;

use fms_shared::{Cursor, Problem, TenantTx};

/// 排序固定 `display_order, code` —— 型錄是給人挑選的清單，
/// `display_order` 就是為此存在的欄位。契約沒有 `sort` 參數。
pub const SORT_COLUMN: &str = "display_order";

pub struct ServiceItemRow {
    pub id: Uuid,
    pub facility_id: Option<Uuid>,
    pub category: String,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub lead_time_minutes: i32,
    pub default_duration_minutes: i32,
    pub relative_offset_minutes: i32,
    pub is_attachable_to_reservation: bool,
    pub is_standalone_requestable: bool,
    pub requires_approval: bool,
    pub chargeable: bool,
    pub unit_price: Option<f64>,
    pub currency: Option<String>,
    pub unit_label: Option<String>,
    pub max_quantity: Option<i32>,
    pub form_schema: serde_json::Value,
    pub response_minutes: Option<i32>,
    pub resolution_minutes: Option<i32>,
    pub icon: Option<String>,
    pub display_order: i32,
}

impl ServiceItemRow {
    /// 游標鍵。與 ORDER BY（`display_order, code`）一致 ——
    /// 不一致的話翻頁會跳過或重複列。
    pub fn cursor_key(&self, _sort_column: &str) -> (String, Uuid) {
        // display_order 補零成固定寬度：游標是字串比較，
        // 不補零的話 "10" 會排在 "9" 前面。
        (format!("{:010}|{}", self.display_order, self.code), self.id)
    }
}

/// 列出某場域可申請的服務。
///
/// # `facility_id IS NULL` 代表全場域適用
///
/// 因此條件是「屬於這個場域**或**不限場域」，不是單純的相等比對。
/// 少了後半，全租戶共用的服務（例如 IT 支援）在任何場域都查不到。
///
/// 只回 `is_active` 且未軟刪除的項目：型錄是給人挑的，
/// 停用的項目出現在清單上只會產生一次注定失敗的請求。
#[allow(clippy::too_many_arguments)]
pub async fn list(
    tx: &mut TenantTx,
    facility_id: Uuid,
    category: Option<&str>,
    attachable: Option<bool>,
    standalone_only: Option<bool>,
    cursor: Option<&Cursor>,
    limit: i64,
) -> Result<Vec<ServiceItemRow>, Problem> {
    let (cursor_key, cursor_id) = match cursor {
        Some(c) => (Some(c.key.clone()), Some(c.uuid_id()?)),
        None => (None, None),
    };

    sqlx::query_as!(
        ServiceItemRow,
        r#"SELECT si.id,
                  si.facility_id,
                  si.category,
                  si.code::text AS "code!",
                  si.name::text AS "name!",
                  si.description,
                  si.lead_time_minutes,
                  si.default_duration_minutes,
                  si.relative_offset_minutes,
                  si.is_attachable_to_reservation,
                  si.is_standalone_requestable,
                  si.requires_approval,
                  si.chargeable,
                  si.unit_price::float8 AS "unit_price",
                  si.currency::text AS "currency",
                  si.unit_label::text AS "unit_label",
                  si.max_quantity,
                  si.form_schema AS "form_schema!",
                  -- **`?` 是必須的，不是風格。** 這兩欄來自 LEFT JOIN，而
                  -- `sla_policies.response_minutes` 在 schema 裡是 NOT NULL ——
                  -- sqlx 因此把它推論成非空，於是任何**沒有 sla_policy_id 的
                  -- 服務項目**都會讓整個清單以 500 收場（decode 遇到 NULL）。
                  --
                  -- 示範資料的三個項目剛好都有 SLA 政策，所以這個缺陷一直沒有
                  -- 被觸發；`POST /facilities/{id}/service-items` 讓它可達之後
                  -- 才被 `service_catalogue_slice.rs` 的 `a_` 抓到。
                  sp.response_minutes AS "response_minutes?",
                  sp.resolution_minutes AS "resolution_minutes?",
                  si.icon::text AS "icon",
                  si.display_order
           FROM fms.service_items si
           LEFT JOIN fms.sla_policies sp ON sp.id = si.sla_policy_id
           WHERE (si.facility_id = $1 OR si.facility_id IS NULL)
             AND si.is_active = true
             AND si.deleted_at IS NULL
             AND ($2::text IS NULL OR si.category = $2)
             AND ($3::boolean IS NULL OR si.is_attachable_to_reservation = $3)
             AND ($4::boolean IS NULL OR si.is_standalone_requestable = true)
             AND ($5::text IS NULL
                  OR (lpad(si.display_order::text, 10, '0') || '|' || si.code::text,
                      si.id) > ($5::text, $6::uuid))
           ORDER BY si.display_order, si.code, si.id
           LIMIT $7"#,
        facility_id,
        category,
        attachable,
        standalone_only.filter(|v| *v),
        cursor_key,
        cursor_id,
        limit + 1
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}
