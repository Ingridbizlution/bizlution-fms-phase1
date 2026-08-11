-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 048: ORG_MANAGER 可以自己訂 SLA policy 的分鐘數
-- =============================================================================
-- 回答的問題是：「LOW／URGENT 的 SLA policy 分鐘數，可否由 ORG 管理員決定？」
--
-- **可以，而且這是唯一正確的答案** —— 那些數字是合約條款，不是技術參數。
-- 「LOW 幾分鐘內要回應」取決於那個客戶簽了什麼，寫死在 migration 裡的任何
-- 數值都是猜的。037 因此刻意讓 `resolve_sla_policy` **沒有預設 fallback**：
-- 沒有政策就是 `NOT_APPLICABLE`，而不是套一個編出來的門檻。
--
-- 缺的只有一個授權：`ORG_MANAGER` 已經有 `sla_policy:read`（看得到），
-- 但沒有 `sla_policy:write`（改不了）。
--
-- -----------------------------------------------------------------------------
-- 範圍模型本來就是對的，不用改
-- -----------------------------------------------------------------------------
-- 這個授權不會讓 ORG 管理員去動別人的東西，因為兩層各自把關：
--
--   * **RLS**：`sla_policies.facility_scope` 是 RESTRICTIVE，
--     `current_facility_ids()` 對 ORG 範圍的使用者只展開成他自己組織子樹
--     底下的場域。
--   * **API**：`sla_policy::require_scope` 對 `facility_id IS NULL`
--     （租戶通用政策）要求的是 `require_tenant_scoped_permission` ——
--     ORG 範圍過不了那一關。也就是說他能為自己子樹裡的場域訂政策，
--     訂不了套用到全租戶的那一種。
--
-- 026／045 的不變量也成立：`sla_policy:write` 的 `min_scope_level` 是
-- FACILITY（`scope_width` = 1），`ORG_MANAGER` 的 `scope_level` 是 ORG
-- （width = 2）—— 角色範圍不比權限要求窄，因此不是 045 清掉的那種組合。
--
-- -----------------------------------------------------------------------------
-- 順帶記下一個**還沒修**的資料庫層缺口（不在這個 migration 的範圍內）
-- -----------------------------------------------------------------------------
-- `sla_policies.facility_scope` 沒有明寫 `WITH CHECK`，因此 PostgreSQL 對
-- `cmd = ALL` 的政策會把 `USING` 拿來當寫入檢查 —— 而那個述詞是
-- `facility_in_scope(facility_id)`，對 **NULL 一律放行**（021 的定義）。
--
-- 實測（以 `fms_app` 連線、`app.facility_ids` 只設一個場域）：
--
--     INSERT ... facility_id = 自己的場域   → 成功（正確）
--     INSERT ... facility_id = NULL         → **也成功**（不該）
--
-- 也就是說在資料庫層，一個場域受限的寫入者能建立套用到全租戶的政策。
-- 這個缺口**是 037 帶進來的，不是這個授權帶進來的** ——
-- `FACILITY_ADMIN`（範圍更窄）早就持有 `sla_policy:write`，所以把權限給
-- ORG_MANAGER（範圍更寬）並沒有讓它擴大一分。
--
-- 目前沒有實際曝露：API 層的 `require_scope` 擋住了。但這個 schema 的原則
-- （007／013／046）是「RLS 要能自己站住 —— 一次 SQL injection 不該足以
-- 關掉隔離」，所以它該修。
--
-- 修法與 046 對 `audit_log` 做的完全同型：**讀維持 NULL 放行**（每個場域都
-- 需要看得到套用在自己身上的租戶通用政策），**寫不放行**：
--
--     WITH CHECK (is_platform_context()
--                 OR current_facility_ids() IS NULL
--                 OR facility_id = ANY (current_facility_ids()))
--
-- 同一形狀的表共 8 張（`alarm_rules`、`announcements`、`holiday_calendars`、
-- `integrations`、`quota_policies`、`service_items`、`sla_policies`、`teams`），
-- 其中只有 `sla_policies` 與 `holiday_calendars` 目前有寫入端點。
-- 那是一個獨立的 migration，牽動的表跟這個問題無關，因此不混在這裡。
--
-- 依賴：026（範圍寬度）、037（sla_policy 權限）、045（角色目錄清理）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'sla_policy:write'
  FROM fms.roles r
 WHERE r.code = 'ORG_MANAGER'
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_bad text;
BEGIN
  -- (1) 授權真的在。
  IF NOT EXISTS (
    SELECT 1 FROM fms.role_permissions rp
      JOIN fms.roles r ON r.id = rp.role_id
     WHERE r.code = 'ORG_MANAGER' AND rp.permission_code = 'sla_policy:write'
  ) THEN
    RAISE EXCEPTION '048 FAILED: ORG_MANAGER 沒有拿到 sla_policy:write';
  END IF;

  -- (2) 045 的不變量沒有被這次授權破壞：**沒有任何角色的範圍比它持有的
  --     權限所要求的更窄**。
  --
  --     這一格是 schema 級的，不是只看我加的那一條 —— 因為「加一條授權」
  --     正是會破壞這個不變量的動作，而破壞的症狀（026 在執行期擋下來，
  --     使用者看到 403）離這裡很遠。
  SELECT string_agg(DISTINCT r.code || '→' || rp.permission_code, '、'
                    ORDER BY r.code || '→' || rp.permission_code)
    INTO v_bad
    FROM fms.role_permissions rp
    JOIN fms.roles r ON r.id = rp.role_id
    JOIN fms.permissions p ON p.code = rp.permission_code
   WHERE fms.scope_width(r.scope_level) < fms.scope_width(p.min_scope_level);
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION
      '048 FAILED: 這些角色的範圍窄於權限要求（026 會在執行期擋下來）：%', v_bad;
  END IF;

  RAISE NOTICE '048 OK: ORG_MANAGER 可以訂自己子樹底下場域的 SLA policy';
END;
$$;

COMMIT;
