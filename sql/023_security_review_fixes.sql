-- =============================================================================
-- 023  獨立安全審查的修正（三項已驗證的缺陷）
-- =============================================================================
-- 這三項都由一次獨立審查找出 —— 審查者沒有看過實作者的推理。
-- 其中兩項是 015／021 自己引入的，而我在寫它們時相信那些防護是有效的。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) HIGH：稽核表重新變回可寫 —— 011 的整批 GRANT 蓋掉了 007 的 REVOKE
-- -----------------------------------------------------------------------------
-- 007 對 audit_log / work_order_transitions / auth_events 撤銷了 fms_app 的
-- UPDATE、DELETE，讓它們是 append-only。011 之後執行
-- `GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA fms TO fms_app`，
-- 把三個撤銷全部蓋掉。
--
-- 值得注意的是 011 自己**知道**這個模式：它在同一個檔案的整批 GRANT 之後
-- 重新 REVOKE 了 `quota_transactions`。只是漏了 007 的那三張。
--
-- 目前沒有 handler 會對這三張表下 UPDATE／DELETE，因此這是「失去的控制」
-- 而非現行漏洞 —— 但那正是用來在應用層被攻破後**限制損害**的控制，
-- 而稽核表的意義就是記錄下手的人無法抹除。
REVOKE UPDATE, DELETE ON fms.audit_log FROM fms_app;
REVOKE UPDATE, DELETE ON fms.work_order_transitions FROM fms_app;
REVOKE UPDATE, DELETE ON fms.auth_events FROM fms_app;

-- -----------------------------------------------------------------------------
-- (2) MEDIUM：021 的跨租戶守衛實際上失效
-- -----------------------------------------------------------------------------
-- 我在 021 寫了「不得跨租戶查詢」的守衛，條件是 `IF NOT is_platform_context()`。
-- 那是錯的：在 SECURITY DEFINER 函式內 `current_user` 已經是 `fms_owner`，
-- 而它**是** fms_platform 的成員，因此 013 的雙條件裡的角色那一半恆為真，
-- 整個守衛塌縮成「呼叫者有沒有設那個 GUC」—— 而任何 fms_app 連線
-- 一行 `set_config` 就能設。
--
-- 審查者實測確認可以用另一個租戶的 user_id 取得該租戶的場域 id。
--
-- 修法：改用 `session_user`。SECURITY DEFINER 不會改變 session_user，
-- 它始終是登入的角色（fms_app），因此角色條件這次真的有意義。
-- 背景作業以 fms_owner 登入，session_user 就是 fms_owner，豁免仍然成立。
CREATE OR REPLACE FUNCTION fms.user_accessible_facilities(p_user_id uuid)
RETURNS TABLE (facility_id uuid)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = fms, public
AS $$
DECLARE
  v_saved text := coalesce(current_setting('app.is_platform', true), 'off');
BEGIN
  -- 守衛：不得跨租戶查詢。
  --
  -- 用 `session_user` 而非 `current_user`：SECURITY DEFINER 會把
  -- current_user 換成函式擁有者（fms_owner，fms_platform 成員），
  -- 因此以 current_user 判斷等於恆為真。session_user 是登入身分，
  -- 不受 DEFINER 影響 —— 這是本守衛唯一能成立的依據。
  IF NOT pg_has_role(session_user, 'fms_platform', 'MEMBER') THEN
    IF NOT EXISTS (
      SELECT 1 FROM fms.users u
       WHERE u.id = p_user_id
         AND u.tenant_id = fms.current_tenant_id()
    ) THEN
      RETURN;
    END IF;
  END IF;

  PERFORM set_config('app.is_platform', 'on', true);

  RETURN QUERY
  SELECT DISTINCT f.id
  FROM fms.facilities f
  JOIN fms.organizations o ON o.id = f.org_id
  WHERE f.deleted_at IS NULL
    AND EXISTS (
      SELECT 1
      FROM fms.user_role_assignments ura
      LEFT JOIN fms.organizations os ON os.id = ura.scope_id
      WHERE ura.user_id = p_user_id
        AND ura.tenant_id = f.tenant_id
        AND (ura.valid_until IS NULL OR ura.valid_until > clock_timestamp())
        AND (
              ura.scope_type = 'TENANT'
          OR (ura.scope_type = 'FACILITY' AND ura.scope_id = f.id)
          OR (ura.scope_type = 'ORG' AND os.org_path IS NOT NULL
              AND o.org_path OPERATOR(public.<@) os.org_path)
        )
    );

  PERFORM set_config('app.is_platform', v_saved, true);
END;
$$;

REVOKE ALL ON FUNCTION fms.user_accessible_facilities(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION fms.user_accessible_facilities(uuid) TO fms_app, fms_readonly;

-- -----------------------------------------------------------------------------
-- (3) MEDIUM：015 建的 catalog 沒有 RLS，且 fms_app 可寫
-- -----------------------------------------------------------------------------
-- 015 只 `GRANT SELECT`，但 007 的 `ALTER DEFAULT PRIVILEGES` 早就對
-- **後續建立的每一張表**授予了完整 DML —— 明確的 GRANT SELECT 不會收回它。
-- 而 015 沒有 ENABLE ROW LEVEL SECURITY，因此它是 fms schema 裡唯一
-- 沒有 RLS 的表。
--
-- 這張表是全租戶共用的，因此竄改天生就是跨租戶的：它決定
-- `available-actions` 的標籤與 `is_destructive` 旗標。
--
-- 其他無租戶的 catalog（work_order_statuses、permissions）都有
-- 平台情境的管理政策，這一張只是被漏掉。照同一個模式補上。
ALTER TABLE fms.work_order_actions ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.work_order_actions FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS work_order_actions_read ON fms.work_order_actions;
CREATE POLICY work_order_actions_read ON fms.work_order_actions
  FOR SELECT USING (true);

DROP POLICY IF EXISTS work_order_actions_admin ON fms.work_order_actions;
CREATE POLICY work_order_actions_admin ON fms.work_order_actions
  FOR ALL USING (fms.is_platform_context()) WITH CHECK (fms.is_platform_context());

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE v_bad int;
BEGIN
  SELECT count(*) INTO v_bad
  FROM information_schema.role_table_grants
  WHERE table_schema = 'fms' AND grantee = 'fms_app'
    AND table_name IN ('audit_log','work_order_transitions','auth_events')
    AND privilege_type IN ('UPDATE','DELETE');
  IF v_bad > 0 THEN
    RAISE EXCEPTION '023 FAILED: 稽核表仍有 % 項 UPDATE/DELETE 授權', v_bad;
  END IF;

  IF NOT EXISTS (
    SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname='fms' AND c.relname='work_order_actions'
       AND c.relrowsecurity AND c.relforcerowsecurity
  ) THEN
    RAISE EXCEPTION '023 FAILED: work_order_actions 未啟用 FORCE RLS';
  END IF;

  IF (SELECT prosrc FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
       WHERE n.nspname='fms' AND p.proname='user_accessible_facilities')
     NOT LIKE '%session_user%' THEN
    RAISE EXCEPTION '023 FAILED: 跨租戶守衛沒有改用 session_user';
  END IF;

  RAISE NOTICE '023 OK: 稽核表恢復 append-only、跨租戶守衛改用 session_user、catalog 已上 RLS';
END;
$$;

COMMIT;
