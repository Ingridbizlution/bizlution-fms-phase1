//! `GET /facilities/{facilityId}/service-items`

use axum::extract::{Path, Query, State};
use axum::Json;
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_permission, Caller, Cursor, PageMeta, Problem,
    SortSpec,
};

use crate::dto::*;
use crate::repo;

#[derive(Clone)]
pub struct CatalogueState {
    pub pool: PgPool,
}

fn to_dto(r: repo::ServiceItemRow) -> ServiceItemDto {
    ServiceItemDto {
        id: r.id,
        facility_id: r.facility_id,
        category: r.category,
        code: r.code,
        name: r.name,
        description: r.description,
        lead_time_minutes: r.lead_time_minutes,
        default_duration_minutes: r.default_duration_minutes,
        relative_offset_minutes: r.relative_offset_minutes,
        is_attachable_to_reservation: r.is_attachable_to_reservation,
        is_standalone_requestable: r.is_standalone_requestable,
        requires_approval: r.requires_approval,
        chargeable: r.chargeable,
        unit_price: r.unit_price,
        currency: r.currency,
        unit_label: r.unit_label,
        max_quantity: r.max_quantity,
        form_schema: r.form_schema,
        // 兩個欄位來自同一個 LEFT JOIN，因此要嘛都有要嘛都沒有。
        // 只有一個有值代表 sla_policies 的資料不完整 —— 那時寧可不回
        // `sla` 物件，也不要用 0 補另一半（0 會被讀成「零分鐘內必須回應」）。
        sla: match (r.response_minutes, r.resolution_minutes) {
            (Some(response_minutes), Some(resolution_minutes)) => Some(SlaDto {
                response_minutes,
                resolution_minutes,
            }),
            _ => None,
        },
        icon: r.icon,
        display_order: r.display_order,
    }
}

/// `GET /facilities/{facilityId}/service-items`
///
/// 權限用 `service_item:read`，範圍是路徑上的場域 —— 型錄是分場域的
/// （`facility_id IS NULL` 代表全場域適用，見 `repo::list`）。
///
/// 這支端點是**附加服務與獨立服務申請的前置條件**：客戶端要靠它取得
/// `service_item_id` 與 `form_schema` 才能組出表單。在它存在之前，
/// `POST /reservations` 的 `services[]` 與 `POST /work-orders` 的
/// `service_item_id` 對客戶端而言是無法填寫的欄位。
pub async fn list(
    State(state): State<CatalogueState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "service_item:read", Some(facility_id), None).await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: repo::SORT_COLUMN.to_string(),
        desc: false,
    };
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };

    let rows = repo::list(
        &mut tx,
        facility_id,
        q.category.as_deref(),
        q.attachable_to_reservation,
        q.standalone_only,
        cursor.as_ref(),
        limit,
    )
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, col| r.cursor_key(col));
    let data: Vec<ServiceItemDto> = paged.data.into_iter().map(to_dto).collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}
