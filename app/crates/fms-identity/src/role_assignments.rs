//! 角色指派（`/users/{id}/role-assignments`、`/role-assignments/{id}`）。
//!
//! # 兩道閘，缺一不可
//!
//! 1. **範圍**：`role:assign` 必須在**這次指派的範圍**上持有。擋的是
//!    「org A 的管理員把人指派進 org B」。走 016 的 `user_permission_codes`，
//!    子樹包含由它的 `org_path` 述詞決定，這裡不自己展開。
//! 2. **提權**：`fms.role_grant_blocked_by()`（052）—— **你不能授出一項
//!    你自己沒有的危險權限**。擋的是「ORG_MANAGER 把 TENANT_ADMIN 指派給自己」。
//!
//! 兩道分別擋不同的事。只有第 1 道時，ORG_MANAGER 在自己的 org 裡指派
//! TENANT_ADMIN 完全合法 —— 實測那會多拿 14 項權限，含 `asset:delete`
//! 與 `reservation:override`。只有第 2 道時，任何持有 `role:assign` 的人
//! 都能指派到任何範圍。
//!
//! 為什麼提權判定是 `is_dangerous` 而不是「權限子集」，量過的數字與
//! 被否決的另外兩個方案都寫在 `sql/052_role_assignment_escalation_guard.sql`
//! 的檔頭。一句話：子集規則會讓 ORG_MANAGER 連技師都指派不了（11 選 2）。
//!
//! # 讀取權限與契約不一致，這裡改了契約
//!
//! `api/ENDPOINTS.md` 原本寫 `GET` 需要 `role:read`。但目錄裡
//! **`role:read` 宣告 TENANT、`role:assign` 宣告 ORG**，而 ORG_MANAGER
//! 只有後者 —— 照契約做出來的結果是：
//!
//! > **ORG_MANAGER 指派得了角色，卻看不到自己指派了什麼。**
//!
//! 連撤銷都做不到，因為 `DELETE /role-assignments/{id}` 要 id，而 id 只能
//! 從那支看不到的清單拿。這裡改成 **`role:read` 或 `role:assign`**：
//! 今天沒有任何人因此失去存取（持有 `role:read` 的三個角色都同時持有
//! `role:assign`），而 ORG_MANAGER 拿回「看得到自己改得動的東西」。
//!
//! 契約已同步更新（ENDPOINTS.md 與 openapi.yaml）—— ADR-09 紀律 1 說契約是
//! 權威，那表示不一致要在契約上修掉，不是在實作裡繞過。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, has_permission, require_permission, require_tenant_scoped_permission, Caller,
    Problem, TenantTx,
};

#[derive(Clone)]
pub struct RoleAssignmentsState {
    pub pool: PgPool,
}

/// `scope_label` 讓清單看得懂：只回 `scope_id` 的話，UI 得為每一列再查一次
/// 那個 uuid 是哪個組織或場域。
const COLUMNS: &str = "ura.id, ura.user_id, r.code::text AS role_code, r.name::text AS role_name,
                       ura.scope_type, ura.scope_id,
                       CASE ura.scope_type
                         WHEN 'ORG'      THEN o.name
                         WHEN 'FACILITY' THEN f.name
                       END::text AS scope_label,
                       ura.source, ura.valid_until";

const FROM: &str = "FROM fms.user_role_assignments ura
                    JOIN fms.roles r ON r.id = ura.role_id
                    LEFT JOIN fms.organizations o ON o.id = ura.scope_id
                    LEFT JOIN fms.facilities f    ON f.id = ura.scope_id";

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RoleAssignmentDto {
    pub id: Uuid,
    pub user_id: Uuid,
    pub role_code: String,
    pub role_name: String,
    pub scope_type: String,
    pub scope_id: Option<Uuid>,
    pub scope_label: Option<String>,
    pub source: String,
    pub valid_until: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct AssignBody {
    pub role_code: Option<String>,
    pub scope_type: Option<String>,
    pub scope_id: Option<Uuid>,
    pub valid_until: Option<chrono::DateTime<chrono::Utc>>,
}

/// `GET /users/{userId}/role-assignments`
///
/// 不分頁：一個人的角色指派是個位數，而契約的 `RoleAssignment` 也沒有
/// 分頁欄位。**包含已到期的指派**（`valid_until` 在過去）—— 那是刻意的：
/// 「他為什麼上週還進得來」是這支端點要回答的問題之一。
pub async fn list(
    State(state): State<RoleAssignmentsState>,
    caller: Caller,
    Path(user_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // `role:read` 或 `role:assign` —— 見模組檔頭。
    if !has_permission(&mut tx, "role:read", None, None).await?
        && !has_permission(&mut tx, "role:assign", None, None).await?
    {
        return Err(Problem::permission_denied(
            "missing permission: role:read 或 role:assign",
        ));
    }

    let rows: Vec<RoleAssignmentDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} {FROM}
          WHERE ura.user_id = $1
          ORDER BY r.code, ura.scope_type"
    ))
    .bind(user_id)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

/// `POST /users/{userId}/role-assignments`
pub async fn assign(
    State(state): State<RoleAssignmentsState>,
    caller: Caller,
    Path(user_id): Path<Uuid>,
    Json(body): Json<AssignBody>,
) -> Result<(StatusCode, Json<RoleAssignmentDto>), Problem> {
    let role_code = body
        .role_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("role_code 為必填"))?;
    let scope_type = body.scope_type.as_deref().unwrap_or("");

    // SPATIAL_NODE 在契約的 enum 裡，但 016 的述詞只認 TENANT／FACILITY／ORG
    // ——「建得起來、一項權限都不生效」比直接拒絕更難查，所以直接拒絕。
    match scope_type {
        "TENANT" | "ORG" | "FACILITY" => {}
        "SPATIAL_NODE" => {
            return Err(Problem::validation(
                "SPATIAL_NODE 範圍的指派不會生效：權限判定只認 TENANT／FACILITY／ORG",
            ))
        }
        _ => {
            return Err(Problem::validation(
                "scope_type 必須是 TENANT／ORG／FACILITY",
            ))
        }
    }
    // 鏡射 002 的 ck_ura_scope。讓約束擋只會得到一個 23514，訊息裡沒有原因。
    if (scope_type == "TENANT") != body.scope_id.is_none() {
        return Err(Problem::validation(
            "scope_type = TENANT 時不可帶 scope_id；其餘 scope_type 必須帶",
        ));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // --- 閘 1：role:assign 必須在這次指派的範圍上持有 ---------------------
    require_assign_in_scope(&mut tx, scope_type, body.scope_id).await?;

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

    // --- 閘 2：不能授出自己沒有的危險權限（052） -------------------------
    let blocked: Vec<String> =
        sqlx::query_scalar("SELECT c::text FROM fms.role_grant_blocked_by($1, $2, $3, $4) c")
            .bind(caller.user_id)
            .bind(role_id)
            .bind(scope_type)
            .bind(body.scope_id)
            .fetch_all(tx.conn())
            .await?;
    if !blocked.is_empty() {
        // 說得出缺哪幾項。只回「不行」會讓對方開一張工單來問。
        return Err(Problem::permission_denied(format!(
            "不能指派 {role_code}：它帶有你在這個範圍並未持有的危險權限 —— {}",
            blocked.join("、")
        )));
    }

    let row: RoleAssignmentDto = sqlx::query_as(&format!(
        "WITH ins AS (
           INSERT INTO fms.user_role_assignments
             (tenant_id, user_id, role_id, scope_type, scope_id, source, granted_by, valid_until)
           VALUES (fms.current_tenant_id(), $1, $2, $3, $4, 'MANUAL', $5, $6)
           RETURNING *)
         SELECT {COLUMNS} FROM ins ura
           JOIN fms.roles r ON r.id = ura.role_id
           LEFT JOIN fms.organizations o ON o.id = ura.scope_id
           LEFT JOIN fms.facilities f    ON f.id = ura.scope_id"
    ))
    .bind(user_id)
    .bind(role_id)
    .bind(scope_type)
    .bind(body.scope_id)
    .bind(caller.user_id)
    .bind(body.valid_until)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `DELETE /role-assignments/{id}`
///
/// 撤銷的範圍判定用**那筆指派自己的範圍**，不是請求帶來的參數 ——
/// 否則「我宣稱它在我的 org 裡」就足以撤銷任何一筆。
pub async fn revoke(
    State(state): State<RoleAssignmentsState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let existing: Option<(Uuid, String, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT user_id, scope_type, scope_id, source
           FROM fms.user_role_assignments WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(tx.conn())
    .await?;
    let (target_user, scope_type, scope_id, source) =
        existing.ok_or_else(|| Problem::not_found("找不到這筆角色指派"))?;

    require_assign_in_scope(&mut tx, &scope_type, scope_id).await?;

    // 目錄同步下一輪就會把它加回來。回 204 等於報告一個不會成立的結果。
    if source == "DIRECTORY_SYNC" {
        return Err(Problem::validation(
            "這筆指派來自目錄同步，撤銷後下一輪同步會再建回來 —— \
             要移除請改群組對應（/directory-role-mappings）",
        ));
    }
    // 與 POST /users/{id}/suspend 同一條理由：撤掉自己最後一個管理角色之後，
    // 沒有人能把你放回來。
    if target_user == caller.user_id {
        return Err(Problem::validation(
            "不能撤銷自己的角色 —— 若那是你最後一個管理角色，就沒有人能把你放回來",
        ));
    }

    sqlx::query("DELETE FROM fms.user_role_assignments WHERE id = $1")
        .bind(id)
        .execute(tx.conn())
        .await?;
    tx.commit().await?;

    Ok(StatusCode::NO_CONTENT)
}

/// 閘 1。`TENANT` 走 `require_tenant_scoped_permission` 而不是
/// `require_permission(.., None, None)` —— 後者的語意是「在**任一**範圍持有」，
/// 那會讓一個場域級的授權足以指派到全租戶（見 `db.rs` 對兩者差異的說明）。
async fn require_assign_in_scope(
    tx: &mut TenantTx,
    scope_type: &str,
    scope_id: Option<Uuid>,
) -> Result<(), Problem> {
    match scope_type {
        "TENANT" => {
            require_tenant_scoped_permission(tx, "role:assign").await?;
        }
        "ORG" => {
            require_permission(tx, "role:assign", None, scope_id).await?;
        }
        _ => {
            require_permission(tx, "role:assign", scope_id, None).await?;
        }
    }
    Ok(())
}

fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        // 目標使用者不在這個租戶（或不存在）。FK 的原始訊息只講欄位名。
        Some("user_role_assignments_user_id_fkey") => {
            Problem::not_found("找不到這個使用者（或不屬於這個租戶）")
        }
        // 同一個人＋同一個角色＋同一個範圍只能有一筆。這一格是測試抓出來的：
        // 原本會回一個沒有主詞的「a conflicting record already exists」，
        // 而重複指派最可能的成因（目錄同步已經給過了）看不出來。
        Some("uq_user_role_assignments") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail(
                "這個人在這個範圍已經有這個角色了 —— \
                 若清單裡看不到，檢查它是不是來自目錄同步（source = DIRECTORY_SYNC）",
            ),
        // `scope_id` 沒有外鍵（它指向四種不同的表），因此這裡只可能是 CHECK。
        Some("ck_ura_scope") => Problem::validation("scope_id 與 scope_type 不相符"),
        _ => Problem::from(err),
    }
}
