-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 051: 把 fms schema 的讀取權授予 fms_backup
-- =============================================================================
-- `fms_backup`（BYPASSRLS、唯讀）由 `docker/postgres/initdb/02-backup-role.sh`
-- 建立 —— 那裡才有超級使用者，而 **BYPASSRLS 只有超級使用者能給**。
--
-- 但 initdb 跑在 migration **之前**，那時 `fms` schema 還不存在。第一版把
-- schema 授權也寫在 initdb 裡，結果是：
--
--   ERROR: schema "fms" does not exist
--
-- 加上 `ON_ERROR_STOP=1`，整個腳本就此中止 —— 角色建出來了（那幾行在前面），
-- 但**所有授權一行都沒跑**。全新環境於是得到一個「屬性正確、卻連 schema 都
-- 進不去」的備份角色，而 `pg_dump` 吐的是一整面 LOCK TABLE 再說
-- permission denied。
--
-- **本機看不出來**：`make backup-role` 是在 migration 之後跑的，那時 schema
-- 已經在，所以走的是另一條分支。這是「長命的開發資料庫」與「全新環境」
-- 行為分歧的又一次 —— 這個專案已經被同一類問題咬過好幾回（CORE 位置的
-- 自我驗證引用種子資料，五次）。
--
-- -----------------------------------------------------------------------------
-- 切法：照「誰有權做什麼」分，不照「哪個檔案比較方便」分
-- -----------------------------------------------------------------------------
--     BYPASSRLS 屬性      只有超級使用者能給   → initdb
--     fms 的 schema 授權  只有 schema 擁有者能給 → 這裡（以 fms_owner 執行）
--
-- 兩邊因此各自可重跑，也不依賴彼此的執行順序。
--
-- -----------------------------------------------------------------------------
-- ALTER DEFAULT PRIVILEGES 那兩行不是可選的
-- -----------------------------------------------------------------------------
-- 少了它們，**每加一張新表就多一張備份讀不到的表**，而 pg_dump 遇到讀不到的
-- 表會直接失敗 —— 症狀是「某天備份突然壞掉」，而那天離改動很遠。
--
-- 依賴：001（fms schema）、007（RLS 與既有角色授權的慣例）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- 角色必須已經存在。**不在這裡建**：這個 migration 以 fms_owner 執行，
-- 而它給不了 BYPASSRLS —— 建出一個沒有 BYPASSRLS 的 fms_backup 比沒有更糟，
-- 因為它看起來對，而 pg_dump 會被 FORCE RLS 濾掉每一列。
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'fms_backup') THEN
    RAISE EXCEPTION
      '051 FAILED: 角色 fms_backup 不存在。它由 initdb 的 02-backup-role.sh 建立'
      '（BYPASSRLS 需要超級使用者）。既有資料卷請先跑： make backup-role';
  END IF;
END;
$$;

GRANT USAGE ON SCHEMA fms TO fms_backup;
GRANT SELECT ON ALL TABLES IN SCHEMA fms TO fms_backup;
-- pg_dump 會讀序列的目前值。
GRANT SELECT ON ALL SEQUENCES IN SCHEMA fms TO fms_backup;

ALTER DEFAULT PRIVILEGES FOR ROLE fms_owner IN SCHEMA fms
  GRANT SELECT ON TABLES TO fms_backup;
ALTER DEFAULT PRIVILEGES FOR ROLE fms_owner IN SCHEMA fms
  GRANT SELECT ON SEQUENCES TO fms_backup;

COMMENT ON SCHEMA fms IS
  'FMS 應用物件。讀取權：fms_app（RLS 生效）、fms_readonly、'
  'fms_backup（BYPASSRLS，僅供 pg_dump；見 051 與 initdb/02-backup-role.sh）。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_n bigint;
BEGIN
  -- (1) 進得了 schema。這就是 CI 抓到的那一格。
  IF NOT has_schema_privilege('fms_backup', 'fms', 'USAGE') THEN
    RAISE EXCEPTION '051 FAILED: fms_backup 對 schema fms 沒有 USAGE';
  END IF;

  -- (2) 讀得到既有的表。**逐張檢查而不是抽樣**：少一張的後果是 pg_dump 整個
  --     失敗，而「大部分表都讀得到」不構成一份備份。
  SELECT count(*) INTO v_n
    FROM pg_class c
   WHERE c.relnamespace = 'fms'::regnamespace
     AND c.relkind IN ('r', 'p')
     AND NOT has_table_privilege('fms_backup', c.oid, 'SELECT');
  IF v_n > 0 THEN
    RAISE EXCEPTION '051 FAILED: 有 % 張表 fms_backup 讀不到 —— pg_dump 會失敗', v_n;
  END IF;

  -- (3) **日後新建的表也要自動涵蓋。** 這一格保的是「某天備份突然壞掉」——
  --     少了 default privileges，下一個 migration 加的表就不在備份範圍內，
  --     而症狀出現的時間點離原因很遠。
  IF NOT EXISTS (
    SELECT 1 FROM pg_default_acl d
     WHERE d.defaclrole = 'fms_owner'::regrole
       AND d.defaclnamespace = 'fms'::regnamespace
       AND d.defaclobjtype = 'r'
       AND array_to_string(d.defaclacl, ',') LIKE '%fms_backup=r/%'
  ) THEN
    RAISE EXCEPTION
      '051 FAILED: 缺少 TABLES 的 default privileges —— 之後新增的表不會納入備份';
  END IF;

  RAISE NOTICE '051 OK: fms_backup 讀得到 fms schema 的全部表，且涵蓋日後新增的';
END;
$$;

COMMIT;
