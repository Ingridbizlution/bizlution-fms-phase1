-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 036: 掃描的兩個門檻改由目錄決定，不再寫死
-- =============================================================================
-- 033／035 把兩個**管理者可以定義的條件**寫進了程式：
--
--   1. `p_at_risk_fraction = 0.8` —— 預警門檻，全域一個值
--   2. 升級迴圈的 `status IN ('ASSIGNED','IN_PROGRESS')` —— 哪些狀態可升級
--
-- 兩者在資料庫裡都已經有權威來源，而我沒有去讀它們。
--
-- -----------------------------------------------------------------------------
-- (1) 預警門檻：`sla_policies.escalation_rules`
-- -----------------------------------------------------------------------------
-- 004 宣告了這個欄位，009 也種了內容，而**零個讀取點**：
--
--     SLA_CRITICAL  [{at_pct: 75, notify:[TEAM_LEAD]},
--                    {at_pct: 100, notify:[FACILITY_ADMIN], escalate_priority: true}]
--     SLA_STANDARD  [{at_pct: 80, notify:[TEAM_LEAD]}]
--     SLA_CLEANING  [{at_pct: 100, notify:[FACILITY_ADMIN]}]
--
-- 也就是說「什麼時候該提醒」這件事，每個 policy 早就各自宣告過了 ——
-- 而 033 用一個全域的 0.8 蓋掉了三者。SLA_CRITICAL 的 75 與
-- SLA_CLEANING 的「不要預警」都被忽略。
--
-- 新規則：**預警門檻 = `escalation_rules` 裡 `at_pct < 100` 的最小值。**
--   * `at_pct = 100` 是逾期本身，不是預警
--   * 沒有 `< 100` 的規則 → **這個 policy 不預警**。SLA_CLEANING 就是這樣
--     宣告的，那是管理者的選擇，不該由一個預設值代為決定
--     （與 032 的 `resolve_sla_policy` 刻意沒有 default 後備同一個立場）
--
-- 因此 `p_at_risk_fraction` 這個參數**整個移除**。留著它就是留一個能蓋掉
-- 管理者設定的旋鈕。
--
-- -----------------------------------------------------------------------------
-- (2) 可升級的狀態：`work_order_transitions_allowed`
-- -----------------------------------------------------------------------------
-- 035 把 `('ASSIGNED','IN_PROGRESS')` 寫進迴圈，並加了一條自我驗證去擋
-- 「目錄新增規則但迴圈沒跟上」。那條驗證本身就是重複的證據 ——
-- 而且它把事情做反了：它會讓管理者**加規則就跑不了 migration**。
--
-- 新做法：不篩狀態，全部交給 `transition_work_order` 判斷，
-- 用 SQLSTATE 分辨兩種結果：
--
--   * `23514`（check_violation）＝目錄不允許從這個狀態升級。
--     **這不是失敗，是不在範圍內**，計入 `not_escalatable`。
--   * 其他錯誤 ＝ 真的失敗（例如競態、或有人給系統動作加了權限要求），
--     計入 `escalation_failed`。
--
-- 代價是對升不了的工單會多一次目錄查詢；換到的是**目錄成為唯一真實來源**。
-- 管理者若補上 `SUBMITTED → SLA_BREACHED`（以及配套的
-- `ASSIGN: SLA_BREACHED → ASSIGNED`，見 035 檔頭），升級範圍自動跟著變，
-- 不需要改任何程式。租戶專屬的規則也自動生效 —— 那是硬編清單做不到的。
--
-- `not_escalatable` 因此成為**覆蓋缺口的量測值**，而不只是註解裡的一段話。
--
-- 依賴：033、035。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- escalation_rules 的形狀約束
-- -----------------------------------------------------------------------------
-- 一旦管理者能編輯這個欄位，掃描就會去讀他打的字。一個 `"at_pct": "80%"`
-- 會讓 `::numeric` 轉型失敗 —— 而那個失敗發生在**掃描裡**，會讓
-- 所有租戶的那一輪標記一起回滾。
--
-- 因此在寫入時就擋掉。CHECK 不能含子查詢，所以包成 IMMUTABLE 函式。
CREATE OR REPLACE FUNCTION fms.escalation_rules_are_valid(p jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE
    WHEN p IS NULL THEN true
    WHEN jsonb_typeof(p) <> 'array' THEN false
    ELSE (
      -- `r ? 'at_pct'` 這一項不是多餘的：少了它，缺 at_pct 的規則會讓
      -- `jsonb_typeof(NULL) = 'number'` 得到 NULL，而 `bool_and` 忽略 NULL
      -- —— 於是 `[{"notify": ["FM"]}]` 會通過。那種規則掃描讀不到，
      -- 是一條永遠不會生效的設定，而管理者不會得到任何提示。
      SELECT coalesce(bool_and(
               jsonb_typeof(r) = 'object'
               AND r ? 'at_pct'
               AND jsonb_typeof(r -> 'at_pct') = 'number'
               AND (r ->> 'at_pct')::numeric > 0
               AND (r ->> 'at_pct')::numeric <= 100
             ), true)
        FROM jsonb_array_elements(p) r
    )
  END;
$$;

COMMENT ON FUNCTION fms.escalation_rules_are_valid(jsonb) IS
  'sla_policies.escalation_rules 的形狀：物件陣列，每個帶 (0,100] 的數值 at_pct。'
  '在寫入時擋，因為讀取端是每分鐘跑的跨租戶掃描 —— 在那裡失敗會拖垮整輪。';

ALTER TABLE fms.sla_policies
  DROP CONSTRAINT IF EXISTS ck_sla_escalation_rules;
ALTER TABLE fms.sla_policies
  ADD CONSTRAINT ck_sla_escalation_rules
  CHECK (fms.escalation_rules_are_valid(escalation_rules));

-- -----------------------------------------------------------------------------
-- 掃描
-- -----------------------------------------------------------------------------
-- 簽章變了兩處（少一個參數、多一個回傳欄），因此要 DROP。
-- 兩個簽章都丟：只丟 `(numeric)` 的話，在本檔已經套用過的資料庫上重跑會撞
-- 「function already exists with same argument types」——
-- migration 正常只跑一次，但開發時重跑是常態。
DROP FUNCTION IF EXISTS fms.sweep_sla_states(numeric);
DROP FUNCTION IF EXISTS fms.sweep_sla_states();

CREATE FUNCTION fms.sweep_sla_states()
RETURNS TABLE (
  at_risk             bigint,
  response_breached   bigint,
  resolution_breached bigint,
  escalated           bigint,
  not_escalatable     bigint,
  escalation_failed   bigint
)
LANGUAGE plpgsql
AS $$
DECLARE
  v_at_risk    bigint;
  v_response   bigint;
  v_resolution bigint;
  v_escalated  bigint := 0;
  v_skipped    bigint := 0;
  v_failed     bigint := 0;
  v_ids        uuid[] := '{}';
  v_id         uuid;
BEGIN
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
     RETURNING wo.id
  )
  SELECT count(*), coalesce(array_agg(id), '{}')
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
     RETURNING wo.id
  )
  SELECT count(*), v_ids || coalesce(array_agg(id), '{}')
    INTO v_resolution, v_ids
    FROM swept;

  -- (3) 有風險：達到該 policy 自己宣告的預警門檻，但還沒逾期。
  --
  -- 門檻來自 `escalation_rules` 裡 `at_pct < 100` 的最小值。沒有那樣的
  -- 規則就不預警 —— `warn` 是 INNER JOIN，因此 SLA_CLEANING（只宣告了
  -- at_pct 100）不會出現在這裡。那是管理者的選擇。
  --
  -- 窗口長度取自 policy 的 `resolution_minutes`。**這一個量不是快照** ——
  -- 事後調分鐘數會讓預警時點位移（`resolution_due_at` 不會，它是絕對時刻）。
  -- 刻意的取捨：預警是提醒，不是拿去談合約的數字。報表不讀 AT_RISK。
  WITH warn AS (
    SELECT sp.id AS policy_id,
           min((r ->> 'at_pct')::numeric) AS at_pct
      FROM fms.sla_policies sp
      CROSS JOIN LATERAL jsonb_array_elements(sp.escalation_rules) r
     WHERE (r ->> 'at_pct')::numeric < 100
     GROUP BY sp.id
  ), swept AS (
    UPDATE fms.work_orders wo
       SET sla_state = 'AT_RISK'
      FROM fms.work_order_statuses st,
           fms.sla_policies sp,
           warn w
     WHERE st.code = wo.status
       AND st.category IN ('OPEN', 'WAITING', 'IN_PROGRESS')
       AND sp.id = wo.sla_policy_id
       AND w.policy_id = sp.id
       AND wo.sla_state = 'ON_TRACK'
       AND wo.resolution_due_at IS NOT NULL
       AND clock_timestamp() < wo.resolution_due_at
       AND clock_timestamp() >= wo.resolution_due_at
             - make_interval(mins => ceil(sp.resolution_minutes
                                          * (1 - w.at_pct / 100.0))::int)
     RETURNING 1
  )
  SELECT count(*) INTO v_at_risk FROM swept;

  -- -------------------------------------------------------------------------
  -- (4) 升級
  -- -------------------------------------------------------------------------
  -- 標記先做、轉移後做：`BREACH_SLA` 的 side_effects 沒有 `compute_sla`，
  -- 因此 032 的 sla_state CASE 走 `ELSE sla_state`，保留剛標好的值。
  --
  -- **不篩狀態。** 哪些狀態可以升級是 `work_order_transitions_allowed`
  -- 說了算（含租戶專屬規則），這裡只負責問。用 SQLSTATE 分辨：
  --   23514 = 目錄不允許 → 不在範圍內，不是失敗
  --   其他   = 真的出事（競態、或有人給系統動作加了權限要求）
  --
  -- 逐筆包 EXCEPTION 是必要的：worker 在單一交易裡呼叫本函式，
  -- 讓一筆失敗炸掉整批會把**這一輪全部的標記回滾**，下一輪再撞同一張
  -- → 永久停擺。
  FOREACH v_id IN ARRAY v_ids LOOP
    BEGIN
      PERFORM fms.transition_work_order(v_id, 'BREACH_SLA', NULL, 'SLA 逾期自動升級');
      v_escalated := v_escalated + 1;
    EXCEPTION
      WHEN check_violation THEN
        v_skipped := v_skipped + 1;
      WHEN others THEN
        v_failed := v_failed + 1;
        RAISE NOTICE 'BREACH_SLA 對工單 % 失敗（%）—— 已標記逾期，狀態未變更',
          v_id, SQLERRM;
    END;
  END LOOP;

  RETURN QUERY SELECT v_at_risk, v_response, v_resolution,
                      v_escalated, v_skipped, v_failed;
END;
$$;

REVOKE ALL ON FUNCTION fms.sweep_sla_states() FROM PUBLIC, fms_app;
GRANT EXECUTE ON FUNCTION fms.sweep_sla_states() TO fms_owner;

COMMENT ON FUNCTION fms.sweep_sla_states() IS
  'ADR-12 量測鏈第 3 段 + 自動升級。呼叫者必須已在平台情境內；刻意不用 DEFINER。'
  '預警門檻來自各 policy 的 escalation_rules（at_pct < 100 的最小值；沒有就不預警）。'
  '可升級的狀態來自 work_order_transitions_allowed —— 兩者都不寫死在程式裡。'
  '先標回應再標解決，讓批次補跑的結果等於連續運行的結果。'
  'sla_state 是摘要；報表一律從時刻欄位計算。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_r record;
BEGIN
  -- (1) 空資料庫上全 0。
  SELECT * INTO v_r FROM fms.sweep_sla_states();
  IF v_r.at_risk <> 0 OR v_r.response_breached <> 0 OR v_r.resolution_breached <> 0
     OR v_r.escalated <> 0 OR v_r.not_escalatable <> 0 OR v_r.escalation_failed <> 0 THEN
    RAISE EXCEPTION '036 FAILED: 空資料庫應回全 0';
  END IF;

  -- (2) 仍然不是 DEFINER、仍然不給 fms_app。
  --     DROP + CREATE 會讓 007 的 ALTER DEFAULT PRIVILEGES 再次把 EXECUTE
  --     自動給 fms_app —— 這一格就是為了擋那個重置（033 已經被咬過一次）。
  IF (SELECT prosecdef FROM pg_proc
       WHERE pronamespace = 'fms'::regnamespace AND proname = 'sweep_sla_states') THEN
    RAISE EXCEPTION '036 FAILED: sweep_sla_states 不該是 SECURITY DEFINER';
  END IF;
  IF has_function_privilege('fms_app', 'fms.sweep_sla_states()', 'EXECUTE') THEN
    RAISE EXCEPTION '036 FAILED: fms_app 不該能執行 sweep_sla_states';
  END IF;

  -- (3) 形狀約束。這一格驗的是「管理者打錯字時，失敗發生在寫入端而不是
  --     每分鐘跑的掃描裡」—— 那是這個約束存在的全部理由。
  --
  --     **直接測函式，不透過 UPDATE。** 第一版寫了
  --     `UPDATE ... WHERE code = 'SLA_STANDARD'` 並期待 check_violation，
  --     但本檔在 CORE 裡執行、早於 009，那時一筆 policy 都沒有 ——
  --     UPDATE 影響 0 列、不會拋錯，於是自我驗證自己失敗了。
  --     （032 與 026／027 都記過同一件事。這是第三次。）
  IF fms.escalation_rules_are_valid('[{"at_pct": "80%"}]'::jsonb) THEN
    RAISE EXCEPTION '036 FAILED: 非數值的 at_pct 應被判定為不合法';
  END IF;
  IF fms.escalation_rules_are_valid('[{"at_pct": 0}]'::jsonb) THEN
    RAISE EXCEPTION '036 FAILED: at_pct = 0 應被判定為不合法';
  END IF;
  IF fms.escalation_rules_are_valid('[{"at_pct": 150}]'::jsonb) THEN
    RAISE EXCEPTION '036 FAILED: at_pct > 100 應被判定為不合法';
  END IF;
  IF fms.escalation_rules_are_valid('{"at_pct": 80}'::jsonb) THEN
    RAISE EXCEPTION '036 FAILED: 物件（非陣列）應被判定為不合法';
  END IF;
  IF fms.escalation_rules_are_valid('[{"notify": ["FM"]}]'::jsonb) THEN
    RAISE EXCEPTION '036 FAILED: 缺 at_pct 的規則永遠不會生效，應被判定為不合法';
  END IF;
  IF NOT fms.escalation_rules_are_valid('[]'::jsonb) THEN
    RAISE EXCEPTION '036 FAILED: 空陣列是合法的（＝這個 policy 不預警）';
  END IF;
  IF NOT fms.escalation_rules_are_valid(
       '[{"at_pct": 75, "notify": ["TEAM_LEAD"]}, {"at_pct": 100}]'::jsonb) THEN
    RAISE EXCEPTION '036 FAILED: 種子的形狀應該是合法的';
  END IF;

  -- 約束真的掛上去了（上面驗的是函式，這裡驗它有被用）。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conrelid = 'fms.sla_policies'::regclass
       AND conname = 'ck_sla_escalation_rules'
  ) THEN
    RAISE EXCEPTION '036 FAILED: 缺少 ck_sla_escalation_rules';
  END IF;

  -- 035 那條「目錄不得有 ASSIGNED/IN_PROGRESS 以外的 BREACH_SLA 規則」的
  -- 斷言**刻意不搬過來**：它會讓管理者一補規則就跑不了 migration，
  -- 而 036 的整個重點就是讓那件事變成合法的設定。
  RAISE NOTICE '036 OK: 預警門檻與可升級狀態都改由目錄決定';
END;
$$;

COMMIT;
