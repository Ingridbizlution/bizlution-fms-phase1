-- 回退 078：移除 DIRECTORY_SYNC 角色與權限，並把 run_type 約束還原成
-- 002 的四值版本。
--
-- 079 的 down 必須先跑（刪服務帳號與指派）—— migrate-down.sh 本身就是
-- 由高到低逆序執行，因此 079 一定先於 078。
BEGIN;
SET search_path = fms, public;
SELECT set_config('app.is_platform', 'on', true);

DELETE FROM fms.role_permissions
 WHERE role_id IN (SELECT id FROM fms.roles WHERE code = 'DIRECTORY_SYNC' AND tenant_id IS NULL);

DELETE FROM fms.roles WHERE code = 'DIRECTORY_SYNC' AND tenant_id IS NULL;

ALTER TABLE fms.directory_sync_runs
  DROP CONSTRAINT directory_sync_runs_run_type_check;

-- 與 002 的定義逐字相同：roundtrip 比對的是 pg_get_constraintdef 全文。
ALTER TABLE fms.directory_sync_runs
  ADD CONSTRAINT directory_sync_runs_run_type_check
  CHECK (run_type IN ('FULL', 'DELTA', 'SCIM_PUSH', 'MANUAL'));

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM fms.roles WHERE code = 'DIRECTORY_SYNC') THEN
    RAISE EXCEPTION 'down 078 FAILED: DIRECTORY_SYNC 角色仍在';
  END IF;
END; $$;

COMMIT;
