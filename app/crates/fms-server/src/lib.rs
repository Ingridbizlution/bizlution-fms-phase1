//! HTTP 服務組裝。刻意與 `main.rs` 分離，讓整合測試能直接建出 router。

use axum::routing::{get, post};
use axum::Router;
use sqlx::postgres::PgPoolOptions;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use fms_asset::handlers as asset;
use fms_attachment::handlers as attachment;
use fms_catalogue::handlers as catalogue;
use fms_identity::handlers as identity;
use fms_maintenance::handlers as maintenance;
use fms_notification::handlers as notification;
use fms_report::handlers as report;
use fms_reservation::handlers as reservation;
use fms_shared::Settings;
use fms_tenancy::handlers as tenancy;
use fms_workorder::handlers as workorder;

pub mod docs;

/// 建立 router。路徑前綴 `/api/v1` 對齊 `openapi.yaml` 的 servers。
///
/// 各模組有自己的 state，但共用同一條 `require_auth` middleware ——
/// 認證與 `X-Tenant-ID` 一致性只實作一次。
pub fn build_router(
    state: identity::IdentityState,
    storage: fms_shared::Storage,
    // `*_secret_ref` 的解析器。以參數傳入而不是在函式裡 `new` 一個
    // `EnvSecretResolver`：那樣寫的話 trait 就只是形式 —— 換成 Vault 或
    // KMS 要改的是這個組裝點，而測試也需要能注入一個固定對照表才驗得到
    // `secret_reference_resolvable` 的 PASSED 分支。
    secrets: std::sync::Arc<dyn fms_shared::SecretResolver>,
    // Microsoft 365 app 憑證（ADR-14 決定 G）：全平台共用一份，不是
    // per-tenant，因此跟 `secrets` 解析器分開傳——這兩個值本身不是密鑰
    // 參照，是憑證本身，讀取方式見 `main.rs`。
    ms365_app_client_id: String,
    ms365_app_client_secret: String,
) -> Router {
    let reservation_state = reservation::ReservationState {
        pool: state.pool.clone(),
    };

    // 在任何 `move` 之前先取出來：`state` 會被 `with_state` 消耗。
    let cors_origins = state.settings.cors_allowed_origins.clone();

    let assets = Router::new()
        .route("/api/v1/assets", get(asset::list).post(asset::create))
        .route(
            "/api/v1/assets/{assetId}",
            get(asset::get).patch(asset::update).delete(asset::delete),
        )
        .route(
            "/api/v1/assets/{assetId}/dependency-graph",
            get(asset::dependency_graph),
        )
        .route(
            "/api/v1/asset-models",
            get(asset::list_models).post(fms_asset::completion::create_model),
        )
        .route(
            "/api/v1/asset-models/{modelId}/compatibility",
            get(fms_asset::completion::model_compatibility),
        )
        .route(
            "/api/v1/asset-categories",
            get(fms_asset::completion::list_categories),
        )
        .route(
            "/api/v1/attribute-definitions",
            get(fms_asset::attributes::list_definitions)
                .post(fms_asset::attributes::create_definition),
        )
        .route(
            "/api/v1/assets/{assetId}/relations",
            post(fms_asset::attributes::create_relation),
        )
        .route(
            "/api/v1/asset-relations/{relationId}",
            axum::routing::delete(fms_asset::attributes::delete_relation),
        )
        .route(
            "/api/v1/assets:bulk-import",
            post(fms_asset::attributes::bulk_import),
        )
        .route(
            "/api/v1/assets/{assetId}/work-orders",
            get(fms_asset::completion::asset_work_orders),
        )
        .route(
            "/api/v1/assets/{assetId}/status-history",
            get(fms_asset::completion::asset_status_history),
        )
        .route(
            "/api/v1/assets/{assetId}/meters/{meterCode}/readings",
            get(fms_asset::completion::meter_readings),
        )
        .route(
            "/api/v1/assets/{assetId}/meters/{meterCode}/readings",
            post(asset::record_reading),
        )
        .route(
            "/api/v1/telemetry:batch-ingest",
            post(fms_asset::telemetry::batch_ingest),
        )
        .route(
            "/api/v1/devices",
            get(fms_asset::devices::list).post(fms_asset::devices::create),
        )
        .route(
            "/api/v1/devices/{deviceId}",
            axum::routing::patch(fms_asset::devices::update),
        )
        .route(
            "/api/v1/devices/{deviceId}/points",
            get(fms_asset::devices::points),
        )
        .route(
            "/api/v1/telemetry/latest",
            get(fms_asset::telemetry_read::latest),
        )
        .route(
            "/api/v1/telemetry/series",
            get(fms_asset::telemetry_read::series),
        )
        .route(
            "/api/v1/alarm-rules",
            get(fms_asset::alarm_rules::list).post(fms_asset::alarm_rules::create),
        )
        .route(
            "/api/v1/alarm-rules/{ruleId}/test",
            post(fms_asset::alarm_rules::dry_run),
        )
        .route("/api/v1/alarms", get(fms_asset::alarms::list))
        .route(
            "/api/v1/alarms/{alarmId}/acknowledge",
            post(fms_asset::alarms::acknowledge),
        )
        .route(
            "/api/v1/alarms/{alarmId}/suppress",
            post(fms_asset::alarms::suppress),
        )
        // 路徑裡的冒號是**靜態字串的一部分**，不是參數分隔 ——
        // axum 0.8 拒絕 `{param}:literal`（同一段裡有參數又有字面值），
        // 但整段都是字面值沒有問題。（report export 因為同樣的限制
        // 把 `{reportCode}:export` 展開成靜態路由。）
        .route(
            "/api/v1/alarms:reconcile-work-orders",
            post(fms_asset::alarms::reconcile_work_orders),
        )
        .route(
            "/api/v1/alarms/{alarmId}/work-order",
            post(fms_asset::alarms::create_work_order),
        )
        .with_state(asset::AssetState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    // 租戶設定：獨立的 state 與 router，因為它的權限是 TENANT 範圍
    // （其餘 tenancy 端點多是場域層級）。
    let tenant_routes = Router::new()
        .route(
            "/api/v1/tenant",
            get(fms_tenancy::tenant::get).patch(fms_tenancy::tenant::patch),
        )
        .with_state(fms_tenancy::tenant::TenantState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let tenancy_routes = Router::new()
        .route(
            "/api/v1/organizations",
            get(tenancy::list_orgs).post(tenancy::create_org),
        )
        .route(
            "/api/v1/facilities",
            get(tenancy::list_facilities).post(tenancy::create_facility),
        )
        .route(
            "/api/v1/facilities/{facilityId}",
            get(tenancy::get_facility).patch(tenancy::update_facility),
        )
        .route(
            "/api/v1/organizations/{organizationId}",
            get(fms_tenancy::tail::get_org)
                .patch(fms_tenancy::tail::patch_org)
                .delete(fms_tenancy::tail::delete_org),
        )
        .route(
            "/api/v1/spatial-nodes/{nodeId}",
            get(fms_tenancy::tail::get_node)
                .patch(fms_tenancy::tail::patch_node)
                .delete(fms_tenancy::tail::delete_node),
        )
        .route(
            "/api/v1/facilities/{facilityId}/spatial-nodes",
            get(tenancy::list_nodes).post(tenancy::create_node),
        )
        .route(
            "/api/v1/facilities/{facilityId}/bim-models",
            get(fms_tenancy::bim::list).post(fms_tenancy::bim::register),
        )
        .route(
            "/api/v1/spatial-node-types",
            get(fms_tenancy::spatial_tail::list_node_types),
        )
        .route(
            "/api/v1/facilities/{facilityId}/spatial-nodes:bulk-import",
            post(fms_tenancy::spatial_tail::bulk_import_nodes),
        )
        .route(
            "/api/v1/facilities/{facilityId}/floor-view",
            get(fms_tenancy::spatial_tail::floor_view),
        )
        .route(
            "/api/v1/spatial-nodes/{floorNodeId}/floor-plan-markers",
            get(fms_tenancy::floor_plan_markers::list)
                .post(fms_tenancy::floor_plan_markers::create),
        )
        .route(
            "/api/v1/floor-plan-markers/{id}",
            axum::routing::delete(fms_tenancy::floor_plan_markers::delete),
        )
        .route(
            "/api/v1/bim-models/{bimModelId}",
            get(fms_tenancy::spatial_tail::get_bim_model).delete(fms_tenancy::bim::delete),
        )
        .route(
            "/api/v1/bim-models/{bimModelId}/reset",
            post(fms_tenancy::bim::reset),
        )
        .route(
            "/api/v1/bim-models/{bimModelId}/mappings",
            post(fms_tenancy::spatial_tail::create_mappings),
        )
        .route(
            "/api/v1/bim-models/{bimModelId}/unresolved-elements",
            get(fms_tenancy::bim::unresolved_elements),
        )
        .with_state(tenancy::TenancyState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let attachments = Router::new()
        .route(
            "/api/v1/attachments",
            get(attachment::list).post(attachment::create),
        )
        .route(
            "/api/v1/attachments/{attachmentId}",
            get(attachment::get).delete(attachment::delete),
        )
        .with_state(attachment::AttachmentState {
            pool: state.pool.clone(),
            storage: storage.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let maintenance_plans = Router::new()
        .route(
            "/api/v1/maintenance-plans",
            get(maintenance::list).post(maintenance::create),
        )
        .route(
            "/api/v1/maintenance-plans/{planId}/preview-schedule",
            get(maintenance::preview_schedule),
        )
        .route(
            "/api/v1/maintenance-plans/{planId}",
            axum::routing::patch(maintenance::patch_plan),
        )
        .route(
            "/api/v1/maintenance-plans/{planId}/generate-now",
            post(maintenance::generate_now),
        )
        .route(
            "/api/v1/maintenance-templates",
            get(fms_maintenance::templates::list).post(fms_maintenance::templates::create),
        )
        .route(
            "/api/v1/maintenance-occurrences",
            get(fms_maintenance::occurrences::list),
        )
        .route(
            "/api/v1/maintenance-occurrences/{occurrenceId}/skip",
            post(fms_maintenance::occurrences::skip),
        )
        .with_state(maintenance::MaintenanceState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let work_orders = Router::new()
        .route(
            "/api/v1/work-orders",
            get(workorder::list).post(workorder::create),
        )
        .route(
            "/api/v1/work-orders/{workOrderId}",
            get(workorder::get).patch(workorder::update),
        )
        .route(
            "/api/v1/work-order-statuses",
            get(fms_workorder::tail::statuses),
        )
        .route(
            "/api/v1/work-order-state-machine",
            get(fms_workorder::tail::state_machine),
        )
        .route(
            "/api/v1/work-orders:bulk-transition",
            post(fms_workorder::tail::bulk_transition),
        )
        .route("/api/v1/teams", get(fms_workorder::tail::list_teams))
        .route(
            "/api/v1/teams/{teamId}/workload",
            get(fms_workorder::tail::team_workload),
        )
        .route(
            "/api/v1/teams/{teamId}/shifts",
            get(fms_workorder::tail::team_shifts).post(fms_workorder::tail::create_shift),
        )
        .route("/api/v1/parts", get(fms_workorder::tail::list_parts))
        .route(
            "/api/v1/part-stock",
            get(fms_workorder::tail::list_part_stock),
        )
        .route(
            "/api/v1/work-orders/{workOrderId}/satisfaction",
            post(fms_workorder::satisfaction::submit),
        )
        .route(
            "/api/v1/work-orders/{workOrderId}/transitions",
            post(workorder::transition),
        )
        .route(
            "/api/v1/work-orders/{workOrderId}/available-actions",
            get(workorder::available_actions),
        )
        .route(
            "/api/v1/work-orders/{workOrderId}/tasks",
            get(workorder::list_tasks),
        )
        .route(
            "/api/v1/work-orders/{workOrderId}/labor",
            post(workorder::record_labor_entry),
        )
        .route(
            "/api/v1/work-orders/{workOrderId}/parts",
            post(workorder::record_parts),
        )
        .route(
            "/api/v1/work-orders/{workOrderId}/tasks/{taskId}",
            axum::routing::patch(workorder::update_task),
        )
        .route(
            "/api/v1/work-orders/{workOrderId}/comments",
            post(workorder::add_comment),
        )
        .with_state(workorder::WorkOrderState {
            pool: state.pool.clone(),
            storage: storage.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let reservations = Router::new()
        .route(
            "/api/v1/reservations",
            get(reservation::list).post(reservation::create),
        )
        // 封鎖時段。GET 走 `reservation:read`（封鎖視窗與原因已經從可用性查詢
        // 洩漏出去，把清單擋在寫入權限後面不保護任何東西），POST 走
        // `blackout:write` —— 見 `fms_reservation::blackouts` 模組說明。
        .route(
            "/api/v1/resource-blackouts",
            get(fms_reservation::blackouts::list).post(fms_reservation::blackouts::create),
        )
        .route(
            "/api/v1/reservations/{reservationId}",
            get(reservation::get)
                .patch(reservation::update)
                .delete(reservation::cancel),
        )
        .route(
            "/api/v1/facilities/{facilityId}/availability",
            get(reservation::availability),
        )
        .route("/api/v1/reservations/holds", post(reservation::create_hold))
        .route(
            "/api/v1/reservations/holds/{holdToken}",
            axum::routing::delete(fms_reservation::tail::release_hold),
        )
        .route(
            "/api/v1/reservations/{reservationId}/check-out",
            post(fms_reservation::tail::check_out),
        )
        .route(
            "/api/v1/reservation-series/{recurrenceGroupId}",
            axum::routing::delete(fms_reservation::tail::cancel_series),
        )
        .route(
            "/api/v1/facilities/{facilityId}/bookable-resources",
            get(fms_reservation::tail::list_bookable_resources),
        )
        .route(
            "/api/v1/bookable-resources/{resourceId}",
            axum::routing::patch(fms_reservation::tail::patch_bookable_resource),
        )
        .route(
            "/api/v1/amenities",
            get(fms_reservation::tail::list_amenities),
        )
        .route(
            "/api/v1/bookable-resources/{resourceId}/amenities",
            get(fms_reservation::tail::list_resource_amenities)
                .put(fms_reservation::tail::put_resource_amenities),
        )
        .route(
            "/api/v1/reservations/{reservationId}/check-in",
            post(reservation::check_in),
        )
        .route(
            "/api/v1/reservations/{reservationId}/approve",
            post(reservation::approve),
        )
        .route(
            "/api/v1/reservations/{reservationId}/reject",
            post(reservation::reject),
        )
        .route(
            "/api/v1/facilities/{facilityId}/occupancy",
            get(reservation::occupancy),
        )
        .with_state(reservation_state)
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    // 需認證的路由：套上 require_auth（含 X-Tenant-ID 一致性檢查）
    let authenticated = Router::new()
        .route("/api/v1/auth/me", get(identity::me))
        // logout 與改密碼都需要**已登入**（契約），因此在這一組而不是 public：
        // logout 要驗 refresh token 屬於呼叫者（見 handler 說明），
        // 改密碼要以 caller 的身分開交易。
        .route("/api/v1/auth/logout", post(identity::logout))
        .route(
            "/api/v1/auth/password/change",
            post(identity::password_change),
        )
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let catalogue_routes = Router::new()
        .route(
            "/api/v1/facilities/{facilityId}/service-items",
            get(catalogue::list).post(fms_catalogue::admin::create),
        )
        .route(
            "/api/v1/service-items/{serviceItemId}",
            axum::routing::patch(fms_catalogue::admin::patch).delete(fms_catalogue::admin::delete),
        )
        .route(
            "/api/v1/service-items/{serviceItemId}/availability",
            get(fms_catalogue::admin::availability),
        )
        .with_state(catalogue::CatalogueState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let sla_policy_routes = Router::new()
        .route(
            "/api/v1/sla-policies",
            get(fms_workorder::sla_policy::list).post(fms_workorder::sla_policy::create),
        )
        .route(
            "/api/v1/sla-policies/{slaPolicyId}",
            axum::routing::patch(fms_workorder::sla_policy::update),
        )
        .with_state(fms_workorder::sla_policy::SlaPolicyState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let users_routes = Router::new()
        .route(
            "/api/v1/users",
            get(fms_identity::users::list).post(fms_identity::users::create),
        )
        .route(
            "/api/v1/users/{userId}",
            axum::routing::patch(fms_identity::users::patch),
        )
        .route(
            "/api/v1/users/{userId}/suspend",
            post(fms_identity::users::suspend),
        )
        .with_state(fms_identity::users::UsersState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let role_assignment_routes = Router::new()
        .route(
            "/api/v1/users/{userId}/role-assignments",
            get(fms_identity::role_assignments::list).post(fms_identity::role_assignments::assign),
        )
        .route(
            "/api/v1/role-assignments/{id}",
            axum::routing::delete(fms_identity::role_assignments::revoke),
        )
        .with_state(fms_identity::role_assignments::RoleAssignmentsState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let role_catalogue_routes = Router::new()
        .route(
            "/api/v1/roles",
            get(fms_identity::roles::list).post(fms_identity::roles::create),
        )
        .route(
            "/api/v1/permissions",
            get(fms_identity::roles::list_permissions),
        )
        .with_state(fms_identity::roles::RolesState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let audit_routes = Router::new()
        .route("/api/v1/audit-log", get(fms_identity::audit::list))
        .with_state(fms_identity::audit::AuditState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let directory_mapping_routes = Router::new()
        .route(
            "/api/v1/directory-role-mappings",
            get(fms_identity::directory_mappings::list)
                .post(fms_identity::directory_mappings::create),
        )
        .route(
            "/api/v1/directory-role-mappings/{id}",
            axum::routing::delete(fms_identity::directory_mappings::delete),
        )
        .with_state(fms_identity::directory_mappings::DirectoryMappingsState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let audit_export_routes = Router::new()
        .route(
            "/api/v1/audit-log:export",
            post(fms_identity::audit_export::create),
        )
        .route(
            "/api/v1/audit-log/exports/{id}",
            get(fms_identity::audit_export::get),
        )
        .with_state(fms_identity::audit_export::AuditExportState {
            pool: state.pool.clone(),
            storage: storage.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let skill_routes = Router::new()
        .route(
            "/api/v1/skills",
            get(fms_identity::skills::list).post(fms_identity::skills::create),
        )
        .route(
            "/api/v1/users/{userId}/skills",
            get(fms_identity::skills::list_for_user),
        )
        .route(
            "/api/v1/users/{userId}/skills/{skillId}",
            axum::routing::put(fms_identity::skills::upsert_for_user),
        )
        .with_state(fms_identity::skills::SkillsState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let identity_provider_routes = Router::new()
        .route(
            "/api/v1/identity-providers",
            get(fms_identity::identity_providers::list)
                .post(fms_identity::identity_providers::create),
        )
        .route(
            "/api/v1/identity-providers/{providerId}",
            axum::routing::patch(fms_identity::identity_providers::patch),
        )
        .route(
            "/api/v1/identity-providers/{providerId}/test-connection",
            post(fms_identity::identity_providers::test_connection),
        )
        .route(
            "/api/v1/identity-providers/{providerId}/sync",
            post(fms_identity::identity_providers::sync),
        )
        .route(
            "/api/v1/identity-providers/{providerId}/sync-runs",
            get(fms_identity::identity_providers::list_runs),
        )
        .with_state(fms_identity::identity_providers::IdentityProvidersState {
            pool: state.pool.clone(),
            outbound: state.settings.outbound.clone(),
            secrets,
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    // `GET /directory-groups` 與 identity-providers 同一個權限閘門
    // （`identity_provider:read`），但 state 不同，因此獨立一組。
    let directory_group_routes = Router::new()
        .route(
            "/api/v1/directory-groups",
            get(fms_identity::directory_groups::list),
        )
        .with_state(fms_identity::directory_groups::DirectoryGroupsState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let upload_routes = Router::new()
        .route(
            "/api/v1/uploads/presign",
            post(fms_tenancy::bim::presign_upload),
        )
        .with_state(fms_tenancy::bim::UploadState {
            storage: storage.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let calendar_routes = Router::new()
        .route(
            "/api/v1/facilities/{facilityId}/calendar-integrations",
            get(fms_calendar::handlers::list).post(fms_calendar::handlers::register),
        )
        .route(
            "/api/v1/calendar-integrations/{integrationId}/unresolved-resources",
            get(fms_calendar::handlers::unresolved_resources),
        )
        .route(
            "/api/v1/calendar-integrations/{integrationId}/resource-mappings",
            post(fms_calendar::handlers::create_mappings),
        )
        .with_state(fms_calendar::handlers::CalendarState {
            pool: state.pool.clone(),
            ms_app_client_id: ms365_app_client_id,
            ms_app_client_secret: ms365_app_client_secret,
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let notification_routes = Router::new()
        .route("/api/v1/notifications", get(notification::list))
        .route(
            "/api/v1/notifications/{notificationId}/read",
            post(notification::mark_read),
        )
        .route(
            "/api/v1/notification-templates",
            get(fms_notification::templates::list).post(fms_notification::templates::create),
        )
        .route(
            "/api/v1/notification-templates/{templateId}",
            axum::routing::patch(fms_notification::templates::update)
                .delete(fms_notification::templates::delete),
        )
        .with_state(notification::NotificationState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    // webhook 訂閱。`WebhookState` 與 `NotificationState` 分開是因為它們的
    // 授權完全不同：收件匣是「已登入」，訂閱是 `tenant:update`（清單裡有客戶的
    // 內部端點網址）。
    let webhook_routes = Router::new()
        .route(
            "/api/v1/webhooks",
            get(fms_notification::webhooks::list).post(fms_notification::webhooks::upsert),
        )
        .with_state(fms_notification::webhooks::WebhookState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let holiday_routes = Router::new()
        .route(
            "/api/v1/holiday-calendars",
            get(fms_workorder::holiday::list).post(fms_workorder::holiday::create),
        )
        .route(
            "/api/v1/holiday-calendars/{holidayId}",
            axum::routing::patch(fms_workorder::holiday::update)
                .delete(fms_workorder::holiday::delete),
        )
        .with_state(fms_workorder::holiday::HolidayState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    let report_routes = Router::new()
        .route(
            "/api/v1/reports/sla-compliance",
            get(report::sla_compliance),
        )
        .route(
            "/api/v1/reports/facility-dashboard",
            get(fms_report::dashboard::facility_dashboard),
        )
        .route("/api/v1/reports/pm-compliance", get(report::pm_compliance))
        .route("/api/v1/reports/group-rollup", get(report::group_rollup))
        .route(
            "/api/v1/reports/asset-reliability",
            get(report::asset_reliability),
        )
        .route(
            "/api/v1/reports/space-utilization",
            get(report::space_utilization),
        )
        .route(
            "/api/v1/reports/service-volume",
            get(report::service_volume),
        )
        .with_state(report::ReportState {
            pool: state.pool.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    // 報表匯出：獨立的 state，因為它需要物件儲存（其餘報表端點不需要）。
    //
    // # 為什麼是逐一註冊而不是 `/{reportCode}:export`
    //
    // axum 0.8 的路由不接受「一個路段裡同時有參數與字面值」——
    // `{reportCode}:export` 會在建 router 時 panic（`Only one parameter is
    // allowed per path segment`）。而把它改成 `/{spec}` 再自己切 `:` 會與
    // 上面那七條靜態的 `GET /reports/<code>` 同段衝突。
    //
    // 所以從 `REPORTS` 逐一展開成靜態路徑。副作用是好的：**路由表就是
    // 白名單**，一支不在 `REPORTS` 裡的報表連路徑都不存在（回 404），
    // 不需要在 handler 裡再維護第二份清單。
    //
    // 契約與 `IMPLEMENTED_OPERATIONS` 仍然是一條 `{reportCode}` 的樣板 ——
    // 那是對外的形狀，而 `report_export_slice.rs` 的 `b_` 逐一 POST 每一支，
    // 所以「樣板說有、實際沒掛上」會被抓到。
    let mut report_export_routes =
        Router::new().route("/api/v1/reports/exports/{id}", get(fms_report::export::get));
    for spec in fms_report::export::REPORTS {
        report_export_routes = report_export_routes.route(
            &format!("/api/v1/reports/{}:export", spec.code),
            post(fms_report::export::create),
        );
    }
    let report_export_routes = report_export_routes
        .with_state(fms_report::export::ReportExportState {
            pool: state.pool.clone(),
            storage: storage.clone(),
        })
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            identity::require_auth,
        ));

    // SCIM 2.0 供裝端點。**唯一不用 JWT 也不用 `X-Tenant-ID` 的一組**：
    // 認證是 074 的 bearer token，而 token 本身就是租戶的判別依據
    // （Entra ID 只能設一個靜態 token，送不出 `X-Tenant-ID`）。
    //
    // 因此它有自己的 middleware，而且**不能**併進 `authenticated` ——
    // `require_auth` 會先要求 JWT 而永遠拒絕它。
    //
    // 路徑是 `/api/v1/scim/v2/…`：`/api/v1` 是整個 API 的 base path
    // （openapi 的 `servers`），而 `/scim/v2` 是 SCIM 自己的版本。
    // 兩個版本號疊在一起不好看，但 Entra 那一側是貼一個完整的 Tenant URL，
    // 因此沒有實際影響 —— 而讓 SCIM 跳出 base path 會讓它成為唯一的例外。
    let scim_routes = Router::new()
        .route(
            "/api/v1/scim/v2/Users",
            get(fms_identity::scim::list_users).post(fms_identity::scim::create_user),
        )
        .route(
            "/api/v1/scim/v2/Users/{id}",
            get(fms_identity::scim::get_user)
                .patch(fms_identity::scim::patch_user)
                .delete(fms_identity::scim::delete_user),
        )
        .route(
            "/api/v1/scim/v2/Groups",
            get(fms_identity::scim::list_groups).post(fms_identity::scim::create_group),
        )
        .route(
            "/api/v1/scim/v2/Groups/{id}",
            get(fms_identity::scim::get_group)
                .patch(fms_identity::scim::patch_group)
                .delete(fms_identity::scim::delete_group),
        )
        .layer(axum::middleware::from_fn(fms_shared::tenant_span))
        .layer(axum::middleware::from_fn_with_state(
            fms_identity::ScimState {
                pool: state.pool.clone(),
            },
            fms_identity::require_scim_token,
        ))
        .with_state(fms_identity::ScimState {
            pool: state.pool.clone(),
        });

    // 取得 token 的端點本身不需要（也不能）認證
    let public = Router::new()
        .route("/api/v1/auth/token", post(identity::token))
        .route("/api/v1/auth/token/refresh", post(identity::refresh))
        // SSO 的兩支也在 public：使用者還沒登入。
        // `/authorize` 需要 `?tenant_code=`（provider code 只在租戶內唯一）；
        // `/callback` 不需要 —— state 那一列帶著 tenant_id。
        .route(
            "/api/v1/auth/sso/{providerCode}/authorize",
            get(fms_identity::sso::authorize),
        )
        .route(
            "/api/v1/auth/sso/{providerCode}/callback",
            get(fms_identity::sso::callback),
        )
        .route("/api/v1/health", get(health))
        // 契約與互動式瀏覽器。與 `/api/v1/health` 同類：在 router 裡，
        // 但不進 `IMPLEMENTED_OPERATIONS`、不進 openapi.yaml、不進
        // ENDPOINTS.md。**不需認證是刻意的** —— 理由寫在 `docs.rs` 檔頭。
        .route("/api/v1/openapi.yaml", get(docs::openapi_yaml))
        .route("/docs", get(docs::index))
        .route("/docs/swagger-ui-bundle.js", get(docs::swagger_ui_js))
        .route("/docs/swagger-ui.css", get(docs::swagger_ui_css));

    let router = Router::new()
        .merge(public)
        .merge(scim_routes)
        .merge(authenticated)
        .merge(reservations)
        .merge(assets)
        .merge(work_orders)
        .merge(maintenance_plans)
        .merge(attachments)
        .merge(tenancy_routes)
        .merge(tenant_routes)
        .merge(catalogue_routes)
        .merge(report_routes)
        .merge(report_export_routes)
        .merge(users_routes)
        .merge(role_assignment_routes)
        .merge(role_catalogue_routes)
        .merge(audit_routes)
        .merge(directory_mapping_routes)
        .merge(audit_export_routes)
        .merge(skill_routes)
        .merge(identity_provider_routes)
        .merge(directory_group_routes)
        .merge(upload_routes)
        .merge(sla_policy_routes)
        .merge(holiday_routes)
        .merge(notification_routes)
        .merge(webhook_routes)
        .merge(calendar_routes)
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid));

    // CORS。**沒有這一層，瀏覽器一個請求都發不出去。**
    //
    // 在此之前這個 router 完全沒有 CORS 設定 —— 而那不是「某些功能不能用」，
    // 是前端 dev server（`localhost:3000`）對 API 的**每一個**跨來源請求都會在
    // preflight 就失敗。純伺服器對伺服器的整合不受影響，所以測試全綠、
    // curl 全通，而一個瀏覽器客戶端一行也跑不動。
    //
    // 加在**最外層**（`SetRequestIdLayer` 之後、也就是請求最先碰到的地方）：
    // preflight 的 `OPTIONS` 不帶 `Authorization`，若它先經過 `require_auth`
    // 會拿到 401 而不是 CORS 標頭，於是瀏覽器仍然擋掉真正的請求。
    let router = match cors_layer(&cors_origins) {
        Some(cors) => router.layer(cors),
        // **空清單 = 不加這一層。** 不是「允許全部」——
        // 一個沒有設定 CORS_ALLOWED_ORIGINS 的部署得到的是「瀏覽器不能用」，
        // 而那是安全的預設。啟動日誌會說出這件事。
        None => {
            tracing::warn!(
                "CORS_ALLOWED_ORIGINS 未設定 —— 瀏覽器客戶端無法呼叫此 API\
                 （跨來源請求會在 preflight 失敗）。伺服器對伺服器的呼叫不受影響。"
            );
            router
        }
    };

    router.with_state(state)
}

/// 由來源允許清單組出 CORS 層。清單為空時回 `None`。
///
/// 三個判斷值得寫下來：
///
/// **不用 `Any`。** 這個 API 的每一個請求都帶 `Authorization`，而 CORS 規範
/// 不允許「通配來源 + 憑證」—— 瀏覽器會拒絕那個回應。用 `Any` 的結果是
/// 「明明設了 CORS 卻還是被擋」，一個很難查的症狀。
///
/// **允許的標頭是逐一列出的，不是 `Any`。** 這個 API 用了四個非簡單標頭
/// （`X-Tenant-ID`、`If-Match`、`Idempotency-Key`、`X-Request-ID`），
/// 漏掉任何一個，對應的功能就會在瀏覽器裡靜默失效 —— 例如漏了 `If-Match`
/// 的話，所有樂觀鎖的 PATCH 都會失敗，而錯誤看起來像後端問題。
///
/// **`ETag` 必須 expose。** 瀏覽器預設只讓 JS 讀到六個回應標頭，`ETag`
/// 不在其中。不 expose 的話前端拿不到版本號，於是拼不出下一次 PATCH 的
/// `If-Match` —— 樂觀鎖從客戶端的角度就是壞的。
fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    use axum::http::{header, HeaderName, HeaderValue, Method};

    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::error!(origin = %o, "CORS_ALLOWED_ORIGINS 含無法作為標頭值的來源，已忽略");
                None
            }
        })
        .collect();
    if parsed.is_empty() {
        return None;
    }

    Some(
        CorsLayer::new()
            .allow_origin(parsed)
            .allow_credentials(true)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PATCH,
                Method::PUT,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                header::IF_MATCH,
                header::ACCEPT,
                // `from_static` 而不是 `parse().unwrap()`：這些是編譯期已知的
                // 常值，讓它在編譯期就不可能是無效的標頭名。
                HeaderName::from_static("x-tenant-id"),
                HeaderName::from_static("x-facility-id"),
                HeaderName::from_static("idempotency-key"),
                HeaderName::from_static("x-request-id"),
            ])
            // ETag 供 If-Match，X-Request-ID 供客戶端在回報問題時附上關聯 id。
            .expose_headers([header::ETAG, HeaderName::from_static("x-request-id")])
            // preflight 的快取時間。10 分鐘：夠久到不會每個請求都多一次往返，
            // 短到改設定之後不必等太久才生效。
            .max_age(std::time::Duration::from_secs(600)),
    )
}

/// 已實作的端點清單，`(method, openapi.yaml 的 path)`。
///
/// 這份清單是 `tests/contract_conformance.rs` 的輸入，不是裝飾：
/// 該測試會斷言每一項都確實存在於 `api/openapi.yaml`（抓「發明了契約沒有的
/// 端點」與路徑拼錯），且每一項都真的可路由（抓「清單與 router 不同步」）。
///
/// 路徑用契約的參數名（`{reservationId}`）而非 axum 的寫法，
/// 因為比對的對象是契約。
pub const IMPLEMENTED_OPERATIONS: &[(&str, &str)] = &[
    ("post", "/auth/token"),
    ("post", "/auth/token/refresh"),
    ("get", "/auth/me"),
    ("get", "/auth/sso/{providerCode}/authorize"),
    ("get", "/auth/sso/{providerCode}/callback"),
    ("post", "/auth/logout"),
    ("post", "/auth/password/change"),
    // SCIM 2.0。資源名的大寫是規範定義的（`Users`／`Groups`），不是筆誤 ——
    // 路徑在 SCIM 裡是大小寫敏感的。
    ("get", "/scim/v2/Users"),
    ("post", "/scim/v2/Users"),
    ("get", "/scim/v2/Users/{id}"),
    ("patch", "/scim/v2/Users/{id}"),
    ("delete", "/scim/v2/Users/{id}"),
    ("get", "/scim/v2/Groups"),
    ("post", "/scim/v2/Groups"),
    ("get", "/scim/v2/Groups/{id}"),
    ("patch", "/scim/v2/Groups/{id}"),
    ("delete", "/scim/v2/Groups/{id}"),
    ("get", "/facilities/{facilityId}/availability"),
    ("post", "/reservations/holds"),
    ("post", "/reservations/{reservationId}/approve"),
    ("post", "/reservations/{reservationId}/reject"),
    ("get", "/facilities/{facilityId}/occupancy"),
    ("post", "/reservations/{reservationId}/check-in"),
    ("delete", "/reservations/holds/{holdToken}"),
    ("post", "/reservations/{reservationId}/check-out"),
    ("delete", "/reservation-series/{recurrenceGroupId}"),
    ("get", "/facilities/{facilityId}/bookable-resources"),
    ("patch", "/bookable-resources/{resourceId}"),
    ("get", "/amenities"),
    ("get", "/bookable-resources/{resourceId}/amenities"),
    ("put", "/bookable-resources/{resourceId}/amenities"),
    ("delete", "/reservations/{reservationId}"),
    ("get", "/reservations"),
    ("get", "/resource-blackouts"),
    ("post", "/resource-blackouts"),
    ("post", "/reservations"),
    ("get", "/reservations/{reservationId}"),
    ("patch", "/reservations/{reservationId}"),
    ("get", "/assets"),
    ("post", "/assets"),
    ("get", "/assets/{assetId}"),
    ("patch", "/assets/{assetId}"),
    ("delete", "/assets/{assetId}"),
    ("get", "/assets/{assetId}/dependency-graph"),
    ("get", "/asset-models"),
    ("post", "/asset-models"),
    ("get", "/asset-models/{modelId}/compatibility"),
    ("get", "/asset-categories"),
    ("get", "/attribute-definitions"),
    ("post", "/attribute-definitions"),
    ("post", "/assets/{assetId}/relations"),
    ("delete", "/asset-relations/{relationId}"),
    ("post", "/assets:bulk-import"),
    ("get", "/assets/{assetId}/work-orders"),
    ("get", "/assets/{assetId}/status-history"),
    ("get", "/assets/{assetId}/meters/{meterCode}/readings"),
    ("post", "/assets/{assetId}/meters/{meterCode}/readings"),
    ("get", "/organizations"),
    ("get", "/organizations/{organizationId}"),
    ("patch", "/organizations/{organizationId}"),
    ("delete", "/organizations/{organizationId}"),
    ("get", "/spatial-nodes/{nodeId}"),
    ("patch", "/spatial-nodes/{nodeId}"),
    ("delete", "/spatial-nodes/{nodeId}"),
    ("post", "/organizations"),
    ("get", "/facilities"),
    ("post", "/facilities"),
    ("get", "/facilities/{facilityId}"),
    ("patch", "/facilities/{facilityId}"),
    ("get", "/facilities/{facilityId}/spatial-nodes"),
    ("get", "/facilities/{facilityId}/service-items"),
    ("post", "/facilities/{facilityId}/service-items"),
    ("patch", "/service-items/{serviceItemId}"),
    ("delete", "/service-items/{serviceItemId}"),
    ("get", "/service-items/{serviceItemId}/availability"),
    ("get", "/reports/sla-compliance"),
    ("get", "/users"),
    ("post", "/users"),
    ("patch", "/users/{userId}"),
    ("post", "/users/{userId}/suspend"),
    ("get", "/users/{userId}/role-assignments"),
    ("post", "/users/{userId}/role-assignments"),
    ("delete", "/role-assignments/{id}"),
    ("get", "/roles"),
    ("post", "/roles"),
    ("get", "/permissions"),
    ("get", "/audit-log"),
    ("get", "/directory-role-mappings"),
    ("post", "/directory-role-mappings"),
    ("delete", "/directory-role-mappings/{id}"),
    ("post", "/audit-log:export"),
    ("get", "/audit-log/exports/{id}"),
    ("get", "/skills"),
    ("post", "/skills"),
    ("get", "/users/{userId}/skills"),
    ("put", "/users/{userId}/skills/{skillId}"),
    ("post", "/uploads/presign"),
    ("get", "/facilities/{facilityId}/bim-models"),
    ("post", "/facilities/{facilityId}/bim-models"),
    ("get", "/bim-models/{bimModelId}/unresolved-elements"),
    ("get", "/spatial-node-types"),
    ("post", "/facilities/{facilityId}/spatial-nodes:bulk-import"),
    ("get", "/facilities/{facilityId}/floor-view"),
    ("get", "/spatial-nodes/{floorNodeId}/floor-plan-markers"),
    ("post", "/spatial-nodes/{floorNodeId}/floor-plan-markers"),
    ("delete", "/floor-plan-markers/{id}"),
    ("get", "/bim-models/{bimModelId}"),
    ("delete", "/bim-models/{bimModelId}"),
    ("post", "/bim-models/{bimModelId}/reset"),
    ("post", "/bim-models/{bimModelId}/mappings"),
    ("get", "/facilities/{facilityId}/calendar-integrations"),
    ("post", "/facilities/{facilityId}/calendar-integrations"),
    (
        "get",
        "/calendar-integrations/{integrationId}/unresolved-resources",
    ),
    (
        "post",
        "/calendar-integrations/{integrationId}/resource-mappings",
    ),
    ("get", "/reports/facility-dashboard"),
    ("get", "/identity-providers"),
    ("post", "/identity-providers"),
    ("patch", "/identity-providers/{providerId}"),
    ("post", "/identity-providers/{providerId}/test-connection"),
    ("post", "/identity-providers/{providerId}/sync"),
    ("get", "/identity-providers/{providerId}/sync-runs"),
    ("get", "/directory-groups"),
    ("post", "/telemetry:batch-ingest"),
    ("get", "/devices"),
    ("post", "/devices"),
    ("patch", "/devices/{deviceId}"),
    ("get", "/devices/{deviceId}/points"),
    ("get", "/telemetry/latest"),
    ("get", "/telemetry/series"),
    ("get", "/alarm-rules"),
    ("post", "/alarm-rules"),
    ("post", "/alarm-rules/{ruleId}/test"),
    ("get", "/alarms"),
    ("post", "/alarms/{alarmId}/acknowledge"),
    ("post", "/alarms/{alarmId}/suppress"),
    ("post", "/alarms:reconcile-work-orders"),
    ("post", "/alarms/{alarmId}/work-order"),
    ("get", "/sla-policies"),
    ("post", "/sla-policies"),
    ("patch", "/sla-policies/{slaPolicyId}"),
    ("get", "/holiday-calendars"),
    ("post", "/holiday-calendars"),
    ("patch", "/holiday-calendars/{holidayId}"),
    ("delete", "/holiday-calendars/{holidayId}"),
    ("get", "/notifications"),
    ("get", "/webhooks"),
    ("post", "/webhooks"),
    ("post", "/notifications/{notificationId}/read"),
    ("get", "/notification-templates"),
    ("post", "/notification-templates"),
    ("patch", "/notification-templates/{templateId}"),
    ("delete", "/notification-templates/{templateId}"),
    ("post", "/facilities/{facilityId}/spatial-nodes"),
    ("get", "/attachments"),
    ("post", "/attachments"),
    ("get", "/attachments/{attachmentId}"),
    ("delete", "/attachments/{attachmentId}"),
    ("get", "/maintenance-plans"),
    ("post", "/maintenance-plans"),
    ("get", "/maintenance-plans/{planId}/preview-schedule"),
    ("patch", "/maintenance-plans/{planId}"),
    ("post", "/maintenance-plans/{planId}/generate-now"),
    ("get", "/maintenance-templates"),
    ("post", "/maintenance-templates"),
    ("get", "/maintenance-occurrences"),
    ("post", "/maintenance-occurrences/{occurrenceId}/skip"),
    ("get", "/reports/pm-compliance"),
    ("get", "/reports/group-rollup"),
    ("get", "/reports/asset-reliability"),
    ("get", "/reports/space-utilization"),
    ("get", "/reports/service-volume"),
    ("post", "/work-orders/{workOrderId}/satisfaction"),
    ("get", "/work-order-statuses"),
    ("get", "/work-order-state-machine"),
    ("post", "/work-orders:bulk-transition"),
    ("get", "/teams"),
    ("get", "/teams/{teamId}/workload"),
    ("get", "/teams/{teamId}/shifts"),
    ("post", "/teams/{teamId}/shifts"),
    ("get", "/parts"),
    ("get", "/part-stock"),
    ("get", "/tenant"),
    ("patch", "/tenant"),
    ("post", "/reports/{reportCode}:export"),
    ("get", "/reports/exports/{id}"),
    ("get", "/work-orders"),
    ("post", "/work-orders"),
    ("get", "/work-orders/{workOrderId}"),
    ("patch", "/work-orders/{workOrderId}"),
    ("post", "/work-orders/{workOrderId}/transitions"),
    ("get", "/work-orders/{workOrderId}/available-actions"),
    ("patch", "/work-orders/{workOrderId}/tasks/{taskId}"),
    ("get", "/work-orders/{workOrderId}/tasks"),
    ("post", "/work-orders/{workOrderId}/labor"),
    ("post", "/work-orders/{workOrderId}/parts"),
    ("post", "/work-orders/{workOrderId}/comments"),
];

/// 存活探針。不在 openapi.yaml 內，屬營運端點。
async fn health() -> &'static str {
    "ok"
}

/// 由設定建立物件儲存客戶端。
///
/// 刻意與 `build_state` 分開並在啟動時就建立：設定缺漏要在**啟動失敗**，
/// 而不是等到第一個上傳請求才回 500。附件沒有「安靜降級」的合理行為。
pub fn build_storage() -> anyhow::Result<fms_shared::Storage> {
    let settings = fms_shared::StorageSettings::from_env()
        .map_err(|e| anyhow::anyhow!("object storage is not configured: {e}"))?;
    Ok(fms_shared::Storage::new(&settings))
}

/// 由設定建立狀態。連線字串必須是 `fms_app`。
pub async fn build_state(settings: Settings) -> anyhow::Result<identity::IdentityState> {
    let pool = PgPoolOptions::new()
        .max_connections(settings.database.max_connections)
        .connect(&settings.database.url)
        .await?;

    Ok(identity::IdentityState::new(pool, settings))
}
