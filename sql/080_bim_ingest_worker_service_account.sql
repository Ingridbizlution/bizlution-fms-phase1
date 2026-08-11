-- =============================================================================
-- 080  BIM 匯入服務帳號
-- =============================================================================
-- `bim_models`／`spatial_nodes`／`assets` 的 schema 從 003 就準備好了，但
-- 從來沒有解析器把 IFC 檔案變成資料 —— `bim_models.status` 永遠停在
-- `UPLOADED`。ADR-09 已經定案這塊由 Python（IfcOpenShell）負責，透過
-- `bim_models`/outbox 解耦。本檔是那個 worker 的寫入身分，與 019
-- （PM 產生器）、078（目錄同步）同一個手法：解析器需要寫
-- `spatial_nodes`／`assets`，而這些表都是租戶級、FORCE RLS，背景作業
-- 需要一個有 TENANT 範圍角色指派的服務帳號，不能借用某個真人的 id。
--
-- # 為什麼 scope_level 必須明確設成 TENANT
--
-- `asset_model:read` 的 `min_scope_level` 是 TENANT（008）——解析器要拿它
-- 去比對設備型號。`roles.scope_level` 預設是 FACILITY（002），而 026 的
-- `no_role_holds_a_permission_wider_than_its_own_scope` 會擋下「角色範圍
-- 窄於自己持有的權限要求」的組合。上一輪 DIRECTORY_SYNC（078）已經踩過
-- 這個坑一次，這裡直接照抄，不重蹈覆轍。
--
-- # 為什麼是專屬角色，權限只給七個
--
-- 與 019／078 同一個判斷：最小權限。解析器只需要讀寫空間節點、設備、
-- BIM 模型本身，以及讀設備型錄做比對；不需要組織/場域/角色相關的任何
-- 權限。借用人類角色會讓「解析器能做什麼」變成要讀程式才知道的事。
--
-- # 生產環境的佈建注意事項（與 019／078 同一句提醒）
--
-- 本檔只為示範租戶建立帳號。**每個租戶都需要自己的服務帳號**
-- （`users.tenant_id` 是 NOT NULL），因此租戶佈建流程必須包含這一步，
-- 否則新租戶上傳的 BIM 模型會永遠停在 UPLOADED，沒有任何錯誤訊息。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

SELECT set_config('app.is_platform', 'on', true);

INSERT INTO fms.roles (tenant_id, code, name, description, is_system, scope_level)
VALUES (NULL, 'BIM_INGEST_WORKER', 'BIM 匯入解析器（服務帳號）',
        '背景作業使用，將上傳的 BIM 模型解析為樓層/空間/設備。僅具讀寫空間節點、
         設備、BIM 模型與讀取設備型錄的權限。',
        true, 'TENANT')
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code
FROM fms.roles r,
  (VALUES ('spatial_node:read'), ('spatial_node:write'),
          ('asset:read'), ('asset:write'),
          ('bim_model:read'), ('bim_model:write'),
          ('asset_model:read')) AS c(code)
WHERE r.code = 'BIM_INGEST_WORKER' AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- 示範租戶的服務帳號。`password_hash` 保持 NULL —— 服務帳號不該能以密碼登入。
INSERT INTO fms.users
  (id, tenant_id, username, display_name, user_type, status)
VALUES ('f5000000-0000-4000-8000-000000000003',
        'aaaaaaaa-0000-4000-8000-000000000001',
        'svc.bim_ingest_worker', 'BIM 匯入解析器（系統）', 'SERVICE_ACCOUNT', 'ACTIVE')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.user_role_assignments
  (tenant_id, user_id, role_id, scope_type, scope_id, source)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001',
       'f5000000-0000-4000-8000-000000000003',
       r.id, 'TENANT', NULL, 'SYSTEM'
FROM fms.roles r
WHERE r.code = 'BIM_INGEST_WORKER' AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證：帳號必須看得到場域，否則 RLS 會讓解析器什麼都做不了
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_facilities int;
  v_perms      int;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM fms.users WHERE id = 'f5000000-0000-4000-8000-000000000003') THEN
    RAISE NOTICE '080 SKIPPED: 示範租戶不存在（尚未執行 009）';
    RETURN;
  END IF;

  SELECT count(*) INTO v_facilities
  FROM fms.user_accessible_facilities('f5000000-0000-4000-8000-000000000003');

  SELECT count(*) INTO v_perms
  FROM fms.user_permission_codes_anywhere('f5000000-0000-4000-8000-000000000003');

  IF v_facilities = 0 THEN
    RAISE EXCEPTION
      '080 FAILED: 服務帳號看不到任何場域，facility_scope 政策會濾掉每一列，解析器將永遠回 0 筆';
  END IF;
  IF v_perms <> 7 THEN
    RAISE EXCEPTION '080 FAILED: 服務帳號應該恰好持有 7 個權限，實際 %', v_perms;
  END IF;

  RAISE NOTICE '080 OK: 服務帳號可見 % 個場域、持有 % 個權限', v_facilities, v_perms;
END;
$$;

COMMIT;
