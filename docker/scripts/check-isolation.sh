#!/bin/sh
# =============================================================================
# 租戶隔離的機械化保險（以 fms_app 身分執行）
# =============================================================================
# 010 的 T1 以 fms_owner 驗證政策本身寫對了；本檔補的是另一件事：
# 驗證「應用程式實際使用的連線角色」在各種情境下都拿不到不該拿的資料。
# 這兩者不能互相取代 —— fms_owner 屬 fms_platform，能取得平台情境；
# fms_app 不屬於，且 013 之後即使自行設上 app.is_platform 也無效。
#
# 對應規格書第 13.1 節 Definition of Done 兩項：
#   * 未設 context 的資料庫連線查詢任一租戶表回傳 0 列（CI 強制）
#   * 以 fms_app 連線並設定 app.is_platform = 'on' 後，查詢其他租戶資料仍回 0 列
#
# 前置：需先跑過 make seed（009 示範租戶為 C 案的資料來源）。
# 本檔唯讀，不寫入任何資料。
# =============================================================================
set -eu

HOST="${PGHOST:-postgres}"
PORT="${PGPORT:-5432}"
DB="${POSTGRES_DB:-fms}"
USER="fms_app"
DEMO_TENANT="aaaaaaaa-0000-4000-8000-000000000001"

# 只取最後一行：多語句的 -c 會先輸出前面語句的命令標籤（例如 SET），
# 我們要的一律是最後那個 SELECT 的單一值。
q() {
  psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -Atc "$1" | tail -n 1
}

echo "==> 以 $USER 驗證租戶隔離（$HOST:$PORT/$DB）"

# 先確認連線角色不是超級使用者，否則 BYPASSRLS 會讓以下斷言全部靜默通過
SUPER=$(q "SELECT rolsuper FROM pg_roles WHERE rolname=current_user;")
if [ "$SUPER" = "t" ]; then
  echo "!! 目前角色為超級使用者，BYPASSRLS 會讓斷言失去意義。請以 fms_app 執行。" >&2
  exit 1
fi

FAILED=0

# --- A. 完全未設 context -----------------------------------------------------
# 這是「新增資料存取路徑時忘記注入 context」的保險：漏寫的後果必須是查不到資料，
# 而不是看到全部租戶的資料。
#
# **掃描每一張含 tenant_id 的表，不是挑兩張。** 原本只查 facilities 與 users，
# 而那讓 migration 038 的缺陷溜了過去：`holiday_calendars` 的 `facility_scope`
# 政策建成 PERMISSIVE（漏了 `AS RESTRICTIVE`），於是它被 OR 進 tenant_isolation
# —— 那張表在**完全沒有情境**的連線上讀得到，跨租戶。
#
# 這一格檢查的正是那個情境。它當時沒有抓到，只因為表的清單是手寫的。
# 現在從 pg_class 列舉，新增表自動納入。
#
# 只數 **tenant_id 非 NULL** 的列。`tenant_id IS NULL` 在這個 schema 裡代表
# 「平台提供、所有租戶共用」（角色目錄、設備型錄、通知範本、狀態機規則…），
# 而那些表的政策刻意讓它們在任何情境下都讀得到。第一版沒有這個條件，
# 於是 7 張平台目錄表全部誤報 —— 一個會誤報的檢查最後會被關掉。
A_BAD=$(q "
  SELECT coalesce(string_agg(t.relname || '=' || t.n, ', ' ORDER BY t.relname), '')
    FROM (
      SELECT c.relname,
             (xpath('/row/c/text()',
                 query_to_xml(
                   format('SELECT count(*) AS c FROM fms.%I WHERE tenant_id IS NOT NULL',
                          c.relname),
                   false, true, '')))[1]::text::bigint AS n
        FROM pg_class c
        JOIN pg_namespace ns ON ns.oid = c.relnamespace
        JOIN pg_attribute a ON a.attrelid = c.oid
       WHERE ns.nspname = 'fms' AND c.relkind IN ('r','p')
         AND a.attname = 'tenant_id' AND NOT c.relispartition
    ) t
   WHERE t.n > 0;")
if [ -z "$A_BAD" ]; then
  A_N=$(q "SELECT count(DISTINCT c.relname) FROM pg_class c
             JOIN pg_namespace ns ON ns.oid = c.relnamespace
             JOIN pg_attribute a ON a.attrelid = c.oid
            WHERE ns.nspname='fms' AND c.relkind IN ('r','p')
              AND a.attname='tenant_id' AND NOT c.relispartition;")
  echo "  [A] PASS 未設 context：$A_N 張含 tenant_id 的表都看不到任何租戶資料"
else
  echo "  [A] FAIL 未設 context 竟可見資料：$A_BAD" >&2
  echo "         最可能的原因：某張表的 facility_scope 政策忘了 AS RESTRICTIVE" >&2
  echo "         （PERMISSIVE 會與 tenant_isolation 以 OR 組合，見 migration 046）" >&2
  FAILED=1
fi

# --- A2. facility_scope 必須是 RESTRICTIVE -----------------------------------
# A 抓的是症狀，這裡抓的是原因 —— 而原因抓得到的時候，症狀還沒發生。
# （A 只有在那張表真的有跨租戶資料時才會亮；一個只有單一租戶的環境不會。）
# 比對用 `facility_scope%` 前綴而不是等於：050 為 8 張表加了
# `facility_scope_update` 與 `facility_scope_delete`（擋修改／刪除租戶通用列），
# 而它們少寫 AS RESTRICTIVE 的後果與 038 那次一模一樣。
A2_BAD=$(q "
  SELECT coalesce(string_agg(c.relname || '.' || p.polname, ', '
                             ORDER BY c.relname, p.polname), '')
    FROM pg_policy p JOIN pg_class c ON c.oid = p.polrelid
   WHERE p.polname LIKE 'facility_scope%' AND p.polpermissive
     AND c.relnamespace = 'fms'::regnamespace;")
if [ -z "$A2_BAD" ]; then
  echo "  [A2] PASS 所有 facility_scope* 政策都是 RESTRICTIVE"
else
  echo "  [A2] FAIL 這些表的 facility_scope 是 PERMISSIVE（會 OR 掉 tenant_isolation）：$A2_BAD" >&2
  FAILED=1
fi

# --- A3. 只有備份角色可以有 BYPASSRLS ---------------------------------------
# 這個叢集現在有一個 `fms_backup`（BYPASSRLS、唯讀），因為 pg_dump 會被
# FORCE RLS 擋住。BYPASSRLS 是**角色屬性**不是政策：它讓政策根本不被評估。
#
# **A 與 B 其實抓得到這件事**（實測給 fms_app 加上 BYPASSRLS：A 列出 29 張
# 洩漏的表、B 也失敗）。所以這一格的價值不是「別人抓不到」，而是與 A2 同型
# 的那兩點：
#
#   * **A 報症狀，A3 報病因。** A 吐出 29 張表，值班的人還要自己推論為什麼；
#     A3 一行講完「fms_app 帶了 BYPASSRLS」。
#   * **A3 在沒有跨租戶資料時仍然會亮。** A 只有在表裡真的有多租戶的列時才
#     測得出來 —— 一個只有單一租戶、或剛 migrate 完還沒 seed 的環境不會。
#     屬性檢查不依賴資料。
#
# 白名單只有 `fms_backup`。任何其他角色（尤其 `fms_app`）拿到 BYPASSRLS，
# 多租戶隔離就整個消失。
A3_BAD=$(q "
  SELECT coalesce(string_agg(rolname, ', ' ORDER BY rolname), '')
    FROM pg_roles
   WHERE rolbypassrls AND NOT rolsuper
     AND rolname NOT IN ('fms_backup');")
if [ -z "$A3_BAD" ]; then
  echo "  [A3] PASS 只有 fms_backup 帶 BYPASSRLS（超級使用者不在此列）"
else
  echo "  [A3] FAIL 這些角色帶了 BYPASSRLS，會完全繞過 RLS：$A3_BAD" >&2
  echo "         BYPASSRLS 繞過的是儲存層，A／A2 的政策檢查看不到它" >&2
  FAILED=1
fi

# --- B. 自行宣稱平台情境（013 硬化） ----------------------------------------
# fms_app 不是 fms_platform 成員，因此 is_platform_context() 必須回 false，
# 政策的 OR 分支不成立，資料仍然看不到。一次 SQL injection 不應足以關閉 RLS。
# 用 SET（不回傳列）而非 SELECT set_config()（會回傳一列），
# 以確保 -Atc 的輸出只有後面那個 SELECT 的單一值
B_CTX=$(q "SET app.is_platform='on'; SELECT fms.is_platform_context();")
B_FAC=$(q "SET app.is_platform='on'; SELECT count(*) FROM fms.facilities;")
if [ "$B_CTX" = "f" ] && [ "$B_FAC" = "0" ]; then
  echo "  [B] PASS 宣稱 app.is_platform='on'：is_platform_context()=f, facilities=0"
else
  echo "  [B] FAIL 013 硬化未生效：is_platform_context()=$B_CTX, facilities=$B_FAC" >&2
  FAILED=1
fi

# --- C. 設定正確 context（反向確認） ---------------------------------------
# 若少了這一案，把政策寫成永遠 false 也會讓 A 與 B 通過。C 確保 RLS 沒有過度阻擋。
C_FAC=$(q "SET app.tenant_id='$DEMO_TENANT'; SELECT count(*) FROM fms.facilities;")
if [ "$C_FAC" -gt 0 ] 2>/dev/null; then
  echo "  [C] PASS 設定示範租戶 context：facilities=$C_FAC"
else
  echo "  [C] FAIL 設定正確 context 後仍查不到資料（facilities=$C_FAC）。" >&2
  echo "         可能是尚未執行 make seed，或政策過度阻擋。" >&2
  FAILED=1
fi

echo ""
if [ "$FAILED" = "0" ]; then
  echo "==> 租戶隔離驗證通過（A 未設 context／B 宣稱平台情境／C 正確 context）"
else
  echo "==> 租戶隔離驗證失敗" >&2
  exit 1
fi
