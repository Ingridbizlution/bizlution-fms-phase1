-- =============================================================================
-- Down migration 053：拿掉「租戶範圍讀者可讀租戶級稽核列」那條分支
-- =============================================================================
-- 還原成 046 的三條分支。後果是租戶級稽核列（身分與授權的整條軌跡）
-- 連租戶管理員都讀不到 —— 那是 046 的行為，不是改善。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

DROP POLICY IF EXISTS facility_scope ON fms.audit_log;

CREATE POLICY facility_scope ON fms.audit_log
AS RESTRICTIVE FOR SELECT
USING (fms.is_platform_context()
       OR fms.current_facility_ids() IS NULL
       OR facility_id = ANY (fms.current_facility_ids()));

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_policy
              WHERE polrelid = 'fms.audit_log'::regclass
                AND polname = 'facility_scope'
                AND pg_get_expr(polqual, polrelid) LIKE '%tenant_wide_write_allowed%') THEN
    RAISE EXCEPTION 'down 053 FAILED: 那條分支還在';
  END IF;
  RAISE NOTICE 'down 053 OK';
END;
$$;

COMMIT;
