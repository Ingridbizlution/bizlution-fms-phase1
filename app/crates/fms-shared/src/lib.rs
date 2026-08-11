//! 跨模組共用層：Problem Details、租戶情境資料庫存取、請求情境、設定。
//!
//! 這裡放的是「所有 165 個端點都會用到」的東西。橫切關注點集中在此，
//! 因為它們的改動成本隨端點數線性放大（ADR-09）。
//!
//! 橫切項目：Problem Details、租戶情境、cursor 分頁（`page`）、
//! 樂觀鎖與冪等（`concurrency`）。後兩者隨 reservations 切片實作 ——
//! auth 端點沒有列表／樂觀鎖／建立類 POST，先做會是憑空抽象。

pub mod body;
pub mod concurrency;
pub mod config;
pub mod context;
pub mod cron;
pub mod db;
pub mod fields;
pub mod form_schema;
pub mod include;
pub mod observability;
pub mod page;
pub mod problem;
pub mod safe_http;
pub mod schedule;
pub mod scope;
pub mod secrets;
pub mod storage;

pub use body::OptionalJson;
pub use concurrency::{
    check_version, optional_if_match, required_if_match, Idempotency, PendingReplay,
};
pub use config::{DatabaseSettings, JwtSettings, LoginThrottleSettings, Settings};
pub use context::{verify_tenant_header, ActorType, Caller};
pub use db::{
    begin_tenant_tx, has_permission, permission_codes, refresh_facility_scope, require_permission,
    require_tenant_scoped_permission, Authorized, TenantContext, TenantTx,
};
pub use include::Includes;
pub use observability::{init_telemetry, otlp_endpoint, tenant_span, TelemetryGuard};
pub use page::{clamp_limit, Cursor, PageMeta, Paged, SortSpec};
pub use problem::{FieldError, Problem, ProblemCode};
pub use safe_http::{OutboundSettings, Rejected as OutboundRejected};
pub use scope::{deny_unless_own, read_scope, ReadScope};
pub use secrets::{EnvSecretResolver, ResolveError, Secret, SecretResolver, StaticSecretResolver};
pub use storage::{object_key, Storage, StorageSettings};
