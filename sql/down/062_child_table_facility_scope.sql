-- 回退 062。**這會重新打開 14 張子表的跨場域洩漏**（那是 062 之前的狀態）。
--
-- 用同一份對應表刪除，而不是硬寫 14 條 DROP POLICY —— 兩份清單會漂移，
-- 而漂移的症狀是「回退之後某一張表還留著政策」，schema 比對才會發現。

SET app.is_platform = 'on';

BEGIN;

DO $$
DECLARE v_t text;
BEGIN
  FOR v_t IN
    SELECT tablename FROM pg_policies
     WHERE schemaname = 'fms' AND policyname = 'facility_scope_via_parent'
  LOOP
    EXECUTE format('DROP POLICY IF EXISTS facility_scope_via_parent ON fms.%I', v_t);
  END LOOP;
END;
$$;

COMMIT;
