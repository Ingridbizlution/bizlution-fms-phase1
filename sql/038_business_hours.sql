-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 038: 營業時間與假日行事曆（讓 business_hours_only 真的算得出來）
-- =============================================================================
-- ADR-12 決定 C 當時寫「一律以自然時間計」，理由是**沒有任何函式能算營業
-- 時間內的經過時間** —— 需要 `facilities.operating_hours` 加一張假日表，
-- 而假日表不存在。後果是 `GET /reports/sla-compliance` 的 `strict` 模式必須
-- 整批排除宣告了 `business_hours_only` 的政策，而種子的 `SLA_STANDARD`
-- （MEDIUM，也就是多數工單）就宣告了它。
--
-- `operating_hours` 本身是第十個「宣告了沒人讀」：001 建了欄位、009 種了
-- 內容（總部週一至五 08:00–21:00、週六 09:00–17:00、週日不營業），
-- 而全 repo 只有 `fms-tenancy` 在存取與回傳它，零個評估點。
--
-- -----------------------------------------------------------------------------
-- 關鍵決定：在**算 due 的時候**把營業時間算進去
-- -----------------------------------------------------------------------------
-- 直覺的做法是在報表裡算「營業時間內的經過時間」。那是錯的方向。
--
-- 決定 F 已經讓 `response_due_at`／`resolution_due_at` 是**絕對時刻的快照**。
-- 因此正確的位置是開單那一刻：
--
--     resolution_due_at = 起算時刻 + N 個「營業分鐘」
--
-- 算出來仍然是一個絕對時刻。於是：
--
--   * 033 的掃描（`now() > resolution_due_at`）不用改一個字，而且是對的
--   * 034 的報表（`completed_at <= resolution_due_at`）不用改一個字，也是對的
--   * 決定 F 的快照語意保留 —— 之後改營業時間或補假日，已開的單不受影響
--   * **營業時間的邏輯只存在於一個地方**
--
-- 把它放在報表裡則相反：每一個讀取點都要重算一次，而且政策或行事曆一改，
-- 上個月的報表就變了。
--
-- -----------------------------------------------------------------------------
-- 剩下的那個缺口：場域沒有定義營業時間
-- -----------------------------------------------------------------------------
-- 政策說要算營業時間，但場域的 `operating_hours` 是 `{}`（種子裡的測試場域
-- 就是這樣）。這種情況下算不出來，而兩個顯而易見的處理都不好：
--
--   * 當成 24/7 → 期限會比預期**緊得多**（週五晚上的單變成週六早上到期，
--     而不是週一），而且沒有任何人知道
--   * 不給期限 → 整張工單退出量測
--
-- 因此：**以自然時間算出期限，但把「怎麼算的」記在工單上**
-- （`sla_basis`）。那是一個快照，因此報表的分類不會因為之後補了營業時間
-- 而回溯改變 —— 與決定 F 同一個理由。
--
-- `strict` 模式排除 `NATURAL_FALLBACK` 的工單、`operational` 納入並計數。
-- 也就是說 `strictness` 保留它的意義與那唯一一個行為差異，只是適用範圍從
-- 「所有 business_hours_only 的政策」縮到「設定不完整的場域」。
--
-- 依賴：001（facilities.operating_hours、timezone）、032（算 due 的觸發器）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 1. operating_hours 的形狀約束
-- -----------------------------------------------------------------------------
-- 這個欄位從「存了沒人看」變成「決定合約期限」，因此它的形狀開始有後果。
-- 一個 `"08:0"` 會讓 `::time` 轉型在**開單時**失敗 —— 那個時候使用者只看到
-- 一個失敗的請求，而原因在三層之外的場域設定裡。
--
-- 形狀：`{"mon": [["08:00","21:00"], ...], ...}`
--   * 鍵是三字母小寫星期（缺鍵＝該日不營業）
--   * 值是 [開始, 結束] 的陣列；結束可以是 `"24:00"`（PostgreSQL 的 time
--     上界，`date + '24:00'::time` 正好是隔日午夜）
--   * 結束必須嚴格晚於開始
CREATE OR REPLACE FUNCTION fms.time_windows_are_valid(p jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE
    WHEN p IS NULL THEN true
    WHEN jsonb_typeof(p) <> 'array' THEN false
    ELSE coalesce((
      SELECT bool_and(
               jsonb_typeof(w) = 'array'
               AND jsonb_array_length(w) = 2
               AND jsonb_typeof(w -> 0) = 'string'
               AND jsonb_typeof(w -> 1) = 'string'
               AND (w ->> 0) ~ '^([01][0-9]|2[0-4]):[0-5][0-9]$'
               AND (w ->> 1) ~ '^([01][0-9]|2[0-4]):[0-5][0-9]$'
               AND (w ->> 1)::time > (w ->> 0)::time)
        FROM jsonb_array_elements(p) w
    ), true)
  END;
$$;

COMMENT ON FUNCTION fms.time_windows_are_valid(jsonb) IS
  '一組 [["08:00","21:00"], ...] 的時段。operating_hours 的每個星期與'
  'holiday_calendars.windows 用的是同一個形狀，因此驗證只有一份。';

CREATE OR REPLACE FUNCTION fms.operating_hours_are_valid(p jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE
    WHEN p IS NULL THEN true
    WHEN jsonb_typeof(p) <> 'object' THEN false
    ELSE coalesce((
      SELECT bool_and(
               day.key IN ('mon','tue','wed','thu','fri','sat','sun')
               AND fms.time_windows_are_valid(day.value))
        FROM jsonb_each(p) day
    ), true)
  END;
$$;

COMMENT ON FUNCTION fms.operating_hours_are_valid(jsonb) IS
  'facilities.operating_hours 的形狀。038 讓這個欄位決定 SLA 期限，'
  '因此壞掉的值要在寫入時擋，而不是在開單時才炸。';

ALTER TABLE fms.facilities
  DROP CONSTRAINT IF EXISTS ck_facilities_operating_hours;
ALTER TABLE fms.facilities
  ADD CONSTRAINT ck_facilities_operating_hours
  CHECK (fms.operating_hours_are_valid(operating_hours));

-- -----------------------------------------------------------------------------
-- 2. 假日行事曆
-- -----------------------------------------------------------------------------
-- `facility_id IS NULL` 代表租戶通用 —— 與 `sla_policies` 同一個慣例，
-- 而且解析時場域專屬的優先（某棟樓可能有自己的休館日）。
--
-- `is_working_day` 是**補班日**：台灣的行事曆有「週六上班」，而那不是
-- 例外中的例外，是每年都有的常態。少了這個布林值，補班日只能靠
-- 修改 `operating_hours` 表達，而那會影響到每一個同星期的日子。
CREATE TABLE IF NOT EXISTS fms.holiday_calendars (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id    uuid REFERENCES fms.facilities(id) ON DELETE CASCADE,
  holiday_date   date NOT NULL,
  name           varchar(100) NOT NULL,
  -- false = 這一天不營業（即使 operating_hours 說有）
  -- true  = 這一天營業（即使那個星期平常不營業）—— 補班日
  is_working_day boolean NOT NULL DEFAULT false,
  -- 補班日的時段。NULL = 沿用那個星期在 operating_hours 裡的時段。
  --
  -- 這個欄位是驗算時逼出來的：**補班日通常落在平常不營業的星期**
  -- （台灣的補班日是週六，而多數辦公場域只排週一至五）。少了它，
  -- `is_working_day = true` 對它唯一的用途無效 —— 那個星期沒有時段，
  -- 一天能做 0 分鐘。
  windows        jsonb
                   CHECK (windows IS NULL OR fms.time_windows_are_valid(windows)),
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at     timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- 同一個 (場域, 日期) 只能有一筆。`NULLS NOT DISTINCT` 的理由與 037 相同：
-- 租戶通用的那一類 `facility_id` 就是 NULL，少了它最常見的重複擋不住。
CREATE UNIQUE INDEX IF NOT EXISTS uq_holiday_calendars_date
  ON fms.holiday_calendars (tenant_id, facility_id, holiday_date)
  NULLS NOT DISTINCT;

-- 解析時要按 (日期, 場域專屬優先) 找，因此索引涵蓋日期。
CREATE INDEX IF NOT EXISTS idx_holiday_calendars_lookup
  ON fms.holiday_calendars (tenant_id, holiday_date);

ALTER TABLE fms.holiday_calendars ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.holiday_calendars FORCE ROW LEVEL SECURITY;

-- 與 `sla_policies` 完全相同的兩條政策。`facility_in_scope(NULL)` 為真，
-- 因此租戶通用的假日對每個場域範圍的使用者都可見 —— 那是必要的，
-- 否則同一張工單的期限會取決於是誰開的單。
--
-- **`CREATE POLICY` 沒有 `IF NOT EXISTS`**，因此先 DROP。少了這兩行，
-- 本檔在已經套用過的資料庫上重跑會在這裡失敗、整個交易回滾 ——
-- 而那個失敗一度讓五個突變測試「全部通過」（突變根本沒套用進去）。
-- 036 也踩過同一個形狀的問題。
DROP POLICY IF EXISTS tenant_isolation ON fms.holiday_calendars;
DROP POLICY IF EXISTS facility_scope ON fms.holiday_calendars;

CREATE POLICY tenant_isolation ON fms.holiday_calendars
  USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
  WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

CREATE POLICY facility_scope ON fms.holiday_calendars
  USING (fms.is_platform_context() OR fms.facility_in_scope(facility_id))
  WITH CHECK (fms.is_platform_context() OR fms.facility_in_scope(facility_id));

GRANT SELECT, INSERT, UPDATE, DELETE ON fms.holiday_calendars TO fms_app;
GRANT SELECT ON fms.holiday_calendars TO fms_readonly;

COMMENT ON TABLE fms.holiday_calendars IS
  'SLA 期限計算用的假日／補班日。facility_id IS NULL = 租戶通用（場域專屬優先）。'
  'is_working_day = false 表示放假；true 表示補班（windows 為 NULL 時沿用該星期的班表）。';

-- -----------------------------------------------------------------------------
-- 3. 某一天的有效時段
-- -----------------------------------------------------------------------------
-- 回傳那一天實際可用的時段陣列；空陣列代表不營業。
--
-- 把「營不營業」與「營業幾點到幾點」合成一個函式，是因為兩者的答案來自
-- 同一次解析：假日覆寫可能同時改變兩件事（補班日既是工作日、又可能有
-- 自己的班表）。分成兩個函式會需要解析兩次，而兩次解析就會有機會不一致。
--
-- 優先序：場域專屬的行事曆 > 租戶通用的行事曆 > `operating_hours` 的星期鍵。
CREATE OR REPLACE FUNCTION fms.business_windows(
  p_facility_id uuid,
  p_date        date
) RETURNS jsonb
LANGUAGE plpgsql
STABLE
AS $$
DECLARE
  v_cal    record;
  v_hours  jsonb;
  v_key    text;
BEGIN
  SELECT hc.is_working_day, hc.windows INTO v_cal
    FROM fms.holiday_calendars hc
   WHERE hc.holiday_date = p_date
     AND (hc.facility_id IS NULL OR hc.facility_id = p_facility_id)
   ORDER BY (hc.facility_id IS NOT NULL) DESC
   LIMIT 1;

  -- 放假：不管班表怎麼寫。
  IF v_cal.is_working_day IS NOT NULL AND NOT v_cal.is_working_day THEN
    RETURN '[]'::jsonb;
  END IF;

  -- 補班日且自帶班表 → 用它。這是這個欄位存在的理由：補班日通常落在
  -- 平常不營業的星期，沿用班表會得到空的時段。
  IF v_cal.is_working_day AND v_cal.windows IS NOT NULL THEN
    RETURN v_cal.windows;
  END IF;

  SELECT f.operating_hours INTO v_hours
    FROM fms.facilities f WHERE f.id = p_facility_id;
  IF v_hours IS NULL THEN
    RETURN '[]'::jsonb;
  END IF;

  v_key := (ARRAY['mon','tue','wed','thu','fri','sat','sun'])[
             extract(isodow FROM p_date)::int];
  RETURN coalesce(v_hours -> v_key, '[]'::jsonb);
END;
$$;

COMMENT ON FUNCTION fms.business_windows(uuid, date) IS
  '某個場域某一天實際可用的營業時段；空陣列＝不營業。'
  '解析順序：場域專屬行事曆 > 租戶通用行事曆 > operating_hours 的星期鍵。';

-- -----------------------------------------------------------------------------
-- 4. 加上 N 個營業分鐘
-- -----------------------------------------------------------------------------
-- 回傳絕對時刻，或 NULL（場域沒有定義營業時間 → 呼叫端決定退路）。
--
-- 逐日往前走，把每個時段裡還沒用掉的部分吃掉。上限 400 天：一個在一年內
-- 都滿足不了的期限是設定錯誤，而不是一個很長的期限 —— 讓它拋錯而不是
-- 無限迴圈。
CREATE OR REPLACE FUNCTION fms.add_business_minutes(
  p_facility_id uuid,
  p_from        timestamptz,
  p_minutes     int
) RETURNS timestamptz
LANGUAGE plpgsql
STABLE
AS $$
DECLARE
  v_tz        text;
  v_hours     jsonb;
  v_today     jsonb;
  v_remaining interval;
  v_cursor    timestamptz := p_from;
  v_date      date;
  v_day       int := 0;
  v_win       jsonb;
  v_start     timestamptz;
  v_end       timestamptz;
  v_from_ts   timestamptz;
  v_avail     interval;
BEGIN
  IF p_facility_id IS NULL OR p_minutes IS NULL OR p_minutes <= 0 THEN
    RETURN NULL;
  END IF;

  SELECT f.timezone, f.operating_hours INTO v_tz, v_hours
    FROM fms.facilities f WHERE f.id = p_facility_id;

  -- 沒有定義營業時間 → 算不出來。**不要偷偷當成 24/7**：那會讓期限比
  -- 預期緊得多（週五晚上的單變成週六早上到期），而且沒有人會知道。
  IF v_tz IS NULL OR v_hours IS NULL OR v_hours = '{}'::jsonb THEN
    RETURN NULL;
  END IF;

  v_remaining := make_interval(mins => p_minutes);
  v_date := (p_from AT TIME ZONE v_tz)::date;

  WHILE v_day < 400 LOOP
    v_today := fms.business_windows(p_facility_id, v_date);
    IF jsonb_array_length(v_today) > 0 THEN
      -- 時段依開始時刻排序：管理者寫進 jsonb 的順序不保證，而吃掉時間的
      -- 演算法依賴順序。（重疊的時段會被重複計入，讓期限變寬 ——
      -- 那是設定問題，形狀約束擋不到，記在這裡。）
      FOR v_win IN
        SELECT w FROM jsonb_array_elements(v_today) w
         ORDER BY (w ->> 0)::time
      LOOP
        v_start := (v_date + (v_win ->> 0)::time) AT TIME ZONE v_tz;
        v_end   := (v_date + (v_win ->> 1)::time) AT TIME ZONE v_tz;

        IF v_end <= v_cursor THEN
          CONTINUE;  -- 這個時段已經過去了
        END IF;

        v_from_ts := greatest(v_cursor, v_start);
        v_avail := v_end - v_from_ts;

        IF v_avail >= v_remaining THEN
          RETURN v_from_ts + v_remaining;
        END IF;
        v_remaining := v_remaining - v_avail;
      END LOOP;
    END IF;

    v_date := v_date + 1;
    -- 游標**不重設**。第一版在這裡把它移到隔日午夜，理由寫的是「否則第一天
    -- 的已過去判定會套用到之後每一天」—— 那個理由是錯的：
    -- `greatest(v_cursor, v_start)` 已經處理了，之後每一天的時段開始時刻
    -- 都晚於原始游標，因此 greatest 一律選中時段開始。
    --
    -- 突變測試把那一行拿掉，11 個測試全部通過 —— 它是多餘的。
    -- 留著一行聲稱自己在防某件事而其實沒有的程式碼，比沒有它更誤導人。
    v_day := v_day + 1;
  END LOOP;

  RAISE EXCEPTION
    'SLA_TARGET_UNREACHABLE: 場域 % 的營業時間在 400 天內累積不到 % 分鐘',
    p_facility_id, p_minutes USING ERRCODE = '22023';
END;
$$;

COMMENT ON FUNCTION fms.add_business_minutes(uuid, timestamptz, int) IS
  '起算時刻 + N 個營業分鐘，回傳絕對時刻。場域未定義營業時間時回 NULL。'
  '刻意在算 due 時使用（決定 F 的快照），因此掃描與報表都不需要營業時間邏輯。';

-- -----------------------------------------------------------------------------
-- 5. 工單記下「期限是怎麼算的」
-- -----------------------------------------------------------------------------
-- 快照，理由與決定 F 相同：之後補了營業時間或假日，已開的單的分類不該改變。
ALTER TABLE fms.work_orders
  ADD COLUMN IF NOT EXISTS sla_basis text
    CHECK (sla_basis IN ('NATURAL', 'BUSINESS_HOURS', 'NATURAL_FALLBACK'));

COMMENT ON COLUMN fms.work_orders.sla_basis IS
  'NATURAL = 政策沒有要求營業時間；BUSINESS_HOURS = 用場域行事曆算的；'
  'NATURAL_FALLBACK = 政策要求營業時間但場域沒有定義，退回自然時間。'
  '報表的 strict 模式排除 NATURAL_FALLBACK（見 migration 039）。';

-- -----------------------------------------------------------------------------
-- 6. 算 due 的地方改用它
-- -----------------------------------------------------------------------------
-- 只有一個地方需要知道怎麼算：這個輔助函式。觸發器與狀態機都呼叫它。
CREATE OR REPLACE FUNCTION fms.sla_target_at(
  p_facility_id        uuid,
  p_from               timestamptz,
  p_minutes            int,
  p_business_hours_only boolean,
  OUT o_due            timestamptz,
  OUT o_basis          text
)
LANGUAGE plpgsql
STABLE
AS $$
BEGIN
  IF NOT p_business_hours_only THEN
    o_due := p_from + make_interval(mins => p_minutes);
    o_basis := 'NATURAL';
    RETURN;
  END IF;

  o_due := fms.add_business_minutes(p_facility_id, p_from, p_minutes);
  IF o_due IS NOT NULL THEN
    o_basis := 'BUSINESS_HOURS';
    RETURN;
  END IF;

  -- 政策要求營業時間、場域沒有定義。給出期限（有目標比沒目標好），
  -- 但標記它是退路 —— 報表的 strict 模式據此排除。
  o_due := p_from + make_interval(mins => p_minutes);
  o_basis := 'NATURAL_FALLBACK';
END;
$$;

CREATE OR REPLACE FUNCTION fms.trg_work_order_sla_targets()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_policy fms.sla_policies;
  v_from   timestamptz;
  v_due    timestamptz;
  v_basis  text;
BEGIN
  -- DRAFT 還沒送出 → 不起算。
  IF NEW.status = 'DRAFT' THEN
    RETURN NEW;
  END IF;

  -- `raise_alarm` 會從告警規則帶 `wo_sla_policy_id` 進來。那是個明示的選擇，
  -- 不該被解析結果蓋掉 —— 但它同樣需要算出 due（006 只設了 id）。
  IF NEW.sla_policy_id IS NOT NULL THEN
    SELECT * INTO v_policy FROM fms.sla_policies WHERE id = NEW.sla_policy_id;
  ELSE
    v_policy := fms.resolve_sla_policy(NEW.tenant_id, NEW.facility_id, NEW.priority);
    NEW.sla_policy_id := v_policy.id;
  END IF;

  IF v_policy.id IS NULL THEN
    -- 解析不到 policy。**標 NOT_APPLICABLE 而不是留 ON_TRACK**：
    -- ON_TRACK 是「有目標且還沒逾期」，這裡是「沒有目標」。
    NEW.sla_state := 'NOT_APPLICABLE';
    RETURN NEW;
  END IF;

  v_from := coalesce(NEW.created_at, clock_timestamp());

  SELECT o_due, o_basis INTO v_due, v_basis
    FROM fms.sla_target_at(NEW.facility_id, v_from,
                           v_policy.response_minutes, v_policy.business_hours_only);
  NEW.response_due_at := v_due;

  SELECT o_due, o_basis INTO v_due, v_basis
    FROM fms.sla_target_at(NEW.facility_id, v_from,
                           v_policy.resolution_minutes, v_policy.business_hours_only);
  NEW.resolution_due_at := v_due;
  NEW.sla_basis := v_basis;
  RETURN NEW;
END;
$$;

-- -----------------------------------------------------------------------------
-- 7. 回填 sla_basis
-- -----------------------------------------------------------------------------
-- 既有工單的 due 是用自然時間算的（032／036 當時沒有別的選擇）。
-- 標成什麼取決於政策當時要求什麼 —— 而政策可能已經被改過，
-- 因此這是**估算**，不是重建。既有工單不重算 due（那會回溯改變報表）。
UPDATE fms.work_orders wo
   SET sla_basis = CASE
         WHEN sp.business_hours_only THEN 'NATURAL_FALLBACK'
         ELSE 'NATURAL' END
  FROM fms.sla_policies sp
 WHERE sp.id = wo.sla_policy_id
   AND wo.sla_basis IS NULL
   AND wo.resolution_due_at IS NOT NULL;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 本檔在 CORE 裡執行、早於 009 → 沒有場域也沒有政策（第五次了，不再踩）。
-- 行為在 `business_hours_slice.rs`。這裡只驗不依賴租戶資料的東西。
DO $$
BEGIN
  -- (1) 形狀函式。直接測，不透過 UPDATE（沒有列可以更新）。
  IF NOT fms.operating_hours_are_valid('{"mon": [["08:00","21:00"]]}'::jsonb) THEN
    RAISE EXCEPTION '038 FAILED: 正常形狀被判為不合法';
  END IF;
  IF NOT fms.operating_hours_are_valid('{"sun": [["10:00","24:00"]]}'::jsonb) THEN
    RAISE EXCEPTION '038 FAILED: 24:00 是合法的結束時刻（date + 24:00 = 隔日午夜）';
  END IF;
  IF NOT fms.operating_hours_are_valid('{}'::jsonb) THEN
    RAISE EXCEPTION '038 FAILED: 空物件是合法的（＝沒有定義營業時間）';
  END IF;
  IF fms.operating_hours_are_valid('{"monday": [["08:00","21:00"]]}'::jsonb) THEN
    RAISE EXCEPTION '038 FAILED: 星期鍵必須是三字母小寫';
  END IF;
  IF fms.operating_hours_are_valid('{"mon": [["08:0","21:00"]]}'::jsonb) THEN
    RAISE EXCEPTION '038 FAILED: 壞掉的時刻字串應被擋（否則在開單時才炸）';
  END IF;
  IF fms.operating_hours_are_valid('{"mon": [["21:00","08:00"]]}'::jsonb) THEN
    RAISE EXCEPTION '038 FAILED: 結束必須晚於開始';
  END IF;
  IF fms.operating_hours_are_valid('{"mon": [["08:00"]]}'::jsonb) THEN
    RAISE EXCEPTION '038 FAILED: 時段必須是兩個元素';
  END IF;

  -- (2) 約束掛上去了。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conrelid = 'fms.facilities'::regclass
       AND conname = 'ck_facilities_operating_hours'
  ) THEN
    RAISE EXCEPTION '038 FAILED: 缺少 ck_facilities_operating_hours';
  END IF;

  -- (3) 行事曆表的 RLS。忘記 FORCE 的症狀是「owner 看得到全部」——
  --     而 migration 與背景作業都以 owner 連線，因此不會有人發現。
  IF NOT EXISTS (
    SELECT 1 FROM pg_class
     WHERE oid = 'fms.holiday_calendars'::regclass
       AND relrowsecurity AND relforcerowsecurity
  ) THEN
    RAISE EXCEPTION '038 FAILED: holiday_calendars 必須啟用並強制 RLS';
  END IF;

  -- (4) 沒有場域時 add_business_minutes 回 NULL 而不是拋錯 ——
  --     `sla_target_at` 的退路完全依賴這個約定。
  IF fms.add_business_minutes(NULL, clock_timestamp(), 60) IS NOT NULL THEN
    RAISE EXCEPTION '038 FAILED: facility 為 NULL 應回 NULL';
  END IF;

  -- (5) 時段形狀函式被兩個地方共用，因此單獨驗它。
  IF NOT fms.time_windows_are_valid('[]'::jsonb) THEN
    RAISE EXCEPTION '038 FAILED: 空時段陣列是合法的（＝不營業）';
  END IF;
  IF fms.time_windows_are_valid('[["09:00","09:00"]]'::jsonb) THEN
    RAISE EXCEPTION '038 FAILED: 零長度的時段應被擋';
  END IF;

  RAISE NOTICE '038 OK: 營業時間與假日行事曆就緒';
END;
$$;

COMMIT;
