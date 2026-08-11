-- =============================================================================
-- Down: 051_backup_role_grants
-- =============================================================================
-- 收回 fms_backup 對 fms schema 的讀取權。角色本身不動 —— 它是 initdb 建的，
-- 不屬於 migration 的範圍（BYPASSRLS 需要超級使用者）。
--
-- 收回之後 `make backup` 會失敗（permission denied for schema fms），
-- 而備份演練的 [0] 會先一步把原因講清楚。那是預期的：down 就是要把狀態
-- 放回去。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'fms_backup') THEN
    ALTER DEFAULT PRIVILEGES FOR ROLE fms_owner IN SCHEMA fms
      REVOKE SELECT ON TABLES FROM fms_backup;
    ALTER DEFAULT PRIVILEGES FOR ROLE fms_owner IN SCHEMA fms
      REVOKE SELECT ON SEQUENCES FROM fms_backup;
    REVOKE SELECT ON ALL SEQUENCES IN SCHEMA fms FROM fms_backup;
    REVOKE SELECT ON ALL TABLES IN SCHEMA fms FROM fms_backup;
    REVOKE USAGE ON SCHEMA fms FROM fms_backup;
  END IF;
END;
$$;

COMMENT ON SCHEMA fms IS NULL;

COMMIT;
