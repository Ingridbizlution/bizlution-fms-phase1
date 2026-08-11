#!/bin/sh
# =============================================================================
# T11 併發搶訂：N 個客戶端同時搶同一資源的同一時段，必須恰好一筆成功
# =============================================================================
# 對應規格書 13.1 Definition of Done：
#   「100 執行緒併發搶訂同一時段的整合測試結果為『恰好一筆成功』」
# 以及 ADR-04：「防止重複預約的最終權威是 PostgreSQL 的 GiST 排他約束」。
#
# 為什麼 010 的 T2 不足以取代本檔：
#   T2 是「先插一筆，再插一筆重疊的」——兩次寫入序列化，證明的是約束語意正確。
#   它無法證明「兩個交易同時抵達」時不會雙雙成功。序列化正確而並發錯誤，
#   正是排他約束若被誤換成「先 SELECT 檢查再 INSERT」時會出現的結果。
#
# 以 fms_app 執行（而非 fms_owner）：這是應用程式實際使用的角色，RLS 完整生效。
#
# 為什麼用 pgbench 而不是背景起 N 個 psql：
#   最初的版本為每個客戶端開一個 psql 行程。100 個行程同時啟動會把容器的 CPU
#   壓垮，導致「贏家遲遲無法 COMMIT → 其餘 99 個等到 statement_timeout(30s)
#   被取消」，測試因此以錯誤的理由失敗（OTHER 而非 LOST_EXCLUSION）。
#   pgbench 用單一行程內的非阻塞連線多工，開 100 條連線的成本低得多，
#   且它會先把所有連線建立好才開始送出交易 —— 這本身就是我們需要的同步起跑點。
#
# 判定標準（硬性）：
#   * 資料庫實際落地恰好 1 筆   ← 這就是 DoD 要求的不變量
#   * 恰好 1 個客戶端自認成功
#
# 落敗方式（記錄但不判失敗）：
#   實測 100 路競爭時，落敗者的 SQLSTATE 會在兩者之間變動：
#     23P01 exclusion_violation — 乾淨地撞上排他約束
#     40P01 deadlock_detected   — 兩個插入者各自先寫入索引項、再檢查衝突，
#                                 因而可能互相等待成環，由 PostgreSQL 擇一犧牲
#   兩者都是「你輸了這場競爭」的合法結果，正確性不受影響（落地仍是 1 筆）。
#   但這對應用層是硬需求：40P01 必須重試或映射成 409，
#   不能讓使用者看到「內部錯誤」。ADR-04 把 Redis 鎖列為第二階段的
#   「減少無效往返」優化，本測試顯示它同時也免掉應用層處理死鎖重試的負擔。
#
#   死鎖犧牲者往往連自己的結果都寫不進 race_probe（交易已中止），
#   因此「回報數 < 客戶端數」是預期現象，改以伺服器端的
#   pg_stat_database.deadlocks 差值交叉印證。
#
# 其他 SQLSTATE 一律判失敗：這條保留了診斷價值 —— 開發本檔的過程中，
# 它先抓到 57014（statement_timeout，因舊版為每個客戶端開一個 psql 行程
# 把 CPU 壓垮），又抓到 22001（reservation_no 是 varchar(40)，
# 'RACE-'||完整 uuid 為 41 字元）。若把「其他」也放寬，這兩個 bug 都會被
# 誤判為通過。
# =============================================================================
set -eu

HOST="${PGHOST:-postgres}"
PORT="${PGPORT:-5432}"
DB="${POSTGRES_DB:-fms}"
CLIENTS="${CLIENTS:-100}"
# pgbench 執行緒數；客戶端以非阻塞方式多工於執行緒之上
THREADS="${THREADS:-8}"
# 測試期間的 statement_timeout（見下方 pgbench 腳本內的說明）
STMT_TIMEOUT="${STMT_TIMEOUT:-120s}"

# 009 示範租戶：401 會議室
TENANT="aaaaaaaa-0000-4000-8000-000000000001"
FACILITY="cccccccc-0000-4000-8000-000000000001"
BOOKABLE="70000000-0000-4000-8000-000000000001"
NODE="10000000-0000-4000-8000-000000000005"
ORGANIZER="ffffffff-0000-4000-8000-000000000002"
# 刻意選一個遠期且不與示範資料重疊的時段
SLOT_START="2027-03-01 10:00+08"
SLOT_END="2027-03-01 11:00+08"

SCRIPT="/tmp/race-$$.sql"

owner_psql() {
  PGPASSWORD="${FMS_OWNER_PASSWORD:-fms_owner_dev}" \
    psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U fms_owner -d "$DB" -qAt "$@"
}

cleanup() {
  owner_psql -c "SET app.is_platform='on';
                 DELETE FROM fms.reservations WHERE tenant_id='$TENANT' AND reservation_no LIKE 'RACE-%';
                 DROP TABLE IF EXISTS public.race_probe;" >/dev/null 2>&1 || true
  rm -f "$SCRIPT"
}
trap cleanup EXIT INT TERM

echo "==> T11 併發搶訂：$CLIENTS 個客戶端搶同一時段（$SLOT_START ~ $SLOT_END）"

# --- 前置檢查 ---------------------------------------------------------------
DEMO=$(owner_psql -c "SET app.is_platform='on';
                      SELECT count(*) FROM fms.tenants WHERE code='DEMO_GROUP';" | tail -n 1)
if [ "$DEMO" = "0" ]; then
  echo "!! 找不到示範租戶 DEMO_GROUP。請先執行 make seed" >&2
  exit 1
fi

# 清掉上一次可能殘留的測試資料，讓本檔可重複執行
owner_psql -c "SET app.is_platform='on';
               DELETE FROM fms.reservations WHERE tenant_id='$TENANT' AND reservation_no LIKE 'RACE-%';" >/dev/null

PRE=$(owner_psql -c "SET app.tenant_id='$TENANT';
                     SELECT count(*) FROM fms.reservations
                     WHERE resource_id='$NODE'
                       AND time_range && tstzrange('$SLOT_START','$SLOT_END','[)')
                       AND status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN');" | tail -n 1)
if [ "$PRE" != "0" ]; then
  echo "!! 目標時段已有 $PRE 筆有效預約，測試前提不成立" >&2
  exit 1
fi

# 結果收集表：刻意建在 public（無 RLS），避免與被測的隔離機制互相干擾
owner_psql -c "DROP TABLE IF EXISTS public.race_probe;
               CREATE TABLE public.race_probe(
                 outcome text NOT NULL,
                 at timestamptz NOT NULL DEFAULT clock_timestamp());
               GRANT INSERT, SELECT ON public.race_probe TO fms_app;" >/dev/null

# --- pgbench 腳本 -----------------------------------------------------------
# 刻意不使用任何 pgbench 變數（:client_id 等）：變數替換是純文字的，
# 在 $do$ ... $do$ 區塊內可能被誤代換。reservation_no 改用 gen_random_uuid()
# 取得唯一值，避免「唯一鍵衝突」掩蓋掉我們要觀察的「排他約束衝突」。
# 注意長度：reservation_no 是 varchar(40)，完整 uuid 是 36 字元，
# 'RACE-'||uuid 會是 41 字元而觸發 22001（string_data_right_truncation），
# 因此截到 30 字元（總長 35）。
cat > "$SCRIPT" <<SQL
-- 刻意放寬 statement_timeout：00-roles.sql 給 fms_app 的預設是 30s，
-- 而 100 路競爭時死鎖偵測與解除的尾端會超過 30s，導致大量客戶端被
-- 57014 取消。若不放寬，本測試量到的是「role 的逾時設定」而不是
-- 「排他約束的行為」，落敗原因的分佈也會被截斷而失去意義。
-- 這個 30s 的行為本身是給應用層的重要輸入，見腳本尾端的提示。
SET statement_timeout = '$STMT_TIMEOUT';
BEGIN;
SELECT set_config('app.tenant_id', '$TENANT', true);
SELECT set_config('app.user_id',   '$ORGANIZER', true);
DO \$do\$
BEGIN
  INSERT INTO fms.reservations (tenant_id, facility_id, bookable_resource_id, reservation_no,
                               resource_type, resource_id, organizer_id, title,
                               start_at, end_at, status)
  VALUES ('$TENANT', '$FACILITY', '$BOOKABLE', 'RACE-'||substr(gen_random_uuid()::text, 1, 30),
          'SPATIAL_NODE', '$NODE', '$ORGANIZER', '併發搶訂',
          '$SLOT_START', '$SLOT_END', 'CONFIRMED');
  INSERT INTO public.race_probe(outcome) VALUES ('WON');
EXCEPTION
  WHEN exclusion_violation THEN
    INSERT INTO public.race_probe(outcome) VALUES ('LOST_EXCLUSION');
  WHEN others THEN
    INSERT INTO public.race_probe(outcome) VALUES ('OTHER_'||SQLSTATE);
END
\$do\$;
COMMIT;
SQL

# --- 發動 -------------------------------------------------------------------
echo "    pgbench：-c $CLIENTS -j $THREADS -t 1（連線全部建立後才開始送出交易）"
PGB_LOG="/tmp/pgbench-$$.log"
PGPASSWORD="${FMS_APP_PASSWORD:-fms_app_dev}" \
  pgbench -h "$HOST" -p "$PORT" -U fms_app -d "$DB" \
          -c "$CLIENTS" -j "$THREADS" -t 1 -f "$SCRIPT" --no-vacuum > "$PGB_LOG" 2>&1 || true
grep -E "number of (clients|transactions actually processed)|number of failed" "$PGB_LOG" || true

# pgbench 自己回報的失敗交易數 —— 這些正是「錯誤逸出 DO 區塊、因而寫不進結果表」
# 的客戶端。用它交叉印證未回報數，比 pg_stat_database.deadlocks 可靠：
# 後者由統計收集器非同步更新，pgbench 剛結束時讀取會偏低而造成偽失敗。
PGB_FAILED=$(sed -n 's/^number of failed transactions: \([0-9]*\).*/\1/p' "$PGB_LOG" | tail -n 1)
[ -n "$PGB_FAILED" ] || PGB_FAILED=0
# 實際完成的交易數；被中止（aborted）的客戶端既不算 processed 也不算 failed
PGB_PROCESSED=$(sed -n 's/^number of transactions actually processed: \([0-9]*\).*/\1/p' "$PGB_LOG" | tail -n 1)
[ -n "$PGB_PROCESSED" ] || PGB_PROCESSED=0
echo "    pgbench 中止的客戶端錯誤（前 3 筆）："
grep -oE "aborted in command.*|ERROR:.*" "$PGB_LOG" | sort -u | head -3 | sed 's/^/      /' || true

# --- 統計 -------------------------------------------------------------------
WON=$(owner_psql   -c "SELECT count(*) FROM public.race_probe WHERE outcome='WON';" | tail -n 1)
LOST=$(owner_psql  -c "SELECT count(*) FROM public.race_probe WHERE outcome='LOST_EXCLUSION';" | tail -n 1)
DEADLOCK=$(owner_psql -c "SELECT count(*) FROM public.race_probe WHERE outcome='OTHER_40P01';" | tail -n 1)
# 「非預期」= 既不是排他約束衝突、也不是死鎖的其他錯誤碼
UNEXPECTED=$(owner_psql -c "SELECT count(*) FROM public.race_probe
                            WHERE outcome LIKE 'OTHER_%' AND outcome <> 'OTHER_40P01';" | tail -n 1)
UNEXPECTED_CODES=$(owner_psql -c "SELECT coalesce(string_agg(DISTINCT outcome, ', '), '無')
                                  FROM public.race_probe
                                  WHERE outcome LIKE 'OTHER_%' AND outcome <> 'OTHER_40P01';" | tail -n 1)
TOTAL=$((WON + LOST + DEADLOCK + UNEXPECTED))
UNREPORTED=$((CLIENTS - TOTAL))

ROWS=$(owner_psql -c "SET app.tenant_id='$TENANT';
                      SELECT count(*) FROM fms.reservations
                      WHERE resource_id='$NODE'
                        AND time_range && tstzrange('$SLOT_START','$SLOT_END','[)')
                        AND status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN');" | tail -n 1)

echo ""
echo "    資料庫實際落地：$ROWS 筆（期望 1）"
echo "    落敗方式：23P01 排他約束=$LOST  40P01 死鎖=$DEADLOCK  非預期=$UNEXPECTED（$UNEXPECTED_CODES）"
echo "    未能回報：$UNREPORTED（pgbench 回報失敗交易 $PGB_FAILED 筆）"
echo ""

FAILED=0
# 硬性：DoD 的不變量 —— 恰好一筆成功
[ "$ROWS" = "1" ]  || { echo "  FAIL 資料庫落地 $ROWS 筆，期望恰好 1 筆（雙重預訂！）" >&2; FAILED=1; }
[ "$WON" = "1" ]   || { echo "  FAIL 有 $WON 個客戶端成功，期望恰好 1 個" >&2; FAILED=1; }
# 硬性：每個客戶端都要有下場，且落敗原因只能是排他約束或死鎖
[ "$TOTAL" = "$CLIENTS" ] || {
  echo "  FAIL 只有 $TOTAL/$CLIENTS 個客戶端回報下場（pgbench 完成 $PGB_PROCESSED 筆）" >&2
  echo "       若伴隨 57014，通常是 statement_timeout 過短；本測試已設為 $STMT_TIMEOUT" >&2
  FAILED=1; }
[ "$((LOST + DEADLOCK))" = "$((CLIENTS - 1))" ] || {
  echo "  FAIL 落敗者 $((LOST + DEADLOCK)) 個，期望 $((CLIENTS - 1)) 個" >&2; FAILED=1; }
[ "$UNEXPECTED" = "0" ] || {
  echo "  FAIL 有 $UNEXPECTED 個客戶端以非預期的錯誤碼失敗：$UNEXPECTED_CODES" >&2
  echo "       （57014=statement_timeout；22001=字串過長，代表測試資料有誤）" >&2
  FAILED=1; }

if [ "$FAILED" = "0" ]; then
  echo "==> T11 PASSED: $CLIENTS 個客戶端同時搶訂，恰好一筆成功（落地 $ROWS 筆）"
  if [ "$DEADLOCK" -gt 0 ]; then
    echo ""
    echo "    ── 給應用層的輸入 ──────────────────────────────────────────"
    echo "    本次有 $DEADLOCK/$((CLIENTS - 1)) 個落敗者是以 40P01（死鎖）收場，而非 23P01。"
    echo "    成因：排他約束是「先寫入索引項、再檢查衝突」，因此兩個插入者可能"
    echo "    互相等待成環，由 PostgreSQL 擇一犧牲。正確性不受影響（落地仍是 1 筆），"
    echo "    但 API 必須把 40P01 視同「時段已被搶走」而重試或回 409，不可回 500。"
    echo "    另注意：本測試把 statement_timeout 放寬到 $STMT_TIMEOUT 才能讓全部"
    echo "    客戶端跑完；若沿用 fms_app 的 role 預設 30s，此競爭程度下會有可觀比例"
    echo "    的客戶端被 57014 取消。這使得 schema 已備的兩階段 reservation_holds"
    echo "    （或 ADR-04 列為第二階段的 Redis 短期鎖）在高競爭場景並非純優化，"
    echo "    而是可用性的前提。"
    echo "    ────────────────────────────────────────────────────────────"
  fi
else
  echo "==> T11 FAILED" >&2
  exit 1
fi
