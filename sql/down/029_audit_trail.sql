-- 回退 029。**這會讓 audit_log 重新變成一張沒人寫的表。**
--
-- 不刪除已寫入的稽核列：那是稽核資料，而 023 讓 fms_app 連 DELETE 都沒有。
-- 「回退一個 migration」不該包含「抹掉它期間留下的軌跡」——
-- 與 down/024 不刪 auth_events 是同一個原則。
BEGIN;
SET search_path = fms, public;

DO $$
DECLARE
  t text;
  audited text[] := ARRAY[
    'users', 'user_role_assignments', 'roles', 'role_permissions',
    'identity_providers', 'tenants'
  ];
BEGIN
  FOREACH t IN ARRAY audited LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS trg_audit ON fms.%I', t);
  END LOOP;
END;
$$;

DROP FUNCTION IF EXISTS fms.trg_audit_row();
DROP FUNCTION IF EXISTS fms.set_request_context(text, text);

COMMIT;
