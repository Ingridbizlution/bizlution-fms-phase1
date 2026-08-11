-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 040: 假日行事曆可由管理者維護
-- =============================================================================
-- 038 建了 `holiday_calendars`，而**只有 migration 能寫它** —— 那正是
-- `sla_policies` 在 037 之前的狀態，也正是「可以讓管理者定義的條件不要寫死」
-- 要避免的那件事。一張只有工程師能填的假日表，等於把每年的行事曆
-- 變成一次部署。
--
-- 權限的形狀與 037 完全相同，因為問題完全相同：
--
--   * **自己的碼，不沿用 `sla_policy:write`。** 「加一個國定假日」與
--     「改 SLA 承諾的分鐘數」是不同的事，共用一個碼會讓授權變成全有全無。
--     但兩者對期限的影響一樣大，因此 `is_dangerous = true`。
--   * `min_scope_level = FACILITY`：某棟樓可能有自己的休館日，而
--     `business_windows` 的解析順序刻意讓場域專屬的優先。
--   * **租戶通用的假日（`facility_id IS NULL`）額外要求 TENANT 範圍**，
--     由應用層擋 —— 它影響每一個場域的每一張工單。
--
-- 依賴：038（holiday_calendars）、026（min_scope_level 生效）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous)
VALUES
  ('holiday:read',  'holiday', 'read',  'CORE',
   '查詢假日與補班日', 'FACILITY', false),
  ('holiday:write', 'holiday', 'write', 'CORE',
   '維護假日與補班日（會改變之後開立工單的 SLA 期限）', 'FACILITY', true)
ON CONFLICT (code) DO UPDATE
  SET resource = EXCLUDED.resource,
      action = EXCLUDED.action,
      module = EXCLUDED.module,
      description = EXCLUDED.description,
      min_scope_level = EXCLUDED.min_scope_level,
      is_dangerous = EXCLUDED.is_dangerous;

-- 自己補列（027 檔頭記過：008 的萬用 INSERT 不會因為新增權限碼而重跑）。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code
FROM fms.roles r,
     (VALUES ('holiday:read'), ('holiday:write')) AS c(code)
WHERE r.code IN ('PLATFORM_ADMIN', 'TENANT_ADMIN', 'FACILITY_ADMIN')
ON CONFLICT DO NOTHING;

-- 讀取放寬：知道下週一放不放假，是派工與預估的前提。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'holiday:read'
FROM fms.roles r
WHERE r.code IN ('ORG_MANAGER', 'MAINTENANCE_SUPERVISOR', 'TECHNICIAN', 'VIEWER')
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE v_n bigint;
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM fms.permissions
     WHERE code = 'holiday:write' AND min_scope_level = 'FACILITY' AND is_dangerous
  ) THEN
    RAISE EXCEPTION '040 FAILED: holiday:write 應宣告 FACILITY 範圍且標為 dangerous';
  END IF;

  -- 有人持有它 —— 027 記過的症狀：新碼有名字、沒有任何人持有。
  SELECT count(DISTINCT r.code) INTO v_n
    FROM fms.roles r
    JOIN fms.role_permissions rp ON rp.role_id = r.id
   WHERE rp.permission_code = 'holiday:write';
  IF v_n < 3 THEN
    RAISE EXCEPTION '040 FAILED: holiday:write 只有 % 個角色持有', v_n;
  END IF;

  RAISE NOTICE '040 OK: holiday 權限就緒';
END;
$$;

COMMIT;
