//! 租戶設定（`GET /tenant`、`PATCH /tenant`）。
//!
//! # 這兩支的全部難度在「哪些欄位可以改」
//!
//! `fms.tenants` 有 18 個欄位，而它們依**誰擁有**分成三組。分錯的後果不對稱：
//!
//! | 組 | 欄位 | PATCH |
//! |---|---|---|
//! | 平台方（合約） | `plan_tier`、`isolation_mode`、`status`、`quota_*`、`contract_*` | 拒絕 |
//! | 平台方（授權） | `feature_flags` | 拒絕 |
//! | 身分 | `id`、`code`、`industry` | 拒絕 |
//! | 租戶（偏好） | `name`、`legal_name`、`default_timezone`／`locale`／`currency`、`settings` | 允許 |
//!
//! **`quota_assets` 可寫等於讓客戶自己解除配額上限，`plan_tier` 可寫等於自己
//! 升級方案。** 那兩個欄位的名字看起來就是「設定」，所以這個界線必須寫下來
//! 並且被測試釘住 —— `c_` 那一格逐一嘗試每一個合約欄位。
//!
//! `feature_flags` 也歸平台方：它已經有讀者（`/auth/me` 回傳它），
//! 而讓租戶自己打開一個沒付費的模組是同一類問題。
//!
//! # 拒絕要出聲，不能靜默忽略
//!
//! serde 的預設行為是忽略未知欄位。若 `PATCH {"quota_assets": 99999}` 回 200
//! 而什麼也沒改，客戶會以為改成功了，然後在配額擋住他的時候回來問為什麼設定
//! 沒生效。所以請求體用 `deny_unknown_fields`，而合約欄位**明列**在一份
//! 名單裡，逐一回 422 並指名「這個欄位由平台方管理」。
//!
//! 兩者的差別：未知欄位是打錯字（"不認識這個欄位"），合約欄位是權限問題
//! （"這個欄位存在，但不是你能改的"）。訊息要不一樣。
//!
//! # `settings` 的形狀由資料庫驗
//!
//! 067 的 `ck_tenants_settings` 已經在那裡（只認已知的鍵與型別，未知的放行）。
//! 這裡不再實作第二份驗證 —— 兩份實作最後總會分歧，而這個欄位的鍵會長大。
//! 約束違反轉成 422 並帶原始訊息。

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_tenant_scoped_permission, Caller, FieldError, Problem};

#[derive(Clone)]
pub struct TenantState {
    pub pool: PgPool,
}

/// 只有平台方能改的欄位，附上「為什麼」。
///
/// 明列而不是「白名單以外全拒」的補集：這樣錯誤訊息說得出理由，而
/// 「這個欄位存在但不是你能改的」與「沒有這個欄位」是不同的事。
const PLATFORM_OWNED: &[(&str, &str)] = &[
    ("plan_tier", "方案等級由合約決定"),
    ("isolation_mode", "隔離模式由合約決定"),
    ("status", "租戶狀態由平台方管理"),
    ("quota_api_rps", "配額由合約決定"),
    ("quota_assets", "配額由合約決定"),
    ("quota_users", "配額由合約決定"),
    ("contract_start_date", "合約期間由平台方管理"),
    ("contract_end_date", "合約期間由平台方管理"),
    (
        "feature_flags",
        "功能開關由合約決定 —— 自行開啟未授權的模組不會生效",
    ),
    ("id", "租戶識別碼不可變更"),
    ("code", "租戶代碼不可變更（它是登入時解析租戶的鍵）"),
    ("industry", "產業別決定預設目錄，由平台方在開通時設定"),
];

/// `GET /tenant` 回傳的全部欄位。合約那組也回 —— 讀得到自己的方案與配額是
/// 合理的（那是他付錢買的東西），不能改而已。
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TenantDto {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub legal_name: Option<String>,
    pub industry: String,
    pub default_timezone: String,
    pub default_locale: String,
    pub default_currency: String,
    pub settings: serde_json::Value,
    // ---- 以下唯讀（PATCH 會拒絕）----
    pub isolation_mode: String,
    pub plan_tier: String,
    pub status: String,
    pub feature_flags: serde_json::Value,
    pub contract_start_date: Option<chrono::NaiveDate>,
    pub contract_end_date: Option<chrono::NaiveDate>,
    pub quota_api_rps: i32,
    pub quota_assets: Option<i32>,
    pub quota_users: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// `PATCH /tenant` 的請求體。
///
/// `deny_unknown_fields` + 明列的合約欄位一起用：前者擋打錯字的欄位，
/// 後者讓合約欄位得到一個說得出理由的 422。少了 `deny_unknown_fields`，
/// 一個打錯的 `default_locale2` 會回 200 而什麼也沒改。
///
/// 每個欄位都是 `Option`，但**這裡的 `None` 只代表「沒有提供」**。
/// `legal_name` 想清空要送 `null`，而 serde 分不出「沒提供」與「提供 null」
/// —— 所以那三個可為 NULL 的欄位用 `Option<Option<T>>`（外層是有沒有提供，
/// 內層是值是不是 null）。不這樣做的話，清空一個欄位是做不到的操作。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchTenantRequest {
    pub name: Option<String>,
    #[serde(default, with = "double_option")]
    pub legal_name: Option<Option<String>>,
    pub default_timezone: Option<String>,
    pub default_locale: Option<String>,
    pub default_currency: Option<String>,
    pub settings: Option<serde_json::Value>,
}

/// `Option<Option<T>>` 的 serde 支援：外層區分「有沒有這個鍵」。
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

const SELECT_TENANT: &str = r#"
    SELECT id, code, name::text AS name, legal_name::text AS legal_name,
           industry, default_timezone::text AS default_timezone,
           default_locale::text AS default_locale,
           default_currency::text AS default_currency,
           settings, isolation_mode, plan_tier, status, feature_flags,
           contract_start_date, contract_end_date,
           quota_api_rps, quota_assets, quota_users, created_at, updated_at
      FROM fms.tenants
     WHERE id = fms.current_tenant_id()
"#;

/// `GET /tenant`
///
/// 需要 `tenant:read`（TENANT 範圍 —— 這不是場域層級的資料）。
pub async fn get(
    State(state): State<TenantState>,
    caller: Caller,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "tenant:read").await?;

    let row: TenantDto = sqlx::query_as(SELECT_TENANT)
        .fetch_one(tx.conn())
        .await
        .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": row,
        "meta": {
            // 哪些欄位改不了，以及為什麼。讓前端不必自己維護一份唯讀清單
            // —— 那份清單會與這裡分歧，而症狀是一個永遠回 422 的表單。
            "read_only_fields": PLATFORM_OWNED
                .iter()
                .map(|(f, why)| serde_json::json!({ "field": f, "reason": why }))
                .collect::<Vec<_>>(),
        },
    })))
}

/// `PATCH /tenant`
///
/// 需要 `tenant:update`。只改租戶擁有的欄位；合約欄位回 422 並說明理由。
pub async fn patch(
    State(state): State<TenantState>,
    caller: Caller,
    body: axum::extract::Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, Problem> {
    // 先看原始 JSON，才分得出「合約欄位」與「不認識的欄位」。
    // 直接反序列化成 `PatchTenantRequest` 的話，`deny_unknown_fields` 會把
    // 兩者都變成同一個 400，而客戶端只會知道「有個欄位不對」。
    if let Some(obj) = body.0.as_object() {
        if obj.is_empty() {
            return Err(Problem::validation(
                "沒有要更新的欄位 —— 空的 PATCH 不會有任何效果",
            ));
        }
        let mut errors: Vec<FieldError> = Vec::new();
        for (field, reason) in PLATFORM_OWNED {
            if obj.contains_key(*field) {
                errors.push(FieldError {
                    pointer: format!("/{field}"),
                    code: "PLATFORM_MANAGED".to_string(),
                    message: format!("`{field}` 由平台方管理：{reason}"),
                });
            }
        }
        if !errors.is_empty() {
            // **不靜默忽略。** 回 200 而什麼也沒改，客戶會以為設定生效了，
            // 然後在配額擋住他的時候回來問為什麼。
            return Err(
                Problem::validation("請求包含由平台方管理的欄位，整個請求已被拒絕")
                    .with_errors(errors),
            );
        }
    } else {
        return Err(Problem::validation("請求體必須是一個 JSON 物件"));
    }

    let req: PatchTenantRequest = serde_json::from_value(body.0).map_err(|e| {
        Problem::validation(format!("不認識的欄位或型別不符：{e}")).with_errors(vec![FieldError {
            pointer: "/".to_string(),
            code: "UNKNOWN_FIELD".to_string(),
            message: e.to_string(),
        }])
    })?;

    if let Some(tz) = &req.default_timezone {
        // 時區在寫入前驗：一個打錯的 `Asia/Taipeh` 會讓每一次時間計算
        // 在三層之外失敗，而錯誤訊息不會提到這個設定。
        if tz.is_empty() || tz.len() > 64 {
            return Err(Problem::validation("`default_timezone` 長度不合"));
        }
    }
    if let Some(c) = &req.default_currency {
        if c.len() != 3 || !c.chars().all(|ch| ch.is_ascii_uppercase()) {
            return Err(Problem::validation(
                "`default_currency` 必須是三個大寫字母的 ISO 4217 代碼",
            )
            .with_errors(vec![FieldError {
                pointer: "/default_currency".to_string(),
                code: "FORMAT".to_string(),
                message: format!("`{c}` 不是合法的貨幣代碼"),
            }]));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "tenant:update").await?;

    // `coalesce($n, col)` 讓「沒提供」等於不動。`legal_name` 走
    // `Option<Option<_>>`：外層 None 不動、內層 None 清空，所以那一欄需要
    // 一個額外的旗標參數（coalesce 分不出 NULL 是「不動」還是「清空」）。
    let (legal_name_given, legal_name) = match req.legal_name {
        None => (false, None),
        Some(v) => (true, v),
    };

    // 投影直接寫在 `RETURNING` 裡。
    //
    // **第一版用 `WITH updated AS (UPDATE … RETURNING id) SELECT … JOIN updated`
    // ，回傳的是更新前的值。** 資料修改型 CTE 的效果對同一個查詢的其他部分
    // 不可見（PostgreSQL 手冊 7.8.2：所有 sub-statement 看到的是同一份
    // snapshot）。症狀特別誤導：UPDATE 真的成功了，只有回應是舊的 ——
    // 於是前端顯示「儲存成功」但畫面上是舊值，重新整理才對。
    // `b_` 那一格抓到它。
    let row: TenantDto = sqlx::query_as(
        r#"UPDATE fms.tenants SET
             name             = coalesce($1, name),
             legal_name       = CASE WHEN $2 THEN $3 ELSE legal_name END,
             default_timezone = coalesce($4, default_timezone),
             default_locale   = coalesce($5, default_locale),
             default_currency = coalesce($6, default_currency),
             settings         = coalesce($7, settings),
             updated_at       = clock_timestamp()
           WHERE id = fms.current_tenant_id()
           RETURNING id, code, name::text AS name, legal_name::text AS legal_name,
                     industry, default_timezone::text AS default_timezone,
                     default_locale::text AS default_locale,
                     default_currency::text AS default_currency,
                     settings, isolation_mode, plan_tier, status, feature_flags,
                     contract_start_date, contract_end_date,
                     quota_api_rps, quota_assets, quota_users,
                     created_at, updated_at"#,
    )
    .bind(req.name.as_deref())
    .bind(legal_name_given)
    .bind(legal_name.as_deref())
    .bind(req.default_timezone.as_deref())
    .bind(req.default_locale.as_deref())
    .bind(req.default_currency.as_deref())
    .bind(req.settings.as_ref())
    .fetch_one(tx.conn())
    .await
    .map_err(map_settings_violation)?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": row })))
}

/// 067 的 `ck_tenants_settings` 違反 → 422，而不是 500。
///
/// 那個約束的訊息（`violates check constraint "ck_tenants_settings"`）對客戶端
/// 沒有意義，所以換成說得出「哪個鍵、要什麼型別」的訊息。約束本身留在資料庫
/// 是刻意的（見模組檔頭），這裡只翻譯它。
fn map_settings_violation(e: sqlx::Error) -> Problem {
    let is_settings = e
        .as_database_error()
        .and_then(|d| d.constraint())
        .is_some_and(|c| c == "ck_tenants_settings");
    if is_settings {
        return Problem::validation(
            "`settings` 的形狀不合：已知的鍵型別必須正確 —— \
             `satisfaction_editable_days` 是 0 到 365 的整數",
        )
        .with_errors(vec![FieldError {
            pointer: "/settings".to_string(),
            code: "SHAPE".to_string(),
            message: "見 migration 067 的 fms.tenant_settings_are_valid".to_string(),
        }]);
    }
    Problem::from(e)
}
