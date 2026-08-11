-- =============================================================================
-- 079  Directory sync 服務帳號（示範租戶）
-- =============================================================================
-- 078 建了 `DIRECTORY_SYNC` 角色（平台層，不依賴任何租戶）。本檔是
-- POST_SEED（依賴 009 的示範租戶已存在），與 019（PM 產生器）同一個位置、
-- 同一個手法：只為示範租戶建立服務帳號，生產環境的租戶佈建流程必須
-- 自己重複這一步。
--
-- # 為什麼**必須**有角色指派（不只是使用者列）
--
-- 與 019 同一個理由：`begin_tenant_tx` 會以 `user_accessible_facilities()`
-- 填 `app.facility_ids`，而 007 的 `facility_scope` RESTRICTIVE 政策讀它。
-- 沒有任何角色指派的使用者，可存取場域清單是空的 → RLS 會濾掉每一列，
-- 症狀是排程連線正常、查詢成功、卻永遠回 0 筆，沒有錯誤訊息。
--
-- # 生產環境的佈建注意事項（與 019 同一句提醒）
--
-- 本檔只為示範租戶建立帳號。**每個租戶都需要自己的服務帳號**
-- （`users.tenant_id` 是 NOT NULL），因此租戶佈建流程必須包含這一步，
-- 否則新租戶的排程同步會安靜地不動。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

SELECT set_config('app.is_platform', 'on', true);

-- 示範租戶的服務帳號。`password_hash` 保持 NULL —— 服務帳號不該能以密碼登入。
INSERT INTO fms.users
  (id, tenant_id, username, display_name, user_type, status)
VALUES ('f5000000-0000-4000-8000-000000000002',
        'aaaaaaaa-0000-4000-8000-000000000001',
        'svc.directory_sync', '目錄同步（系統）', 'SERVICE_ACCOUNT', 'ACTIVE')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.user_role_assignments
  (tenant_id, user_id, role_id, scope_type, scope_id, source)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001',
       'f5000000-0000-4000-8000-000000000002',
       r.id, 'TENANT', NULL, 'SYSTEM'
FROM fms.roles r
WHERE r.code = 'DIRECTORY_SYNC' AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證：帳號必須看得到場域，否則 RLS 會讓排程什麼都做不了
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_facilities int;
  v_perms      int;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM fms.users WHERE id = 'f5000000-0000-4000-8000-000000000002') THEN
    RAISE NOTICE '079 SKIPPED: 示範租戶不存在（尚未執行 009）';
    RETURN;
  END IF;

  SELECT count(*) INTO v_facilities
  FROM fms.user_accessible_facilities('f5000000-0000-4000-8000-000000000002');

  SELECT count(*) INTO v_perms
  FROM fms.user_permission_codes_anywhere('f5000000-0000-4000-8000-000000000002');

  IF v_facilities = 0 THEN
    RAISE EXCEPTION
      '079 FAILED: 服務帳號看不到任何場域，facility_scope 政策會濾掉每一列，排程同步將永遠回 0 筆';
  END IF;
  IF v_perms <> 1 THEN
    RAISE EXCEPTION '079 FAILED: 服務帳號應該恰好持有 1 個權限（directory:sync），實際 %', v_perms;
  END IF;

  RAISE NOTICE '079 OK: 服務帳號可見 % 個場域、持有 % 個權限（directory:sync）', v_facilities, v_perms;
END;
$$;

COMMIT;
