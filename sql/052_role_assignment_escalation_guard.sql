-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 052: 角色指派的提權防護 —— 讓 permissions.is_dangerous 第一次被讀
-- =============================================================================
-- docs/security-review-open-items.md 第 2 節留了兩件事給「那支端點要做時」：
--
--   1.「把一個場域級角色指派給某人」聽起來就是場域管理員該能做的事，
--      等那支端點要做時，得依同一條規則重新判斷。
--   3.「roles.scope_level 與 permissions.is_dangerous 仍然無人讀取。」
--
-- 那支端點就是 POST /users/{id}/role-assignments。這個 migration 補的是它
-- 需要的判定，而判定的形狀由**量出來的數字**決定，不是由直覺。
--
-- -----------------------------------------------------------------------------
-- 要擋的東西：ORG_MANAGER 把 TENANT_ADMIN 指派給自己
-- -----------------------------------------------------------------------------
-- ORG_MANAGER 持有 role:assign（008 宣告 min_scope_level = ORG），
-- 因此它**本來就有權指派角色**。問題是指派誰。
--
-- 026 讓「比自己範圍寬的權限」失效，所以把 TENANT_ADMIN 指派在 ORG 範圍
-- 不會拿到整組租戶管理權。但實測那不是零 ——
--
--   新增權限 14 項：alarm_rule:write, asset:delete, bim_model:write,
--   device:write, holiday:write, meter:write, part:read, part:write,
--   reservation:override, reservation:read_own, telemetry:ingest,
--   work_order:execute, work_order:read_own, work_order:reopen
--
-- 其中 asset:delete 與 reservation:override 都標了 is_dangerous。
-- 也就是說 026 收斂了大部分，但沒有把這條路關上。
--
-- -----------------------------------------------------------------------------
-- 為什麼不是「權限子集」（Kubernetes 式的 escalation prevention）
-- -----------------------------------------------------------------------------
-- 最直覺的規則是「你只能授出你自己持有的權限」。它確實擋住上面那件事，
-- 但實測它同時把 role:assign @ORG 變成幾乎沒有用的權限：
--
--   ORG_MANAGER 在子集規則下可指派的角色：DISPATCHER、PM_GENERATOR —— 11 選 2。
--   **連 TECHNICIAN 與 VIEWER 都指派不了**，因為 ORG_MANAGER 沒有
--   work_order:execute、part:read、work_order:read_own。
--
-- 那個結果是錯的，而錯在規則混淆了兩件事：「我能不能自己做 X」與
-- 「我能不能授權別人做 X」。一個不會修設備的主管當然可以聘技師。
--
-- -----------------------------------------------------------------------------
-- 也不是 roles.scope_level
-- -----------------------------------------------------------------------------
-- 那個欄位的註解寫「Highest scope at which this role is intended to be granted」，
-- 看起來剛好可以用。但它的語意在現行目錄裡並不一致：
--
--   * IOT_INGEST 宣告 scope_level = TENANT，四項權限卻**全是** FACILITY-min。
--     用它當上限會擋掉「只在單一場域收資料的 ingest 帳號」，而那是合理需求。
--   * 現存指派 PM_GENERATOR[FACILITY] @ TENANT 已經違反那個上限（019 刻意的）。
--
-- 一個既不一致、現存資料又已經違反的欄位，不適合拿來當授權判定。
-- 它仍然無人讀取，這個 migration 沒有改變那件事。
--
-- -----------------------------------------------------------------------------
-- 採用的規則：**你不能授出一項你自己沒有的危險權限**
-- -----------------------------------------------------------------------------
-- 作業型權限（work_order:execute、part:read…）自由委派；
-- 行政型權限（is_dangerous）要自己先有。實測這條規則的判別力：
--
--   ORG_MANAGER 可指派 8 個角色（DISPATCHER／MAINTENANCE_SUPERVISOR／
--   ORG_MANAGER／PM_GENERATOR／REQUESTER／SERVICE_STAFF／TECHNICIAN／VIEWER
--   ／IOT_INGEST），擋掉 TENANT_ADMIN 與 PLATFORM_ADMIN —— 正是要擋的那兩個。
--
-- 它也順帶關掉 security review 第 2 節記的那條鏈：`role:write` + `role:assign`
-- 「鑄造一個含任意權限的角色再指派給自己」。鑄出來的角色若含你沒有的危險權限，
-- 一樣過不了；若只含你已經有的，那就不是提權。
--
-- **這條規則完全是資料。** 哪些權限危險在 permissions.is_dangerous，
-- 誰持有在 role_permissions。目前 ORG_MANAGER 差一項 holiday:write 才能指派
-- FACILITY_ADMIN —— 要放行，管理員改資料即可，不必改程式碼。
--
-- -----------------------------------------------------------------------------
-- 為什麼是函式而不是 trigger
-- -----------------------------------------------------------------------------
-- 房規（022／026）是「把執行下移到唯一權威」，照那個慣例應該做成 trigger。
-- 這裡刻意不做，理由是判定的輸入包含**授權者是誰**，而
-- user_role_assignments 有三個沒有人類授權者的合法寫入來源：
-- DIRECTORY_SYNC、SCIM、SYSTEM（source 欄位的 CHECK 就列著）。
--
-- trigger 要嘛擋掉目錄同步，要嘛需要一個豁免開關 —— 而那個開關就是繞道。
-- 折衷是：判定本身是**一份** SQL（下面這支函式，可在 psql 直接驗），
-- 呼叫點在 handler。代價誠實寫在這裡：**繞過 handler 的寫入不受這條規則約束。**
--
-- 依賴：002（is_dangerous、role_permissions）、016（user_permission_codes）、
--       026（scope_width 與視圖述詞）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 判定：這次指派會授出哪些「授權者自己沒有的危險權限」
-- -----------------------------------------------------------------------------
-- 回傳空集合 = 允許。回傳非空 = 拒絕，而回傳的內容就是拒絕的理由
-- （handler 直接把它放進錯誤訊息 —— 一個只說「不行」的 403 會變成一次工單）。
--
-- 授權者持有與否，是在**這次指派的範圍**上判定，不是「在任何範圍」。
-- 差別是實的：org A 的管理員在 A 持有 organization:write，不該因此能把它
-- 授到 org B 去。走 016 的 user_permission_codes，範圍包含由它的 org_path
-- 述詞決定，這裡不自己展開子樹。
--
-- SPATIAL_NODE 直接擋掉並說明原因：016 的述詞只認 TENANT／FACILITY／ORG
-- 三種 scope_type，SPATIAL_NODE 的指派**一項權限都不會生效**。
-- 允許它建立等於發一張永遠不會兌現的授權，而那比拒絕更難查。
CREATE OR REPLACE FUNCTION fms.role_grant_blocked_by(
  p_grantor_id uuid,
  p_role_id    uuid,
  p_scope_type text,
  p_scope_id   uuid
) RETURNS SETOF varchar
LANGUAGE plpgsql STABLE
AS $$
BEGIN
  IF p_scope_type = 'SPATIAL_NODE' THEN
    RAISE EXCEPTION
      'SPATIAL_NODE 範圍的角色指派不會生效：user_permission_codes 只認 '
      'TENANT／FACILITY／ORG 三種 scope_type（見 016）';
  END IF;

  RETURN QUERY
  SELECT rp.permission_code
    FROM fms.role_permissions rp
    JOIN fms.permissions p ON p.code = rp.permission_code
   WHERE rp.role_id = p_role_id
     AND p.is_dangerous
     AND rp.permission_code NOT IN (
           SELECT c FROM fms.user_permission_codes(
             p_grantor_id,
             CASE WHEN p_scope_type = 'FACILITY' THEN p_scope_id END,
             CASE WHEN p_scope_type = 'ORG'      THEN p_scope_id END) c)
   ORDER BY rp.permission_code;
END;
$$;

COMMENT ON FUNCTION fms.role_grant_blocked_by(uuid, uuid, text, uuid) IS
  '角色指派的提權防護：回傳「這個角色帶有、但授權者在該範圍並未持有」的危險權限。'
  ' 空集合代表允許。permissions.is_dangerous 的第一個讀取者（052）。'
  ' 呼叫點在 handler 而非 trigger —— DIRECTORY_SYNC／SCIM／SYSTEM 沒有人類授權者。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- **刻意不依賴 seed。** 第一版的格子是「找一個帶 ORG_MANAGER 的使用者，
-- 驗證他指派不了 TENANT_ADMIN」—— 而示範租戶根本沒有 ORG_MANAGER 指派
-- （現存只有 TENANT_ADMIN／FACILITY_ADMIN／TECHNICIAN／SERVICE_STAFF／
-- REQUESTER／PM_GENERATOR）。那個版本會靜默跳過最重要的兩格，
-- 而「跳過」在輸出裡看起來跟「通過」一樣。
--
-- 改成用一個**不存在的授權者**（隨機 uuid，持有零項權限）當探針：
-- 他能授出什麼，完全由角色本身的危險權限決定，不需要任何資料。
--
-- 端到端那一段（ORG_MANAGER 走 HTTP 被擋 403、指派 TECHNICIAN 得 201）
-- 在 fms-server 的 role_assignments_slice 測試，那裡可以自由建帳號與指派。
DO $$
DECLARE
  v_nobody   uuid := '00000000-0000-4000-8000-0000000052ff';  -- 沒有任何指派
  v_r_tadmin uuid;
  v_r_tech   uuid;
  v_blocked  text;
  v_n        int;
  v_expect   int;
BEGIN
  SELECT id INTO v_r_tadmin FROM fms.roles WHERE code = 'TENANT_ADMIN' AND tenant_id IS NULL;
  SELECT id INTO v_r_tech   FROM fms.roles WHERE code = 'TECHNICIAN'   AND tenant_id IS NULL;

  -- (1) is_dangerous 真的還有值可讀。若哪天有人把它全清成 false，
  --     這條規則會靜默地變成「什麼都放行」—— 那正是最該擋的失效模式。
  SELECT count(*) INTO v_n FROM fms.permissions WHERE is_dangerous;
  IF v_n < 10 THEN
    RAISE EXCEPTION '052 FAILED: is_dangerous 只有 % 項，這條規則等於失效', v_n;
  END IF;
  RAISE NOTICE '052 (1) OK：is_dangerous 有 % 項', v_n;

  -- (2) 目錄層的判別力：TENANT_ADMIN 帶的危險權限必須是 ORG_MANAGER 的**真超集**。
  --     少了這一格，(3) 的「擋下來了」可能只是因為兩個角色剛好不相干。
  SELECT count(*) INTO v_n
    FROM (
      SELECT rp.permission_code FROM fms.role_permissions rp
        JOIN fms.permissions p ON p.code = rp.permission_code
       WHERE rp.role_id = v_r_tadmin AND p.is_dangerous
      EXCEPT
      SELECT rp.permission_code FROM fms.roles r
        JOIN fms.role_permissions rp ON rp.role_id = r.id
       WHERE r.code = 'ORG_MANAGER' AND r.tenant_id IS NULL) d;
  IF v_n = 0 THEN
    RAISE EXCEPTION
      '052 FAILED: TENANT_ADMIN 沒有任何 ORG_MANAGER 缺少的危險權限，'
      '這條規則對這組角色不具判別力';
  END IF;
  RAISE NOTICE '052 (2) OK：TENANT_ADMIN 比 ORG_MANAGER 多 % 項危險權限', v_n;

  -- (3) 零權限的授權者要授出 TENANT_ADMIN → 必須擋下**全部**危險權限，
  --     不是擋下一部分。比對的是精確數量而非「有沒有被擋」：
  --     一個只回第一列的實作也會讓「有沒有被擋」通過。
  SELECT count(*) INTO v_expect
    FROM fms.role_permissions rp JOIN fms.permissions p ON p.code = rp.permission_code
   WHERE rp.role_id = v_r_tadmin AND p.is_dangerous;
  SELECT count(*) INTO v_n
    FROM fms.role_grant_blocked_by(v_nobody, v_r_tadmin, 'TENANT', NULL);
  IF v_n <> v_expect THEN
    RAISE EXCEPTION '052 FAILED: 零權限授權者指派 TENANT_ADMIN 只擋下 % 項，應為 %',
                    v_n, v_expect;
  END IF;
  RAISE NOTICE '052 (3) OK：零權限授權者被擋下 TENANT_ADMIN 的全部 % 項危險權限', v_n;

  -- (4) (3) 的反面。TECHNICIAN 一項危險權限都沒有（20 項全是作業型），
  --     因此**連零權限的授權者都不會被這條規則擋**。
  --     這正是這個設計的重點：作業型權限自由委派，行政型權限要自己先有。
  --     擋住「隨便誰都能指派技師」的是另一道閘 —— handler 的
  --     require_permission('role:assign', …)，兩道缺一不可。
  --
  --     若規則被改成「一律拒絕」或「子集規則」，這一格會失敗而 (3) 仍通過。
  SELECT string_agg(c, ',' ORDER BY c) INTO v_blocked
    FROM fms.role_grant_blocked_by(v_nobody, v_r_tech, 'TENANT', NULL) c;
  IF v_blocked IS NOT NULL THEN
    RAISE EXCEPTION
      '052 FAILED: TECHNICIAN 被這條規則擋下（%），但它沒有任何危險權限 —— '
      '規則變成了子集規則，那會讓 role:assign @ORG 幾乎無法使用', v_blocked;
  END IF;
  RAISE NOTICE '052 (4) OK：TECHNICIAN 不含危險權限，不受這條規則限制';

  -- (5) SPATIAL_NODE 必須拋錯而不是回空集合。回空集合等於「允許」，
  --     而那會建出一張永遠不兌現的授權（016 的述詞不認這個 scope_type）。
  BEGIN
    PERFORM fms.role_grant_blocked_by(v_nobody, v_r_tech, 'SPATIAL_NODE', v_nobody);
    RAISE EXCEPTION '052 FAILED: SPATIAL_NODE 沒有被擋下，會建出不生效的指派';
  EXCEPTION WHEN raise_exception THEN
    IF sqlerrm LIKE '052 FAILED%' THEN RAISE; END IF;
    RAISE NOTICE '052 (5) OK：SPATIAL_NODE 被明確拒絕';
  END;

  RAISE NOTICE '052 OK';
END;
$$;

COMMIT;
