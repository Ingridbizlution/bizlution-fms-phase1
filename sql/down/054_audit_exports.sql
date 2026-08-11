-- =============================================================================
-- Down migration 054：移除稽核匯出作業表
-- =============================================================================
-- 連同已產出的匯出紀錄一起消失。**S3 上的物件不會被刪除** ——
-- 這個 down migration 摸不到物件儲存，那些檔案會變成孤兒。
-- 若要真的回退，先自行清理 bucket 裡 `audit-exports/` 前綴下的物件。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

DROP TABLE IF EXISTS fms.audit_exports;

DO $$
BEGIN
  IF to_regclass('fms.audit_exports') IS NOT NULL THEN
    RAISE EXCEPTION 'down 054 FAILED: audit_exports 仍然存在';
  END IF;
  RAISE NOTICE 'down 054 OK（提醒：S3 上的匯出物件未被刪除）';
END;
$$;

COMMIT;
