-- 回退 045：把五筆授權加回去（也就是把那五句謊話加回目錄）。
--
-- 需要平台情境（動 role_permissions，029 的稽核觸發器掛在上面）。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code
FROM fms.roles r,
     (VALUES ('audit:read'), ('identity_provider:read'), ('role:read'), ('tenant:read'))
       AS c(code)
WHERE r.code = 'VIEWER'
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'maintenance_template:write'
FROM fms.roles r WHERE r.code = 'MAINTENANCE_SUPERVISOR'
ON CONFLICT DO NOTHING;

COMMIT;
