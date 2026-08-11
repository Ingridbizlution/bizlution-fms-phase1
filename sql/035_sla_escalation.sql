-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 035: 逾期自動升級（掃描觸發 BREACH_SLA）
-- =============================================================================
-- 033 刻意只標 `sla_state`，把「要不要自動改工單狀態並升級」留作產品決定。
-- 那個決定是「要」，因此本檔讓掃描在標記之後呼叫 `BREACH_SLA` 轉移。
--
-- -----------------------------------------------------------------------------
-- 覆蓋範圍：只有 ASSIGNED 與 IN_PROGRESS
-- -----------------------------------------------------------------------------
-- 目錄裡 `BREACH_SLA` 只有兩條規則：
--     ASSIGNED    --BREACH_SLA--> SLA_BREACHED
--     IN_PROGRESS --BREACH_SLA--> SLA_BREACHED
--
-- 本檔**不擴充那張目錄**，因此以下兩類逾期只會被標 `sla_state`、不會改狀態：
--
--   * **還停在 `SUBMITTED`（沒有人接手）** —— 而這是最該升級的一類。
--     不加規則的理由是加了會把工單困死：`SLA_BREACHED` 出去的路只有
--     `CANCEL`／`COMPLETE`／`RESUME`，**沒有 `ASSIGN`**。一張從未派工的
--     工單一旦進了 `SLA_BREACHED`，就再也指派不出去 —— 只能取消，
--     或由某個有 `work_order:execute` 的人直接 RESUME 成 IN_PROGRESS
--     （跳過派工）。要覆蓋這一類，得同時補上
--     `ASSIGN: SLA_BREACHED → ASSIGNED`，而那是工作流程的改動，
--     會改變使用者的待辦清單語意，不是掃描該順手決定的事。
--
--   * **`WAITING` 類別的四個狀態**（`ON_HOLD`／`PENDING_APPROVAL`／
--     `WAITING_PARTS`／`WAITING_VENDOR`）。這裡不加規則的理由不同：
--     改成 `SLA_BREACHED` 會**抹掉「為什麼卡住」**（等料？等廠商？等核准？）
--     ——而那正是 ADR-12 決定 D 要讓人看見的資訊。逾期已經記在 `sla_state`
--     與報表裡；再犧牲卡住的原因換一個狀態碼是淨損失。
--
-- 這兩個缺口是目錄的形狀造成的，不是本檔的疏漏。它們**仍然**進報表分母、
-- 仍然被標記，只是沒有狀態變更與事件。
--
-- -----------------------------------------------------------------------------
-- 誠實話：今天不會有人收到通知
-- -----------------------------------------------------------------------------
-- 目錄裡那兩條規則的 `side_effects` 帶
-- `notify: [FACILITY_ADMIN, MAINTENANCE_SUPERVISOR]`，而
-- `transition_work_order` 會 `emit_event('work_order.sla_breached', ...)`。
--
-- 但**沒有任何程式碼寫 `fms.notifications`**（全 repo 零個
-- `INSERT INTO fms.notifications`），而 `fms-jobs` 的 relay 只處理
-- `maintenance.meter_threshold_reached`，其餘型別會被標成 `SKIPPED`。
--
-- 因此升級目前實際產出的是：
--   1. 工單狀態變成 `SLA_BREACHED`（UI 與查詢看得到）
--   2. 一筆 `work_order_transitions`（`actor_type = 'SYSTEM'`，032 修好的）
--   3. 一筆躺在 `event_outbox` 裡、狀態為 `SKIPPED` 的事件
--
-- 沒有 email、沒有 push。`notify` 是第八個「宣告了沒人讀」。
-- 這一段寫在這裡，是為了讓「已經自動升級了」不會被讀成「有人會被通知」。
--
-- 依賴：033（掃描）、032（actor_type 與 sla_state 判定）、015（動作目錄）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- 回傳型別要多兩欄，而 CREATE OR REPLACE 改不了回傳型別。
DROP FUNCTION IF EXISTS fms.sweep_sla_states(numeric);

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

COMMENT ON FUNCTION fms.sweep_sla_states(numeric) IS
  'ADR-12 量測鏈第 3 段 + 自動升級。呼叫者必須已在平台情境內；刻意不用 DEFINER。'
  '先標回應再標解決，讓批次補跑的結果等於連續運行的結果。'
  '標記後對 ASSIGNED/IN_PROGRESS 的工單觸發 BREACH_SLA（目錄只允許這兩個 from_status）。'
  'SUBMITTED 與 WAITING 的逾期只標記不改狀態 —— 理由見 migration 035 檔頭。'
  'sla_state 是摘要；報表一律從時刻欄位計算。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 行為在 `sla_sweep_slice.rs`（本檔在 CORE 裡、早於 009，沒有租戶資料）。
DO $$
DECLARE
  v_r record;
  v_n bigint;
BEGIN
  -- (1) 空資料庫上五個 0。排程器最常見的情況是「沒事要做」，
  --     那條路徑不能是沒跑過的。
  SELECT * INTO v_r FROM fms.sweep_sla_states();
  IF v_r.at_risk <> 0 OR v_r.response_breached <> 0 OR v_r.resolution_breached <> 0
     OR v_r.escalated <> 0 OR v_r.escalation_failed <> 0 THEN
    RAISE EXCEPTION '033/035 FAILED: 空資料庫應回全 0，實際 (%,%,%,%,%)',
      v_r.at_risk, v_r.response_breached, v_r.resolution_breached,
      v_r.escalated, v_r.escalation_failed;
  END IF;

  -- (2) 參數仍然要驗（033 的斷言，換了簽章不能掉）。
  BEGIN
    PERFORM fms.sweep_sla_states(0);
    RAISE EXCEPTION '035 FAILED: p_at_risk_fraction = 0 應被拒絕';
  EXCEPTION WHEN invalid_parameter_value THEN NULL;
  END;

  -- (3) 仍然不是 SECURITY DEFINER、仍然不給 fms_app（033 的兩條斷言）。
  --     DROP + CREATE 會讓 007 的 ALTER DEFAULT PRIVILEGES 再次自動把
  --     EXECUTE 給 fms_app —— 這一格就是為了擋那個重置。
  IF (SELECT prosecdef FROM pg_proc
       WHERE pronamespace = 'fms'::regnamespace AND proname = 'sweep_sla_states') THEN
    RAISE EXCEPTION '035 FAILED: sweep_sla_states 不該是 SECURITY DEFINER';
  END IF;
  IF has_function_privilege('fms_app', 'fms.sweep_sla_states(numeric)', 'EXECUTE') THEN
    RAISE EXCEPTION '035 FAILED: fms_app 不該能執行 sweep_sla_states';
  END IF;

  -- (4) 目錄的形狀就是覆蓋範圍。若日後有人補了 BREACH_SLA 的規則，
  --     035 的迴圈條件（只收 ASSIGNED/IN_PROGRESS）就會落後於目錄 ——
  --     而那個落後沒有症狀：新狀態的工單會被標記卻不升級。
  SELECT count(*) INTO v_n
    FROM fms.work_order_transitions_allowed
   WHERE action = 'BREACH_SLA' AND is_active
     AND from_status NOT IN ('ASSIGNED', 'IN_PROGRESS');
  IF v_n > 0 THEN
    RAISE EXCEPTION
      '035 FAILED: 目錄新增了 % 條 BREACH_SLA 規則，但掃描的升級迴圈只收 ASSIGNED/IN_PROGRESS', v_n;
  END IF;

  RAISE NOTICE '035 OK: 逾期自動升級就緒（僅 ASSIGNED/IN_PROGRESS —— 見檔頭）';
END;
$$;

COMMIT;
