//! 身分來源與目錄同步（`/identity-providers`）。
//!
//! # `:sync` 是 `directory_role_mappings` 的第一個消費者
//!
//! 在它之前，群組→角色對應**建得出來、列得到、刪得掉，卻永遠不會產生任何
//! 角色指派** —— 除了表定義、種子與那組 CRUD，整個 codebase 沒有程式碼讀它。
//! 而 `DELETE /role-assignments/{id}` 的錯誤訊息寫著「要移除請改群組對應」，
//! 那句話在同步存在之前是假的。
//!
//! 對帳的邏輯在 migration 058 的 `fms.reconcile_directory_roles()`，
//! 不在這裡 —— 它是集合運算，而且**收回**那一半在 SQL 裡寫得對得多。
//!
//! # 同步是同步執行的，仍然回 202
//!
//! 契約原本寫「非同步作業」，而非同步的理由是外部 I/O 慢。
//! 但 Phase 1 的對帳**不連 AD／Entra**：成員關係是別人放進
//! `user_directory_groups` 的，這裡只做規則 → 授權的集合運算，毫秒級完成。
//!
//! 為一個瞬間完成的操作套一層 outbox 與 worker，只會讓「好了沒」多一次往返。
//! 等 connector 進來時，**抓成員關係那一半**才需要非同步。
//!
//! # API 不接受明文密鑰
//!
//! 契約對 `client_secret_ref` 的說明是「密鑰管理服務中的參照名稱；
//! API 不接受明文密鑰」。這裡**主動擋掉看起來像密鑰的值** ——
//! 只在文件裡寫「請填參照」的話，第一個整合的人一定會直接貼密鑰進去，
//! 而它會進資料庫、進備份、進稽核的 `after_data`。

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, concurrency, page, require_tenant_scoped_permission, Caller,
    Cursor, PageMeta, Problem, SortSpec,
};

const ENDPOINT: &str = "POST /identity-providers";

const PROVIDER_TYPES: [&str; 4] = ["OIDC", "SAML2", "LDAP", "LOCAL"];

const STATUSES: [&str; 3] = ["ACTIVE", "DISABLED", "TESTING"];

/// `PATCH` 不接受的欄位，附理由。
///
/// 分成兩類，而**兩類都必須指名**：直接靠 `deny_unknown_fields` 會讓
/// 「這個欄位不能改」與「你打錯字了」變成同一個 400。與 `/tenant` 的
/// `PLATFORM_OWNED` 同一個判斷。
const NOT_PATCHABLE: &[(&str, &str)] = &[
    (
        "code",
        "身分來源代碼不可變更 —— 它是 /auth/sso/{providerCode} 的路徑鍵，\
         改掉會讓既有的 SSO 連結指向不存在的來源",
    ),
    (
        "provider_type",
        "型別不可變更 —— 每一種型別有各自的必填欄位（ck_idp_oidc_fields／\
         ck_idp_ldap_fields），換型別實際上是換一個來源。請新建一個",
    ),
    ("id", "識別碼不可變更"),
    ("tenant_id", "租戶歸屬不可變更"),
    ("created_at", "由資料庫維護"),
    ("updated_at", "由資料庫維護"),
    ("deleted_at", "刪除請走 DELETE"),
    (
        "last_sync_at",
        "由同步流程寫入，不是設定值 —— 手改它會讓「上次同步成功」變成假的",
    ),
    // 以下是真實存在的欄位，但 Phase 1 沒有任何程式碼讀它們，**而且**它們
    // 屬於連線／對應那一組：能改而完全不生效，比不能改更糟。
    // 這幾行的理由不是「還沒做」而是「改了不會有任何效果」，見 PATCH 的
    // 回應裡 `fields_with_no_consumer_yet` 那一段的完整說明。
    (
        "ldap_port",
        "Phase 1 沒有 LDAP 客戶端（見 migration 058 檔頭），這一欄沒有讀者",
    ),
    ("ldap_use_tls", "同上：沒有 LDAP 客戶端"),
    ("ldap_bind_dn", "同上：沒有 LDAP 客戶端"),
    (
        "ldap_bind_secret_ref",
        "參照本身已可解析（test-connection 的 secret_reference_resolvable），\
         但 Phase 1 沒有 LDAP 客戶端，因此沒有任何地方會拿它去 bind",
    ),
    ("ldap_user_filter", "同上：沒有 LDAP 客戶端"),
    ("ldap_group_filter", "同上：沒有 LDAP 客戶端"),
    (
        "attribute_mapping",
        "屬性對應由 JIT 佈建讀取，而 Phase 1 沒有 JIT 佈建（沒有 SSO 登入路徑）",
    ),
    ("group_claim_name", "同上：沒有 JIT 佈建"),
    ("jit_default_role_code", "同上：沒有 JIT 佈建"),
    ("auto_deprovision", "同上：沒有 JIT 佈建"),
    (
        "jwks_uri",
        "OIDC 的簽章金鑰位置由 /auth/sso/* 讀取，而那兩支尚未實作",
    ),
    ("audience", "同上：沒有 SSO 登入路徑"),
    (
        "metadata_xml_ref",
        "參照本身已可解析（test-connection 的 secret_reference_resolvable），\
         但沒有 SAML2 的處理程式會去讀那份 metadata 的內容",
    ),
    // `scim_enabled` **已經可以改**（見 `ProviderPatch`）—— SCIM 端點實作之後
    // 它成了那組端點的總開關（074 的 `authenticate_scim_token` 會檢查它）。
    (
        "scim_token_ref",
        "密鑰管理服務的參照，沒有解析器 —— 實際的 SCIM 憑證由 \
         `rotate_scim_token: true` 產生並存在 fms.scim_tokens（只存雜湊）",
    ),
    // `sync_cron` 現在**有**消費者了（migration 078 的服務帳號 +
    // `directory_sync_watchdog` 的背景迴圈），但仍然不可 PATCH ——
    // 目前沒有寫入路徑能逐一設定，所有來源共用 002 的預設值
    // `0 */4 * * *`。開放個別設定是後續工作，不是本次的範圍。
    (
        "sync_cron",
        "排程本身已經有消費者（migration 078），但目前沒有 PATCH 路徑可以個別\
         設定 —— 所有來源共用 002 的預設排程 `0 */4 * * *`",
    ),
    (
        "scope_org_path",
        "這一欄決定該來源管轄哪一棵組織子樹，目前沒有讀者；\
         而且它是 ltree，要改必須驗證那條路徑對應真實的組織 —— \
         沒有驗證就改，會寫進一條指向不存在節點的路徑",
    ),
];

/// PATCH 可以改，但**目前沒有任何程式碼讀**的欄位。
///
/// 與 [`NOT_PATCHABLE`] 的界線在哪裡：這一組是「記下你的 IdP 設定」這件事本身
/// 就有價值的欄位（`issuer`、`client_id` 是整合時要先談好的東西，記在系統裡
/// 是對的），而 `NOT_PATCHABLE` 裡那些是**只有在客戶端存在時才有意義**的旋鈕。
///
/// 這一組會在回應的 `meta.fields_with_no_consumer_yet` 裡列出來。**不列出來
/// 的話，管理者填完 issuer 與 client_id 會合理地以為 SSO 可以用了** ——
/// 而 `/auth/sso/*` 還是 404。這正是本專案反覆出現的那一類缺陷。
///
/// **`issuer`、`discovery_url`、`client_id` 已經不在這份清單裡** ——
/// `GET /auth/sso/{providerCode}/authorize` 會讀它們三個（前兩者用來取得
/// discovery 文件，`client_id` 直接進授權網址）。留在清單裡會讓欄位名說謊。
/// SSO 仍然無法完成登入這件事由 `client_secret_ref` 那一行說明。
const NO_CONSUMER_YET: &[(&str, &str)] = &[
    (
        "client_secret_ref",
        "已經有一個讀者：test-connection 的 secret_reference_resolvable 會回報這個\
         參照在本部署解不解得開。但 token 交換仍未實作 —— \
         /auth/sso/{providerCode}/callback 還是回 501，剩下的缺口是 id_token 的\
         JWKS 簽章驗證",
    ),
    (
        "ldap_host",
        "Phase 1 沒有 LDAP 客戶端（migration 058 檔頭）—— 成員關係目前由外部\
         寫進 user_directory_groups",
    ),
    ("ldap_base_dn", "同上：沒有 LDAP 客戶端"),
    (
        "jit_provisioning",
        "JIT 佈建發生在 SSO 登入成功的那一刻，而 callback 停在 token 交換之前\
         （501）—— 因此還沒有觸發點",
    ),
];

#[derive(Clone)]
pub struct IdentityProvidersState {
    pub pool: PgPool,
    /// `test-connection` 用。這是整個系統唯一一組「往呼叫端填的網址發請求」
    /// 的設定，見 `fms_shared::safe_http` 模組說明。
    pub outbound: fms_shared::OutboundSettings,
    /// `*_secret_ref` 的解析器。`test-connection` 用它回答
    /// 「這個參照在這個部署裡解得開嗎」—— 見
    /// `fms_shared::secrets` 模組說明。
    ///
    /// `Arc<dyn>` 而不是泛型：這個 state 被 clone 進每一條路由，
    /// 泛型會讓 `build_router` 的簽章長出一個只有這裡用得到的型別參數。
    pub secrets: std::sync::Arc<dyn fms_shared::SecretResolver>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ProviderDto {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub provider_type: String,
    pub scope_org_path: Option<String>,
    pub issuer: Option<String>,
    pub jit_provisioning: bool,
    pub sync_enabled: bool,
    pub scim_enabled: bool,
    pub is_default: bool,
    pub status: String,
    pub last_sync_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SyncRunDto {
    pub id: Uuid,
    pub run_type: String,
    pub status: String,
    pub groups_synced: i32,
    pub roles_granted: i32,
    pub roles_revoked: i32,
    pub error_summary: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ProviderCreate {
    pub code: Option<String>,
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub issuer: Option<String>,
    pub discovery_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret_ref: Option<String>,
    pub ldap_host: Option<String>,
    pub ldap_base_dn: Option<String>,
    pub jit_provisioning: Option<bool>,
    pub sync_enabled: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SyncBody {
    pub run_type: Option<String>,
}

const COLUMNS: &str = "id, code::text AS code, name::text AS name, provider_type,
                       scope_org_path::text AS scope_org_path, issuer,
                       jit_provisioning, sync_enabled, scim_enabled, is_default,
                       status, last_sync_at";

/// `GET /identity-providers`
pub async fn list(
    State(state): State<IdentityProvidersState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "identity_provider:read").await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "code".to_string(),
        desc: false,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.key.clone()), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<ProviderDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM fms.identity_providers
          WHERE deleted_at IS NULL
            AND ($1::text IS NULL OR (code::text, id) > ($1::text, $2::uuid))
          ORDER BY code, id
          LIMIT $3"
    ))
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.code.clone(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

/// `POST /identity-providers`
pub async fn create(
    State(state): State<IdentityProvidersState>,
    caller: Caller,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let body: ProviderCreate = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid IdentityProviderCreate: {e}")))?;

    let code = required(&body.code, "code")?;
    let name = required(&body.name, "name")?;
    let ptype = required(&body.provider_type, "provider_type")?.to_uppercase();
    if !PROVIDER_TYPES.contains(&ptype.as_str()) {
        return Err(Problem::validation(format!(
            "provider_type 必須是 {} 其中之一",
            PROVIDER_TYPES.join("／")
        )));
    }
    reject_plaintext_secret(body.client_secret_ref.as_deref())?;

    // 每個 provider_type 的必填欄位。
    //
    // **這組規則抄自資料庫的 CHECK 約束，不是我自己想的。**
    // 第一版寫成「LDAP 要 ldap_host、OIDC 要 issuer 或 discovery_url」，
    // 而 002 的 `ck_idp_ldap_fields` / `ck_idp_oidc_fields` 要的是
    // `ldap_host + ldap_base_dn` 與 `issuer + client_id` ——
    // 資料庫比我嚴格，而且它是對的（少了 base_dn 就查不到任何項目，
    // 少了 client_id 就換不到 token）。
    //
    // 在這裡擋是為了**錯誤訊息說得出缺什麼**；真正的權威是那兩條 CHECK，
    // 而 `translate` 會把它們翻成 422 當作漂移的後盾。
    match ptype.as_str() {
        "LDAP" if body.ldap_host.is_none() || body.ldap_base_dn.is_none() => {
            return Err(Problem::validation(
                "provider_type = LDAP 必須同時提供 ldap_host 與 ldap_base_dn                  —— 少了 base_dn 就查不到任何項目",
            ))
        }
        "OIDC" if body.issuer.is_none() || body.client_id.is_none() => {
            return Err(Problem::validation(
                "provider_type = OIDC 必須同時提供 issuer 與 client_id                  —— 少了 client_id 就換不到 token",
            ))
        }
        _ => {}
    }

    let idem_key = concurrency::key_from(&headers)?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let pending = match idem_key.as_deref() {
        Some(key) => concurrency::begin(&mut tx, key, ENDPOINT, &raw)
            .await?
            .pending(),
        None => None,
    };
    let auth = require_tenant_scoped_permission(&mut tx, "identity_provider:write").await?;
    // 回放直接回存下來的 JSON，不再反序列化成 DTO —— 回放的權威是
    // **當初回給客戶端的那份 body**，重新組一次只會多一個漂移的機會。
    if let Some(replay) = pending {
        let (code, body) = replay.release(auth);
        return Ok((code, Json(body)));
    }

    let row: ProviderDto = sqlx::query_as(&format!(
        "INSERT INTO fms.identity_providers
           (tenant_id, code, name, provider_type, issuer, discovery_url,
            client_id, client_secret_ref, ldap_host, ldap_base_dn,
            jit_provisioning, sync_enabled)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7, $8, $9,
                 coalesce($10, false), coalesce($11, false))
         RETURNING {COLUMNS}"
    ))
    .bind(code)
    .bind(name)
    .bind(&ptype)
    .bind(body.issuer.as_deref())
    .bind(body.discovery_url.as_deref())
    .bind(body.client_id.as_deref())
    .bind(body.client_secret_ref.as_deref())
    .bind(body.ldap_host.as_deref())
    .bind(body.ldap_base_dn.as_deref())
    .bind(body.jit_provisioning)
    .bind(body.sync_enabled)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;

    let as_json = serde_json::to_value(&row)
        .map_err(|e| Problem::internal(std::io::Error::other(e.to_string())))?;
    if let Some(key) = idem_key.as_deref() {
        concurrency::complete(&mut tx, key, ENDPOINT, 201, &as_json).await?;
    }
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(as_json)))
}

/// `PATCH /identity-providers/{providerId}` 的請求。
///
/// 可為 NULL 的欄位走 `Option<Option<T>>`：外層 `None` 是「沒提供，不要動」，
/// 內層 `None` 是「清空」。少了這層區分，`{"issuer": null}` 與完全不提 `issuer`
/// 會變成同一件事，於是**沒有辦法清掉一個填錯的值**。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPatch {
    pub name: Option<String>,
    pub status: Option<String>,
    pub is_default: Option<bool>,
    pub jit_provisioning: Option<bool>,
    pub sync_enabled: Option<bool>,
    /// SCIM 供裝的總開關。關掉之後 074 的 `authenticate_scim_token` 認不過
    /// 任何 token —— 也就是說這個開關是**真的**開關，不是設定備忘。
    pub scim_enabled: Option<bool>,
    /// **不是資料庫欄位，是一個動作。** `true` 時產生一個新的 SCIM token，
    /// 撤銷舊的，並在這次回應的 `meta.scim_token` 裡回傳明文**一次**。
    ///
    /// 為什麼混在 PATCH 裡而不是開一支端點：契約沒有這一支，而
    /// `identity_provider:write` 的持有者本來就能改掉整個身分來源 ——
    /// 輪替它的憑證不是一個更高的權限。
    pub rotate_scim_token: Option<bool>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub issuer: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub discovery_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub client_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub client_secret_ref: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub ldap_host: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option::deserialize")]
    pub ldap_base_dn: Option<Option<String>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::deserialize(d).map(Some)
    }
}

/// PATCH 之前的狀態。**必須先讀出來**，理由有兩個：
///
///   1. 每一種型別的必填欄位要對**合併後**的值檢查。只看請求的話，
///      `{"issuer": null}` 送給一個 OIDC 來源看起來沒問題，而它會把
///      `ck_idp_oidc_fields` 弄壞 —— 資料庫會擋（`translate` 翻成 422），
///      但錯誤訊息只說得出「約束被違反」，說不出「你把唯一的 issuer 清掉了」。
///   2. `provider_type` 不在請求裡（它不可變更），而必填規則依它而定。
///
/// `FOR UPDATE` 順帶把同一列的並發 PATCH 排成序 —— 兩個請求各改一半欄位時，
/// 「先讀再合併再寫」若沒有鎖，後寫的那個會用它讀到的舊值覆蓋前一個的變更。
#[derive(Debug, sqlx::FromRow)]
struct ProviderEditable {
    provider_type: String,
    issuer: Option<String>,
    client_id: Option<String>,
    ldap_host: Option<String>,
    ldap_base_dn: Option<String>,
}

/// `PATCH /identity-providers/{providerId}`
///
/// 需要 `identity_provider:write`（TENANT 範圍）。
///
/// # 回應會列出「填了但沒有人讀」的欄位
///
/// `identity_providers` 在 Phase 1 有一部分欄位還沒有消費者：migration 058
/// 檔頭記著「去外部目錄抓成員關係那一半不存在」（沒有 LDAP／Graph 客戶端），
/// 而 `/auth/sso/{code}/callback` 停在 token 交換之前回 501。因此填完
/// `client_secret_ref` 之後 **SSO 登入仍然無法完成** —— 雖然 `/authorize`
/// 已經可以正常跳轉。
///
/// 回 200 加一份看起來完整的設定，會讓管理者以為可以用了；
/// 所以這裡把那些欄位列在 `meta.fields_with_no_consumer_yet`。
/// 這不是免責聲明 —— 它是這次請求真的改了、而且真的還不會生效的清單。
pub async fn patch(
    State(state): State<IdentityProvidersState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    body: Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, Problem> {
    // 先看原始 JSON：`deny_unknown_fields` 會把「這個欄位不能改」與
    // 「你打錯字了」壓成同一個錯誤，而那兩件事的處置完全不同。
    let Some(obj) = body.0.as_object() else {
        return Err(Problem::validation("請求體必須是一個 JSON 物件"));
    };
    if obj.is_empty() {
        return Err(Problem::validation(
            "沒有要更新的欄位 —— 空的 PATCH 不會有任何效果",
        ));
    }
    let blocked: Vec<fms_shared::FieldError> = NOT_PATCHABLE
        .iter()
        .filter(|(f, _)| obj.contains_key(*f))
        .map(|(f, why)| fms_shared::FieldError {
            pointer: format!("/{f}"),
            code: "NOT_PATCHABLE".to_string(),
            message: format!("`{f}` 不能改：{why}"),
        })
        .collect();
    if !blocked.is_empty() {
        // **整個請求被拒，不是靜默忽略那幾個欄位。** 回 200 而其中一半沒生效，
        // 管理者會以為都寫進去了。
        return Err(
            Problem::validation("請求包含不可變更的欄位，整個請求已被拒絕").with_errors(blocked),
        );
    }

    let req: ProviderPatch = serde_json::from_value(body.0.clone()).map_err(|e| {
        Problem::validation(format!("不認識的欄位或型別不符：{e}")).with_errors(vec![
            fms_shared::FieldError {
                pointer: "/".to_string(),
                code: "UNKNOWN_FIELD".to_string(),
                message: e.to_string(),
            },
        ])
    })?;

    if let Some(s) = &req.status {
        if !STATUSES.contains(&s.as_str()) {
            return Err(Problem::validation(format!(
                "status 必須是 {} 其中之一",
                STATUSES.join("／")
            )));
        }
    }
    if let Some(Some(secret)) = &req.client_secret_ref {
        reject_plaintext_secret(Some(secret))?;
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "identity_provider:write").await?;

    let current: ProviderEditable = sqlx::query_as(
        "SELECT provider_type, issuer, client_id, ldap_host, ldap_base_dn
           FROM fms.identity_providers
          WHERE id = $1 AND deleted_at IS NULL
          FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("identity provider not found"))?;

    // 合併：外層 None 保留現值，內層 None 清空。
    let merge = |patch: &Option<Option<String>>, cur: &Option<String>| -> Option<String> {
        match patch {
            None => cur.clone(),
            Some(v) => v
                .clone()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    };
    let issuer = merge(&req.issuer, &current.issuer);
    let client_id = merge(&req.client_id, &current.client_id);
    let ldap_host = merge(&req.ldap_host, &current.ldap_host);
    let ldap_base_dn = merge(&req.ldap_base_dn, &current.ldap_base_dn);

    // 對**合併後**的值檢查型別必填規則。權威仍是 002 的那兩條 CHECK
    // （`translate` 是後盾），這裡的作用是讓訊息說得出少了什麼。
    match current.provider_type.as_str() {
        "OIDC" if issuer.is_none() || client_id.is_none() => {
            return Err(Problem::validation(
                "OIDC 來源必須同時有 issuer 與 client_id —— \
                 這次修改會把其中一個清空",
            ))
        }
        "LDAP" if ldap_host.is_none() || ldap_base_dn.is_none() => {
            return Err(Problem::validation(
                "LDAP 來源必須同時有 ldap_host 與 ldap_base_dn —— \
                 這次修改會把其中一個清空",
            ))
        }
        _ => {}
    }

    // 投影寫在 `RETURNING` 裡，不用資料修改型 CTE：後者回傳的是**更新前**的
    // snapshot（PostgreSQL 手冊 7.8.2），症狀是「儲存成功但畫面是舊值」。
    let row: ProviderDto = sqlx::query_as(&format!(
        "UPDATE fms.identity_providers
            SET name              = coalesce($2, name),
                status            = coalesce($3, status),
                is_default        = coalesce($4, is_default),
                jit_provisioning  = coalesce($5, jit_provisioning),
                sync_enabled      = coalesce($6, sync_enabled),
                issuer            = $7,
                client_id         = $8,
                ldap_host         = $9,
                ldap_base_dn      = $10,
                discovery_url     = CASE WHEN $11 THEN $12 ELSE discovery_url END,
                client_secret_ref = CASE WHEN $13 THEN $14 ELSE client_secret_ref END,
                scim_enabled      = coalesce($15, scim_enabled),
                updated_at        = clock_timestamp()
          WHERE id = $1 AND deleted_at IS NULL
        RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(req.name.as_deref())
    .bind(req.status.as_deref())
    .bind(req.is_default)
    .bind(req.jit_provisioning)
    .bind(req.sync_enabled)
    .bind(issuer.as_deref())
    .bind(client_id.as_deref())
    .bind(ldap_host.as_deref())
    .bind(ldap_base_dn.as_deref())
    // `discovery_url` 與 `client_secret_ref` 不參與上面的必填檢查，所以不需要
    // 合併出一個值 —— 但仍要分辨「沒提供」與「清空」，因此走一個布林旗標。
    // `coalesce` 分不出這兩者（NULL 對它一律是「用現值」）。
    .bind(req.discovery_url.is_some())
    .bind(req.discovery_url.clone().flatten())
    .bind(req.client_secret_ref.is_some())
    .bind(req.client_secret_ref.clone().flatten())
    .bind(req.scim_enabled)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;

    // 輪替在 UPDATE **之後**：`rotate_scim_token: true` 與
    // `scim_enabled: false` 一起送時，順序決定結果。先發 token 再關開關
    // 會回傳一個立刻就無效的 token，而那看起來像成功。
    let scim_token = match req.rotate_scim_token {
        Some(true) => Some(crate::scim::issue_token(&mut tx, id, caller.user_id).await?),
        _ => None,
    };

    tx.commit().await?;

    // 只列這次請求真的提到的欄位。全部列出來會變成一份免責聲明，
    // 而管理者需要的是「我剛剛填的這幾個還不會生效」。
    let inert: Vec<serde_json::Value> = NO_CONSUMER_YET
        .iter()
        .filter(|(f, _)| obj.contains_key(*f))
        .map(|(f, why)| serde_json::json!({ "field": f, "reason": why }))
        .collect();

    let mut meta = serde_json::json!({
        "fields_with_no_consumer_yet": inert,
        "not_patchable_fields": NOT_PATCHABLE
            .iter()
            .map(|(f, why)| serde_json::json!({ "field": f, "reason": why }))
            .collect::<Vec<_>>(),
    });
    if let Some(token) = scim_token {
        // **唯一的一次。** 074 只存 SHA-256，沒有任何路徑能再讀出明文。
        // 放在 meta 而不是 data：它不是這個資源的一個欄位，
        // 而是這次請求的一個一次性產出（下一次 GET 不會有它）。
        meta["scim_token"] = serde_json::json!(token);
        meta["scim_token_notice"] = serde_json::json!(
            "這是唯一一次看到明文的機會 —— 系統只存 SHA-256 雜湊。\
             請立刻貼進 Entra ID 的 Secret Token。舊的 token 已被撤銷。"
        );
    }

    Ok(Json(serde_json::json!({ "data": row, "meta": meta })))
}

/// `test-connection` 的一格檢查結果。
#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    /// `PASSED` / `FAILED`。
    status: &'static str,
    detail: String,
}

/// 一格**沒有做**的檢查，附理由。
///
/// 這個型別存在的意義就是不讓「沒做」偽裝成「通過」。契約寫的是
/// 「測試 LDAP bind / OIDC discovery」，而 LDAP bind 在 Phase 1 做不到 ——
/// 回一個 `result: PASSED` 而其實只 TCP 連了一下，是在對整合的人說謊。
#[derive(Debug, Serialize)]
struct NotPerformed {
    name: &'static str,
    reason: &'static str,
}

/// 測試連線所需的欄位。
#[derive(Debug, sqlx::FromRow)]
struct ProviderProbe {
    provider_type: String,
    issuer: Option<String>,
    discovery_url: Option<String>,
    client_secret_ref: Option<String>,
    ldap_host: Option<String>,
    ldap_port: Option<i32>,
    ldap_use_tls: bool,
    ldap_bind_dn: Option<String>,
    ldap_bind_secret_ref: Option<String>,
    metadata_xml_ref: Option<String>,
}

/// 「這個 `*_ref` 在這個部署裡解得開嗎」這一格檢查。
///
/// # 為什麼這格值得存在
///
/// 「IdP 上設了 `client_secret_ref`，但部署忘了提供對應的環境變數」是真實而
/// 常見的組態錯誤，而在有解析器之前它**完全不可觀察** —— 要等到有人試著登入
/// 才會炸，而那時症狀出現在 IdP 那一側。
///
/// # 回應裡絕不出現密鑰值
///
/// 只回「解開了／沒解開」與**預期的環境變數名**。後者是這格全部的實用價值：
/// 維運不必去讀命名規則就知道要設什麼。`Secret` 拿到就立刻丟掉。
///
/// # 只在參照**有設定**時才呼叫
///
/// 「沒設定」不等於「錯」：OIDC 的 public client（PKCE-only）本來就沒有
/// client_secret，LDAP 也可以匿名 bind。而 `identity_providers` 沒有任何一欄
/// 分得出 confidential 與 public client —— 對沒設定的情形回 FAILED 會誤報，
/// 而**做一個會誤報的檢查比不做更糟**（與 `tls_handshake` 同一個判斷）。
/// 沒設定的情形由呼叫端放進 `checks_not_performed`。
fn check_secret_ref(
    resolver: &dyn fms_shared::SecretResolver,
    field: &'static str,
    name: &'static str,
    reference: &str,
) -> Check {
    match resolver.resolve(reference) {
        Ok(_) => Check {
            name,
            status: "PASSED",
            detail: format!("{field} 的參照在這個部署裡解析成功"),
        },
        Err(e) => Check {
            name,
            status: "FAILED",
            detail: format!("{field} = {reference}：{e}"),
        },
    }
}

/// 早退時把一格 FAILED 接在**已經收集到的** checks 後面。
///
/// 早退曾經寫成 `vec![Check { … }]`，也就是丟掉前面所有結果。
/// 那讓「網路目標沒設好」把「密鑰參照解不開」蓋掉 —— 而後者是呼叫端同樣
/// 需要知道、而且與網路無關的事實。
fn with_check(mut checks: Vec<Check>, name: &'static str, detail: String) -> Vec<Check> {
    checks.push(Check {
        name,
        status: "FAILED",
        detail,
    });
    checks
}

/// `POST /identity-providers/{providerId}/test-connection`
///
/// 需要 `identity_provider:write`（TENANT 範圍）—— 讀權限不夠：這支端點會讓
/// 伺服器對外發出網路請求，那是一個副作用。
///
/// # 這是整個系統第一段真的去跟設定好的 IdP 說話的程式碼
///
/// 在它之前，`identity_providers` 的每一個連線欄位都沒有讀者（見 `patch` 的
/// `NO_CONSUMER_YET`）。因此這支端點的價值不只是「測試」——
/// 它是那些設定值第一次被真的使用。
///
/// 目標網址取自**儲存的設定**，不是 request body。那本身是一層限制：
/// 要改目標得先 `PATCH`，而那需要 `identity_provider:write` 並留下稽核軌。
/// SSRF 的其餘防護在 `fms_shared::safe_http`（強制 https、解析後逐一檢查
/// 位址、pin 住已檢查的 IP、不跟隨轉址）。
///
/// # 每一種型別能驗到什麼，以及驗不到什麼
///
/// 三種外部型別都會多一格 `secret_reference_resolvable`：對應的 `*_ref` 在
/// **這個部署**裡解不解得開（`fms_shared::secrets`）。它不出網、不受目標可達性
/// 影響，因此即使網路那幾格全 FAILED 它也答得出來 —— 這也是為什麼它在
/// 任何網路請求之前就先算好。
///
/// * **OIDC** —— 完整可驗：抓 discovery 文件、檢查必要端點、並比對文件裡的
///   `issuer` 與設定的 `issuer` 相符（OIDC Discovery §4.3 要求，不符是真的
///   組態錯誤）。
/// * **LDAP** —— **bind 驗不到。** 兩個獨立的原因：Phase 1 沒有 LDAP 客戶端
///   （migration 058 檔頭）。密鑰參照本身**解得開**（`secret_reference_resolvable`），
///   但沒有任何程式碼會拿它去 bind。能做的只有 TCP 可達性，
///   而那連「對面是不是 LDAP」都不能斷言。
/// * **SAML2** —— `metadata_xml_ref` 同樣是參照，沒有可抓的網址。
/// * **LOCAL** —— 沒有外部系統。回 `NOT_TESTABLE`，不是 `PASSED`。
///
/// 這些界線全部落在回應的 `checks_not_performed` 裡，型別化而不是寫在文件裡。
pub async fn test_connection(
    State(state): State<IdentityProvidersState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "identity_provider:write").await?;
    let p: ProviderProbe = sqlx::query_as(
        "SELECT provider_type, issuer, discovery_url, client_secret_ref, ldap_host, ldap_port,
                ldap_use_tls, ldap_bind_dn, ldap_bind_secret_ref, metadata_xml_ref
           FROM fms.identity_providers
          WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .ok_or_else(|| Problem::not_found("identity provider not found"))?;
    // **在發出網路請求之前結束交易。** 出站請求最多花 total_timeout（預設
    // 10 秒），而一個橫跨網路 I/O 的資料庫交易會佔住連線池的一格那麼久 ——
    // 幾個同時進行的測試就能把池子吃光。這一支不寫任何東西，所以沒有
    // 「先寫再對外」的一致性問題。
    tx.commit().await?;

    let mut checks: Vec<Check> = Vec::new();
    let mut not_performed: Vec<NotPerformed> = Vec::new();
    let mut target = serde_json::Value::Null;

    // 密鑰參照先驗，**在任何網路請求之前**。它不需要出網，而且它是唯一一格
    // 「無論網路通不通都答得出來」的檢查 —— 放在後面會被前面的早退吃掉。
    let secret_ref = match p.provider_type.as_str() {
        "OIDC" => Some(("client_secret_ref", p.client_secret_ref.as_deref())),
        // LDAP 只在設了 bind DN 時才需要密碼；匿名 bind 是合法組態。
        // 「設了 DN 卻沒有 ref」由下面的 bind_credentials_configured 報。
        "LDAP" if p.ldap_bind_dn.is_some() => {
            Some(("ldap_bind_secret_ref", p.ldap_bind_secret_ref.as_deref()))
        }
        "SAML2" => Some(("metadata_xml_ref", p.metadata_xml_ref.as_deref())),
        _ => None,
    };
    if let Some((field, reference)) = secret_ref {
        match reference.map(str::trim).filter(|s| !s.is_empty()) {
            Some(reference) => checks.push(check_secret_ref(
                state.secrets.as_ref(),
                field,
                "secret_reference_resolvable",
                reference,
            )),
            None => not_performed.push(NotPerformed {
                name: "secret_reference_resolvable",
                reason: "這個 provider 沒有設定密鑰參照，因此沒有可解析的對象。\
                         不回 FAILED：OIDC 的 public client（PKCE-only）本來就沒有\
                         client_secret，而這張表沒有任何一欄分得出 confidential 與\
                         public client —— 會誤報的檢查比不做更糟。",
            }),
        }
    }

    match p.provider_type.as_str() {
        "OIDC" => {
            // discovery_url 優先；沒有就用 issuer 依 OIDC Discovery §4 組出來。
            let url = match p.discovery_url.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(u) => u.to_string(),
                None => match p.issuer.as_deref().filter(|s| !s.trim().is_empty()) {
                    Some(iss) => format!(
                        "{}/.well-known/openid-configuration",
                        iss.trim_end_matches('/')
                    ),
                    None => {
                        return Ok(Json(render(
                            &p.provider_type,
                            "FAILED",
                            with_check(
                                checks,
                                "target_configured",
                                "既沒有 discovery_url 也沒有 issuer，沒有可以測試的目標"
                                    .to_string(),
                            ),
                            not_performed,
                            target,
                        )))
                    }
                },
            };

            let checked =
                match fms_shared::safe_http::resolve_and_check(&url, &state.outbound).await {
                    Ok(c) => c,
                    Err(rejection) => {
                        // 被閘門擋下來也是一個檢查結果，不是 500 ——
                        // 而且原因必須說出來（那是呼叫端可以修的事）。
                        return Ok(Json(render(
                            &p.provider_type,
                            "FAILED",
                            with_check(checks, "target_is_permitted", rejection.to_string()),
                            not_performed,
                            serde_json::json!({ "url": url }),
                        )));
                    }
                };
            target = serde_json::json!({
                "url": checked.url.as_str(),
                "resolved_addr": checked.addr.to_string(),
                // 一個「因為白名單才成立」的成功與一般的成功不同，要說出來。
                "allowed_by_private_target_allowlist": checked.allowlisted,
            });

            match fms_shared::safe_http::get_capped(&checked, &state.outbound).await {
                Err(e) => checks.push(Check {
                    name: "discovery_document_reachable",
                    status: "FAILED",
                    detail: e,
                }),
                Ok((status, body)) => {
                    if !status.is_success() {
                        checks.push(Check {
                            name: "discovery_document_reachable",
                            status: "FAILED",
                            detail: format!("HTTP {status}"),
                        });
                    } else {
                        checks.push(Check {
                            name: "discovery_document_reachable",
                            status: "PASSED",
                            detail: format!("HTTP {status}，{} bytes", body.len()),
                        });
                        inspect_discovery(&body, p.issuer.as_deref(), &mut checks);
                    }
                }
            }
        }
        "LDAP" => {
            let Some(host) = p.ldap_host.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Ok(Json(render(
                    &p.provider_type,
                    "FAILED",
                    with_check(
                        checks,
                        "target_configured",
                        "沒有 ldap_host，沒有可以測試的目標".to_string(),
                    ),
                    not_performed,
                    target,
                )));
            };
            // 沒設埠時用 ldap_use_tls 決定預設：636 是 LDAPS，389 是明文／StartTLS。
            let port = p
                .ldap_port
                .and_then(|v| u16::try_from(v).ok())
                .unwrap_or(if p.ldap_use_tls { 636 } else { 389 });

            let checked =
                match fms_shared::safe_http::resolve_and_check_host(host, port, &state.outbound)
                    .await
                {
                    Ok(c) => c,
                    Err(rejection) => {
                        return Ok(Json(render(
                            &p.provider_type,
                            "FAILED",
                            with_check(checks, "target_is_permitted", rejection.to_string()),
                            not_performed,
                            serde_json::json!({ "host": host, "port": port }),
                        )))
                    }
                };
            target = serde_json::json!({
                "host": checked.host,
                "port": checked.port,
                "resolved_addr": checked.addr.to_string(),
                "allowed_by_private_target_allowlist": checked.allowlisted,
            });

            match fms_shared::safe_http::tcp_probe(&checked, &state.outbound).await {
                Ok(ms) => checks.push(Check {
                    name: "tcp_reachable",
                    status: "PASSED",
                    detail: format!("{ms} ms 內建立 TCP 連線"),
                }),
                Err(e) => checks.push(Check {
                    name: "tcp_reachable",
                    status: "FAILED",
                    detail: e,
                }),
            }

            not_performed.push(NotPerformed {
                name: "ldap_bind",
                reason: "Phase 1 沒有 LDAP 客戶端（migration 058 檔頭）—— \
                         沒有任何程式碼會拿密鑰去 bind。參照解不解得開由 \
                         secret_reference_resolvable 回報，而解得開不等於 bind 得成功。\
                         `tcp_reachable` 通過也只代表那個埠有東西在聽。",
            });
            not_performed.push(NotPerformed {
                name: "tls_handshake",
                reason: "LDAPS（通常 636）是 TLS 包裝、StartTLS（通常 389）不是，\
                         而 ldap_use_tls 這一欄分不出兩者。對 389 做 TLS 交握會失敗\
                         **即使伺服器正常** —— 做一個會誤報的檢查比不做更糟。",
            });
            if p.ldap_bind_dn.is_some() && p.ldap_bind_secret_ref.is_none() {
                checks.push(Check {
                    name: "bind_credentials_configured",
                    status: "FAILED",
                    detail: "設了 ldap_bind_dn 但沒有 ldap_bind_secret_ref —— \
                             等目錄客戶端接上時這組設定會無法認證"
                        .to_string(),
                });
            }
        }
        "SAML2" => {
            not_performed.push(NotPerformed {
                name: "saml_metadata_fetch",
                reason: "SAML2 的 metadata 存在 metadata_xml_ref（密鑰管理服務的參照），\
                         沒有可抓的網址。參照解不解得開由 secret_reference_resolvable \
                         回答，但 Phase 1 沒有 SAML2 的處理程式去讀那份 XML 的內容。",
            });
        }
        "LOCAL" => {
            not_performed.push(NotPerformed {
                name: "external_connection",
                reason: "LOCAL 來源沒有外部系統可測 —— 帳號與密碼都在這個資料庫裡。\
                         回 PASSED 會讓「測試通過」這句話失去意義。",
            });
        }
        other => {
            return Err(Problem::validation(format!(
                "provider_type `{other}` 沒有對應的連線測試"
            )))
        }
    }

    // 一格都沒做 → NOT_TESTABLE；有 FAILED → FAILED；否則 PASSED。
    //
    // **沒有檢查可做時不能回 PASSED。** 那是這支端點最容易寫錯的地方：
    // 一個空的檢查清單搭配 `checks.iter().all(passed)` 會回傳 true。
    let result = if checks.is_empty() {
        "NOT_TESTABLE"
    } else if checks.iter().any(|c| c.status == "FAILED") {
        "FAILED"
    } else {
        "PASSED"
    };

    Ok(Json(render(
        &p.provider_type,
        result,
        checks,
        not_performed,
        target,
    )))
}

/// 檢查 discovery 文件的內容。
///
/// 只驗「這份文件能不能用來做 OIDC 登入」所需的最小集合。抓得到但缺
/// `token_endpoint` 的文件是**組態錯誤**，而症狀會延到第一個使用者登入時
/// 才出現 —— 那正是這支端點該提早抓到的東西。
fn inspect_discovery(body: &[u8], configured_issuer: Option<&str>, checks: &mut Vec<Check>) {
    let doc: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            checks.push(Check {
                name: "discovery_document_is_json",
                status: "FAILED",
                detail: format!("回應不是合法的 JSON：{e}"),
            });
            return;
        }
    };
    let Some(obj) = doc.as_object() else {
        checks.push(Check {
            name: "discovery_document_is_json",
            status: "FAILED",
            detail: "回應是 JSON 但不是物件".to_string(),
        });
        return;
    };

    let missing: Vec<&str> = [
        "issuer",
        "authorization_endpoint",
        "token_endpoint",
        "jwks_uri",
    ]
    .into_iter()
    .filter(|k| !obj.get(*k).map(|v| v.is_string()).unwrap_or(false))
    .collect();
    if missing.is_empty() {
        checks.push(Check {
            name: "discovery_document_has_required_endpoints",
            status: "PASSED",
            detail: "issuer／authorization_endpoint／token_endpoint／jwks_uri 都在".to_string(),
        });
    } else {
        checks.push(Check {
            name: "discovery_document_has_required_endpoints",
            status: "FAILED",
            detail: format!("缺少：{}", missing.join("、")),
        });
    }

    // **這一格是這支端點真正有價值的部分。**
    //
    // OIDC Discovery §4.3 要求文件裡的 issuer 必須與用來取得它的 issuer 相符。
    // 不符有兩種來源：把設定貼錯（常見），或者那個網址被指向另一個 IdP
    // （那時我們會拿別人的 token 當成自己的使用者）。兩者都必須在
    // 第一個使用者登入之前就被發現。
    match (
        configured_issuer.map(str::trim).filter(|s| !s.is_empty()),
        obj.get("issuer").and_then(|v| v.as_str()),
    ) {
        (Some(cfg), Some(doc_iss)) => {
            // 尾斜線不算差異 —— OIDC 的 issuer 不該有尾斜線，但實務上兩種都見得到。
            if cfg.trim_end_matches('/') == doc_iss.trim_end_matches('/') {
                checks.push(Check {
                    name: "issuer_matches_configuration",
                    status: "PASSED",
                    detail: format!("文件與設定的 issuer 相符：{doc_iss}"),
                });
            } else {
                checks.push(Check {
                    name: "issuer_matches_configuration",
                    status: "FAILED",
                    detail: format!(
                        "設定的 issuer 是 `{cfg}`，文件裡是 `{doc_iss}` —— \
                         OIDC Discovery §4.3 要求兩者相符。\
                         這通常是設定貼錯，也可能表示這個網址指向另一個 IdP"
                    ),
                });
            }
        }
        (None, _) => checks.push(Check {
            name: "issuer_matches_configuration",
            status: "FAILED",
            detail: "設定裡沒有 issuer，無法比對 —— 少了它就沒有辦法驗證 token 的來源".to_string(),
        }),
        (Some(_), None) => { /* 上面的 missing 已經回報過缺 issuer */ }
    }
}

fn render(
    provider_type: &str,
    result: &'static str,
    checks: Vec<Check>,
    not_performed: Vec<NotPerformed>,
    target: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "provider_type": provider_type,
        "result": result,
        "checks": checks,
        // 空陣列也照樣回。少了這個欄位，客戶端會以為 checks 就是全部 ——
        // 而對 LDAP 來說最重要的資訊正是「bind 沒有被驗證」。
        "checks_not_performed": not_performed,
        "target": target,
        "meta": {
            // 這支端點不寫任何東西。說出來，因為「測試連線」聽起來像會更新
            // last_sync_at 或 status 之類的東西。
            "read_only": true,
            "outbound_guard": "強制 https、解析後逐一檢查位址、連線 pin 住已檢查的 IP、不跟隨轉址",
        },
    })
}

/// 一輪同步的結果，不論由 HTTP handler 或背景排程觸發。
pub struct SyncOutcome {
    pub run_id: Uuid,
    pub status: &'static str,
    pub groups_synced: i32,
    pub roles_granted: i32,
    pub roles_revoked: i32,
    pub blocked: Vec<String>,
}

/// 對一個身分來源跑一輪對帳，並把結果寫進 `directory_sync_runs`。
///
/// **HTTP handler 與排程迴圈共用同一份邏輯**，不是各寫一次：兩者的差異只在
/// 「誰觸發、以誰的身分」，對帳本身、被擋對應的判斷、狀態機（SUCCEEDED 或
/// PARTIAL）完全相同，複製只會製造漂移（見 `fms_shared::schedule` 檔頭
/// 同一條 ADR-09 紀律）。
///
/// 呼叫端負責：`tx` 已經是正確的租戶情境、`actor_user_id` 已通過
/// `directory:sync` 的存在性檢查（handler 用 `require_tenant_scoped_permission`；
/// 排程迴圈的服務帳號權限是固定的，見 migration 078）。這支函式**不**重複
/// 那個檢查 —— 它只管一件事：對帳、記錄、回報。
pub async fn run_sync(
    tx: &mut fms_shared::TenantTx,
    provider_id: Uuid,
    actor_user_id: Uuid,
    run_type: &str,
) -> Result<SyncOutcome, Problem> {
    let provider: Option<(bool,)> = sqlx::query_as(
        "SELECT sync_enabled FROM fms.identity_providers
          WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(provider_id)
    .fetch_optional(tx.conn())
    .await?;
    let (sync_enabled,) = provider.ok_or_else(|| Problem::not_found("找不到這個身分來源"))?;

    // `sync_enabled = false` 是管理員刻意關掉的。照樣跑一輪會讓那個開關
    // 看起來沒有作用 —— 而那正是這個專案反覆出現的缺陷類型。
    if !sync_enabled {
        return Err(Problem::validation(
            "這個身分來源的 sync_enabled 是 false —— 先啟用它再同步",
        ));
    }

    let (granted, revoked, groups, blocked): (i32, i32, i32, Vec<String>) = sqlx::query_as(
        "SELECT roles_granted, roles_revoked, groups_synced, blocked_mappings
           FROM fms.reconcile_directory_roles($1, $2)",
    )
    .bind(provider_id)
    .bind(actor_user_id)
    .fetch_one(tx.conn())
    .await?;

    // **被擋的對應讓這一輪是 PARTIAL，不是 SUCCEEDED。**
    // 回 SUCCEEDED 會讓「這條對應設定了但不生效」完全看不見。
    let (status, error_summary) = if blocked.is_empty() {
        ("SUCCEEDED", None)
    } else {
        (
            "PARTIAL",
            Some(format!(
                "以下角色被提權防護擋下（觸發者未持有它們的危險權限）：{}",
                blocked.join("、")
            )),
        )
    };

    let run_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.directory_sync_runs
           (tenant_id, identity_provider_id, run_type, status,
            groups_synced, roles_granted, roles_revoked, error_summary, finished_at)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7, clock_timestamp())
         RETURNING id",
    )
    .bind(provider_id)
    .bind(run_type)
    .bind(status)
    .bind(groups)
    .bind(granted)
    .bind(revoked)
    .bind(error_summary.as_deref())
    .fetch_one(tx.conn())
    .await?;

    sqlx::query("UPDATE fms.identity_providers SET last_sync_at = clock_timestamp() WHERE id = $1")
        .bind(provider_id)
        .execute(tx.conn())
        .await?;

    Ok(SyncOutcome {
        run_id,
        status,
        groups_synced: groups,
        roles_granted: granted,
        roles_revoked: revoked,
        blocked,
    })
}

/// `POST /identity-providers/{providerId}/sync`
///
/// 對帳邏輯在 058、共用實作在 [`run_sync`]。這裡負責：權限、
/// `run_type` 的請求層驗證、交易邊界、把結果轉成回應。
pub async fn sync(
    State(state): State<IdentityProvidersState>,
    caller: Caller,
    Path(provider_id): Path<Uuid>,
    body: Option<Json<SyncBody>>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let run_type = body
        .map(|Json(b)| b)
        .unwrap_or_default()
        .run_type
        .unwrap_or_else(|| "DELTA".to_string())
        .to_uppercase();
    if !["FULL", "DELTA"].contains(&run_type.as_str()) {
        return Err(Problem::validation("run_type 必須是 FULL 或 DELTA"));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "directory:sync").await?;

    let outcome = run_sync(&mut tx, provider_id, caller.user_id, &run_type).await?;
    tx.commit().await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "sync_run_id": outcome.run_id,
            "status": outcome.status,
            "groups_synced": outcome.groups_synced,
            "roles_granted": outcome.roles_granted,
            "roles_revoked": outcome.roles_revoked,
            "blocked_roles": outcome.blocked,
        })),
    ))
}

/// `GET /identity-providers/{providerId}/sync-runs`
pub async fn list_runs(
    State(state): State<IdentityProvidersState>,
    caller: Caller,
    Path(provider_id): Path<Uuid>,
    Query(q): Query<RunsQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "identity_provider:read").await?;

    let exists: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM fms.identity_providers WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(provider_id)
    .fetch_optional(tx.conn())
    .await?;
    exists.ok_or_else(|| Problem::not_found("找不到這個身分來源"))?;

    let rows: Vec<SyncRunDto> = sqlx::query_as(
        "SELECT id, run_type, status, groups_synced, roles_granted, roles_revoked,
                error_summary, started_at, finished_at
           FROM fms.directory_sync_runs
          WHERE identity_provider_id = $1
          ORDER BY started_at DESC
          LIMIT $2",
    )
    .bind(provider_id)
    .bind(clamp_limit(q.limit))
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "items": rows })))
}

/// 擋掉看起來像明文密鑰的 `client_secret_ref`。
///
/// 契約說那個欄位是「密鑰管理服務中的參照名稱」。**只在文件裡寫這句話不夠**
/// —— 第一個整合的人會直接貼密鑰進去，而它會進資料庫、進備份、
/// 進稽核的 `after_data`。
///
/// 判準刻意寬鬆：參照名稱通常短、像路徑或識別碼
/// （`kv/fms/entra-secret`、`arn:aws:secretsmanager:...`）；
/// 而密鑰通常長且高熵。抓不到所有情況，但抓得到最常見的那種貼上。
fn reject_plaintext_secret(v: Option<&str>) -> Result<(), Problem> {
    let Some(s) = v.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let looks_like_ref = s.contains('/') || s.contains(':') || s.starts_with("ref_");
    if s.len() > 40 && !looks_like_ref {
        return Err(Problem::validation(
            "client_secret_ref 是**密鑰管理服務裡的參照名稱**，不是密鑰本身。\
             這個值看起來像明文密鑰（長且不含路徑分隔），而它會進資料庫、\
             備份與稽核紀錄。請填參照，例如 kv/fms/entra-client-secret",
        ));
    }
    Ok(())
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
        Some(c) if c.contains("identity_providers") && c.contains("code") => {
            Problem::new(fms_shared::ProblemCode::Conflict)
                .with_detail("這個租戶已經有同樣 code 的身分來源")
        }
        // 002 的 `uq_identity_providers_default` 是部分唯一索引（WHERE
        // is_default），因此「設第二個預設來源」會撞它。少了這一段會回 500 ——
        // 而正確答案是「先把現在那個取消預設」，那是呼叫端做得到的事。
        Some(c) if c.contains("identity_providers") && c.contains("default") => {
            Problem::new(fms_shared::ProblemCode::Conflict)
                .with_detail("這個租戶已經有一個預設身分來源 —— 請先把現有的 is_default 設為 false")
        }
        // **CHECK 違反是呼叫端的輸入錯誤，不是伺服器故障。**
        // 少了這一段會回 500 —— 而那會讓整合的人去查伺服器日誌，
        // 而答案其實是「你少填了一個欄位」。
        //
        // 上面的驗證應該已經擋掉這些，所以走到這裡代表**驗證與約束漂移了**。
        // 訊息因此指名約束，讓下一個人知道要去對哪一條。
        Some("ck_idp_ldap_fields") => Problem::validation(
            "provider_type = LDAP 必須同時提供 ldap_host 與 ldap_base_dn（ck_idp_ldap_fields）",
        ),
        Some("ck_idp_oidc_fields") => Problem::validation(
            "provider_type = OIDC 必須同時提供 issuer 與 client_id（ck_idp_oidc_fields）",
        ),
        _ => Problem::from(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(checks: &[Check]) -> Vec<(&str, &str)> {
        checks.iter().map(|c| (c.name, c.status)).collect()
    }

    fn status_of(checks: &[Check], name: &str) -> Option<&'static str> {
        checks.iter().find(|c| c.name == name).map(|c| c.status)
    }

    /// 一份完整、issuer 相符的 discovery 文件全部通過。
    #[test]
    fn a_complete_discovery_document_passes() {
        let body = br#"{
            "issuer": "https://login.example.com/v2.0",
            "authorization_endpoint": "https://login.example.com/oauth2/v2.0/authorize",
            "token_endpoint": "https://login.example.com/oauth2/v2.0/token",
            "jwks_uri": "https://login.example.com/discovery/v2.0/keys"
        }"#;
        let mut checks = Vec::new();
        inspect_discovery(body, Some("https://login.example.com/v2.0"), &mut checks);
        assert!(
            checks.iter().all(|c| c.status == "PASSED"),
            "{:?}",
            names(&checks)
        );
    }

    /// **issuer 不符必須是 FAILED。**
    ///
    /// 這是這支端點真正有價值的一格：不符有兩種來源 —— 設定貼錯（常見），
    /// 或那個網址指向另一個 IdP（那時我們會拿別人的 token 當成自己的使用者）。
    /// 兩者都必須在第一個使用者登入之前被發現，而抓得到文件本身不會失敗。
    #[test]
    fn b_issuer_mismatch_fails_even_though_the_document_is_valid() {
        let body = br#"{
            "issuer": "https://login.attacker.example/v2.0",
            "authorization_endpoint": "https://a/authorize",
            "token_endpoint": "https://a/token",
            "jwks_uri": "https://a/keys"
        }"#;
        let mut checks = Vec::new();
        inspect_discovery(body, Some("https://login.example.com/v2.0"), &mut checks);
        assert_eq!(
            status_of(&checks, "discovery_document_has_required_endpoints"),
            Some("PASSED"),
            "文件本身是完整的：{:?}",
            names(&checks)
        );
        assert_eq!(
            status_of(&checks, "issuer_matches_configuration"),
            Some("FAILED"),
            "issuer 不符卻通過了 —— 這條路徑會讓別的 IdP 的 token 被接受：{:?}",
            names(&checks)
        );
    }

    /// 尾斜線不算差異。OIDC 的 issuer 不該有尾斜線，但實務上兩種都見得到，
    /// 而因為一個斜線回 FAILED 只會讓人開始忽略這支端點的輸出。
    #[test]
    fn c_trailing_slash_is_not_a_mismatch() {
        let body = br#"{
            "issuer": "https://login.example.com/v2.0/",
            "authorization_endpoint": "https://a/authorize",
            "token_endpoint": "https://a/token",
            "jwks_uri": "https://a/keys"
        }"#;
        let mut checks = Vec::new();
        inspect_discovery(body, Some("https://login.example.com/v2.0"), &mut checks);
        assert_eq!(
            status_of(&checks, "issuer_matches_configuration"),
            Some("PASSED"),
            "{:?}",
            names(&checks)
        );
    }

    /// 缺端點要指名缺哪一個 —— 「文件不完整」對整合的人沒有幫助。
    #[test]
    fn d_missing_endpoints_are_named() {
        let body = br#"{"issuer": "https://i", "jwks_uri": "https://k"}"#;
        let mut checks = Vec::new();
        inspect_discovery(body, Some("https://i"), &mut checks);
        let c = checks
            .iter()
            .find(|c| c.name == "discovery_document_has_required_endpoints")
            .unwrap();
        assert_eq!(c.status, "FAILED");
        assert!(c.detail.contains("token_endpoint"), "{}", c.detail);
        assert!(c.detail.contains("authorization_endpoint"), "{}", c.detail);
    }

    /// 非 JSON、以及 JSON 但不是物件。
    ///
    /// 這兩種都是真實情境：一個打錯的網址常常回一頁 HTML，
    /// 而那時 `serde_json` 的錯誤訊息會是「expected value at line 1」——
    /// 所以必須有一格明確說「回應不是合法的 JSON」。
    #[test]
    fn e_non_json_and_non_object_bodies_fail_cleanly() {
        for body in [&b"<html>404</html>"[..], &b"[1,2,3]"[..]] {
            let mut checks = Vec::new();
            inspect_discovery(body, Some("https://i"), &mut checks);
            assert_eq!(
                status_of(&checks, "discovery_document_is_json"),
                Some("FAILED"),
                "{:?}",
                names(&checks)
            );
            // 不能因此漏掉其他檢查而讓整體變成「沒有 FAILED」。
            assert!(checks.iter().any(|c| c.status == "FAILED"));
        }
    }

    /// 設定裡沒有 issuer 時**不能**默默跳過那一格。
    ///
    /// 少了 issuer 就沒有辦法驗證 token 的來源，所以它是 FAILED 而不是
    /// 「無法比對，略過」。略過的話一個沒填 issuer 的 OIDC 設定會拿到
    /// `result: PASSED`。
    #[test]
    fn f_missing_configured_issuer_is_a_failure_not_a_skip() {
        let body = br#"{
            "issuer": "https://i",
            "authorization_endpoint": "https://a",
            "token_endpoint": "https://t",
            "jwks_uri": "https://k"
        }"#;
        for configured in [None, Some(""), Some("   ")] {
            let mut checks = Vec::new();
            inspect_discovery(body, configured, &mut checks);
            assert_eq!(
                status_of(&checks, "issuer_matches_configuration"),
                Some("FAILED"),
                "configured = {configured:?} 時沒有回報 FAILED：{:?}",
                names(&checks)
            );
        }
    }
}
