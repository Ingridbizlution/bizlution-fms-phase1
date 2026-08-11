-- 回退 034：拆掉報表函式。純讀取，沒有資料要還原。
BEGIN;
SET search_path = fms, public;
DROP FUNCTION IF EXISTS fms.report_sla_compliance(text, date, date, text);
COMMIT;
