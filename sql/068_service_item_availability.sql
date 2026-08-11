-- =============================================================================
-- 068：服務項目的可用時段
-- =============================================================================
-- `GET /service-items/{id}/availability` 的落地處。
--
-- -----------------------------------------------------------------------------
-- `availability` 從這裡起有第一個讀者
-- -----------------------------------------------------------------------------
-- 004 建了那個欄位並在註解裡寫了形狀
-- （`{"mon":[["07:00","20:00"]], "blackout_dates":["2026-01-01"]}`），
-- 但 `fms-catalogue` 從來沒有讀過它 —— 量過：那個 crate 的四個檔案裡
-- 一次都沒有出現。所以它到現在是一個「存了但沒有人看」的欄位。
--
-- 與 065 對 `bookable_resources.opening_hours` 的處理一樣，一旦有讀者就要：
--
--   1. **加形狀約束。** 壞掉的值會在算時段的時候讓 `::time` 炸掉，
--      而那離設定它的人三層之外。038 的 `time_windows_are_valid` 與
--      `operating_hours_are_valid` 已經驗過星期鍵那一半，這裡只需要多驗
--      `blackout_dates`（一個日期字串陣列）。
--   2. **把解析寫成一個函式**，而不是讓應用層自己拆 jsonb。
--      那組規則有三層（服務自己的星期表 → 場域營運時間 → 停止服務日），
--      而兩份實作最後總會分歧。
--
-- -----------------------------------------------------------------------------
-- 解析順序，以及為什麼 blackout 在最前面
-- -----------------------------------------------------------------------------
--   1. `blackout_dates` 含這一天 → 空陣列（不營業），**不管星期表怎麼寫**。
--      停止服務日是例外覆寫，而例外的意義就是蓋過常規。
--   2. 服務自己有那個星期的時段 → 用它。
--   3. 否則退回**場域**的 `fms.business_windows()`（038）——
--      它連假日行事曆一起解，所以休館日自然不營業。
--      服務項目的 `facility_id` 為 NULL（全場域適用）時沒有場域可退，
--      此時回空陣列並讓呼叫端知道原因。
--
-- `lead_time_minutes` **不在這個函式裡**：它是「最晚要提前多久申請」，
-- 與「哪幾個小時營業」是兩件事。混在一起會讓一個 lead time 48 小時的服務
-- 看起來像「明天不營業」。端點分開回傳兩者。
--
-- 依賴：004（service_items.availability）、038（時段驗證與 business_windows）、
--       065（time_windows_hours）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) availability 的形狀
-- -----------------------------------------------------------------------------
-- `2026-02-30` 這種「形狀對但不存在」的日期要擋掉。
-- 單獨一個函式是因為 `::date` 轉型失敗會拋錯而不是回 false，
-- 而 CHECK 裡不能有拋錯的表達式（那會讓 INSERT 得到一個轉型錯誤而不是
-- 約束違反，訊息完全不同）。
CREATE OR REPLACE FUNCTION fms.is_a_date(p text)
RETURNS boolean
LANGUAGE plpgsql
IMMUTABLE
AS $$
BEGIN
  PERFORM p::date;
  RETURN true;
EXCEPTION WHEN others THEN
  RETURN false;
END;
$$;

-- 星期鍵那一半直接用 038 的驗證；這裡加的是 `blackout_dates`。
--
-- **未知的鍵一律拒絕** —— 這裡與 067 對 `tenants.settings` 的判斷相反，
-- 而理由是那個欄位的**用途不同**：
--
--   * `tenants.settings` 是一個開放的擴充點，會不斷長出新鍵，
--     每加一個就改一次 migration 沒有意義。
--   * `availability` 是一個封閉的形狀（七個星期鍵 + `blackout_dates`），
--     而它的打錯字**很危險**：`"blackout_date"`（少一個 s）在寬鬆的版本下
--     會被靜默忽略，於是那些停止服務日一天都沒有生效 —— 而畫面上看起來
--     設定好了。這正是本專案反覆出現的那類缺陷，所以寧可在寫入時就擋。
--
-- 自我驗證第一版寫成「未知的鍵放行」，然後被自己的檢查抓到：
-- `operating_hours_are_valid` 本來就只認星期鍵。那個矛盾逼我想清楚
-- 這兩個欄位為什麼該有不同的答案。
CREATE OR REPLACE FUNCTION fms.service_availability_is_valid(p jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE
    WHEN p IS NULL THEN true
    WHEN jsonb_typeof(p) <> 'object' THEN false
    -- 星期鍵：與 operating_hours 完全同形，所以驗證只有一份。
    -- 先把 blackout_dates 拿掉再交給它，否則那個鍵會被當成星期鍵而不合格。
    WHEN NOT fms.operating_hours_are_valid(p - 'blackout_dates') THEN false
    WHEN NOT (p ? 'blackout_dates') THEN true
    WHEN jsonb_typeof(p -> 'blackout_dates') <> 'array' THEN false
    ELSE coalesce((
      SELECT bool_and(
               jsonb_typeof(d) = 'string'
               -- `to_date` 對 '2026-13-45' 會靜默地算出一個奇怪的日期，
               -- 所以先用 regexp 擋形狀，再讓轉型擋真正不存在的日期。
               AND (d #>> '{}') ~ '^\d{4}-\d{2}-\d{2}$'
               AND fms.is_a_date(d #>> '{}'))
        FROM jsonb_array_elements(p -> 'blackout_dates') d
    ), true)
  END;
$$;

COMMENT ON FUNCTION fms.service_availability_is_valid(jsonb) IS
  'service_items.availability 的形狀：星期鍵沿用 038 的驗證，'
  '外加 blackout_dates（日期字串陣列，形狀與真實性都驗）。未知的鍵放行。';

ALTER TABLE fms.service_items DROP CONSTRAINT IF EXISTS ck_service_items_availability;
ALTER TABLE fms.service_items
  ADD CONSTRAINT ck_service_items_availability
  CHECK (fms.service_availability_is_valid(availability));

-- -----------------------------------------------------------------------------
-- (2) 某一天的可用時段
-- -----------------------------------------------------------------------------
-- 回傳 `{"windows": [...], "basis": "...", "is_blackout": bool}`。
--
-- **`basis` 不是裝飾。** 一個回空陣列的答案有三種原因（停止服務日、那個星期
-- 沒有時段、沒有場域可退），而它們對使用者的意義完全不同：第一個是「今天
-- 不提供」，第二個是「星期天不提供」，第三個是「這個服務沒設定，去問管理員」。
CREATE OR REPLACE FUNCTION fms.service_item_windows(
  p_service_item_id uuid,
  p_date            date
) RETURNS jsonb
LANGUAGE plpgsql
STABLE
AS $$
DECLARE
  v_avail       jsonb;
  v_facility_id uuid;
  v_key         text;
  v_own         jsonb;
BEGIN
  SELECT si.availability, si.facility_id
    INTO v_avail, v_facility_id
    FROM fms.service_items si
   WHERE si.id = p_service_item_id AND si.deleted_at IS NULL;

  IF NOT FOUND THEN
    RETURN NULL;   -- 呼叫端翻成 404
  END IF;

  -- (1) 停止服務日蓋過一切。
  IF v_avail ? 'blackout_dates'
     AND (v_avail -> 'blackout_dates') @> to_jsonb(p_date::text) THEN
    RETURN jsonb_build_object(
      'windows', '[]'::jsonb, 'basis', 'blackout_date', 'is_blackout', true);
  END IF;

  -- (2) 服務自己的星期表。
  v_key := (ARRAY['mon','tue','wed','thu','fri','sat','sun'])[
             extract(isodow FROM p_date)::int];
  v_own := v_avail -> v_key;
  IF v_own IS NOT NULL AND jsonb_array_length(v_own) > 0 THEN
    RETURN jsonb_build_object(
      'windows', v_own, 'basis', 'service_item.availability', 'is_blackout', false);
  END IF;

  -- (3) 退回場域。`facility_id` 為 NULL（全場域適用）時沒有場域可退。
  IF v_facility_id IS NULL THEN
    RETURN jsonb_build_object(
      'windows', '[]'::jsonb, 'basis', 'no_facility_to_fall_back_to',
      'is_blackout', false);
  END IF;

  RETURN jsonb_build_object(
    'windows', fms.business_windows(v_facility_id, p_date),
    'basis', 'facility.operating_hours', 'is_blackout', false);
END;
$$;

COMMENT ON FUNCTION fms.service_item_windows(uuid, date) IS
  '某個服務項目在某一天的可用時段。解析順序：blackout_dates > 服務自己的'
  ' 星期表 > 場域的 business_windows()。回傳的 basis 讓「空陣列」的三種原因'
  ' 分得開（今天不提供／這個星期不提供／沒設定）。NULL = 找不到那個服務項目。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 形狀驗證可以在 CORE 跑（純函式）；`service_item_windows` 需要資料，
-- 因此它的行為驗證在 `service_catalogue_slice.rs`。
DO $$
BEGIN
  -- (1) 星期鍵沿用 038 的驗證。
  IF fms.service_availability_is_valid('{"mon": [["9:0","18:00"]]}'::jsonb) THEN
    RAISE EXCEPTION '068 FAILED: 壞掉的時段字串被放行了（038 的驗證沒有生效）';
  END IF;
  -- 結束早於開始也該擋（038 的規則）。
  IF fms.service_availability_is_valid('{"mon": [["18:00","09:00"]]}'::jsonb) THEN
    RAISE EXCEPTION '068 FAILED: 結束早於開始的時段被放行了';
  END IF;

  -- (2) blackout_dates 的形狀。
  IF fms.service_availability_is_valid('{"blackout_dates": "2026-01-01"}'::jsonb) THEN
    RAISE EXCEPTION '068 FAILED: blackout_dates 不是陣列卻被放行';
  END IF;
  IF fms.service_availability_is_valid('{"blackout_dates": ["2026/01/01"]}'::jsonb) THEN
    RAISE EXCEPTION '068 FAILED: 錯誤的日期格式被放行';
  END IF;
  -- **形狀對但不存在的日期。** 這一格是 `is_a_date` 存在的理由：
  -- 只用 regexp 擋的話 `2026-02-30` 會通過，然後在解析時變成 3 月 2 日。
  IF fms.service_availability_is_valid('{"blackout_dates": ["2026-02-30"]}'::jsonb) THEN
    RAISE EXCEPTION
      '068 FAILED: 2026-02-30 被放行 —— 形狀對但那一天不存在，'
      '而它會在解析時安靜地變成 3 月 2 日';
  END IF;
  -- 合法的組合。
  IF NOT fms.service_availability_is_valid(
       '{"mon": [["07:00","20:00"]], "blackout_dates": ["2026-01-01","2026-02-28"]}'::jsonb
     ) THEN
    RAISE EXCEPTION '068 FAILED: 合法的 availability 被擋了';
  END IF;
  -- 空物件（絕大多數既有列的值）必須通得過，否則這支 migration 加約束會失敗。
  IF NOT fms.service_availability_is_valid('{}'::jsonb) THEN
    RAISE EXCEPTION '068 FAILED: {} 該通過';
  END IF;
  -- **打錯字的鍵要被擋。** `blackout_date` 少一個 s 在寬鬆的版本下會被
  -- 靜默忽略，於是那些停止服務日一天都沒有生效，而畫面上看起來設定好了。
  IF fms.service_availability_is_valid('{"blackout_date": ["2026-01-01"]}'::jsonb) THEN
    RAISE EXCEPTION
      '068 FAILED: `blackout_date`（少一個 s）被放行 —— '
      '那會讓所有停止服務日靜默失效，而設定畫面上看起來是對的';
  END IF;
  IF fms.service_availability_is_valid('{"monday": [["07:00","20:00"]]}'::jsonb) THEN
    RAISE EXCEPTION '068 FAILED: `monday`（該是 mon）被放行';
  END IF;

  -- (3) 約束真的掛上去了。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_service_items_availability'
       AND conrelid = 'fms.service_items'::regclass
  ) THEN
    RAISE EXCEPTION '068 FAILED: service_items 沒有 ck_service_items_availability';
  END IF;

  -- (4) 解析函式回得出三種 basis。**基準必須具名** ——
  --     一個回空陣列的答案有三種原因，混成一個會讓使用者問錯問題。
  IF (SELECT prosrc FROM pg_proc
       WHERE proname = 'service_item_windows'
         AND pronamespace = 'fms'::regnamespace) NOT LIKE '%blackout_date%' THEN
    RAISE EXCEPTION '068 FAILED: service_item_windows 沒有回報 blackout 這個原因';
  END IF;

  RAISE NOTICE '068 OK：availability 有形狀約束（含 blackout_dates 的真實性）'
               '與第一個讀者（打錯字的鍵會被擋）、解析順序 blackout > 服務 > 場域'
               '（行為驗證在 service_catalogue_slice.rs）';
END;
$$;

COMMIT;
