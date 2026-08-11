-- 回退 030。**這會讓 DELTA 型讀表的 IoT 上報重新把增量寫成總量。**
-- ingest_telemetry 還原成 006 的定義（逐字），並移除規則函式。
-- 人工登錄端點的 Rust 側會一起回退（見同一個 commit）。
BEGIN;
SET search_path = fms, public;

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
    UPDATE fms.asset_meters
       SET last_value = p_value_num, last_read_at = p_observed_at
     WHERE id = v_point.asset_meter_id
       AND (last_read_at IS NULL OR last_read_at <= p_observed_at);

    INSERT INTO fms.asset_meter_readings
      (tenant_id, asset_meter_id, reading_at, value, source)
    VALUES (v_point.tenant_id, v_point.asset_meter_id, p_observed_at, p_value_num, 'IOT');
  END IF;
END;
$$;

DROP FUNCTION IF EXISTS fms.next_meter_value(uuid, numeric);

COMMIT;
