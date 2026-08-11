-- 回退 069。
--
-- **把守衛拿掉會讓循環重新變成可達的**，而它的後果是資料損毀而不是錯誤
-- （見 069 的檔頭）。所以這支 down 只是為了讓 migrate-roundtrip 能驗
-- schema 可逆 —— 實務上不該執行它。
--
-- 兩支函式還原成 001／003 的版本（只擋自己當自己的父節點）。
BEGIN;
SET search_path = fms, public;

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

COMMIT;
