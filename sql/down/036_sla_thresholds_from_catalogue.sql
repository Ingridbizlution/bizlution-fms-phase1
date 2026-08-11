-- 回退 036：還原 035 的版本（全域 0.8 門檻、硬編可升級狀態）。
--
-- 形狀約束一併拆掉 —— 它是為了保護 036 的讀取端而加的。
--
-- **已經標記／升級的工單不動。** 回退的是機制，不是歷史。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

ALTER TABLE fms.sla_policies DROP CONSTRAINT IF EXISTS ck_sla_escalation_rules;

-- 簽章要改回去（多一個參數、少一個回傳欄），CREATE OR REPLACE 做不到。
DROP FUNCTION IF EXISTS fms.sweep_sla_states();

CREATE FUNCTION fms.sweep_sla_states(
  p_at_risk_fraction numeric DEFAULT 0.8
) RETURNS TABLE (
  at_risk             bigint,
  response_breached   bigint,
  resolution_breached bigint,
  escalated           bigint,
  escalation_failed   bigint
)
LANGUAGE plpgsql
AS $$
DECLARE
  v_at_risk    bigint;
  v_response   bigint;
  v_resolution bigint;
  v_escalated  bigint := 0;
  v_failed     bigint := 0;
  -- 只有這兩個狀態進得了 SLA_BREACHED（見檔頭）。綁狀態碼而不是類別是
  -- 因為這裡要對齊的正是目錄裡那兩條規則的 from_status，不是一個語意分類。
  v_ids        uuid[] := '{}';
  v_id         uuid;
BEGIN
  IF p_at_risk_fraction <= 0 OR p_at_risk_fraction >= 1 THEN
    RAISE EXCEPTION 'p_at_risk_fraction 必須在 (0,1) 之間，收到 %', p_at_risk_fraction
      USING ERRCODE = '22023';
  END IF;

  -- (1) 逾回應：到了 response_due_at 而還沒有人接下。
  --
  -- 綁 `first_responded_at IS NULL` 而不是狀態碼 —— 032 已經把
  -- 「有人接下」這件事收斂到那一個欄位（且排除了系統動作）。
  --
  -- 這裡收到的 ASSIGNED 工單就是「AUTO_ASSIGN 派了、但沒有人接手」——
  -- 決定 B 要暴露的正是這一類，而它現在會被自動升級。
  WITH swept AS (
    UPDATE fms.work_orders wo
       SET sla_state = 'RESPONSE_BREACHED'
      FROM fms.work_order_statuses st
     WHERE st.code = wo.status
       AND st.category IN ('OPEN', 'WAITING', 'IN_PROGRESS')
       AND wo.sla_state IN ('ON_TRACK', 'AT_RISK')
       AND wo.first_responded_at IS NULL
       AND wo.response_due_at IS NOT NULL
       AND wo.response_due_at < clock_timestamp()
     RETURNING wo.id, wo.status
  )
  SELECT count(*),
         coalesce(array_agg(id) FILTER (WHERE status IN ('ASSIGNED', 'IN_PROGRESS')), '{}')
    INTO v_response, v_ids
    FROM swept;

  -- (2) 逾解決：到了 resolution_due_at 而還沒完成。
  WITH swept AS (
    UPDATE fms.work_orders wo
       SET sla_state = 'RESOLUTION_BREACHED'
      FROM fms.work_order_statuses st
     WHERE st.code = wo.status
       AND st.category IN ('OPEN', 'WAITING', 'IN_PROGRESS')
       AND wo.sla_state IN ('ON_TRACK', 'AT_RISK')
       AND wo.resolution_due_at IS NOT NULL
       AND wo.resolution_due_at < clock_timestamp()
     RETURNING wo.id, wo.status
  )
  SELECT count(*),
         v_ids || coalesce(array_agg(id) FILTER (WHERE status IN ('ASSIGNED', 'IN_PROGRESS')), '{}')
    INTO v_resolution, v_ids
    FROM swept;

  -- (3) 有風險：窗口用掉 p_at_risk_fraction 以上但還沒逾期。
  --
  -- 窗口長度取自 policy 的 `resolution_minutes`。**這一個量不是快照** ——
  -- 若有人事後調了 policy 的分鐘數，AT_RISK 的提醒時點會跟著位移
  -- （`resolution_due_at` 不會，它是絕對時刻）。這是刻意的取捨：
  -- AT_RISK 是提醒門檻，不是拿去談合約的數字，為它多存一個欄位
  -- 不划算。報表不讀 AT_RISK。
  WITH swept AS (
    UPDATE fms.work_orders wo
       SET sla_state = 'AT_RISK'
      FROM fms.work_order_statuses st,
           fms.sla_policies sp
     WHERE st.code = wo.status
       AND st.category IN ('OPEN', 'WAITING', 'IN_PROGRESS')
       AND sp.id = wo.sla_policy_id
       AND wo.sla_state = 'ON_TRACK'
       AND wo.resolution_due_at IS NOT NULL
       AND clock_timestamp() < wo.resolution_due_at
       AND clock_timestamp() >= wo.resolution_due_at
             - make_interval(mins => ceil(sp.resolution_minutes
                                          * (1 - p_at_risk_fraction))::int)
     RETURNING 1
  )
  SELECT count(*) INTO v_at_risk FROM swept;

  -- -------------------------------------------------------------------------
  -- (4) 升級
  -- -------------------------------------------------------------------------
  -- 標記先做、轉移後做，順序是必要的：`BREACH_SLA` 的 side_effects 沒有
  -- `compute_sla`，因此 032 的 sla_state CASE 會走到 `ELSE sla_state`
  -- —— 也就是保留上面剛標好的值。反過來做會被 032 的判定蓋掉。
  --
  -- **逐筆包 EXCEPTION 而不是讓整批失敗。** 掃描每分鐘對線上系統跑一次，
  -- 而 `transition_work_order` 會 `FOR UPDATE`：從上面的 UPDATE 到這裡之間
  -- 有人推進了同一張工單，`BREACH_SLA` 就可能不再合法（23514）。
  -- 那不是不可能的情境，是每分鐘都有機會發生的競態。
  --
  -- 若讓它整批失敗，這一輪的**全部標記都會回滾**（worker 是單一交易），
  -- 下一輪再撞同一張 → 永久停擺。因此逐筆捕捉，並把次數回報出去 ——
  -- 靜默跳過會讓「升級沒發生」變成看不見的事。
  FOREACH v_id IN ARRAY v_ids LOOP
    BEGIN
      PERFORM fms.transition_work_order(v_id, 'BREACH_SLA', NULL, 'SLA 逾期自動升級');
      v_escalated := v_escalated + 1;
    EXCEPTION WHEN others THEN
      v_failed := v_failed + 1;
      RAISE NOTICE 'BREACH_SLA 對工單 % 失敗（%）—— 已標記逾期，狀態未變更',
        v_id, SQLERRM;
    END;
  END LOOP;

  RETURN QUERY SELECT v_at_risk, v_response, v_resolution, v_escalated, v_failed;
END;
$$;

REVOKE ALL ON FUNCTION fms.sweep_sla_states(numeric) FROM PUBLIC, fms_app;
GRANT EXECUTE ON FUNCTION fms.sweep_sla_states(numeric) TO fms_owner;

DROP FUNCTION IF EXISTS fms.escalation_rules_are_valid(jsonb);

COMMIT;
