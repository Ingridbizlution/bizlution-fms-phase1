-- =============================================================================
-- Down migration 052：移除角色指派的提權防護
-- =============================================================================
-- 回退之後 permissions.is_dangerous 又回到「無人讀取」的狀態，而
-- POST /users/{id}/role-assignments 會因為找不到這支函式而整支失敗
-- （不是「放行」—— 那是刻意的：判定不見了就不該還能指派）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

DROP FUNCTION IF EXISTS fms.role_grant_blocked_by(uuid, uuid, text, uuid);

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
              WHERE n.nspname = 'fms' AND p.proname = 'role_grant_blocked_by') THEN
    RAISE EXCEPTION 'down 052 FAILED: role_grant_blocked_by 仍然存在';
  END IF;
  RAISE NOTICE 'down 052 OK';
END;
$$;

COMMIT;
