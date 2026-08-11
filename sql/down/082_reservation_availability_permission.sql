-- 回退 082：刪權限碼（連帶清掉 role_permissions，理由與 040／037 相同）。
SET app.is_platform = 'on';
BEGIN;
SET search_path = fms, public;
DELETE FROM fms.permissions WHERE code = 'reservation:read_availability';
COMMIT;
