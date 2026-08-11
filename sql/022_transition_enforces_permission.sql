-- =============================================================================
-- 022  transition_work_order 自行執行 required_permission
-- =============================================================================
-- # 背景
--
-- `work_order_transitions_allowed.required_permission` 從 004 起就存在，
-- 但 `fms.transition_work_order()` **查出了規則列卻從不讀那一欄**
-- （見 WBS-rebaseline 4.1c）。S4 時由應用層補上檢查，並在當時記下風險：
--
-- > 任何**不經 REST API** 的呼叫者（PM 產生器、SLA 逾期排程器）
-- > 都會繞過權限與必填欄位檢查。要讓那條路徑也安全，
-- > 正解是把檢查下移到 SQL 函式。
--
-- 那個風險已經不是假設：`fms-jobs` 裡的 PM 產生器就是一個非 API 呼叫者。
-- 本 migration 把權限檢查放進函式本身，成為唯一入口的一部分。
--
-- # 為什麼 `required_fields` 留在應用層
--
-- 必填欄位判定需要看**請求 body**（例如 CANCEL 的 `reason` 不是資料表欄位，
-- 只能來自請求）。資料庫看不到 body，因此那一關無法下移。
-- 分工從此明確：**權限在資料庫、必填欄位在應用層**，而不是兩者都在應用層。
--
-- # 誰的權限
--
-- 用 `p_actor_user_id`（或 `fms.current_user_id()`），與寫入稽核列的是
-- 同一個身分 —— 否則會出現「A 的身分執行、記成 B 做的」。
--
-- 場域範圍取自工單本身的 `facility_id`，不由呼叫端指定：
-- 讓呼叫端傳範圍等於讓它自己決定要用哪個範圍受檢。
--
-- # `required_permission IS NULL` 的動作
--
-- 那是系統驅動的動作（`AUTO_ASSIGN`、`BREACH_SLA`）。函式**允許**它們，
-- 因為排程器就是要能執行；擋在對外 API 那一層（見 fms-workorder 的
-- handler：對外一律回 403）。這個分工是刻意的：
-- 資料庫負責「這個身分有沒有權限做這件事」，
-- API 負責「這個動作能不能從對外介面觸發」。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

CREATE OR REPLACE FUNCTION fms.transition_work_order(
  p_work_order_id uuid,
  p_action        varchar,
  p_actor_user_id uuid DEFAULT NULL,
  p_reason        varchar DEFAULT NULL,
  p_metadata      jsonb DEFAULT '{}'::jsonb
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

  -- 權限檢查（022 新增）。以 42501 拋出，讓應用層的既有 SQLSTATE 映射
  -- 轉成 403 —— 與 set_context 的 PLATFORM_CONTEXT_DENIED 同一個碼，
  -- 語意也相同：這個身分不被允許做這件事。
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

COMMENT ON FUNCTION fms.transition_work_order IS
  'Single sanctioned path for work order status changes. Validates the transition AND the actor''s required_permission (022), writes the audit row and the outbox event atomically. required_fields stays in the application layer because it needs the request body.';

-- -----------------------------------------------------------------------------
-- 自我驗證：函式必須真的參照 required_permission
-- -----------------------------------------------------------------------------
DO $$
DECLARE v_src text;
BEGIN
  SELECT prosrc INTO v_src FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
   WHERE n.nspname = 'fms' AND p.proname = 'transition_work_order';

  IF v_src NOT LIKE '%required_permission%' THEN
    RAISE EXCEPTION '022 FAILED: 函式沒有讀取 required_permission';
  END IF;
  IF v_src NOT LIKE '%user_has_permission%' THEN
    RAISE EXCEPTION '022 FAILED: 函式沒有呼叫 user_has_permission';
  END IF;
  RAISE NOTICE '022 OK: transition_work_order 現在自行執行 required_permission';
END;
$$;

COMMIT;
