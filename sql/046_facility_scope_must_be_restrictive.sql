-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 046: facility_scope 必須是 RESTRICTIVE，以及稽核的場域可見性
-- =============================================================================
-- 這個 migration 修兩件事，其中第一件是 **038 引入的租戶隔離缺陷**。
--
-- -----------------------------------------------------------------------------
-- (1) holiday_calendars 的 facility_scope 是 PERMISSIVE —— 那是個洞
-- -----------------------------------------------------------------------------
-- PostgreSQL 把多條 **PERMISSIVE** 政策以 **OR** 組合、**RESTRICTIVE** 以
-- **AND** 組合。這個 schema 的慣例因此是：
--
--     tenant_isolation  PERMISSIVE   ← 基礎授權
--     facility_scope    RESTRICTIVE  ← 額外收斂
--
-- 27 張表都是這樣。而 038 建 `holiday_calendars` 時用了普通的 `CREATE POLICY`
-- （沒有 `AS RESTRICTIVE`），於是它的 `facility_scope` 是 PERMISSIVE ——
-- 可見性變成 `tenant_isolation OR facility_scope`。
--
-- 而 `facility_scope` 的述詞是 `is_platform_context() OR facility_in_scope(...)`，
-- 其中 `facility_in_scope(NULL)` 回 **true**（021 的定義：`p_facility_id IS NULL`
-- 就通過）。租戶通用的假日 `facility_id` 正是 NULL。
--
-- 結果：**那個 OR 讓 tenant_isolation 完全失效。** 實測（以 `fms_app` 連線、
-- 完全不設租戶情境）：
--
--     holiday_calendars = 2 列（跨 2 個租戶）
--     sla_policies      = 0 列   ← 對照組，RESTRICTIVE，正確
--
-- 也就是說 `holiday_calendars` 在沒有任何情境的連線上就讀得到，
-- 而那正是 FORCE RLS 存在的理由。`check-isolation.sh` 的第一個情境
-- （「未設 context」）就是這一格 —— 只是那個腳本沒有涵蓋這張表。
--
-- -----------------------------------------------------------------------------
-- (2) audit_log：場域受限的讀者不該看到租戶層的稽核列
-- -----------------------------------------------------------------------------
-- `audit_log.facility_scope` **是** RESTRICTIVE（沒有 (1) 的問題），
-- 但它的述詞同樣是 `facility_in_scope(facility_id)`，而 029 稽核的六張表
-- （users／user_role_assignments／roles／role_permissions／
--  identity_providers／tenants）**都沒有場域維度**，因此每一列的
-- `facility_id` 都是 NULL → 那條政策目前什麼都不過濾。
--
-- 今天沒有實際曝露：045 之後只有 PLATFORM_ADMIN 與 TENANT_ADMIN 持有
-- `audit:read`，兩者都是 TENANT 範圍。**危險在於那條政策的存在會誘人把
-- `audit:read` 降成 FACILITY** —— 我自己在上一輪就差點那樣建議，
-- 而攔住我的只是「那個欄位全是 NULL」這個實測，不是政策本身。
--
-- 因此收緊讀取端：場域受限的讀者只看得到**自己場域**的稽核列，
-- 租戶層的（`facility_id IS NULL`）看不到。那讓「日後把 audit:read 降級」
-- 變成一個安全的動作。
--
-- **但寫入端不能收緊。** 一個 ORG 範圍的 `ORG_MANAGER` 指派角色時
-- （`role:assign` 宣告 ORG），觸發器會寫一列 `facility_id IS NULL` 的稽核。
-- 若 RESTRICTIVE 政策的 WITH CHECK 也收緊，那筆 INSERT 會失敗 ——
-- 而 029 的設計是「稽核寫不進去就該讓業務寫入一起失敗」，
-- 也就是**他的角色指派會整個失敗**。
--
-- 解法是讓政策只管 SELECT：`AS RESTRICTIVE FOR SELECT`。
-- INSERT 仍由 `tenant_isolation` 的 WITH CHECK 把關（租戶要對），
-- 而 UPDATE／DELETE 早在 007 就對 `fms_app` REVOKE 掉了。
--
-- 觸發器本身不用改：029 已經寫了 `(v_rec ->> 'facility_id')::uuid`，
-- 因此日後把稽核擴大到有場域維度的表（work_orders／assets）時，
-- 這個欄位會自己填上，而 (2) 的收斂會立刻開始生效。
--
-- 依賴：007（RLS 慣例）、021（facility_in_scope）、029（稽核）、038（行事曆）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) holiday_calendars
-- -----------------------------------------------------------------------------
DROP POLICY IF EXISTS facility_scope ON fms.holiday_calendars;

CREATE POLICY facility_scope ON fms.holiday_calendars
  AS RESTRICTIVE
  USING (fms.is_platform_context() OR fms.facility_in_scope(facility_id))
  WITH CHECK (fms.is_platform_context() OR fms.facility_in_scope(facility_id));

-- -----------------------------------------------------------------------------
-- (2) audit_log
-- -----------------------------------------------------------------------------
DROP POLICY IF EXISTS facility_scope ON fms.audit_log;

-- 只管讀。述詞刻意**不用** `facility_in_scope()`：那支函式對 NULL 一律放行，
-- 而這裡要的正好相反 —— 租戶層的稽核列不屬於任何場域，因此不在場域受限
-- 讀者的範圍內。
CREATE POLICY facility_scope ON fms.audit_log
  AS RESTRICTIVE
  FOR SELECT
  USING (
    fms.is_platform_context()
    -- 讀者不受場域限制（TENANT／平台範圍）→ 全部看得到
    OR fms.current_facility_ids() IS NULL
    -- 場域受限的讀者：只看自己場域的列。NULL（租戶層）不在其中。
    OR facility_id = ANY (fms.current_facility_ids())
  );

COMMENT ON TABLE fms.audit_log IS
  '稽核軌跡。facility_scope 政策只管 SELECT（見 migration 046）：'
  '寫入不能被場域收斂，否則 ORG 範圍的使用者做被稽核的動作時，'
  '稽核列寫不進去會讓他的整個動作失敗（029 的設計）。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_bad text;
  v_n   bigint;
BEGIN
  -- (1) **全 schema 的不變量**：沒有 facility_scope 政策是 PERMISSIVE。
  --
  -- 這一格就是 038 那個缺陷的偵測器。它抓的不是「我這次改對了」，
  -- 而是「以後有人再建一張表時又忘了 AS RESTRICTIVE」。
  SELECT string_agg(c.relname, '、' ORDER BY c.relname) INTO v_bad
    FROM pg_policy p
    JOIN pg_class c ON c.oid = p.polrelid
   WHERE p.polname = 'facility_scope'
     AND p.polpermissive
     AND c.relnamespace = 'fms'::regnamespace;
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION
      '046 FAILED: 這些表的 facility_scope 是 PERMISSIVE（會 OR 掉 tenant_isolation）：%',
      v_bad;
  END IF;

  -- (2) 數量沒有掉。DROP + CREATE 打錯字的話會少一條，而少一條政策的症狀
  --     是「更多東西看得到」—— 沒有任何外顯錯誤。
  SELECT count(*) INTO v_n
    FROM pg_policy p JOIN pg_class c ON c.oid = p.polrelid
   WHERE p.polname = 'facility_scope' AND c.relnamespace = 'fms'::regnamespace;
  IF v_n < 28 THEN
    RAISE EXCEPTION '046 FAILED: facility_scope 政策只剩 % 條（應至少 28）', v_n;
  END IF;

  -- (3) audit_log 的政策只管 SELECT，且沒有 WITH CHECK。
  --     有 WITH CHECK 就會擋掉 ORG 範圍使用者的稽核寫入（見檔頭）。
  IF NOT EXISTS (
    SELECT 1 FROM pg_policy
     WHERE polrelid = 'fms.audit_log'::regclass
       AND polname = 'facility_scope'
       AND NOT polpermissive
       AND polcmd = 'r'            -- 'r' = SELECT
       AND polwithcheck IS NULL
  ) THEN
    RAISE EXCEPTION
      '046 FAILED: audit_log.facility_scope 必須是「RESTRICTIVE FOR SELECT 且無 WITH CHECK」';
  END IF;

  -- (4) 新述詞不再對 NULL 一律放行。直接驗那個語意：
  --     若還在用 facility_in_scope()，這個斷言會失敗。
  IF (SELECT pg_get_expr(polqual, polrelid) FROM pg_policy
       WHERE polrelid = 'fms.audit_log'::regclass AND polname = 'facility_scope')
     LIKE '%facility_in_scope%' THEN
    RAISE EXCEPTION
      '046 FAILED: audit_log 不該用 facility_in_scope() —— 它對 NULL 一律放行，'
      '而租戶層的稽核列不屬於任何場域';
  END IF;

  RAISE NOTICE '046 OK: facility_scope 全部 RESTRICTIVE；稽核的場域可見性已收緊';
END;
$$;

COMMIT;
