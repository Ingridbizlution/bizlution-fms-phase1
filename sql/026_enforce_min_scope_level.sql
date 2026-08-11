-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 026: 執行 permissions.min_scope_level
-- =============================================================================
-- 補的是 docs/security-review-open-items.md 第 2 項。
--
-- 原始描述說這是「設計缺口，需要先決定租戶級物件該由誰建立」。實際查過之後
-- 那個決定**早就做完了**：002 給 fms.permissions 加了 min_scope_level
-- （CHECK 限定 TENANT／ORG／FACILITY／SPATIAL_NODE），008 與 011 為每一項
-- 權限都填了值。缺的不是決定，是執行 —— 全 schema 與全 app 對這個欄位的
-- 引用只有「定義它」與「填它」，沒有任何一處讀它。
--
-- 這與 022 是同一型缺陷：work_order_transitions_allowed.required_permission
-- 宣告在表上、transition_work_order 完全忽略它。修法也一樣：把執行下移到
-- 唯一權威，而不是在每個呼叫端補一次判斷。
--
-- 實測過的可達性（fm.lin，FACILITY_ADMIN，範圍只有總部一個場域）：
--   * POST /facilities   → 403，但擋它的是 007 的 facility_scope 政策
--     （新場域不在可見快照裡，create_facility 讀不回來就回滾），
--     **不是**權限檢查。handler 從沒看過範圍。
--   * 加派 TENANT_ADMIN 但範圍仍限單一場域後 POST /organizations → **201 成功**
--     organizations 沒有 facility_id，也就沒有那條政策，於是完全無守衛。
--
-- 也就是說：目前唯一擋住租戶級建立的東西，是「那張表剛好有 facility_scope
-- 政策」這個巧合。57 張無 facility_id 的表裡，organizations 只是第一個
-- 有 POST 端點的。
--
-- -----------------------------------------------------------------------------
-- 為什麼判定加在視圖裡
-- -----------------------------------------------------------------------------
-- v_user_effective_permissions 已經 JOIN 了 fms.permissions，min_scope_level
-- 就在手邊。加在這裡，一條述詞就自動傳播到全部四個消費者：
--   * user_permission_codes（讀視圖）
--   * user_permission_codes_anywhere（讀視圖）
--   * user_has_permission（以 user_permission_codes 定義）
--   * /auth/me 的權限清單（fms-identity 的 load_permission_strings 讀視圖）
--
-- 最後一項不是附帶效果而是必要條件：若只收斂函式而不收斂視圖，
-- /auth/me 會向前端宣告一組實際上用不了的權限，而 012 的 T12
-- 正是在交叉比對這兩者。判定必須只有一份。
--
-- 依賴：002（視圖與 min_scope_level）、008／011（宣告值）、016（函式）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 範圍寬度
-- -----------------------------------------------------------------------------
-- 未知值回 NULL，因此比較結果是 NULL、該列被過濾掉 —— 這是刻意的 fail-closed：
-- 若日後有人加了第五個層級卻忘記加進這裡，症狀是「權限失效」而不是
-- 「權限一律通過」。前者會被馬上發現，後者不會。
CREATE OR REPLACE FUNCTION fms.scope_width(p_scope text)
RETURNS int
LANGUAGE sql IMMUTABLE PARALLEL SAFE
AS $$
  SELECT CASE p_scope
           WHEN 'TENANT'       THEN 3
           WHEN 'ORG'          THEN 2
           WHEN 'FACILITY'     THEN 1
           WHEN 'SPATIAL_NODE' THEN 0
         END;
$$;

COMMENT ON FUNCTION fms.scope_width(text) IS
  '範圍層級的寬度序，TENANT 最寬。用於比較「授權的範圍」與「權限要求的最低範圍」。'
  ' 未知值回 NULL，使比較失敗而非通過（fail-closed）。';

-- -----------------------------------------------------------------------------
-- (2) 視圖加上層級述詞
-- -----------------------------------------------------------------------------
-- 欄位清單與 002 完全相同（CREATE OR REPLACE VIEW 的要求），只多一條 WHERE。
CREATE OR REPLACE VIEW fms.v_user_effective_permissions AS
SELECT
  ura.tenant_id,
  ura.user_id,
  rp.permission_code,
  p.module,
  p.resource,
  p.action,
  ura.scope_type,
  ura.scope_id,
  r.code AS role_code
FROM fms.user_role_assignments ura
JOIN fms.roles r            ON r.id = ura.role_id
JOIN fms.role_permissions rp ON rp.role_id = r.id
JOIN fms.permissions p       ON p.code = rp.permission_code
WHERE ura.valid_from <= clock_timestamp()
  AND (ura.valid_until IS NULL OR ura.valid_until > clock_timestamp())
  -- 026：授權的範圍必須不窄於權限宣告的最低範圍。
  --
  -- 這一行是整個第 2 項的修正。少了它，「把角色指派在單一場域」對
  -- 沒有 facility_id 的物件完全沒有意義 —— scope_type 在展開權限時被丟掉。
  AND fms.scope_width(ura.scope_type) >= fms.scope_width(p.min_scope_level);

COMMENT ON VIEW fms.v_user_effective_permissions IS
  '使用者實際持有的權限，含授權範圍。026 起會過濾掉「授權範圍比權限要求的最低範圍更窄」的列，'
  ' 因此租戶級動作無法由場域級授權取得。這是 min_scope_level 的唯一執行點。';

-- -----------------------------------------------------------------------------
-- (3) 修正被過度宣告的四項
-- -----------------------------------------------------------------------------
-- 開始執行之後才看得出來：008 是按「這個**資源**住在哪一層」填 min_scope_level
-- （organization／user／role／tenant 是租戶級資源），而不是按「這個**動作**的
-- 影響範圍」。在沒有任何東西執行它的情況下這個區別沒有成本，所以沒人需要小心。
--
-- 正確的語意是後者：**讀一個租戶級資源不是租戶級特權，寫它才是。**
-- 依這條規則檢查全部 20 項宣告 TENANT 的權限，結論是 :write 類全部正確
-- （role:write、tenant:update、identity_provider:write、quota:manage、
--  user:write、asset_model:write、maintenance_template:write、directory:sync、
--  integration:write、user:impersonate、audit:export 都真的是租戶級動作），
-- 被過度宣告的是 :read 類。
--
-- 這不是理論問題：不改這四格，兩支已上線的端點會回歸成 403
-- （GET /asset-models 與 GET /organizations 對 FACILITY_ADMIN），
-- 而 work_order_slice.rs 的
-- `facility_scoped_roles_can_list_and_only_see_their_facility`
-- 正是在守這個方向。
UPDATE fms.permissions SET min_scope_level = 'ORG'
 WHERE code = 'organization:write';   -- 組織是 ltree 階層物件：ORG 經理該能在自己子樹內建子組織

UPDATE fms.permissions SET min_scope_level = 'FACILITY'
 WHERE code IN (
   'organization:read',   -- 場域管理員要知道自己屬於哪個組織
   'asset_model:read',    -- 共用型錄查詢；007 已把 asset_models 列入 catalog_tables
   'user:read'            -- 派工要選人
 );

-- 刻意不動的：role:read、tenant:read、identity_provider:read、integration:read
-- 目前沒有任何端點使用，實際需要多寬還不知道。現在猜是憑空決定，
-- 留到它們有端點時再依同一條規則判斷。

-- -----------------------------------------------------------------------------
-- 留給後續的已知問題：role:assign 宣告 ORG，但 008 給了 FACILITY_ADMIN
-- -----------------------------------------------------------------------------
-- 008 第 130 行把 role:assign 給了 ORG_MANAGER 與 FACILITY_ADMIN 兩者。
-- 加上本 migration 的述詞後，FACILITY 範圍的 FACILITY_ADMIN 會失去它。
--
-- 目前沒有角色指派端點，因此**沒有活的回歸**。但這個組合本身有疑問：
-- 「把一個場域級角色指派給某人」聽起來就是場域管理員該能做的事。
-- 等那支端點要做時，得依同一條規則決定 role:assign 到底該宣告 ORG
-- 還是 FACILITY —— 刻意不在這裡順手改，因為那需要看那支端點的實際語意
-- （能指派哪些角色？能指派到哪些範圍？），而那支端點還不存在。

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 行為層的斷言（哪個角色在哪個範圍下持有什麼）放在
-- app/crates/fms-server/tests/rbac_scope_slice.rs：那些需要 009 的示範資料，
-- 而本檔在 CORE 裡執行、位置早於 009。在這裡假裝驗證得到會是自欺。
--
-- 這裡驗的是不依賴任何資料的三件事：寬度序的完整性與單調性、述詞真的
-- 進了視圖、四格宣告已改。
DO $$
DECLARE
  v_bad text;
BEGIN
  -- 四個合法層級都要有寬度，且順序嚴格遞增
  IF fms.scope_width('SPATIAL_NODE') >= fms.scope_width('FACILITY')
     OR fms.scope_width('FACILITY') >= fms.scope_width('ORG')
     OR fms.scope_width('ORG')      >= fms.scope_width('TENANT') THEN
    RAISE EXCEPTION '026 FAILED: scope_width 不是嚴格遞增';
  END IF;
  IF fms.scope_width('NOT_A_LEVEL') IS NOT NULL THEN
    RAISE EXCEPTION '026 FAILED: 未知層級應回 NULL（fail-closed）';
  END IF;

  -- CHECK 允許的每一個值都必須有寬度，否則那個層級的授權會全部失效
  SELECT string_agg(lvl, ', ') INTO v_bad
    FROM (VALUES ('TENANT'),('ORG'),('FACILITY'),('SPATIAL_NODE')) AS t(lvl)
   WHERE fms.scope_width(lvl) IS NULL;
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION '026 FAILED: 這些合法層級沒有寬度：%', v_bad;
  END IF;

  IF pg_get_viewdef('fms.v_user_effective_permissions'::regclass) NOT LIKE '%scope_width%' THEN
    RAISE EXCEPTION '026 FAILED: 視圖沒有套上層級述詞';
  END IF;

  SELECT string_agg(code || '=' || min_scope_level, ', ' ORDER BY code) INTO v_bad
    FROM fms.permissions
   WHERE (code = 'organization:write' AND min_scope_level <> 'ORG')
      OR (code IN ('organization:read','asset_model:read','user:read')
          AND min_scope_level <> 'FACILITY');
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION '026 FAILED: 宣告未依預期修正：%', v_bad;
  END IF;

  RAISE NOTICE '026 OK: min_scope_level 已由 v_user_effective_permissions 執行，四格過度宣告已修正';
END;
$$;

COMMIT;
