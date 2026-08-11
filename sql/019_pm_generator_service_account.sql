-- =============================================================================
-- 019  PM generator service account
-- =============================================================================
-- PM 產生器需要一個身分來寫入：`work_orders.created_by` 有外鍵，
-- 而 `fms.set_context()` 也需要 user_id。002 早就支援
-- `user_type = 'SERVICE_ACCOUNT'`，只是沒有人建過帳號。
--
-- # 為什麼**必須**有角色指派（不只是使用者列）
--
-- 這不是「順手給權限」。`begin_tenant_tx` 會以
-- `fms.user_accessible_facilities()` 填 `app.facility_ids`，而 007 的
-- `facility_scope` RESTRICTIVE 政策讀它。沒有任何角色指派的使用者，
-- 可存取場域清單是空的 → 應用層填入全零 uuid 哨兵 →
-- **RLS 會濾掉每一列**。症狀是產生器連線正常、查詢成功、卻永遠回 0 筆，
-- 而且沒有錯誤訊息。
--
-- 因此服務帳號需要一個 **TENANT 範圍**的指派。
--
-- # 為什麼是專屬角色而不是借用 MAINTENANCE_SUPERVISOR
--
-- 最小權限：新增 `PM_GENERATOR` 角色，只給產生器實際用到的三個權限。
-- 借用人類角色會讓「產生器能做什麼」變成一個要靠讀程式才知道的問題，
-- 而且日後調整那個人類角色會意外改變背景作業的能力。
--
-- # 生產環境的佈建注意事項
--
-- 本檔只為示範租戶建立帳號。**每個租戶都需要自己的服務帳號**
-- （`users.tenant_id` 是 NOT NULL），因此租戶佈建流程必須包含這一步，
-- 否則新租戶的 PM 產生器會安靜地不動。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

SELECT set_config('app.is_platform', 'on', true);

-- 平台層角色（tenant_id IS NULL，所有租戶共用）
INSERT INTO fms.roles (tenant_id, code, name, description, is_system)
VALUES (NULL, 'PM_GENERATOR', 'PM 產生器（服務帳號）',
        '預防性維護產生器背景作業使用。僅具讀取計畫／設備與建立工單的權限。',
        true)
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code
FROM fms.roles r,
  (VALUES ('maintenance_plan:read'), ('asset:read'), ('work_order:create')) AS c(code)
WHERE r.code = 'PM_GENERATOR' AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- 示範租戶的服務帳號。`password_hash` 保持 NULL —— 服務帳號不該能以密碼登入。
INSERT INTO fms.users
  (id, tenant_id, username, display_name, user_type, status)
VALUES ('f5000000-0000-4000-8000-000000000001',
        'aaaaaaaa-0000-4000-8000-000000000001',
        'svc.pm_generator', 'PM 產生器（系統）', 'SERVICE_ACCOUNT', 'ACTIVE')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.user_role_assignments
  (tenant_id, user_id, role_id, scope_type, scope_id, source)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001',
       'f5000000-0000-4000-8000-000000000001',
       r.id, 'TENANT', NULL, 'SYSTEM'
FROM fms.roles r
WHERE r.code = 'PM_GENERATOR' AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證：帳號必須看得到場域，否則 RLS 會讓產生器什麼都做不了
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_facilities int;
  v_perms      int;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM fms.users WHERE id = 'f5000000-0000-4000-8000-000000000001') THEN
    RAISE NOTICE '019 SKIPPED: 示範租戶不存在（尚未執行 009）';
    RETURN;
  END IF;

  SELECT count(*) INTO v_facilities
  FROM fms.user_accessible_facilities('f5000000-0000-4000-8000-000000000001');

  SELECT count(*) INTO v_perms
  FROM fms.user_permission_codes_anywhere('f5000000-0000-4000-8000-000000000001');

  IF v_facilities = 0 THEN
    RAISE EXCEPTION
      '019 FAILED: 服務帳號看不到任何場域，facility_scope 政策會濾掉每一列，產生器將永遠回 0 筆';
  END IF;
  IF v_perms < 3 THEN
    RAISE EXCEPTION '019 FAILED: 服務帳號只有 % 個權限，預期至少 3 個', v_perms;
  END IF;

  RAISE NOTICE '019 OK: 服務帳號可見 % 個場域、持有 % 個權限', v_facilities, v_perms;
END;
$$;

COMMIT;
