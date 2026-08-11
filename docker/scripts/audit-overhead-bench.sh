#!/bin/sh
# =============================================================================
# 049 的稽核寫入放大有多大 —— A/B 量測
# =============================================================================
# 049 把稽核擴大到 `work_orders` 與 `assets`，而稽核存的是 `before_data` 與
# `after_data` **整列**（56／35 欄）。049 的檔頭說那是「一筆真實的寫入放大」，
# 但**沒有數字**。沒有數字就無法回答那份檔頭自己提出的問題：
#
#     「若日後真的太多，該做的是分割輪替策略」—— 多少才算太多？
#
# 這個腳本量的就是那個倍數。
#
# -----------------------------------------------------------------------------
# 為什麼是 A/B，而不是「量一個絕對數字」
# -----------------------------------------------------------------------------
# 絕對的 TPS 取決於這台機器：開發筆電、CI runner、正式環境的數字互相沒有意義，
# 而把它們拿來比較會得出錯誤的結論。
#
# **倍數（trigger on ÷ trigger off）在同一台機器上量，是可攜的。** 它回答的是
# 「稽核讓寫入慢了幾倍」，而那個比例在不同機器上大致穩定 —— 也正是做容量規劃
# 時真正需要的輸入。
#
# 唯一的變數是觸發器開關。角色與情境兩次都相同（fms_owner + 平台情境），
# 所以量到的是**稽核觸發器**的成本，不含 RLS 的成本。要量 RLS 是另一個 A/B。
#
# -----------------------------------------------------------------------------
# 為什麼不把它變成 CI 的門檻
# -----------------------------------------------------------------------------
# 共用 runner 的效能數字抖動很大（鄰居噪音、CPU 型號不同、磁碟不同）。
# 一個會偶發失敗的效能門檻，最後的命運是被加上 `continue-on-error` 或直接刪掉,
# 而那比沒有門檻更糟：它看起來還在守著。
#
# 因此這個腳本**只量測與記錄**，不判定成敗。數字寫進 docs/perf-baseline.md，
# 由人在容量規劃時讀它。要變成門檻的話，先要有同一台機器上的多次觀測。
#
# 在 `fms_template` 的複本上跑，不動 `fms` —— 與整合測試同一個機制。
# =============================================================================
set -eu

HOST="${PGHOST:-postgres}"
PORT="${PGPORT:-5432}"
SUPER="${POSTGRES_SUPERUSER:-postgres}"
TEMPLATE="${TEMPLATE_DB:-fms_template}"
DB="fms_bench"
CLIENTS="${CLIENTS:-4}"
THREADS="${THREADS:-4}"
TXNS="${TXNS:-250}"

supsql() {
  PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
    psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U "$SUPER" -d postgres "$@"
}
owner() {
  PGPASSWORD="${FMS_OWNER_PASSWORD:-fms_owner_dev}" \
    psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U fms_owner -d "$DB" "$@"
}

cleanup() {
  supsql -q -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" 2>/dev/null || true
  rm -f /tmp/bench-*.sql /tmp/bench-*.log
}
trap cleanup EXIT

echo "==> 稽核寫入放大 A/B（$CLIENTS 客戶端 × $TXNS 交易）"

if ! supsql -tAq -c "SELECT 1 FROM pg_database WHERE datname='$TEMPLATE'" | grep -q 1; then
  echo "  !! 找不到 $TEMPLATE。先跑 make test-template" >&2
  exit 1
fi
supsql -q -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
supsql -q -c "CREATE DATABASE $DB TEMPLATE $TEMPLATE OWNER fms_owner"

# 前提：049 的觸發器在。少了它整個量測沒有意義（兩邊會一樣快）。
HAS=$(owner -tAq -c "SELECT count(*) FROM pg_trigger g JOIN pg_class c ON c.oid=g.tgrelid
                      WHERE g.tgname='trg_audit' AND NOT g.tgisinternal
                        AND c.relname='assets' AND c.relnamespace='fms'::regnamespace")
if [ "$HAS" != "1" ]; then
  echo "  !! assets 上沒有 trg_audit（=$HAS），A/B 會量到兩個相同的數字" >&2
  exit 1
fi

# 每筆交易改一台設備 —— 觸發一次 049 的稽核（整列前後寫進 audit_log）。
# `:client_id` 讓不同客戶端改不同的列，避免量到列鎖等待而不是寫入成本。
#
# **改 `updated_at` 而不是 `name`。** 第一版寫的是 `name = name || 'x'`，
# 而 `assets.name` 是 `varchar(200)`：約 180 筆之後就超長，於是 A 只成功 769/1000
# 筆、B 一開始就全滅（名稱已達上限），pgbench 全部客戶端中止、連 tps 都沒印。
#
# 也就是說那一版量的是「append 幾次會爆掉 varchar(200)」。
# 一個會隨執行次數改變行為的基準不是基準。
#
# `updated_at = clock_timestamp()` 每次都真的變（029 對「沒有欄位變動的 UPDATE」
# 不寫稽核列，所以必須真的變），而且長度固定。稽核仍然複製整列 ——
# 那正是要量的成本。
cat > /tmp/bench-update.sql <<'SQL'
SET app.is_platform = 'on';
UPDATE fms.assets
   SET updated_at = clock_timestamp()
 WHERE id = (SELECT id FROM fms.assets ORDER BY id
              OFFSET (:client_id % (SELECT count(*) FROM fms.assets)) LIMIT 1);
SQL

# 一個回報 0 而不說原因的基準是沒用的 —— 因此解析不到 tps 就把 pgbench 的
# 輸出印出來並中止，而不是讓 0 流進最後的倍數計算（0 會讓倍數變成 inf 或 0，
# 兩者都會被誤讀）。
run() {
  PGPASSWORD="${FMS_OWNER_PASSWORD:-fms_owner_dev}" \
    pgbench -h "$HOST" -p "$PORT" -U fms_owner -d "$DB" \
            -c "$CLIENTS" -j "$THREADS" -t "$TXNS" \
            -f /tmp/bench-update.sql --no-vacuum > "$1" 2>&1 || true
  TPS=$(sed -n 's/^tps = \([0-9.]*\).*/\1/p' "$1" | tail -n 1)
  LAT=$(sed -n 's/^latency average = \([0-9.]*\).*/\1/p' "$1" | tail -n 1)
  PROC=$(sed -n 's/^number of transactions actually processed: \([0-9]*\).*/\1/p' "$1" | tail -n 1)
  FAIL=$(sed -n 's/^number of failed transactions: \([0-9]*\).*/\1/p' "$1" | tail -n 1)
  [ -n "$PROC" ] || PROC=0
  [ -n "$FAIL" ] || FAIL=0
  if [ -z "$TPS" ]; then
    echo "  !! pgbench 沒有回報 tps —— 這一輪失敗了。輸出：" >&2
    sed -n '1,25p' "$1" >&2
    exit 1
  fi
  # 交易有失敗的話倍數就不可信：兩輪的分母不同。
  if [ "$FAIL" != "0" ]; then
    echo "  !! 有 $FAIL 筆交易失敗，倍數不可信。輸出：" >&2
    sed -n '1,25p' "$1" >&2
    exit 1
  fi
}

echo "  --- A：稽核觸發器開著（049 之後的實際情況）---"
run /tmp/bench-on.log
TPS_ON=$TPS; LAT_ON=$LAT
AUDIT_ROWS=$(owner -tAq -c "SET app.is_platform='on';
  SELECT count(*) FROM fms.audit_log WHERE entity_type='ASSETS'")
echo "      tps=$TPS_ON  平均延遲=${LAT_ON}ms  完成=$PROC 筆  產生稽核列=$AUDIT_ROWS"
EXPECTED=$((CLIENTS * TXNS))
if [ "$AUDIT_ROWS" -lt "$EXPECTED" ]; then
  echo "  !! 稽核列 $AUDIT_ROWS < 交易數 $EXPECTED —— 有 UPDATE 沒有觸發稽核" >&2
  echo "     （029 對「沒有欄位變動的 UPDATE」不寫列。基準的 UPDATE 必須真的改到東西）" >&2
  exit 1
fi

echo "  --- B：關掉稽核觸發器（049 之前的等價情況）---"
owner -q -c "ALTER TABLE fms.assets DISABLE TRIGGER trg_audit"
BEFORE=$AUDIT_ROWS
run /tmp/bench-off.log
TPS_OFF=$TPS; LAT_OFF=$LAT
AFTER=$(owner -tAq -c "SET app.is_platform='on';
  SELECT count(*) FROM fms.audit_log WHERE entity_type='ASSETS'")
echo "      tps=$TPS_OFF  平均延遲=${LAT_OFF}ms  完成=$PROC 筆  產生稽核列=$((AFTER - BEFORE))"

# 防空轉：B 階段必須真的沒有寫稽核。若這裡不是 0，DISABLE TRIGGER 沒有生效，
# 而兩個數字會幾乎相同 —— 那會被誤讀成「稽核幾乎免費」。
if [ "$((AFTER - BEFORE))" != "0" ]; then
  echo "  !! B 階段仍在寫稽核列 —— DISABLE TRIGGER 沒生效，倍數無意義" >&2
  exit 1
fi
owner -q -c "ALTER TABLE fms.assets ENABLE TRIGGER trg_audit"

echo ""
echo "==> 結果"
awk -v on="$TPS_ON" -v off="$TPS_OFF" -v lon="$LAT_ON" -v loff="$LAT_OFF" 'BEGIN {
  if (off > 0) printf "    吞吐：%.0f → %.0f tps（稽核讓它降到 %.0f%%）\n", off, on, on/off*100;
  if (lon > 0 && loff > 0) printf "    延遲：%.3f → %.3f ms（%.2f 倍）\n", loff, lon, lon/loff;
  printf "\n    這個倍數才是可攜的數字。絕對 tps 只對這台機器有意義。\n";
}'
echo "    量測條件：$CLIENTS 客戶端 × $TXNS 交易，fms_owner + 平台情境（不含 RLS 成本）"
