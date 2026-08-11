-- 回退 043：扇出改回 042 的版本（IN_APP 也插成 QUEUED）。
--
-- **回填不還原。** 把已經送達的 IN_APP 通知改回 QUEUED，會讓監控查詢
-- 重新回一個假的待送數字 —— 那正是 043 要修的東西。
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

  IF v_ev.aggregate_type <> 'WORK_ORDER' THEN
    RETURN QUERY SELECT 0, 0, 0;
    RETURN;
  END IF;

  SELECT side_effects INTO v_rule
    FROM fms.work_order_transitions_allowed t
   WHERE t.is_active
     AND t.from_status = v_ev.payload ->> 'from'
     AND t.action = v_ev.payload ->> 'action'
     AND (t.tenant_id IS NULL OR t.tenant_id = v_ev.tenant_id)
   ORDER BY t.tenant_id NULLS LAST
   LIMIT 1;

  IF v_rule.side_effects IS NULL OR NOT (v_rule.side_effects ? 'notify') THEN
    RETURN QUERY SELECT 0, 0, 0;
    RETURN;
  END IF;

  v_vars := fms.notification_vars(v_ev.aggregate_id);

  IF NOT (v_rule.side_effects ? 'template') THEN
    RETURN QUERY SELECT 0, 1, 0;
    RETURN;
  END IF;

  WITH people AS (
    SELECT DISTINCT r.user_id
      FROM fms.notification_recipients(v_ev.aggregate_id,
                                       v_rule.side_effects -> 'notify') r
     WHERE r.user_id IS NOT NULL
  ), tpl AS (
    -- **每個頻道只取一個範本。** 041 沒有這個限制，於是租戶建了覆寫版本之後
    -- 平台版與租戶版都會匹配，而唯一索引讓其中一個任意勝出（見本檔檔頭）。
    --
    -- 優先序：租戶版 > 平台版；同層再以 locale 字母序決勝
    -- （目前沒有使用者語系偏好可以據此選擇 —— 不確定的答案換成確定的）。
    SELECT DISTINCT ON (nt.channel)
           nt.channel, nt.code, nt.subject_template, nt.body_template
      FROM fms.notification_templates nt
     WHERE lower(nt.code) = lower(v_rule.side_effects ->> 'template')
       AND nt.is_active
       AND (nt.tenant_id IS NULL OR nt.tenant_id = v_ev.tenant_id)
     ORDER BY nt.channel, (nt.tenant_id IS NOT NULL) DESC, nt.locale
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

COMMIT;
