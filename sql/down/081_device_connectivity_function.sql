-- 回退 081：移除 device_connectivity()。
BEGIN;
SET search_path = fms, public;

DROP FUNCTION IF EXISTS fms.device_connectivity(text, timestamptz, int);

COMMIT;
