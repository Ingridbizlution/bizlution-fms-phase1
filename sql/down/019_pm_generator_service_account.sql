-- 回退 019：移除 PM 產生器的服務帳號與 PM_GENERATOR 角色。
--
-- 順序：先解除指派、再刪使用者、最後刪角色（外鍵方向）。
-- 若該帳號已經建立過工單，`work_orders.created_by` 是 ON DELETE SET NULL，
-- 因此刪除是安全的 —— 工單留下，只是失去建立者。
BEGIN;
SET search_path = fms, public;
SELECT set_config('app.is_platform', 'on', true);

DELETE FROM fms.user_role_assignments
 WHERE user_id = 'f5000000-0000-4000-8000-000000000001';

DELETE FROM fms.users WHERE id = 'f5000000-0000-4000-8000-000000000001';

DELETE FROM fms.role_permissions
 WHERE role_id IN (SELECT id FROM fms.roles WHERE code = 'PM_GENERATOR' AND tenant_id IS NULL);

DELETE FROM fms.roles WHERE code = 'PM_GENERATOR' AND tenant_id IS NULL;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM fms.roles WHERE code = 'PM_GENERATOR') THEN
    RAISE EXCEPTION 'down 019 FAILED: PM_GENERATOR 角色仍在';
  END IF;
END; $$;

COMMIT;
