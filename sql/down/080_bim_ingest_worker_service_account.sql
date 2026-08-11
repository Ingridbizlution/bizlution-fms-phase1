-- 回退 080：移除 BIM 匯入解析器的服務帳號與 BIM_INGEST_WORKER 角色。
--
-- 順序：先解除指派、再刪使用者、最後刪角色（外鍵方向），與 019/078 同一個手法。
-- 若該帳號已經建立過資產，`assets.custodian_user_id` 之類的外鍵是
-- ON DELETE SET NULL（見 003），刪除是安全的。
BEGIN;
SET search_path = fms, public;
SELECT set_config('app.is_platform', 'on', true);

DELETE FROM fms.user_role_assignments
 WHERE user_id = 'f5000000-0000-4000-8000-000000000003';

DELETE FROM fms.users WHERE id = 'f5000000-0000-4000-8000-000000000003';

DELETE FROM fms.role_permissions
 WHERE role_id IN (SELECT id FROM fms.roles WHERE code = 'BIM_INGEST_WORKER' AND tenant_id IS NULL);

DELETE FROM fms.roles WHERE code = 'BIM_INGEST_WORKER' AND tenant_id IS NULL;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM fms.roles WHERE code = 'BIM_INGEST_WORKER') THEN
    RAISE EXCEPTION 'down 080 FAILED: BIM_INGEST_WORKER 角色仍在';
  END IF;
END; $$;

COMMIT;
