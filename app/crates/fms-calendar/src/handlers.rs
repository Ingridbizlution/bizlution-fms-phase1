//! HTTP 端點（ADR-14 §8）。形狀比照 `fms_tenancy::bim`——註冊、列出、
//! 待審清單、手動對應。
//!
//! # 「待審清單」是即時查出來的，不是儲存的狀態
//!
//! ADR-14 決定 J 原本設想比對不到的外部資源會落地成一列
//! `status='UNRESOLVED'` 的 `calendar_resource_mappings`。但這一輪沒有做
//! 自動比對（BIM 的比對鍵是 manufacturer/model 這種精確值，房間名稱沒有
//! 那麼明確的鍵，硬做名稱比對只會猜錯），因此
//! `calendar_resource_mappings` 只存**已經確定**的對應——`status` 只會是
//! `ACTIVE` 或 `DISABLED`。「待審清單」改成即時算：呼叫
//! `CalendarProvider::list_resources()`，排掉已經有 `ACTIVE` 對應的，
//! 剩下的就是待人工對應的清單。這樣比「先猜測寫一列，猜錯了再讓人改」
//! 更貼近 ADR 決定 J 的核心原則：**不猜測建立**。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_permission, Caller, Cursor, PageMeta, Problem,
    SortSpec,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::provider::{build_provider, ProviderBuildIssue};

#[derive(Clone)]
pub struct CalendarState {
    pub pool: sqlx::PgPool,
    pub ms_app_client_id: String,
    pub ms_app_client_secret: String,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CalendarIntegrationDto {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub facility_id: Option<Uuid>,
    pub provider: String,
    pub status: String,
    pub ms_tenant_id: Option<String>,
    pub sync_cron: String,
    pub last_synced_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_sync_error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const COLUMNS: &str = "id, tenant_id, facility_id, provider::text AS provider, status,
                       ms_tenant_id, sync_cron, last_synced_at, last_sync_error, created_at";

#[derive(Debug, Deserialize)]
pub struct CalendarIntegrationCreate {
    pub provider: Option<String>,
    /// MS365 專用。留空時整合停在 `PENDING_CONSENT`——admin consent 是客戶
    /// 的 M365 系統管理員在 Microsoft 自己的頁面完成，我們拿不到回呼
    /// （沒有做那段 OAuth redirect 的接線，見檔頭），因此租戶 id 由拿到它
    /// 的人（通常是導入時的技術支援）直接填進來。
    pub ms_tenant_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `POST /facilities/{facilityId}/calendar-integrations`
pub async fn register(
    State(state): State<CalendarState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Json(body): Json<CalendarIntegrationCreate>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let provider = body
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_uppercase)
        .ok_or_else(|| Problem::validation("provider 為必填"))?;
    if !matches!(provider.as_str(), "MS365" | "GOOGLE") {
        return Err(Problem::validation("provider 必須是 MS365 或 GOOGLE"));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(
        &mut tx,
        "calendar_integration:write",
        Some(facility_id),
        None,
    )
    .await?;

    let ms_tenant_id = body
        .ms_tenant_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let status = if provider == "MS365" && ms_tenant_id.is_some() {
        "ACTIVE"
    } else {
        "PENDING_CONSENT"
    };

    let row: CalendarIntegrationDto = sqlx::query_as(&format!(
        "INSERT INTO fms.calendar_integrations
           (tenant_id, facility_id, provider, status, ms_tenant_id, created_by)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5)
         RETURNING {COLUMNS}"
    ))
    .bind(facility_id)
    .bind(&provider)
    .bind(status)
    .bind(ms_tenant_id)
    .bind(caller.user_id)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    let mut v = serde_json::to_value(&row)
        .map_err(|e| Problem::internal(std::io::Error::other(e.to_string())))?;
    if row.provider == "MS365" {
        // v2 admin consent 端點——`common`：這個時間點我們還不知道客戶的
        // Azure AD 租戶 id（那正是要透過這個流程取得的東西）。app 本身的
        // redirect URI 在 Azure 應用註冊上設定，這裡不需要帶。
        v["admin_consent_url"] = serde_json::Value::String(format!(
            "https://login.microsoftonline.com/common/adminconsent?client_id={}",
            state.ms_app_client_id
        ));
    }

    Ok((StatusCode::CREATED, Json(v)))
}

/// `GET /facilities/{facilityId}/calendar-integrations`
pub async fn list(
    State(state): State<CalendarState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(
        &mut tx,
        "calendar_integration:read",
        Some(facility_id),
        None,
    )
    .await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "created_at".to_string(),
        desc: true,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.as_timestamp()?), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<CalendarIntegrationDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM fms.calendar_integrations
          WHERE facility_id = $1
            AND ($2::timestamptz IS NULL
                 OR (created_at, id) < ($2::timestamptz, $3::uuid))
          ORDER BY created_at DESC, id DESC
          LIMIT $4"
    ))
    .bind(facility_id)
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.created_at.to_rfc3339(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

/// `GET /calendar-integrations/{integrationId}/unresolved-resources`
///
/// 即時算出來的清單，不是儲存的狀態——見檔頭。
pub async fn unresolved_resources(
    State(state): State<CalendarState>,
    caller: Caller,
    Path(integration_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let integration: Option<(Uuid, String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT facility_id, provider::text, status, ms_tenant_id, id::text
           FROM fms.calendar_integrations WHERE id = $1",
    )
    .bind(integration_id)
    .fetch_optional(tx.conn())
    .await?;
    let (facility_id, provider, status, ms_tenant_id, _id) =
        integration.ok_or_else(|| Problem::not_found("找不到這個日曆整合"))?;

    require_permission(
        &mut tx,
        "calendar_integration:read",
        Some(facility_id),
        None,
    )
    .await?;

    if status != "ACTIVE" {
        tx.commit().await?;
        return Ok(Json(serde_json::json!({
            "data": [],
            "meta": { "reason": "整合尚未 ACTIVE，還沒有可以比對的外部資源清單" },
        })));
    }

    let mapped: Vec<String> = sqlx::query_scalar(
        "SELECT external_resource_id FROM fms.calendar_resource_mappings
          WHERE calendar_integration_id = $1 AND status = 'ACTIVE'",
    )
    .bind(integration_id)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let provider_impl = build_provider(
        &provider,
        ms_tenant_id.as_deref(),
        &state.ms_app_client_id,
        &state.ms_app_client_secret,
        None,
    )
    .map_err(|issue| match issue {
        ProviderBuildIssue::MissingMsTenantId => {
            Problem::validation("整合缺 ms_tenant_id，尚未完成 admin consent")
        }
        ProviderBuildIssue::Unsupported(p) => {
            Problem::validation(format!("provider {p} 還沒有實作"))
        }
    })?;

    let resources = provider_impl.list_resources().await?;
    let unresolved: Vec<_> = resources
        .into_iter()
        .filter(|r| !mapped.iter().any(|m| m == &r.external_id))
        .map(
            |r| serde_json::json!({ "external_id": r.external_id, "display_name": r.display_name }),
        )
        .collect();

    Ok(Json(serde_json::json!({ "data": unresolved })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRow {
    pub external_resource_id: String,
    pub external_resource_name: Option<String>,
    pub spatial_node_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMappingsRequest {
    pub mappings: Vec<MappingRow>,
}

/// `POST /calendar-integrations/{integrationId}/resource-mappings`
pub async fn create_mappings(
    State(state): State<CalendarState>,
    caller: Caller,
    Path(integration_id): Path<Uuid>,
    Json(body): Json<CreateMappingsRequest>,
) -> Result<Json<serde_json::Value>, Problem> {
    if body.mappings.is_empty() {
        return Err(Problem::validation("mappings 不能是空陣列"));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let facility_id: Option<Uuid> =
        sqlx::query_scalar("SELECT facility_id FROM fms.calendar_integrations WHERE id = $1")
            .bind(integration_id)
            .fetch_optional(tx.conn())
            .await?
            .ok_or_else(|| Problem::not_found("找不到這個日曆整合"))?;

    require_permission(&mut tx, "calendar_integration:write", facility_id, None).await?;

    let mut created = Vec::with_capacity(body.mappings.len());
    for m in &body.mappings {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO fms.calendar_resource_mappings
               (tenant_id, calendar_integration_id, spatial_node_id,
                external_resource_id, external_resource_name, status)
             VALUES (fms.current_tenant_id(), $1, $2, $3, $4, 'ACTIVE')
             RETURNING id",
        )
        .bind(integration_id)
        .bind(m.spatial_node_id)
        .bind(&m.external_resource_id)
        .bind(&m.external_resource_name)
        .fetch_one(tx.conn())
        .await
        .map_err(translate)?;
        created.push(id);
    }
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": { "created": created } })))
}

fn translate(err: sqlx::Error) -> Problem {
    match &err {
        sqlx::Error::Database(db) => match db.constraint() {
            Some(c) if c.contains("calendar_integrations_scope") => {
                Problem::new(fms_shared::ProblemCode::Conflict)
                    .with_detail("這個租戶／場域範圍已經有一個相同 provider 的日曆整合")
            }
            Some(c) if c.contains("calendar_resource_mappings_external") => {
                Problem::new(fms_shared::ProblemCode::Conflict)
                    .with_detail("這個外部資源在這個整合裡已經有對應")
            }
            Some(c) if c.contains("calendar_resource_mappings_node") => {
                Problem::new(fms_shared::ProblemCode::Conflict)
                    .with_detail("這個空間節點在這個整合裡已經有生效中的對應")
            }
            Some(c) if c.contains("fk_") || c.contains("spatial_node") => {
                Problem::not_found("找不到這個空間節點（或它不在你的範圍內）")
            }
            _ => Problem::from(err),
        },
        _ => Problem::from(err),
    }
}
