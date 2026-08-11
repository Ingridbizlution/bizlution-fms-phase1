-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 027: 拆 facility:write 成 facility:create 與 facility:update
-- =============================================================================
-- 026 讓 min_scope_level 開始生效，隨即暴露一個 008 目錄本身的問題：
-- 一個權限碼同時管兩種層級的動作。
--
--   * **建立**場域是租戶／組織級動作：新場域沒有父場域，「在哪個場域裡建立
--     一個場域」不成立。
--   * **修改**場域是場域級動作：範圍就是那一個場域。
--
-- 一個碼只能填一個 min_scope_level，因此無論填什麼都必然對其中一邊是錯的。
-- 008 填的是 FACILITY，於是「建立」那一邊沒有守衛。
--
-- 目前 POST /facilities 之所以還是回 403，是因為 007 的 facility_scope
-- RESTRICTIVE 政策：新場域的 id 不在交易開始時取的可見快照裡，
-- create_facility 重算後仍讀不回來就回滾。行為是對的，但那是 RLS 的副產品
-- ——任何人日後調整那條政策，這個保護就無聲失效，而錯誤訊息也一直是
-- 「你的範圍不涵蓋它」而不是「你沒有權限建立場域」。
--
-- 拆開之後，那個 403 由權限判定給出，而 RLS 退回它該扮演的角色（第二道防線）。
--
-- 不是契約變更：api/openapi.yaml 完全沒有提到權限碼（實測 0 處），
-- 因此 ADR-09 紀律 1 的方向性不受影響。要同步的是 api/ENDPOINTS.md。
--
-- 依賴：008（facility:write 與角色對應）、026（min_scope_level 已生效）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 新增兩個權限碼
-- -----------------------------------------------------------------------------
-- facility:create 宣告 ORG 而非 TENANT：與 026 對 organization:write 的處理
-- 一致 —— 組織經理在自己的組織子樹內建立場域是合理的，而 008 本來就把
-- facility:write 給了 ORG_MANAGER。
INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous)
VALUES
  ('facility:create', 'facility', 'create', 'CORE', '建立設施（租戶／組織級動作）', 'ORG',      false),
  ('facility:update', 'facility', 'update', 'CORE', '維護既有設施資料',              'FACILITY', false)
ON CONFLICT (code) DO UPDATE
  SET resource = EXCLUDED.resource,
      action = EXCLUDED.action,
      module = EXCLUDED.module,
      description = EXCLUDED.description,
      min_scope_level = EXCLUDED.min_scope_level;

-- -----------------------------------------------------------------------------
-- (2) 角色對應
-- -----------------------------------------------------------------------------
-- **必須自己補列。** 008 給 PLATFORM_ADMIN 的是「全部權限」（第 118 行）、
-- 給 TENANT_ADMIN 的是「除 user:impersonate 以外的全部」（第 124 行），
-- 而那兩個萬用 INSERT 不會因為後來新增權限碼而重跑。只靠它們的話，
-- 新碼會有名字而沒有任何人持有 —— 症狀是「管理員突然不能建場域」。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code
FROM fms.roles r,
     (VALUES ('facility:create'), ('facility:update')) AS c(code)
WHERE r.code IN ('PLATFORM_ADMIN', 'TENANT_ADMIN', 'ORG_MANAGER')
ON CONFLICT DO NOTHING;

-- FACILITY_ADMIN 只拿 update：他管的是既有場域，不是新增場域。
-- 這正是拆碼要換來的區別。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'facility:update'
FROM fms.roles r
WHERE r.code = 'FACILITY_ADMIN'
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- (3) 移除舊碼
-- -----------------------------------------------------------------------------
-- role_permissions.permission_code 對 permissions.code 是 ON DELETE CASCADE，
-- 因此刪除權限碼會連帶清掉所有角色的授權列，包含客戶自訂角色的。
-- 這是刻意的：留下一個沒有任何程式讀取的碼，只會讓下一個人以為它還有效。
--
-- work_order_transitions_allowed.required_permission 也參照 permissions.code，
-- 但那是 NO ACTION 的外鍵 —— 若有任何狀態機規則引用 facility:write，
-- 這行會失敗而不是靜默破壞規則。實測目前沒有引用，
-- 而讓它以外鍵違反的形式失敗比事後發現規則失效好。
DELETE FROM fms.permissions WHERE code = 'facility:write';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE v_bad text;
BEGIN
  IF EXISTS (SELECT 1 FROM fms.permissions WHERE code = 'facility:write') THEN
    RAISE EXCEPTION '027 FAILED: facility:write 仍然存在';
  END IF;

  SELECT string_agg(code || '=' || min_scope_level, ', ' ORDER BY code) INTO v_bad
    FROM fms.permissions
   WHERE (code = 'facility:create' AND min_scope_level <> 'ORG')
      OR (code = 'facility:update' AND min_scope_level <> 'FACILITY');
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION '027 FAILED: 新碼的宣告層級不對：%', v_bad;
  END IF;

  -- FACILITY_ADMIN 必須有 update 而沒有 create —— 這是本檔的重點，
  -- 只斷言權限碼存在會完全漏掉它。
  IF NOT EXISTS (
    SELECT 1 FROM fms.roles r JOIN fms.role_permissions rp ON rp.role_id = r.id
     WHERE r.code = 'FACILITY_ADMIN' AND rp.permission_code = 'facility:update'
  ) THEN
    RAISE EXCEPTION '027 FAILED: FACILITY_ADMIN 沒有 facility:update';
  END IF;
  IF EXISTS (
    SELECT 1 FROM fms.roles r JOIN fms.role_permissions rp ON rp.role_id = r.id
     WHERE r.code = 'FACILITY_ADMIN' AND rp.permission_code = 'facility:create'
  ) THEN
    RAISE EXCEPTION '027 FAILED: FACILITY_ADMIN 不該持有 facility:create';
  END IF;

  -- 三個較寬的角色都要有 create，否則沒有人建得了場域
  SELECT string_agg(want.code, ', ') INTO v_bad
    FROM (VALUES ('PLATFORM_ADMIN'),('TENANT_ADMIN'),('ORG_MANAGER')) AS want(code)
   WHERE NOT EXISTS (
     SELECT 1 FROM fms.roles r JOIN fms.role_permissions rp ON rp.role_id = r.id
      WHERE r.code = want.code AND rp.permission_code = 'facility:create'
   );
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION '027 FAILED: 這些角色缺少 facility:create：%（008 的萬用授權不會重跑）', v_bad;
  END IF;

  RAISE NOTICE '027 OK: facility:write 已拆成 create（ORG）與 update（FACILITY），角色對應已補';
END;
$$;

COMMIT;
