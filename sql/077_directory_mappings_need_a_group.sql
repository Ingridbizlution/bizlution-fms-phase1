-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 077: 目錄對應必須錨定在一個已同步的群組上
-- =============================================================================
-- `fms.directory_role_mappings` 從 002 起允許兩種比對來源，二選一即可
-- （`ck_drm_source`）：
--
--   * `directory_group_id` —— 指向一列已同步的 `directory_groups`
--   * `claim_value`        —— 直接比對原始 claim 值
--                             （002 的註解：「第一次群組同步之前」需要）
--
-- 而 `POST /directory-role-mappings` 照著那條約束放行，只填 `claim_value`
-- 也回 201。
--
-- **但對帳完全不讀 `claim_value`。** 058 的發放迴圈是內連接：
--
--     FROM fms.directory_role_mappings m
--     JOIN fms.directory_groups g ON g.id = m.directory_group_id
--     JOIN fms.user_directory_groups udg ON udg.directory_group_id = g.id
--
-- `directory_group_id IS NULL` 的列在第一個 JOIN 就被丟掉，沒有錯誤、
-- 沒有計數、不進 `blocked_mappings`。全 codebase 除了那組 CRUD 與它的測試，
-- 沒有任何一處讀 `claim_value`。
--
-- 也就是說：管理者建一條只有 `claim_value` 的對應，拿到 201，
-- 然後那條規則永遠不會授予任何角色，而且**沒有任何症狀**。
-- 這是這個 repo 反覆出現的缺陷類型 —— 動作成功了，
-- 但它沒有達成使用者以為它達成的事。
--
-- -----------------------------------------------------------------------------
-- 為什麼修法不是「讓 058 支援 claim_value」
-- -----------------------------------------------------------------------------
-- 先量測「claim 值該跟什麼比」，四個候選全都走不通：
--
--   1. `user_identities.raw_claims -> identity_providers.group_claim_name`
--      —— 語意上唯一正確的目標（「原始 claim 值」就是這個）。
--      但 `fms.user_identities` 在 002 的 CREATE TABLE 之外**被零處參照**：
--      沒有 Rust、沒有其他 migration、009 也不種它。`raw_claims` 從來沒有
--      被寫入過，因為 **Phase 1 沒有 OIDC／SAML／LDAP 的登入流程**
--      （`handlers.rs` 只驗本地密碼，非 LOCAL 帳號一律 `NO_LOCAL_PASSWORD`）。
--      比對一張永遠是空的表，那條規則還是不會生效 —— 只是多了程式碼。
--
--   2. `directory_groups.name`
--   3. `directory_groups.external_group_id`
--   4. `directory_groups.distinguished_name`
--      —— 這三個都是**已同步的群組列**上的欄位。群組列存在時
--      `directory_group_id` 就拿得到，`claim_value` 依定義是多餘的；
--      而在 002 說它存在的理由（「第一次同步之前」）那個時刻，
--      群組列還不存在，所以照樣比不到東西。用它們只會讓 `claim_value`
--      變成 `directory_group_id` 的一個較慢的別名。
--
-- 還有兩件事就算給了 claim 來源也仍然缺：
--
--   * **成員關係**。發放需要「這個人屬於這個來源」這個事實，而唯一的存放處是
--     `user_directory_groups`，它的鍵是 `directory_group_id`。
--     沒有以 claim 為鍵的對應物。
--   * **收回**。`user_role_assignments` 唯一的回指欄位是
--     `origin_directory_group_id`，而 058 的收回是
--     `USING fms.directory_groups g WHERE ura.origin_directory_group_id = g.id`。
--     由 claim 產生的授權沒有群組可填，於是**發得出去、收不回來** ——
--     那正是 058 檔頭說絕不可以發生的事（「只加不減會讓改對應沒有效果」）。
--
-- 所以「支援 claim_value」不是補一個 JOIN，是要先做出 OIDC／SAML 登入流程、
-- 一個以 claim 為鍵的成員關係存放處、以及一個以 claim 為鍵的收回身分。
-- 那是 connector 那一半的工作，而且**假裝做完會留下比現在更糟的狀態**：
-- 一條發得出去卻收不回來的授權路徑。
--
-- -----------------------------------------------------------------------------
-- 為什麼是「建立時就擋掉」，而不是「建得起來但回報它不會生效」
-- -----------------------------------------------------------------------------
-- 同一支 handler 已經對同一類缺陷做過這個決定：`scope_type = SPATIAL_NODE`
-- 的對應會產生**一項權限都不生效**的授權（016 的述詞只認三種 scope_type），
-- 而它回的是 422 而不是「201 + 一句提醒」。
--
-- 理由相同：一條永遠不會生效的授權規則不是資訊不足，是設定錯誤。
-- 讓它存在，管理者就會以為某個存取受一條規則管著，而那條規則什麼都沒做 ——
-- 那比「建不起來」危險，因為前者要靠人記得去看一個計數，後者當場就知道。
-- 而 422 說得出**真正的前置條件**（先讓群組同步進來），
-- 一句「這條目前不會生效」說不出。
--
-- -----------------------------------------------------------------------------
-- 為什麼約束放在資料庫，而不是只在 handler
-- -----------------------------------------------------------------------------
-- 058 檔頭自己記過這件事：「對應可以被種子或手寫 SQL 建立（繞過 handler 的
-- 052 檢查）」。同一句話對這裡成立 —— 只擋 handler 的話，
-- 種子與維運腳本仍然造得出永遠不會生效的對應列。
--
-- `claim_value` 欄位**保留**（不 DROP）：它不是壞主意，是還沒有消費者。
-- connector 進來時，它會連同「以 claim 為鍵的成員關係與收回身分」
-- 一起變得可用，而那一份變更必須同時放寬這裡的約束
-- —— 見下方自我驗證的第 (4) 格。
--
-- -----------------------------------------------------------------------------
-- 既有的違規列：大聲失敗，不代為刪除
-- -----------------------------------------------------------------------------
-- 這些列**不曾授予任何權限**，所以刪掉它們不會改變任何實際授權。
-- 但它們記錄的是管理者的**意圖**，而正確的處置是把那個意圖接到一個真的群組上，
-- 不是讓一支 migration 靜默丟掉它。因此這裡列出它們並失敗，
-- 附上可直接執行的處置 SQL。
--
-- 依賴：002（directory_role_mappings／directory_groups／ck_drm_source）、
--       058（reconcile_directory_roles —— 那個內連接是這條約束的理由）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 既有的違規列
-- -----------------------------------------------------------------------------
-- 平台情境：這支 migration 要看**所有租戶**的列，而 007 的 FORCE RLS
-- 對擁有者也生效（046 檔頭記過這件事）。
SET app.is_platform = 'on';

DO $$
DECLARE
  v_n    bigint;
  v_ids  text;
BEGIN
  SELECT count(*), string_agg(id::text, ', ' ORDER BY id)
    INTO v_n, v_ids
    FROM fms.directory_role_mappings
   WHERE directory_group_id IS NULL;

  IF v_n > 0 THEN
    RAISE EXCEPTION
      '077 停止：有 % 條只填 claim_value 的對應，它們永遠不會授予任何角色。'
      ' id：%。'
      ' 這些列不曾生效，刪掉不影響任何實際授權；但請先確認那個意圖要接到哪個'
      ' 已同步的群組上。處置：'
      ' UPDATE fms.directory_role_mappings SET directory_group_id = ''<群組 id>'''
      ' WHERE id = ''<對應 id>'';  或'
      ' DELETE FROM fms.directory_role_mappings WHERE directory_group_id IS NULL;',
      v_n, v_ids;
  END IF;
END;
$$;

-- -----------------------------------------------------------------------------
-- (2) 換掉約束
-- -----------------------------------------------------------------------------
-- `ck_drm_source`（二選一）換成「一定要有群組」。
-- `claim_value` 仍可與群組並存 —— 欄位保留給 connector，見檔頭。
ALTER TABLE fms.directory_role_mappings
  DROP CONSTRAINT IF EXISTS ck_drm_source;

ALTER TABLE fms.directory_role_mappings
  ADD CONSTRAINT ck_drm_group_required CHECK (directory_group_id IS NOT NULL);

COMMENT ON COLUMN fms.directory_role_mappings.claim_value IS
  '保留給未來的 connector，目前**沒有任何消費者**，因此 API 不接受寫入。'
  ' 002 原本讓它作為 directory_group_id 的替代來源（「第一次群組同步之前」），'
  ' 但 058 的對帳是對 directory_groups 的內連接，只填這個欄位的對應會被靜默'
  ' 丟掉；而且 Phase 1 沒有登入流程會寫入 user_identities.raw_claims，'
  ' 也沒有以 claim 為鍵的成員關係與收回身分（user_role_assignments 只有'
  ' origin_directory_group_id 這一個回指）。要啟用它必須連同那三件一起做，'
  ' 並在同一份變更裡放寬 ck_drm_group_required。見 migration 077。';

COMMENT ON CONSTRAINT ck_drm_group_required ON fms.directory_role_mappings IS
  '每一條對應都必須錨定在一列已同步的群組上，否則 058 的對帳會靜默丟掉它 ——'
  ' 建得起來、回 201、永遠不授予任何角色、沒有症狀。見 migration 077。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 行為的，不是只看 pg_constraint 有沒有那個名字：這個缺陷的症狀正是
-- 「宣告在那裡但沒有人執行」，而「約束存在」與「約束真的擋得住」是兩件事。
--
-- CORE 階段沒有種子（009 在 CORE 之後），因此自己建一個拋棄式租戶、
-- provider 與群組，全部在區塊內回捲 —— 與 069 同一個做法。
DO $$
DECLARE
  v_tenant uuid;
  v_idp    uuid;
  v_group  uuid;
  v_role   uuid;
  v_src    text;
BEGIN
  SELECT id INTO v_role FROM fms.roles WHERE code = 'VIEWER' AND tenant_id IS NULL;
  IF v_role IS NULL THEN
    RAISE EXCEPTION '077 FAILED: 找不到平台角色 VIEWER（008 應該已經種下）';
  END IF;

  INSERT INTO fms.tenants (code, name) VALUES ('T077_DRM', '077 對應驗證')
    RETURNING id INTO v_tenant;
  INSERT INTO fms.identity_providers (tenant_id, code, name, provider_type)
    VALUES (v_tenant, 'local-077', '077 本地來源', 'LOCAL')
    RETURNING id INTO v_idp;
  INSERT INTO fms.directory_groups
    (tenant_id, identity_provider_id, external_group_id, name)
    VALUES (v_tenant, v_idp, 'ext-077', '077 群組')
    RETURNING id INTO v_group;

  -- (1) 只有 claim_value → 必須被擋。這一格就是缺陷本身。
  BEGIN
    INSERT INTO fms.directory_role_mappings
      (tenant_id, directory_group_id, claim_value, role_id, scope_type)
      VALUES (v_tenant, NULL, 'CN=X,DC=example,DC=com', v_role, 'TENANT');
    RAISE EXCEPTION
      '077 FAILED: 只填 claim_value 的對應仍然建得起來 —— '
      '而 058 的對帳是內連接，它會被靜默丟掉，永遠不授予任何角色';
  EXCEPTION WHEN check_violation THEN
    NULL;
  END;

  -- (2) 兩個都沒有 → 一樣要被擋。
  --     少了這一格，把約束寫成 `claim_value IS NULL` 之類的反向條件也會通過 (1)。
  BEGIN
    INSERT INTO fms.directory_role_mappings
      (tenant_id, directory_group_id, claim_value, role_id, scope_type)
      VALUES (v_tenant, NULL, NULL, v_role, 'TENANT');
    RAISE EXCEPTION '077 FAILED: 兩個來源都沒有的對應建得起來';
  EXCEPTION WHEN check_violation THEN
    NULL;
  END;

  -- (3) 有群組 → 必須仍然建得起來。
  --     少了這一格，一個「什麼都擋」的約束也會通過 (1) 與 (2)，
  --     而那會讓整個目錄同步失去唯一一條能用的路徑。
  --
  --     自己接住 check_violation 再改寫訊息：直接讓它冒上去只會得到一句
  --     「violates check constraint」，說不出「被誤擋的是合法的對應」。
  BEGIN
    INSERT INTO fms.directory_role_mappings
      (tenant_id, directory_group_id, role_id, scope_type)
      VALUES (v_tenant, v_group, v_role, 'TENANT');
  EXCEPTION WHEN check_violation THEN
    RAISE EXCEPTION
      '077 FAILED: 錨定在已同步群組上的對應也被擋下來了 —— '
      '約束收得太緊，目錄同步會失去唯一一條能用的路徑';
  END;
  IF NOT EXISTS (SELECT 1 FROM fms.directory_role_mappings
                  WHERE directory_group_id = v_group) THEN
    RAISE EXCEPTION '077 FAILED: 錨定在群組上的對應沒有建起來';
  END IF;

  -- (4) **這條約束的正當性來自 058 不讀 claim_value。**
  --     若哪天 058 真的支援了 claim 比對，這裡就必須同時放寬 ——
  --     否則會變成反過來的那個缺陷：實作支援了，但約束不讓你用。
  v_src := pg_get_functiondef('fms.reconcile_directory_roles(uuid,uuid)'::regprocedure);
  IF v_src LIKE '%claim_value%' THEN
    RAISE EXCEPTION
      '077 FAILED: 058 的對帳已經讀 claim_value 了，但約束仍然要求群組 —— '
      '兩邊必須在同一份變更裡一起改（見 077 檔頭）';
  END IF;

  RAISE EXCEPTION 'ROLLBACK_077_SELFTEST';
EXCEPTION WHEN others THEN
  IF SQLERRM = 'ROLLBACK_077_SELFTEST' THEN
    RAISE NOTICE '077 OK：只有 claim_value 的對應擋得住、兩個都沒有也擋得住、'
                 '錨定在群組上的仍然建得起來，且 058 確實還沒讀 claim_value'
                 '（行為驗證在 directory_mappings_slice.rs）';
  ELSE
    RAISE;
  END IF;
END;
$$;

COMMIT;
