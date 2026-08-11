-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 050: 租戶通用的列（facility_id IS NULL）只有 TENANT 範圍的人能動
-- =============================================================================
-- 048 的檔頭記了一個缺口：8 張表的 `facility_scope` 政策沒有明寫 `WITH CHECK`，
-- 於是 PostgreSQL 對 `cmd = ALL` 的政策會把 `USING` 拿來當寫入檢查 ——
-- 而那個述詞是 `facility_in_scope(facility_id)`，對 **NULL 一律放行**（021）。
--
-- 實測（以 `fms_app` 連線、`app.facility_ids` 只設一個場域）：
--
--     INSERT ... facility_id = 自己的場域   → 成功（正確）
--     INSERT ... facility_id = NULL         → **也成功**（不該）
--
-- 8 張表：`alarm_rules`、`announcements`、`holiday_calendars`、`integrations`、
-- `quota_policies`、`service_items`、`sla_policies`、`teams`。它們的
-- `facility_id IS NULL` 一律代表「租戶通用」，因此那是一個範圍放大。
--
-- -----------------------------------------------------------------------------
-- 048 檔頭寫的修法是錯的
-- -----------------------------------------------------------------------------
-- 那裡提議的是：
--
--     WITH CHECK (is_platform_context()
--                 OR current_facility_ids() IS NULL          -- ← 這一行沒用
--                 OR facility_id = ANY (current_facility_ids()))
--
-- 它抄的是 046 對 `audit_log` 做的形狀，但那個形狀在這裡不成立。原因是
-- **`current_facility_ids()` 在 `begin_tenant_tx` 底下永遠不是 NULL**：
-- `set_facility_scope`（fms-shared/src/db.rs）一律寫入一份具體清單，
-- 連「沒有任何角色」都寫成全零 uuid 哨兵而不是空字串 ——
-- 那個哨兵的存在本身就是為了避免「空 = 不限制」。
--
-- 於是 `TENANT_ADMIN` 拿到的是**全部 13 個場域的清單**，
-- 而 `NULL = ANY('{13 個 uuid}')` 的結果是 **NULL**，不是 true。
-- 那個 WITH CHECK 會把租戶管理員一起擋掉，租戶通用的政策從此建不出來
-- （種子裡已經有 3 筆 `sla_policies` 是那一類）。
--
-- 更根本的問題：**`app.facility_ids` 分不出 TENANT_ADMIN 與 FACILITY_ADMIN。**
-- 兩者都是非 NULL 清單，只差長度。改成比「清單是否涵蓋租戶全部場域」是個
-- 啟發式，而它在單場域租戶會把那唯一場域的管理員誤判成租戶管理員。
--
-- -----------------------------------------------------------------------------
-- 要問的問題在角色指派裡
-- -----------------------------------------------------------------------------
-- 真正的不變量是：**能寫租戶通用的列 ⇔ 這個人的角色指派裡有 TENANT 範圍。**
-- 那件事在 `user_role_assignments.scope_type` 裡，是資料，不是 GUC。
--
-- 刻意**不**用「多加一個 GUC 讓應用層宣告自己能寫」那種做法：013 的教訓是
-- 自行宣稱的 GUC 不構成安全邊界（`app.is_platform` 之所以還算數，是因為它
-- 額外要求 `fms_platform` 的角色成員身分）。讀真實的角色資料才是邊界。
--
-- 這個判定比 API 層的 `require_tenant_scoped_permission` **粗**：它不檢查是
-- 哪一個權限，只問「這個人是不是租戶範圍的」。那是刻意的 —— RLS 這一層不該
-- 需要一份「表 → 權限」的對照，而那份對照要寫死才做得到。細緻的判定仍然由
-- API 層做；這一層是縱深防禦，答的是「一次 SQL injection 不該足以放大範圍」。
--
-- 不需要 SECURITY DEFINER：`user_role_assignments` 只有 `tenant_isolation`
-- 一條政策（沒有場域維度，因此沒有 `facility_scope`），而這支函式只需要看
-- **當前使用者在當前租戶自己的那幾列** —— 那正是 tenant_isolation 允許的。
-- 少一個 SECURITY DEFINER 就少一個要寫 session_user 守衛的地方。
--
-- -----------------------------------------------------------------------------
-- 讀與寫要分開，而且不只是 INSERT
-- -----------------------------------------------------------------------------
-- **讀必須維持 NULL 放行。** 每一個場域都需要看得到套用在自己身上的租戶通用
-- 政策 —— 那正是 `resolve_sla_policy` 的 fallback 順序在做的事。把讀一起收緊
-- 會讓場域管理員看不到自己受哪份合約條款約束。
--
-- **寫的範圍比 048 描述的更大。** 我當時只說了 INSERT，但同一個不變量涵蓋四件事：
--
--     INSERT  facility_id = NULL          建立租戶通用的列
--     UPDATE  新列 facility_id = NULL     把場域列**放大**成租戶通用
--     UPDATE  舊列 facility_id IS NULL    修改別人訂的租戶通用列
--     DELETE  舊列 facility_id IS NULL    **刪掉**租戶通用列
--
-- 前兩者靠 `WITH CHECK`，後兩者靠 `USING`。而 `USING` 對 ALL 政策是
-- SELECT／UPDATE／DELETE 共用的 —— 收緊它會連讀一起收緊。
--
-- 因此拆成三條 RESTRICTIVE 政策（RESTRICTIVE 以 AND 組合，所以加政策只會更嚴）：
--
--     facility_scope          ALL      USING = 寬（讀要看得到）
--                                      WITH CHECK = 嚴（擋 INSERT 與放大）
--     facility_scope_update   UPDATE   USING = 嚴（擋修改租戶通用列）
--     facility_scope_delete   DELETE   USING = 嚴（擋刪除租戶通用列）
--
-- 只修 INSERT 會留下「你建不了我們的租戶政策，但你刪得掉」——
-- 那比原本的缺口更難解釋。
--
-- **`facility_scope` 這個名字保留不動**：`check-isolation.sh` 的 A2 與 046 的
-- 自我驗證都是按這個名字找政策的。新增的兩條用 `facility_scope_*` 前綴，
-- 並把 A2 的比對放寬成前綴比對，讓它們也被涵蓋。
--
-- 依賴：007（RLS 慣例）、013（GUC 不可自行宣稱）、021（facility_in_scope）、
--       026（範圍寬度）、046（讀寫分離的先例）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 1. 「這個人是不是租戶範圍的」
-- -----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.tenant_wide_write_allowed()
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
  SELECT EXISTS (
    SELECT 1
      FROM fms.user_role_assignments ura
     WHERE ura.user_id = fms.current_user_id()
       AND ura.tenant_id = fms.current_tenant_id()
       AND ura.scope_type = 'TENANT'
       AND (ura.valid_until IS NULL OR ura.valid_until > clock_timestamp())
  );
$$;

COMMENT ON FUNCTION fms.tenant_wide_write_allowed() IS
  '當前使用者的角色指派裡是否有 TENANT 範圍。判定「能不能動 facility_id IS NULL '
  '的列」（那一類列套用到整個租戶）。刻意讀真實的角色資料而不是新增一個 GUC：'
  '自行宣稱的 GUC 不構成邊界（013）。不含平台情境的判斷 —— 那由 '
  'facility_write_in_scope 統一處理。';

-- -----------------------------------------------------------------------------
-- 2. 寫入用的範圍述詞
-- -----------------------------------------------------------------------------
-- 與 `facility_in_scope` 的差別只有一處：**NULL 不放行**，而是要求 TENANT 範圍。
CREATE OR REPLACE FUNCTION fms.facility_write_in_scope(p_facility_id uuid)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
  SELECT fms.is_platform_context()
      OR CASE
           WHEN p_facility_id IS NULL THEN fms.tenant_wide_write_allowed()
           ELSE fms.facility_in_scope(p_facility_id)
         END;
$$;

COMMENT ON FUNCTION fms.facility_write_in_scope(uuid) IS
  '寫入端的場域範圍判定。與 facility_in_scope 唯一的差別：NULL 不放行 —— '
  'facility_id IS NULL 代表「租戶通用」，改動它需要 TENANT 範圍（050）。'
  '讀取端仍然用 facility_in_scope：每個場域都要看得到套用在自己身上的租戶通用列。';

-- -----------------------------------------------------------------------------
-- 3. 八張表
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  t text;
  -- `facility_id` 可為 NULL、且 NULL 代表「租戶通用」的表。
  -- 名單是實測出來的（有 facility_scope 政策 + facility_id 可為 NULL +
  -- 寫入檢查落在 facility_in_scope 上），不是憑印象列的。
  scoped text[] := ARRAY[
    'alarm_rules', 'announcements', 'holiday_calendars', 'integrations',
    'quota_policies', 'service_items', 'sla_policies', 'teams'
  ];
BEGIN
  FOREACH t IN ARRAY scoped LOOP
    -- (a) 讀維持寬、寫改嚴
    EXECUTE format('DROP POLICY IF EXISTS facility_scope ON fms.%I', t);
    EXECUTE format(
      'CREATE POLICY facility_scope ON fms.%I
         AS RESTRICTIVE
         USING (fms.is_platform_context() OR fms.facility_in_scope(facility_id))
         WITH CHECK (fms.facility_write_in_scope(facility_id))', t);

    -- (b) 不能修改租戶通用列
    EXECUTE format('DROP POLICY IF EXISTS facility_scope_update ON fms.%I', t);
    EXECUTE format(
      'CREATE POLICY facility_scope_update ON fms.%I
         AS RESTRICTIVE FOR UPDATE
         USING (fms.facility_write_in_scope(facility_id))', t);

    -- (c) 不能刪除租戶通用列
    EXECUTE format('DROP POLICY IF EXISTS facility_scope_delete ON fms.%I', t);
    EXECUTE format(
      'CREATE POLICY facility_scope_delete ON fms.%I
         AS RESTRICTIVE FOR DELETE
         USING (fms.facility_write_in_scope(facility_id))', t);
  END LOOP;
END;
$$;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- CORE 位置：**跑在 009 之前**，沒有任何租戶、使用者或場域，因此不能驗
-- 「FACILITY_ADMIN 寫不進去」。那一格由整合測試負責。
-- 這裡驗結構，加上一格純函式的行為。
DO $$
DECLARE
  v_bad   text;
  v_n     bigint;
  v_saved text;
  scoped  text[] := ARRAY[
    'alarm_rules', 'announcements', 'holiday_calendars', 'integrations',
    'quota_policies', 'service_items', 'sla_policies', 'teams'
  ];
BEGIN
  -- (1) 八張表的寫入檢查都改用 facility_write_in_scope。
  SELECT string_agg(x.t, '、' ORDER BY x.t) INTO v_bad
    FROM unnest(scoped) AS x(t)
   WHERE coalesce(
           (SELECT pg_get_expr(p.polwithcheck, p.polrelid)
              FROM pg_policy p
             WHERE p.polrelid = ('fms.' || x.t)::regclass
               AND p.polname = 'facility_scope'), '') NOT LIKE '%facility_write_in_scope%';
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION
      '050 FAILED: 這些表的 facility_scope 沒有明寫用 facility_write_in_scope 的 '
      'WITH CHECK —— cmd=ALL 的政策會退回用 USING，而那對 NULL 一律放行：%', v_bad;
  END IF;

  -- (2) **讀沒有被一起收緊。** 這一格是反向保險：把 USING 也換成寫入述詞
  --     的話，場域管理員會看不到套用在自己身上的租戶通用政策，
  --     而症狀是「SLA 目標忽然變成 NOT_APPLICABLE」——
  --     離這裡很遠，而且看起來像 SLA 的 bug 而不是政策的 bug。
  SELECT string_agg(x.t, '、' ORDER BY x.t) INTO v_bad
    FROM unnest(scoped) AS x(t)
   WHERE coalesce(
           (SELECT pg_get_expr(p.polqual, p.polrelid)
              FROM pg_policy p
             WHERE p.polrelid = ('fms.' || x.t)::regclass
               AND p.polname = 'facility_scope'), '') LIKE '%facility_write_in_scope%';
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION
      '050 FAILED: 這些表的 facility_scope 把**讀**也收緊了 —— 每個場域都必須'
      '看得到套用在自己身上的租戶通用列（resolve_sla_policy 的 fallback）：%', v_bad;
  END IF;

  -- (3) UPDATE 與 DELETE 各有一條 RESTRICTIVE 政策，指令別對。
  --     只修 INSERT 會留下「你建不了我們的租戶政策，但你刪得掉」。
  SELECT string_agg(x.t || '(' || x.pol || ')', '、' ORDER BY x.t, x.pol) INTO v_bad
    FROM (SELECT t, pol, cmd
            FROM unnest(scoped) AS u(t),
                 (VALUES ('facility_scope_update', 'w'),
                         ('facility_scope_delete', 'd')) AS v(pol, cmd)) x
   WHERE NOT EXISTS (
     SELECT 1 FROM pg_policy p
      WHERE p.polrelid = ('fms.' || x.t)::regclass
        AND p.polname = x.pol
        AND NOT p.polpermissive
        AND p.polcmd = x.cmd
        AND pg_get_expr(p.polqual, p.polrelid) LIKE '%facility_write_in_scope%');
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION '050 FAILED: 這些政策缺失或形狀不對：%', v_bad;
  END IF;

  -- (4) 046 的不變量還在：沒有 facility_scope 政策是 PERMISSIVE。
  --     這裡新建了 24 條政策，而 `CREATE POLICY` 少寫 `AS RESTRICTIVE`
  --     的後果是它被 OR 進 tenant_isolation —— 也就是 038 那個缺陷。
  SELECT string_agg(c.relname || '.' || p.polname, '、' ORDER BY c.relname, p.polname)
    INTO v_bad
    FROM pg_policy p JOIN pg_class c ON c.oid = p.polrelid
   WHERE p.polname LIKE 'facility_scope%'
     AND p.polpermissive
     AND c.relnamespace = 'fms'::regnamespace;
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION '050 FAILED: 這些 facility_scope* 政策是 PERMISSIVE：%', v_bad;
  END IF;

  SELECT count(*) INTO v_n
    FROM pg_policy p JOIN pg_class c ON c.oid = p.polrelid
   WHERE p.polname = 'facility_scope' AND c.relnamespace = 'fms'::regnamespace;
  IF v_n < 28 THEN
    RAISE EXCEPTION '050 FAILED: facility_scope 政策只剩 % 條（應至少 28）', v_n;
  END IF;

  -- (5) **行為**：純函式的語意，不需要租戶資料就驗得到。
  --     平台情境放行；非平台情境下沒有使用者 → 沒有 TENANT 指派 → 不放行。
  --     這一格抓的是「把 CASE 寫反」那種錯 —— 而寫反的症狀是完全沒有保護，
  --     且結構檢查 (1)(3) 全部通過（述詞名字還在）。
  IF NOT fms.facility_write_in_scope(NULL) THEN
    RAISE EXCEPTION '050 FAILED: 平台情境應該放行租戶通用列的寫入';
  END IF;

  v_saved := coalesce(current_setting('app.is_platform', true), 'off');
  PERFORM set_config('app.is_platform', 'off', true);

  IF fms.facility_write_in_scope(NULL) THEN
    RAISE EXCEPTION
      '050 FAILED: 非平台情境、且沒有任何 TENANT 範圍指派時，'
      '不該放行 facility_id IS NULL 的寫入 —— CASE 可能寫反了';
  END IF;
  -- 反面：非 NULL 的場域仍然交給 facility_in_scope 判斷（此處 app.facility_ids
  -- 沒設，所以 current_facility_ids() 回 NULL = 不限制 → 應放行）。
  IF NOT fms.facility_write_in_scope('00000000-0000-4000-8000-000000000001') THEN
    RAISE EXCEPTION
      '050 FAILED: 非 NULL 的場域不該被這次改動影響 —— '
      '它仍該由 facility_in_scope 決定';
  END IF;

  PERFORM set_config('app.is_platform', v_saved, true);

  RAISE NOTICE '050 OK: 8 張表的租戶通用列現在需要 TENANT 範圍才能 建立／放大／修改／刪除';
END;
$$;

COMMIT;
