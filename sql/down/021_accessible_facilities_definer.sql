-- 回退 021：user_accessible_facilities 恢復為 002 的 SECURITY INVOKER 版本。
--
-- 注意這會讓「建立場域後看不到它」的循環回來（見 021 檔頭與
-- WBS-rebaseline 4.1r）。回退這一個 migration 就等於接受那個缺陷，
-- 因此通常應該連 020 一起回退，或根本不要回退它。
BEGIN;
SET search_path = fms, public;

CREATE OR REPLACE FUNCTION fms.user_accessible_facilities(p_user_id uuid)
RETURNS TABLE (facility_id uuid)
LANGUAGE sql STABLE
AS $$
  SELECT DISTINCT f.id
  FROM fms.facilities f
  JOIN fms.organizations o ON o.id = f.org_id
  WHERE f.deleted_at IS NULL
    AND EXISTS (
      SELECT 1
      FROM fms.user_role_assignments ura
      LEFT JOIN fms.organizations os ON os.id = ura.scope_id
      WHERE ura.user_id = p_user_id
        AND ura.tenant_id = f.tenant_id
        AND (ura.valid_until IS NULL OR ura.valid_until > clock_timestamp())
        AND (
              ura.scope_type = 'TENANT'
          OR (ura.scope_type = 'FACILITY' AND ura.scope_id = f.id)
          OR (ura.scope_type = 'ORG' AND os.org_path IS NOT NULL
              AND o.org_path OPERATOR(public.<@) os.org_path)
        )
    );
$$;

DO $$
BEGIN
  IF (SELECT prosecdef FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
       WHERE n.nspname='fms' AND p.proname='user_accessible_facilities') THEN
    RAISE EXCEPTION 'down 021 FAILED: 函式仍是 SECURITY DEFINER';
  END IF;
END; $$;

COMMIT;
