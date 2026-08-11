#!/bin/sh
# =============================================================================
# 備份／還原演練
# =============================================================================
# `make backup` 與 `make restore` 從第一天就存在，而**從來沒有被執行過一次**。
# 一份沒有人還原過的備份不是備份，是一個檔案。
#
# 049 之後這件事有了法遵意義：稽核軌跡涵蓋 work_orders 與 assets，
# 「誰在什麼時候把這張工單的優先度改掉」現在是一個要答得出來的問題，
# 而答案只存在資料庫裡。
#
# -----------------------------------------------------------------------------
# 這個演練驗什麼，以及為什麼是這幾項
# -----------------------------------------------------------------------------
# `pg_dump -Fc` + `pg_restore --clean --if-exists` 最可怕的失敗不是「還原失敗」
# —— 那會直接報錯。可怕的是**還原成功但少了某樣東西**，而少掉的那樣正好是
# 隔離的依據：
#
#   * **政策**（RLS policy）少掉 → 租戶隔離消失，而查詢全部照樣成功
#   * **FORCE ROW LEVEL SECURITY** 少掉 → 擁有者繞過 RLS，同樣沒有外顯錯誤
#   * **觸發器**少掉 → 稽核不再寫、狀態機不再擋，兩者都是安靜的
#   * **分割**少掉 → 列掉進 DEFAULT，然後自我鎖死（見 028）
#
# 因此比對的不是「還原有沒有報錯」，而是這四類物件的數量與名稱。
#
# -----------------------------------------------------------------------------
# 兩個刻意的設計
# -----------------------------------------------------------------------------
# **在拋棄式資料庫上做，不動 `fms`。** 演練不該有毀掉開發資料的風險。
#
# **還原前先插一個標記列。** 少了這一步，「pg_restore 什麼都沒做」也會讓
# 所有比對通過 —— 因為來源與目標本來就一樣。標記列必須在還原後消失，
# 那是「還原真的取代了資料」的唯一證據。這與 `the_maintainer_closes_a_real_gap`
# 是同一個教訓：先製造差異，再驗證它被消除。
#
# -----------------------------------------------------------------------------
# 已知不在 dump 裡的東西（本腳本會明白報出來）
# -----------------------------------------------------------------------------
# `pg_dump` 是**單一資料庫**的匯出，**不含角色定義**（`fms_owner`／`fms_app`／
# `fms_readonly`）。物件上的 GRANT 有，但角色本身沒有。也就是說還原到一個
# 全新的叢集之前，必須先有那些角色，否則 pg_restore 會在 GRANT 那裡失敗。
#
# 正式環境的備份程序因此需要兩份：
#
#     pg_dumpall --globals-only    # 角色與叢集層設定
#     pg_dump -Fc -d fms           # 這一份
# =============================================================================
set -eu

HOST="${PGHOST:-postgres}"
PORT="${PGPORT:-5432}"
SUPER="${POSTGRES_SUPERUSER:-postgres}"
SRC="${POSTGRES_DB:-fms}"
DB="fms_drill"
DUMP="/tmp/fms-drill.dump"

supsql() {
  PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
    psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U "$SUPER" -d postgres "$@"
}
# 還原目標。**不加 ON_ERROR_STOP**：pg_restore --clean 對不存在的物件會抱怨，
# 那是預期的（--if-exists 已經盡量壓下，仍有殘餘）。
# 查詢用**超級使用者**：FORCE RLS 對 fms_owner 也生效，而這裡沒有租戶情境
# —— 以 fms_owner 數列數會全部回 0，而「兩邊都是 0」會讓比對通過。
# 那正是這個腳本要抓的那種假通過。
q() {
  PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
    psql -h "$HOST" -p "$PORT" -U "$SUPER" -d "$2" -Atc "$1" | tail -n 1
}

cleanup() {
  supsql -q -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" 2>/dev/null || true
  rm -f "$DUMP"
}
trap cleanup EXIT

FAILED=0

echo "==> 備份／還原演練（來源 $SRC，拋棄式目標 $DB）"

# --- 0. 備份角色的前提 -------------------------------------------------------
# `fms_backup` 必須存在、有 BYPASSRLS、且**不屬於 fms_platform**。
# 少了 BYPASSRLS，pg_dump 會被 FORCE RLS 擋住（這個演練當初就是這樣發現
# `make backup` 壞掉的）。屬於 fms_platform 則是另一回事：那會給它一把
# 應用語意層的鑰匙，而備份只需要儲存層讀得到。
ROLE_OK=$(PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
  psql -h "$HOST" -p "$PORT" -U "$SUPER" -d "$SRC" -Atc \
  "SELECT coalesce(bool_and(rolbypassrls AND NOT rolsuper
                            AND NOT pg_has_role(rolname,'fms_platform','MEMBER')), false)
     FROM pg_roles WHERE rolname='fms_backup'")
if [ "$ROLE_OK" != "t" ]; then
  echo "  !! fms_backup 不存在或屬性不對（需 BYPASSRLS、非超級使用者、不屬 fms_platform）" >&2
  echo "     跑 make backup-role" >&2
  exit 1
fi

# **屬性對不代表用得了。** BYPASSRLS 繞過的是列級安全，不是 schema 權限。
# 少了 USAGE 的話 pg_dump 會吐一整面 LOCK TABLE 的 SQL 再說
# permission denied —— 那面牆看不出缺的是什麼。這一行把它講成一句話。
SCHEMA_OK=$(PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
  psql -h "$HOST" -p "$PORT" -U "$SUPER" -d "$SRC" -Atc \
  "SELECT has_schema_privilege('fms_backup','fms','USAGE')")
if [ "$SCHEMA_OK" != "t" ]; then
  echo "  !! fms_backup 對 schema fms 沒有 USAGE —— pg_dump 會 permission denied" >&2
  echo "     全新環境的 initdb 跑在 migration 之前，那時 schema 還不存在；" >&2
  echo "     靠的是 ALTER DEFAULT PRIVILEGES ... ON SCHEMAS（見 02-backup-role.sh）" >&2
  exit 1
fi
echo "  [0] fms_backup 就緒（BYPASSRLS、非超級使用者、不屬 fms_platform、schema 進得去）"

# --- 1. 備份（以最小權限的 fms_backup，與 make backup 同一條路徑）------------
PGPASSWORD="${FMS_BACKUP_PASSWORD:-fms_backup_dev}" \
  pg_dump -h "$HOST" -p "$PORT" -U fms_backup -d "$SRC" -Fc > "$DUMP"
SIZE=$(wc -c < "$DUMP")
echo "  [1] 備份完成：$SIZE bytes"
if [ "$SIZE" -lt 10000 ]; then
  echo "  !! 備份檔小得可疑，可能是空的" >&2
  exit 1
fi

# --- 2. 角色不在 dump 裡（明白報出來，不是靜默的假設）------------------------
ROLES_IN_DUMP=$(pg_restore -l "$DUMP" | grep -c "CREATE ROLE" || true)
echo "  [2] dump 內的角色定義數：$ROLES_IN_DUMP（預期 0 —— 需搭配 pg_dumpall --globals-only）"

# --- 3. 全新還原 --------------------------------------------------------------
supsql -q -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
supsql -q -c "CREATE DATABASE $DB OWNER fms_owner"
PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
  pg_restore -h "$HOST" -p "$PORT" -U "$SUPER" -d "$DB" "$DUMP" >/dev/null 2>&1 || true
echo "  [3] 全新還原完成"

# --- 4. 比對隔離的依據 ------------------------------------------------------
# 這四類的共同點：少掉都不會有外顯錯誤。
compare() {
  local label="$1" sql="$2"
  local a b
  a=$(q "$sql" "$SRC")
  b=$(q "$sql" "$DB")
  if [ "$a" = "$b" ] && [ -n "$a" ]; then
    printf "  [4] %-22s 來源=%s 還原=%s ✓\n" "$label" "$a" "$b"
  else
    printf "  [4] %-22s 來源=%s 還原=%s  ← 不一致\n" "$label" "$a" "$b" >&2
    FAILED=1
  fi
}
compare "RLS 政策" \
  "SELECT count(*)::text FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid
    WHERE c.relnamespace='fms'::regnamespace;"
compare "FORCE RLS 的表" \
  "SELECT count(*)::text FROM pg_class c
    WHERE c.relnamespace='fms'::regnamespace AND c.relforcerowsecurity;"
compare "觸發器" \
  "SELECT count(*)::text FROM pg_trigger g JOIN pg_class c ON c.oid=g.tgrelid
    WHERE c.relnamespace='fms'::regnamespace AND NOT g.tgisinternal;"
compare "分割" \
  "SELECT count(*)::text FROM pg_inherits i JOIN pg_class c ON c.oid=i.inhparent
    WHERE c.relnamespace='fms'::regnamespace;"
compare "函式" \
  "SELECT count(*)::text FROM pg_proc p
    WHERE p.pronamespace='fms'::regnamespace;"
# 名稱而非只有數量：數量相同但少了某條政策、多了另一條，是會發生的。
compare "政策名稱指紋" \
  "SELECT md5(string_agg(c.relname||'.'||p.polname, ',' ORDER BY c.relname, p.polname))
     FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid
    WHERE c.relnamespace='fms'::regnamespace;"

# --- 5. 資料列數 -------------------------------------------------------------
# 走 q()，也就是超級使用者 —— 見上面的說明：以 fms_owner 數會兩邊都回 0
# 而「一致」，那是這個腳本最容易掉進去的假通過。
ROWS_SQL="SELECT md5(string_agg(t.relname||'='||t.n::text, ',' ORDER BY t.relname))
    FROM (SELECT c.relname,
                 (xpath('/row/c/text()', query_to_xml(
                    format('SELECT count(*) AS c FROM fms.%I', c.relname),
                    false, true, '')))[1]::text::bigint AS n
            FROM pg_class c JOIN pg_attribute a ON a.attrelid=c.oid
           WHERE c.relnamespace='fms'::regnamespace AND c.relkind IN ('r','p')
             AND a.attname='tenant_id' AND NOT c.relispartition) t;"
A_ROWS=$(q "$ROWS_SQL" "$SRC")
B_ROWS=$(q "$ROWS_SQL" "$DB")
if [ "$A_ROWS" = "$B_ROWS" ] && [ -n "$A_ROWS" ]; then
  echo "  [5] 租戶表列數指紋一致 ✓"
else
  echo "  [5] 租戶表列數指紋不一致：來源=$A_ROWS 還原=$B_ROWS" >&2
  FAILED=1
fi

# --- 6. `--clean --if-exists` 這條路徑（Makefile 實際用的） -------------------
# **先插標記列**：少了這一步，「pg_restore 什麼都沒做」也會通過上面全部比對。
PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
  psql -v ON_ERROR_STOP=1 -q -h "$HOST" -p "$PORT" -U "$SUPER" -d "$DB" \
  -c "INSERT INTO fms.tenants (code, name, status)
      VALUES ('drill-marker', '還原前的標記', 'ACTIVE')" >/dev/null

MARKER_BEFORE=$(q "SELECT count(*)::text FROM fms.tenants WHERE code='drill-marker';" "$DB")
if [ "$MARKER_BEFORE" != "1" ]; then
  echo "  !! 標記列沒插進去（=$MARKER_BEFORE），這個檢查會變成空轉" >&2
  exit 1
fi

PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
  pg_restore -h "$HOST" -p "$PORT" -U "$SUPER" -d "$DB" --clean --if-exists "$DUMP" \
  >/dev/null 2>&1 || true

MARKER_AFTER=$(q "SELECT count(*)::text FROM fms.tenants WHERE code='drill-marker';" "$DB")
if [ "$MARKER_AFTER" = "0" ]; then
  echo "  [6] --clean --if-exists 真的取代了資料（標記列已消失）✓"
else
  echo "  [6] 標記列還在（=$MARKER_AFTER）—— 還原沒有真的取代資料" >&2
  FAILED=1
fi

# 還原兩次之後，隔離的依據仍然完整。
compare "政策名稱指紋（二次還原後）" \
  "SELECT md5(string_agg(c.relname||'.'||p.polname, ',' ORDER BY c.relname, p.polname))
     FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid
    WHERE c.relnamespace='fms'::regnamespace;"
compare "FORCE RLS（二次還原後）" \
  "SELECT count(*)::text FROM pg_class c
    WHERE c.relnamespace='fms'::regnamespace AND c.relforcerowsecurity;"

echo ""
if [ "$FAILED" = "0" ]; then
  echo "==> 演練通過：備份可還原，且政策／FORCE RLS／觸發器／分割／列數都完整"
  echo "    提醒：dump 不含角色定義，正式環境需搭配 pg_dumpall --globals-only"
else
  echo "==> 演練失敗" >&2
  exit 1
fi
