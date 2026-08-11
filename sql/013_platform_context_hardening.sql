-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 013: 平台情境（platform context）權限硬化
-- =============================================================================
-- 問題
--   001 定義的 fms.is_platform_context() 只檢查 session 變數：
--       coalesce(current_setting('app.is_platform', true), 'off') = 'on'
--   由於任何連線都能執行 SET LOCAL app.is_platform = 'on'，一旦應用層出現
--   SQL injection（或某支腳本誤設此變數），攻擊者即可一行 SQL 關閉整套 RLS
--   並讀取所有租戶資料。這是我方自行設計中最嚴重的一個缺陷。
--
-- 修補
--   平台情境改為「session 變數 AND 目前角色屬於 fms_platform」雙條件。
--   應用連線角色 fms_app 不是 fms_platform 的成員，因此即使變數被設上也無效；
--   只有 migration／支援工具使用的角色（fms_owner）才具備繞過能力。
--
-- 部署順序
--   本檔必須在 001–012 之後執行；執行者需具 CREATEROLE（fms_owner 已具備）。
-- =============================================================================

BEGIN;

-- 1. 平台角色 -----------------------------------------------------------------
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'fms_platform') THEN
    CREATE ROLE fms_platform NOLOGIN;
  END IF;

  -- migration 執行者與支援工具需要平台能力；應用角色刻意不加入
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'fms_owner') THEN
    EXECUTE 'GRANT fms_platform TO fms_owner';
  END IF;

  -- 保險：若 fms_app 曾被誤加入，移除之
  IF EXISTS (
    SELECT 1 FROM pg_auth_members m
    JOIN pg_roles r ON r.oid = m.roleid
    JOIN pg_roles g ON g.oid = m.member
    WHERE r.rolname = 'fms_platform' AND g.rolname = 'fms_app'
  ) THEN
    EXECUTE 'REVOKE fms_platform FROM fms_app';
    RAISE NOTICE 'fms_app 已自 fms_platform 移除（應用角色不得具備平台能力）';
  END IF;
EXCEPTION WHEN insufficient_privilege THEN
  RAISE NOTICE '權限不足，無法建立／授予 fms_platform；請以具 CREATEROLE 的角色手動執行後重跑本檔';
END;
$$;

-- 2. 雙條件判定 ---------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.is_platform_context()
RETURNS boolean
LANGUAGE sql STABLE PARALLEL SAFE
AS $$
  SELECT coalesce(current_setting('app.is_platform', true), 'off') = 'on'
     AND pg_has_role(current_user, 'fms_platform', 'USAGE');
$$;

COMMENT ON FUNCTION fms.is_platform_context() IS
  '平台情境判定：必須同時滿足 (1) 交易內設定 app.is_platform=on 且 (2) 目前角色屬於 fms_platform。'
  '單靠 SET 變數無法繞過 RLS——這是防止 SQL injection 導致跨租戶洩漏的最後一道防線。';

-- 3. set_context 增設防呆 ------------------------------------------------------
-- 非 fms_platform 成員若嘗試要求平台情境，直接拒絕而非默默忽略：
-- 默默忽略會讓維運人員誤以為自己有權限，進而寫出「看起來成功卻查不到資料」的腳本。
CREATE OR REPLACE FUNCTION fms.set_context(
  p_tenant_id   uuid,
  p_user_id     uuid DEFAULT NULL,
  p_is_platform boolean DEFAULT false
) RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
  IF p_is_platform AND NOT pg_has_role(current_user, 'fms_platform', 'USAGE') THEN
    RAISE EXCEPTION '角色 % 不具平台情境權限（需為 fms_platform 成員）', current_user
      USING ERRCODE = '42501', HINT = 'PLATFORM_CONTEXT_DENIED';
  END IF;

  PERFORM set_config('app.tenant_id',   coalesce(p_tenant_id::text, ''), true);
  PERFORM set_config('app.user_id',     coalesce(p_user_id::text, ''),   true);
  PERFORM set_config('app.is_platform', CASE WHEN p_is_platform THEN 'on' ELSE 'off' END, true);
END;
$$;

COMMENT ON FUNCTION fms.set_context(uuid, uuid, boolean) IS
  '設定請求範圍的 tenant／user／platform 情境（交易級）。要求平台情境但角色不符時直接拋錯。';

-- 4. 自我驗證 -----------------------------------------------------------------
-- 若本檔由不具 fms_platform 的角色執行，以下區塊會提醒設定不完整。
DO $$
BEGIN
  IF pg_has_role(current_user, 'fms_platform', 'USAGE') THEN
    RAISE NOTICE '013 OK：目前角色 % 具備平台情境能力', current_user;
  ELSE
    RAISE WARNING '013 注意：目前角色 % 不具 fms_platform，後續 008／009 種子與 010／012 測試將因 RLS 而失敗', current_user;
  END IF;
END;
$$;

COMMIT;
