//! 身分模組的資料存取。所有查詢都以 `query_as!` / `query_scalar!` 撰寫，
//! 由 sqlx 在編譯期對照真實 schema 驗證（ADR-09 的核心理由之一）。
//!
//! `fms.users` 的 `username` / `email` 是 `citext`，sqlx 沒有內建對應型別，
//! 因此一律以 `::text` 明確轉換。

use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{Problem, TenantTx};

/// 預認證階段：由租戶代碼解析 tenant_id。
///
/// 此時尚無 RLS 情境，因此走 014 的 `SECURITY DEFINER` 函式而非直接查
/// `fms.tenants`（後者在 `FORCE ROW LEVEL SECURITY` 下必定回 0 筆）。
/// 這是整個應用層唯一一處不經 `TenantTx` 的資料庫呼叫。
pub async fn resolve_tenant_by_code(pool: &PgPool, code: &str) -> Result<Option<Uuid>, Problem> {
    let id = sqlx::query_scalar!("SELECT fms.resolve_tenant_by_code($1)", code)
        .fetch_one(pool)
        .await
        .map_err(Problem::from)?;
    Ok(id)
}

/// 寫入一筆登入事件（`fms.auth_events`）。
///
/// # 為什麼不經 `TenantTx`
///
/// 這是應用層第二處（也是最後一處）刻意不經 `TenantTx` 的資料庫呼叫，
/// 理由與 `resolve_tenant_by_code` 相同但不同源：
///
///   * **失敗時根本沒有租戶**。`tenant_code` 解析不出來時 `tenant_id` 是 NULL，
///     組不出 `TenantContext`。
///   * **必須在認證交易之外**。登入失敗會回滾（或根本沒開交易），
///     跟著回滾的稽核記錄等於沒有記錄。
///
/// 因此固定在一條**沒有租戶情境**的連線上寫入，由 024 的
/// `auth_events_preauth_append` 政策放行。那條政策只允許
/// `LOGIN_SUCCESS`／`LOGIN_FAILED`，所以這裡的 `event_type`
/// 不是自由字串而是這兩個值 —— 傳其他值會被資料庫擋掉，不會靜默寫錯。
///
/// # 為什麼失敗只記 log 而不回傳錯誤
///
/// 呼叫端是登入路徑。「稽核寫入失敗」不該把一次成功的登入變成 500，
/// 也不該把一次 401 變成 500 —— 前者拒絕了合法使用者，
/// 後者則向攻擊者洩漏了「這條路徑上發生了不一樣的事」。
/// 寫不進去是營運問題（磁碟、政策被回退），該由 log 與告警處理。
pub async fn record_login_event(
    pool: &PgPool,
    tenant_id: Option<Uuid>,
    user_id: Option<Uuid>,
    success: bool,
    failure_reason: Option<&str>,
    user_agent: Option<&str>,
) {
    let event_type = if success {
        "LOGIN_SUCCESS"
    } else {
        "LOGIN_FAILED"
    };
    let result = if success { "SUCCESS" } else { "FAILURE" };

    // ip_address 刻意留 NULL。可取得的只有 X-Forwarded-For，而在沒有
    // 「可信代理清單」的設定之前那是客戶端可任意偽造的字串 ——
    // 在安全軌裡放一個看起來權威、實際上由攻擊者填寫的位址，
    // 比留空更糟：事後調查會據此追錯對象。見 docs/security-review-open-items.md。
    let written = sqlx::query!(
        r#"
        INSERT INTO fms.auth_events
               (tenant_id, user_id, event_type, result, failure_reason, user_agent)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        tenant_id,
        user_id,
        event_type,
        result,
        failure_reason,
        user_agent
    )
    .execute(pool)
    .await;

    if let Err(e) = written {
        tracing::error!(
            error = %e,
            event_type,
            "無法寫入 auth_events —— 登入軌出現缺口"
        );
    }
}

/// 登入用的使用者資料。刻意只取驗證所需欄位，不整列撈出。
pub struct AuthUser {
    pub id: Uuid,
    pub password_hash: Option<String>,
    pub status: String,
    pub must_change_password: bool,
}

/// 依**登入識別碼**取得同租戶內的使用者：`email` 或 `username` 皆可。
/// 須在已設 context 的交易內執行，因此 RLS 已保證只會看到本租戶的列 ——
/// 查詢本身不需要寫 tenant_id 條件。
///
/// # 為什麼收兩種而不是只認 email
///
/// 前端的登入畫面收 email（使用者記得的是那個），但 `fms.users.email`
/// **可為 NULL** —— 002 的唯一索引就寫著 `WHERE email IS NOT NULL`。
/// 而沒有 email 的正是最依賴本地密碼的一群：外包（示範租戶的
/// `clean.vendor01`）、Kiosk、服務帳號。只認 email 會讓他們從此無法登入，
/// 而規格書把 LOCAL provider 的定位寫成「外包技師、Kiosk」。
///
/// 因此 `username` 這個欄位的語意擴大為「識別碼」，契約
/// （`TokenRequest.username`）不必改，既有以 username 登入的客戶端照舊。
///
/// # email 命中優先於 username，是刻意的
///
/// 兩者是不同欄位、各自只在租戶內唯一，因此「A 的 username 等於 B 的
/// email」是資料庫允許的狀態。若 username 優先，任何人只要把自己的
/// username 設成別人的 email，就能截走那個登入嘗試 —— 他拿不到 token
/// （密碼比對仍會失敗），但受害者會把真密碼打進別人的帳號上。
/// `ORDER BY ... DESC LIMIT 1` 讓這種撞名收斂到「email 的擁有者」，
/// 也順帶保證撞名時不會回傳多列（`fetch_optional` 遇多列會是 500）。
///
/// # `lower(...)`：因為 `::text` 會把 citext 的語意丟掉
///
/// `username` 與 `email` 都是 `citext`，但 `username::text = $1` 是
/// **text 比 text**，大小寫敏感 —— 原本的查詢就是這樣寫的，於是資料庫認為
/// 同一個身分（citext 的唯一索引不分大小寫）的兩種拼法，登入卻只認一種。
/// 這件事對 username 是潛在問題，對 email 是一定會發生的問題：沒有人會
/// 在意自己的信箱首字母有沒有大寫。
///
/// 不寫 `$1::citext` 是為了讓參數維持 text 綁定（sqlx 沒有 citext 的內建
/// 對應型別，見本檔檔頭）。`lower()` 用不到 `uq_users_tenant_email` 索引，
/// 但這條查詢已經被 RLS 收斂到單一租戶的 users，且每次登入只跑一次。
pub async fn find_auth_user_by_identifier(
    tx: &mut TenantTx,
    identifier: &str,
) -> Result<Option<AuthUser>, Problem> {
    let row = sqlx::query_as!(
        AuthUser,
        r#"
        SELECT id,
               password_hash,
               status,
               must_change_password
        FROM fms.users
        WHERE (lower(email::text) = lower($1) OR lower(username::text) = lower($1))
          AND deleted_at IS NULL
        ORDER BY (lower(email::text) = lower($1)) DESC
        LIMIT 1
        "#,
        identifier
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;

    Ok(row)
}

// ---------------------------------------------------------------------------
// GET /auth/me 的聚合查詢
// ---------------------------------------------------------------------------
// 刻意拆成數支小查詢而非一支巨大 JOIN：後者會產生笛卡兒積（一個使用者
// 有 N 個角色 × M 個技能 × K 個場館），在應用層還要去重，得不償失。

/// `CurrentUser.tenant`
pub struct TenantSummary {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub industry: Option<String>,
    pub feature_flags: serde_json::Value,
}

/// 讀取當前租戶。RLS 的 `tenant_self_read` 政策保證只會取到自己這一列。
pub async fn load_tenant(tx: &mut TenantTx) -> Result<TenantSummary, Problem> {
    sqlx::query_as!(
        TenantSummary,
        r#"SELECT id, code::text AS "code!", name::text AS "name!",
                  industry::text AS "industry", feature_flags AS "feature_flags!"
           FROM fms.tenants"#
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

/// `CurrentUser.user`
pub struct UserProfile {
    pub id: Uuid,
    pub employee_no: Option<String>,
    pub username: String,
    pub email: Option<String>,
    pub display_name: String,
    pub phone: Option<String>,
    pub job_title: Option<String>,
    pub user_type: String,
    pub primary_org_id: Option<Uuid>,
    pub default_facility_id: Option<Uuid>,
    pub status: String,
    pub last_login_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn load_user_profile(tx: &mut TenantTx, user_id: Uuid) -> Result<UserProfile, Problem> {
    sqlx::query_as!(
        UserProfile,
        r#"SELECT id,
                  employee_no::text AS "employee_no",
                  username::text AS "username!",
                  email::text AS "email",
                  display_name::text AS "display_name!",
                  phone::text AS "phone",
                  job_title::text AS "job_title",
                  user_type,
                  primary_org_id,
                  default_facility_id,
                  status,
                  last_login_at
           FROM fms.users
           WHERE id = $1 AND deleted_at IS NULL"#,
        user_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("user not found"))
}

/// `CurrentUser.accessible_facilities`
pub struct FacilityRef {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub org_id: Uuid,
}

/// 可存取場館。授權展開（含 ORG 範圍沿 ltree 涵蓋子樹）由
/// `fms.user_accessible_facilities()` 負責，應用層只做 JOIN 取顯示欄位。
pub async fn load_accessible_facilities(
    tx: &mut TenantTx,
    user_id: Uuid,
) -> Result<Vec<FacilityRef>, Problem> {
    sqlx::query_as!(
        FacilityRef,
        r#"SELECT f.id, f.code::text AS "code!", f.name::text AS "name!", f.org_id
           FROM fms.user_accessible_facilities($1) a
           JOIN fms.facilities f ON f.id = a.facility_id
           WHERE f.deleted_at IS NULL
           ORDER BY f.code"#,
        user_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// `CurrentUser.roles`
pub struct RoleGrant {
    pub role_code: String,
    pub scope_type: String,
    pub scope_id: Option<Uuid>,
}

pub async fn load_roles(tx: &mut TenantTx, user_id: Uuid) -> Result<Vec<RoleGrant>, Problem> {
    sqlx::query_as!(
        RoleGrant,
        r#"SELECT r.code::text AS "role_code!", ura.scope_type, ura.scope_id
           FROM fms.user_role_assignments ura
           JOIN fms.roles r ON r.id = ura.role_id
           WHERE ura.user_id = $1
             AND (ura.valid_from IS NULL OR ura.valid_from <= now())
             AND (ura.valid_until IS NULL OR ura.valid_until > now())
           ORDER BY r.code"#,
        user_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// `CurrentUser.permissions`，格式 `permission@scope_type:scope_id`。
///
/// 直接取用 `fms.v_user_effective_permissions` —— 展開邏輯留在資料庫，
/// 應用層只負責組字串（ADR-09 實作紀律 2）。
pub async fn load_permission_strings(
    tx: &mut TenantTx,
    user_id: Uuid,
) -> Result<Vec<String>, Problem> {
    let rows = sqlx::query!(
        r#"SELECT permission_code::text AS "permission_code!",
                  scope_type,
                  scope_id
           FROM fms.v_user_effective_permissions
           WHERE user_id = $1
           ORDER BY permission_code, scope_type"#,
        user_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;

    Ok(rows
        .into_iter()
        .map(|r| match (r.scope_type, r.scope_id) {
            (Some(st), Some(sid)) => format!("{}@{}:{}", r.permission_code, st, sid),
            (Some(st), None) => format!("{}@{}", r.permission_code, st),
            _ => r.permission_code,
        })
        .collect())
}

/// 記錄登入時間。失敗不影響登入結果，因此由呼叫端決定是否忽略。
pub async fn touch_last_login(tx: &mut TenantTx, user_id: Uuid) -> Result<(), Problem> {
    sqlx::query!(
        "UPDATE fms.users SET last_login_at = now() WHERE id = $1",
        user_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 在**有租戶情境的交易裡**寫一筆 `auth_events`。
///
/// 為什麼不能沿用 [`record_login_event`]：024 的 `auth_events_preauth_append`
/// 政策要求 `current_tenant_id() IS NULL`，而且只放行 `LOGIN_SUCCESS`／
/// `LOGIN_FAILED`。登出／改密碼發生在**已認證**之後，情境裡有租戶，因此走的是
/// 007 的 `tenant_isolation`（政策是 OR 的關係）—— 那條沒有 event_type 白名單。
///
/// 也因此 `event_type` 在這條路徑上**沒有任何資料庫層的把關**，呼叫端傳的常數
/// 就是唯一的定義。目前只有 [`super::handlers`] 的三處：`LOGOUT`、
/// `TOKEN_REUSE`、`PASSWORD_CHANGED`。
///
/// 與 [`record_login_event`] 相反，這裡的失敗**回傳錯誤**而不是只記 log：
/// 它與撤銷／改密碼在同一個交易裡，寫不進去就整筆回滾。理由是這條路徑的
/// 呼叫端是幂等的（再登出一次沒有壞處），因此「要嘛都成功、要嘛都沒發生」
/// 比「撤銷了但沒有軌跡」好。登入路徑沒有這個選項 —— 那裡回滾會把一次
/// 成功的登入變成 500。
pub async fn record_auth_event_tx(
    tx: &mut TenantTx,
    user_id: Uuid,
    event_type: &str,
    result: &str,
    failure_reason: Option<&str>,
    user_agent: Option<&str>,
) -> Result<(), Problem> {
    // tenant_id 由 current_tenant_id() 取，不由呼叫端傳：政策的 WITH CHECK
    // 比對的就是它，傳一個不同的值只會得到一個難讀的 RLS 錯誤。
    sqlx::query!(
        r#"
        INSERT INTO fms.auth_events
               (tenant_id, user_id, event_type, result, failure_reason, user_agent)
        VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5)
        "#,
        user_id,
        event_type,
        result,
        failure_reason,
        user_agent
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 這個 jti 是否已被撤銷；回傳撤銷原因（`LOGOUT` / `ROTATED`）。
///
/// 回傳原因而不是 bool：兩者對呼叫端是不同的事件。`LOGOUT` 是「使用者自己
/// 登出了」，`ROTATED` 是「這個 token 已經被換掉，卻又被拿來用」—— 後者是
/// token 被複製的訊號（RFC 6819 §5.2.2.3），值得留下軌跡。
pub async fn refresh_token_revocation(
    tx: &mut TenantTx,
    jti: Uuid,
) -> Result<Option<String>, Problem> {
    sqlx::query_scalar!(
        "SELECT reason FROM fms.revoked_refresh_tokens WHERE jti = $1",
        jti
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 把一個 refresh token 記進黑名單。回傳 `true` 表示這次真的新增了一列。
///
/// `ON CONFLICT DO NOTHING` 讓重複撤銷是幂等的（070 把 jti 設成主鍵正是為此）。
/// 回傳「有沒有新增」讓 logout 能誠實回報 `already_revoked` —— 兩次登出都回
/// 一樣的 200 沒有錯，但客戶端分不出「剛剛撤銷了」與「早就撤銷了」。
pub async fn revoke_refresh_token(
    tx: &mut TenantTx,
    jti: Uuid,
    user_id: Uuid,
    expires_at: chrono::DateTime<chrono::Utc>,
    reason: &str,
) -> Result<bool, Problem> {
    let inserted = sqlx::query_scalar!(
        r#"
        INSERT INTO fms.revoked_refresh_tokens
               (jti, tenant_id, user_id, expires_at, reason)
        VALUES ($1, fms.current_tenant_id(), $2, $3, $4)
        ON CONFLICT (jti) DO NOTHING
        RETURNING true AS "inserted!"
        "#,
        jti,
        user_id,
        expires_at,
        reason
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(inserted.is_some())
}

/// 改密碼所需的三個事實：現有雜湊、帳號狀態、租戶的最短長度政策。
pub struct PasswordChangeContext {
    /// `None` 代表這個帳號只能經目錄來源登入（002：`password_hash` 僅
    /// LOCAL provider 有值）。不可視為「空密碼通過」。
    pub password_hash: Option<String>,
    pub status: String,
    pub min_length: i32,
}

/// 讀改密碼的前置條件。
///
/// `min_length` 走 067 的 `fms.tenant_setting_int()`（070 把
/// `password_min_length` 加進 `tenants.settings` 的已知鍵）——
/// 密碼政策是管理者定義的條件，不寫死在 Rust 裡。預設 12 是這裡唯一的
/// 硬編碼值，而它的角色是「租戶沒設定時的回退」，不是上限也不是下限
/// （下限 8 由 070 的形狀約束守著）。
pub async fn load_password_change_context(
    tx: &mut TenantTx,
    user_id: Uuid,
) -> Result<PasswordChangeContext, Problem> {
    let row = sqlx::query!(
        r#"
        SELECT u.password_hash,
               u.status::text AS "status!",
               fms.tenant_setting_int('password_min_length', 12) AS "min_length!"
          FROM fms.users u
         WHERE u.id = $1
        "#,
        user_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("user not found"))?;

    Ok(PasswordChangeContext {
        password_hash: row.password_hash,
        status: row.status,
        min_length: row.min_length,
    })
}

/// 寫入新密碼。
///
/// `password_updated_at` 在這支之前**從來沒有寫入者**（002 建了欄位，
/// 全專案沒有一處 UPDATE 它）—— 也就是說在改密碼端點存在之前，那一欄
/// 永遠是 NULL。這裡是它的第一個寫入者。
///
/// `must_change_password` 一併清掉：它的語意是「下次登入必須改密碼」，
/// 改完了還留著會讓使用者被無限次要求改密碼。
pub async fn update_password(
    tx: &mut TenantTx,
    user_id: Uuid,
    new_hash: &str,
) -> Result<(), Problem> {
    let affected = sqlx::query!(
        r#"
        UPDATE fms.users
           SET password_hash = $2,
               password_updated_at = now(),
               must_change_password = false
         WHERE id = $1
        "#,
        user_id,
        new_hash
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?
    .rows_affected();

    if affected == 0 {
        return Err(Problem::not_found("user not found"));
    }
    Ok(())
}

// =============================================================================
// SSO 授權碼流程（073）
// =============================================================================
// 這一段刻意**不用 `query!` 巨集**，改用非巨集的 `query_as`／`query_scalar`。
//
// 理由：巨集在編譯期要連資料庫（或命中 `.sqlx` 快取），而這幾支查詢的形狀很簡單
// （沒有 JOIN、沒有需要 `!`／`?` 覆寫的可空性推論）。用非巨集的形式讓這個模組
// 在快取尚未更新時也編得過 —— 而 `sso_slice.rs` 會實際執行它們，
// 因此「欄位打錯」仍然會被抓到，只是在測試而不是編譯期。

/// `/auth/sso/*` 需要的身分來源欄位。
pub struct SsoProvider {
    pub id: Uuid,
    pub provider_type: String,
    pub status: String,
    pub issuer: Option<String>,
    pub discovery_url: Option<String>,
    pub client_id: Option<String>,
}

/// 依 `(tenant_id, code)` 取身分來源。
///
/// **不經 `TenantTx`**：`/authorize` 在使用者登入之前，沒有租戶情境。
/// `identity_providers` 有 FORCE RLS，因此這裡以 `set_config` 明確帶入租戶 ——
/// tenant_id 是剛剛由 014 的 SECURITY DEFINER 函式從 `tenant_code` 解析出來的，
/// 不是呼叫端給的 uuid。
pub async fn load_sso_provider(
    pool: &PgPool,
    tenant_id: Uuid,
    code: &str,
) -> Result<SsoProvider, Problem> {
    let mut tx = pool.begin().await.map_err(Problem::from)?;
    sqlx::query("SELECT set_config('app.tenant_id', $1::text, true)")
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(Problem::from)?;

    /// `(id, provider_type, status, issuer, discovery_url, client_id)`。
    /// 具名別名讓下面的解構讀得懂欄位順序。
    type ProviderRow = (
        Uuid,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    let row: Option<ProviderRow> = sqlx::query_as(
        "SELECT id, provider_type, status, issuer, discovery_url, client_id
               FROM fms.identity_providers
              WHERE tenant_id = $1 AND lower(code) = lower($2) AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Problem::from)?;
    tx.commit().await.map_err(Problem::from)?;

    let (id, provider_type, status, issuer, discovery_url, client_id) =
        row.ok_or_else(|| Problem::not_found("這個租戶沒有這個 code 的身分來源"))?;
    Ok(SsoProvider {
        id,
        provider_type,
        status,
        issuer,
        discovery_url,
        client_id,
    })
}

/// 寫入一筆授權請求。
///
/// 走 073 的 `sso_requests_preauth` 政策（`current_tenant_id() IS NULL`），
/// 因此**刻意不設租戶情境** —— 設了反而會落到 `tenant_isolation` 那條，
/// 而那也可以，只是兩條政策擇一即可，而 pre-auth 那條才是這條路徑的常態。
#[allow(clippy::too_many_arguments)]
pub async fn insert_sso_request(
    pool: &PgPool,
    tenant_id: Uuid,
    identity_provider_id: Uuid,
    state: &str,
    nonce: &str,
    pkce_verifier: &str,
    redirect_uri: &str,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), Problem> {
    sqlx::query(
        "INSERT INTO fms.sso_auth_requests
                (tenant_id, identity_provider_id, state, nonce, pkce_verifier,
                 redirect_uri, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(tenant_id)
    .bind(identity_provider_id)
    .bind(state)
    .bind(nonce)
    .bind(pkce_verifier)
    .bind(redirect_uri)
    .bind(expires_at)
    .execute(pool)
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// `fms.consume_sso_state` 的結果。
pub struct ConsumedState {
    /// `CONSUMED` / `NOT_FOUND` / `ALREADY_USED` / `EXPIRED`。
    ///
    /// **四種分開**：合併成一個「無效」會讓重放（可能的攻擊）與
    /// 「使用者在 IdP 上待太久」在日誌裡長得一樣。
    pub outcome: String,
    pub tenant_id: Option<Uuid>,
    pub identity_provider_id: Option<Uuid>,
    pub nonce: Option<String>,
    pub pkce_verifier: Option<String>,
    pub redirect_uri: Option<String>,
}

/// 一次性消耗 state。原子性在 073 的函式裡（條件式 UPDATE）。
pub async fn consume_sso_state(pool: &PgPool, state: &str) -> Result<ConsumedState, Problem> {
    /// `consume_sso_state` 的七個回傳欄位，順序與 073 的 `RETURNS TABLE` 一致：
    /// `(outcome, request_id, tenant_id, identity_provider_id, nonce,
    ///   pkce_verifier, redirect_uri)`。
    type ConsumeRow = (
        String,
        Option<Uuid>,
        Option<Uuid>,
        Option<Uuid>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    let row: ConsumeRow = sqlx::query_as("SELECT * FROM fms.consume_sso_state($1)")
        .bind(state)
        .fetch_one(pool)
        .await
        .map_err(Problem::from)?;

    Ok(ConsumedState {
        outcome: row.0,
        tenant_id: row.2,
        identity_provider_id: row.3,
        nonce: row.4,
        pkce_verifier: row.5,
        redirect_uri: row.6,
    })
}
