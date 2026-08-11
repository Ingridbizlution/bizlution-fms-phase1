-- =============================================================================
-- Down migration 056：移除「由告警人工補建工單」
-- =============================================================================
-- 已經建出來的工單與 alarms.work_order_id 的關聯**不動** ——
-- 那些是業務資料，不是這個 migration 的產物。
-- 回退後 POST /alarms/{id}/work-order 會因為找不到函式而整支失敗
--（不是靜默放行：判定不見了就不該還能補建）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

DROP FUNCTION IF EXISTS fms.create_work_order_from_alarm(uuid, text, text, uuid, uuid, text);

DO $$
BEGIN
  IF to_regprocedure('fms.create_work_order_from_alarm(uuid,text,text,uuid,uuid,text)')
     IS NOT NULL THEN
    RAISE EXCEPTION 'down 056 FAILED: 函式仍然存在';
  END IF;
  RAISE NOTICE 'down 056 OK';
END;
$$;

COMMIT;
