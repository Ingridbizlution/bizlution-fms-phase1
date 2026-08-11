//! 使用者的維護（`/users`）。
//!
//! # 為什麼這一塊擋住了很多東西
//!
//! 在這支端點之前，把一個人放進系統的唯一方式是寫 SQL。三個具體後果：
//!
//! 1. **端到端驗收走不完。** 「新人入職 → 指派角色 → 派工」缺中間那段，
//!    任何完整流程的 UAT 都得靠手動 SQL。
//! 2. **稽核軌跡對身分變更是空的。** 029 稽核了 `users` 與
//!    `user_role_assignments`，但**沒有任何已實作的端點會寫它們** ——
//!    那份軌跡至今沒有一列來自真實操作。
//! 3. **種子的不一致修不了。** 示範租戶的設備與工單全在台北總部，唯一的
//!    技師卻在信義影城，因此總部沒有任何場域級的執行者（010 的 T3 因此
//!    只能改用租戶管理員當執行者）。要正確修它得能建人。
//!
//! # 兩個範圍不對稱，而那是刻意的
//!
//! `user:read` 宣告 **FACILITY**、`user:write` 宣告 **TENANT**。
//! 看起來不對稱，但 026 的規則說得通：**讀一個租戶級資源不是租戶級特權，
//! 寫它才是。** 場域管理員派工時要選人，所以得看得到租戶的使用者清單；
//! 但建立與停用帳號會影響整個租戶，那是租戶級動作。
//!
//! `users` 沒有 `facility_id` 欄位，因此 RLS 只隔離租戶 —— 場域管理員看到的
//! 是**全租戶**的使用者。那正是「派工要選人」需要的，不是漏洞。
//!
//! # 這支端點刻意不碰密碼
//!
//! `POST /users` 建立的帳號是 `INVITED` 且**沒有密碼**。
//!
//! 把明文密碼放進使用者管理端點會多開一個憑證處理面，而它不需要存在：
//! 密碼的設定屬於 `POST /auth/password/change`（契約已定義）或 SSO。
//! 一個管理員替別人設定初始密碼，也意味著那個密碼曾經被第三人知道。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_permission, require_tenant_scoped_permission,
    Caller, Cursor, FieldError, PageMeta, Problem, SortSpec,
};

#[derive(Clone)]
pub struct UsersState {
    pub pool: PgPool,
}

// =============================================================================
// 契約 enum 的白名單
// =============================================================================
// 用切片而不是 Rust enum：這些值直接對應資料庫的 CHECK 約束，而讓約束在
// INSERT 時才擋下來只會得到一個 23514，訊息裡沒有合法值清單。
// 在進 SQL 之前擋掉，錯誤訊息才說得出「可以填什麼」。

const USER_TYPES: [&str; 6] = [
    "EMPLOYEE",
    "CONTRACTOR",
    "VENDOR",
    "TENANT_USER",
    "SERVICE_ACCOUNT",
    "KIOSK",
];

const COLUMNS: &str = "id, username::text AS username, email::text AS email, display_name,
                       given_name, family_name, employee_no, phone, job_title,
                       user_type, status, primary_org_id, default_facility_id,
                       locale, timezone, last_login_at, created_at, updated_at";

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserDto {
    pub id: Uuid,
    pub username: String,
    pub email: Option<String>,
    pub display_name: String,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub employee_no: Option<String>,
    pub phone: Option<String>,
    pub job_title: Option<String>,
    pub user_type: String,
    pub status: String,
    pub primary_org_id: Option<Uuid>,
    pub default_facility_id: Option<Uuid>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
    pub last_login_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// 參數比照 `openapi.yaml` 的 `listUsers` —— **契約是權威**（ADR-09 紀律 1）。
/// 我第一版只寫了 `status` 與 `q` 就實作了，是把順序做反：契約早就定義了
/// 分頁與另外四個過濾條件。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// 對 username／display_name／email 做子字串比對（不分大小寫）。
    pub q: Option<String>,
    pub org_id: Option<Uuid>,
    pub role_code: Option<String>,
    /// **「可存取這個場域的人」**，不是「預設場域是它的人」。
    /// 派工要選的是能到那個場域工作的人，而那由角色指派的範圍決定，
    /// 不是由使用者的個人偏好欄位決定。
    pub facility_id: Option<Uuid>,
    pub user_type: Option<String>,
    /// 未指定時**排除 DEPROVISIONED** —— 見 `list` 的說明。
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UserCreate {
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub employee_no: Option<String>,
    pub phone: Option<String>,
    pub job_title: Option<String>,
    pub user_type: Option<String>,
    pub primary_org_id: Option<Uuid>,
    pub default_facility_id: Option<Uuid>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
}

/// `Option<Option<T>>`：對可為 null 的欄位，「沒有提供」與「明確設為 null」
/// 是不同的意思 —— 前者不動，後者清空。單層 `Option` 兩者在型別上分不出來，
/// 於是「清掉某人的 email」就變成做不到的事。
#[derive(Debug, Deserialize)]
pub struct UserPatch {
    pub display_name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub email: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub given_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub family_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub employee_no: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub phone: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub job_title: Option<Option<String>>,
    pub user_type: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub primary_org_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "double_option")]
    pub default_facility_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "double_option")]
    pub locale: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub timezone: Option<Option<String>>,
}

fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

// =============================================================================
// 驗證
// =============================================================================

fn check_user_type(value: Option<&str>) -> Result<(), Problem> {
    match value {
        None => Ok(()),
        Some(v) if USER_TYPES.contains(&v) => Ok(()),
        Some(v) => Err(Problem::validation(format!(
            "user_type 必須是 {} 其中之一",
            USER_TYPES.join("／")
        ))
        .with_errors(vec![FieldError {
            pointer: "/user_type".to_string(),
            code: "ENUM".to_string(),
            message: format!("收到 `{v}`"),
        }])),
    }
}

fn check_required(field: &str, value: &str) -> Result<(), Problem> {
    if value.trim().is_empty() {
        return Err(
            Problem::validation(format!("{field} 不可為空白")).with_errors(vec![FieldError {
                pointer: format!("/{field}"),
                code: "REQUIRED".to_string(),
                // 空白字串與缺欄位不同：前者會通過 serde 的必填檢查，
                // 然後在資料庫裡變成一個看不見的名字。
                message: "只有空白字元不算有值".to_string(),
            }]),
        );
    }
    Ok(())
}

// =============================================================================
// Handlers
// =============================================================================

/// `GET /users`
///
/// **預設不回 `DEPROVISIONED`。** 這支端點最主要的用途是派工時選人，
/// 而把已離職的人混進候選清單，最好的情況是選錯人、最壞的情況是把工單
/// 指派給一個永遠不會看到它的帳號。要看全部就明確指定 `status`。
pub async fn list(
    State(state): State<UsersState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    if let Some(s) = q.status.as_deref() {
        if !["INVITED", "ACTIVE", "SUSPENDED", "DEPROVISIONED"].contains(&s) {
            return Err(Problem::validation(
                "status 必須是 INVITED／ACTIVE／SUSPENDED／DEPROVISIONED 其中之一",
            ));
        }
    }
    check_user_type(q.user_type.as_deref())?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // FACILITY 範圍就夠：派工要選人。`users` 沒有 facility_id，
    // 所以 RLS 只隔離租戶 —— 場域管理員看到的是全租戶的人，那是刻意的。
    require_permission(&mut tx, "user:read", q.facility_id, None).await?;

    let limit = clamp_limit(q.limit);
    // 依 display_name 排序：這份清單的用途是「選一個人」，而人是用名字找的。
    // id 當第二鍵讓同名不會讓游標跳過某一列。
    let sort = SortSpec {
        column: "display_name".to_string(),
        desc: false,
    };
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };
    let (ckey, cid) = match cursor.as_ref() {
        Some(c) => (Some(c.key.clone()), Some(c.uuid_id()?)),
        None => (None, None),
    };

    // `facility_id` 走 021 的權威函式而不是自己重寫範圍展開。
    // 那是每列一次函式呼叫 —— 在使用者數量級（每租戶數百）可以接受，
    // 而正確性比這裡的常數因子重要：範圍展開寫錯的後果是把不該出現的人
    // 列進派工候選。
    let rows: Vec<UserDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM fms.users u
          WHERE u.deleted_at IS NULL
            AND ($1::text IS NULL OR u.status = $1::text)
            AND ($1::text IS NOT NULL OR u.status <> 'DEPROVISIONED')
            AND ($2::text IS NULL
                 OR u.username::text ILIKE '%' || $2 || '%'
                 OR u.display_name ILIKE '%' || $2 || '%'
                 OR u.email::text ILIKE '%' || $2 || '%')
            AND ($3::uuid IS NULL OR u.primary_org_id = $3::uuid)
            AND ($4::text IS NULL OR u.user_type = $4::text)
            AND ($5::text IS NULL OR EXISTS (
                  SELECT 1 FROM fms.user_role_assignments ura
                    JOIN fms.roles r ON r.id = ura.role_id
                   WHERE ura.user_id = u.id AND r.code = $5::text))
            AND ($6::uuid IS NULL OR EXISTS (
                  SELECT 1 FROM fms.user_accessible_facilities(u.id) af
                   WHERE af = $6::uuid))
            AND ($7::text IS NULL OR (u.display_name, u.id) > ($7::text, $8::uuid))
          ORDER BY u.display_name, u.id
          LIMIT $9"
    ))
    .bind(q.status.as_deref())
    .bind(q.q.as_deref())
    .bind(q.org_id)
    .bind(q.user_type.as_deref())
    .bind(q.role_code.as_deref())
    .bind(q.facility_id)
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1) // 多取一列判斷還有沒有下一頁
    .fetch_all(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.display_name.clone(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
        // 未指定 status 時排除了什麼，明說。少了這一行，「為什麼找不到某人」
        // 會變成一次除錯，而答案只是他已離職。
        "meta": { "default_excludes": ["DEPROVISIONED"] }
    })))
}

/// `POST /users`
///
/// 建立的帳號是 **`INVITED` 且沒有密碼**（見模組檔頭）。
pub async fn create(
    State(state): State<UsersState>,
    caller: Caller,
    Json(body): Json<UserCreate>,
) -> Result<(StatusCode, Json<UserDto>), Problem> {
    check_required("username", &body.username)?;
    check_required("display_name", &body.display_name)?;
    check_user_type(body.user_type.as_deref())?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // TENANT 範圍：新增一個帳號影響整個租戶，而不是某一個場域。
    require_tenant_scoped_permission(&mut tx, "user:write").await?;

    let row: UserDto = sqlx::query_as(&format!(
        "INSERT INTO fms.users
           (tenant_id, username, display_name, email, given_name, family_name,
            employee_no, phone, job_title, user_type, status,
            primary_org_id, default_facility_id, locale, timezone)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7, $8,
                 coalesce($9, 'EMPLOYEE'), 'INVITED', $10, $11, $12, $13)
         RETURNING {COLUMNS}"
    ))
    .bind(body.username.trim())
    .bind(body.display_name.trim())
    .bind(body.email.as_deref().map(str::trim))
    .bind(&body.given_name)
    .bind(&body.family_name)
    .bind(&body.employee_no)
    .bind(&body.phone)
    .bind(&body.job_title)
    .bind(&body.user_type)
    .bind(body.primary_org_id)
    .bind(body.default_facility_id)
    .bind(&body.locale)
    .bind(&body.timezone)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `PATCH /users/{id}`
///
/// **不能改 `username`、`status` 與 `tenant_id`。**
///
/// `username` 是身分：改它等於把一個人的歷史（稽核、工單、角色指派）
/// 接到另一個名字上，而那些紀錄不會跟著改。要換名字就是換一個帳號。
///
/// `status` 走 `POST /users/{id}/suspend` —— 停用是一個有後果的動作，
/// 不該混在「順手改個電話」的同一個請求裡。
pub async fn patch(
    State(state): State<UsersState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<UserPatch>,
) -> Result<Json<UserDto>, Problem> {
    if let Some(name) = body.display_name.as_deref() {
        check_required("display_name", name)?;
    }
    check_user_type(body.user_type.as_deref())?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "user:write").await?;

    // `$N::bool` 決定「有沒有提供這個欄位」，值本身可以是 NULL。
    // 少了那個布林旗標，「沒提供」與「設為 null」在 SQL 裡就同一個樣子。
    let row: Option<UserDto> = sqlx::query_as(&format!(
        "UPDATE fms.users SET
           display_name        = coalesce($2, display_name),
           email               = CASE WHEN $3::bool  THEN $4::citext  ELSE email END,
           given_name          = CASE WHEN $5::bool  THEN $6          ELSE given_name END,
           family_name         = CASE WHEN $7::bool  THEN $8          ELSE family_name END,
           employee_no         = CASE WHEN $9::bool  THEN $10         ELSE employee_no END,
           phone               = CASE WHEN $11::bool THEN $12         ELSE phone END,
           job_title           = CASE WHEN $13::bool THEN $14         ELSE job_title END,
           user_type           = coalesce($15, user_type),
           primary_org_id      = CASE WHEN $16::bool THEN $17::uuid   ELSE primary_org_id END,
           default_facility_id = CASE WHEN $18::bool THEN $19::uuid   ELSE default_facility_id END,
           locale              = CASE WHEN $20::bool THEN $21         ELSE locale END,
           timezone            = CASE WHEN $22::bool THEN $23         ELSE timezone END,
           updated_at          = clock_timestamp()
         WHERE id = $1 AND deleted_at IS NULL
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(body.display_name.as_deref().map(str::trim))
    .bind(body.email.is_some())
    .bind(body.email.clone().flatten())
    .bind(body.given_name.is_some())
    .bind(body.given_name.clone().flatten())
    .bind(body.family_name.is_some())
    .bind(body.family_name.clone().flatten())
    .bind(body.employee_no.is_some())
    .bind(body.employee_no.clone().flatten())
    .bind(body.phone.is_some())
    .bind(body.phone.clone().flatten())
    .bind(body.job_title.is_some())
    .bind(body.job_title.clone().flatten())
    .bind(&body.user_type)
    .bind(body.primary_org_id.is_some())
    .bind(body.primary_org_id.flatten())
    .bind(body.default_facility_id.is_some())
    .bind(body.default_facility_id.flatten())
    .bind(body.locale.is_some())
    .bind(body.locale.clone().flatten())
    .bind(body.timezone.is_some())
    .bind(body.timezone.clone().flatten())
    .fetch_optional(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    row.map(Json)
        .ok_or_else(|| Problem::not_found("找不到這個使用者（或已被刪除）"))
}

#[derive(Debug, Deserialize)]
pub struct SuspendBody {
    /// 目標狀態。`SUSPENDED`（可復原）或 `DEPROVISIONED`（離職）。
    /// 預設 `SUSPENDED`。
    pub status: Option<String>,
    pub reason: Option<String>,
}

/// `POST /users/{id}/suspend`
///
/// **停用而不是刪除。** `users` 被工單、稽核、角色指派、預約引用；刪掉一列
/// 會讓那些紀錄指向不存在的人，而稽核軌跡的意義正是「誰做的」。
///
/// 也**不能停用自己** —— 那是一個把自己鎖在門外的操作，而且若操作者是
/// 租戶裡最後一個 TENANT_ADMIN，就沒有人能把他放回來。
pub async fn suspend(
    State(state): State<UsersState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SuspendBody>,
) -> Result<Json<UserDto>, Problem> {
    let target = body.status.as_deref().unwrap_or("SUSPENDED");
    if !["SUSPENDED", "DEPROVISIONED"].contains(&target) {
        return Err(Problem::validation(
            "status 只能是 SUSPENDED（可復原）或 DEPROVISIONED（離職）",
        ));
    }
    if id == caller.user_id {
        return Err(Problem::validation(
            "不能停用自己 —— 若你是租戶最後一個管理員，就沒有人能把你放回來",
        ));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "user:write").await?;

    let row: Option<UserDto> = sqlx::query_as(&format!(
        "UPDATE fms.users
            SET status = $2, updated_at = clock_timestamp(),
                attributes = attributes
                             || jsonb_build_object('suspend_reason', $3::text)
          WHERE id = $1 AND deleted_at IS NULL
          RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(target)
    .bind(body.reason.as_deref())
    .fetch_optional(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    row.map(Json)
        .ok_or_else(|| Problem::not_found("找不到這個使用者（或已被刪除）"))
}

// =============================================================================
// 錯誤翻譯
// =============================================================================
// 唯一約束是部分索引（`WHERE deleted_at IS NULL`），因此「跟一個已刪除的
// 帳號同名」是允許的。訊息要說得出是哪一個欄位撞到 —— 只回 409 會讓前端
// 得自己猜是 username、email 還是員工編號。
fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    let dup = |field: &str, note: &str| {
        Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail(format!("{field} 在這個租戶內已被使用（{note}）"))
    };
    match constraint.as_deref() {
        Some("uq_users_tenant_username") => dup("username", "不分大小寫；已刪除的帳號不算"),
        Some("uq_users_tenant_email") => dup("email", "不分大小寫；已刪除的帳號不算"),
        Some("uq_users_tenant_employee_no") => dup("employee_no", "已刪除的帳號不算"),
        Some("users_user_type_check") => Problem::validation(format!(
            "user_type 必須是 {} 其中之一",
            USER_TYPES.join("／")
        )),
        _ => Problem::from(err),
    }
}
