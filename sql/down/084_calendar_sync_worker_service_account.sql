-- 回退 084：移除日曆同步服務帳號與 CALENDAR_SYNC_WORKER 角色。
--
-- 順序：先解除指派、再刪使用者、最後刪角色（外鍵方向），與 019/078/080 同一個手法。
BEGIN;
SET search_path = fms, public;
SELECT set_config('app.is_platform', 'on', true);

DELETE FROM fms.user_role_assignments
 WHERE user_id = 'f5000000-0000-4000-8000-000000000004';

DELETE FROM fms.users WHERE id = 'f5000000-0000-4000-8000-000000000004';

DELETE FROM fms.role_permissions
 WHERE role_id IN (SELECT id FROM fms.roles WHERE code = 'CALENDAR_SYNC_WORKER' AND tenant_id IS NULL);

DELETE FROM fms.roles WHERE code = 'CALENDAR_SYNC_WORKER' AND tenant_id IS NULL;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM fms.roles WHERE code = 'CALENDAR_SYNC_WORKER') THEN
    RAISE EXCEPTION 'down 084 FAILED: CALENDAR_SYNC_WORKER 角色仍在';
  END IF;
END; $$;

COMMIT;
