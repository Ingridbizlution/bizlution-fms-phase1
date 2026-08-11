-- =============================================================================
-- 062：子表補上場域收斂 —— 20 張只能透過外鍵到得了場域的表
-- =============================================================================
-- 為什麼需要它
--
-- 060 修掉三張遙測表的跨場域洩漏，而那個洩漏是**碰巧**被發現的：剛好要加
-- `/telemetry/series`，才去對照 `telemetry:read` 宣告的 `min_scope_level`
-- 與那三張表的政策。偵測機制是運氣，不是任何會重複的東西。
--
-- 所以這一支把同一類洞查完了。查法與結果：
--
--   * 有 `facility_id` 欄位卻沒有場域政策的表：**0 張**。
--   * **沒有** `facility_id`、只能透過外鍵到得了場域的表：**20 張**
--     （第一次盤點寫 15 —— 那次我用了一份手寫的父表清單，漏掉四個父表的
--     子表。正確的數字是 `rls_scope_audit_slice.rs` 的 `a_` 列舉出來的，
--     它不依賴任何手寫清單。）
--
-- 然後對其中一張（`asset_meters`）做了決定性 probe（fms_template，
-- `app.facility_ids` 只含台北總部，也就是 `begin_tenant_tx` 給 fm.lin 的情境）：
--
--   別場域的設備        0 列   ← 父表的場域 RLS 有效
--   別場域設備的計量    1 列   ← **洩漏**
--
-- 與遙測那次一模一樣的形狀：看不到父列，卻看得到子列。這不是單一個案。
--
-- -----------------------------------------------------------------------------
-- 述詞：「看得到父列就看得到子列」，而不是重新推導場域
-- -----------------------------------------------------------------------------
-- 060 用的是明寫 `facility_in_scope(d.facility_id)`，理由是不想依賴 devices
-- 的政策保持現狀。這一支**刻意不同**，理由是量出來的：
--
--   * 060 只有三張表、父表只有一個（devices）、一段 join。
--   * 這裡有 20 張表、11 個不同的父表，其中兩張要走兩段（`plan_id` →
--     `maintenance_plans` → facility）。在 14 個地方各重寫一次場域推導，
--     打錯一個欄位的症狀是**那一張表繼續洩漏而其他 19 張看起來修好了**。
--
-- 而政策運算式裡的子查詢**會套用被引用那張表自己的 RLS** —— 這一點是實測的，
-- 不是推論：對 `asset_meters` 掛上只寫 `EXISTS (SELECT 1 FROM fms.assets a
-- WHERE a.id = asset_meters.asset_id)` 的政策之後，
--
--   總部設備的計量      1 列   ← 看得到
--   別場域設備的計量    0 列   ← 看不到
--
-- （第一次測這件事時我拿「總部的計量該 > 0」去斷言，而整個資料庫只有 1 筆
-- `asset_meters` 且它就在別的場域 —— 那次的 0 是對的，結論無效。補了兩邊
-- 各一筆之後才是上面這個結果。）
--
-- 「看得到工單就看得到它的工時」本身也是更準確的敘述：子表的可見性
-- **定義上**就跟著父列，而不是恰好共用同一個場域。
--
-- -----------------------------------------------------------------------------
-- 錨點一律選 NOT NULL 的那個外鍵
-- -----------------------------------------------------------------------------
-- 可為 NULL 的外鍵配 `EXISTS` 會變成「這一列對所有人隱藏」（NULL 找不到父列
-- → false → RESTRICTIVE 擋掉）。那不是洩漏，是反過來的失效 ——
-- 而它更難發現，因為症狀是「資料不見了」而沒有錯誤。
--
-- 所以每一張表都選它**非空**的那個外鍵當錨點：
--
--   maintenance_occurrences  → plan_id（asset_id 與 work_order_id 都可空）
--   quota_transactions       → quota_policy_id（兩個業務外鍵都可空）
--   visitor_access_grants    → invitation_id（device_id 與 spatial_node_id 都可空）
--
-- 全部 11 個父表都已經有場域政策 —— 查過，鏈是完整的。
-- （這件事由 migration 自己每次執行時再確認一遍，見下面的迴圈。）
--
-- -----------------------------------------------------------------------------
-- 刻意不動的：users
-- -----------------------------------------------------------------------------
-- `users` 也在名單裡（它有 `default_facility_id` 指向 facilities，而且可為 NULL），
-- 但它**應該**是租戶級：租戶管理員必須看得到所有使用者，而場域管理員要能
-- 在派工時看到其他場域的人（026 的 `user_accessible_facilities` 就是為此）。
-- 把它場域化會讓 `/users` 與角色指派整條路徑壞掉。
--
-- -----------------------------------------------------------------------------
-- asset_relations 只錨定 from_asset_id
-- -----------------------------------------------------------------------------
-- 兩端都是 NOT NULL，所以兩者都能當錨點。選 `from`（而不是要求兩端都看得到）
-- 是刻意的：一條「總部的冷水主機供應信義影城的配電盤」的依賴關係，
-- 總部的管理員**應該**看得到 —— 那正是他排修時需要知道的事
-- （停這台會影響到別棟）。要求兩端都在範圍內會把跨場域依賴整個藏起來，
-- 而跨場域依賴恰好是最需要被看見的那一種。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;

-- (child, 錨點外鍵, 父表) 的對應表寫在一個地方 —— 20 條幾乎相同的
-- CREATE POLICY 各自展開的話，改一處漏一處的機會遠高於現在。
DO $$
DECLARE
  v_map text[][] := ARRAY[
    -- 工單子表：五張，父表都是 work_orders
    ARRAY['work_order_tasks',        'work_order_id',        'work_orders'],
    ARRAY['work_order_labor',        'work_order_id',        'work_orders'],
    ARRAY['work_order_parts',        'work_order_id',        'work_orders'],
    ARRAY['work_order_comments',     'work_order_id',        'work_orders'],
    ARRAY['work_order_transitions',  'work_order_id',        'work_orders'],
    -- 設備子表
    ARRAY['asset_meters',            'asset_id',             'assets'],
    ARRAY['asset_status_history',    'asset_id',             'assets'],
    ARRAY['asset_relations',         'from_asset_id',        'assets'],
    -- 預約子表
    ARRAY['reservation_participants','reservation_id',       'reservations'],
    ARRAY['reservation_services',    'reservation_id',       'reservations'],
    ARRAY['resource_amenities',      'bookable_resource_id', 'bookable_resources'],
    -- 兩段路徑的三張（錨點是唯一非空的那個外鍵）
    ARRAY['maintenance_occurrences', 'plan_id',              'maintenance_plans'],
    ARRAY['quota_transactions',      'quota_policy_id',      'quota_policies'],
    ARRAY['visitor_access_grants',   'invitation_id',        'visitor_invitations'],
    -- 這六張是 rls_scope_audit_slice.rs 的 `a_` 找出來的，不是我列的。
    -- 我第一次盤點用了一份**手寫的父表清單**（assets／work_orders／
    -- reservations…），於是漏掉了 announcements／integrations／
    -- quota_policies／teams 的子表 —— 檔頭原本寫「15 張」，那個數字是錯的。
    ARRAY['team_members',            'team_id',              'teams'],
    ARRAY['team_shifts',             'team_id',              'teams'],
    ARRAY['announcement_reads',      'announcement_id',      'announcements'],
    ARRAY['quota_assignments',       'quota_policy_id',      'quota_policies'],
    ARRAY['quota_usage',             'quota_policy_id',      'quota_policies'],
    ARRAY['integration_user_grants', 'integration_id',       'integrations']
  ];
  v_child  text;
  v_fk     text;
  v_parent text;
  i        int;
BEGIN
  FOR i IN 1 .. array_length(v_map, 1) LOOP
    v_child  := v_map[i][1];
    v_fk     := v_map[i][2];
    v_parent := v_map[i][3];

    -- 欄位存在與欄位非空是**兩件事**，訊息必須分開。
    -- 混成一句的代價實測過：把錨點打成一個不存在的欄位名時，
    -- 它會說「可為 NULL」，而那把人指向完全錯誤的方向。
    IF NOT EXISTS (
      SELECT 1 FROM pg_attribute a
       WHERE a.attrelid = ('fms.' || v_child)::regclass
         AND a.attname = v_fk AND a.attnum > 0 AND NOT a.attisdropped
    ) THEN
      RAISE EXCEPTION '062 FAILED: %.% 這個欄位不存在（錨點名稱打錯？）', v_child, v_fk;
    END IF;

    -- 可空的錨點配 EXISTS 會讓整張表對所有人隱藏 ——
    -- 那不是洩漏，是反過來的失效，而症狀（資料不見了）比洩漏更難追。
    IF NOT EXISTS (
      SELECT 1 FROM pg_attribute a
       WHERE a.attrelid = ('fms.' || v_child)::regclass
         AND a.attname = v_fk AND a.attnotnull AND NOT a.attisdropped
    ) THEN
      RAISE EXCEPTION
        '062 FAILED: %.% 可為 NULL，不能當錨點 —— 用它會把整張表對所有人隱藏',
        v_child, v_fk;
    END IF;

    -- 父表自己必須已經場域收斂，否則這條政策什麼也沒收斂。
    IF NOT EXISTS (
      SELECT 1 FROM pg_policies p
       WHERE p.schemaname = 'fms' AND p.tablename = v_parent
         AND p.permissive = 'RESTRICTIVE' AND coalesce(p.qual, '') LIKE '%facility%'
    ) THEN
      RAISE EXCEPTION
        '062 FAILED: 父表 % 沒有場域政策 —— % 掛上去也不會收斂任何東西',
        v_parent, v_child;
    END IF;

    -- 先 DROP：020／023／024／038 建政策時都是這個寫法。
    -- 少了它，這支 migration 重跑會在第一張表就撞上 "already exists"，
    -- 而那讓突變測試跑不起來（實際踩到過）。
    EXECUTE format('DROP POLICY IF EXISTS facility_scope_via_parent ON fms.%I', v_child);
    EXECUTE format(
      'CREATE POLICY facility_scope_via_parent ON fms.%I
         AS RESTRICTIVE FOR ALL
         USING (fms.is_platform_context()
                OR EXISTS (SELECT 1 FROM fms.%I p WHERE p.id = fms.%I.%I))',
      v_child, v_parent, v_child, v_fk);
  END LOOP;
END;
$$;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構檢查在這裡；**行為驗證（別場域真的看不到）在 rls_scope_audit_slice.rs**。
-- 理由與 059／060 相同：這支 migration 跑在 seed 009 之前，
-- 沒有場域級使用者的情境可用。
DO $$
DECLARE
  v_missing text[];
  v_n       int;
BEGIN
  -- (1) 20 張都要有。漏一張的症狀是「另外 19 張擋住了，看起來像修好了」。
  SELECT array_agg(t ORDER BY t) INTO v_missing
    FROM unnest(ARRAY[
      'work_order_tasks','work_order_labor','work_order_parts','work_order_comments',
      'work_order_transitions','asset_meters','asset_status_history','asset_relations',
      'reservation_participants','reservation_services','resource_amenities',
      'maintenance_occurrences','quota_transactions','visitor_access_grants',
      'team_members','team_shifts','announcement_reads','quota_assignments',
      'quota_usage','integration_user_grants']) AS t
   WHERE NOT EXISTS (
     SELECT 1 FROM pg_policies p
      WHERE p.schemaname = 'fms' AND p.tablename = t
        AND p.policyname = 'facility_scope_via_parent' AND p.permissive = 'RESTRICTIVE');
  IF v_missing IS NOT NULL THEN
    RAISE EXCEPTION '062 FAILED: 這些子表沒有 facility_scope_via_parent：%', v_missing;
  END IF;

  -- (2) `users` 必須**仍然是**租戶級。這一條是反向守衛：
  --     未來有人「順手把剩下那一張也補上」會讓 /users 與角色指派整條路徑壞掉。
  IF EXISTS (
    SELECT 1 FROM pg_policies p
     WHERE p.schemaname = 'fms' AND p.tablename = 'users'
       AND p.permissive = 'RESTRICTIVE' AND coalesce(p.qual, '') LIKE '%facility%'
  ) THEN
    RAISE EXCEPTION
      '062 FAILED: users 被場域收斂了 —— 租戶管理員必須看得到所有使用者，'
      '而派工需要看到其他場域的人（見檔頭）';
  END IF;

  SELECT count(*) INTO v_n FROM pg_policies
   WHERE schemaname = 'fms' AND policyname = 'facility_scope_via_parent';
  IF v_n <> 20 THEN
    RAISE EXCEPTION '062 FAILED: 預期 20 條政策，實際 % 條', v_n;
  END IF;

  RAISE NOTICE '062 OK：% 張子表補上場域收斂（靠父表 RLS）、users 仍為租戶級'
               '（行為驗證在 rls_scope_audit_slice.rs）', v_n;
END;
$$;

COMMIT;
