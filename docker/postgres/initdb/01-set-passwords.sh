#!/bin/sh
# =============================================================================
# 依環境變數設定角色密碼（在 00-roles.sql 之後執行）
# =============================================================================
# 之所以分成兩個檔案：postgres entrypoint 執行 .sql 時不會傳入 psql 變數，
# 只有 .sh 能讀到環境變數。密碼因此在此設定，而非寫死在 SQL 裡。
#
# ALTER ROLE ... PASSWORD 會出現在伺服器日誌中，開發環境可接受；
# 正式環境請以密鑰管理服務注入並關閉 log_statement。
# =============================================================================
set -eu

: "${FMS_OWNER_PASSWORD:=fms_owner_dev}"
: "${FMS_APP_PASSWORD:=fms_app_dev}"
: "${FMS_READONLY_PASSWORD:=fms_readonly_dev}"

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
     -v owner_pw="$FMS_OWNER_PASSWORD" \
     -v app_pw="$FMS_APP_PASSWORD" \
     -v ro_pw="$FMS_READONLY_PASSWORD" <<'SQL'
ALTER ROLE fms_owner    PASSWORD :'owner_pw';
ALTER ROLE fms_app      PASSWORD :'app_pw';
ALTER ROLE fms_readonly PASSWORD :'ro_pw';
SQL

echo "角色密碼已設定（fms_owner / fms_app / fms_readonly）"
