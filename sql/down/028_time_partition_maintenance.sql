-- 回退 028。
--
-- **只移除函式，不移除已建立的分區。** 分區裡可能已經有資料，而
-- 「回退一個 migration」不該包含「刪掉它期間寫進去的列」——
-- 這與 024 的 down 不刪 auth_events 是同一個原則。
--
-- 後果是回退後 schema 會比 001 多幾張月分區。那是刻意的：它們是**資料容器**
-- 而不是本 migration 的結構產物，而 roundtrip 比對的是簽章，
-- 因此下面同時把它們刪掉——僅限**空的**那些。
BEGIN;
SET search_path = fms, public;

-- 只刪空分區，且只刪 001 沒有建立的那些。有資料的留下並在 NOTICE 說明，
-- 讓 roundtrip 的差異是可解釋的，而不是靜默地刪掉別人的資料。
DO $$
DECLARE
  r record;
  v_rows bigint;
  v_kept int := 0;
BEGIN
  FOR r IN
    SELECT c.relname AS part, p.relname AS parent
    FROM pg_class c
    JOIN pg_inherits i ON i.inhrelid = c.oid
    JOIN pg_class p ON p.oid = i.inhparent
    JOIN pg_namespace n ON n.oid = p.relnamespace
    WHERE n.nspname = 'fms'
      AND p.relname IN ('audit_log', 'telemetry_readings')
      -- 001 建的是 2026m07／2026m08 與 default，那三個不動
      AND c.relname NOT LIKE '%\_2026m07'
      AND c.relname NOT LIKE '%\_2026m08'
      AND c.relname NOT LIKE '%\_default'
  LOOP
    EXECUTE format('SELECT count(*) FROM fms.%I', r.part) INTO v_rows;
    IF v_rows = 0 THEN
      EXECUTE format('DROP TABLE fms.%I', r.part);
    ELSE
      v_kept := v_kept + 1;
      RAISE NOTICE '保留 %（有 % 列資料，回退不刪資料）', r.part, v_rows;
    END IF;
  END LOOP;
  IF v_kept > 0 THEN
    RAISE NOTICE 'down 028：保留了 % 個非空分區，roundtrip 的簽章差異來自它們', v_kept;
  END IF;
END;
$$;

DROP FUNCTION IF EXISTS fms.ensure_time_partitions(int);
DROP FUNCTION IF EXISTS fms.partition_boundary_timezone();

-- 還原 001 的 ensure_monthly_partitions，逐字複製 001 的定義。
-- roundtrip 比對函式定義的 md5，因此這裡若有任何差異都會被抓到 ——
-- 那也是為什麼要逐字抄而不是「寫一個等價的」。
CREATE OR REPLACE FUNCTION fms.ensure_monthly_partitions(
  p_parent regclass,
  p_months integer DEFAULT 3
) RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
  v_base  date := date_trunc('month', clock_timestamp())::date;
  v_start date;
  v_end   date;
  v_name  text;
  i integer;
BEGIN
  FOR i IN 0..p_months LOOP
    v_start := (v_base + (i || ' month')::interval)::date;
    v_end   := (v_start + interval '1 month')::date;
    v_name  := replace(p_parent::text, 'fms.', '') || '_' || to_char(v_start, 'YYYY"m"MM');
    IF NOT EXISTS (
      SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = 'fms' AND c.relname = v_name
    ) THEN
      EXECUTE format('CREATE TABLE fms.%I PARTITION OF %s FOR VALUES FROM (%L) TO (%L)',
                     v_name, p_parent, v_start, v_end);
    END IF;
  END LOOP;
END;
$$;

COMMIT;
