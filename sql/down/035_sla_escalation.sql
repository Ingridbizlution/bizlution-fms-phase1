-- 回退 035：拿掉自動升級，還原 033 的三欄版本（只標 sla_state）。
--
-- **已經升級的工單不改回去。** 它們的狀態是 SLA_BREACHED、轉移歷史裡有
-- 那一筆記錄，而那是真的發生過的事。回退的是機制，不是歷史。
--
-- 需要平台情境（033 的理由）。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- 回傳型別要從五欄改回三欄，CREATE OR REPLACE 做不到。
DROP FUNCTION IF EXISTS fms.sweep_sla_states(numeric);

CREATE OR REPLACE FUNCTION fms.sweep_sla_states(
  p_at_risk_fraction numeric DEFAULT 0.8
) RETURNS TABLE (at_risk bigint, response_breached bigint, resolution_breached bigint)
LANGUAGE plpgsql
AS $$
DECLARE
  v_at_risk    bigint;
  v_response   bigint;
  v_resolution bigint;
BEGIN
  IF p_at_risk_fraction <= 0 OR p_at_risk_fraction >= 1 THEN
    RAISE EXCEPTION 'p_at_risk_fraction 必須在 (0,1) 之間，收到 %', p_at_risk_fraction
      USING ERRCODE = '22023';
  END IF;

  -- (1) 逾回應：到了 response_due_at 而還沒有人接下。
  --
  -- 綁 `first_responded_at IS NULL` 而不是狀態碼 —— 032 已經把
  -- 「有人接下」這件事收斂到那一個欄位（且排除了系統動作）。
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
     RETURNING 1
  )
  SELECT count(*) INTO v_response FROM swept;

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
     RETURNING 1
  )
  SELECT count(*) INTO v_resolution FROM swept;

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

  RETURN QUERY SELECT v_at_risk, v_response, v_resolution;
END;
$$;

-- **`FROM PUBLIC` 不夠。** 007 有
-- `ALTER DEFAULT PRIVILEGES IN SCHEMA fms GRANT EXECUTE ON FUNCTIONS TO fms_app`，
-- 因此 schema 裡每一個新函式都自動對 `fms_app` 開放 —— 那是個具名授權，
-- 不是 PUBLIC，REVOKE ... FROM PUBLIC 碰不到它。
-- （023 的檔頭記過同一個陷阱的另一面：`GRANT SELECT` 看起來多餘，
--   因為預設權限早就給了。）
--
-- 第一版只寫了 FROM PUBLIC，是下面自我驗證第 (4) 項抓到的。
REVOKE ALL ON FUNCTION fms.sweep_sla_states(numeric) FROM PUBLIC, fms_app;
GRANT EXECUTE ON FUNCTION fms.sweep_sla_states(numeric) TO fms_owner;

COMMIT;
