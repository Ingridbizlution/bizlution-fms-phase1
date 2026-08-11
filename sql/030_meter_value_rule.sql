-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 030: 計量讀數的推進規則收斂成一份
-- =============================================================================
-- WBS 4.1h(a) 記載的已知落差：
--
--   > schema 宣告三種 reading_type（CUMULATIVE／GAUGE／DELTA），但唯一的既有
--   > 寫入路徑 fms.ingest_telemetry（006）一律 last_value = value。
--   > 對 DELTA 型讀表那是錯的 —— 會把增量寫成總量。
--   > **ingest_telemetry 未修** …… 若有 DELTA 型讀表接上 IoT 點位，資料會是錯的。
--
-- S4c 的人工登錄端點（POST /assets/{id}/meters/{code}/readings）後來把規則
-- 實作對了，但實作在 **Rust**（`next_last_value`）。於是同一條規則有兩份：
-- 應用層那份是對的，資料庫這份是錯的。
--
-- 這比「有一個 bug」更糟：同一支讀表，人工登錄與 IoT 上報會得到不同的
-- `last_value`，而 PM 的門檻觸發是讀 `last_value` 的 —— 也就是說保養會不會
-- 被觸發，取決於讀數是誰送進來的。
--
-- 因此本 migration 不只是「修 ingest_telemetry」，而是把規則收斂成
-- **一支 SQL 函式**，兩條路徑都呼叫它。與 016 把 scope 述詞收斂成一份、
-- 029 讓稽核只有一個寫入者是同一個判斷。
--
-- -----------------------------------------------------------------------------
-- 錯誤以「訊息含穩定標記」表達，而不是靠 SQLSTATE
-- -----------------------------------------------------------------------------
-- 應用層的 `Problem::from(sqlx::Error)` 已經在用這個模式
-- （consume_quota 的 QUOTA_EXCEEDED、狀態機的 illegal transition）：
-- schema 用訊息內容表達語意，應用層忠實轉譯成 HTTP 語意。
-- 這裡沿用同一個模式，標記是 `METER_VALUE_INVALID`。
--
-- 依賴：003（asset_meters）、006（ingest_telemetry）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 唯一的推進規則
-- -----------------------------------------------------------------------------
-- 回傳「這筆讀數之後，last_value 應該是多少」。不寫入 —— 呼叫端決定要不要寫、
-- 以及在同一個交易裡還要做什麼。
CREATE OR REPLACE FUNCTION fms.next_meter_value(
  p_asset_meter_id uuid,
  p_value          numeric
) RETURNS numeric
LANGUAGE plpgsql STABLE
AS $$
DECLARE
  v_meter fms.asset_meters;
  v_prev  numeric;
BEGIN
  SELECT * INTO v_meter FROM fms.asset_meters WHERE id = p_asset_meter_id;
  IF v_meter.id IS NULL THEN
    RAISE EXCEPTION 'asset meter % not found', p_asset_meter_id USING ERRCODE = 'P0002';
  END IF;

  v_prev := coalesce(v_meter.last_value, 0);

  CASE v_meter.reading_type
    WHEN 'DELTA' THEN
      -- 增量：累加。負增量沒有意義 —— 讀表不會倒退著計。
      IF p_value < 0 THEN
        RAISE EXCEPTION
          'METER_VALUE_INVALID: value must not be negative for a DELTA meter (%)', p_value
          USING ERRCODE = '23514';
      END IF;
      RETURN v_prev + p_value;

    WHEN 'CUMULATIVE' THEN
      -- 累計：取代，但不得倒退。
      IF p_value >= v_prev THEN
        RETURN p_value;
      END IF;
      -- 會歸零的計數器（四位數電表）：繞回一圈之後的實際增量是
      -- (上限 - 前值) + 新值。WBS 4.1h(b) 記載 rollover_at 全系統未使用；
      -- 人工登錄路徑已實作，這裡是把同一個處理帶進 IoT 路徑。
      IF v_meter.rollover_at IS NOT NULL AND v_meter.rollover_at > v_prev THEN
        RETURN v_prev + (v_meter.rollover_at - v_prev) + p_value;
      END IF;
      RAISE EXCEPTION
        'METER_VALUE_INVALID: a CUMULATIVE meter cannot go backwards (% → %); '
        'set rollover_at on the meter if it wraps', v_prev, p_value
        USING ERRCODE = '23514';

    ELSE
      -- GAUGE（與任何日後新增的型別）：瞬時值，直接取代。
      RETURN p_value;
  END CASE;
END;
$$;

COMMENT ON FUNCTION fms.next_meter_value(uuid, numeric) IS
  '依 asset_meters.reading_type 算出讀數推進後的 last_value。'
  ' DELTA 累加、CUMULATIVE 取代並檢查倒退（含 rollover_at）、GAUGE 取代。'
  ' 這是該規則的唯一實作 —— IoT（ingest_telemetry）與人工登錄端點都呼叫它。';

-- -----------------------------------------------------------------------------
-- (2) ingest_telemetry 改用它
-- -----------------------------------------------------------------------------
-- 除了 asset_meters 那一段之外，其餘與 006 的定義相同。
CREATE OR REPLACE FUNCTION fms.ingest_telemetry(
  p_telemetry_point_id uuid,
  p_observed_at        timestamptz,
  p_value_num          numeric DEFAULT NULL,
  p_value_bool         boolean DEFAULT NULL,
  p_value_text         text DEFAULT NULL,
  p_quality            text DEFAULT 'GOOD'
) RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
  v_point fms.telemetry_points;
  v_next  numeric;
BEGIN
  SELECT * INTO v_point FROM fms.telemetry_points WHERE id = p_telemetry_point_id;
  IF v_point.id IS NULL THEN
    RAISE EXCEPTION 'telemetry point % not found', p_telemetry_point_id USING ERRCODE = 'P0002';
  END IF;

  INSERT INTO fms.telemetry_readings
    (tenant_id, telemetry_point_id, observed_at, value_num, value_bool, value_text, quality)
  VALUES
    (v_point.tenant_id, p_telemetry_point_id, p_observed_at,
     p_value_num, p_value_bool, p_value_text, p_quality);

  INSERT INTO fms.telemetry_latest
    (telemetry_point_id, tenant_id, device_id, observed_at, value_num, value_bool, value_text, quality)
  VALUES
    (p_telemetry_point_id, v_point.tenant_id, v_point.device_id, p_observed_at,
     p_value_num, p_value_bool, p_value_text, p_quality)
  ON CONFLICT (telemetry_point_id) DO UPDATE
    SET observed_at = EXCLUDED.observed_at,
        value_num   = EXCLUDED.value_num,
        value_bool  = EXCLUDED.value_bool,
        value_text  = EXCLUDED.value_text,
        quality     = EXCLUDED.quality,
        updated_at  = clock_timestamp()
    WHERE fms.telemetry_latest.observed_at <= EXCLUDED.observed_at;

  UPDATE fms.devices
     SET last_seen_at = greatest(coalesce(last_seen_at, p_observed_at), p_observed_at),
         status = CASE WHEN status IN ('OFFLINE','UNKNOWN') THEN 'ONLINE' ELSE status END
   WHERE id = v_point.device_id;

  -- Keep the asset meter aligned so meter-triggered PM works off live data.
  IF v_point.asset_meter_id IS NOT NULL AND p_value_num IS NOT NULL THEN
    -- 030：改為依 reading_type 推進。先前這裡一律 `last_value = p_value_num`，
    -- 對 DELTA 型讀表是把增量寫成總量 —— 而 PM 的門檻觸發讀的正是 last_value。
    v_next := fms.next_meter_value(v_point.asset_meter_id, p_value_num);

    UPDATE fms.asset_meters
       SET last_value = v_next, last_read_at = p_observed_at
     WHERE id = v_point.asset_meter_id
       AND (last_read_at IS NULL OR last_read_at <= p_observed_at);

    -- 讀數列存的是**原始上報值**（DELTA 就是增量），不是推進後的總量：
    -- 那是這筆觀測的事實，改寫它等於偽造原始資料。
    INSERT INTO fms.asset_meter_readings
      (tenant_id, asset_meter_id, reading_at, value, source)
    VALUES (v_point.tenant_id, v_point.asset_meter_id, p_observed_at, p_value_num, 'IOT');
  END IF;
END;
$$;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 三種 reading_type 各驗一次，外加 rollover 與負增量。
-- 用臨時建立的資產與讀表，驗完刪掉 —— 不依賴示範資料（本檔在 CORE 裡執行，
-- 位置早於 009）。
DO $$
DECLARE
  v_tenant   uuid;
  v_facility uuid;
  v_asset    uuid;
  v_meter    uuid;
  v_got      numeric;
BEGIN
  PERFORM set_config('app.is_platform', 'on', true);

  -- 借用任何一個既有租戶與場域；008／011 已經種了平台資料但不一定有租戶，
  -- 因此沒有就跳過行為驗證，只斷言函式存在。
  SELECT id INTO v_tenant FROM fms.tenants LIMIT 1;
  IF v_tenant IS NULL THEN
    IF to_regprocedure('fms.next_meter_value(uuid,numeric)') IS NULL THEN
      RAISE EXCEPTION '030 FAILED: next_meter_value 未建立';
    END IF;
    RAISE NOTICE '030 OK（尚無租戶，僅驗證函式存在；行為由整合測試覆蓋）';
    RETURN;
  END IF;

  SELECT id INTO v_facility FROM fms.facilities WHERE tenant_id = v_tenant LIMIT 1;
  INSERT INTO fms.assets (tenant_id, facility_id, category_id, asset_code, name)
  SELECT v_tenant, v_facility, c.id, 'M030-SELFTEST', '030 自我測試'
  FROM fms.asset_categories c LIMIT 1
  RETURNING id INTO v_asset;

  -- DELTA：累加
  INSERT INTO fms.asset_meters (tenant_id, asset_id, meter_code, name, unit, reading_type, last_value)
  VALUES (v_tenant, v_asset, 'M030D', 'delta', 'kWh', 'DELTA', 100)
  RETURNING id INTO v_meter;
  v_got := fms.next_meter_value(v_meter, 25);
  IF v_got <> 125 THEN
    RAISE EXCEPTION '030 FAILED: DELTA 應累加成 125，實際 %', v_got;
  END IF;
  BEGIN
    PERFORM fms.next_meter_value(v_meter, -1);
    RAISE EXCEPTION '030 FAILED: DELTA 竟接受負值';
  EXCEPTION WHEN check_violation THEN NULL;
  END;

  -- CUMULATIVE：取代、且不得倒退
  UPDATE fms.asset_meters SET reading_type = 'CUMULATIVE', last_value = 100 WHERE id = v_meter;
  v_got := fms.next_meter_value(v_meter, 130);
  IF v_got <> 130 THEN
    RAISE EXCEPTION '030 FAILED: CUMULATIVE 應取代成 130，實際 %', v_got;
  END IF;
  BEGIN
    PERFORM fms.next_meter_value(v_meter, 90);
    RAISE EXCEPTION '030 FAILED: CUMULATIVE 竟接受倒退';
  EXCEPTION WHEN check_violation THEN NULL;
  END;

  -- CUMULATIVE + rollover：9999 的電表從 100 繞回到 5 → 100 + (9999-100) + 5
  UPDATE fms.asset_meters SET rollover_at = 9999 WHERE id = v_meter;
  v_got := fms.next_meter_value(v_meter, 5);
  IF v_got <> 10004 THEN
    RAISE EXCEPTION '030 FAILED: 繞回應算成 10004，實際 %', v_got;
  END IF;

  -- GAUGE：直接取代，倒退也允許（瞬時值本來就會上下）
  UPDATE fms.asset_meters SET reading_type = 'GAUGE', rollover_at = NULL WHERE id = v_meter;
  v_got := fms.next_meter_value(v_meter, 7);
  IF v_got <> 7 THEN
    RAISE EXCEPTION '030 FAILED: GAUGE 應取代成 7，實際 %', v_got;
  END IF;

  DELETE FROM fms.asset_meters WHERE id = v_meter;
  DELETE FROM fms.assets WHERE id = v_asset;
  PERFORM set_config('app.is_platform', 'off', true);

  RAISE NOTICE '030 OK: DELTA 累加、CUMULATIVE 取代與繞回、GAUGE 取代皆正確；ingest_telemetry 已改用同一支規則';
END;
$$;

COMMIT;
