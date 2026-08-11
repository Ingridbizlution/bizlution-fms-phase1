#!/bin/sh
# =============================================================================
# 回退 migration（逆序執行 sql/down/ 的對應檔案）
# =============================================================================
# 用法：
#   TO=016 sh /scripts/migrate-down.sh    # 回退到 016（執行 021…017 的 down）
#
# 只涵蓋 014 以後：001–013 是規格書交付的基線 schema，
# 回退它們的意思是清空資料庫，正確做法是從備份還原（見 sql/down/README.md）。
#
# 每一步在自己的交易裡（psql 的 -1 只作用於單一檔案）：
# 一步失敗不會回滾前面成功的步驟，狀態因此始終是「停在某個明確的版本」，
# 而不是半個版本。
# =============================================================================
set -eu

HOST="${PGHOST:-postgres}"
PORT="${PGPORT:-5432}"
DB="${POSTGRES_DB:-fms}"
USER="fms_owner"
TO="${TO:-013}"

FLOOR=13
if [ "$TO" -lt "$FLOOR" ]; then
  echo "拒絕：$FLOOR 以前沒有 down migration（見 sql/down/README.md）" >&2
  exit 1
fi

echo "==> 回退 $DB 至 $TO"

# 逆序：由高到低
for f in $(ls -r /sql/down/*.sql 2>/dev/null); do
  base=$(basename "$f")
  num=$(echo "$base" | cut -c1-3)
  case "$num" in ''|*[!0-9]*) continue ;; esac
  if [ "$num" -le "$TO" ]; then
    continue
  fi
  echo "--> down $base"
  psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -q -f "$f"
done

echo "==> 回退完成，目前版本 $TO"
