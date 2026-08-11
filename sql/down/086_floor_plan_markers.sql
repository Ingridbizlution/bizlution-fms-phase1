-- 回退 086。
--
-- DROP TABLE 會讓所有已標定的設備位置一起消失——樓層平面圖影像本身（存在
-- fms.attachments）不受影響，只有「圖上哪個點是哪個設備」這件事會不見。
BEGIN;
SET search_path = fms, public;

DROP TABLE IF EXISTS fms.floor_plan_markers;

COMMIT;
