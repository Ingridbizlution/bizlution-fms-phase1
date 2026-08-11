-- 回退 023。**這會重新引入三個已驗證的安全缺陷**，
-- 只為 roundtrip 驗證的完整性而存在，不是可以隨手做的操作。
BEGIN;
SET search_path = fms, public;
GRANT UPDATE, DELETE ON fms.audit_log TO fms_app;
GRANT UPDATE, DELETE ON fms.work_order_transitions TO fms_app;
GRANT UPDATE, DELETE ON fms.auth_events TO fms_app;
DROP POLICY IF EXISTS work_order_actions_admin ON fms.work_order_actions;
DROP POLICY IF EXISTS work_order_actions_read ON fms.work_order_actions;
ALTER TABLE fms.work_order_actions NO FORCE ROW LEVEL SECURITY;
ALTER TABLE fms.work_order_actions DISABLE ROW LEVEL SECURITY;
COMMIT;
