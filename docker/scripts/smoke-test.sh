#!/bin/sh
# =============================================================================
# 執行煙霧測試 010（核心不變量）與 012（Offision 對標補強）
# =============================================================================
# 以 fms_owner 執行：
#   * FORCE ROW LEVEL SECURITY 對擁有者亦生效 → T1 租戶隔離斷言有意義
#   * fms_owner 屬 fms_platform → 測試中的前置資料建立可取得平台情境
#   絕不可用 postgres 超級使用者執行，否則 BYPASSRLS 會讓 T1 靜默跳過。
#
# 兩支測試全程 ROLLBACK，不留任何資料；可重複執行。
# 前置：需先跑過 MIGRATE_MODE=seed（009 示範租戶為測試資料來源）。
# =============================================================================
set -eu

HOST="${PGHOST:-postgres}"
PORT="${PGPORT:-5432}"
DB="${POSTGRES_DB:-fms}"
USER="fms_owner"

echo "==> 檢查前置條件"
DEMO=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -Atc \
  "SET app.is_platform='on';
   SELECT count(*) FROM fms.tenants WHERE code='DEMO_GROUP';")
if [ "$DEMO" = "0" ]; then
  echo "!! 找不到示範租戶 DEMO_GROUP。請先執行： MIGRATE_MODE=seed docker compose run --rm migrate" >&2
  exit 1
fi

SUPER=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -Atc \
  "SELECT rolsuper FROM pg_roles WHERE rolname=current_user;")
if [ "$SUPER" = "t" ]; then
  echo "!! 目前角色為超級使用者，RLS 會被繞過，T1 將無意義。請以 fms_owner 執行。" >&2
  exit 1
fi

FAILED=0
for f in 010_smoke_tests.sql 012_smoke_tests_offision_parity.sql; do
  echo ""
  echo "==================== $f ===================="
  if psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -f "/sql/$f" 2>&1; then
    echo "---- $f 完成 ----"
  else
    echo "!! $f 失敗" >&2
    FAILED=1
  fi
done

echo ""
if [ "$FAILED" = "0" ]; then
  echo "==> 全部煙霧測試通過（T1–T10）"
else
  echo "==> 有測試失敗，請檢視上方 EXCEPTION 訊息" >&2
  exit 1
fi
