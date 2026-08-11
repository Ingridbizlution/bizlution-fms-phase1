-- =============================================================================
-- Down: 048_org_manager_writes_sla_policies
-- =============================================================================
-- 收回授權。ORG_MANAGER 回到「看得到但改不了」——
-- 也就是 LOW／URGENT 的分鐘數又只能由 TENANT_ADMIN 或 FACILITY_ADMIN 訂。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DELETE FROM fms.role_permissions rp
 USING fms.roles r
 WHERE r.id = rp.role_id
   AND r.code = 'ORG_MANAGER'
   AND rp.permission_code = 'sla_policy:write';

COMMIT;
