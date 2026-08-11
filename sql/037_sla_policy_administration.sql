-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 037: SLA policy 可由管理者維護（權限 + 目錄不得含糊）
-- =============================================================================
-- 032 之後，SLA policy 決定了每一張工單的目標時刻，而目標時刻決定了報表上
-- 那個要拿去談合約的百分比。但**沒有任何端點能維護它** ——
-- `GET /sla-policies` 在 ENDPOINTS.md 裡契約與實作都是「—」。
--
-- 直接後果：種子只覆蓋 CRITICAL／HIGH／MEDIUM，而 `LOW` 與 `URGENT` 的工單
-- 一律 `NOT_APPLICABLE`（不進分母、不被掃描、不會升級）。`URGENT` 特別刺眼：
-- 名字比 `HIGH` 急，卻沒有任何時限。
--
-- 補那兩筆的方式不該是再寫一個 migration 把分鐘數寫死 —— 那是合約數字，
-- 屬於管理者。這個 migration 打開那條路：加權限、加約束，端點由應用層補。
--
-- -----------------------------------------------------------------------------
-- (1) 為什麼是新權限碼而不是沿用 tenant:update
-- -----------------------------------------------------------------------------
-- `sla_policies` 不是一般的租戶設定：它是**合約條款**。用 `tenant:update`
-- 會讓「能改公司名稱」等於「能改 SLA 承諾」，而後者是報表數字的來源。
--
-- `min_scope_level = FACILITY`，因為 032 的解析順序刻意讓**場域專屬的
-- policy 勝過租戶通用的**（理由：SLA 通常寫在「這棟樓的合約」裡）。
-- 若這個權限要求 TENANT，那條設計就沒有人走得到 —— 與 026 之後
-- `organization:write` 的處境一樣（031 決定 #5 才補上授權）。
--
-- **但租戶通用的 policy（`facility_id IS NULL`）影響每一個場域**，
-- 因此那一類由應用層額外要求 TENANT 範圍
-- （`require_tenant_scoped_permission`）。這是 027 拆
-- `facility:create`／`facility:update` 的同一個判斷，只是這裡用範圍而不是
-- 拆碼來表達 —— 因為動作是同一個（維護 policy），差的只是影響範圍。
--
-- -----------------------------------------------------------------------------
-- (2) 為什麼要加唯一索引
-- -----------------------------------------------------------------------------
-- 032 的 `resolve_sla_policy` 在同樣具體的候選之間以 `sp.code` 決勝：
--
--     ORDER BY (facility_id IS NOT NULL) DESC,
--              (applies_to_priority IS NOT NULL) DESC,
--              code
--
-- 那個 `code` 是防止不確定結果的最後手段，不是一條有意義的規則。
-- 一旦管理者能自己建 policy，它就變成陷阱：對同一個
-- `(facility, priority)` 建了第二個 active policy，**生效的是代碼字典序
-- 較小的那個**。管理者建了 `SLA_HQ_HIGH_V2` 卻發現沒有作用，
-- 而系統不會給任何提示。
--
-- 因此加一個部分唯一索引，讓那件事在**寫入時**就變成 409。
-- `NULLS NOT DISTINCT`（PG 15+）是必要的：預設 NULL 互不相等，
-- 於是兩個「租戶通用 + 所有優先度」的 policy 都會被放行 ——
-- 而那正是最含糊的一種重複。
--
-- 只約束 `is_active` 的列：停用的舊 policy 要能留著（工單快照了它的 id，
-- 而且歷史紀錄有價值）。
--
-- 依賴：004（sla_policies）、026（min_scope_level 生效）、032（解析函式）。
-- =============================================================================

-- 動 permissions／role_permissions（029 的稽核觸發器掛在後者上）→ 需要平台情境。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 權限碼
-- -----------------------------------------------------------------------------
INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous)
VALUES
  ('sla_policy:read',  'sla_policy', 'read',  'CORE',
   '查詢 SLA 政策', 'FACILITY', false),
  ('sla_policy:write', 'sla_policy', 'write', 'CORE',
   '維護 SLA 政策（合約條款：決定工單的目標時刻與報表數字）', 'FACILITY', true)
ON CONFLICT (code) DO UPDATE
  SET resource = EXCLUDED.resource,
      action = EXCLUDED.action,
      module = EXCLUDED.module,
      description = EXCLUDED.description,
      min_scope_level = EXCLUDED.min_scope_level,
      is_dangerous = EXCLUDED.is_dangerous;

-- **必須自己補列**（027 檔頭記過）：008 給 PLATFORM_ADMIN／TENANT_ADMIN 的
-- 萬用 INSERT 不會因為後來新增權限碼而重跑。只靠它們的話，新碼會有名字
-- 而沒有任何人持有。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code
FROM fms.roles r,
     (VALUES ('sla_policy:read'), ('sla_policy:write')) AS c(code)
WHERE r.code IN ('PLATFORM_ADMIN', 'TENANT_ADMIN')
ON CONFLICT DO NOTHING;

-- FACILITY_ADMIN 也拿 write：他管的那棟樓的 SLA 就寫在他的合約裡，
-- 而 032 的解析順序刻意讓場域專屬 policy 優先。租戶通用的那一類
-- 他仍然建不了 —— 那需要 TENANT 範圍，由應用層擋。
--
-- 讀取則放寬給所有需要判斷派工優先順序的角色。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'sla_policy:write'
FROM fms.roles r WHERE r.code = 'FACILITY_ADMIN'
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'sla_policy:read'
FROM fms.roles r
WHERE r.code IN ('FACILITY_ADMIN', 'ORG_MANAGER', 'MAINTENANCE_SUPERVISOR', 'TECHNICIAN', 'VIEWER')
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- (2) 目錄不得含糊
-- -----------------------------------------------------------------------------
CREATE UNIQUE INDEX IF NOT EXISTS uq_sla_policies_scope
  ON fms.sla_policies (tenant_id, facility_id, applies_to_priority)
  NULLS NOT DISTINCT
  WHERE is_active;

COMMENT ON INDEX fms.uq_sla_policies_scope IS
  '同一個 (facility, priority) 只能有一個 active policy。'
  '沒有它，resolve_sla_policy 的 code 決勝會讓「第二個 policy 靜默沒有作用」——'
  '而那是管理者無法自己察覺的。NULLS NOT DISTINCT 是必要的：'
  '預設 NULL 互不相等，最含糊的那種重複（通用+通用）會被放行。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 本檔在 CORE 裡執行、早於 009，因此**沒有任何 sla_policies 列**
-- （032／036 都在這裡踩過，第四次就不踩了）。
-- 行為在 `sla_policy_slice.rs`；這裡只驗不依賴租戶資料的部分。
DO $$
DECLARE
  v_n bigint;
BEGIN
  -- (1) 兩個碼都在，而且範圍宣告如預期。寫死期望值是刻意的：
  --     `min_scope_level` 是 026 的執行對象，改動它會改變誰能做這件事。
  IF NOT EXISTS (
    SELECT 1 FROM fms.permissions
     WHERE code = 'sla_policy:write' AND min_scope_level = 'FACILITY'
  ) THEN
    RAISE EXCEPTION '037 FAILED: sla_policy:write 應宣告 FACILITY 範圍';
  END IF;

  -- (2) 有人持有它。這一格擋的正是 027 檔頭記的那個症狀
  --     （新碼有名字、沒有任何人持有 → 端點對所有人都是 403）。
  SELECT count(DISTINCT r.code) INTO v_n
    FROM fms.roles r
    JOIN fms.role_permissions rp ON rp.role_id = r.id
   WHERE rp.permission_code = 'sla_policy:write';
  IF v_n < 3 THEN
    RAISE EXCEPTION '037 FAILED: sla_policy:write 只有 % 個角色持有', v_n;
  END IF;

  -- (3) 索引在，而且是 NULLS NOT DISTINCT 的部分索引。
  --     兩個性質都會被人「順手簡化」掉，而簡化後的症狀是靜默的。
  IF NOT EXISTS (
    SELECT 1 FROM pg_index i
     WHERE i.indexrelid = 'fms.uq_sla_policies_scope'::regclass
       AND i.indisunique
       AND NOT i.indnullsnotdistinct = false
       AND i.indpred IS NOT NULL
  ) THEN
    RAISE EXCEPTION
      '037 FAILED: uq_sla_policies_scope 必須是 NULLS NOT DISTINCT 的部分唯一索引';
  END IF;

  RAISE NOTICE '037 OK: sla_policy 權限與唯一索引就緒';
END;
$$;

COMMIT;
