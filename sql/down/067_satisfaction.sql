-- 回退 067。
--
-- **不清 `satisfaction_score`／`satisfaction_comment`**：那是使用者親自輸入的
-- 資料，不是衍生值。欄位是 004 建的，回退這支不該讓它們消失。
--
-- 已經發出的 SATISFACTION_REQUEST 通知也留著：它們是「那件事發生過」的紀錄，
-- 而 `request_satisfaction()` 正是用它們判斷有沒有邀請過。刪掉會讓重新套用
-- 之後對同一批工單再邀請一次。
BEGIN;
DROP FUNCTION IF EXISTS fms.request_satisfaction(uuid);
DROP FUNCTION IF EXISTS fms.tenant_setting_int(text, int);
ALTER TABLE fms.tenants DROP CONSTRAINT IF EXISTS ck_tenants_settings;
DROP FUNCTION IF EXISTS fms.tenant_settings_are_valid(jsonb);
DELETE FROM fms.notification_templates
 WHERE code = 'SATISFACTION_REQUEST' AND tenant_id IS NULL;
COMMIT;
