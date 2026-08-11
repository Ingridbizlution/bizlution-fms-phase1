-- 回退 020：facility_scope 恢復為 007 的「只有 USING」版本。
--
-- 注意 PostgreSQL 會把 FOR ALL 的 USING 同時當作 WITH CHECK，
-- 因此回退後**建立場域會再次變成不可能**（見 020 檔頭）。
BEGIN;
SET search_path = fms, public;

DROP POLICY IF EXISTS facility_scope ON fms.facilities;
CREATE POLICY facility_scope ON fms.facilities AS RESTRICTIVE FOR ALL
  USING (fms.is_platform_context() OR fms.facility_in_scope(id));

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_policies
              WHERE schemaname='fms' AND tablename='facilities'
                AND policyname='facility_scope' AND with_check IS NOT NULL) THEN
    RAISE EXCEPTION 'down 020 FAILED: WITH CHECK 仍然存在';
  END IF;
END; $$;

COMMIT;
