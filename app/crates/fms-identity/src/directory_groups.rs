//! `GET /directory-groups` —— 已同步進來的 AD／Entra 群組。
//!
//! # 這張表的列是誰寫進來的
//!
//! **不是這個系統。** `fms.directory_groups` 與 `fms.user_directory_groups`
//! 在 Phase 1 由外部填入（migration 058 檔頭：「去外部目錄抓成員關係」那一半
//! 需要 LDAP／Graph 客戶端，Phase 1 沒有）。這支端點是**讀取面**，讓管理者
//! 看得到「系統目前認得哪些群組」，因為那決定了
//! `directory_role_mappings` 能對應到什麼。
//!
//! 因此這支端點最有用的資訊不是群組清單本身，而是兩個計數：
//!
//!   * `groups_never_synced` —— `last_synced_at` 是 NULL 的群組。它們是
//!     「建了一列但從來沒有真的同步過」，而對應到這種群組的規則永遠不會
//!     產生任何角色指派。
//!   * `groups_not_mapped_to_any_role` —— 同步進來了卻沒有任何對應。
//!     這一半是反過來的缺口：目錄裡有這個群組，但它在 FMS 裡不代表任何權限。
//!
//! 兩個數字都不是錯誤，是**覆蓋缺口的量測值**。不回報的話，一份看起來正常的
//! 群組清單會讓人以為授權已經接好了。

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_tenant_scoped_permission, Caller, Cursor, PageMeta,
    Problem, SortSpec,
};

#[derive(Clone)]
pub struct DirectoryGroupsState {
    pub pool: PgPool,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
    /// 只看某一個身分來源的群組。
    pub identity_provider_id: Option<Uuid>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct GroupDto {
    pub id: Uuid,
    pub identity_provider_id: Uuid,
    pub external_group_id: String,
    pub name: String,
    pub distinguished_name: Option<String>,
    pub description: Option<String>,
    /// 002 的欄位，由同步流程寫入。**注意它與 `member_count_in_fms` 可能不同**：
    /// 前者是目錄那邊的人數，後者是這個系統真的認得幾個人。兩者不一致代表
    /// 有成員在 FMS 裡沒有對應的使用者。
    pub member_count: i32,
    /// `user_directory_groups` 裡實際有幾列指向這個群組。
    pub member_count_in_fms: i64,
    /// 有幾條 `directory_role_mappings` 對應到這個群組。0 代表這個群組
    /// 在 FMS 裡不代表任何權限。
    ///
    /// **只算 `directory_group_id` 相符的對應，不算 `claim_value` 的。**
    /// 那不是疏漏：058 的對帳是
    /// `JOIN fms.directory_groups g ON g.id = m.directory_group_id`（內連接），
    /// 因此只填 `claim_value` 的對應**永遠不會產生任何角色指派**。
    /// 把它們算進來會讓這個數字說「這個群組有授權」，而實際上沒有。
    pub role_mapping_count: i64,
    pub last_synced_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// `GET /directory-groups`
///
/// 需要 `identity_provider:read`（TENANT 範圍 —— 目錄設定不是場域層級的資料）。
pub async fn list(
    State(state): State<DirectoryGroupsState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "identity_provider:read").await?;

    let limit = clamp_limit(q.limit);
    // 以 name 排序：管理者是照名字找群組的。name 不唯一（兩個來源可以有
    // 同名群組），因此游標是 (name, id) 的複合鍵。
    let sort = SortSpec {
        column: "name".to_string(),
        desc: false,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.key.clone()), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<GroupDto> = sqlx::query_as(
        r#"
        SELECT g.id, g.identity_provider_id, g.external_group_id::text AS external_group_id,
               g.name::text AS name, g.distinguished_name, g.description,
               g.member_count,
               (SELECT count(*) FROM fms.user_directory_groups udg
                 WHERE udg.directory_group_id = g.id)      AS member_count_in_fms,
               (SELECT count(*) FROM fms.directory_role_mappings m
                 WHERE m.directory_group_id = g.id)        AS role_mapping_count,
               g.last_synced_at, g.created_at
          FROM fms.directory_groups g
         WHERE ($1::uuid IS NULL OR g.identity_provider_id = $1::uuid)
           AND ($2::text IS NULL OR (g.name::text, g.id) > ($2::text, $3::uuid))
         ORDER BY g.name, g.id
         LIMIT $4
        "#,
    )
    .bind(q.identity_provider_id)
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;

    // 兩個缺口計數。**跨整個租戶算，不是只算這一頁** —— 分頁的那一頁裡
    // 剛好沒有問題，不代表沒有問題。
    let gaps: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*)                                                    AS total,
               count(*) FILTER (WHERE g.last_synced_at IS NULL)            AS never_synced,
               count(*) FILTER (WHERE NOT EXISTS (
                 SELECT 1 FROM fms.directory_role_mappings m
                  WHERE m.directory_group_id = g.id))                      AS unmapped
          FROM fms.directory_groups g
         WHERE ($1::uuid IS NULL OR g.identity_provider_id = $1::uuid)
        "#,
    )
    .bind(q.identity_provider_id)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.name.clone(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
        "meta": {
            "total_groups": gaps.0,
            // 建了一列但從來沒同步過。對應到它的規則永遠不會產生角色指派。
            "groups_never_synced": gaps.1,
            // 同步進來了但沒有任何對應 —— 目錄裡有這個群組，
            // 但它在 FMS 裡不代表任何權限。
            "groups_not_mapped_to_any_role": gaps.2,
            // 這些列的來源。寫在回應裡是因為「為什麼清單是空的」這個問題
            // 的答案不在這個系統裡，而在外部有沒有推資料進來。
            "populated_by": "外部目錄同步寫入 directory_groups／user_directory_groups；\
                             Phase 1 沒有 LDAP／Graph 客戶端（見 migration 058 檔頭），\
                             因此這張表不會因為呼叫 POST /identity-providers/{id}/sync 而長出新列",
        },
    })))
}
