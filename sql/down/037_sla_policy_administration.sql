-- 回退 037。
--
-- 刪權限碼會連帶清掉 role_permissions 的授權列（ON DELETE CASCADE），
-- 包含客戶自訂角色的 —— 與 027 的判斷相同：留下一個沒有任何程式讀取的碼，
-- 只會讓下一個人以為它還有效。
--
-- 需要平台情境（動 role_permissions，029 的稽核觸發器掛在上面）。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DROP INDEX IF EXISTS fms.uq_sla_policies_scope;
DELETE FROM fms.permissions WHERE code IN ('sla_policy:read', 'sla_policy:write');

COMMIT;
