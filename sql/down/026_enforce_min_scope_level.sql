-- 回退 026。**這會讓 min_scope_level 重新變成無人執行的宣告**，
-- 也就是重新開啟「場域級授權可以執行租戶級動作」，只為 roundtrip 驗證而存在。
--
-- 視圖還原成 002 的定義（逐字），宣告還原成 008 的原值。
BEGIN;
SET search_path = fms, public;

CREATE OR REPLACE VIEW fms.v_user_effective_permissions AS
SELECT
  ura.tenant_id,
  ura.user_id,
  rp.permission_code,
  p.module,
  p.resource,
  p.action,
  ura.scope_type,
  ura.scope_id,
  r.code AS role_code
FROM fms.user_role_assignments ura
JOIN fms.roles r            ON r.id = ura.role_id
JOIN fms.role_permissions rp ON rp.role_id = r.id
JOIN fms.permissions p       ON p.code = rp.permission_code
WHERE ura.valid_from <= clock_timestamp()
  AND (ura.valid_until IS NULL OR ura.valid_until > clock_timestamp());

-- 002 沒有給這個視圖 COMMENT，因此還原成沒有，而不是寫回一段從未存在的文字。
COMMENT ON VIEW fms.v_user_effective_permissions IS NULL;

UPDATE fms.permissions SET min_scope_level = 'TENANT'
 WHERE code IN ('organization:write','organization:read','asset_model:read','user:read');

DROP FUNCTION IF EXISTS fms.scope_width(text);

COMMIT;
