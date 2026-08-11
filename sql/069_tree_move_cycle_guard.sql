-- =============================================================================
-- 069：樹搬移的循環守衛（organizations 與 spatial_nodes）
-- =============================================================================
-- 001 與 003 的 re-path 觸發器都會在搬移時把整個子樹重新編路徑，而兩者都只擋
-- 了「把自己設成自己的父節點」（`NEW.parent_id = OLD.id`）。
--
-- **把一個節點搬到它自己的後代底下沒有被擋，而結果是資料損毀。** 量出來的：
--
--   CYC_A（root）→ CYC_B（CYC_A 的子節點）
--   UPDATE organizations SET parent_id = CYC_B WHERE code = 'CYC_A';
--
--   code  | org_path                 | parent
--   ------+--------------------------+--------
--   CYC_A | CYC_A.CYC_B.CYC_A        | CYC_B
--   CYC_B | CYC_A.CYC_B.CYC_A.CYC_B  | CYC_A
--
-- 兩者都成了自己的祖先、`parent_id` 形成一個 2-cycle，而**沒有任何錯誤**。
--
-- -----------------------------------------------------------------------------
-- 為什麼這件事比看起來嚴重
-- -----------------------------------------------------------------------------
-- ltree 的 `<@` 是整個系統做子樹彙總的方式，而損毀之後它永遠回錯的答案：
--
--   * `report_group_rollup`（065）用 `org_path <@` 算集團彙總 ——
--     一個循環會讓某些設施被算兩次、某些組織的數字含它自己的祖先。
--   * `GET /organizations?subtree_of=` 與 `spatial_nodes` 的子樹查詢同理。
--   * 而症狀不是錯誤，是**數字不對** —— 那種東西會被拿去談合約。
--
-- 更糟的是它**不可自我修復**：路徑已經寫壞，之後任何一次搬移都以壞掉的路徑
-- 為基準再算一次。
--
-- -----------------------------------------------------------------------------
-- 為什麼守衛放在觸發器而不是應用層
-- -----------------------------------------------------------------------------
-- 這與 `fms-workorder` 檔頭記過的取捨相反，而理由是**這一條沒有應用層的
-- 對應物**：re-path 本身就在觸發器裡，任何寫入者（API、migration、維運腳本、
-- 未來的匯入工具）都會觸發它。把守衛留在應用層等於「只有走 API 的搬移是安全的」，
-- 而 003 的搬移行為本來就是由觸發器定義的。
--
-- 依賴：001（organizations 與 trg_organization_path）、
--       003（spatial_nodes 與 trg_spatial_node_path）。
--       兩支都用 `CREATE OR REPLACE` 取代既有函式（觸發器本身不動）——
--       與 061 取代 057 的評估器同一個做法。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) organizations
-- -----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.trg_organization_path()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_parent_path ltree;
  v_old_path    ltree;
  v_label       text := regexp_replace(NEW.code, '[^A-Za-z0-9_]', '_', 'g');
BEGIN
  IF NEW.parent_id IS NULL THEN
    NEW.org_path := text2ltree(v_label);
  ELSE
    SELECT org_path INTO v_parent_path FROM fms.organizations WHERE id = NEW.parent_id;
    IF v_parent_path IS NULL THEN
      RAISE EXCEPTION 'parent organization % not found', NEW.parent_id USING ERRCODE = '23503';
    END IF;
    NEW.org_path := v_parent_path || text2ltree(v_label);
  END IF;

  IF TG_OP = 'UPDATE' THEN
    IF NEW.parent_id = OLD.id THEN
      RAISE EXCEPTION 'an organization cannot be its own parent' USING ERRCODE = '23514';
    END IF;

    -- **069 新增的守衛。** 新父節點若在自己的子樹裡（含自己），搬移會產生
    -- 一個循環，而那不會報錯、只會讓 `<@` 從此回錯的答案。見檔頭的量測。
    --
    -- 用 `<@` 比對舊路徑而不是遞迴走 `parent_id`：路徑就是為了這個而存在的，
    -- 而 `idx_organizations_path` 是 GiST 索引，所以這個檢查是一次索引查找。
    IF NEW.parent_id IS NOT NULL
       AND v_parent_path OPERATOR(public.<@) OLD.org_path THEN
      RAISE EXCEPTION
        'cannot move organization % under its own descendant %: that creates a cycle',
        OLD.code, NEW.parent_id
        USING ERRCODE = '23514', HINT = 'TREE_CYCLE';
    END IF;

    v_old_path := OLD.org_path;
    IF v_old_path IS DISTINCT FROM NEW.org_path THEN
      UPDATE fms.organizations
         SET org_path = NEW.org_path || subpath(org_path, nlevel(v_old_path))
       WHERE tenant_id = NEW.tenant_id
         AND org_path OPERATOR(public.<@) v_old_path
         AND id <> NEW.id;
    END IF;
  END IF;

  RETURN NEW;
END;
$$;

-- -----------------------------------------------------------------------------
-- (2) spatial_nodes
-- -----------------------------------------------------------------------------
-- 同一個缺陷、同一個修法。**兩份實作要一起改** —— 只改一邊會留下一個
-- 「組織不會壞但空間會壞」的系統，而兩者的子樹查詢用途完全一樣。
CREATE OR REPLACE FUNCTION fms.trg_spatial_node_path()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_parent_path ltree;
  v_old_path    ltree;
BEGIN
  IF NEW.parent_id IS NULL THEN
    NEW.node_path := text2ltree(regexp_replace(NEW.code, '[^A-Za-z0-9_]', '_', 'g'));
  ELSE
    SELECT node_path INTO v_parent_path FROM fms.spatial_nodes WHERE id = NEW.parent_id;
    IF v_parent_path IS NULL THEN
      RAISE EXCEPTION 'parent spatial node % not found', NEW.parent_id USING ERRCODE = '23503';
    END IF;
    NEW.node_path := v_parent_path || text2ltree(regexp_replace(NEW.code, '[^A-Za-z0-9_]', '_', 'g'));
  END IF;

  NEW.depth := nlevel(NEW.node_path) - 1;

  IF TG_OP = 'UPDATE' THEN
    v_old_path := OLD.node_path;
    IF NEW.parent_id IS NOT NULL AND NEW.parent_id = OLD.id THEN
      RAISE EXCEPTION 'a spatial node cannot be its own parent' USING ERRCODE = '23514';
    END IF;

    -- 069 的守衛，理由同上。
    IF NEW.parent_id IS NOT NULL
       AND v_parent_path OPERATOR(public.<@) v_old_path THEN
      RAISE EXCEPTION
        'cannot move spatial node % under its own descendant %: that creates a cycle',
        OLD.code, NEW.parent_id
        USING ERRCODE = '23514', HINT = 'TREE_CYCLE';
    END IF;

    IF v_old_path IS DISTINCT FROM NEW.node_path THEN
      UPDATE fms.spatial_nodes
         SET node_path = NEW.node_path || subpath(node_path, nlevel(v_old_path)),
             depth     = nlevel(NEW.node_path || subpath(node_path, nlevel(v_old_path))) - 1
       WHERE facility_id = NEW.facility_id
         AND node_path OPERATOR(public.<@) v_old_path
         AND id <> NEW.id;
    END IF;
  END IF;

  RETURN NEW;
END;
$$;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- **這一支的驗證必須是行為的，不能只看程式碼**：那個缺陷的症狀是「沒有錯誤
-- 但資料壞了」，而結構檢查看不出「有沒有真的擋住」。
--
-- 這支 migration 跑在種子 009 之後嗎？不是 —— 它在 CORE 階段。但這裡不需要
-- 種子：自己建三層再回捲即可。租戶那一列必須存在（外鍵），所以用
-- 平台情境建一個拋棄式租戶並在 savepoint 內回捲。
SET app.is_platform = 'on';

DO $$
DECLARE
  v_tenant uuid;
  v_a      uuid;
  v_b      uuid;
  v_c      uuid;
  v_msg    text;
  v_fac    uuid;
  v_na     uuid;
  v_nb     uuid;
BEGIN
  -- ---- organizations ----
  INSERT INTO fms.tenants (code, name) VALUES ('T069_GUARD', '069 守衛驗證')
    RETURNING id INTO v_tenant;
  INSERT INTO fms.organizations (tenant_id, code, name, parent_id)
    VALUES (v_tenant, 'G_A', 'A', NULL) RETURNING id INTO v_a;
  INSERT INTO fms.organizations (tenant_id, code, name, parent_id)
    VALUES (v_tenant, 'G_B', 'B', v_a) RETURNING id INTO v_b;
  INSERT INTO fms.organizations (tenant_id, code, name, parent_id)
    VALUES (v_tenant, 'G_C', 'C', v_b) RETURNING id INTO v_c;

  -- (1) 搬到直接子節點底下 → 必須被擋。
  BEGIN
    UPDATE fms.organizations SET parent_id = v_b WHERE id = v_a;
    RAISE EXCEPTION
      '069 FAILED: 把組織搬到它的直接子節點底下沒有被擋 —— '
      '結果是兩者互為祖先，而 <@ 從此回錯的答案（見檔頭的量測）';
  EXCEPTION WHEN check_violation THEN
    GET STACKED DIAGNOSTICS v_msg = MESSAGE_TEXT;
    IF v_msg NOT LIKE '%cycle%' THEN
      RAISE EXCEPTION '069 FAILED: 擋下來了但訊息沒有說出是循環：%', v_msg;
    END IF;
  END;

  -- (2) 搬到**孫節點**底下 → 一樣必須被擋。這一格與 (1) 不同：
  --     只比對「新父節點是不是我的直接子節點」的實作會漏掉它。
  BEGIN
    UPDATE fms.organizations SET parent_id = v_c WHERE id = v_a;
    RAISE EXCEPTION
      '069 FAILED: 把組織搬到它的**孫**節點底下沒有被擋 —— '
      '只檢查直接子節點的實作會漏掉這一條';
  EXCEPTION WHEN check_violation THEN
    NULL;
  END;

  -- (3) 合法的搬移仍然要成功，而且子樹要跟著走。
  --     少了這一格，一個「什麼都擋」的守衛也會通過前兩格。
  UPDATE fms.organizations SET parent_id = NULL WHERE id = v_b;
  IF (SELECT org_path::text FROM fms.organizations WHERE id = v_b) <> 'G_B' THEN
    RAISE EXCEPTION '069 FAILED: 合法的搬移沒有生效';
  END IF;
  IF (SELECT org_path::text FROM fms.organizations WHERE id = v_c) <> 'G_B.G_C' THEN
    RAISE EXCEPTION
      '069 FAILED: 子樹沒有跟著搬 —— 孫節點的路徑仍然指向舊的祖先';
  END IF;

  -- ---- spatial_nodes（同一個缺陷、同一個修法，所以同樣要驗）----
  INSERT INTO fms.facilities (tenant_id, org_id, code, name, facility_type, timezone)
    VALUES (v_tenant, v_b, 'F069', '069 場域', 'OFFICE', 'Asia/Taipei')
    RETURNING id INTO v_fac;
  -- 欄位是 `node_type_code`（不是 `node_type`），而它參照 003 的型別目錄。
  INSERT INTO fms.spatial_nodes
    (tenant_id, facility_id, code, name, node_type_code, parent_id)
    VALUES (v_tenant, v_fac, 'N_A', 'A', 'BUILDING', NULL) RETURNING id INTO v_na;
  INSERT INTO fms.spatial_nodes
    (tenant_id, facility_id, code, name, node_type_code, parent_id)
    VALUES (v_tenant, v_fac, 'N_B', 'B', 'FLOOR', v_na) RETURNING id INTO v_nb;

  BEGIN
    UPDATE fms.spatial_nodes SET parent_id = v_nb WHERE id = v_na;
    RAISE EXCEPTION
      '069 FAILED: spatial_nodes 的循環沒有被擋 —— '
      '只改 organizations 會留下一個「組織不會壞但空間會壞」的系統';
  EXCEPTION WHEN check_violation THEN
    NULL;
  END;

  -- 全部回捲：這支 migration 不該留下任何資料。
  RAISE EXCEPTION 'ROLLBACK_069_SELFTEST';
EXCEPTION WHEN others THEN
  IF SQLERRM = 'ROLLBACK_069_SELFTEST' THEN
    RAISE NOTICE '069 OK：兩棵樹的搬移都擋得住循環（直接子節點與孫節點），'
                 '而合法的搬移仍然會帶著子樹一起走';
  ELSE
    RAISE;
  END IF;
END;
$$;

COMMIT;
