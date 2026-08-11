-- =============================================================================
-- 016  Set-returning permission lookup (WBS 3.9)
-- =============================================================================
-- 為什麼需要
--
-- `fms.user_has_permission()` 一次只答一個問題。工單的
-- `GET /work-orders/{id}/available-actions` 要對「當前狀態下的每個動作」
-- 各問一次（示範資料是 6 個），也就是 6 次往返、6 次同樣的 view 掃描。
-- 這是實作 S4 時自己造出來的 N+1。
--
-- 一起改掉的還有一個更根本的問題：如果只是在應用層另寫一句 SQL 來取
-- 「這個使用者的全部權限」，那段 SQL 就會有第二份 scope 判定邏輯
-- （TENANT／FACILITY／ORG ltree 三種），而兩份判定遲早會漂移。
-- 因此改成：**先有集合版，`user_has_permission` 再用集合版實作**。
-- 判定邏輯從此只有一份。
--
-- 實測（示範資料，Docker Desktop 上的 PG16）：
--   * 冷啟第一次呼叫 4.5ms（含 2.4ms planning、913 shared buffer hits）
--   * 暖機後每次呼叫約 0.16ms（55 次不同參數共 8.68ms）
-- 這個數字是「暫不加 Redis 快取」的依據，見 docs/WBS-rebaseline.md 4.1f。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 集合版：這個使用者在指定範圍內實際持有的權限碼
-- -----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.user_permission_codes(
  p_user_id     uuid,
  p_facility_id uuid DEFAULT NULL,
  p_org_id      uuid DEFAULT NULL
) RETURNS SETOF varchar
LANGUAGE sql STABLE
AS $$
  SELECT DISTINCT ep.permission_code
  FROM fms.v_user_effective_permissions ep
  LEFT JOIN fms.facilities f ON f.id = p_facility_id
  LEFT JOIN fms.organizations o_target ON o_target.id = coalesce(p_org_id, f.org_id)
  LEFT JOIN fms.organizations o_scope  ON o_scope.id = ep.scope_id
  WHERE ep.user_id = p_user_id
    AND (
          ep.scope_type = 'TENANT'
      OR (ep.scope_type = 'FACILITY' AND ep.scope_id = p_facility_id)
      OR (ep.scope_type = 'ORG'
          AND o_scope.org_path IS NOT NULL
          AND o_target.org_path IS NOT NULL
          AND o_target.org_path OPERATOR(public.<@) o_scope.org_path)
    );
$$;

COMMENT ON FUNCTION fms.user_permission_codes IS
  'Every permission code the user effectively holds in the given scope. Single source of the scope predicate; user_has_permission is defined in terms of it.';

-- -----------------------------------------------------------------------------
-- 單一問題版改以集合版實作
--
-- 行為必須與 002 的原定義完全相同 —— 這是純重構。下方的等價驗證會逐一比對。
-- 沒有效能顧慮：`EXISTS` 讓規劃器仍然可以在找到第一列時短路，
-- 而 `DISTINCT` 對 EXISTS 而言是可省略的。
-- -----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.user_has_permission(
  p_user_id     uuid,
  p_permission  varchar,
  p_facility_id uuid DEFAULT NULL,
  p_org_id      uuid DEFAULT NULL
) RETURNS boolean
LANGUAGE sql STABLE
AS $$
  SELECT EXISTS (
    SELECT 1
    FROM fms.user_permission_codes(p_user_id, p_facility_id, p_org_id) AS c
    WHERE c = p_permission
  );
$$;

-- -----------------------------------------------------------------------------
-- 不限範圍版：這個使用者在**任一**範圍內持有的權限碼
-- -----------------------------------------------------------------------------
-- 為什麼需要它
--
-- 列表端點常常沒有 `facility_id` 過濾條件（「給我所有工單」）。
-- 用 `user_has_permission(user, perm, NULL)` 檢查時，FACILITY 分支比對的是
-- `scope_id = p_facility_id`，傳 NULL 就永遠不成立 —— 於是**只有 TENANT
-- 範圍的角色能用任何列表端點**，`FACILITY_ADMIN`、`TECHNICIAN`、`REQUESTER`
-- 全部被拒。那不是「安全的預設值」，那是功能壞掉。
--
-- 正確語意是分成兩件事：
--   * **能不能用這個端點** → 在任一範圍持有該權限即可（本函式）
--   * **看得到哪些列** → 由 RLS 的 `facility_scope` 政策收斂
--     （007 已經寫好，但需要 API 設定 `app.facility_ids` 才會生效；
--      應用層在 `begin_tenant_tx` 內設定它）
--
-- 把授權判定與列可見性混為一談，就會像原本那樣：用「拒絕整個端點」
-- 來代替「限制回傳的列」。
CREATE OR REPLACE FUNCTION fms.user_permission_codes_anywhere(p_user_id uuid)
RETURNS SETOF varchar
LANGUAGE sql STABLE
AS $$
  SELECT DISTINCT ep.permission_code
  FROM fms.v_user_effective_permissions ep
  WHERE ep.user_id = p_user_id;
$$;

COMMENT ON FUNCTION fms.user_permission_codes_anywhere IS
  'Permission codes the user holds in ANY scope. For endpoint-level gating when the request has no facility filter; row visibility is then narrowed by the facility_scope RLS policy, not by denying the endpoint.';

COMMENT ON FUNCTION fms.user_has_permission IS
  'Authoritative authorisation check. Defined in terms of user_permission_codes so the scope predicate exists exactly once. The API layer calls this before any mutating operation.';

COMMIT;
