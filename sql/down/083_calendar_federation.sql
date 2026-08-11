-- 回退 083。
--
-- **DROP TABLE 會讓所有日曆整合設定、資源對應與待審衝突一起消失。**
-- 回退之後每個租戶都要重新走一次 admin consent／OAuth 授權，資源對應也要
-- 重新比對或手動掛一次。寫在這裡，因為執行回退的人不會去讀 083 的檔頭。
BEGIN;

SET search_path = fms, public;

-- role_permissions 掛了 029 的稽核觸發器，而 audit_log 有 RLS——
-- 刪 permissions 會級聯刪 role_permissions（外鍵），沒有平台情境那個級聯
-- 刪除會在稽核觸發器裡被 RLS 擋下（071、072 的 down 都踩過同一件事）。
SELECT set_config('app.is_platform', 'on', true);

-- 083 改寫了這個欄位的註解，回退時還原成原本沒有註解的狀態
-- （005 只有一句行內 SQL 註解，從來沒有 COMMENT ON COLUMN）。
COMMENT ON COLUMN fms.reservations.external_event_id IS NULL;

DROP INDEX IF EXISTS fms.uq_reservations_external_event;

DELETE FROM fms.permissions
 WHERE code IN ('calendar_integration:read', 'calendar_integration:write');

-- 依相依順序：conflicts → mappings → integrations。
DROP TABLE IF EXISTS fms.calendar_sync_conflicts;
DROP TABLE IF EXISTS fms.calendar_resource_mappings;
DROP TABLE IF EXISTS fms.calendar_integrations;

COMMIT;
