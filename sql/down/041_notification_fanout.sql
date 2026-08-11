-- 回退 041：拆掉扇出函式、移除規則上的 template 鍵、移除 IN_APP 範本。
--
-- **已經建立的 notifications 列不刪。** 那些是「某個人被通知過」的紀錄，
-- 而回退機制不該抹掉紀錄。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DROP FUNCTION IF EXISTS fms.fan_out_notifications(bigint);
DROP FUNCTION IF EXISTS fms.notification_recipients(uuid, jsonb);
DROP FUNCTION IF EXISTS fms.notification_vars(uuid);
DROP FUNCTION IF EXISTS fms.render_template(text, jsonb);

UPDATE fms.work_order_transitions_allowed
   SET side_effects = side_effects - 'template'
 WHERE side_effects ? 'template';

DELETE FROM fms.notification_templates
 WHERE code = 'WO_SLA_BREACH' AND channel = 'IN_APP';

DROP INDEX IF EXISTS fms.uq_notifications_event_recipient;
-- `source_event_id` 欄位**不移除**：既有通知列上的值是「這筆通知是哪個事件
-- 產生的」，屬於紀錄的一部分。留一個沒有人寫的欄位比丟掉來源好。

COMMIT;
