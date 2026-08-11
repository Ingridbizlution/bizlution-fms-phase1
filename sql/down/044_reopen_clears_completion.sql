-- 回退 044：狀態機還原成 032 的版本（REOPEN 不清 completed_at/actual_end_at）。
--
-- **回填的 NULL 不還原。** 那些欄位被清掉是因為它們說了假話
-- （「已完成」而工單正在進行）。把假話填回去只是讓下一個查詢重新踩到。
-- 而且原值仍在 REOPEN 轉移的 metadata 裡 —— 真的要重建是做得到的，
-- 但那該是一次明確的資料操作。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

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
  v_wo         fms.work_orders;
  v_rule       fms.work_order_transitions_allowed;
  v_actor      uuid := coalesce(p_actor_user_id, fms.current_user_id());
  -- 032: 系統動作與人為動作必須分得開。`side_effects.actor` 已經宣告了
  -- （`AUTO_ASSIGN` 是 `SYSTEM`），但過去沒有任何地方讀它。
  v_actor_type text;
  v_by_system  boolean;
  v_policy     fms.sla_policies;
  v_meta       jsonb := coalesce(p_metadata, '{}'::jsonb);
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

  -- 032（改動 1）：actor_type 從目錄讀，不再吃 DEFAULT 'USER'。
  --
  -- 過去每一筆 transition 都是 'USER'，包含 AUTO_ASSIGN 與 BREACH_SLA
  -- 這種沒有人參與的轉移。稽核軌跡上「誰做的」那一欄有一部分是假的，
  -- 而且看不出來 —— 這比缺值糟。
  v_actor_type := coalesce(v_rule.side_effects ->> 'actor', 'USER');
  IF v_actor_type NOT IN ('USER', 'SYSTEM', 'SERVICE_ACCOUNT') THEN
    RAISE EXCEPTION
      'side_effects.actor = % is not a valid actor_type (action %)',
      v_actor_type, p_action USING ERRCODE = '22023';
  END IF;
  v_by_system := v_actor_type <> 'USER';

  -- 032（改動 2）：SUBMIT 時起算時鐘（決定 A）。
  --
  -- DRAFT 開單時觸發器刻意不算 due，因此草稿送出是唯一的補算點。
  -- 用轉移後的優先度：使用者可能在草稿階段改過它。
  IF v_wo.status = 'DRAFT' AND v_rule.to_status = 'SUBMITTED'
     AND v_wo.resolution_due_at IS NULL THEN
    IF v_wo.sla_policy_id IS NOT NULL THEN
      SELECT * INTO v_policy FROM fms.sla_policies WHERE id = v_wo.sla_policy_id;
    ELSE
      v_policy := fms.resolve_sla_policy(v_wo.tenant_id, v_wo.facility_id, v_wo.priority);
    END IF;
  END IF;

  -- 032（改動 3）：REOPEN 是新的量測（決定 E）。
  --
  -- 「第一次有沒有準時修好」與「重開後有沒有準時修好」是兩個事實，
  -- 合併會讓兩者都看不見。因此把前一輪的結果**快照進這筆轉移的 metadata**
  -- 再重設 —— 那個事實被保留下來，只是不再是工單的當前狀態。
  --
  -- 為什麼放 metadata 而不是新開一張表：transitions 本來就是狀態機的
  -- 歷史軌跡，而「重開時前一輪的結果是什麼」正是這筆轉移的性質。
  -- 為它另立一張表是在既有答案旁邊再造一個。
  IF p_action = 'REOPEN' THEN
    v_meta := v_meta || jsonb_build_object(
      'sla_cycle_closed', jsonb_build_object(
        'sla_state',          v_wo.sla_state,
        'response_due_at',    v_wo.response_due_at,
        'resolution_due_at',  v_wo.resolution_due_at,
        'first_responded_at', v_wo.first_responded_at,
        'completed_at',       v_wo.completed_at,
        'reopened_count',     v_wo.reopened_count));

    IF v_wo.sla_policy_id IS NOT NULL THEN
      SELECT * INTO v_policy FROM fms.sla_policies WHERE id = v_wo.sla_policy_id;
    END IF;
  END IF;

  UPDATE fms.work_orders
     SET status = v_rule.to_status,
         -- 032（改動 4）：系統動作不算「有人回應」（決定 B）。
         --
         -- AUTO_ASSIGN 把工單塞給某個人；那個人還沒看過它。
         -- 舊條件會把那一刻記成回應時刻，於是回應時間量到的是
         -- 「系統派工多快」而不是「人多快接手」。
         first_responded_at = CASE
           WHEN first_responded_at IS NULL
                AND (v_rule.side_effects ->> 'set_responded') = 'true'
                AND NOT v_by_system
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
         -- SUBMIT 補算（改動 2）／REOPEN 重設解決時鐘（改動 3）。
         sla_policy_id = coalesce(sla_policy_id, v_policy.id),
         response_due_at = CASE
           WHEN p_action = 'REOPEN' THEN response_due_at   -- 回應已經發生過，不重設
           WHEN v_policy.id IS NOT NULL AND response_due_at IS NULL
           THEN clock_timestamp() + make_interval(mins => v_policy.response_minutes)
           ELSE response_due_at END,
         resolution_due_at = CASE
           WHEN p_action = 'REOPEN' AND v_policy.id IS NOT NULL
           THEN clock_timestamp() + make_interval(mins => v_policy.resolution_minutes)
           WHEN v_policy.id IS NOT NULL AND resolution_due_at IS NULL
           THEN clock_timestamp() + make_interval(mins => v_policy.resolution_minutes)
           ELSE resolution_due_at END,
         -- `reopened_count` **刻意不在這裡加**。`fms-workorder` 的 repo 已經
         -- 有一個 `reopened_count = reopened_count + 1`（repo.rs:614），
         -- 在這裡再加一次就是同一條規則的第二份實作 —— 測試實測到 2 而不是 1。
         --
         -- 留下的不對稱：直接呼叫本函式的路徑（排程器）不會遞增。
         -- 那是既有狀況，不在 032 的授權範圍內，記在這裡而不是順手改掉。
         --
         -- 032（改動 5）：那個恆真的 MET 判定。
         --
         -- 舊條件是 `resolution_due_at IS NULL OR now() <= resolution_due_at`，
         -- 而 resolution_due_at 從來沒有東西會寫 → 恆為 NULL → 恆為真
         -- → 每一張完成的工單都是 MET。這是報表現在會回 100% 的直接原因。
         --
         -- 沒有 due 就是**沒在量**，不是達成。
         --
         -- 同時讓 `side_effects.compute_sla` 有意義（過去四個動作宣告、
         -- 零個讀取點）：進入 ASSIGNED 時比對回應時刻與 response_due_at。
         sla_state = CASE
           WHEN p_action = 'REOPEN' THEN
             CASE WHEN v_policy.id IS NULL THEN 'NOT_APPLICABLE' ELSE 'ON_TRACK' END

           WHEN (v_rule.side_effects ->> 'compute_sla') = 'true'
                AND response_due_at IS NOT NULL
                AND NOT v_by_system
                AND first_responded_at IS NULL          -- 這一刻才回應
                AND clock_timestamp() > response_due_at
             THEN 'RESPONSE_BREACHED'

           WHEN v_rule.to_status IN ('COMPLETED','CLOSED') THEN
             CASE
               WHEN sla_state IN ('RESPONSE_BREACHED','RESOLUTION_BREACHED')
                 THEN sla_state
               WHEN resolution_due_at IS NULL THEN 'NOT_APPLICABLE'
               WHEN clock_timestamp() <= resolution_due_at THEN 'MET'
               ELSE 'RESOLUTION_BREACHED'
             END

           ELSE sla_state
         END
   WHERE id = p_work_order_id
   RETURNING * INTO v_wo;

  INSERT INTO fms.work_order_transitions
    (tenant_id, work_order_id, from_status, action, to_status,
     actor_user_id, actor_type, reason, metadata)
  VALUES
    (v_wo.tenant_id, v_wo.id, v_rule.from_status, p_action, v_rule.to_status,
     v_actor, v_actor_type, p_reason, v_meta);

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

COMMIT;
