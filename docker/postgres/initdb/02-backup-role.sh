#!/bin/sh
# =============================================================================
# fms_backup —— 專用的備份角色（BYPASSRLS，唯讀）
# =============================================================================
# 為什麼需要它：`backup-restore-drill` 發現 `make backup` 從第一天就沒有真的
# 可用過。以 `fms_owner` 跑 `pg_dump` 會失敗 ——
#
#   ERROR: query would be affected by row-level security policy for table "..."
#
# 007 的 `FORCE ROW LEVEL SECURITY` **對擁有者也生效**，而 pg_dump 沒有租戶
# 情境，所以政策把每一列都濾掉。pg_dump 拒絕產出被過濾過的備份（報錯而不是
# 靜默備份空的 —— 那是對的行為）。
#
# 當時的權宜修法是改用超級使用者。**那不該是正式環境的做法**：一個排程執行、
# 憑證放在備份系統裡的身分，不需要（也不該有）建資料庫、改角色、關掉稽核的
# 能力。這個角色把它收斂成剛好夠用。
#
# -----------------------------------------------------------------------------
# 三個刻意的選擇
# -----------------------------------------------------------------------------
# **BYPASSRLS 而不是加進 fms_platform。** 013 之後平台情境需要「GUC + 屬於
# fms_platform」兩個條件。把備份角色放進 fms_platform 等於給它一把能在應用
# 語意層繞過隔離的鑰匙，而它只需要在**儲存層**讀到所有列。BYPASSRLS 是後者，
# 而且它是一個看得見的角色屬性（`\du` 就列得出來），不是一個要追兩層才看得懂
# 的群組成員身分。
#
# **唯讀。** 因此**還原不能用它** —— 那是刻意的不對稱：備份是排程的、無人值守
# 的；還原是有人在場的 DBA 動作。給備份身分寫入權，等於讓一個常駐憑證有能力
# 覆蓋整個資料庫。
#
# **不給 CREATEDB／CREATEROLE／SUPERUSER。**
#
# -----------------------------------------------------------------------------
# 這個檔案同時服務兩種情境
# -----------------------------------------------------------------------------
# * 全新環境：postgres entrypoint 在資料卷為空時自動執行（檔名的 02- 前綴
#   讓它排在角色與密碼之後）。
# * 既有資料卷：`make backup-role` 直接 exec 這同一個檔。
#
# 因此它必須是**幂等**的 —— 而幂等這件事有測試守著（見 make backup-role 的
# 說明與 backup-restore-drill）。寫成兩份（一份給 initdb、一份給 make）會漂移，
# 而漂移的那一天你會發現「開發環境好好的，正式環境沒有這個角色」。
# =============================================================================
set -eu

: "${FMS_BACKUP_PASSWORD:=fms_backup_dev}"
DB="${POSTGRES_DB:-fms}"

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$DB" \
     -v backup_pw="$FMS_BACKUP_PASSWORD" <<'SQL'
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'fms_backup') THEN
    CREATE ROLE fms_backup LOGIN BYPASSRLS NOCREATEDB NOCREATEROLE NOSUPERUSER;
  ELSE
    -- 幂等：角色可能已存在但屬性被改過（例如有人手動建了一個沒有 BYPASSRLS 的）。
    -- 少了 BYPASSRLS 的症狀正是這個角色要解決的那個問題本身。
    ALTER ROLE fms_backup LOGIN BYPASSRLS NOCREATEDB NOCREATEROLE NOSUPERUSER;
  END IF;
END;
$$;

ALTER ROLE fms_backup PASSWORD :'backup_pw';
ALTER ROLE fms_backup SET search_path = fms, public;
-- 備份可能很久。不設 statement_timeout，但設一個上限避免無限期卡住。
ALTER ROLE fms_backup SET statement_timeout = '0';
ALTER ROLE fms_backup SET idle_in_transaction_session_timeout = '30min';

DO $$
DECLARE db text := quote_ident(current_database());
BEGIN
  EXECUTE format('GRANT CONNECT ON DATABASE %s TO fms_backup', db);
END;
$$;

GRANT USAGE ON SCHEMA public TO fms_backup;
-- **這裡不碰 fms schema。**
--
-- initdb 跑在 migration **之前**，那時 `fms` schema 還不存在，而
-- `ALTER DEFAULT PRIVILEGES ... IN SCHEMA fms` 會直接報
-- 「schema "fms" does not exist」—— 加上 ON_ERROR_STOP，整個腳本就此中止，
-- 後面的授權一行都沒跑。實測踩過（CI 的備份演練抓到）。
--
-- 切法因此照「誰有權做什麼」分：
--   BYPASSRLS      → 只有超級使用者能給   → 這裡（initdb）
--   fms 的 schema 授權 → 只有 schema 擁有者能給 → migration 051（以 fms_owner 執行）
--
-- 這也讓兩邊各自可重跑，而且不互相依賴順序。
SQL

echo "fms_backup 已就緒（LOGIN BYPASSRLS，唯讀；還原請用 DBA 身分）"
