-- 回退 027。還原成 008 的單一 facility:write（宣告 FACILITY），
-- 並還原角色對應：PLATFORM_ADMIN 與 TENANT_ADMIN 由萬用授權取得，
-- ORG_MANAGER 與 FACILITY_ADMIN 由 008 第 129 行的明列取得。
--
-- 這會讓「建立場域」重新失去權限層的守衛（退回只靠 007 的 facility_scope
-- 政策擋），只為 roundtrip 驗證而存在。
BEGIN;
SET search_path = fms, public;

INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous)
VALUES ('facility:write', 'facility', 'write', 'CORE', '維護設施資料', 'FACILITY', false)
ON CONFLICT (code) DO UPDATE
  SET resource = EXCLUDED.resource,
      action = EXCLUDED.action,
      module = EXCLUDED.module,
      description = EXCLUDED.description,
      min_scope_level = EXCLUDED.min_scope_level;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'facility:write'
FROM fms.roles r
WHERE r.code IN ('PLATFORM_ADMIN', 'TENANT_ADMIN', 'ORG_MANAGER', 'FACILITY_ADMIN')
ON CONFLICT DO NOTHING;

-- CASCADE 會一併清掉 role_permissions 的列
DELETE FROM fms.permissions WHERE code IN ('facility:create', 'facility:update');

COMMIT;
