-- =============================================================================
-- 061：把 057 的兩個述詞抽成共用函式，讓試跑與真跑不會漂移
-- =============================================================================
-- 為什麼需要它
--
-- `POST /alarm-rules/{id}/test`（「以歷史資料試跑規則」）必須回答
-- 「這條規則在過去七天會響幾次」。它**不能呼叫 `evaluate_telemetry_rules`**
-- —— 那支會真的 `raise_alarm`，試跑不該有副作用。
--
-- 於是試跑得自己判斷「這筆讀數有沒有越界」與「這條規則管不管這個點位」。
-- 那就是**同一套語意的第二份實作**，而兩份實作只會往一個方向走：漂移。
--
-- 而漂移的症狀最糟：試跑說「會響 3 次」，上線之後響了 0 次或 30 次，
-- 而使用者對這個系統的信任建立在那個預覽上。
--
-- 所以這支 migration 把兩個述詞抽出來，並**改寫 057 的評估器去呼叫它們**。
-- 只有一份實作，兩邊都用它。
--
-- -----------------------------------------------------------------------------
-- NULL 的意義被保留下來
-- -----------------------------------------------------------------------------
-- 057 用「`v_fired IS NULL` 代表 op 不認得」來區分「沒觸發」與「規則設定錯」。
-- 那個區分是刻意的（見 057 檔頭），所以 `telemetry_rule_fires` 沿用它：
--
--   true  → 越界
--   false → 沒越界
--   NULL  → 這個 condition 判斷不出來（op 不認得、或缺 value）
--
-- 把 NULL 收斂成 false 會讓一條打錯字的規則看起來「正常但從沒響過」。
-- =============================================================================

BEGIN;

-- -----------------------------------------------------------------------------
-- (1) 單筆讀數對 condition 的判斷
-- -----------------------------------------------------------------------------
-- IMMUTABLE：同樣的 (condition, value) 永遠給同樣的答案。這讓它可以出現在
-- 索引運算式或大量列的 WHERE 裡而不必每列重算。
CREATE OR REPLACE FUNCTION fms.telemetry_rule_fires(
  p_condition jsonb,
  p_value     numeric
) RETURNS boolean
LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $$
  SELECT CASE
    -- 非數值讀數沒有門檻可比 —— 與 057 對 p_value IS NULL 的處理一致。
    WHEN p_value IS NULL THEN NULL
    -- 缺 value 的 THRESHOLD condition 是設定錯，不是「沒觸發」。
    WHEN p_condition->>'value' IS NULL THEN NULL
    ELSE CASE p_condition->>'op'
           WHEN '>'  THEN p_value >  (p_condition->>'value')::numeric
           WHEN '>=' THEN p_value >= (p_condition->>'value')::numeric
           WHEN '<'  THEN p_value <  (p_condition->>'value')::numeric
           WHEN '<=' THEN p_value <= (p_condition->>'value')::numeric
           WHEN '='  THEN p_value =  (p_condition->>'value')::numeric
           WHEN '!=' THEN p_value <> (p_condition->>'value')::numeric
           -- 不認得的 op：留 NULL。見檔頭。
         END
  END;
$$;

COMMENT ON FUNCTION fms.telemetry_rule_fires(jsonb, numeric) IS
  '單筆讀數是否越界。true=越界、false=沒越界、NULL=condition 判斷不出來（op 不認得或缺 value）。'
  '057 的評估器與 /alarm-rules/{id}/test 的試跑共用這一份。';

-- -----------------------------------------------------------------------------
-- (2) 規則的點位範圍
-- -----------------------------------------------------------------------------
-- 006 的 `ck_alarm_rule_scope` 允許三種：指定點位、依 point_code 套用、
-- 或 DEVICE_OFFLINE（不看點位）。這裡只表達前兩種 ——
-- 第三種不是「哪些點位」的問題。
-- 這支**一定回 true 或 false，永不回 NULL** —— 與上面那支刻意相反。
--
-- 抽出來的過程量到一件事：057 原本的述詞
--
--   telemetry_point_id = p_telemetry_point_id OR (telemetry_point_id IS NULL AND ...)
--
-- 在 `p_rule_point_id` 為 NULL 時左半邊是 NULL，於是整個運算式可能是
-- `NULL OR false` = **NULL**。它在 057 裡是對的，因為它待在 `WHERE` 裡
-- —— 過濾時 NULL 等於 false。但抽成回傳布林的函式之後那個 NULL 就漏出來了，
-- 而 `WHERE NOT covers(...)` 會一列都不回。
--
-- 這支 migration 自己的真值表抓到了它。
--
-- 這裡 NULL 也沒有意義可表達：一條沒有任何點位範圍的規則就是 DEVICE_OFFLINE 型，
-- 它**確定**不管任何點位 —— 那是 false，不是「不知道」。
CREATE OR REPLACE FUNCTION fms.alarm_rule_covers_point(
  p_rule_point_id   uuid,
  p_rule_point_code text,
  p_point_id        uuid,
  p_point_code      text
) RETURNS boolean
LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $$
  SELECT coalesce(
    (p_rule_point_id IS NOT NULL AND p_rule_point_id = p_point_id)
    OR (p_rule_point_id IS NULL
        AND p_rule_point_code IS NOT NULL
        AND p_point_code IS NOT NULL
        AND p_rule_point_code = p_point_code),
    false);
$$;

COMMENT ON FUNCTION fms.alarm_rule_covers_point(uuid, text, uuid, text) IS
  '這條規則管不管這個點位（指定點位，或依 point_code 套用）。'
  '057 的規則選取與 /alarm-rules/{id}/test 的範圍計算共用這一份。';

-- -----------------------------------------------------------------------------
-- (3) 改寫 057 的評估器去呼叫上面兩支
-- -----------------------------------------------------------------------------
-- 行為必須完全不變 —— 057 的 self-test 與 telemetry_ingest_slice.rs 都還在跑。
-- 這裡改的只有「判斷寫在哪裡」。
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
       AND fms.alarm_rule_covers_point(telemetry_point_id, point_code::text,
                                       p_telemetry_point_id, v_point_code)
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

    v_fired := fms.telemetry_rule_fires(v_rule.condition, p_value);

    -- 不認得的 op → **不是「沒觸發」**。見 057 檔頭。
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
               v_rule.name, coalesce(v_point_code, '讀數'),
               v_rule.condition->>'op', v_rule.condition->>'value', p_value),
        coalesce(p_observed_at, clock_timestamp()));
      raised := raised + 1;
    END IF;
  END LOOP;

  RETURN NEXT;
END;
$$;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 這一支的行為檢查**可以**放在 migration 裡：兩個述詞都是純函式，
-- 驗它們不需要業務資料，也不會留下任何東西。
-- 這與 059／060 的情況不同（那兩支需要使用者與場域情境）。
DO $$
DECLARE
  v_src text;
BEGIN
  -- (1) 真值表。每一個 op 都要驗，包含邊界值 —— `>` 與 `>=` 打錯一個字
  --     會讓一條「超過 28 度」的規則在剛好 28 度時的行為反過來。
  IF NOT (
       fms.telemetry_rule_fires('{"op":">","value":28}',  29) IS TRUE
   AND fms.telemetry_rule_fires('{"op":">","value":28}',  28) IS FALSE
   AND fms.telemetry_rule_fires('{"op":">=","value":28}', 28) IS TRUE
   AND fms.telemetry_rule_fires('{"op":"<","value":10}',   9) IS TRUE
   AND fms.telemetry_rule_fires('{"op":"<","value":10}',  10) IS FALSE
   AND fms.telemetry_rule_fires('{"op":"<=","value":10}', 10) IS TRUE
   AND fms.telemetry_rule_fires('{"op":"=","value":5}',    5) IS TRUE
   AND fms.telemetry_rule_fires('{"op":"!=","value":5}',   5) IS FALSE
  ) THEN
    RAISE EXCEPTION '061 FAILED: telemetry_rule_fires 的真值表不對';
  END IF;

  -- (2) NULL 的三種來源都必須是 NULL 而不是 false。
  --     收斂成 false 會讓設定錯的規則看起來「正常但從沒響過」。
  IF NOT (
       fms.telemetry_rule_fires('{"op":"~=","value":28}', 29) IS NULL   -- op 不認得
   AND fms.telemetry_rule_fires('{"op":">"}',             29) IS NULL   -- 缺 value
   AND fms.telemetry_rule_fires('{"op":">","value":28}', NULL) IS NULL  -- 非數值讀數
  ) THEN
    RAISE EXCEPTION '061 FAILED: 判斷不出來的 condition 必須回 NULL，不是 false';
  END IF;

  -- (3) 點位範圍：指定點位優先，且 point_code 只在未指定點位時生效。
  IF NOT (
       fms.alarm_rule_covers_point('11111111-1111-4111-8111-111111111111', NULL,
                                   '11111111-1111-4111-8111-111111111111', 'TEMP') IS TRUE
   AND fms.alarm_rule_covers_point('11111111-1111-4111-8111-111111111111', NULL,
                                   '22222222-2222-4222-8222-222222222222', 'TEMP') IS FALSE
   AND fms.alarm_rule_covers_point(NULL, 'TEMP',
                                   '22222222-2222-4222-8222-222222222222', 'TEMP') IS TRUE
   AND fms.alarm_rule_covers_point(NULL, 'TEMP',
                                   '22222222-2222-4222-8222-222222222222', 'CO2') IS FALSE
   -- 兩者皆 NULL：DEVICE_OFFLINE 型的規則不管任何點位。
   AND fms.alarm_rule_covers_point(NULL, NULL,
                                   '22222222-2222-4222-8222-222222222222', 'TEMP') IS FALSE
   -- 找不到 point_code 時也必須是 false 而不是 NULL。
   AND fms.alarm_rule_covers_point(NULL, 'TEMP',
                                   '22222222-2222-4222-8222-222222222222', NULL) IS FALSE
  ) THEN
    RAISE EXCEPTION '061 FAILED: alarm_rule_covers_point 的範圍判斷不對';
  END IF;

  -- (4) 評估器真的改用了共用述詞。少了這條，未來有人把 CASE 貼回去
  --     就會重新長出第二份實作，而測試不會有反應。
  SELECT prosrc INTO v_src FROM pg_proc
   WHERE proname = 'evaluate_telemetry_rules'
     AND pronamespace = 'fms'::regnamespace;
  IF v_src NOT LIKE '%fms.telemetry_rule_fires%'
     OR v_src NOT LIKE '%fms.alarm_rule_covers_point%' THEN
    RAISE EXCEPTION
      '061 FAILED: evaluate_telemetry_rules 沒有呼叫共用述詞 —— '
      '試跑與真跑會漂移，而那是這支 migration 唯一的理由';
  END IF;

  RAISE NOTICE '061 OK：真值表與範圍判斷正確、判斷不出來時回 NULL、'
               '評估器改用共用述詞（試跑與真跑一致由 alarm_rules_slice.rs 驗）';
END;
$$;

COMMIT;
