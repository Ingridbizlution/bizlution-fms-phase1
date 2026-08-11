-- 回退 068。純函式與一個約束，沒有資料要回捲。
--
-- `availability` 欄位是 004 建的，留著 —— 回退這支不該讓管理者設好的
-- 可用時段消失。
BEGIN;
DROP FUNCTION IF EXISTS fms.service_item_windows(uuid, date);
ALTER TABLE fms.service_items DROP CONSTRAINT IF EXISTS ck_service_items_availability;
DROP FUNCTION IF EXISTS fms.service_availability_is_valid(jsonb);
DROP FUNCTION IF EXISTS fms.is_a_date(text);
COMMIT;
