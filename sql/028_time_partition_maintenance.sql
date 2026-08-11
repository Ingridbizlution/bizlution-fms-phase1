-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 028: 時間分區的預先建立
-- =============================================================================
-- 001 為 fms.audit_log 與 fms.telemetry_readings 建了 2026 年 7、8 月的月分區
-- 各一個，加上一個 DEFAULT 分區。**沒有任何機制會建立後續月份。**
--
-- 症狀不是失敗而是無聲降級：DEFAULT 分區存在，所以 9 月起的列照樣寫得進去，
-- 只是全部落在同一張表裡。分區的用意（保留期到了直接 DROP 一整個月、
-- 查詢只掃相關月份）就此失效，而且沒有任何錯誤訊息。
--
-- 更糟的是它會**自我鎖死**：一旦某個月的列進了 DEFAULT，之後就再也不能為
-- 那個月建立分區 —— PostgreSQL 會拒絕（新分區的約束會被 DEFAULT 裡的既有列
-- 違反）。修復需要先把那些列搬出 DEFAULT，而那要 ACCESS EXCLUSIVE 鎖。
-- 也就是說這件事**越晚處理越貴**。
--
-- -----------------------------------------------------------------------------
-- 刻意不做的事：不刪除舊分區
-- -----------------------------------------------------------------------------
-- 保留多久是政策決定（稽核法規、遙測成本），不是這支函式該替人決定的。
-- 它只保證「未來的月份有地方放」。刪除舊分區留給明確的保留政策，
-- 到時候那會是一行 `DROP TABLE fms.telemetry_readings_2026m07`
-- —— 而那正是分區換來的東西。
--
-- -----------------------------------------------------------------------------
-- 為什麼用探索而不是寫死表名
-- -----------------------------------------------------------------------------
-- 目前只有兩張分區表，寫死兩個名字更短。但這個 schema 這一輪已經出現過三次
-- 「宣告了沒人讀」的缺陷（min_scope_level、required_permission、
-- roles.scope_level），而寫死清單是同一個形狀的問題：日後有人加了第三張
-- 分區表，這支函式會**安靜地漏掉它**，症狀又是無聲降級。
-- 因此改為從 pg_partitioned_table 找出所有「以單一 timestamptz 欄位做 RANGE
-- 分區」的表。
--
-- -----------------------------------------------------------------------------
-- 001 已經有一支 `ensure_monthly_partitions`，而它沒有任何呼叫者
-- -----------------------------------------------------------------------------
-- 這是本 schema 第五次出現同一個形狀（前四次：min_scope_level、
-- required_permission、roles.scope_level、以及「一個排程作業會把分區往前推」
-- 這句寫在 001 註解裡卻沒人實作的話）。
--
-- 兩支函式做同一件事，因此**不能並存** —— 那正是第二份真實來源。
-- 本 migration 移除 001 的那一支，理由不只是「我的比較新」：
--
--   * **邊界用 `date` 字面值**（`FOR VALUES FROM ('2026-09-01')`），
--     實際的 timestamptz 取決於 session 的 `TimeZone`。在容器裡剛好是
--     Asia/Taipei 所以看起來對；換一條時區不同的連線呼叫，就會與既有分區
--     產生一小時的縫，而縫裡的列會掉進 DEFAULT。這正是本檔用
--     `partition_boundary_timezone()` 明確固定的原因。
--   * **一次只處理一張表**（參數是 `regclass`），因此呼叫端必須自己維護
--     一份表清單 —— 而那份清單會漂移（實測：我一開始也以為只有兩張，
--     漏掉了 asset_meter_readings）。
--   * **回傳 void**，呼叫端無法分辨「建了三個」與「什麼都沒建」。
--   * DEFAULT 已有該月列時只會拋出原始錯誤，看不出要怎麼修。
--
-- 移除未被呼叫的函式是安全的；roundtrip 的函式簽章比對會確認 down 有還原它。
--
-- 依賴：001（分區表與被本檔移除的舊函式）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 月分區的邊界時區
-- -----------------------------------------------------------------------------
-- 001 建的邊界是 '2026-07-01 00:00:00+08'，也就是 Asia/Taipei 的月初。
-- 新分區必須用**同一個**時區算邊界，否則會與既有分區重疊（PostgreSQL 會拒絕，
-- 算是安全的失敗）或留下一小時的縫（那才可怕：縫裡的列會掉進 DEFAULT）。
--
-- 因此這裡明確寫死，而不是靠 session 的 TimeZone —— 後者會讓同一支函式
-- 在不同連線下算出不同邊界。要改這個值等於要重新分區，不是改一個參數。
CREATE OR REPLACE FUNCTION fms.partition_boundary_timezone()
RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $$ SELECT 'Asia/Taipei'::text $$;

COMMENT ON FUNCTION fms.partition_boundary_timezone() IS
  '月分區邊界所用的時區。必須與 001 建立的既有邊界（+08）一致 —— 改它等於要重新分區。';

-- -----------------------------------------------------------------------------
-- 預先建立未來月份的分區
-- -----------------------------------------------------------------------------
-- 回傳做了什麼，讓呼叫端（背景作業）能記進 log。「什麼都沒做」與
-- 「建立了三個」在維運上是不同的訊息，靜默成功會讓人無法確認它真的在跑。
CREATE OR REPLACE FUNCTION fms.ensure_time_partitions(p_months_ahead int DEFAULT 3)
RETURNS TABLE (parent_table text, partition_name text, action text)
LANGUAGE plpgsql
AS $$
DECLARE
  r_parent   record;
  v_tz       text := fms.partition_boundary_timezone();
  v_month    int;
  v_from     timestamptz;
  v_to       timestamptz;
  v_name     text;
BEGIN
  IF p_months_ahead < 0 OR p_months_ahead > 60 THEN
    RAISE EXCEPTION 'p_months_ahead 應在 0..60 之間，收到 %', p_months_ahead;
  END IF;

  FOR r_parent IN
    -- 只認「單一欄位、RANGE 分區、鍵型別是 timestamptz」的表。
    -- 多欄位或其他型別的分區策略不在本函式的語意內，跳過比猜錯好。
    SELECT c.oid, c.relname::text AS relname
    FROM pg_partitioned_table pt
    JOIN pg_class c ON c.oid = pt.partrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'fms'
      AND pt.partstrat = 'r'
      AND pt.partnatts = 1
      AND (SELECT a.atttypid FROM pg_attribute a
            WHERE a.attrelid = c.oid AND a.attnum = pt.partattrs[0]) = 'timestamptz'::regtype
    ORDER BY c.relname
  LOOP
    FOR v_month IN 0..p_months_ahead LOOP
      -- 在固定時區算月初，再轉回 timestamptz —— 與 001 的邊界對齊。
      v_from := (date_trunc('month', (clock_timestamp() AT TIME ZONE v_tz))
                 + (v_month || ' months')::interval) AT TIME ZONE v_tz;
      v_to   := (date_trunc('month', (clock_timestamp() AT TIME ZONE v_tz))
                 + ((v_month + 1) || ' months')::interval) AT TIME ZONE v_tz;
      v_name := r_parent.relname || '_' || to_char(v_from AT TIME ZONE v_tz, 'YYYY"m"MM');

      IF EXISTS (
        SELECT 1 FROM pg_class pc JOIN pg_namespace pn ON pn.oid = pc.relnamespace
         WHERE pn.nspname = 'fms' AND pc.relname = v_name
      ) THEN
        RETURN QUERY SELECT r_parent.relname::text, v_name, 'exists'::text;
        CONTINUE;
      END IF;

      BEGIN
        EXECUTE format(
          'CREATE TABLE fms.%I PARTITION OF fms.%I FOR VALUES FROM (%L) TO (%L)',
          v_name, r_parent.relname, v_from, v_to
        );
        RETURN QUERY SELECT r_parent.relname::text, v_name, 'created'::text;
      EXCEPTION WHEN check_violation OR invalid_table_definition THEN
        -- 幾乎必然是「DEFAULT 分區裡已經有屬於這個月的列」。
        -- 訊息要可行動：這個狀態只能靠把列搬出 DEFAULT 來解，
        -- 而那需要停機窗（ACCESS EXCLUSIVE）。含糊的錯誤會讓人先去猜。
        RAISE EXCEPTION
          '無法為 % 建立分區 %：DEFAULT 分區裡已有屬於該區間的列。'
          '需先把它們搬出 DEFAULT（需 ACCESS EXCLUSIVE 鎖）。原始錯誤：%',
          r_parent.relname, v_name, SQLERRM;
      END;
    END LOOP;
  END LOOP;
END;
$$;

COMMENT ON FUNCTION fms.ensure_time_partitions(int) IS
  '為所有以 timestamptz 做 RANGE 分區的 fms 表預建未來 N 個月的月分區（含當月）。'
  ' 幂等；回傳每個分區是 created 還是 exists。刻意不刪除舊分區 —— 保留期是政策決定。';

REVOKE ALL ON FUNCTION fms.ensure_time_partitions(int) FROM PUBLIC;
-- 只有 owner（背景作業的連線身分）能建表；fms_app 不該有 DDL 能力。
GRANT EXECUTE ON FUNCTION fms.ensure_time_partitions(int) TO fms_owner;

-- 移除 001 的重複實作（理由見檔頭）。它從未被呼叫過。
DROP FUNCTION IF EXISTS fms.ensure_monthly_partitions(regclass, integer);

-- -----------------------------------------------------------------------------
-- 立刻補上缺的月份
-- -----------------------------------------------------------------------------
-- migration 本身就跑一次：否則要等背景作業第一次醒來，而在那之前
-- 這個 migration 只是「加了一支沒人叫的函式」。
DO $$
DECLARE v_created int;
BEGIN
  SELECT count(*) INTO v_created
  FROM fms.ensure_time_partitions(3) WHERE action = 'created';
  RAISE NOTICE '028：預建了 % 個月分區', v_created;
END;
$$;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 斷言的是**沒有縫**：把每張分區表的所有非 DEFAULT 分區依下界排序，
-- 前一個的上界必須等於後一個的下界。縫是這件事最危險的失敗方式 ——
-- 縫裡的列會掉進 DEFAULT，而那正是本 migration 要消除的狀態。
DO $$
DECLARE
  r record;
  v_prev_to text;
  v_parent  text := '';
BEGIN
  FOR r IN
    SELECT p.relname AS parent, c.relname AS part,
           split_part(split_part(pg_get_expr(c.relpartbound, c.oid), '''', 2), '''', 1) AS lo,
           split_part(split_part(pg_get_expr(c.relpartbound, c.oid), '''', 4), '''', 1) AS hi
    FROM pg_class c
    JOIN pg_inherits i ON i.inhrelid = c.oid
    JOIN pg_class p ON p.oid = i.inhparent
    JOIN pg_namespace n ON n.oid = p.relnamespace
    WHERE n.nspname = 'fms'
      AND pg_get_expr(c.relpartbound, c.oid) NOT LIKE 'DEFAULT%'
    ORDER BY p.relname, c.relname
  LOOP
    IF r.parent <> v_parent THEN
      v_parent := r.parent;
      v_prev_to := NULL;
    END IF;
    IF v_prev_to IS NOT NULL AND r.lo <> v_prev_to THEN
      RAISE EXCEPTION '028 FAILED: % 的分區之間有縫：上一個到 %，下一個從 % 開始',
        r.parent, v_prev_to, r.lo;
    END IF;
    v_prev_to := r.hi;
  END LOOP;

  -- 至少要涵蓋到「當月 + 3」的月初，否則本 migration 沒有達成目的。
  IF EXISTS (
    SELECT 1
    FROM pg_partitioned_table pt
    JOIN pg_class c ON c.oid = pt.partrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'fms' AND pt.partstrat = 'r' AND pt.partnatts = 1
      AND NOT EXISTS (
        SELECT 1 FROM pg_class pc JOIN pg_namespace pn ON pn.oid = pc.relnamespace
         WHERE pn.nspname = 'fms'
           AND pc.relname = c.relname || '_' || to_char(
                 (date_trunc('month', (clock_timestamp() AT TIME ZONE fms.partition_boundary_timezone()))
                  + interval '3 months'), 'YYYY"m"MM')
      )
  ) THEN
    RAISE EXCEPTION '028 FAILED: 有分區表沒有涵蓋到當月 +3';
  END IF;

  RAISE NOTICE '028 OK: 分區連續無縫，且已涵蓋當月 +3';
END;
$$;

COMMIT;
