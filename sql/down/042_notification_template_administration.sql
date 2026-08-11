-- 回退 042：還原 041 的 fan_out（沒有覆寫優先序）、刪權限碼與 placeholder 函式。
--
-- **租戶建立的覆寫範本不刪。** 那是他們寫的文案，而回退機制不該丟掉內容。
-- 代價是回退之後那些覆寫會回到「有時候生效、有時候不生效」的狀態 ——
-- 那正是 042 要修的東西，因此回退就是回到那個狀態。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

CREATE OR REPLACE FUNCTION fms.fan_out_notifications(p_event_id bigint)
RETURNS TABLE (created int, no_template int, unresolved int)
LANGUAGE plpgsql
AS $$
DECLARE
  v_ev        record;
  v_rule      record;
  v_vars      jsonb;
  v_created   int := 0;
  v_missing   int := 0;
  v_unres     int := 0;
BEGIN
  SELECT tenant_id, event_type, aggregate_type, aggregate_id, payload
    INTO v_ev
    FROM fms.event_outbox WHERE id = p_event_id;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'event % not found', p_event_id USING ERRCODE = 'P0002';
  END IF;

  -- 目前只有工單的狀態機會宣告 notify。其他 aggregate（告警、預約）
  -- 也有範本，但它們的 notify 清單還不存在 —— 那是另一件事。
  IF v_ev.aggregate_type <> 'WORK_ORDER' THEN
    RETURN QUERY SELECT 0, 0, 0;
    RETURN;
  END IF;

  -- 事件的 payload 帶了 `from` 與 `action`，因此可以找回當初那條規則
  -- （notify 與 template 都住在它的 side_effects 裡）。
  SELECT side_effects INTO v_rule
    FROM fms.work_order_transitions_allowed t
   WHERE t.is_active
     AND t.from_status = v_ev.payload ->> 'from'
     AND t.action = v_ev.payload ->> 'action'
     AND (t.tenant_id IS NULL OR t.tenant_id = v_ev.tenant_id)
   ORDER BY t.tenant_id NULLS LAST
   LIMIT 1;

  IF v_rule.side_effects IS NULL OR NOT (v_rule.side_effects ? 'notify') THEN
    RETURN QUERY SELECT 0, 0, 0;   -- 這條轉移不通知任何人，正常情況
    RETURN;
  END IF;

  v_vars := fms.notification_vars(v_ev.aggregate_id);

  -- 沒有 template 鍵 → 找不到要用哪個範本 → 沒有人會收到。
  IF NOT (v_rule.side_effects ? 'template') THEN
    RETURN QUERY SELECT 0, 1, 0;
    RETURN;
  END IF;

  -- 每個 (收件人 × 範本頻道) 一筆。同一個範本碼可以有多個頻道
  -- （EMAIL 給外部、IN_APP 給站內），而收件人不必選 —— 兩者都建。
  WITH people AS (
    SELECT DISTINCT r.user_id
      FROM fms.notification_recipients(v_ev.aggregate_id,
                                       v_rule.side_effects -> 'notify') r
     WHERE r.user_id IS NOT NULL
  ), tpl AS (
    SELECT nt.channel, nt.code, nt.subject_template, nt.body_template
      FROM fms.notification_templates nt
     WHERE lower(nt.code) = lower(v_rule.side_effects ->> 'template')
       AND nt.is_active
       AND (nt.tenant_id IS NULL OR nt.tenant_id = v_ev.tenant_id)
  ), inserted AS (
    INSERT INTO fms.notifications
      (tenant_id, recipient_user_id, channel, template_code,
       subject, body, entity_type, entity_id, priority, source_event_id)
    SELECT v_ev.tenant_id, p.user_id, t.channel, t.code,
           fms.render_template(t.subject_template, v_vars),
           fms.render_template(t.body_template, v_vars),
           'WORK_ORDER', v_ev.aggregate_id,
           CASE WHEN v_ev.event_type = 'work_order.sla_breached'
                THEN 'HIGH' ELSE 'NORMAL' END,
           p_event_id
      FROM people p CROSS JOIN tpl t
    -- 重放時什麼都不做（見本檔第 0 節）。因此 `created` 在重放時是 0，
    -- 而那是正確的答案：這一輪沒有建立新的通知。
    --
    -- `WHERE source_event_id IS NOT NULL` 必須重述：索引是部分索引，
    -- 而 ON CONFLICT 的推斷要看得到同一個述詞才對得上。少了它，
    -- PostgreSQL 回「there is no unique or exclusion constraint matching
    -- the ON CONFLICT specification」—— 而那個錯誤只會在真的有事件要扇出時
    -- 才出現（空資料庫上的自我驗證看不到）。
    ON CONFLICT (source_event_id, recipient_user_id, channel)
      WHERE source_event_id IS NOT NULL
      DO NOTHING
    RETURNING 1
  )
  SELECT count(*)::int INTO v_created FROM inserted;

  SELECT count(*)::int INTO v_unres
    FROM fms.notification_recipients(v_ev.aggregate_id,
                                     v_rule.side_effects -> 'notify') r
   WHERE r.user_id IS NULL;

  -- 範本碼在 notification_templates 裡不存在 → 沒有人會收到。
  --
  -- **不能用 `v_created = 0` 判斷**：重放時 ON CONFLICT 讓 created 也是 0，
  -- 而那不是缺範本。直接問範本存不存在。
  IF NOT EXISTS (
    SELECT 1 FROM fms.notification_templates nt
     WHERE lower(nt.code) = lower(v_rule.side_effects ->> 'template') AND nt.is_active
  ) THEN
    v_missing := 1;
  END IF;

  RETURN QUERY SELECT v_created, v_missing, v_unres;
END;
$$;

REVOKE ALL ON FUNCTION fms.fan_out_notifications(bigint) FROM PUBLIC, fms_app;
GRANT EXECUTE ON FUNCTION fms.fan_out_notifications(bigint) TO fms_owner;

DROP FUNCTION IF EXISTS fms.template_placeholders(text);
DELETE FROM fms.permissions
 WHERE code IN ('notification_template:read', 'notification_template:write');

COMMIT;
