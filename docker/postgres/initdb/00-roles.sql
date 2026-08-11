-- =============================================================================
-- 資料庫角色初始化（僅在資料卷為空時由 postgres entrypoint 執行一次）
-- =============================================================================
-- 角色分工是多租戶隔離能否成立的前提：
--   fms_owner     擁有 schema 與所有物件、執行 migration；屬 fms_platform
--   fms_app       API 連線角色；非 owner、非 fms_platform → RLS 與 FORCE RLS 完整生效
--   fms_readonly  報表與稽核唯讀
--   fms_platform  平台情境群組（013 之後，只有其成員能繞過 RLS）
--
-- 本檔只建立角色與權限，不含密碼；密碼由隨後的 01-set-passwords.sh 依環境變數設定。
-- 正式環境請改用密鑰管理服務並定期輪替。
-- =============================================================================

\set ON_ERROR_STOP on

-- 平台情境群組（013 會再次確認其存在）
CREATE ROLE fms_platform NOLOGIN;

-- migration／支援工具角色：需要 CREATEROLE 以便 007／013 建立其他角色
CREATE ROLE fms_owner LOGIN CREATEROLE;
GRANT fms_platform TO fms_owner;

-- 應用角色：刻意不給 fms_platform，也不給任何 DDL 權限
CREATE ROLE fms_app LOGIN NOCREATEDB NOCREATEROLE NOSUPERUSER;

-- 唯讀角色
CREATE ROLE fms_readonly LOGIN NOCREATEDB NOCREATEROLE NOSUPERUSER;

-- 資料庫層權限（以 current_database() 取得名稱，避免寫死）
DO $$
DECLARE db text := quote_ident(current_database());
BEGIN
  EXECUTE format('GRANT CONNECT ON DATABASE %s TO fms_owner, fms_app, fms_readonly', db);
  EXECUTE format('GRANT CREATE ON DATABASE %s TO fms_owner', db);
END;
$$;

-- 收緊 public schema：所有應用物件都應建立在 fms schema 內
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA public TO fms_owner, fms_app, fms_readonly;
-- 擴充（ltree／btree_gist／pgcrypto／pg_trgm／citext）安裝於 public，
-- 因此 app 與 readonly 需要 USAGE 才能使用其型別與運算子
GRANT CREATE ON SCHEMA public TO fms_owner;

-- 連線層預設值
ALTER ROLE fms_owner    SET search_path = fms, public;
ALTER ROLE fms_app      SET search_path = fms, public;
ALTER ROLE fms_readonly SET search_path = fms, public;

-- 讓慢查詢與長交易更容易被發現（開發環境設定；正式環境另行調校）
ALTER ROLE fms_app      SET statement_timeout = '30s';
ALTER ROLE fms_app      SET idle_in_transaction_session_timeout = '60s';
ALTER ROLE fms_readonly SET statement_timeout = '120s';

-- pg_stat_statements 供效能測試使用
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;

DO $$
BEGIN
  RAISE NOTICE '角色初始化完成：fms_owner（migration，屬 fms_platform）／fms_app（API，RLS 生效）／fms_readonly（唯讀）';
END;
$$;
