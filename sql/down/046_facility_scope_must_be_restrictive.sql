-- 回退 046：把兩條政策還原成 038／029 當時的樣子。
--
-- **這會把 holiday_calendars 的租戶隔離洞放回去**（facility_scope 變成
-- PERMISSIVE，於是 OR 掉 tenant_isolation）。回退的定義就是回到當時的行為，
-- 而當時的行為是有洞的 —— 記在這裡，不要以為這個 down 是無害的。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DROP POLICY IF EXISTS facility_scope ON fms.holiday_calendars;
CREATE POLICY facility_scope ON fms.holiday_calendars
  USING (fms.is_platform_context() OR fms.facility_in_scope(facility_id))
  WITH CHECK (fms.is_platform_context() OR fms.facility_in_scope(facility_id));

DROP POLICY IF EXISTS facility_scope ON fms.audit_log;
CREATE POLICY facility_scope ON fms.audit_log
  AS RESTRICTIVE
  USING (fms.is_platform_context() OR fms.facility_in_scope(facility_id));

COMMIT;
