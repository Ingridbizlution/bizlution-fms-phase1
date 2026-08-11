-- =============================================================================
-- 021  user_accessible_facilities 改為 SECURITY DEFINER
-- =============================================================================
-- # 症狀
--
-- 建立場域後，即使在同一交易內重算 `app.facility_ids`，建立者仍然看不到
-- 那個新場域 —— 連讀回自己剛寫入的那一列都被 RLS 擋掉。
--
-- # 原因：授權來源被它所授權的東西過濾
--
-- `fms.user_accessible_facilities()` 的實作會讀 `fms.facilities`：
--
--     SELECT DISTINCT f.id FROM fms.facilities f JOIN fms.organizations o ...
--
-- 而 `fms.facilities` 有 RESTRICTIVE 政策 `facility_scope`，判定式是
-- `facility_in_scope(id)`，讀的是 `app.facility_ids`。
--
-- 也就是說：**用來計算可見場域清單的函式，本身被那份清單過濾**。
-- 新場域不在舊清單裡 → 函式看不到它 → 重算出來的清單還是沒有它 →
-- 永遠看不到。這是結構性的循環，不是快照過期。
--
-- # 修法
--
-- 讓它 `SECURITY DEFINER`：授權的**權威來源**不該受它所授權的可見性限制。
--
-- **但 DEFINER 本身不夠。** 所有租戶表都是 `FORCE ROW LEVEL SECURITY`，
-- 那代表**連表的擁有者都受政策約束**。DEFINER 只是把 `current_user`
-- 換成 `fms_owner`，政策照樣套用，循環依然存在。
--
-- 因此還要在函式內**暫時取得平台情境**：`fms_owner` 是 `fms_platform`
-- 的成員，所以在函式內把 `app.is_platform` 設為 `on` 就能讓
-- `is_platform_context()` 成立（013 的雙條件：GUC + 角色成員身分）。
-- 離開前必須還原，否則呼叫者會意外繼承平台情境。
--
-- 這與 014 的 `resolve_tenant_by_code` 是完全相同的手法與相同的理由 ——
-- 那次的教訓（DEFINER 在 FORCE RLS 下不足）在這裡再次適用。
--
-- # 安全性
--
-- 函式本身**不因此變得寬鬆**：它回傳的場域仍然只有
-- 「與該使用者的角色指派同租戶」的那些（`ura.tenant_id = f.tenant_id`），
-- 這個限制來自資料而非 RLS，因此 DEFINER 不會放寬它。
--
-- 但 DEFINER 會讓呼叫者能查詢**任意** user_id 的可見場域。因此加一道
-- 明確的守衛：只允許查詢當前租戶的使用者（或在平台情境下不限）。
-- 沒有這道守衛，一個租戶可以用猜到的 user_id 探測另一個租戶的場域數量。
--
-- `SET search_path` 是 DEFINER 函式的必要防護：不設會讓呼叫者
-- 用自己的 search_path 劫持未限定的名稱。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

CREATE OR REPLACE FUNCTION fms.user_accessible_facilities(p_user_id uuid)
RETURNS TABLE (facility_id uuid)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = fms, public
AS $$
DECLARE
  v_saved text := coalesce(current_setting('app.is_platform', true), 'off');
BEGIN
  -- 守衛：不得跨租戶查詢。平台情境（含遷移、背景作業）不受限。
  --
  -- 順序重要：這一段必須在取得平台情境**之前**執行，
  -- 否則 `current_tenant_id()` 的比對會在已經旁通政策的情境下進行，
  -- 守衛就形同虛設。
  IF NOT fms.is_platform_context() THEN
    IF NOT EXISTS (
      SELECT 1 FROM fms.users u
       WHERE u.id = p_user_id
         AND u.tenant_id = fms.current_tenant_id()
    ) THEN
      RETURN;
    END IF;
  END IF;

  -- 取得平台情境以看穿 facility_scope 政策（見檔頭說明）。
  -- 只在本函式內有效：離開前還原。
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

COMMENT ON FUNCTION fms.user_accessible_facilities IS
  'Authoritative facility scope for a user. SECURITY DEFINER because it must not be filtered by the facility_scope RLS policy it feeds — otherwise a newly created facility can never enter the allow-list. Guarded against cross-tenant probing.';

-- DEFINER 函式的執行權限要收斂：只給執行期角色，不給 PUBLIC。
REVOKE ALL ON FUNCTION fms.user_accessible_facilities(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION fms.user_accessible_facilities(uuid) TO fms_app, fms_readonly;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_kind  text;
  v_count int;
BEGIN
  SELECT CASE WHEN p.prosecdef THEN 'DEFINER' ELSE 'INVOKER' END INTO v_kind
  FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
  WHERE n.nspname = 'fms' AND p.proname = 'user_accessible_facilities';

  IF v_kind <> 'DEFINER' THEN
    RAISE EXCEPTION '021 FAILED: 函式不是 SECURITY DEFINER，循環未解';
  END IF;

  -- search_path 必須被固定，否則 DEFINER 函式可被呼叫者劫持
  IF NOT EXISTS (
    SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = 'fms' AND p.proname = 'user_accessible_facilities'
       AND p.proconfig IS NOT NULL
       AND EXISTS (SELECT 1 FROM unnest(p.proconfig) c WHERE c LIKE 'search_path=%')
  ) THEN
    RAISE EXCEPTION '021 FAILED: DEFINER 函式沒有固定 search_path';
  END IF;

  -- 示範租戶的租戶管理員應看得到兩個場域（行為未因改寫而變）
  PERFORM set_config('app.is_platform', 'on', true);
  SELECT count(*) INTO v_count
  FROM fms.user_accessible_facilities('ffffffff-0000-4000-8000-000000000001');
  IF v_count < 2 THEN
    RAISE NOTICE '021: 示範資料不足（可見場域 %），略過行為斷言', v_count;
  END IF;

  -- 情境必須被還原：函式內設了 'on'，回來後不該還是 'on'
  PERFORM set_config('app.is_platform', 'off', true);
  PERFORM fms.user_accessible_facilities('ffffffff-0000-4000-8000-000000000001');
  IF coalesce(current_setting('app.is_platform', true), 'off') <> 'off' THEN
    RAISE EXCEPTION '021 FAILED: 函式沒有還原 app.is_platform，呼叫者會意外繼承平台情境';
  END IF;

  RAISE NOTICE '021 OK: DEFINER + 內部平台情境已套用且會還原、search_path 已固定（管理員可見 % 個場域）', v_count;
END;
$$;

COMMIT;
