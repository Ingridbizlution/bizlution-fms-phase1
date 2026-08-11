-- 回退 015：移除動作標籤 catalog。
--
-- 只是顯示用資料，沒有任何東西以外鍵參照它，因此直接 DROP 是安全的。
-- 回退後 `available-actions` 的 `label_zh` 會變成 null ——
-- 那正是 015 之前的狀態。
BEGIN;
SET search_path = fms, public;
DROP TABLE IF EXISTS fms.work_order_actions;
COMMIT;
