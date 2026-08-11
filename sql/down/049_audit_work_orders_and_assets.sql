-- =============================================================================
-- Down: 049_audit_work_orders_and_assets
-- =============================================================================
-- 拿掉兩張表的稽核觸發器。回到 029 的六張（全部沒有場域維度），
-- 也就是 046 的場域收斂再次變成什麼都不過濾。
--
-- **不刪已經寫進去的稽核列。** 稽核軌跡不因為「不再稽核這張表」而消失 ——
-- 那些列記錄的是真的發生過的事，而 007 也早就對 fms_app REVOKE 了
-- audit_log 的 DELETE。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DROP TRIGGER IF EXISTS trg_audit ON fms.work_orders;
DROP TRIGGER IF EXISTS trg_audit ON fms.assets;

COMMENT ON TABLE fms.audit_log IS
  '稽核軌跡。facility_scope 政策只管 SELECT（見 migration 046）：'
  '寫入不能被場域收斂，否則 ORG 範圍的使用者做被稽核的動作時，'
  '稽核列寫不進去會讓他的整個動作失敗（029 的設計）。';

COMMIT;
