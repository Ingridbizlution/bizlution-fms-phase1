//! 工單端點（WBS S4）。契約中的六支：
//! `GET/POST /work-orders`、`GET/PATCH /work-orders/{workOrderId}`、
//! `POST /work-orders/{workOrderId}/transitions`、
//! `GET /work-orders/{workOrderId}/available-actions`。
//!
//! # 三段檢查的順序是契約明訂的
//!
//! 契約寫的是「權限 → 必填欄位 → 狀態機合法性，任何一關不通過都不會寫入」。
//! 順序有實質意義：沒有權限的人不該從錯誤訊息裡得知「這個動作缺哪些欄位」，
//! 那等於洩漏狀態機的形狀。因此權限先擋。
//!
//! # 這三關分別由誰執行
//!
//! `fms.transition_work_order()` 只做第三關。它查出了規則列，卻**沒有**讀取
//! 該列的 `required_permission` 與 `required_fields` 兩個欄位 ——
//! 那兩欄在資料庫端是完全惰性的。004 的欄位註解寫
//! 「Fields the API must supply for this action」與
//! 「Declarative effects executed by the service layer」，
//! 也就是設計上本來就把這兩關交給應用層。本模組補上它們。
//!
//! 代價要說清楚：任何**不經本 API** 的呼叫者（例如日後的 PM 產單器直接呼叫
//! `transition_work_order`）都會繞過權限與必填欄位檢查。若要讓那條路徑也安全，
//! 正確做法是把檢查下移到 SQL 函式，而不是在每個呼叫端重複實作。

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, concurrency, deny_unless_own, fields, include, page,
    permission_codes, read_scope, require_permission, Caller, Cursor, FieldError, PageMeta,
    Problem, ProblemCode, SortSpec, TenantTx,
};

use crate::dto::*;
use crate::repo;

#[derive(Clone)]
pub struct WorkOrderState {
    pub pool: PgPool,
    /// 展開 `include=attachments` 需要它預簽下載網址。
    pub storage: fms_shared::Storage,
}

const ENDPOINT: &str = "POST /work-orders";

/// `fields` 允許的欄位 = 契約 `WorkOrder` schema 宣告的全部欄位。
const WO_FIELDS: &[&str] = &[
    "id",
    "wo_no",
    "facility_id",
    "work_order_type",
    "source",
    "title",
    "description",
    "status",
    "status_category",
    "priority",
    "asset",
    "location",
    "service_item_id",
    "reservation_id",
    "alarm_id",
    "requester",
    "assignee",
    "team_id",
    "payload",
    "scheduled_start_at",
    "scheduled_end_at",
    "actual_start_at",
    "actual_end_at",
    "response_due_at",
    "resolution_due_at",
    "sla_state",
    "labor_minutes",
    "total_cost",
    "satisfaction_score",
    "version",
    "created_at",
    "updated_at",
    "completed_at",
];

const WO_SORTABLE: &[&str] = &["created_at", "wo_no", "priority"];

/// `work_orders.work_order_type` 的 CHECK 允許值。在應用層再擋一次的理由
/// 與 `fms-asset::validate_enums` 相同：`query!` 不驗證 CHECK 的字串值，
/// 不先擋就會把客戶端的錯字變成 500。
const WO_TYPES: &[&str] = &[
    "MAINTENANCE",
    "SERVICE",
    "INSPECTION",
    "CORRECTIVE",
    "PROJECT",
];
const WO_PRIORITIES: &[&str] = &["LOW", "MEDIUM", "HIGH", "URGENT", "CRITICAL"];
const WO_STATUS_CATEGORIES: &[&str] = &["OPEN", "IN_PROGRESS", "WAITING", "TERMINAL"];

/// 供其他模組嵌入 `WorkOrder` 用（契約的 `AssetDetail.open_work_orders`）。
///
/// 匯出這一支而不是匯出 `to_dto` + `repo`，是為了讓「什麼算未結工單」
/// 只有一個定義。資產模組的 `open_work_order_count` 與這裡的清單若各自
/// 寫一份 status 排除清單，就會出現「count 是 2 但陣列有 3 筆」。
pub async fn open_work_orders_for_asset(
    tx: &mut TenantTx,
    asset_id: Uuid,
) -> Result<Vec<WorkOrderDto>, Problem> {
    let ids = repo::open_for_asset(tx, asset_id).await?;
    Ok(repo::by_ids(tx, &ids)
        .await?
        .into_iter()
        .map(to_dto)
        .collect())
}

fn to_dto(r: repo::WorkOrderRow) -> WorkOrderDto {
    // asset / location / requester / assignee 只在 JOIN 真的帶回名稱時才組出
    // 物件。id 有值而名稱為 NULL 代表被參照的列已不可見（軟刪除或 RLS），
    // 此時回 null 比回一個沒有名字的物件誠實。
    let asset = match (r.asset_id, r.asset_code, r.asset_name) {
        (Some(id), Some(asset_code), Some(name)) => Some(AssetRefDto {
            id,
            asset_code,
            name,
        }),
        _ => None,
    };
    let location = match (r.spatial_node_id, r.node_name) {
        (Some(spatial_node_id), Some(name)) => Some(LocationDto {
            spatial_node_id,
            name,
            node_path: r.node_path,
        }),
        _ => None,
    };
    let requester = match (r.requester_id, r.requester_name) {
        (Some(id), Some(display_name)) => Some(UserRefDto { id, display_name }),
        _ => None,
    };
    let assignee = match (r.assignee_id, r.assignee_name) {
        (Some(id), Some(display_name)) => Some(UserRefDto { id, display_name }),
        _ => None,
    };

    WorkOrderDto {
        id: r.id,
        wo_no: r.wo_no,
        facility_id: r.facility_id,
        work_order_type: r.work_order_type,
        source: r.source,
        title: r.title,
        description: r.description,
        status: r.status,
        status_category: r.status_category,
        priority: r.priority,
        asset,
        location,
        service_item_id: r.service_item_id,
        reservation_id: r.reservation_id,
        alarm_id: r.alarm_id,
        requester,
        assignee,
        team_id: r.team_id,
        payload: r.payload,
        scheduled_start_at: r.scheduled_start_at,
        scheduled_end_at: r.scheduled_end_at,
        actual_start_at: r.actual_start_at,
        actual_end_at: r.actual_end_at,
        response_due_at: r.response_due_at,
        resolution_due_at: r.resolution_due_at,
        sla_state: r.sla_state,
        labor_minutes: r.labor_minutes,
        total_cost: r.total_cost,
        satisfaction_score: r.satisfaction_score,
        version: r.version,
        created_at: r.created_at,
        updated_at: r.updated_at,
        completed_at: r.completed_at,
    }
}

fn etag_of(version: i32) -> Result<HeaderMap, Problem> {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::ETAG,
        format!("\"{version}\"")
            .parse()
            .map_err(|_| Problem::internal(std::io::Error::other("bad etag")))?,
    );
    Ok(headers)
}

/// `GET /work-orders`
pub async fn list(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let sort = SortSpec::parse(q.sort.as_deref(), WO_SORTABLE, "created_at", true)?;
    let projection = fields::parse(q.fields.as_deref(), WO_FIELDS)?;
    let caller_user_id = caller.user_id;

    if let Some(c) = q.status_category.as_deref() {
        if !WO_STATUS_CATEGORIES.contains(&c) {
            return Err(Problem::validation(format!(
                "invalid status_category `{c}`; allowed: {WO_STATUS_CATEGORIES:?}"
            )));
        }
    }
    // 契約的 status 是逗號分隔多值。空字串視為未指定，而不是「符合空清單」
    // ——後者會回零筆，看起來像沒有資料。
    let statuses: Option<Vec<String>> = q.status.as_deref().and_then(|raw| {
        let v: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        (!v.is_empty()).then_some(v)
    });

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // read_own 的角色（REQUESTER／TECHNICIAN／SERVICE_STAFF）只看得到與自己
    // 相關的列。實作方式刻意重用契約既有的 `mine` 過濾條件，而不是另加一組
    // SQL 分支 —— 兩者要表達的是同一件事，分成兩份遲早會不一致。
    let scope = read_scope(
        &mut tx,
        "work_order:read",
        "work_order:read_own",
        q.facility_id,
    )
    .await?;

    let limit = clamp_limit(q.limit);
    let cursor = match q.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw, &sort.column)?),
        None => None,
    };

    let filter = repo::ListFilter {
        facility_id: q.facility_id,
        work_order_type: q.work_order_type.as_deref(),
        statuses,
        status_category: q.status_category.as_deref(),
        priority: q.priority.as_deref(),
        assignee_id: q.assignee_id,
        team_id: q.team_id,
        asset_id: q.asset_id,
        spatial_node_id: q.spatial_node_id,
        source: q.source.as_deref(),
        sla_state: q.sla_state.as_deref(),
        // 客戶端要求 mine，或權限只允許 own —— 後者不可被客戶端關掉。
        mine_user_id: scope
            .own_user_id()
            .or_else(|| q.mine.unwrap_or(false).then_some(caller_user_id)),
        created_from: q.created_from,
        created_to: q.created_to,
    };

    let rows = repo::list(&mut tx, &filter, cursor.as_ref(), &sort, limit).await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, col| r.cursor_key(col));
    let data: Vec<serde_json::Value> = paged
        .data
        .into_iter()
        .map(|r| serde_json::to_value(to_dto(r)).unwrap_or(serde_json::Value::Null))
        .collect();

    Ok(Json(serde_json::json!({
        "data": fields::project_all(data, &projection),
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

/// `include` 目前真能提供的關聯。
const WO_INCLUDES: &[&str] = &[
    "transitions",
    "attachments",
    "tasks",
    "comments",
    "parts",
    "labor",
];

/// 契約 `include` 列出但尚未提供的關聯。
/// 契約 `include` 列出但尚未提供的關聯。目前為空 —— 六個值都已實作。
/// 保留機制供後續模組使用（回 422 並附原因，比空陣列誠實）。
const WO_INCLUDES_DEFERRED: &[(&str, &str)] = &[];

/// `GET /work-orders/{workOrderId}`
pub async fn get(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Query(q): Query<GetQuery>,
) -> Result<(HeaderMap, Json<serde_json::Value>), Problem> {
    let includes = include::parse(q.include.as_deref(), WO_INCLUDES, WO_INCLUDES_DEFERRED)?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let row = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("work order not found"))?;
    let scope = read_scope(
        &mut tx,
        "work_order:read",
        "work_order:read_own",
        Some(row.facility_id),
    )
    .await?;
    // 「相關」= 申請人或負責人。與 `list` 的 `mine` 過濾條件同一個定義。
    deny_unless_own(scope, &[row.requester_id, row.assignee_id], "work order")?;

    let version = row.version;
    let mut body = serde_json::to_value(to_dto(row)).map_err(Problem::internal)?;

    if includes.has("parts") {
        let parts: Vec<UsedPartDto> = repo::used_parts(&mut tx, id)
            .await?
            .into_iter()
            .map(|p| UsedPartDto {
                part_code: p.part_code,
                name: p.name,
                quantity_used: p.quantity_used,
                total_cost: p.total_cost,
            })
            .collect();
        let Some(obj) = body.as_object_mut() else {
            return Err(Problem::internal(std::io::Error::other(
                "work order serialised to a non-object",
            )));
        };
        obj.insert(
            "parts".into(),
            serde_json::to_value(parts).map_err(Problem::internal)?,
        );
    }

    if includes.has("labor") {
        let labor: Vec<LaborDto> = repo::labor(&mut tx, id)
            .await?
            .into_iter()
            .map(|l| LaborDto {
                user_name: l.user_name,
                started_at: l.started_at,
                ended_at: l.ended_at,
                minutes: l.minutes,
                cost: l.cost,
                is_overtime: l.is_overtime,
            })
            .collect();
        let Some(obj) = body.as_object_mut() else {
            return Err(Problem::internal(std::io::Error::other(
                "work order serialised to a non-object",
            )));
        };
        obj.insert(
            "labor".into(),
            serde_json::to_value(labor).map_err(Problem::internal)?,
        );
    }

    if includes.has("tasks") {
        let tasks: Vec<TaskDto> = repo::tasks(&mut tx, id)
            .await?
            .into_iter()
            .map(task_to_dto)
            .collect();
        let Some(obj) = body.as_object_mut() else {
            return Err(Problem::internal(std::io::Error::other(
                "work order serialised to a non-object",
            )));
        };
        obj.insert(
            "tasks".into(),
            serde_json::to_value(tasks).map_err(Problem::internal)?,
        );
    }

    if includes.has("comments") {
        let comments: Vec<CommentDto> = repo::comments(&mut tx, id)
            .await?
            .into_iter()
            .map(|c| CommentDto {
                id: c.id,
                author_name: c.author_name,
                visibility: c.visibility,
                body: c.body,
                created_at: c.created_at,
            })
            .collect();
        let Some(obj) = body.as_object_mut() else {
            return Err(Problem::internal(std::io::Error::other(
                "work order serialised to a non-object",
            )));
        };
        obj.insert(
            "comments".into(),
            serde_json::to_value(comments).map_err(Problem::internal)?,
        );
    }

    if includes.has("attachments") {
        let files =
            fms_attachment::handlers::for_entity(&mut tx, &state.storage, "WORK_ORDER", id).await?;
        let Some(obj) = body.as_object_mut() else {
            return Err(Problem::internal(std::io::Error::other(
                "work order serialised to a non-object",
            )));
        };
        obj.insert(
            "attachments".into(),
            serde_json::to_value(files).map_err(Problem::internal)?,
        );
    }

    if includes.has("transitions") {
        let log: Vec<TransitionLogDto> = repo::transition_log(&mut tx, id)
            .await?
            .into_iter()
            .map(|t| TransitionLogDto {
                from_status: t.from_status,
                action: t.action,
                to_status: t.to_status,
                actor_name: t.actor_name,
                reason: t.reason,
                occurred_at: t.occurred_at,
            })
            .collect();
        let Some(obj) = body.as_object_mut() else {
            return Err(Problem::internal(std::io::Error::other(
                "work order serialised to a non-object",
            )));
        };
        obj.insert(
            "transitions".into(),
            serde_json::to_value(log).map_err(Problem::internal)?,
        );
    }
    tx.commit().await?;

    Ok((etag_of(version)?, Json(body)))
}

/// 以 `service_items.form_schema` 驗證 `payload`。
///
/// 契約要求「驗證失敗回 422 並在 `errors[]` 指出違規的 JSON Pointer」，
/// 這正好對應 `jsonschema` 的 `instance_path`。schema 存在資料庫裡
/// （租戶可自訂表單），因此每次請求都要編譯一次 —— 對 Phase 1 的量可接受；
/// 若成為瓶頸，正確解法是按 `service_item_id` + 版本快取編譯結果。
/// `form_schema` 驗證已搬到 `fms_shared::form_schema` —— 預約的附加服務要驗的是
/// 同一份 schema、同一個語意，兩處各寫一份遲早會出現「同樣的 payload 在工單
/// 被接受、在預約被拒」。
fn validate_payload(
    schema: &serde_json::Value,
    payload: &serde_json::Value,
) -> Result<(), Problem> {
    fms_shared::form_schema::validate(schema, payload, "/payload")
}

/// `POST /work-orders`
pub async fn create(
    State(state): State<WorkOrderState>,
    caller: Caller,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let w: WorkOrderCreate = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid WorkOrderCreate: {e}")))?;

    // 契約的 required: [facility_id, work_order_type, title]
    let facility_id = w
        .facility_id
        .ok_or_else(|| Problem::validation("facility_id is required"))?;
    let work_order_type = w
        .work_order_type
        .as_deref()
        .ok_or_else(|| Problem::validation("work_order_type is required"))?;
    let title = w
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| Problem::validation("title is required"))?;

    if !WO_TYPES.contains(&work_order_type) {
        return Err(Problem::validation(format!(
            "invalid work_order_type `{work_order_type}`; allowed: {WO_TYPES:?}"
        )));
    }
    if let Some(p) = w.priority.as_deref() {
        if !WO_PRIORITIES.contains(&p) {
            return Err(Problem::validation(format!(
                "invalid priority `{p}`; allowed: {WO_PRIORITIES:?}"
            )));
        }
    }
    // ck_wo_target：至少要有 asset 或 spatial_node 之一。先擋才不會變成 500。
    if w.asset_id.is_none() && w.spatial_node_id.is_none() {
        return Err(Problem::validation(
            "either asset_id or spatial_node_id must be supplied",
        ));
    }
    // ck_wo_service_item
    if work_order_type == "SERVICE" && w.service_item_id.is_none() {
        return Err(Problem::validation(
            "service_item_id is required when work_order_type is SERVICE",
        ));
    }

    let idem_key = concurrency::key_from(&headers)?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // 冪等**登記**在最前面；**回放**必須等授權跑完
    // （見 docs/security-review-open-items.md 第 1 項與 PendingReplay）。
    let pending = match idem_key.as_deref() {
        Some(key) => concurrency::begin(&mut tx, key, ENDPOINT, &raw)
            .await?
            .pending(),
        None => None,
    };

    let auth = require_permission(&mut tx, "work_order:create", Some(facility_id), None).await?;

    if let Some(replay) = pending {
        let (code, body) = replay.release(auth);
        tx.commit().await?;
        return Ok((code, Json(body)));
    }

    // SERVICE 類工單的 payload 必須符合服務項目自訂的表單 schema。
    if let Some(item_id) = w.service_item_id {
        let schema = repo::service_item_form_schema(&mut tx, item_id)
            .await?
            .ok_or_else(|| Problem::validation(format!("unknown service_item_id: {item_id}")))?;
        let payload = w.payload.clone().unwrap_or(serde_json::json!({}));
        validate_payload(&schema, &payload)?;
    }

    // 契約：預設 SUBMITTED，as_draft=true 時 DRAFT。
    let status = if w.as_draft { "DRAFT" } else { "SUBMITTED" };

    let id = repo::create(
        &mut tx,
        repo::NewWorkOrder {
            facility_id,
            work_order_type,
            title,
            description: w.description.as_deref(),
            asset_id: w.asset_id,
            spatial_node_id: w.spatial_node_id,
            service_item_id: w.service_item_id,
            reservation_id: w.reservation_id,
            priority: w.priority.as_deref(),
            requested_start_at: w.requested_start_at,
            payload: w.payload.as_ref(),
            team_id: w.team_id,
            assignee_id: w.assignee_id,
            status,
            // 經 REST 建立：provenance 是 API
            source: "API",
            maintenance_plan_id: None,
            maintenance_occurrence_id: None,
        },
    )
    .await?;

    let created = repo::get(&mut tx, id).await?.ok_or_else(|| {
        Problem::internal(std::io::Error::other("work order vanished after insert"))
    })?;
    let body = serde_json::to_value(to_dto(created)).map_err(Problem::internal)?;

    if let Some(key) = idem_key.as_deref() {
        concurrency::complete(&mut tx, key, ENDPOINT, 201, &body).await?;
    }
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(body)))
}

/// `PATCH /work-orders/{workOrderId}`；`If-Match` 必填。
///
/// 契約明訂本端點不含 `status` —— `WorkOrderUpdate` 沒有那個欄位，
/// 因此送 `{"status": "CLOSED"}` 會被 serde 忽略，狀態不會變。
/// 即使繞過本層直接 UPDATE，004 的 `trg_enforce_wo_transition` 也會擋下。
pub async fn update(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(u): Json<WorkOrderUpdate>,
) -> Result<Json<WorkOrderDto>, Problem> {
    let expected_version = concurrency::required_if_match(&headers)?;
    if let Some(p) = u.priority.as_deref() {
        if !WO_PRIORITIES.contains(&p) {
            return Err(Problem::validation(format!(
                "invalid priority `{p}`; allowed: {WO_PRIORITIES:?}"
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // **先鎖，再讀。** 讀出來的 `version` 要用來比對，而沒有鎖的讀取會讓
    // 兩個並發的 PATCH 讀到同一個版本、都通過比對、都寫入（lost update）。
    // 見 `concurrency::check_version` 的說明與 `concurrency_correctness_slice.rs` 的 `d_`。
    repo::lock(&mut tx, id).await?;
    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("work order not found"))?;
    require_permission(
        &mut tx,
        "work_order:update",
        Some(current.facility_id),
        None,
    )
    .await?;
    concurrency::check_version(expected_version, current.version)?;

    repo::update(&mut tx, id, &u).await?;
    let updated = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("work order not found"))?;
    tx.commit().await?;

    Ok(Json(to_dto(updated)))
}

/// 檢查規則的 `required_fields` 是否都已滿足。
///
/// 「滿足」= 出現在請求 body 裡，**或**工單上已有值。後者不可省：
/// 平台預設把 `title` 列為 SUBMIT 的必填，而 `title` 在建立時就寫好了、
/// `WorkOrderTransitionRequest` 裡也沒有這個欄位，只看 body 會讓
/// SUBMIT 永遠回 422。
///
/// body 的存在性用原始 JSON 判斷而非具型別欄位：租戶可以在
/// `required_fields` 裡放任何欄位名，硬編一份對照表會讓自訂規則
/// 靜默失效。`null` 不算有值 —— 明確傳 null 是「清空」而不是「提供」。
fn check_required_fields(
    action: &str,
    required: &[String],
    raw_body: &serde_json::Value,
    current: &repo::WorkOrderRow,
) -> Result<(), Problem> {
    let missing: Vec<&String> = required
        .iter()
        .filter(|f| {
            let in_body = raw_body.get(f.as_str()).is_some_and(|v| !v.is_null());
            !in_body && !current.has_value_for(f)
        })
        .collect();

    if missing.is_empty() {
        return Ok(());
    }
    Err(Problem::validation(format!(
        "action `{action}` requires {missing:?}; supply them in the request body"
    ))
    .with_errors(
        missing
            .into_iter()
            .map(|f| FieldError {
                pointer: format!("/{f}"),
                code: "REQUIRED".to_string(),
                message: format!("`{f}` is required for action `{action}`"),
            })
            .collect(),
    ))
}

/// 執行規則宣告的、資料庫函式沒有實作的副作用。
///
/// 只做語意明確的。其餘的 key 需要尚不存在的模組，
/// 因此**不假裝**執行（見本檔頂部與 docs/WBS-rebaseline.md 的清單）：
///   * `notify` —— 通知模組（006 有表，無派送器）
///   * `compute_sla` —— 全 schema 沒有任何 SLA 計算函式
///   * `update_asset_status` —— 「結案後把設備改回什麼狀態」沒有規則可循
///   * `release_reservation_step` —— 同樣缺模組
///
/// `request_satisfaction` 從 migration 067 起**真的執行**了。在那之前它與上面
/// 那幾個一樣只是宣告，而症狀特別安靜：每次結案都宣告要請申請人評分，
/// 而沒有人收到邀請、`satisfaction_score` 從 004 至今一直是 NULL。
///
/// 與轉換在同一個交易內，因此副作用與狀態變更一起成功或一起回滾。
/// 狀態機轉換的核心：三關（權限 → 必填欄位 → 狀態機）+ 副作用。
///
/// `POST /work-orders/{id}/transitions` 與 `POST /work-orders:bulk-transition`
/// **共用這一個函式**。分成兩份實作的話，最可能分歧的地方正是權限那一關 ——
/// 而分歧的後果是「批次是繞過個別權限的後門」。
///
/// 呼叫端負責自己那一層：單筆的 If-Match 與工時／料件明細，批次的 savepoint。
/// `raw` 是原始請求體（批次時是 `fields` 那個物件）—— `check_required_fields`
/// 需要它才能指出缺哪個欄位的 JSON Pointer。
pub(crate) async fn transition_one(
    tx: &mut TenantTx,
    id: Uuid,
    action: &str,
    raw: &serde_json::Value,
) -> Result<String, Problem> {
    let current = repo::get(tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("work order not found"))?;

    // 先取規則：權限碼寫在規則裡，因此必須先找到規則才知道要檢查什麼權限。
    // 找不到規則就是動作不合法 —— 這一步不洩漏任何欄位資訊，
    // 而且無論有無權限都是同樣的答案，所以放在權限檢查之前不會洩漏狀態機形狀。
    let rule = repo::matched_rule(tx, id, action).await?.ok_or_else(|| {
        Problem::new(ProblemCode::WorkOrderIllegalTransition).with_detail(format!(
            "action {action} is not allowed from status {}",
            current.status
        ))
    })?;

    // 1) 權限
    match rule.required_permission.as_deref() {
        Some(perm) => {
            require_permission(tx, perm, Some(current.facility_id), None).await?;
        }
        // NULL 代表系統驅動的動作（AUTO_ASSIGN、BREACH_SLA）。
        // 沒有權限碼不等於「任何人都能做」—— 那是給排程器用的，
        // 不該從對外 API 觸發。
        None => {
            return Err(Problem::permission_denied(format!(
                "action {action} is system-driven and cannot be invoked through the API"
            )))
        }
    }

    // 2) 必填欄位
    check_required_fields(action, &rule.required_fields, raw, &current)?;

    // 3) 狀態機。先把 body 帶來的欄位寫進工單，函式的副作用才會作用在正確的
    //    資料上（例如 ASSIGN 之後 assignee_id 必須已經是新的負責人）。
    let req: TransitionRequest = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid WorkOrderTransitionRequest: {e}")))?;
    repo::apply_transition_fields(tx, id, &req).await?;
    let metadata = req.metadata.clone().unwrap_or(serde_json::json!({}));
    let new_status = repo::transition(tx, id, action, req.reason.as_deref(), &metadata).await?;
    debug_assert_eq!(new_status, rule.to_status, "規則與函式選到了不同的規則列");

    apply_side_effects(tx, id, &rule.side_effects).await?;
    Ok(new_status)
}

/// 這個函式**真的會執行**的 side effect key。
///
/// 單一來源：`GET /work-order-state-machine` 直接回報這份清單，好讓前端畫流程
/// 圖時分得出「規則宣告了」與「系統真的會做」。008 的資料宣告了七個 key，
/// 而這裡只有三個 —— 少了這個區分，一份看起來完整的流程圖會把惰性的宣告
/// 呈現成實際行為。
pub const EXECUTED_SIDE_EFFECTS: &[&str] = &[
    "increment_reopen",
    "release_assignee",
    "request_satisfaction",
];

async fn apply_side_effects(
    tx: &mut TenantTx,
    id: Uuid,
    side_effects: &serde_json::Value,
) -> Result<(), Problem> {
    let flag = |key: &str| side_effects.get(key).and_then(|v| v.as_bool()) == Some(true);

    // 走 `EXECUTED_SIDE_EFFECTS` 而不是各寫一個 `if`：那份清單是對外回報的
    // 同一份，所以「清單說會做但實際沒做」在這裡會變成一個大聲的 panic
    // 而不是一個安靜的差異。
    for key in EXECUTED_SIDE_EFFECTS {
        if !flag(key) {
            continue;
        }
        match *key {
            "increment_reopen" => repo::increment_reopen(tx, id).await?,
            "release_assignee" => repo::release_assignee(tx, id).await?,
            "request_satisfaction" => apply_request_satisfaction(tx, id).await?,
            other => unreachable!(
                "EXECUTED_SIDE_EFFECTS 列了 `{other}` 但 apply_side_effects 沒有實作它 \
                 —— 對外回報的清單與實際行為分歧了"
            ),
        }
    }
    Ok(())
}

async fn apply_request_satisfaction(tx: &mut TenantTx, id: Uuid) -> Result<(), Problem> {
    {
        // 回傳建立的通知筆數。0 有三種原因（無範本／工單無申請人／已邀請過），
        // 全都不是錯誤 —— 一封邀請信發不出去不該讓結案失敗。
        //
        // 但「無範本」值得記一筆：那代表這個租戶把平台範本停用了，
        // 而後果是他的申請人永遠不知道可以評分。與 041／059 同一個判斷。
        let n = repo::request_satisfaction(tx, id).await?;
        if n == 0 {
            tracing::debug!(
                work_order_id = %id,
                "結案宣告了 request_satisfaction 但沒有建立通知（無範本／無申請人／已邀請過）"
            );
        }
    }
    Ok(())
}

/// `POST /work-orders/{workOrderId}/transitions`
///
/// 檢查順序即契約順序：權限 → 必填欄位 → 狀態機。
///
/// 動作在當前狀態下不存在時回 **409**（`WORK_ORDER_ILLEGAL_TRANSITION`）
/// 而非 404 或 422：資源存在、請求格式也對，是**當前狀態**讓這個動作不合法。
/// 同一個請求重送永遠會得到同樣結果，客戶端該做的是重新拉
/// `available-actions`，而 409 正是這個語意。
pub async fn transition(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> Result<Json<WorkOrderDto>, Problem> {
    let req: TransitionRequest = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid WorkOrderTransitionRequest: {e}")))?;
    let action = req
        .action
        .as_deref()
        .filter(|a| !a.trim().is_empty())
        .ok_or_else(|| Problem::validation("action is required"))?
        .to_string();

    // If-Match 是選用的（契約列為 parameters 但非 required）。有帶就檢查 ——
    // 帶了卻不檢查等於默默忽略客戶端的並行保護。
    let expected_version = concurrency::optional_if_match(&headers)?;

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 狀態轉移同樣要鎖：`If-Match` 是選填的，但**帶了就會比對**，
    // 而比對沒有鎖就沒有原子性。而且轉移本身也是讀-改-寫。
    repo::lock(&mut tx, id).await?;
    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("work order not found"))?;
    if let Some(expected) = expected_version {
        concurrency::check_version(expected, current.version)?;
    }

    // 三關 + 狀態機 + 副作用共用 `transition_one` —— `:bulk-transition` 走
    // 同一個函式，所以「批次繞過了個別權限」這種分歧在結構上不可能發生。
    transition_one(&mut tx, id, &action, &raw).await?;

    // 工時與料件明細：契約把它們掛在 transition 請求上（COMPLETE 帶
    // labor_minutes 與 parts_used），因此在轉換成功後才寫入 ——
    // 轉換被狀態機拒絕時不該留下領料紀錄。同一個交易，因此仍是原子的。
    if let Some(minutes) = req.labor_minutes.filter(|m| *m > 0) {
        repo::record_labor(&mut tx, id, minutes).await?;
    }
    for usage in req.parts_used.iter().flatten() {
        let part_id = usage
            .part_id
            .ok_or_else(|| Problem::validation("parts_used[].part_id is required"))?;
        let quantity = usage
            .quantity
            .filter(|q| *q > 0.0)
            .ok_or_else(|| Problem::validation("parts_used[].quantity must be positive"))?;
        let issued =
            repo::record_part_usage(&mut tx, id, current.facility_id, part_id, quantity).await?;
        if !issued {
            // 該場域有這個料件的庫存但不足 —— 409 而非 422：
            // 請求本身合法，是**當前庫存**讓它不可行，補貨後重試就會成功。
            return Err(Problem::new(ProblemCode::Conflict).with_detail(format!(
                "insufficient stock for part {part_id} at this facility"
            )));
        }
    }
    if req.labor_minutes.is_some() || req.parts_used.is_some() {
        repo::recompute_costs(&mut tx, id).await?;
    }

    let updated = repo::get(&mut tx, id).await?.ok_or_else(|| {
        Problem::internal(std::io::Error::other(
            "work order vanished after transition",
        ))
    })?;
    tx.commit().await?;

    Ok(Json(to_dto(updated)))
}

/// `GET /work-orders/{workOrderId}/available-actions`
///
/// 不合權限的動作仍然列出、只是 `permitted=false`。這是刻意的：
/// 把它們整個藏起來，使用者只會看到「按鈕不見了」而不知道原因，
/// 前端也沒辦法顯示「需要 X 權限」的提示。
///
/// 系統驅動的動作（`required_permission IS NULL`，例如 `BREACH_SLA`）
/// 一律 `permitted=false`，與 `transition` 的行為一致 ——
/// 列出來卻按不動，好過列出來按了得到 403。
pub async fn available_actions(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let current = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("work order not found"))?;
    let scope = read_scope(
        &mut tx,
        "work_order:read",
        "work_order:read_own",
        Some(current.facility_id),
    )
    .await?;
    deny_unless_own(
        scope,
        &[current.requester_id, current.assignee_id],
        "work order",
    )?;

    let rules = repo::available_actions(&mut tx, id).await?;

    // 一次取回整組權限，而不是每個動作問一次。原本的迴圈是 6 次往返
    // （示範資料下 SUBMITTED 有 6 個動作），而且每次都重掃同一個 view。
    let codes = permission_codes(&mut tx, Some(current.facility_id), None)
        .await?
        .clone();

    let mut out: Vec<AvailableActionDto> = Vec::with_capacity(rules.len());
    for r in rules {
        let permitted = match r.required_permission.as_deref() {
            Some(perm) => codes.contains(perm),
            None => false,
        };
        out.push(AvailableActionDto {
            action: r.action,
            to_status: r.to_status,
            label_zh: r.label_zh,
            required_fields: r.required_fields,
            permitted,
        });
    }
    tx.commit().await?;

    Ok(Json(serde_json::json!({ "data": out })))
}

// =============================================================================
// 工單子資源：檢查表與留言
// =============================================================================

/// `work_order_comments.visibility` 的 CHECK 允許值。
const COMMENT_VISIBILITY: &[&str] = &["INTERNAL", "REQUESTER_VISIBLE", "PUBLIC"];

fn task_to_dto(t: repo::TaskRow) -> TaskDto {
    TaskDto {
        id: t.id,
        seq: t.seq,
        title: t.title,
        input_type: t.input_type,
        unit: t.unit,
        min_value: t.min_value,
        max_value: t.max_value,
        is_required: t.is_required,
        result_value: t.result_value,
        is_pass: t.is_pass,
        completed_at: t.completed_at,
    }
}

/// 依 `input_type` 驗證回填的值。
///
/// # 為什麼這一層必須存在
///
/// 契約把 `result_value` 宣告成無型別（`{}`），而 `work_order_tasks`
/// 只有 `input_type` 的 CHECK，**沒有任何約束能保證結果值符合那個型別**：
/// `result_value` 是 jsonb，資料庫會欣然接受把字串填進 NUMBER 項目。
/// 沒有這一層，檢查表的 `min_value`／`max_value`／`options`
/// 就只是裝飾性欄位。
///
/// 超出範圍是 422 而不是 `is_pass = false`：那兩件事語意不同 ——
/// 「進風溫度 55°C」是超標（技師該回報的事實，`is_pass=false`），
/// 「進風溫度 = '熱'」是格式錯誤。範本設 `min/max` 是為了界定**合理讀值**，
/// 落在界外的多半是打錯字，靜默收下會污染後續的趨勢分析。
fn validate_result(task: &repo::TaskRow, value: &serde_json::Value) -> Result<(), Problem> {
    use serde_json::Value;
    match task.input_type.as_str() {
        "CHECKBOX" => {
            if !value.is_boolean() {
                return Err(Problem::validation(format!(
                    "task `{}` is a CHECKBOX; result_value must be true or false",
                    task.title
                )));
            }
        }
        "NUMBER" => {
            let Some(n) = value.as_f64() else {
                return Err(Problem::validation(format!(
                    "task `{}` is a NUMBER; result_value must be numeric",
                    task.title
                )));
            };
            if let Some(min) = task.min_value {
                if n < min {
                    return Err(Problem::validation(format!(
                        "task `{}`: {n} is below the template minimum {min}",
                        task.title
                    )));
                }
            }
            if let Some(max) = task.max_value {
                if n > max {
                    return Err(Problem::validation(format!(
                        "task `{}`: {n} is above the template maximum {max}",
                        task.title
                    )));
                }
            }
        }
        "SELECT" => {
            let Some(chosen) = value.as_str() else {
                return Err(Problem::validation(format!(
                    "task `{}` is a SELECT; result_value must be a string",
                    task.title
                )));
            };
            // options 缺漏時不擋：範本沒給選項清單就無從比對，
            // 而那是範本的問題，不該讓技師無法回填。
            if let Some(Value::Array(opts)) = task.options.as_ref() {
                if !opts.iter().any(|o| o.as_str() == Some(chosen)) {
                    let allowed: Vec<&str> = opts.iter().filter_map(|o| o.as_str()).collect();
                    return Err(Problem::validation(format!(
                        "task `{}`: `{chosen}` is not one of {allowed:?}",
                        task.title
                    )));
                }
            }
        }
        // TEXT／PHOTO／SIGNATURE：PHOTO 與 SIGNATURE 存的是附件 id 或
        // 資料 URL，形狀由客戶端決定，這裡只要求非 null。
        _ => {
            if value.is_null() {
                return Err(Problem::validation(format!(
                    "task `{}`: result_value must not be null",
                    task.title
                )));
            }
        }
    }
    Ok(())
}

/// `PATCH /work-orders/{workOrderId}/tasks/{taskId}`
pub async fn update_task(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path((work_order_id, task_id)): Path<(Uuid, Uuid)>,
    Json(u): Json<TaskUpdate>,
) -> Result<Json<TaskDto>, Problem> {
    if u.result_value.is_none() && u.is_pass.is_none() && u.notes.is_none() {
        return Err(Problem::validation(
            "supply at least one of result_value, is_pass or notes",
        ));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let wo = repo::get(&mut tx, work_order_id)
        .await?
        .ok_or_else(|| Problem::not_found("work order not found"))?;
    // 回填檢查表是執行作業的一部分，因此用 execute 而非 update 的權限。
    require_permission(&mut tx, "work_order:execute", Some(wo.facility_id), None).await?;

    let task = repo::task(&mut tx, work_order_id, task_id)
        .await?
        .ok_or_else(|| Problem::not_found("task not found on this work order"))?;

    if let Some(value) = u.result_value.as_ref() {
        validate_result(&task, value)?;
    }

    repo::update_task(
        &mut tx,
        task_id,
        u.result_value.as_ref(),
        u.is_pass,
        u.notes.as_deref(),
    )
    .await?;
    let updated = repo::task(&mut tx, work_order_id, task_id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("task vanished")))?;
    tx.commit().await?;

    Ok(Json(task_to_dto(updated)))
}

/// `POST /work-orders/{workOrderId}/comments`
///
/// 契約原本沒有這支端點，但 `WorkOrderDetail.comments` 已經宣告了留言陣列
/// —— 與附件一樣是「有讀無寫」。這是本次新增的契約面。
pub async fn add_comment(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(work_order_id): Path<Uuid>,
    Json(c): Json<CommentCreate>,
) -> Result<(StatusCode, Json<CommentDto>), Problem> {
    let body = c
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| Problem::validation("body is required"))?;
    let visibility = c.visibility.unwrap_or_else(|| "INTERNAL".to_string());
    if !COMMENT_VISIBILITY.contains(&visibility.as_str()) {
        return Err(Problem::validation(format!(
            "invalid visibility `{visibility}`; allowed: {COMMENT_VISIBILITY:?}"
        )));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let wo = repo::get(&mut tx, work_order_id)
        .await?
        .ok_or_else(|| Problem::not_found("work order not found"))?;
    // 留言等同更新工單的紀錄，沿用 update 權限而不新造 comment 專屬權限。
    require_permission(&mut tx, "work_order:update", Some(wo.facility_id), None).await?;

    let id = repo::add_comment(&mut tx, work_order_id, &visibility, body).await?;
    let created = repo::comments(&mut tx, work_order_id)
        .await?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| Problem::internal(std::io::Error::other("comment vanished")))?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CommentDto {
            id: created.id,
            author_name: created.author_name,
            visibility: created.visibility,
            body: created.body,
            created_at: created.created_at,
        }),
    ))
}

// =============================================================================
// 工單執行面：tasks / labor / parts
// =============================================================================
//
// 這三支的共同點：**表在、邏輯在、只缺 HTTP 入口**。
//
//   `repo::tasks`            —— 已存在（`PATCH .../tasks/{taskId}` 在用）
//   `repo::record_part_usage` —— 已存在，而且難的部分都做對了
//   `repo::record_labor`      —— 已存在，但只記分鐘（沒有費率、人員、加班）
//
// 也就是說技師在現場做的三件事（照檢查表、登工時、領備品）中，
// 只有「回填檢查結果」有端點。另外兩件**沒有辦法從 API 記錄**。
//
// 而 `/reports/service-volume`（可 chargeback）與 `asset-reliability`
// 需要的正是這些明細列。先做報表就是再造一次空報表。

/// `GET /work-orders/{workOrderId}/tasks`
///
/// `PATCH .../tasks/{taskId}` 早就有了，而**列出檢查項的端點沒有** ——
/// 也就是說技師得先知道 taskId 才回填得了。
pub async fn list_tasks(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(work_order_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let wo = repo::get(&mut tx, work_order_id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這張工單（或它不在你的範圍內）"))?;
    // 讀檢查表只需要讀權限 —— 回填才需要 execute（見 `update_task`）。
    require_permission(&mut tx, "work_order:read", Some(wo.facility_id), None).await?;

    let tasks = repo::tasks(&mut tx, work_order_id).await?;
    tx.commit().await?;

    let items: Vec<TaskDto> = tasks.into_iter().map(task_to_dto).collect();
    // `required_outstanding` 是「還不能結案」的直接答案 ——
    // 前端不必自己數，而那個數字正是結案守衛看的東西。
    let outstanding = items
        .iter()
        .filter(|t| t.is_required && t.completed_at.is_none())
        .count();
    Ok(Json(serde_json::json!({
        "items": items,
        "meta": { "required_outstanding": outstanding },
    })))
}

/// `POST /work-orders/{workOrderId}/labor`
#[derive(Debug, serde::Deserialize)]
pub struct LaborEntry {
    /// 省略時記在呼叫者自己身上。填別人需要 `work_order:assign`
    /// —— 替別人登記工時是排程者的動作，見 handler 註解。
    pub user_id: Option<Uuid>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 與 `started_at`/`ended_at` 二擇一。兩者都給時必須一致。
    pub minutes: Option<i32>,
    /// 每小時費率。**沒有預設來源** —— schema 裡沒有任何 users/teams/skills
    /// 的費率欄位，所以由呼叫端提供。省略時 `cost` 是 null 而不是 0
    /// （見 handler 註解）。
    pub hourly_rate: Option<f64>,
    pub is_overtime: Option<bool>,
    pub notes: Option<String>,
}

/// `POST /work-orders/{workOrderId}/labor`
///
/// # `cost` 是算出來的，不由呼叫端給
///
/// `work_order_labor` 同時有 `hourly_rate` 與 `cost`。讓呼叫端兩個都填的話，
/// 那個 `cost` 可以與它的輸入不一致 —— 而它會被 `recompute_costs` 加總進
/// `work_orders.labor_cost`，然後出現在 chargeback 的帳單上。
///
/// 所以 `cost = minutes / 60 × hourly_rate`，在 SQL 裡算。
///
/// # 沒有費率時 `cost` 是 null，不是 0
///
/// schema 裡沒有任何費率來源（量過：`users`、`teams`、`skills` 上都沒有
/// rate 欄位），所以費率一定得由呼叫端給。沒給時 `cost` 留 NULL ——
/// 「這筆工時的成本未知」與「這筆工時免費」是完全不同的兩件事，
/// 而 0 會讓前者安靜地變成後者，直接壓低 chargeback 的金額。
///
/// 要讓費率不必每次輸入，需要決定它掛在哪裡（人？團隊？技能？）——
/// 那是一個獨立的設計決定，不在這一輪。
pub async fn record_labor_entry(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(work_order_id): Path<Uuid>,
    Json(body): Json<LaborEntry>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    // 分鐘數：由區間推導，或直接給。兩者都給時必須一致 ——
    // 不一致而選一個會讓另一個變成謊。
    let minutes = match (body.started_at, body.ended_at, body.minutes) {
        (Some(s), Some(e), given) => {
            if e <= s {
                return Err(Problem::validation("ended_at 必須晚於 started_at"));
            }
            let derived = (e - s).num_minutes() as i32;
            if let Some(m) = given {
                if m != derived {
                    return Err(Problem::validation(format!(
                        "minutes={m} 與 started_at/ended_at 推導出的 {derived} 不一致 —— \
                         兩者都給的話必須相符，否則其中一個是假的"
                    )));
                }
            }
            derived
        }
        (_, _, Some(m)) => m,
        _ => {
            return Err(Problem::validation(
                "要有 minutes，或 started_at + ended_at",
            ))
        }
    };
    if !(1..=24 * 60).contains(&minutes) {
        return Err(Problem::validation(
            "minutes 必須是 1 到 1440（一天）—— 跨日的工時請分兩筆登記",
        ));
    }
    if let Some(r) = body.hourly_rate {
        if !(0.0..=100_000.0).contains(&r) {
            return Err(Problem::validation("hourly_rate 超出合理範圍"));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let wo = repo::get(&mut tx, work_order_id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這張工單（或它不在你的範圍內）"))?;
    require_permission(&mut tx, "work_order:execute", Some(wo.facility_id), None).await?;

    // 替**別人**登記工時是排程者的動作，不是執行者的 ——
    // 少了這道檢查，任何技師都能把工時掛到同事身上，而那會影響
    // 團隊負載與 chargeback 的歸屬。
    let self_id = tx.context().user_id;
    let target = body.user_id.unwrap_or(self_id);
    if target != self_id {
        require_permission(&mut tx, "work_order:assign", Some(wo.facility_id), None).await?;
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.work_order_labor
           (tenant_id, work_order_id, user_id, started_at, ended_at, minutes,
            hourly_rate, cost, is_overtime, notes)
         SELECT fms.current_tenant_id(), $1, $2,
                coalesce($3::timestamptz,
                         coalesce(w.actual_start_at, clock_timestamp())
                           - ($5::int * interval '1 minute')),
                coalesce($4::timestamptz,
                         coalesce(w.actual_start_at, clock_timestamp())),
                $5,
                $6::float8::numeric,
                -- cost 由 minutes 與費率算出，不由呼叫端給（見 handler 註解）。
                -- 沒有費率時留 NULL，不是 0。
                CASE WHEN $6::float8 IS NOT NULL
                     THEN round(($5::numeric / 60) * $6::float8::numeric, 2) END,
                coalesce($7, false), $8
           FROM fms.work_orders w
          WHERE w.id = $1
         RETURNING id",
    )
    .bind(work_order_id)
    .bind(target)
    .bind(body.started_at)
    .bind(body.ended_at)
    .bind(minutes)
    .bind(body.hourly_rate)
    .bind(body.is_overtime)
    .bind(body.notes.as_deref())
    .fetch_one(tx.conn())
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.constraint() == Some("work_order_labor_user_id_fkey") => {
            Problem::not_found("找不到這個使用者")
        }
        _ => Problem::from(e),
    })?;

    // rollup。004 沒有觸發器做這件事，所以明細寫完要自己叫 ——
    // 少了它 `work_orders.labor_cost` 會停在舊值，而報表讀的是那一欄。
    repo::recompute_costs(&mut tx, work_order_id).await?;

    let row: (Option<f64>, Option<f64>, i32) = sqlx::query_as(
        "SELECT l.cost::float8, w.labor_cost::float8, w.labor_minutes
           FROM fms.work_order_labor l
           JOIN fms.work_orders w ON w.id = l.work_order_id
          WHERE l.id = $1",
    )
    .bind(id)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": id,
            "user_id": target,
            "minutes": minutes,
            // null = 沒有給費率，成本未知。**不是 0**。
            "cost": row.0,
            "work_order_labor_cost": row.1,
            "work_order_labor_minutes": row.2,
        })),
    ))
}

/// `POST /work-orders/{workOrderId}/parts`
#[derive(Debug, serde::Deserialize)]
pub struct PartUsage {
    pub part_id: Option<Uuid>,
    pub quantity: Option<f64>,
}

/// `POST /work-orders/{workOrderId}/parts`
///
/// 薄包裝在 `repo::record_part_usage` 上 —— 那支已經把難的部分做對了：
///
///   * **原子扣帳**（`WHERE quantity_on_hand >= $qty`），不是先查再扣。
///     兩張工單同時領最後一片濾網時由資料庫仲裁。
///   * **區分兩種缺料**：該場域沒有庫存列 → 允許（廠商當場帶料是真實情境）；
///     有庫存列但不足 → 409。
///   * **領用時快照單價** —— 料件日後調價不會改寫已完成工單的成本。
///
/// 這支端點只負責驗輸入、映射那個 `false` 到 409、並在寫完之後叫 rollup。
pub async fn record_parts(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(work_order_id): Path<Uuid>,
    Json(body): Json<PartUsage>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let part_id = body
        .part_id
        .ok_or_else(|| Problem::validation("part_id 為必填"))?;
    let quantity = body
        .quantity
        .ok_or_else(|| Problem::validation("quantity 為必填"))?;
    if !(quantity > 0.0 && quantity <= 1_000_000.0) {
        return Err(Problem::validation("quantity 必須大於 0"));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let wo = repo::get(&mut tx, work_order_id)
        .await?
        .ok_or_else(|| Problem::not_found("找不到這張工單（或它不在你的範圍內）"))?;
    require_permission(&mut tx, "work_order:execute", Some(wo.facility_id), None).await?;

    let issued =
        repo::record_part_usage(&mut tx, work_order_id, wo.facility_id, part_id, quantity).await?;
    if !issued {
        return Err(Problem::new(fms_shared::ProblemCode::Conflict).with_detail(
            "這個場域的庫存不足 —— 若是廠商當場帶料，請先在庫存裡登記入庫，\
             或由備品管理者調撥",
        ));
    }

    repo::recompute_costs(&mut tx, work_order_id).await?;

    let row: (Option<f64>, Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT wp.total_cost::float8, w.parts_cost::float8,
                (SELECT s.quantity_on_hand::float8 FROM fms.part_stock s
                  WHERE s.id = wp.issued_from_stock_id)
           FROM fms.work_order_parts wp
           JOIN fms.work_orders w ON w.id = wp.work_order_id
          WHERE wp.work_order_id = $1 AND wp.part_id = $2
          ORDER BY wp.issued_at DESC NULLS LAST
          LIMIT 1",
    )
    .bind(work_order_id)
    .bind(part_id)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "part_id": part_id,
            "quantity_used": quantity,
            "total_cost": row.0,
            "work_order_parts_cost": row.1,
            // null = 這個場域沒有這個料件的庫存列（廠商帶料），不是 0 庫存。
            "stock_on_hand_after": row.2,
        })),
    ))
}
