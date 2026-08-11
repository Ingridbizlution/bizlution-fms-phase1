-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 014: 預認證的租戶解析（tenant_code → tenant_id）
-- =============================================================================
-- 補的是一個架構級缺口，不是筆誤。
--
-- api/openapi.yaml 的 TokenRequest 定義 `tenant_code`「password grant 時用來
-- 定位租戶」。但 007 給 fms.tenants 的政策是
--     tenant_self_read: SELECT USING (is_platform_context() OR id = current_tenant_id())
-- 而 013（正確地）讓 fms_app 不屬於 fms_platform，且全 schema 沒有任何
-- SECURITY DEFINER 函式。因此以 fms_app 執行
--     SELECT id FROM fms.tenants WHERE code = $1
-- 永遠回 0 筆 —— 登入在契約定義的形式下無法實作。
--
-- 實測（fms_app）：
--   無 context 查 code='DEMO_GROUP'        → 0 筆
--   自行 SET app.is_platform='on' 後       → 0 筆（013 生效）
--   已知 tenant_id 並設好 context 後查 users → 5 筆
-- 最後一項說明缺口極小：只要 tenant_id 一確定，後續全部走正常 RLS 即可。
-- 因此本檔只開這一個洞，而不是引入新角色或放寬 tenants 的政策。
--
-- 為什麼不用其他做法：
--   * 另建登入角色並授予 fms_platform —— 等於把「繞過 RLS 的能力」放進請求路徑，
--     一次 SQL injection 就能讀取全平台。013 的整個用意就是避免這件事。
--   * 放寬 tenants 的 SELECT 政策 —— 會洩漏租戶清單，可被枚舉。
--   * 要求前端在 /auth/token 也帶 X-Tenant-ID —— 需改契約，且前端通常只知道
--     代碼或子網域而非 UUID。
--
-- 依賴：001（tenants）、007（政策）、013（平台情境硬化）。須在 013 之後執行。
-- =============================================================================

BEGIN;

-- 為什麼單靠 SECURITY DEFINER 不夠：
--   007 對 fms.tenants 下了 FORCE ROW LEVEL SECURITY，而 FORCE 的語意是
--   「政策對表的擁有者也適用」。SECURITY DEFINER 只改變「你是誰」
--   （current_user 變成 fms_owner），不會讓你豁免 RLS。因此若只加
--   SECURITY DEFINER，函式體內仍然被 tenant_self_read 過濾，
--   current_tenant_id() 為 NULL 時一樣回 0 筆。（實測確認過。）
--
-- 因此函式體內臨時開啟平台情境：
--   013 的判定是「session 變數為 on」AND「current_user 屬 fms_platform」。
--   在 SECURITY DEFINER 之下 current_user 是 fms_owner，本身就是 fms_platform
--   成員，因此兩個條件在函式體內同時成立。離開前還原原值。
--
--   （不用函式層級的 `SET app.is_platform = 'on'` 子句：對自訂 GUC 設定
--   函式屬性需要 fms_owner 不具備的權限，會在 CREATE 時就被拒絕。）
--
--   即使不還原也不會外洩：函式外 current_user 回到 fms_app，
--   而 fms_app 不屬於 fms_platform，第二個條件不成立。還原只是保持整潔。
--   這正是 013 雙條件設計的價值 —— 洩漏 session 變數本身不足以繞過 RLS。
--
-- 刻意的收斂：
--   * 只回傳 id，不回傳 name／settings／feature_flags 等任何其他欄位
--   * 只認 status='ACTIVE' 的租戶（停用租戶無法登入）
--   * 參數用 text 對齊 tenants.code 的 varchar(50)，維持精確比對；
--     不引入大小寫不敏感，因為 schema 本身沒有這個語意
--   * 固定 search_path —— SECURITY DEFINER 函式若不固定，呼叫端可用
--     自訂 search_path 誘導函式解析到偽造的物件（CVE-2018-1058 類型）
--   * VOLATILE（非 STABLE）：函式體內呼叫了 volatile 的 set_config
CREATE OR REPLACE FUNCTION fms.resolve_tenant_by_code(p_code text)
RETURNS uuid
LANGUAGE plpgsql
VOLATILE
SECURITY DEFINER
SET search_path = fms, public
AS $$
DECLARE
  v_prev text;
  v_id   uuid;
BEGIN
  v_prev := coalesce(current_setting('app.is_platform', true), 'off');
  PERFORM set_config('app.is_platform', 'on', true);

  SELECT id INTO v_id
  FROM fms.tenants
  WHERE code = p_code
    AND status = 'ACTIVE';

  PERFORM set_config('app.is_platform', v_prev, true);
  RETURN v_id;
END;
$$;

COMMENT ON FUNCTION fms.resolve_tenant_by_code(text) IS
  '預認證用：由租戶代碼解析出 tenant_id，供 password grant 在設定 RLS 情境前定位租戶。'
  ' SECURITY DEFINER + 函式層級 SET app.is_platform，刻意只回傳 ACTIVE 租戶的 id。';

-- 預設 EXECUTE 是 PUBLIC，對 SECURITY DEFINER 函式必須先收回再逐一授予
REVOKE ALL ON FUNCTION fms.resolve_tenant_by_code(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION fms.resolve_tenant_by_code(text) TO fms_app;

-- fms_readonly 不需要登入，因此不授予

COMMIT;

-- =============================================================================
-- 驗證：以 fms_app 呼叫應能取得 id，但仍不能直接讀 tenants
-- =============================================================================
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
    WHERE n.nspname = 'fms' AND p.proname = 'resolve_tenant_by_code' AND p.prosecdef
  ) THEN
    RAISE EXCEPTION '014 FAILED: resolve_tenant_by_code 未建立或未標記 SECURITY DEFINER';
  END IF;
  RAISE NOTICE '014 OK：fms.resolve_tenant_by_code 已建立（SECURITY DEFINER，僅授予 fms_app）';
END;
$$;
