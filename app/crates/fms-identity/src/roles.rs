//! 角色目錄與權限字典（`/roles`、`/permissions`）。
//!
//! # 為什麼 `GET /roles` 收 `role:assign`
//!
//! 與 `GET /users/{id}/role-assignments` 是同一個病灶。目錄裡只有
//! `PLATFORM_ADMIN` 與 `TENANT_ADMIN` 持有 `role:read`（宣告 TENANT），
//! 而 `ORG_MANAGER` 持有的是 `role:assign`（宣告 ORG）——
//! 照原契約做，他**指派得了角色卻列不出有哪些角色可以指派**，
//! UI 連下拉選單都填不出來。
//!
//! # `permissions.is_dangerous` 為什麼一定要回給前端
//!
//! 052 之前它是裝飾（四個 migration 寫它、零個讀者）。052 之後它決定
//! 「誰可以把這項權限授出去」。管理員看不到它，就無法理解
//! `POST /users/{id}/role-assignments` 為什麼回 403。
//!
//! `min_scope_level` 同理，而且更隱蔽：一項宣告 TENANT 的權限被指派在
//! ORG 範圍時**不會報錯，只是靜默地不生效**（026 在視圖層過濾）。
//!
//! # `POST /roles` 的守衛是縱深防禦，不是缺口
//!
//! 提權鏈是「鑄造一個含任意權限的角色 → 指派給自己」。**第二步已經被 052
//! 擋住了**（`role_grant_blocked_by`），所以鏈是斷的 —— `roles_slice` 有一格
//! 直接跑完整條鏈來證明這件事，而不是靠推論。
//!
//! 仍然擋在鑄造這一步的理由是：一個沒有人指派得了的角色會出現在角色清單裡，
//! 而「為什麼這個角色永遠指派不了」是一次除錯。擋在源頭，訊息才說得出原因。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, has_permission, require_tenant_scoped_permission, Caller, Problem,
};

#[derive(Clone)]
pub struct RolesState {
    pub pool: PgPool,
}

const SCOPE_LEVELS: [&str; 4] = ["TENANT", "ORG", "FACILITY", "SPATIAL_NODE"];

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RoleDto {
    pub id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub is_system: bool,
    pub is_assignable: bool,
    pub scope_level: String,
    pub permissions: Vec<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PermissionDto {
    pub code: String,
    pub resource: String,
    pub action: String,
    pub module: String,
    pub description: Option<String>,
    pub min_scope_level: String,
    pub is_dangerous: bool,
}

#[derive(Debug, Deserialize)]
pub struct RoleQuery {
    pub q: Option<String>,
    pub assignable_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct PermissionQuery {
    pub module: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RoleCreate {
    pub code: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub scope_level: Option<String>,
    pub permissions: Option<Vec<String>>,
}

/// `GET /roles`
///
/// 平台角色（`tenant_id IS NULL`）與本租戶的自訂角色。**不分頁** ——
/// 角色是目錄不是資料，現行 12 個平台角色，自訂角色的數量級是「幾個」。
pub async fn list(
    State(state): State<RolesState>,
    caller: Caller,
    Query(q): Query<RoleQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_read_or_assign(&mut tx).await?;

    let rows: Vec<RoleDto> = sqlx::query_as(
        "SELECT r.id, r.tenant_id, r.code::text AS code, r.name::text AS name,
                r.description, r.is_system, r.is_assignable, r.scope_level,
                coalesce(
                  (SELECT array_agg(rp.permission_code::text ORDER BY rp.permission_code)
                     FROM fms.role_permissions rp WHERE rp.role_id = r.id),
                  ARRAY[]::text[]) AS permissions
           FROM fms.roles r
          WHERE ($1::text IS NULL
                 OR r.code ILIKE '%' || $1 || '%' OR r.name ILIKE '%' || $1 || '%')
            AND (NOT $2::bool OR r.is_assignable)
          ORDER BY r.tenant_id NULLS FIRST, r.code",
    )
    .bind(q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(q.assignable_only.unwrap_or(false))
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

/// `GET /permissions`
///
/// 平台層的字典，與租戶無關 —— 因此不做租戶過濾（`fms.permissions` 沒有
/// `tenant_id`）。仍然走 `begin_tenant_tx`：認證與情境注入是同一條路徑。
pub async fn list_permissions(
    State(state): State<RolesState>,
    caller: Caller,
    Query(q): Query<PermissionQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "role:read").await?;

    let rows: Vec<PermissionDto> = sqlx::query_as(
        "SELECT code::text AS code, resource::text AS resource, action::text AS action,
                module::text AS module, description, min_scope_level, is_dangerous
           FROM fms.permissions
          WHERE $1::text IS NULL OR upper(module) = upper($1)
          ORDER BY module, resource, action",
    )
    .bind(q.module.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

/// `POST /roles`
pub async fn create(
    State(state): State<RolesState>,
    caller: Caller,
    Json(body): Json<RoleCreate>,
) -> Result<(StatusCode, Json<RoleDto>), Problem> {
    let code = required(&body.code, "code")?;
    let name = required(&body.name, "name")?;
    let scope_level = body.scope_level.as_deref().unwrap_or("FACILITY");
    if !SCOPE_LEVELS.contains(&scope_level) {
        return Err(Problem::validation(format!(
            "scope_level 必須是 {} 其中之一",
            SCOPE_LEVELS.join("／")
        )));
    }
    let permissions = body.permissions.unwrap_or_default();

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // `role:write` 的 min_scope_level 就是 TENANT，而角色目錄是租戶級資源。
    require_tenant_scoped_permission(&mut tx, "role:write").await?;

    // 002 的唯一索引是 `(coalesce(tenant_id, 全零 uuid), lower(code))`，
    // 因此平台角色與租戶角色分屬**不同**命名空間 —— 資料庫允許租戶建一個
    // 叫 `technician` 的角色。這裡擋下來，因為那會造成無聲的遮蔽：
    // `assign` 解析 role_code 時 `ORDER BY tenant_id NULLS LAST`，
    // 同名的租戶角色會蓋過平台角色，於是「我指派了 TECHNICIAN，
    // 他卻什麼都不能做」—— 而錯誤訊息一個都不會出現。
    //
    // 這一格是測試逼出來的：我原本以為兩者共用命名空間，寫了一格期待 409
    // 的測試，結果回 201。假設錯了，但那一格問對了問題。
    let shadows: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM fms.roles
                         WHERE tenant_id IS NULL AND lower(code) = lower($1))",
    )
    .bind(code)
    .fetch_one(tx.conn())
    .await?;
    if shadows {
        return Err(
            Problem::new(fms_shared::ProblemCode::Conflict).with_detail(format!(
                "{code} 與一個平台角色同名（不分大小寫）。\
             資料庫允許，但同名的租戶角色會在指派時遮蔽平台角色，\
             結果是「指派了卻什麼都不能做」而且沒有任何錯誤訊息。請換一個 code。"
            )),
        );
    }

    // 先擋不存在的權限碼。交給外鍵擋只會得到一個 23503，訊息裡看不出是哪一個
    // 碼拼錯了 —— 而權限碼是手打的字串，拼錯是常態不是例外。
    let unknown: Vec<String> = sqlx::query_scalar(
        "SELECT c FROM unnest($1::text[]) c
          WHERE NOT EXISTS (SELECT 1 FROM fms.permissions p WHERE p.code = c)",
    )
    .bind(&permissions)
    .fetch_all(tx.conn())
    .await?;
    if !unknown.is_empty() {
        return Err(Problem::validation(format!(
            "找不到這些權限碼：{}（可用清單見 GET /permissions）",
            unknown.join("、")
        )));
    }

    // 縱深防禦，見模組檔頭。判定用**TENANT 範圍**持有與否：新角色是租戶級
    // 目錄物件，它可以被指派到任何範圍，因此「我在某個場域持有」不足以授出。
    let blocked: Vec<String> = sqlx::query_scalar(
        "SELECT c FROM unnest($1::text[]) c
           JOIN fms.permissions p ON p.code = c
          WHERE p.is_dangerous
            AND c NOT IN (SELECT h::text FROM fms.user_permission_codes($2, NULL, NULL) h)
          ORDER BY c",
    )
    .bind(&permissions)
    .bind(caller.user_id)
    .fetch_all(tx.conn())
    .await?;
    if !blocked.is_empty() {
        return Err(Problem::permission_denied(format!(
            "不能把自己沒有的危險權限放進新角色 —— {}",
            blocked.join("、")
        )));
    }

    let role_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.roles (tenant_id, code, name, description, is_system, scope_level)
         VALUES (fms.current_tenant_id(), $1, $2, $3, false, $4)
         RETURNING id",
    )
    .bind(code)
    .bind(name)
    .bind(body.description.as_deref())
    .bind(scope_level)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;

    sqlx::query(
        "INSERT INTO fms.role_permissions (role_id, permission_code)
         SELECT $1, c FROM unnest($2::text[]) c",
    )
    .bind(role_id)
    .bind(&permissions)
    .execute(tx.conn())
    .await
    .map_err(translate)?;

    let row: RoleDto = sqlx::query_as(
        "SELECT r.id, r.tenant_id, r.code::text AS code, r.name::text AS name,
                r.description, r.is_system, r.is_assignable, r.scope_level,
                coalesce(
                  (SELECT array_agg(rp.permission_code::text ORDER BY rp.permission_code)
                     FROM fms.role_permissions rp WHERE rp.role_id = r.id),
                  ARRAY[]::text[]) AS permissions
           FROM fms.roles r WHERE r.id = $1",
    )
    .bind(role_id)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `role:read` 或 `role:assign` —— 見模組檔頭。
async fn require_read_or_assign(tx: &mut fms_shared::TenantTx) -> Result<(), Problem> {
    if has_permission(tx, "role:read", None, None).await?
        || has_permission(tx, "role:assign", None, None).await?
    {
        Ok(())
    } else {
        Err(Problem::permission_denied(
            "missing permission: role:read 或 role:assign",
        ))
    }
}

fn required<'a>(v: &'a Option<String>, field: &str) -> Result<&'a str, Problem> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation(format!("{field} 為必填")))
}

fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        // 只可能是撞到**本租戶**既有的自訂角色 —— 撞平台角色的情況在上面就擋掉了
        // （那個索引的鍵是 `coalesce(tenant_id, 全零 uuid)`，兩者不同命名空間）。
        Some("uq_roles_code") => Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail("這個租戶已經有同樣 code 的自訂角色（不分大小寫）"),
        _ => Problem::from(err),
    }
}
