-- 回退 060。
--
-- 注意這個回退會**重新打開**場域洩漏（那是 060 之前的狀態，所以回退就該回到那裡）。
--
-- `role_permissions` 的刪除要靠 `permission_code` 而不是靠角色名單 ——
-- 060 的授予是由 `alarm:read` 推導的，中間若有人手動加減角色，
-- 照名單刪會留下孤兒列。

-- 與 up 同一個理由：`role_permissions` 沒有 `tenant_id`，於是 `trg_audit_row`
-- 的稽核列退回 NULL 租戶而被 `audit_log` 的政策擋掉。
-- up 加了這一行，down 一開始忘了 —— `make migrate-roundtrip` 抓到的。
SET app.is_platform = 'on';

BEGIN;

DROP POLICY IF EXISTS facility_scope ON fms.telemetry_readings;
DROP POLICY IF EXISTS facility_scope ON fms.telemetry_latest;
DROP POLICY IF EXISTS facility_scope ON fms.telemetry_points;

DELETE FROM fms.role_permissions WHERE permission_code = 'alarm_rule:read';
DELETE FROM fms.permissions WHERE code = 'alarm_rule:read';

COMMIT;
