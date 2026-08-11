-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 033: 逾期掃描（ADR-12 量測鏈第 3 段）
-- =============================================================================
-- 逾期是**「時間到了而某事沒有發生」**，因此沒有觸發點。
-- 032 的狀態機只能在有人推進工單時判定；沒有人動它的那些單，
-- 逾期了也不會有任何地方知道。這與預約的 no-show 掃描是同一個形狀。
--
-- -----------------------------------------------------------------------------
-- sla_state 是摘要，時刻才是事實
-- -----------------------------------------------------------------------------
-- `sla_state` 只有一格，而一張工單可以同時逾回應與逾解決。004 的
-- `idx_wo_sla_watch` 是部分索引（`WHERE sla_state IN ('ON_TRACK','AT_RISK')`），
-- 因此一旦標成 `RESPONSE_BREACHED`，那一列就離開了索引，之後的解決逾期
-- 不會再被掃到。
--
-- 這不是要修的缺陷，是要講清楚的分工：
--
--   * `sla_state` = **第一個發生的逾期**，給 UI 過濾與通知用
--   * `response_due_at` / `resolution_due_at` / `first_responded_at` /
--     `completed_at` = 事實，**報表一律從這些時刻算**
--
-- 報表不讀 `sla_state`。否則達成率會取決於掃描有沒有跑過，
-- 而那是一個排程器的實作細節，不該影響拿去談合約的數字。
--
-- -----------------------------------------------------------------------------
-- 為什麼先標回應、後標解決
-- -----------------------------------------------------------------------------
-- 一張同時逾兩者的工單，兩種順序會得到不同的 `sla_state`。選「回應先」的
-- 理由是**批次補跑的結果要等於連續運行的結果**：
-- 掃描每分鐘跑一次時，`response_due_at` 必然先到，於是那一刻就標成
-- `RESPONSE_BREACHED` 並離開索引。若批次補跑（例如停機後）改成解決先，
-- 同一批資料會得到不一樣的答案 —— 那種不一致查起來很痛，
-- 因為它取決於「掃描有沒有中斷過」。
--
-- -----------------------------------------------------------------------------
-- 守衛只用 sla_state 與狀態類別，不用 completed_at
-- -----------------------------------------------------------------------------
-- 第一版三個掃描都帶了 `completed_at IS NULL`，看起來是理所當然的
-- 「不要重判已完成的工單」。突變測試把它拿掉，**十個測試全部照過** ——
-- 因為那個條件是多餘的：已完成的工單 `sla_state` 已經被 032 定成
-- MET／NOT_APPLICABLE／RESOLUTION_BREACHED，本來就不在 `('ON_TRACK','AT_RISK')` 裡。
--
-- 追下去才發現它不只是多餘，而是**有害**：032 的 REOPEN 把 `sla_state`
-- 重設成 `ON_TRACK`，但**沒有清掉 `completed_at`**（那個欄位只在進入
-- COMPLETED 時寫）。於是重開過的工單永遠帶著一個過期的 `completed_at`，
-- 而 `completed_at IS NULL` 會把它們**永久排除在掃描之外** ——
-- 重開後的第二輪不管逾期多久都不會被標記。決定 E（重開是新的量測）
-- 會靜默失效。
--
-- 正確的守衛是兩個既有條件的組合，各管一半：
--   * `sla_state IN ('ON_TRACK','AT_RISK')` —— 排除已判定的（含已完成）
--   * `st.category IN ('OPEN','WAITING','IN_PROGRESS')` —— 排除終止狀態
--     （CANCEL 不改 sla_state，所以已取消的單仍是 ON_TRACK，只有類別攔得住）
--
-- **「重開後的工單帶著過期的 completed_at」本身是個資料層的問題**，
-- 影響不只 SLA（DTO 也曝露那個欄位）。033 不改它 —— 那是 032 的 REOPEN
-- 語意，而動它會影響非 SLA 的消費者。記在這裡，報表（第 4 段）必須用
-- 狀態類別而不是 `completed_at` 判斷「這一輪做完了沒有」。
--
-- -----------------------------------------------------------------------------
-- 刻意不做：BREACH_SLA 轉移
-- -----------------------------------------------------------------------------
-- 目錄裡有 `BREACH_SLA`（`ASSIGNED`／`IN_PROGRESS` → `SLA_BREACHED`，
-- `actor: SYSTEM`，`notify: [FACILITY_ADMIN, MAINTENANCE_SUPERVISOR]`），
-- 而 `api/ENDPOINTS.md` 記載的 `sla-watchdog` 也寫了「觸發 BREACH_SLA」。
--
-- **本檔不呼叫它。** 那不是量測，是升級流程：它會改變工單的工作狀態
-- （使用者的待辦清單會變）並向兩個角色發通知。要不要讓排程器自動做這件事、
-- 以及在逾期多久之後做，是產品決定，而 ADR-12 沒有涵蓋它。
--
-- 後果是 `work_order.sla_breached` 這個事件目前仍然沒有產生者
-- —— 又一個「宣告了沒人寫」。記在這裡等決定，而不是順手補上。
--
-- 依賴：004（work_orders、sla_policies、idx_wo_sla_watch）、032（due 時刻）。
-- =============================================================================

-- 讀寫租戶資料（work_orders）→ 需要平台情境，理由見 032 檔頭。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- fms.sweep_sla_states()
-- -----------------------------------------------------------------------------
-- `p_at_risk_fraction`：解決窗口用掉多少比例之後算「有風險」。
-- 0.8 表示還剩 20% 時提醒。
--
-- 這個函式跨租戶運作，而 `work_orders` 是 FORCE RLS（連 owner 都受限），
-- 因此**呼叫者必須已經在平台情境內**。
--
-- **刻意不用 SECURITY DEFINER。** 021 那個模式（DEFINER + 函式內暫時取得
-- 平台情境）是為了打破一個循環：授權的權威來源不能被它自己餵養的 RLS
-- 政策過濾。這裡沒有循環 —— `fms-worker` 本來就以 `fms_owner` 連線並有
-- `begin_platform_tx()`（見該 crate 檔頭）。
--
-- 差別很重要：DEFINER 版本會讓任何拿到 EXECUTE 權的角色取得一次跨租戶
-- 計數（三個數字橫跨所有租戶）。那是個小但真實的資訊洩漏面，
-- 而它換不到任何東西 —— 呼叫端早就有平台情境了。
--
-- 因此 EXECUTE 只給 `fms_owner`（與 028 的 `ensure_time_partitions` 同樣的
-- 授權範圍：維護工作，不走請求路徑）。
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

COMMENT ON FUNCTION fms.sweep_sla_states(numeric) IS
  'ADR-12 量測鏈第 3 段：標記 AT_RISK / RESPONSE_BREACHED / RESOLUTION_BREACHED。'
  '呼叫者必須已在平台情境內（work_orders 是 FORCE RLS）；刻意不用 DEFINER。'
  '先標回應再標解決，讓批次補跑的結果等於連續運行的結果。'
  '不觸發 BREACH_SLA 轉移（那是升級流程，屬產品決定）。'
  'sla_state 是摘要；報表一律從時刻欄位計算。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 與 032 同樣的限制：本檔在 CORE 裡執行、早於 009，因此沒有租戶資料可用。
-- 行為在 `sla_sweep_slice.rs` 斷言。這裡只驗**不依賴資料**的部分。
DO $$
DECLARE
  v_r record;
BEGIN
  -- (1) 空資料庫上跑得起來且回三個 0。這一格看似瑣碎，但排程器每分鐘
  --     呼叫它一次，「沒有事情要做」是最常見的情況 ——
  --     那條路徑不能是沒有跑過的。
  SELECT * INTO v_r FROM fms.sweep_sla_states();
  IF v_r.at_risk <> 0 OR v_r.response_breached <> 0 OR v_r.resolution_breached <> 0 THEN
    RAISE EXCEPTION '033 FAILED: 空資料庫應回 (0,0,0)，實際 (%,%,%)',
      v_r.at_risk, v_r.response_breached, v_r.resolution_breached;
  END IF;

  -- (2) 參數要驗。0 或 1 會讓 AT_RISK 分別退化成「一開始就有風險」
  --     與「永遠沒風險」，兩者都是靜默的錯誤設定。
  BEGIN
    PERFORM fms.sweep_sla_states(0);
    RAISE EXCEPTION '033 FAILED: p_at_risk_fraction = 0 應被拒絕';
  EXCEPTION WHEN invalid_parameter_value THEN NULL;
  END;
  BEGIN
    PERFORM fms.sweep_sla_states(1);
    RAISE EXCEPTION '033 FAILED: p_at_risk_fraction = 1 應被拒絕';
  EXCEPTION WHEN invalid_parameter_value THEN NULL;
  END;

  -- (3) 不是 SECURITY DEFINER。這是 021 那條斷言的反面：那裡要確保函式
  --     **是** DEFINER（否則循環沒解開），這裡要確保它**不是**
  --     （否則多出一個跨租戶計數的洩漏面，而且換不到任何東西）。
  --
  --     把設計決定寫成斷言，是因為「為什麼不用 DEFINER」只存在於註解裡時，
  --     下一個人為了修 RLS 錯誤順手加上 DEFINER 是很自然的動作。
  IF (SELECT prosecdef FROM pg_proc
       WHERE pronamespace = 'fms'::regnamespace AND proname = 'sweep_sla_states') THEN
    RAISE EXCEPTION
      '033 FAILED: sweep_sla_states 不該是 SECURITY DEFINER（見檔頭：呼叫端本來就有平台情境）';
  END IF;

  -- (4) EXECUTE 不給 fms_app。它是維護工作，不走請求路徑。
  IF has_function_privilege('fms_app', 'fms.sweep_sla_states(numeric)', 'EXECUTE') THEN
    RAISE EXCEPTION '033 FAILED: fms_app 不該能執行 sweep_sla_states';
  END IF;

  RAISE NOTICE '033 OK: sweep_sla_states 就緒（INVOKER、僅 fms_owner）';
END;
$$;

COMMIT;
