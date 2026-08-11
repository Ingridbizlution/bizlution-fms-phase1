-- 回退 040：刪權限碼（連帶清掉 role_permissions，理由與 027／037 相同）。
SET app.is_platform = 'on';
BEGIN;
SET search_path = fms, public;
DELETE FROM fms.permissions WHERE code IN ('holiday:read', 'holiday:write');
COMMIT;
