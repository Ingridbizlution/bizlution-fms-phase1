//! 身分與存取控制模組（規格書 L2 的 identity 模組）。
//!
//! 本模組是 auth 垂直切片的載體：它同時驗證了跨模組的共用機制
//! （Problem Details、`fms.set_context()` 注入、`X-Tenant-ID` 一致性、
//! 權限委派給 `fms.user_has_permission()`）能不能接得起來。

pub mod audit;
pub mod audit_export;
pub mod directory_groups;
pub mod directory_mappings;
pub mod directory_sync_watchdog;
pub mod dto;
pub mod handlers;
pub mod identity_providers;
pub mod jwt;
pub mod password;
pub mod repo;
pub mod role_assignments;
pub mod roles;
pub mod scim;
pub mod skills;
pub mod sso;
pub mod throttle;
pub mod users;

pub use audit::AuditState;
pub use audit_export::AuditExportState;
pub use directory_groups::DirectoryGroupsState;
pub use directory_mappings::DirectoryMappingsState;
pub use handlers::{require_auth, IdentityState};
pub use identity_providers::IdentityProvidersState;
pub use role_assignments::RoleAssignmentsState;
pub use roles::RolesState;
pub use scim::{require_scim_token, ScimState};
pub use skills::SkillsState;
pub use throttle::LoginThrottle;
pub use users::UsersState;
