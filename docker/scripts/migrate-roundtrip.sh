#!/bin/sh
# =============================================================================
# 驗證 down migration 真的可逆：up → down → up
# =============================================================================
# 沒有被執行過的 down migration 等於沒有 down migration。這個腳本在一個
# 拋棄式資料庫上跑完整循環，並斷言最終狀態與第一次 up 之後相同。
#
# 比對的是**物件簽章**（表、欄位、約束、視圖定義、函式定義、政策）而不只是
# 數量：數量相同但內容不同是很容易發生的（例如函式定義被換掉），只比數字會漏掉。
#
# 欄位、約束、視圖定義都是後來補上的，而且每一次都是因為新的 migration 暴露
# 了盲點：025 動的是「一個欄位加一個主鍵」，026 動的是「一個既有視圖的 WHERE」
# —— 兩者在原本的快照下都完全隱形，也就是說它們的 down 無論寫得對不對，
# roundtrip 都會通過。一個對特定變更類型視而不見的驗證比沒有更危險。
# =============================================================================
set -eu

HOST="${PGHOST:-postgres}"
PORT="${PGPORT:-5432}"
SUPER="${POSTGRES_SUPERUSER:-postgres}"
DB="fms_roundtrip"

supsql() {
  PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
    psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U "$SUPER" -d postgres "$@"
}
owner() {
  psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U fms_owner -d "$DB" "$@"
}

echo "==> roundtrip：建立拋棄式資料庫 $DB"
supsql -q -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
supsql -q -c "CREATE DATABASE $DB OWNER fms_owner"

snapshot() {
  owner -tAq <<'SQL'
SELECT string_agg(sig, E'\n' ORDER BY sig) FROM (
  SELECT 'T:' || c.relname AS sig
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
   WHERE n.nspname = 'fms' AND c.relkind IN ('r','p','v')
  UNION ALL
  -- 欄位：名稱、型別、可空性。DROP COLUMN 留下的 attisdropped 要排除，
  -- 否則同一個名字加回來時簽章會不同而誤判。
  SELECT 'C:' || c.relname || '.' || a.attname || ':'
         || format_type(a.atttypid, a.atttypmod) || ':' || a.attnotnull
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_attribute a ON a.attrelid = c.oid
   WHERE n.nspname = 'fms' AND c.relkind IN ('r','p')
     AND a.attnum > 0 AND NOT a.attisdropped
  UNION ALL
  -- 約束：主鍵、唯一鍵、外鍵、CHECK 的定義全文
  SELECT 'K:' || conrelid::regclass::text || ':' || conname || ':'
         || pg_get_constraintdef(oid)
    FROM pg_constraint WHERE connamespace = 'fms'::regnamespace
  UNION ALL
  -- 視圖定義。只記名字是不夠的：026 改的就是一個既有視圖的 WHERE，
  -- 名字完全沒變，因此少了這一項，026 的 down 寫錯也不會被抓到。
  SELECT 'V:' || c.relname || ':' || md5(pg_get_viewdef(c.oid))
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
   WHERE n.nspname = 'fms' AND c.relkind = 'v'
  UNION ALL
  SELECT 'F:' || p.proname || ':' || md5(pg_get_functiondef(p.oid))
    FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
   WHERE n.nspname = 'fms'
  UNION ALL
  SELECT 'P:' || tablename || ':' || policyname || ':' || coalesce(with_check,'-')
    FROM pg_policies WHERE schemaname = 'fms'
) s;
SQL
}

echo "==> 第一次 up"
POSTGRES_DB="$DB" MIGRATE_MODE=all sh /scripts/migrate.sh >/dev/null
snapshot > /tmp/rt_first.txt

echo "==> down 至 013"
POSTGRES_DB="$DB" TO=013 sh /scripts/migrate-down.sh >/dev/null

echo "==> 第二次 up（只重跑 014 以後）"
for f in /sql/014_*.sql /sql/015_*.sql /sql/016_*.sql /sql/017_*.sql \
         /sql/018_*.sql /sql/019_*.sql /sql/020_*.sql /sql/021_*.sql /sql/022_*.sql \
         /sql/023_*.sql /sql/024_*.sql /sql/025_*.sql \
         /sql/026_*.sql /sql/027_*.sql /sql/028_*.sql /sql/029_*.sql /sql/030_*.sql /sql/031_*.sql /sql/032_*.sql /sql/033_*.sql /sql/034_*.sql /sql/035_*.sql /sql/036_*.sql /sql/037_*.sql /sql/038_*.sql /sql/039_*.sql /sql/040_*.sql /sql/041_*.sql /sql/042_*.sql /sql/043_*.sql /sql/044_*.sql /sql/045_*.sql /sql/046_*.sql /sql/047_*.sql /sql/048_*.sql /sql/049_*.sql /sql/050_*.sql /sql/051_*.sql /sql/052_*.sql /sql/053_*.sql /sql/054_*.sql /sql/055_*.sql /sql/056_*.sql /sql/057_*.sql /sql/058_*.sql /sql/059_*.sql /sql/060_*.sql /sql/061_*.sql /sql/062_*.sql /sql/063_*.sql /sql/064_*.sql /sql/065_*.sql /sql/066_*.sql /sql/067_*.sql /sql/068_*.sql /sql/069_*.sql /sql/070_*.sql /sql/071_*.sql /sql/072_*.sql /sql/073_*.sql /sql/074_*.sql /sql/077_*.sql /sql/078_*.sql /sql/081_*.sql /sql/083_*.sql /sql/084_*.sql /sql/085_*.sql /sql/086_*.sql; do
  psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U fms_owner -d "$DB" -q -f "$f" >/dev/null
done
snapshot > /tmp/rt_second.txt

if diff -u /tmp/rt_first.txt /tmp/rt_second.txt > /tmp/rt_diff.txt; then
  echo "==> roundtrip 通過：up → down → up 之後 schema 完全相同"
else
  echo "==> roundtrip 失敗：down migration 沒有完整還原" >&2
  head -40 /tmp/rt_diff.txt >&2
  supsql -q -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
  exit 1
fi

supsql -q -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
