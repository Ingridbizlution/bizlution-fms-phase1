-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 057: 即時門檻評估 —— 讓 raise_alarm 第一次有生產呼叫者
-- =============================================================================
-- 做 `POST /telemetry:batch-ingest` 時量到的：
--
--   * `alarm_rules` 有資料（009 種了 3 條）
--   * `raise_alarm()` 完整實作、含去重與自動開單，010 的 T4 驗過
--   * **中間那一段沒有人做** —— 沒有任何東西決定「哪一筆讀數該觸發哪一條規則」
--
-- `raise_alarm` 在整個 codebase 裡的呼叫者只有 010 的 T4。
-- `ingest_telemetry()` 只寫讀數，不評估規則。也就是說 IoT 那條鏈
-- **從來沒有在生產路徑上跑過**。
--
-- 而契約的回應裡有 `alarms_raised`。只做寫入的話那個欄位會永遠是 0 ——
-- 那正是這個專案反覆出現的缺陷類型，只是這次會是我們親手製造的。
--
-- -----------------------------------------------------------------------------
-- 這支函式**只做**即時門檻，而且會說出它跳過了什麼
-- -----------------------------------------------------------------------------
-- 009 的三條規則剛好覆蓋三種情況：
--
--   UPS_SOC_LOW         {"op":"<","value":40}                → 評估
--   AHU_FILTER_DP_HIGH  {"op":">","value":450,"for_seconds":600} → 跳過（持續型）
--   DEVICE_OFFLINE      rule_type = DEVICE_OFFLINE           → 跳過（掃描型）
--
-- **持續型（`for_seconds`）需要回看歷史讀數**，掃描型則是「沒有讀數進來」
-- 才觸發 —— 兩者都不是單筆讀數當下判斷得出來的，形狀與 SLA watchdog 相同，
-- 屬於獨立的一件工程。
--
-- 但它們不會被靜默忽略：回傳值裡有 `skipped_sustained`，端點把它放進回應。
-- 「這條規則設定了但永遠不會響」必須看得見。
--
-- -----------------------------------------------------------------------------
-- 不認得的 `op` 是錯誤，不是「沒觸發」
-- -----------------------------------------------------------------------------
-- 管理員把 `op` 打成 `">="` 以外的東西時，最糟的行為是**當成條件不成立** ——
-- 那條規則從此永遠不響，而沒有任何訊號。因此不認得的 op 會被收集進
-- `bad_rule_codes`，由端點放進 `errors[]`。
--
-- -----------------------------------------------------------------------------
-- `debounce_seconds` 仍然沒有讀者，這是刻意的
-- -----------------------------------------------------------------------------
-- 那個欄位（預設 60）在整個 codebase 裡只有宣告。它的語意是「同一條規則兩次
-- 觸發的最小間隔」，而 `raise_alarm` 的 `dedupe_window_minutes`（預設 120 分）
-- 已經涵蓋了更長的窗 —— 在現行設定下實作它**不會改變任何可觀察的行為**。
--
-- 加一個沒有效果的判斷，只會讓下一個人以為那個欄位在起作用。
-- 誠實記在這裡，而不是假裝讀了它。
--
-- 依賴：006（alarm_rules／telemetry_points／raise_alarm）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- 不是 SECURITY DEFINER：呼叫端已注入租戶與場域情境，RLS 照常生效。
-- 看不到的規則就是不存在 —— 這支函式不該成為跨場域觸發告警的後門。
CREATE OR REPLACE FUNCTION fms.evaluate_telemetry_rules(
  p_telemetry_point_id uuid,
  p_value              numeric,
  p_observed_at        timestamptz DEFAULT NULL
) RETURNS TABLE (raised int, skipped_sustained int, bad_rule_codes text[])
LANGUAGE plpgsql
AS $$
DECLARE
  v_point_code text;
  v_rule       fms.alarm_rules;
  v_op         text;
  v_threshold  numeric;
  v_fired      boolean;
BEGIN
  raised := 0;
  skipped_sustained := 0;
  bad_rule_codes := ARRAY[]::text[];

  IF p_value IS NULL THEN
    RETURN NEXT;                      -- 非數值讀數沒有門檻可比
    RETURN;
  END IF;

  SELECT point_code INTO v_point_code
    FROM fms.telemetry_points WHERE id = p_telemetry_point_id;

  FOR v_rule IN
    SELECT * FROM fms.alarm_rules
     WHERE is_active
       AND (telemetry_point_id = p_telemetry_point_id
            OR (telemetry_point_id IS NULL
                AND point_code IS NOT NULL
                AND point_code = v_point_code))
  LOOP
    -- 掃描型：沒有讀數進來才觸發，不是這裡的事。
    IF v_rule.rule_type <> 'THRESHOLD' THEN
      skipped_sustained := skipped_sustained + 1;
      CONTINUE;
    END IF;

    -- 持續型：要回看歷史讀數，單筆判斷不出來。
    IF v_rule.condition ? 'for_seconds' THEN
      skipped_sustained := skipped_sustained + 1;
      CONTINUE;
    END IF;

    v_op := v_rule.condition->>'op';
    v_threshold := (v_rule.condition->>'value')::numeric;

    v_fired := CASE v_op
                 WHEN '>'  THEN p_value >  v_threshold
                 WHEN '>=' THEN p_value >= v_threshold
                 WHEN '<'  THEN p_value <  v_threshold
                 WHEN '<=' THEN p_value <= v_threshold
                 WHEN '='  THEN p_value =  v_threshold
                 WHEN '!=' THEN p_value <> v_threshold
               END;

    -- 不認得的 op → **不是「沒觸發」**。見檔頭。
    IF v_fired IS NULL THEN
      bad_rule_codes := bad_rule_codes || v_rule.code::text;
      CONTINUE;
    END IF;

    IF v_fired THEN
      PERFORM fms.raise_alarm(
        v_rule.id,
        p_telemetry_point_id,
        p_value,
        format('%s：%s %s %s（實測 %s）',
               v_rule.name, coalesce(v_point_code, '讀數'), v_op, v_threshold, p_value),
        coalesce(p_observed_at, clock_timestamp()));
      raised := raised + 1;
    END IF;
  END LOOP;

  RETURN NEXT;
END;
$$;

COMMENT ON FUNCTION fms.evaluate_telemetry_rules(uuid, numeric, timestamptz) IS
  '即時門檻評估：raise_alarm 的第一個生產呼叫者。'
  ' 只處理單筆讀數判斷得出來的 THRESHOLD 規則；持續型（for_seconds）與掃描型'
  '（DEVICE_OFFLINE）計入 skipped_sustained 而不是靜默忽略。'
  ' 不認得的 op 進 bad_rule_codes —— 當成「沒觸發」會讓那條規則永遠不響而無人知曉。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- (1)(2) 是行為的但不依賴 seed；(3)(4)(5) 是結構的。
-- 需要真實規則與租戶情境的驗證在 `telemetry_ingest_slice.rs`（見 (3) 的說明）。
DO $$
DECLARE
  r     record;
  v_src text := pg_get_functiondef(
          'fms.evaluate_telemetry_rules(uuid,numeric,timestamptz)'::regprocedure);
  v_point_soc uuid := 'a3000000-0000-4000-8000-000000000003';  -- BATTERY_SOC
BEGIN
  -- (1) 不存在的點：三個欄位都要有值，不能回 NULL。
  --     回 NULL 的話端點的加總會靜默變成 NULL 而不是 0。
  SELECT * INTO r FROM fms.evaluate_telemetry_rules(gen_random_uuid(), 1.0);
  IF r.raised IS NULL OR r.skipped_sustained IS NULL OR r.bad_rule_codes IS NULL THEN
    RAISE EXCEPTION '057 FAILED: 找不到規則時回了 NULL —— 端點的加總會變成 NULL';
  END IF;
  IF r.raised <> 0 OR r.skipped_sustained <> 0 THEN
    RAISE EXCEPTION '057 FAILED: 不存在的點竟然觸發了規則';
  END IF;

  -- (2) NULL 值（布林／文字讀數）不能爆炸。
  SELECT * INTO r FROM fms.evaluate_telemetry_rules(v_point_soc, NULL);
  IF r.raised <> 0 THEN
    RAISE EXCEPTION '057 FAILED: NULL 讀數不該觸發門檻';
  END IF;

  -- (3) 結構：持續型與掃描型必須被**計數**跳過，不能靜默忽略。
  --
  --     第一版在這裡做行為驗證（真的丟一筆 SOC 12 進去看有沒有觸發）。
  --     兩個問題：migration 沒有租戶情境，而 `alarm_rules` 有 RLS ——
  --     那條規則根本看不到，於是整段靜默跳過（輸出寫著「未 seed」而資料庫
  --     其實有 seed）。加平台情境可以看到規則，但 (4) 會**真的建出告警與工單**
  --     並留在資料庫裡 —— migration 不該留下業務資料。
  --
  --     這是 053／054／056 記過的同一個層次問題：只依賴結構的放這裡，
  --     需要資料與情境的放切片測試。行為驗證在 `telemetry_ingest_slice.rs`。
  IF v_src NOT LIKE '%skipped_sustained := skipped_sustained + 1%' THEN
    RAISE EXCEPTION '057 FAILED: 沒有計數跳過的規則 —— 「設定了但永遠不會響」會看不見';
  END IF;
  IF v_src NOT LIKE '%for_seconds%' THEN
    RAISE EXCEPTION '057 FAILED: 沒有處理 for_seconds（持續型）';
  END IF;

  -- (4) 不認得的 op 必須進 bad_rule_codes，不能當成「沒觸發」。
  IF v_src NOT LIKE '%bad_rule_codes := bad_rule_codes%' THEN
    RAISE EXCEPTION
      '057 FAILED: 未知的 op 沒有被收集 —— 那條規則會永遠不響而沒有任何訊號';
  END IF;

  -- (5) 不可以是 SECURITY DEFINER：那會讓它成為跨場域觸發告警的後門。
  IF EXISTS (SELECT 1 FROM pg_proc
              WHERE oid = 'fms.evaluate_telemetry_rules(uuid,numeric,timestamptz)'::regprocedure
                AND prosecdef) THEN
    RAISE EXCEPTION '057 FAILED: 函式是 SECURITY DEFINER';
  END IF;

  RAISE NOTICE '057 OK：跳過會計數、未知 op 會收集、非 SECURITY DEFINER'
               '（門檻真的會觸發由 telemetry_ingest_slice.rs 驗）';
END;
$$;

COMMIT;
