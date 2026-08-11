//! SCIM 2.0 供裝端點（RFC 7643 資源結構、RFC 7644 協定）。
//!
//! 這是整個 API 裡**唯一不遵守本專案自己的慣例**的一組端點，而每一項偏離
//! 都是被規範逼出來的：
//!
//! | 本專案的慣例 | SCIM 這裡 | 為什麼 |
//! |---|---|---|
//! | 錯誤是 RFC 7807 `application/problem+json` | RFC 7644 §3.12 的 Error 結構 | Entra ID 解析的是後者。回 problem+json 它只會看到「未知錯誤」 |
//! | `{"data": …, "meta": …}` 封裝 | `ListResponse` / 裸資源 | 供裝端是機器，它照 schema 解析 |
//! | `snake_case` 欄位 | `camelCase` | RFC 7643 的屬性名 |
//! | JWT + `X-Tenant-ID` | bearer token（074 的 `scim_tokens`） | Entra 只能設一個靜態 token |
//! | 游標分頁 | `startIndex` / `count`（1-based） | RFC 7644 §3.4.2.4 |
//!
//! 混用會讓端點兩邊都不對：既不是 SCIM，也不是這個 API。
//!
//! # 租戶從哪裡來
//!
//! **token 本身就是租戶的判別依據。** 沒有 `X-Tenant-ID`（Entra 送不出來），
//! 也沒有 JWT。`require_scim_token` 拿 bearer token 的 SHA-256 去問
//! `fms.authenticate_scim_token()`，換回 `(identity_provider_id, tenant_id)`。
//!
//! # 讀取範圍限定在發出請求的那個身分來源
//!
//! `GET /Users` 只回**有這個 provider 的 `user_identities` 列**的使用者，
//! 不是整個租戶的使用者。
//!
//! 反過來做（回整個租戶）的後果是很具體的：Entra 在建立前會先用
//! `userName eq "..."` 查一次，如果我們把一個本地建立的帳號回給它，
//! 它會認為那是自己管的，接著就能改那個帳號的密碼登入方式與狀態 ——
//! 包含租戶管理員的帳號。**外部目錄不該能接管它沒有佈建的帳號。**
//!
//! 代價是誠實的：使用者名稱在租戶內已被本地帳號佔用時，`POST /Users` 回
//! 409 `uniqueness`，而 detail 會說出「這個名稱屬於一個不由此來源管理的帳號」。
//! 那是一個需要人介入的狀態，而它會被說出來，不會被猜。
//!
//! # 沒有實作的部分
//!
//! * **`PUT`（整體替換）** —— 契約只列了 GET/POST/PATCH/DELETE。Entra 預設
//!   用 PATCH，PUT 只在「Make this the authoritative source」模式下才送。
//! * **`/ServiceProviderConfig`、`/Schemas`、`/ResourceTypes`** —— 探索端點。
//!   Entra 不讀它們（它假設一組固定的能力），因此實作它們只是多三份要維護
//!   的靜態 JSON。
//! * **完整的 filter 文法**（`and`／`or`／`co`／`sw`／`pr`／括號）——
//!   只支援 `attr eq "value"` 與 `members[value eq "id"]`。這**不是**偷懶的
//!   子集：那兩種就是 Entra 實際會送的全部。其他文法回 400 `invalidFilter`
//!   並列出支援的形式，而不是靜默忽略條件 —— 靜默忽略 filter 會讓
//!   「查一個使用者」變成「回傳全部使用者」，那是最糟的失敗方式。
//! * **`sortBy`／`sortOrder`、`Bulk`、ETag** —— 同樣不在 Entra 的路徑上。
//! * **群組的巢狀成員**（群組屬於群組）—— `user_directory_groups` 只接受
//!   使用者。送巢狀成員進來會得到 400 而不是被靜默丟掉。

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{request::Parts, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, RequestPartsExt};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, ActorType, TenantContext};

const SCHEMA_USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const SCHEMA_GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const SCHEMA_LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const SCHEMA_ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const SCHEMA_PATCH: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";

/// `count` 的上限。RFC 7644 允許伺服器自行決定並在回應裡告知實際值
/// （`itemsPerPage`），因此夾住不是違規 —— 不夾住才是問題。
const MAX_COUNT: i64 = 200;

#[derive(Clone)]
pub struct ScimState {
    pub pool: PgPool,
}

// =============================================================================
// 錯誤：RFC 7644 §3.12
// =============================================================================

/// SCIM 的錯誤回應。**刻意不是 `Problem`。**
///
/// `scim_type` 是規範定義的一組固定字串（`invalidFilter`、`uniqueness`、
/// `mutability`、`invalidPath`、`invalidValue`、`invalidSyntax`、`noTarget`）。
/// Entra 的同步報告會顯示它，因此它決定了管理者在畫面上看到什麼 ——
/// 給錯或不給，錯誤就變成「未知失敗」。
#[derive(Debug)]
pub struct ScimError {
    status: StatusCode,
    scim_type: Option<&'static str>,
    detail: String,
}

impl ScimError {
    fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            scim_type: None,
            detail: detail.into(),
        }
    }

    fn typed(status: StatusCode, scim_type: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            scim_type: Some(scim_type),
            detail: detail.into(),
        }
    }

    fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, detail)
    }

    fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail)
    }

    fn bad_request(scim_type: &'static str, detail: impl Into<String>) -> Self {
        Self::typed(StatusCode::BAD_REQUEST, scim_type, detail)
    }

    /// 內部錯誤。**detail 不含資料庫的原始訊息** —— 那可能洩漏 schema，
    /// 而供裝端拿到它也做不了任何事。原因寫進 tracing。
    fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!(error = %err, "SCIM 端點內部錯誤");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "內部錯誤，詳情見伺服器日誌",
        )
    }
}

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "schemas": [SCHEMA_ERROR],
            // 規範要求 status 是**字串**，不是數字。送數字會讓嚴格的
            // 客戶端解析失敗，而那個失敗看起來像網路問題。
            "status": self.status.as_u16().to_string(),
            "detail": self.detail,
        });
        if let Some(t) = self.scim_type {
            body["scimType"] = json!(t);
        }
        scim_response(self.status, body)
    }
}

/// 帶 `application/scim+json` 的回應。
///
/// 規範要求這個 media type。`application/json` 多數客戶端能吞，
/// 但那是靠對方寬鬆，不是靠我們正確。
fn scim_response(status: StatusCode, body: Value) -> Response {
    let mut res = (status, Json(body)).into_response();
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/scim+json"),
    );
    res
}

impl From<sqlx::Error> for ScimError {
    fn from(e: sqlx::Error) -> Self {
        // 唯一性衝突要翻成 SCIM 的 409 `uniqueness`：那是供裝端唯一能理解
        // 並據以行動的錯誤（它會去查既有資源）。翻成 500 會讓它一直重試。
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return ScimError::typed(
                    StatusCode::CONFLICT,
                    "uniqueness",
                    format!(
                        "資源已存在：{}",
                        db.constraint().unwrap_or("違反了一個唯一約束")
                    ),
                );
            }
        }
        ScimError::internal(e)
    }
}

impl From<fms_shared::Problem> for ScimError {
    /// 內部 helper（`begin_tenant_tx`）回的是 `Problem`。轉成 SCIM 的形狀，
    /// 而不是讓一個 problem+json 從 SCIM 端點漏出去。
    fn from(p: fms_shared::Problem) -> Self {
        ScimError::internal(format!("{p:?}"))
    }
}

// =============================================================================
// 認證
// =============================================================================

/// 已認證的 SCIM 呼叫端。`user_id` 不存在 —— 供裝請求沒有人類發動者。
#[derive(Debug, Clone, Copy)]
pub struct ScimCaller {
    pub tenant_id: Uuid,
    pub identity_provider_id: Uuid,
}

impl From<ScimCaller> for TenantContext {
    fn from(c: ScimCaller) -> Self {
        // `Uuid::nil()` 作為 actor：SCIM 沒有發動它的使用者。
        // 稽核軌靠 `actor_type = DIRECTORY_SYNC` 表達這件事 ——
        // 那一列會是 actor_user_id = 全零 + DIRECTORY_SYNC，
        // 讀起來就是「一次目錄推送，沒有人類」。
        //
        // （沿用 handlers.rs 在租戶已知但使用者未定時的同一個佔位慣例。）
        TenantContext::background(c.tenant_id, Uuid::nil(), ActorType::DirectorySync)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ScimCaller {
    type Rejection = ScimError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<ScimCaller>()
            .copied()
            .ok_or_else(|| ScimError::unauthorized("SCIM token 未通過認證"))
    }
}

/// SHA-256 的十六進位。
///
/// 明文只存在於這個函式的輸入。傳給資料庫的是雜湊 —— 因此 token 不會
/// 出現在 Postgres 的語句日誌或 `pg_stat_activity` 裡（見 074 檔頭）。
fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// SCIM 端點的認證 middleware。
///
/// 失敗一律是 401，**不區分**「沒有標頭」「格式不對」「token 無效」
/// 「provider 停用」。區分它們會讓這支端點變成一個可探測的預言機，
/// 而供裝端對這四種情況的處置完全相同。
pub async fn require_scim_token(
    State(state): State<ScimState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, ScimError> {
    let (mut parts, body) = req.into_parts();

    let token = parts
        .extract::<axum::http::HeaderMap>()
        .await
        .ok()
        .and_then(|h| h.get(AUTHORIZATION).cloned())
        .and_then(|v| v.to_str().ok().map(str::to_owned))
        .and_then(|v| v.strip_prefix("Bearer ").map(str::to_owned))
        .ok_or_else(|| ScimError::unauthorized("需要 `Authorization: Bearer <token>`"))?;

    // 認證走 SECURITY DEFINER 的函式，因此**不需要**也不可能先有租戶情境 ——
    // token 就是租戶的判別依據。用 pool 直接查（無交易、無 set_context）。
    let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT identity_provider_id, tenant_id, scim_token_id
           FROM fms.authenticate_scim_token($1)",
    )
    .bind(token_hash(&token))
    .fetch_optional(&state.pool)
    .await?;

    let (identity_provider_id, tenant_id, _token_id) =
        row.ok_or_else(|| ScimError::unauthorized("SCIM token 無效、已撤銷，或該來源未啟用供裝"))?;

    parts.extensions.insert(ScimCaller {
        tenant_id,
        identity_provider_id,
    });

    req = Request::from_parts(parts, body);
    Ok(next.run(req).await)
}

// =============================================================================
// filter：`attr eq "value"`
// =============================================================================

/// 解析出來的等值條件。
#[derive(Debug)]
struct EqFilter {
    attr: String,
    value: String,
}

/// 只認 `attr eq "value"`。
///
/// 回 `Err` 而不是忽略：一個無法解析的 filter 若被忽略，`GET /Users?filter=…`
/// 會回傳**整個租戶**而供裝端會以為那就是符合條件的結果。
fn parse_eq_filter(raw: &str, allowed: &[&str]) -> Result<EqFilter, ScimError> {
    let s = raw.trim();
    // 大小寫不敏感地找 ` eq `：屬性名與運算子的大小寫在 RFC 7644 §3.4.2.2
    // 都是不敏感的，而 Entra 送的是小寫 `eq`。
    let lower = s.to_ascii_lowercase();
    let Some(pos) = lower.find(" eq ") else {
        return Err(ScimError::bad_request(
            "invalidFilter",
            format!(
                "只支援 `attr eq \"value\"`（可用屬性：{}）。收到：{s}",
                allowed.join("、")
            ),
        ));
    };
    let attr = s[..pos].trim().to_string();
    let value = s[pos + 4..].trim();

    // 值必須被雙引號包住。少了引號通常代表對方在組 filter 時有 bug，
    // 而把它當成裸字串接受會讓那個 bug 靜默地成為我們的行為。
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or_else(|| {
            ScimError::bad_request("invalidFilter", format!("filter 的值必須以雙引號包住：{s}"))
        })?;

    // **剝掉外層引號之後，裡面不能再有引號。**
    //
    // 少了這一行，`userName eq "a" and active eq "true"` 會被解析成
    // value = `a" and active eq "true` 而**成功通過** —— 因為它確實以引號
    // 開頭與結尾，而屬性名也在允許清單裡。結果是「查 a」變成「查一個
    // 不存在的名字」，回 0 筆，於是供裝端建立一個重複的帳號。
    //
    // 這個缺陷是 `unsupported_filter_syntax_is_an_error_not_a_full_scan`
    // 抓到的：我原本以為引號檢查就足以擋下複合條件，並不是。
    //
    // 代價：值裡含轉義引號（`\"`）的 filter 會被拒絕。SCIM 允許那種寫法，
    // 但使用者名稱與群組名不含引號，因此不支援它比錯誤解析安全。
    if value.contains('"') {
        return Err(ScimError::bad_request(
            "invalidFilter",
            format!(
                "值裡不能有引號 —— 只支援單一的 `attr eq \"value\"`，\
                 不支援 and／or 複合條件或轉義引號。收到：{s}"
            ),
        ));
    }

    // 只認允許清單裡的屬性 —— 不在清單裡的屬性沒有對應的資料庫欄位，
    // 而回「空結果」會讓供裝端以為那個使用者不存在，然後建立一個重複的。
    if !allowed
        .iter()
        .any(|a| a.eq_ignore_ascii_case(attr.as_str()))
    {
        return Err(ScimError::bad_request(
            "invalidFilter",
            format!("不支援用 `{attr}` 過濾（可用屬性：{}）", allowed.join("、")),
        ));
    }

    Ok(EqFilter {
        attr: attr.to_ascii_lowercase(),
        value: value.to_string(),
    })
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub filter: Option<String>,
    /// **1-based**（RFC 7644 §3.4.2.4）。0 或負數依規範視為 1。
    #[serde(rename = "startIndex")]
    pub start_index: Option<i64>,
    pub count: Option<i64>,
}

impl ListQuery {
    /// 回 `(limit, offset)`。
    fn window(&self) -> (i64, i64) {
        let count = self.count.unwrap_or(MAX_COUNT).clamp(0, MAX_COUNT);
        // startIndex 是 1-based，因此 offset = startIndex - 1。
        // 規範明說「小於 1 的值視為 1」—— 不是錯誤。
        let start = self.start_index.unwrap_or(1).max(1);
        (count, start - 1)
    }
}

fn list_response(total: i64, start_index: i64, resources: Vec<Value>) -> Response {
    scim_response(
        StatusCode::OK,
        json!({
            "schemas": [SCHEMA_LIST],
            "totalResults": total,
            "itemsPerPage": resources.len(),
            "startIndex": start_index.max(1),
            // 鍵名是大寫 R —— 規範如此，而小寫會讓客戶端看到零筆結果。
            "Resources": resources,
        }),
    )
}

// =============================================================================
// Users
// =============================================================================

#[derive(Debug, sqlx::FromRow)]
struct UserRow {
    id: Uuid,
    username: String,
    email: Option<String>,
    display_name: String,
    given_name: Option<String>,
    family_name: Option<String>,
    phone: Option<String>,
    job_title: Option<String>,
    status: String,
    external_subject: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

/// 讀取用的欄位清單與 JOIN。
///
/// `JOIN user_identities`（不是 LEFT JOIN）就是「範圍限定在這個 provider」
/// 那條規則的實作點 —— 見模組檔頭。
const USER_SELECT: &str = "SELECT u.id, u.username::text AS username, u.email::text AS email,
              u.display_name::text AS display_name, u.given_name::text AS given_name,
              u.family_name::text AS family_name, u.phone::text AS phone,
              u.job_title::text AS job_title, u.status,
              ui.external_subject, u.created_at, u.updated_at
         FROM fms.users u
         JOIN fms.user_identities ui
           ON ui.user_id = u.id AND ui.identity_provider_id = $1
        WHERE u.deleted_at IS NULL
          AND u.status <> 'DEPROVISIONED'";

fn render_user(u: &UserRow) -> Value {
    let mut v = json!({
        "schemas": [SCHEMA_USER],
        "id": u.id,
        "externalId": u.external_subject,
        "userName": u.username,
        "displayName": u.display_name,
        // SCIM 的 active 是布林；本系統有四個狀態。對應規則：
        // ACTIVE → true，其餘（INVITED／SUSPENDED）→ false。
        // DEPROVISIONED 讀不到（USER_SELECT 排除它，見 delete 的說明）。
        "active": u.status == "ACTIVE",
        "meta": {
            "resourceType": "User",
            "created": u.created_at,
            "lastModified": u.updated_at,
            "location": format!("/scim/v2/Users/{}", u.id),
        },
    });
    if u.given_name.is_some() || u.family_name.is_some() {
        v["name"] = json!({ "givenName": u.given_name, "familyName": u.family_name });
    }
    if let Some(e) = &u.email {
        v["emails"] = json!([{ "value": e, "type": "work", "primary": true }]);
    }
    if let Some(p) = &u.phone {
        v["phoneNumbers"] = json!([{ "value": p, "type": "work" }]);
    }
    if let Some(t) = &u.job_title {
        v["title"] = json!(t);
    }
    v
}

/// `GET /scim/v2/Users`
pub async fn list_users(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Query(q): Query<ListQuery>,
) -> Result<Response, ScimError> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let (limit, offset) = q.window();

    // filter 只會是 userName 或 externalId 兩者之一，因此不必組動態 SQL：
    // 兩個具名參數各自帶 NULL 或值，讓查詢計畫固定。
    let f = q
        .filter
        .as_deref()
        .map(|raw| parse_eq_filter(raw, &["userName", "externalId"]))
        .transpose()?;
    let (by_username, by_external) = match &f {
        Some(f) if f.attr == "username" => (Some(f.value.as_str()), None),
        Some(f) => (None, Some(f.value.as_str())),
        None => (None, None),
    };

    let where_extra = " AND ($2::text IS NULL OR u.username = $2::citext)
                        AND ($3::text IS NULL OR ui.external_subject = $3)";

    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM ({USER_SELECT}{where_extra}) t"
    ))
    .bind(caller.identity_provider_id)
    .bind(by_username)
    .bind(by_external)
    .fetch_one(tx.conn())
    .await?;

    // 排序鍵加上 id：username 唯一，但顯式的第二鍵讓分頁在任何情況下都穩定。
    let rows: Vec<UserRow> = sqlx::query_as(&format!(
        "{USER_SELECT}{where_extra} ORDER BY u.username, u.id LIMIT $4 OFFSET $5"
    ))
    .bind(caller.identity_provider_id)
    .bind(by_username)
    .bind(by_external)
    .bind(limit)
    .bind(offset)
    .fetch_all(tx.conn())
    .await?;

    tx.commit().await?;
    Ok(list_response(
        total,
        offset + 1,
        rows.iter().map(render_user).collect(),
    ))
}

/// `GET /scim/v2/Users/{id}`
pub async fn get_user(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Path(id): Path<Uuid>,
) -> Result<Response, ScimError> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let row: Option<UserRow> = sqlx::query_as(&format!("{USER_SELECT} AND u.id = $2"))
        .bind(caller.identity_provider_id)
        .bind(id)
        .fetch_optional(tx.conn())
        .await?;
    tx.commit().await?;

    let u = row.ok_or_else(|| ScimError::not_found(format!("找不到使用者 {id}")))?;
    Ok(scim_response(StatusCode::OK, render_user(&u)))
}

#[derive(Debug, Deserialize)]
pub struct ScimName {
    #[serde(rename = "givenName")]
    pub given_name: Option<String>,
    #[serde(rename = "familyName")]
    pub family_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ScimValue {
    pub value: Option<String>,
    pub primary: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UserCreate {
    #[serde(rename = "userName")]
    pub user_name: Option<String>,
    #[serde(rename = "externalId")]
    pub external_id: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    pub name: Option<ScimName>,
    pub emails: Option<Vec<ScimValue>>,
    #[serde(rename = "phoneNumbers")]
    pub phone_numbers: Option<Vec<ScimValue>>,
    pub title: Option<String>,
    pub active: Option<bool>,
}

/// 從 SCIM 的多值屬性挑一個值：優先 `primary: true`，否則第一個非空的。
fn pick(values: &Option<Vec<ScimValue>>) -> Option<String> {
    let list = values.as_ref()?;
    list.iter()
        .find(|v| v.primary == Some(true) && v.value.is_some())
        .or_else(|| list.iter().find(|v| v.value.is_some()))
        .and_then(|v| v.value.clone())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `POST /scim/v2/Users`
pub async fn create_user(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Json(body): Json<UserCreate>,
) -> Result<Response, ScimError> {
    let user_name = body
        .user_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ScimError::bad_request("invalidValue", "`userName` 是必填的（RFC 7643 §4.1.1）")
        })?
        .to_string();

    // display_name 在 002 是 NOT NULL，而 SCIM 的 displayName 是選填的。
    // 退化順序：displayName → 「givenName familyName」→ userName。
    // 最後那一步不好看，但它是**真的能識別這個人**的值，
    // 而讓資料庫的 NOT NULL 擋下請求只會讓供裝失敗且訊息難懂。
    let display_name = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let n = body.name.as_ref()?;
            let joined = [n.given_name.as_deref(), n.family_name.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            Some(joined).filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| user_name.clone());

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // 先看這個使用者名稱是不是已經被一個**不屬於此來源**的帳號佔用。
    //
    // 不靠唯一索引的 23505 是因為那個錯誤說不出「屬於誰」，而這正是管理者
    // 需要知道的事：本地帳號、還是另一個身分來源的帳號。
    let clash: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT u.id,
                EXISTS (SELECT 1 FROM fms.user_identities ui
                         WHERE ui.user_id = u.id
                           AND ui.identity_provider_id = $2) AS ours
           FROM fms.users u
          WHERE u.username = $1::citext AND u.deleted_at IS NULL",
    )
    .bind(&user_name)
    .bind(caller.identity_provider_id)
    .fetch_optional(tx.conn())
    .await?;

    if let Some((existing_id, ours)) = clash {
        return Err(ScimError::typed(
            StatusCode::CONFLICT,
            "uniqueness",
            if ours {
                format!("userName `{user_name}` 已由此來源佈建（id {existing_id}）")
            } else {
                format!(
                    "userName `{user_name}` 屬於一個不由此來源管理的帳號 —— \
                     不會自動接管。請在 FMS 這一側改名，或手動把該帳號連結到此來源"
                )
            },
        ));
    }

    let status = if body.active.unwrap_or(true) {
        "ACTIVE"
    } else {
        // 停用送進來的是 SUSPENDED（可復原），不是 DEPROVISIONED（離職）。
        // 兩者在 002 是不同狀態，而 SCIM 的 active=false 對應前者 ——
        // Entra 用它表達「暫時停用」，用 DELETE 表達「移出範圍」。
        "SUSPENDED"
    };

    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.users
           (tenant_id, username, email, display_name, given_name, family_name,
            phone, job_title, status)
         VALUES (fms.current_tenant_id(), $1::citext, $2::citext, $3, $4, $5, $6, $7, $8)
         RETURNING id",
    )
    .bind(&user_name)
    .bind(pick(&body.emails))
    .bind(&display_name)
    .bind(body.name.as_ref().and_then(|n| n.given_name.clone()))
    .bind(body.name.as_ref().and_then(|n| n.family_name.clone()))
    .bind(pick(&body.phone_numbers))
    .bind(body.title.clone())
    .bind(status)
    .fetch_one(tx.conn())
    .await?;

    // externalId 在 SCIM 是選填的，而 `user_identities.external_subject` 是
    // NOT NULL 且與 provider 組成唯一鍵。沒送就用我們自己的 id ——
    // 那保證唯一，且供裝端之後仍能用 SCIM 的 `id` 定位這個資源。
    let external_subject = body
        .external_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| user_id.to_string());

    sqlx::query(
        "INSERT INTO fms.user_identities
           (tenant_id, user_id, identity_provider_id, external_subject, is_primary)
         VALUES (fms.current_tenant_id(), $1, $2, $3, true)",
    )
    .bind(user_id)
    .bind(caller.identity_provider_id)
    .bind(&external_subject)
    .execute(tx.conn())
    .await?;

    // 重讀一次而不是用手上的欄位組回應：`created_at`／`updated_at` 由資料庫
    // 產生，而 `meta` 裡它們是供裝端用來判斷「我剛寫的生效了嗎」的依據。
    let row: UserRow = sqlx::query_as(&format!("{USER_SELECT} AND u.id = $2"))
        .bind(caller.identity_provider_id)
        .bind(user_id)
        .fetch_one(tx.conn())
        .await?;

    tx.commit().await?;
    Ok(scim_response(StatusCode::CREATED, render_user(&row)))
}

// -----------------------------------------------------------------------------
// PATCH
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PatchOp {
    pub op: String,
    pub path: Option<String>,
    pub value: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct PatchRequest {
    pub schemas: Option<Vec<String>>,
    /// 鍵名是大寫 O —— RFC 7644 §3.5.2 如此。
    #[serde(rename = "Operations")]
    pub operations: Option<Vec<PatchOp>>,
}

impl PatchRequest {
    fn ops(&self) -> Result<&[PatchOp], ScimError> {
        if let Some(s) = &self.schemas {
            if !s.iter().any(|x| x == SCHEMA_PATCH) {
                return Err(ScimError::bad_request(
                    "invalidSyntax",
                    format!("PATCH 的 schemas 必須包含 {SCHEMA_PATCH}"),
                ));
            }
        }
        let ops = self
            .operations
            .as_deref()
            .filter(|o| !o.is_empty())
            .ok_or_else(|| {
                ScimError::bad_request("invalidSyntax", "PATCH 需要非空的 `Operations`")
            })?;
        Ok(ops)
    }
}

/// SCIM 的 `active` 可能是布林，**也可能是字串 `"True"`／`"False"`**。
///
/// 後者不合規範，但 Entra ID 實際會送 —— 而拒絕它的後果是每一次停用都失敗。
/// 這是一個對真實世界的讓步，寫在這裡以免下一個人以為是筆誤。
fn as_bool(v: Option<&Value>) -> Result<bool, ScimError> {
    match v {
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(true),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Ok(false),
        other => Err(ScimError::bad_request(
            "invalidValue",
            format!("`active` 需要布林值，收到 {other:?}"),
        )),
    }
}

/// 把一個 op 攤平成 `(路徑, 值)` 的清單。
///
/// Entra 兩種寫法都會送：
///
///   * `{"op":"replace","path":"active","value":false}`
///   * `{"op":"replace","value":{"active":false,"displayName":"新名字"}}`
///
/// 第二種（沒有 path，value 是物件）在規範裡是合法的，而只實作第一種
/// 會讓某些同步靜默地什麼都不改。
fn flatten_op(op: &PatchOp) -> Result<Vec<(String, Value)>, ScimError> {
    match (&op.path, &op.value) {
        (Some(p), Some(v)) => Ok(vec![(p.to_ascii_lowercase(), v.clone())]),
        (None, Some(Value::Object(map))) => Ok(map
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect()),
        _ => Err(ScimError::bad_request(
            "invalidSyntax",
            "每個 operation 需要 `value`，以及 `path` 或一個物件形式的 value",
        )),
    }
}

/// `PATCH /scim/v2/Users/{id}`
pub async fn patch_user(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Path(id): Path<Uuid>,
    Json(body): Json<PatchRequest>,
) -> Result<Response, ScimError> {
    let ops = body.ops()?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // 先確認這個使用者屬於這個 provider。少了這一步，一個租戶內的 SCIM token
    // 就能改到同租戶其他來源（或本地）建立的帳號 —— 那是模組檔頭那條規則
    // 在寫入路徑上的同一個要求。
    let exists: Option<Uuid> =
        sqlx::query_scalar(&format!("SELECT u.id FROM ({USER_SELECT} AND u.id = $2) u"))
            .bind(caller.identity_provider_id)
            .bind(id)
            .fetch_optional(tx.conn())
            .await?;
    if exists.is_none() {
        return Err(ScimError::not_found(format!("找不到使用者 {id}")));
    }

    let mut set_status: Option<&str> = None;
    let mut set_display: Option<String> = None;
    let mut set_given: Option<String> = None;
    let mut set_family: Option<String> = None;
    let mut set_title: Option<String> = None;

    for op in ops {
        let verb = op.op.to_ascii_lowercase();
        // `add` 與 `replace` 在單值屬性上是同一件事（RFC 7644 §3.5.2.1）。
        // `remove` 需要 path 且沒有 value，這裡的屬性都是單值的，
        // 因此 remove 等於清空 —— 但 `active` 不能被清空（它是布林），
        // 所以 remove 只允許在可為 NULL 的屬性上。
        if !matches!(verb.as_str(), "add" | "replace" | "remove") {
            return Err(ScimError::bad_request(
                "invalidSyntax",
                format!("不支援的 op `{}`（只有 add／replace／remove）", op.op),
            ));
        }
        if verb == "remove" {
            let path = op.path.as_deref().unwrap_or_default().to_ascii_lowercase();
            match path.as_str() {
                "displayname" | "name.givenname" | "name.familyname" | "title" => {
                    // 清空。用空字串當標記，寫入時轉回 NULL。
                    match path.as_str() {
                        "displayname" => {
                            return Err(ScimError::bad_request(
                                "mutability",
                                "displayName 不能被移除 —— 它在 FMS 是必填的",
                            ))
                        }
                        "name.givenname" => set_given = Some(String::new()),
                        "name.familyname" => set_family = Some(String::new()),
                        _ => set_title = Some(String::new()),
                    }
                    continue;
                }
                other => {
                    return Err(ScimError::bad_request(
                        "invalidPath",
                        format!("不支援移除 `{other}`"),
                    ))
                }
            }
        }

        for (path, value) in flatten_op(op)? {
            match path.as_str() {
                "active" => {
                    set_status = Some(if as_bool(Some(&value))? {
                        "ACTIVE"
                    } else {
                        "SUSPENDED"
                    })
                }
                "displayname" => {
                    set_display = value.as_str().map(str::to_owned).filter(|s| !s.is_empty())
                }
                "name.givenname" => set_given = value.as_str().map(str::to_owned),
                "name.familyname" => set_family = value.as_str().map(str::to_owned),
                "title" => set_title = value.as_str().map(str::to_owned),
                "name" => {
                    // 整個 name 物件被替換。
                    if let Some(o) = value.as_object() {
                        set_given = o
                            .get("givenName")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                        set_family = o
                            .get("familyName")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                    }
                }
                // **不支援的路徑一律報錯，不靜默略過。** 靜默略過會讓
                // Entra 的同步報告顯示「成功」，而那個屬性從來沒有被寫入。
                other => {
                    return Err(ScimError::bad_request(
                        "invalidPath",
                        format!(
                            "不支援修改 `{other}`（可改：active、displayName、\
                             name.givenName、name.familyName、title）"
                        ),
                    ))
                }
            }
        }
    }

    // 空字串 → NULL（remove 的標記）。
    let nullify = |s: Option<String>| -> (bool, Option<String>) {
        match s {
            None => (false, None),
            Some(v) if v.is_empty() => (true, None),
            Some(v) => (true, Some(v)),
        }
    };
    let (given_set, given) = nullify(set_given);
    let (family_set, family) = nullify(set_family);
    let (title_set, title) = nullify(set_title);

    sqlx::query(
        "UPDATE fms.users
            SET status       = coalesce($2, status),
                display_name = coalesce($3, display_name),
                given_name   = CASE WHEN $4 THEN $5 ELSE given_name END,
                family_name  = CASE WHEN $6 THEN $7 ELSE family_name END,
                job_title    = CASE WHEN $8 THEN $9 ELSE job_title END,
                updated_at   = clock_timestamp()
          WHERE id = $1",
    )
    .bind(id)
    .bind(set_status)
    .bind(set_display)
    .bind(given_set)
    .bind(given)
    .bind(family_set)
    .bind(family)
    .bind(title_set)
    .bind(title)
    .execute(tx.conn())
    .await?;

    let row: UserRow = sqlx::query_as(&format!("{USER_SELECT} AND u.id = $2"))
        .bind(caller.identity_provider_id)
        .bind(id)
        .fetch_one(tx.conn())
        .await?;

    tx.commit().await?;
    Ok(scim_response(StatusCode::OK, render_user(&row)))
}

/// `DELETE /scim/v2/Users/{id}`
///
/// **不是實體刪除，也不設 `deleted_at`。** 狀態改成 `DEPROVISIONED` ——
/// 與 `POST /users/{id}:suspend` 對「離職」的既有詞彙一致。
///
/// 那一列留著是因為它被工單、稽核軌、勞務紀錄引用：真的刪掉會讓
/// 「這張單是誰做的」變成 NULL。而 SCIM 那一側看到的是刪除：
/// `USER_SELECT` 排除 `DEPROVISIONED`，因此後續的 GET 是 404
/// （RFC 7644 §3.6 要求的行為）。
pub async fn delete_user(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Path(id): Path<Uuid>,
) -> Result<Response, ScimError> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let found: Option<Uuid> =
        sqlx::query_scalar(&format!("SELECT u.id FROM ({USER_SELECT} AND u.id = $2) u"))
            .bind(caller.identity_provider_id)
            .bind(id)
            .fetch_optional(tx.conn())
            .await?;
    if found.is_none() {
        return Err(ScimError::not_found(format!("找不到使用者 {id}")));
    }

    sqlx::query(
        "UPDATE fms.users
            SET status = 'DEPROVISIONED', updated_at = clock_timestamp()
          WHERE id = $1",
    )
    .bind(id)
    .execute(tx.conn())
    .await?;

    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// =============================================================================
// Groups
// =============================================================================

#[derive(Debug, sqlx::FromRow)]
struct GroupRow {
    id: Uuid,
    name: String,
    external_group_id: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

const GROUP_SELECT: &str = "SELECT g.id, g.name::text AS name, g.external_group_id, g.created_at
         FROM fms.directory_groups g
        WHERE g.identity_provider_id = $1";

async fn group_members(
    tx: &mut fms_shared::TenantTx,
    group_id: Uuid,
) -> Result<Vec<Value>, ScimError> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT u.id, u.display_name::text
           FROM fms.user_directory_groups m
           JOIN fms.users u ON u.id = m.user_id AND u.deleted_at IS NULL
          WHERE m.directory_group_id = $1
          ORDER BY u.display_name, u.id",
    )
    .bind(group_id)
    .fetch_all(tx.conn())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| json!({ "value": id, "display": name, "type": "User" }))
        .collect())
}

fn render_group(g: &GroupRow, members: Vec<Value>) -> Value {
    json!({
        "schemas": [SCHEMA_GROUP],
        "id": g.id,
        "externalId": g.external_group_id,
        "displayName": g.name,
        "members": members,
        "meta": {
            "resourceType": "Group",
            "created": g.created_at,
            // directory_groups 沒有 updated_at 欄位（002）。回 created_at
            // 而不是省略：SCIM 客戶端會讀 lastModified，而省略它比一個
            // 保守（偏舊）的值更容易讓對方走進「重讀全部資源」的路徑。
            "lastModified": g.created_at,
            "location": format!("/scim/v2/Groups/{}", g.id),
        },
    })
}

/// `GET /scim/v2/Groups`
pub async fn list_groups(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Query(q): Query<ListQuery>,
) -> Result<Response, ScimError> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let (limit, offset) = q.window();

    let f = q
        .filter
        .as_deref()
        .map(|raw| parse_eq_filter(raw, &["displayName", "externalId"]))
        .transpose()?;
    let (by_name, by_external) = match &f {
        Some(f) if f.attr == "displayname" => (Some(f.value.as_str()), None),
        Some(f) => (None, Some(f.value.as_str())),
        None => (None, None),
    };

    let where_extra = " AND ($2::text IS NULL OR g.name = $2)
                        AND ($3::text IS NULL OR g.external_group_id = $3)";

    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM ({GROUP_SELECT}{where_extra}) t"
    ))
    .bind(caller.identity_provider_id)
    .bind(by_name)
    .bind(by_external)
    .fetch_one(tx.conn())
    .await?;

    let rows: Vec<GroupRow> = sqlx::query_as(&format!(
        "{GROUP_SELECT}{where_extra} ORDER BY g.name, g.id LIMIT $4 OFFSET $5"
    ))
    .bind(caller.identity_provider_id)
    .bind(by_name)
    .bind(by_external)
    .bind(limit)
    .bind(offset)
    .fetch_all(tx.conn())
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for g in &rows {
        let members = group_members(&mut tx, g.id).await?;
        out.push(render_group(g, members));
    }

    tx.commit().await?;
    Ok(list_response(total, offset + 1, out))
}

/// `GET /scim/v2/Groups/{id}`
pub async fn get_group(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Path(id): Path<Uuid>,
) -> Result<Response, ScimError> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let row: Option<GroupRow> = sqlx::query_as(&format!("{GROUP_SELECT} AND g.id = $2"))
        .bind(caller.identity_provider_id)
        .bind(id)
        .fetch_optional(tx.conn())
        .await?;
    let g = match row {
        Some(g) => g,
        None => return Err(ScimError::not_found(format!("找不到群組 {id}"))),
    };
    let members = group_members(&mut tx, g.id).await?;
    tx.commit().await?;
    Ok(scim_response(StatusCode::OK, render_group(&g, members)))
}

#[derive(Debug, Deserialize)]
pub struct GroupCreate {
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "externalId")]
    pub external_id: Option<String>,
    pub members: Option<Vec<ScimValue>>,
}

/// `POST /scim/v2/Groups`
pub async fn create_group(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Json(body): Json<GroupCreate>,
) -> Result<Response, ScimError> {
    let name = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ScimError::bad_request("invalidValue", "`displayName` 是必填的（RFC 7643 §4.2）")
        })?
        .to_string();

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let group_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.directory_groups
           (tenant_id, identity_provider_id, external_group_id, name, last_synced_at)
         VALUES (fms.current_tenant_id(), $1, coalesce($2, gen_random_uuid()::text), $3,
                 clock_timestamp())
         RETURNING id",
    )
    .bind(caller.identity_provider_id)
    // externalId 選填而 external_group_id 是 NOT NULL 且與 provider 組唯一鍵。
    // 沒送就生一個 uuid：那保證唯一，且 SCIM 的 `id` 仍是定位這個資源的鍵。
    .bind(
        body.external_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
    .bind(&name)
    .fetch_one(tx.conn())
    .await?;

    let member_ids = parse_member_ids(&body.members)?;
    add_members(&mut tx, caller, group_id, &member_ids).await?;
    sync_member_count(&mut tx, group_id).await?;

    let g: GroupRow = sqlx::query_as(&format!("{GROUP_SELECT} AND g.id = $2"))
        .bind(caller.identity_provider_id)
        .bind(group_id)
        .fetch_one(tx.conn())
        .await?;
    let members = group_members(&mut tx, group_id).await?;

    tx.commit().await?;
    Ok(scim_response(
        StatusCode::CREATED,
        render_group(&g, members),
    ))
}

fn parse_member_ids(members: &Option<Vec<ScimValue>>) -> Result<Vec<Uuid>, ScimError> {
    let Some(list) = members else {
        return Ok(vec![]);
    };
    list.iter()
        .map(|m| {
            let raw = m.value.as_deref().unwrap_or_default();
            raw.parse::<Uuid>().map_err(|_| {
                ScimError::bad_request(
                    "invalidValue",
                    format!("成員的 `value` 必須是這個系統的使用者 id（uuid），收到 `{raw}`"),
                )
            })
        })
        .collect()
}

/// 加入成員。
///
/// **只接受屬於同一個 provider 的使用者。** 不檢查的話，一個 SCIM token
/// 就能把同租戶任何使用者（含管理員）塞進一個群組，而 058 的目錄對應會
/// 依群組授予角色 —— 那是一條完整的權限提升路徑。
async fn add_members(
    tx: &mut fms_shared::TenantTx,
    caller: ScimCaller,
    group_id: Uuid,
    user_ids: &[Uuid],
) -> Result<(), ScimError> {
    if user_ids.is_empty() {
        return Ok(());
    }
    let inserted: i64 = sqlx::query_scalar(
        "WITH eligible AS (
           SELECT u.id FROM fms.users u
             JOIN fms.user_identities ui
               ON ui.user_id = u.id AND ui.identity_provider_id = $3
            WHERE u.id = ANY($2) AND u.deleted_at IS NULL
         ), ins AS (
           INSERT INTO fms.user_directory_groups (user_id, directory_group_id, tenant_id)
           SELECT e.id, $1, fms.current_tenant_id() FROM eligible e
           ON CONFLICT (user_id, directory_group_id) DO NOTHING
           RETURNING 1
         )
         SELECT (SELECT count(*) FROM eligible)",
    )
    .bind(group_id)
    .bind(user_ids)
    .bind(caller.identity_provider_id)
    .fetch_one(tx.conn())
    .await?;

    // 有成員被過濾掉就報錯，**不靜默接受一部分**。回 200 而群組裡少了人，
    // Entra 的報告會顯示成功，而那個人永遠拿不到群組對應的角色。
    if inserted != user_ids.len() as i64 {
        return Err(ScimError::bad_request(
            "invalidValue",
            format!(
                "{} 個成員裡有 {} 個不是此身分來源佈建的使用者 —— \
                 整個請求已被拒絕，不會只加入一部分",
                user_ids.len(),
                user_ids.len() as i64 - inserted
            ),
        ));
    }
    Ok(())
}

/// `directory_groups.member_count` 是快取欄位（002）。不更新它，
/// 「這個群組有幾個人」在管理界面上會永遠是 0。
async fn sync_member_count(tx: &mut fms_shared::TenantTx, group_id: Uuid) -> Result<(), ScimError> {
    sqlx::query(
        "UPDATE fms.directory_groups g
            SET member_count = (SELECT count(*) FROM fms.user_directory_groups m
                                 WHERE m.directory_group_id = g.id),
                last_synced_at = clock_timestamp()
          WHERE g.id = $1",
    )
    .bind(group_id)
    .execute(tx.conn())
    .await?;
    Ok(())
}

/// 從 `members[value eq "…"]` 這種形式的 path 取出 id。
///
/// Entra 移除成員時會用它（而不是把 id 放在 value 裡），因此不支援它
/// 等於「移除成員永遠不會生效」。
fn member_id_in_path(path: &str) -> Option<&str> {
    let inner = path.split_once('[')?.1.strip_suffix(']')?;
    let (_, v) = inner.split_once(" eq ")?;
    v.trim().strip_prefix('"')?.strip_suffix('"')
}

/// `PATCH /scim/v2/Groups/{id}`
pub async fn patch_group(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Path(id): Path<Uuid>,
    Json(body): Json<PatchRequest>,
) -> Result<Response, ScimError> {
    let ops = body.ops()?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let exists: Option<Uuid> = sqlx::query_scalar(&format!(
        "SELECT g.id FROM ({GROUP_SELECT} AND g.id = $2) g"
    ))
    .bind(caller.identity_provider_id)
    .bind(id)
    .fetch_optional(tx.conn())
    .await?;
    if exists.is_none() {
        return Err(ScimError::not_found(format!("找不到群組 {id}")));
    }

    for op in ops {
        let verb = op.op.to_ascii_lowercase();
        let path = op.path.as_deref().unwrap_or_default().to_ascii_lowercase();

        // `members[value eq "id"]` 形式的移除。
        if verb == "remove" && path.starts_with("members[") {
            let raw =
                member_id_in_path(op.path.as_deref().unwrap_or_default()).ok_or_else(|| {
                    ScimError::bad_request(
                        "invalidPath",
                        "只支援 `members[value eq \"<uuid>\"]` 這一種帶條件的 path",
                    )
                })?;
            let uid: Uuid = raw.parse().map_err(|_| {
                ScimError::bad_request("invalidValue", format!("`{raw}` 不是合法的 uuid"))
            })?;
            remove_members(&mut tx, id, &[uid]).await?;
            continue;
        }

        match (verb.as_str(), path.as_str()) {
            ("add", "members") | ("replace", "members") => {
                let members: Option<Vec<ScimValue>> = op
                    .value
                    .clone()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| {
                        ScimError::bad_request("invalidValue", format!("members 格式不對：{e}"))
                    })?;
                let ids = parse_member_ids(&members)?;
                if verb == "replace" {
                    // replace 的語意是「成員就是這一組」，因此先清空。
                    sqlx::query(
                        "DELETE FROM fms.user_directory_groups WHERE directory_group_id = $1",
                    )
                    .bind(id)
                    .execute(tx.conn())
                    .await?;
                }
                add_members(&mut tx, caller, id, &ids).await?;
            }
            ("remove", "members") => {
                // 沒有帶條件的 path 時，value 裡是要移除的成員清單。
                let members: Option<Vec<ScimValue>> = op
                    .value
                    .clone()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| {
                        ScimError::bad_request("invalidValue", format!("members 格式不對：{e}"))
                    })?;
                let ids = parse_member_ids(&members)?;
                if ids.is_empty() {
                    // `{"op":"remove","path":"members"}` 沒有 value：清空全部。
                    sqlx::query(
                        "DELETE FROM fms.user_directory_groups WHERE directory_group_id = $1",
                    )
                    .bind(id)
                    .execute(tx.conn())
                    .await?;
                } else {
                    remove_members(&mut tx, id, &ids).await?;
                }
            }
            ("add" | "replace", "displayname") => {
                let name = op
                    .value
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ScimError::bad_request("invalidValue", "displayName 不能是空的")
                    })?;
                sqlx::query("UPDATE fms.directory_groups SET name = $2 WHERE id = $1")
                    .bind(id)
                    .bind(name)
                    .execute(tx.conn())
                    .await?;
            }
            (_, other) => {
                return Err(ScimError::bad_request(
                    "invalidPath",
                    format!(
                        "群組不支援 `{}` `{other}`（可改：members 的 add／remove／replace、\
                         displayName 的 replace）",
                        op.op
                    ),
                ))
            }
        }
    }

    sync_member_count(&mut tx, id).await?;

    let g: GroupRow = sqlx::query_as(&format!("{GROUP_SELECT} AND g.id = $2"))
        .bind(caller.identity_provider_id)
        .bind(id)
        .fetch_one(tx.conn())
        .await?;
    let members = group_members(&mut tx, id).await?;

    tx.commit().await?;
    Ok(scim_response(StatusCode::OK, render_group(&g, members)))
}

async fn remove_members(
    tx: &mut fms_shared::TenantTx,
    group_id: Uuid,
    user_ids: &[Uuid],
) -> Result<(), ScimError> {
    sqlx::query(
        "DELETE FROM fms.user_directory_groups
          WHERE directory_group_id = $1 AND user_id = ANY($2)",
    )
    .bind(group_id)
    .bind(user_ids)
    .execute(tx.conn())
    .await?;
    Ok(())
}

/// `DELETE /scim/v2/Groups/{id}`
///
/// **這裡是真的刪除**，與使用者不同。`directory_groups` 沒有 `deleted_at`
/// 欄位（002），而群組本身不被工單或稽核軌引用 —— 引用它的
/// `user_directory_groups` 與 `directory_role_mappings` 都是 ON DELETE CASCADE。
///
/// 後果要說清楚：**由這個群組授予的角色會一併消失**（058 的對應失去來源）。
/// 那是正確的行為 —— 群組被移出供裝範圍就代表它不再授權任何人。
pub async fn delete_group(
    State(state): State<ScimState>,
    caller: ScimCaller,
    Path(id): Path<Uuid>,
) -> Result<Response, ScimError> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let affected = sqlx::query(
        "DELETE FROM fms.directory_groups
          WHERE id = $1 AND identity_provider_id = $2",
    )
    .bind(id)
    .bind(caller.identity_provider_id)
    .execute(tx.conn())
    .await?
    .rows_affected();

    tx.commit().await?;

    if affected == 0 {
        return Err(ScimError::not_found(format!("找不到群組 {id}")));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// =============================================================================
// token 發放
// =============================================================================

/// 產生一個新的 SCIM token 並存下它的雜湊。回傳**明文**（唯一的一次）。
///
/// 由 `PATCH /identity-providers/{id}` 的 `rotate_scim_token: true` 呼叫。
///
/// 為什麼由伺服器產生而不是讓管理者貼一個值進來：呼叫端提供的 token 可能是
/// `password123`。而 SCIM 端點的整個授權就只有這一個 bearer token ——
/// 它的強度不該取決於管理者的選擇。
pub async fn issue_token(
    tx: &mut fms_shared::TenantTx,
    identity_provider_id: Uuid,
    created_by: Uuid,
) -> Result<String, fms_shared::Problem> {
    // 撤銷既有的有效 token。074 的 `uq_scim_tokens_active` 讓「忘記撤銷」
    // 變成一個約束違反而不是兩個都能用的 token —— 但正確的順序仍要寫對。
    sqlx::query(
        "UPDATE fms.scim_tokens
            SET revoked_at = clock_timestamp(), revoked_reason = 'ROTATED'
          WHERE identity_provider_id = $1 AND revoked_at IS NULL",
    )
    .bind(identity_provider_id)
    .execute(tx.conn())
    .await
    .map_err(fms_shared::Problem::from)?;

    // 256 bit。取自作業系統的 CSPRNG（`Uuid::new_v4` 用 getrandom），
    // 兩個 uuid 的 simple 形式串起來剛好 64 個十六進位字元。
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let prefix = token[..8].to_string();

    sqlx::query(
        "INSERT INTO fms.scim_tokens
           (tenant_id, identity_provider_id, token_hash, token_prefix, created_by_user_id)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4)",
    )
    .bind(identity_provider_id)
    .bind(token_hash(&token))
    .bind(&prefix)
    // Uuid::nil() 不是一個真的使用者。背景／未認證路徑不會走到這裡
    // （發放需要 identity_provider:write），但 FK 是 ON DELETE SET NULL，
    // 因此保守地把 nil 轉成 NULL 而不是插一個指不到任何列的值。
    .bind((created_by != Uuid::nil()).then_some(created_by))
    .execute(tx.conn())
    .await
    .map_err(fms_shared::Problem::from)?;

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 對固定輸入的值。**這是一個獨立可驗的向量**
    /// （`printf 'scim-token' | shasum -a 256`），因此它同時證明
    /// 演算法與十六進位編碼都對 —— 自己算一次再貼上等於什麼都沒驗。
    #[test]
    fn token_hash_matches_a_known_vector() {
        assert_eq!(
            token_hash("scim-token"),
            "ae7370645e03c7c8af559179d3c40c931dffcc8863ea4bf42d59a7f509f6e735"
        );
    }

    #[test]
    fn eq_filter_accepts_what_entra_sends() {
        let f = parse_eq_filter("userName eq \"wang@corp.example.com\"", &["userName"]).unwrap();
        assert_eq!(f.attr, "username");
        assert_eq!(f.value, "wang@corp.example.com");

        // 運算子與屬性名的大小寫都不敏感（RFC 7644 §3.4.2.2）。
        let f = parse_eq_filter("USERNAME EQ \"a\"", &["userName"]).unwrap();
        assert_eq!(f.attr, "username");
    }

    /// 不支援的文法必須是錯誤。**這一格守的是最糟的失敗方式**：
    /// 忽略 filter 會讓「查一個人」變成「回傳全部人」。
    #[test]
    fn unsupported_filter_syntax_is_an_error_not_a_full_scan() {
        for raw in [
            "userName co \"wang\"",                     // co 不支援
            "userName eq \"a\" and active eq \"true\"", // and 不支援
            "userName eq wang",                         // 沒有引號
            "emails.value eq \"a@b.c\"",                // 不在允許清單
        ] {
            let err = parse_eq_filter(raw, &["userName", "externalId"]).unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{raw}");
            assert_eq!(err.scim_type, Some("invalidFilter"), "{raw}");
        }
    }

    /// 複合條件被擋下來的**原因**要對。
    ///
    /// 這一格存在是因為第一版真的錯了：我以為「值必須被引號包住」就足以擋下
    /// `userName eq "a" and active eq "true"`，但那個字串確實以引號開頭與結尾，
    /// 於是它通過了，value 被解析成 `a" and active eq "true` ——
    /// 查一個不存在的名字，回 0 筆，供裝端接著建立一個重複帳號。
    ///
    /// 真正擋住它的是「剝掉外層引號後裡面不能再有引號」。斷言指名那個理由，
    /// 這樣把該檢查拿掉時這一格會失敗，而不是恰好仍然通過。
    #[test]
    fn compound_filter_is_rejected_because_the_value_contains_a_quote() {
        let err =
            parse_eq_filter("userName eq \"a\" and active eq \"true\"", &["userName"]).unwrap_err();
        assert!(
            err.detail.contains("值裡不能有引號"),
            "擋下來的理由不是內層引號，那表示是巧合擋住的：{}",
            err.detail
        );
    }

    #[test]
    fn start_index_is_one_based_and_count_is_clamped() {
        let q = ListQuery {
            filter: None,
            start_index: Some(1),
            count: Some(10),
        };
        assert_eq!(q.window(), (10, 0), "startIndex=1 對應 offset 0");

        let q = ListQuery {
            filter: None,
            start_index: Some(21),
            count: None,
        };
        assert_eq!(q.window(), (MAX_COUNT, 20));

        // 規範：小於 1 的 startIndex 視為 1，不是錯誤。
        let q = ListQuery {
            filter: None,
            start_index: Some(0),
            count: Some(9999),
        };
        assert_eq!(q.window(), (MAX_COUNT, 0));
    }

    #[test]
    fn entra_sends_active_as_a_quoted_string() {
        assert!(as_bool(Some(&json!("True"))).unwrap());
        assert!(!as_bool(Some(&json!("False"))).unwrap());
        assert!(!as_bool(Some(&json!(false))).unwrap());
        assert!(as_bool(Some(&json!("yes"))).is_err());
        assert!(as_bool(None).is_err());
    }

    #[test]
    fn member_removal_path_is_parsed() {
        assert_eq!(
            member_id_in_path("members[value eq \"3f2504e0-4f89-41d3-9a0c-0305e82c3301\"]"),
            Some("3f2504e0-4f89-41d3-9a0c-0305e82c3301")
        );
        assert_eq!(member_id_in_path("members"), None);
        assert_eq!(member_id_in_path("members[value pr]"), None);
    }

    /// 沒有 path、value 是物件的 op 要被攤平。Entra 會送這種形式，
    /// 而只處理有 path 的那種會讓那次同步靜默地什麼都不改。
    #[test]
    fn op_without_path_is_flattened_from_the_value_object() {
        let op = PatchOp {
            op: "replace".into(),
            path: None,
            value: Some(json!({"active": false, "displayName": "新名字"})),
        };
        let flat = flatten_op(&op).unwrap();
        assert_eq!(flat.len(), 2);
        assert!(flat
            .iter()
            .any(|(k, v)| k == "active" && v == &json!(false)));
        assert!(flat.iter().any(|(k, _)| k == "displayname"));
    }

    #[test]
    fn pick_prefers_the_primary_value() {
        let vals = Some(vec![
            ScimValue {
                value: Some("second@corp.example.com".into()),
                primary: None,
            },
            ScimValue {
                value: Some("main@corp.example.com".into()),
                primary: Some(true),
            },
        ]);
        assert_eq!(pick(&vals).unwrap(), "main@corp.example.com");
    }
}
