-- =============================================================================
-- Down migration 058：移除目錄同步的對帳
-- =============================================================================
-- 回退後 directory_role_mappings 又變回一份沒有人讀的規則。
-- **已經產生的 source = DIRECTORY_SYNC 授權不動** —— 那是業務資料，
-- 而且回退不該讓任何人突然失去存取。
-- POST /identity-providers/{id}/sync 會因為找不到函式而整支失敗
-- （不是靜默什麼都不做）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

DROP FUNCTION IF EXISTS fms.reconcile_directory_roles(uuid, uuid);

DO $$
BEGIN
  IF to_regprocedure('fms.reconcile_directory_roles(uuid,uuid)') IS NOT NULL THEN
    RAISE EXCEPTION 'down 058 FAILED: 函式仍然存在';
  END IF;
  RAISE NOTICE 'down 058 OK';
END;
$$;

COMMIT;
