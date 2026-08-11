-- =============================================================================
-- 084  日曆同步服務帳號（ADR-14）
-- =============================================================================
-- 083 的三張表已經就緒，但 `CalendarSyncWatchdog` 是一個背景作業（見
-- `fms_calendar::watchdog`），要寫 `reservations`／`calendar_resource_mappings`／
-- `calendar_integrations`／`calendar_sync_conflicts`——這些都是租戶級、
-- FORCE RLS 的表，背景作業需要一個有角色指派的服務帳號，不能借用某個真人
-- 的 id。與 019（PM 產生器）、078／080（目錄同步／BIM 解析器）同一個手法。
--
-- # 為什麼即使背景寫入不檢查權限碼，還是需要角色指派
--
-- `CalendarSyncWatchdog` 直接下 SQL，不經過 `require_permission()`——
-- 這點與 `directory_sync_watchdog`／`bim-worker` 完全一樣（它們也是純
-- SQL 寫入，沒有呼叫任何權限檢查函式）。但 `begin_tenant_tx()`
-- （`fms-shared/src/db.rs`）**一定會**呼叫 `set_facility_scope()`，
-- 而後者從 `user_accessible_facilities(actor_user_id)` 推導
-- `app.facility_ids`——那張函式看的是 `user_role_assignments` 的
-- **範圍指派**，不是 `role_permissions` 的權限碼。
--
-- 也就是說：沒有角色指派，`user_accessible_facilities()` 回空清單，
-- `app.facility_ids` 變成哨兵值（全零 uuid），007 的 `facility_scope`
-- RESTRICTIVE 政策會濾掉**所有**列——watchdog 對 `reservations`／
-- `bookable_resources` 的每一次讀寫都會靜默地看不到／寫不進任何東西，
-- 且不會有任何錯誤訊息。這不是可以省略的裝飾。
--
-- # 為什麼 scope_level 必須明確設成 TENANT
--
-- `calendar_integration:read`／`write`（083）的 `min_scope_level` 都是
-- `TENANT`。`roles.scope_level` 預設是 `FACILITY`（002），而 026 的
-- `no_role_holds_a_permission_wider_than_its_own_scope` 會擋下「角色範圍
-- 窄於自己持有的權限要求」的組合——上一輪 DIRECTORY_SYNC（078）與
-- BIM_INGEST_WORKER（080）都踩過同一個坑，這裡直接照抄，不重蹈覆轍。
--
-- # 為什麼是專屬角色，權限只給五個
--
-- 與 019／078／080 同一個判斷：最小權限。同步只需要讀寫預約、讀空間節點
-- 與可預約資源做對應、讀寫日曆整合設定；不需要組織/場域/角色相關的
-- 任何權限。借用人類角色會讓「這個 worker 能做什麼」變成要讀程式才知道
-- 的事。
--
-- # 生產環境的佈建注意事項（與 019／078／080 同一句提醒）
--
-- 本檔只為示範租戶建立帳號。**每個租戶都需要自己的服務帳號**
-- （`users.tenant_id` 是 NOT NULL），因此租戶佈建流程必須包含這一步，
-- 否則新租戶設定的日曆整合會永遠不會真的同步，沒有任何錯誤訊息。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

SELECT set_config('app.is_platform', 'on', true);

INSERT INTO fms.roles (tenant_id, code, name, description, is_system, scope_level)
VALUES (NULL, 'CALENDAR_SYNC_WORKER', '日曆同步（服務帳號）',
        '背景作業使用，將 Microsoft 365／Google Workspace 的日曆事件與 FMS 的
         預約雙向同步。僅具讀寫預約、讀空間節點與可預約資源、讀寫日曆整合
         設定的權限。',
        true, 'TENANT')
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code
FROM fms.roles r,
  (VALUES ('reservation:read'), ('reservation:create'),
          ('spatial_node:read'),
          ('calendar_integration:read'), ('calendar_integration:write')) AS c(code)
WHERE r.code = 'CALENDAR_SYNC_WORKER' AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- 示範租戶的服務帳號。`password_hash` 保持 NULL —— 服務帳號不該能以密碼登入。
INSERT INTO fms.users
  (id, tenant_id, username, display_name, user_type, status)
VALUES ('f5000000-0000-4000-8000-000000000004',
        'aaaaaaaa-0000-4000-8000-000000000001',
        'svc.calendar_sync_worker', '日曆同步（系統）', 'SERVICE_ACCOUNT', 'ACTIVE')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.user_role_assignments
  (tenant_id, user_id, role_id, scope_type, scope_id, source)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001',
       'f5000000-0000-4000-8000-000000000004',
       r.id, 'TENANT', NULL, 'SYSTEM'
FROM fms.roles r
WHERE r.code = 'CALENDAR_SYNC_WORKER' AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證：帳號必須看得到場域，否則 RLS 會讓 watchdog 什麼都做不了
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_facilities int;
  v_perms      int;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM fms.users WHERE id = 'f5000000-0000-4000-8000-000000000004') THEN
    RAISE NOTICE '084 SKIPPED: 示範租戶不存在（尚未執行 009）';
    RETURN;
  END IF;

  SELECT count(*) INTO v_facilities
  FROM fms.user_accessible_facilities('f5000000-0000-4000-8000-000000000004');

  SELECT count(*) INTO v_perms
  FROM fms.user_permission_codes_anywhere('f5000000-0000-4000-8000-000000000004');

  IF v_facilities = 0 THEN
    RAISE EXCEPTION
      '084 FAILED: 服務帳號看不到任何場域，facility_scope 政策會濾掉每一列，watchdog 將永遠讀寫不到任何預約';
  END IF;
  IF v_perms <> 5 THEN
    RAISE EXCEPTION '084 FAILED: 服務帳號應該恰好持有 5 個權限，實際 %', v_perms;
  END IF;

  RAISE NOTICE '084 OK: 服務帳號可見 % 個場域、持有 % 個權限', v_facilities, v_perms;
END;
$$;

COMMIT;
