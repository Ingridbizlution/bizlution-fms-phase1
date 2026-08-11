-- 回退 014：移除登入前的租戶解析函式。
--
-- 回退後 `POST /auth/token` 會無法把 tenant_code 換成 tenant_id
-- （那正是 014 要解的問題），因此應用層會壞掉。
-- 這個 down 存在是為了完整性與 roundtrip 驗證，不是給生產用的操作。
BEGIN;
SET search_path = fms, public;
DROP FUNCTION IF EXISTS fms.resolve_tenant_by_code(text);
COMMIT;
