-- =============================================================================
-- 081  device_connectivity()：把「連線狀態怎麼算」收斂成一份定義
-- =============================================================================
-- 這條判定式在 `fms-asset/src/devices.rs` 已經以字串常數 `CONNECTIVITY` 的
-- 形式存在，而且**同一支檔案裡已經有兩份手抄本**：一份在 `SELECT` 子句
-- （算出 `connectivity` 欄位），一份在 `list()` 的 `WHERE` 子句（`offline_only`
-- 篩選，用等價但寫法不同的條件重複判斷同一件事）。
--
-- 現在要加第三個消費者（`floor-view` 要顯示樓層下設備的即時連線狀態），
-- 再抄一份就是三份。這正是 ADR-09「不要製造第二份真實來源」該套用的
-- 地方——只是這次真實來源本來就該是資料庫函式，而不是 Rust 字串常數，
-- 因為判定的基準是**資料庫的現在**（`now()`），應用伺服器與資料庫時鐘
-- 不同步時，算在 Rust 端的「幾秒前」會漂移，那個偏差只在部署環境才
-- 出現，本機測不到——這正是 `devices.rs` 原本把它寫在 SQL 而非 Rust
-- 的理由，現在只是把「寫在 SQL」升級成「寫在一個函式裡」。
--
-- STABLE 而非 IMMUTABLE：結果依賴 `now()`，同一個交易內結果穩定，
-- 但跨交易會變——與 `now()` 自己的易變性分類一致。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

CREATE FUNCTION fms.device_connectivity(
  p_status text,
  p_last_seen_at timestamptz,
  p_offline_alarm_after_seconds int
) RETURNS text
LANGUAGE sql STABLE PARALLEL SAFE
AS $$
  SELECT CASE
    -- 行政狀態優先：這兩個是「我們故意讓它不回報」，
    -- 被「它沒在回報」蓋掉就失去意義了。
    WHEN p_status IN ('DISABLED', 'MAINTENANCE') THEN p_status
    WHEN p_last_seen_at IS NULL THEN 'NEVER_SEEN'
    WHEN p_last_seen_at >= now() - (p_offline_alarm_after_seconds || ' seconds')::interval
      THEN 'ONLINE'
    ELSE 'OFFLINE'
  END
$$;

COMMENT ON FUNCTION fms.device_connectivity(text, timestamptz, int) IS
  '設備即時連線狀態：ONLINE／OFFLINE／NEVER_SEEN／DISABLED／MAINTENANCE。
   唯一的真實來源——devices.rs 與 floor-view 都呼叫這支，不各自算一份。';

DO $$
DECLARE
  v_disabled text;
  v_never    text;
  v_online   text;
  v_offline  text;
BEGIN
  v_disabled := fms.device_connectivity('DISABLED', now() - interval '1 hour', 60);
  v_never    := fms.device_connectivity('ACTIVE', NULL, 60);
  v_online   := fms.device_connectivity('ACTIVE', now(), 60);
  v_offline  := fms.device_connectivity('ACTIVE', now() - interval '1 hour', 60);

  IF v_disabled <> 'DISABLED' THEN
    RAISE EXCEPTION '081 FAILED: 行政狀態應該蓋過連線判斷，實際 %', v_disabled;
  END IF;
  IF v_never <> 'NEVER_SEEN' THEN
    RAISE EXCEPTION '081 FAILED: 從未回報應該是 NEVER_SEEN，實際 %', v_never;
  END IF;
  IF v_online <> 'ONLINE' THEN
    RAISE EXCEPTION '081 FAILED: 剛回報過應該是 ONLINE，實際 %', v_online;
  END IF;
  IF v_offline <> 'OFFLINE' THEN
    RAISE EXCEPTION '081 FAILED: 超過門檻沒回報應該是 OFFLINE，實際 %', v_offline;
  END IF;

  RAISE NOTICE '081 OK: 五種連線狀態的判定式行為正確';
END;
$$;

COMMIT;
