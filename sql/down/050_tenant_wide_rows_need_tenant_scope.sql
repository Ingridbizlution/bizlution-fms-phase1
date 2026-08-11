-- =============================================================================
-- Down: 050_tenant_wide_rows_need_tenant_scope
-- =============================================================================
-- 回到 037／038 的形狀：`facility_scope` 是 ALL 且沒有明寫 WITH CHECK，
-- 於是寫入檢查退回用 USING，而那對 NULL 一律放行 ——
-- 也就是**把缺口放回去**。
--
-- 兩支函式一併移除。順序要對：`facility_write_in_scope` 依賴
-- `tenant_wide_write_allowed`，而政策依賴前者，所以先拆政策。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DO $$
DECLARE
  t text;
  scoped text[] := ARRAY[
    'alarm_rules', 'announcements', 'holiday_calendars', 'integrations',
    'quota_policies', 'service_items', 'sla_policies', 'teams'
  ];
BEGIN
  FOREACH t IN ARRAY scoped LOOP
    EXECUTE format('DROP POLICY IF EXISTS facility_scope_update ON fms.%I', t);
    EXECUTE format('DROP POLICY IF EXISTS facility_scope_delete ON fms.%I', t);

    EXECUTE format('DROP POLICY IF EXISTS facility_scope ON fms.%I', t);
    -- 037／038 的原形：沒有明寫 WITH CHECK。
    EXECUTE format(
      'CREATE POLICY facility_scope ON fms.%I
         AS RESTRICTIVE
         USING (fms.is_platform_context() OR fms.facility_in_scope(facility_id))', t);
  END LOOP;
END;
$$;

DROP FUNCTION IF EXISTS fms.facility_write_in_scope(uuid);
DROP FUNCTION IF EXISTS fms.tenant_wide_write_allowed();

COMMIT;
