-- =============================================================================
-- Down migration 057：移除即時門檻評估
-- =============================================================================
-- 回退後 raise_alarm 又會回到「只有煙霧測試呼叫過」的狀態 ——
-- IoT 那條鏈在生產路徑上不再跑。已經產生的告警與工單不動。
-- POST /telemetry:batch-ingest 會因為找不到函式而整支失敗（不是靜默不評估）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

DROP FUNCTION IF EXISTS fms.evaluate_telemetry_rules(uuid, numeric, timestamptz);

DO $$
BEGIN
  IF to_regprocedure('fms.evaluate_telemetry_rules(uuid,numeric,timestamptz)') IS NOT NULL THEN
    RAISE EXCEPTION 'down 057 FAILED: 函式仍然存在';
  END IF;
  RAISE NOTICE 'down 057 OK';
END;
$$;

COMMIT;
