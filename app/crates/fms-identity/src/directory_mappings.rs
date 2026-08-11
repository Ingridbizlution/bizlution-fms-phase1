//! 目錄群組 → 角色對應（`/directory-role-mappings`）。
//!
//! # 這三支端點不存在時，一個錯誤訊息指向了不存在的地方
//!
//! `DELETE /role-assignments/{id}` 拒絕撤銷 `source = DIRECTORY_SYNC` 的授權
//! （撤了下一輪同步就加回來），訊息寫「要移除請改群組對應
//! （/directory-role-mappings）」—— 而那三支端點當時契約與實作都不存在。
//!
//! # 這裡是 052 的一條繞道，若不補上同一道閘
//!
//! `POST /users/{id}/role-assignments` 有提權防護：**你不能授出一項你自己
//! 沒有的危險權限**。實測 `TENANT_ADMIN` 直接指派 `PLATFORM_ADMIN` 會被擋下
//! —— 他沒有 `user:impersonate`。
//!
//! 但一條 `群組 X → PLATFORM_ADMIN @ TENANT` 的對應做的是同一件事，
//! 只是晚一輪同步才生效。少了守衛，`role:write` 就等於 `role:assign` 的
//! 無限制版本。因此這裡呼叫**同一支** `fms.role_grant_blocked_by()`，
//! 而不是另寫一份判斷（053 的教訓：一條判定散成兩份手抄本就會漂移）。
//!
//! # 刪除對應不會撤銷已經發出的授權
//!
//! 那些 `user_role_assignments` 仍然存在，直到下一輪同步收回。
//! 回應因此帶 `orphaned_assignments` —— 少了那個數字，
//! 「我刪掉對應了，為什麼他還進得來」會變成一次除錯，而答案只是「還沒同步」。
//!
//! # `claim_value` 不再被接受：它沒有消費者（migration 077）
//!
//! 這支端點原本照 002 的 `ck_drm_source` 放行「兩個來源二選一」，只填
//! `claim_value` 也回 201。**但 058 的對帳是對 `directory_groups` 的內連接**，
//! 那種列在第一個 JOIN 就被丟掉 —— 建得起來、回 201、永遠不授予任何角色、
//! 而且沒有任何症狀。
//!
//! 修法與同一個函式裡 `SPATIAL_NODE` 那一條**同一個判斷**：一條永遠不會生效的
//! 授權規則不是資訊不足，是設定錯誤，所以回 422 而不是「201 + 一句提醒」。
//! 讓它建得起來，管理者就會以為某個存取受一條規則管著，而那條規則什麼都沒做。
//!
//! 422 也說得出**真正的前置條件**（先讓群組同步進來），
//! 一句「這條目前不會生效」說不出。完整量測（claim 值該跟什麼比，
//! 以及為什麼四個候選全都走不通）在 migration 077 的檔頭。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_tenant_scoped_permission, Caller, Problem};

#[derive(Clone)]
pub struct DirectoryMappingsState {
    pub pool: PgPool,
}

const COLUMNS: &str = "m.id, m.directory_group_id, g.name AS directory_group_name,
                       m.claim_value, r.code::text AS role_code, r.name::text AS role_name,
                       m.scope_type, m.scope_id,
                       CASE m.scope_type
                         WHEN 'ORG'      THEN o.name
                         WHEN 'FACILITY' THEN f.name
                       END::text AS scope_label,
                       m.priority, m.is_active";

const FROM: &str = "FROM fms.directory_role_mappings m
                    JOIN fms.roles r ON r.id = m.role_id
                    LEFT JOIN fms.directory_groups g ON g.id = m.directory_group_id
                    LEFT JOIN fms.organizations o ON o.id = m.scope_id
                    LEFT JOIN fms.facilities f    ON f.id = m.scope_id";

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct MappingDto {
    pub id: Uuid,
    pub directory_group_id: Option<Uuid>,
    pub directory_group_name: Option<String>,
    pub claim_value: Option<String>,
    pub role_code: String,
    pub role_name: String,
    pub scope_type: String,
    pub scope_id: Option<Uuid>,
    pub scope_label: Option<String>,
    pub priority: i32,
    pub is_active: bool,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub is_active: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct MappingCreate {
    pub directory_group_id: Option<Uuid>,
    pub claim_value: Option<String>,
    pub role_code: Option<String>,
    pub scope_type: Option<String>,
    pub scope_id: Option<Uuid>,
    pub priority: Option<i32>,
    pub is_active: Option<bool>,
}

/// `GET /directory-role-mappings`
pub async fn list(
    State(state): State<DirectoryMappingsState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "role:read").await?;

    let rows: Vec<MappingDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} {FROM}
          WHERE ($1::bool IS NULL OR m.is_active = $1::bool)
          ORDER BY m.priority, r.code"
    ))
    .bind(q.is_active)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

/// `POST /directory-role-mappings`
pub async fn create(
    State(state): State<DirectoryMappingsState>,
    caller: Caller,
    Json(body): Json<MappingCreate>,
) -> Result<(StatusCode, Json<MappingDto>), Problem> {
    let role_code = body
        .role_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("role_code 為必填"))?;
    // 鏡射 077 的 ck_drm_group_required。交給 CHECK 擋只會得到一個 23514，
    // 而那個訊息說不出「該去哪裡拿一個 directory_group_id」。
    let group_id = body.directory_group_id.ok_or_else(|| {
        Problem::validation(
            "directory_group_id 為必填 —— 對應必須錨定在一列已同步的目錄群組上。\
             同步的對帳只認群組（migration 058 是內連接），\
             因此只填 claim_value 的對應永遠不會授予任何角色",
        )
    })?;
    // **不是靜默忽略。** 這個欄位目前沒有任何消費者（見模組檔頭與 077），
    // 而「存進去、讀得回來、但沒有人拿它比對」正是這條規則要防的那個缺陷。
    if body
        .claim_value
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        return Err(Problem::validation(
            "claim_value 目前沒有任何消費者，因此不接受寫入：對帳只比對已同步的\
             群組成員關係，填了它不會改變任何授權結果。\
             要用原始 claim 比對，還需要一條會寫入 raw_claims 的登入流程、\
             一個以 claim 為鍵的成員關係存放處，以及一個以 claim 為鍵的收回身分\
             （見 migration 077）",
        ));
    }

    let scope_type = body.scope_type.as_deref().unwrap_or("");
    match scope_type {
        "TENANT" | "ORG" | "FACILITY" => {}
        // 與角色指派同一個理由：016 的述詞只認三種 scope_type，
        // SPATIAL_NODE 產生的授權一項權限都不會生效。
        "SPATIAL_NODE" => {
            return Err(Problem::validation(
                "SPATIAL_NODE 範圍的對應會產生不生效的授權：權限判定只認 TENANT／FACILITY／ORG",
            ))
        }
        _ => {
            return Err(Problem::validation(
                "scope_type 必須是 TENANT／ORG／FACILITY",
            ))
        }
    }
    if (scope_type == "TENANT") != body.scope_id.is_none() {
        return Err(Problem::validation(
            "scope_type = TENANT 時不可帶 scope_id；其餘 scope_type 必須帶",
        ));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "role:write").await?;

    let role_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM fms.roles
          WHERE lower(code) = lower($1)
            AND (tenant_id IS NULL OR tenant_id = fms.current_tenant_id())
            AND is_assignable
          ORDER BY tenant_id NULLS LAST
          LIMIT 1",
    )
    .bind(role_code)
    .fetch_optional(tx.conn())
    .await?
    .ok_or_else(|| {
        Problem::validation(format!(
            "找不到可指派的角色 {role_code}（不存在、屬於別的租戶，或 is_assignable = false）"
        ))
    })?;

    // **與 POST /users/{id}/role-assignments 同一道閘。** 見模組檔頭：
    // 少了它，`role:write` 就是 `role:assign` 的無限制版本。
    let blocked: Vec<String> =
        sqlx::query_scalar("SELECT c::text FROM fms.role_grant_blocked_by($1, $2, $3, $4) c")
            .bind(caller.user_id)
            .bind(role_id)
            .bind(scope_type)
            .bind(body.scope_id)
            .fetch_all(tx.conn())
            .await?;
    if !blocked.is_empty() {
        return Err(Problem::permission_denied(format!(
            "不能把 {role_code} 對應到目錄群組：它帶有你在這個範圍並未持有的危險權限 —— {}。\
             （這道閘與直接指派角色的那一道是同一個，否則目錄對應就是一條繞道）",
            blocked.join("、")
        )));
    }

    let row: MappingDto = sqlx::query_as(&format!(
        // claim_value 不在欄位清單裡：它沒有消費者，寫進去只會讓下一個讀
        // 這張表的人以為有（見模組檔頭與 077）。
        "WITH ins AS (
           INSERT INTO fms.directory_role_mappings
             (tenant_id, directory_group_id, role_id,
              scope_type, scope_id, priority, is_active)
           VALUES (fms.current_tenant_id(), $1, $2, $3, $4,
                   coalesce($5, 100), coalesce($6, true))
           RETURNING *)
         SELECT {COLUMNS} FROM ins m
           JOIN fms.roles r ON r.id = m.role_id
           LEFT JOIN fms.directory_groups g ON g.id = m.directory_group_id
           LEFT JOIN fms.organizations o ON o.id = m.scope_id
           LEFT JOIN fms.facilities f    ON f.id = m.scope_id"
    ))
    .bind(group_id)
    .bind(role_id)
    .bind(scope_type)
    .bind(body.scope_id)
    .bind(body.priority)
    .bind(body.is_active)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `DELETE /directory-role-mappings/{id}`
///
/// 回 200 而不是 204：**要帶回一個數字**。刪除對應不會撤銷已經發出的授權，
/// 而「我刪掉對應了，為什麼他還進得來」不該變成一次除錯。
pub async fn delete(
    State(state): State<DirectoryMappingsState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "role:write").await?;

    // 先數再刪：刪掉之後 `origin_directory_group_id` 那條線就斷了。
    // 用群組而不是對應本身，因為 `user_role_assignments` 記的是來源群組
    // （002 的 `origin_directory_group_id`），沒有指回對應的欄位。
    let orphaned: Option<i64> = sqlx::query_scalar(
        "SELECT count(ura.id)
           FROM fms.directory_role_mappings m
           LEFT JOIN fms.user_role_assignments ura
                  ON ura.origin_directory_group_id = m.directory_group_id
                 AND ura.role_id = m.role_id
          WHERE m.id = $1
          GROUP BY m.id",
    )
    .bind(id)
    .fetch_optional(tx.conn())
    .await?;
    let orphaned = orphaned.ok_or_else(|| Problem::not_found("找不到這條對應"))?;

    sqlx::query("DELETE FROM fms.directory_role_mappings WHERE id = $1")
        .bind(id)
        .execute(tx.conn())
        .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "deleted": id,
        "orphaned_assignments": orphaned,
    })))
}

fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        Some("directory_role_mappings_directory_group_id_fkey") => {
            Problem::validation("找不到這個目錄群組（或它不屬於這個租戶）")
        }
        Some("ck_drm_scope") => Problem::validation("scope_id 與 scope_type 不相符"),
        // 077 的約束。handler 已經先擋過了，所以走到這裡代表兩邊漂移了
        // （例如日後放寬了 handler 卻沒有一起改約束）—— 訊息要說得出是哪一條。
        Some("ck_drm_group_required") => Problem::validation(
            "對應必須錨定在一列已同步的目錄群組上（migration 077 的 ck_drm_group_required）",
        ),
        _ => Problem::from(err),
    }
}
