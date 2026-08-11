#!/bin/sh
# =============================================================================
# 建立測試用的 template 資料庫
# =============================================================================
# 為什麼需要它
#
# 在此之前所有測試共用同一個開發資料庫，靠「以樣式比對刪除 + 還原種子值」
# 清理（`asset_code LIKE 'TEST-%'`、`source='API'`、把庫存改回 24…）。
# 那個做法有三個代價，而且都真的踩到過：
#
#   1. **每個測試檔只能有一個測試函式** —— 同檔案的測試會平行執行，
#      而清理是全域的，第二個測試的 setup 會刪掉第一個測試的資料。
#   2. **每加一種資料就要補一條還原**（庫存、讀表值、核准旗標、密碼…），
#      漏一條的症狀是「第二次執行才失敗」，最難查的那一種。
#   3. **測試會改到共用開發資料庫**，開發者同時在用的話會互相干擾。
#
# 解法是每個測試拿自己的資料庫。PostgreSQL 的 `CREATE DATABASE ... TEMPLATE`
# 是檔案層級複製，比重跑 21 個 migration 快兩個數量級，
# 因此「每個測試一個全新的、已完成 migration 與種子的資料庫」是可行的。
#
# 本腳本只負責建那個 template；複製由測試腳手架自己做。
# =============================================================================
set -eu

HOST="${PGHOST:-postgres}"
PORT="${PGPORT:-5432}"
SUPER="${POSTGRES_SUPERUSER:-postgres}"
# 容器的 PGPASSWORD 是 fms_owner 的（migrate.sh 用）。超級使用者只在
# 建立資料庫與授予 CREATEDB 時需要，因此用一個 wrapper 帶它自己的密碼，
# 而不是把整個腳本的預設身分改成超級使用者。
supsql() {
  PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD:-postgres}" \
    psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U "$SUPER" -d postgres "$@"
}
TEMPLATE="${TEST_TEMPLATE_DB:-fms_template}"

echo "==> 建立測試 template 資料庫：$TEMPLATE"

# fms_owner 預設沒有 CREATEDB（00-roles.sql 只給了 CREATEROLE）。
# 測試要自己建立與丟棄資料庫，因此在這裡補上 —— 這是**開發／CI 環境**的
# 設定，不屬於 migration：生產環境的 fms_owner 不該有 CREATEDB。
supsql -q -c "ALTER ROLE fms_owner CREATEDB;"

# 丟棄測試資料庫時要用 DROP DATABASE ... WITH (FORCE)，那需要終止 fms_app 的
# 連線 —— 而終止別的角色的行程需要 pg_signal_backend。
# 沒有它，teardown 會在測試剛好留著一條連線時失敗（42501），
# 而那取決於測試內部的細節，是很脆弱的依賴。
supsql -q -c "GRANT pg_signal_backend TO fms_owner;"

# 重建以確保 template 與當前 migration 一致。
# 先斷開所有連線：CREATE DATABASE ... TEMPLATE 要求來源沒有其他連線，
# 而殘留的測試資料庫也會擋住 DROP。
#
# 清理用 shell 迴圈而不是 plpgsql 的 DO 區塊：**DROP DATABASE 不能在函式或
# 交易內執行**，寫成 DO 區塊會在真的有殘留資料庫時才失敗 ——
# 也就是只在最需要它的時候壞掉。（這個 bug 一度存在於本腳本，
# 因為當時剛好沒有殘留資料庫，迴圈體從未執行。）
for db in $(supsql -tAc "SELECT datname FROM pg_database WHERE datname LIKE 'fms_test_%'"); do
  echo "--> 清除殘留的測試資料庫 $db"
  supsql -q -c "DROP DATABASE IF EXISTS \"$db\" WITH (FORCE)"
done

# 先取消 template 標記：被標記為 template 的資料庫不能 DROP。
# （這個 bug 只在**重建**時出現 —— 第一次建立時它還不是 template，
#  因此第一次總是成功、第二次總是失敗。）
supsql -q -c "UPDATE pg_database SET datistemplate = false WHERE datname = '$TEMPLATE'"
supsql -q -c "DROP DATABASE IF EXISTS $TEMPLATE WITH (FORCE)"
supsql -q -c "CREATE DATABASE $TEMPLATE OWNER fms_owner"

# 以 fms_owner 身分套用完整 migration + 種子（理由見 migrate.sh 檔頭）
POSTGRES_DB="$TEMPLATE" MIGRATE_MODE=all sh /scripts/migrate.sh

# 標記為 template：這會讓 PostgreSQL 允許非超級使用者以它為來源複製。
supsql -q -c "UPDATE pg_database SET datistemplate = true WHERE datname = '$TEMPLATE';"

echo "==> template 就緒：$TEMPLATE（測試會由它複製出各自的資料庫）"
