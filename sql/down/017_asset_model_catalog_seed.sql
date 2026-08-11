-- 回退 017：移除型錄種子。
--
-- 先解除設備對型號的參照：`assets.asset_model_id` 是 ON DELETE SET NULL，
-- 但明確解除比依賴 cascade 清楚，也讓「哪些設備受影響」在 down 裡看得見。
BEGIN;
SET search_path = fms, public;
SELECT set_config('app.is_platform', 'on', true);

UPDATE fms.assets SET asset_model_id = NULL
 WHERE asset_model_id IN (
   SELECT id FROM fms.asset_models
    WHERE lower(model_no) IN ('dph-100k','csaa-020','sp4k-15c','demo-fcu-450'));

DELETE FROM fms.asset_models
 WHERE lower(model_no) IN ('dph-100k','csaa-020','sp4k-15c','demo-fcu-450');

COMMIT;
