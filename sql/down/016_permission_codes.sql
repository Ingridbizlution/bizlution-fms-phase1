-- 回退 016：user_has_permission 恢復為 002 的獨立實作，移除集合版函式。
--
-- 恢復的是 002 原始的判定式（與 012 的 T12 持有的參考複本相同）。
BEGIN;
SET search_path = fms, public;

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
    FROM fms.v_user_effective_permissions ep
    LEFT JOIN fms.facilities f ON f.id = p_facility_id
    LEFT JOIN fms.organizations o_target ON o_target.id = coalesce(p_org_id, f.org_id)
    LEFT JOIN fms.organizations o_scope  ON o_scope.id = ep.scope_id
    WHERE ep.user_id = p_user_id
      AND ep.permission_code = p_permission
      AND (
            ep.scope_type = 'TENANT'
        OR (ep.scope_type = 'FACILITY' AND ep.scope_id = p_facility_id)
        OR (ep.scope_type = 'ORG'
            AND o_scope.org_path IS NOT NULL
            AND o_target.org_path IS NOT NULL
            AND o_target.org_path OPERATOR(public.<@) o_scope.org_path)
      )
  );
$$;

DROP FUNCTION IF EXISTS fms.user_permission_codes_anywhere(uuid);
DROP FUNCTION IF EXISTS fms.user_permission_codes(uuid, uuid, uuid);

COMMIT;
