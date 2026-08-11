-- 回退 032。
--
-- 拆掉觸發器與解析函式，把 `transition_work_order` 還原成 022 的版本
-- （含那個恆真的 MET 判定與吃 DEFAULT 的 actor_type —— 回退的意思就是
--  回到當時的行為，不是回到當時的行為再加上一點修正）。
--
-- **回填的資料不還原。** 那些 due 時刻算得出來也不假，抹掉它們只是把
-- 「沒在量」偽裝成另一種樣子。回退的是規則，不是已經記錄下來的事實。
-- 若真的要清，那是一次獨立的資料操作，應該有人明確決定。
--
-- 需要平台情境，理由與 032 相同：這個檔要讀寫 work_orders。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DROP TRIGGER IF EXISTS trg_work_order_sla_targets ON fms.work_orders;
DROP FUNCTION IF EXISTS fms.trg_work_order_sla_targets();

-- 022 的版本，逐字還原。
CREATE OR REPLACE FUNCTION fms.transition_work_order(
  p_work_order_id uuid,
  p_action        varchar,
  p_actor_user_id uuid    DEFAULT NULL,
  p_reason        varchar DEFAULT NULL,
  p_metadata      jsonb   DEFAULT '{}'::jsonb
) RETURNS fms.work_orders
LANGUAGE plpgsql
AS $$
DECLARE
  v_wo    fms.work_orders;
  v_rule  fms.work_order_transitions_allowed;
  v_actor uuid := coalesce(p_actor_user_id, fms.current_user_id());
BEGIN
  SELECT * INTO v_wo FROM fms.work_orders WHERE id = p_work_order_id FOR UPDATE;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'work order % not found', p_work_order_id USING ERRCODE = 'P0002';
  END IF;

  SELECT * INTO v_rule
  FROM fms.work_order_transitions_allowed t
  WHERE t.is_active
    AND (t.tenant_id IS NULL OR t.tenant_id = v_wo.tenant_id)
    AND (t.work_order_type IS NULL OR t.work_order_type = v_wo.work_order_type)
    AND t.from_status = v_wo.status
    AND t.action = p_action
  ORDER BY t.tenant_id NULLS LAST, t.work_order_type NULLS LAST
  LIMIT 1;

  IF v_rule.id IS NULL THEN
    RAISE EXCEPTION 'action % is not allowed from status % (wo %)',
      p_action, v_wo.status, v_wo.wo_no USING ERRCODE = '23514';
  END IF;

  IF v_rule.required_permission IS NOT NULL THEN
    IF v_actor IS NULL THEN
      RAISE EXCEPTION
        'transition % requires permission % but no actor was supplied',
        p_action, v_rule.required_permission USING ERRCODE = '42501';
    END IF;
    IF NOT fms.user_has_permission(v_actor, v_rule.required_permission, v_wo.facility_id) THEN
      RAISE EXCEPTION
        'actor % lacks permission % required for action % on work order %',
        v_actor, v_rule.required_permission, p_action, v_wo.wo_no
        USING ERRCODE = '42501';
    END IF;
  END IF;

  UPDATE fms.work_orders
     SET status = v_rule.to_status,
         first_responded_at = CASE
           WHEN first_responded_at IS NULL AND (v_rule.side_effects ->> 'set_responded') = 'true'
           THEN clock_timestamp() ELSE first_responded_at END,
         actual_start_at = CASE
           WHEN (v_rule.side_effects ->> 'set_actual_start') = 'true'
           THEN coalesce(actual_start_at, clock_timestamp()) ELSE actual_start_at END,
         actual_end_at = CASE
           WHEN (v_rule.side_effects ->> 'set_actual_end') = 'true'
           THEN clock_timestamp() ELSE actual_end_at END,
         completed_at = CASE
           WHEN v_rule.to_status = 'COMPLETED' THEN clock_timestamp() ELSE completed_at END,
         closed_at = CASE
           WHEN v_rule.to_status = 'CLOSED' THEN clock_timestamp() ELSE closed_at END,
         cancelled_reason = CASE
           WHEN v_rule.to_status IN ('CANCELLED','REJECTED') THEN p_reason ELSE cancelled_reason END,
         sla_state = CASE
           WHEN v_rule.to_status IN ('COMPLETED','CLOSED')
                AND (resolution_due_at IS NULL OR clock_timestamp() <= resolution_due_at)
                AND sla_state NOT IN ('RESPONSE_BREACHED','RESOLUTION_BREACHED')
           THEN 'MET' ELSE sla_state END
   WHERE id = p_work_order_id
   RETURNING * INTO v_wo;

  INSERT INTO fms.work_order_transitions
    (tenant_id, work_order_id, from_status, action, to_status, actor_user_id, reason, metadata)
  VALUES
    (v_wo.tenant_id, v_wo.id, v_rule.from_status, p_action, v_rule.to_status,
     v_actor, p_reason, p_metadata);

  PERFORM fms.emit_event(
    v_wo.tenant_id,
    coalesce(v_rule.side_effects ->> 'emit', 'work_order.status_changed'),
    'WORK_ORDER', v_wo.id,
    jsonb_build_object(
      'wo_no', v_wo.wo_no, 'from', v_rule.from_status, 'to', v_rule.to_status,
      'action', p_action, 'actor_user_id', v_actor,
      'facility_id', v_wo.facility_id, 'assignee_id', v_wo.assignee_id));

  RETURN v_wo;
END;
$$;

DROP FUNCTION IF EXISTS fms.resolve_sla_policy(uuid, uuid, text);

COMMIT;
