-- 回退 065。四支都是純讀取函式，沒有資料要回捲。
--
-- `ck_bookable_opening_hours` 一起拿掉：它是 065 為了讓 opening_hours
-- 能當分母才加的。留著會讓回退後的 schema 與 064 不一致。
BEGIN;
DROP FUNCTION IF EXISTS fms.report_group_rollup(date, date, uuid);
DROP FUNCTION IF EXISTS fms.report_asset_reliability(date, date, uuid, int);
DROP FUNCTION IF EXISTS fms.report_space_utilization(date, date, uuid);
DROP FUNCTION IF EXISTS fms.report_service_volume(date, date, text);
ALTER TABLE fms.bookable_resources DROP CONSTRAINT IF EXISTS ck_bookable_opening_hours;
DROP FUNCTION IF EXISTS fms.time_windows_hours(jsonb);
COMMIT;
