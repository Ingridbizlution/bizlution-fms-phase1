-- 回退 070。
--
-- **DROP TABLE 會讓所有撤銷失效**：黑名單是「預設有效、被撤銷的才有列」，
-- 表沒了就等於每一個已登出的 refresh token 都復活到它自己過期為止。
-- 這不是回退腳本能規避的事（要規避就得先把所有活著的 token 作廢，而
-- 那需要換 jwt secret，超出一支 migration 的範圍）—— 回退這支的正確做法是
-- 同時換掉 `FMS_JWT_SECRET`。寫在這裡，因為執行回退的人不會去讀 070 的檔頭。
--
-- `tenant_settings_are_valid` 還原成 067 的版本（只認
-- satisfaction_editable_days），不是 DROP：067 的 `ck_tenants_settings` 依賴它。
-- 還原之後 password_min_length 變成未知的鍵，因此既有設定值不會擋在約束上。
BEGIN;

DROP FUNCTION IF EXISTS fms.purge_expired_refresh_revocations();
DROP TABLE IF EXISTS fms.revoked_refresh_tokens;

CREATE OR REPLACE FUNCTION fms.tenant_settings_are_valid(p jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE
    WHEN p IS NULL THEN true
    WHEN jsonb_typeof(p) <> 'object' THEN false
    ELSE
      CASE
        WHEN NOT (p ? 'satisfaction_editable_days') THEN true
        WHEN jsonb_typeof(p -> 'satisfaction_editable_days') <> 'number' THEN false
        WHEN (p ->> 'satisfaction_editable_days')::numeric
               <> trunc((p ->> 'satisfaction_editable_days')::numeric) THEN false
        WHEN (p ->> 'satisfaction_editable_days')::numeric NOT BETWEEN 0 AND 365
          THEN false
        ELSE true
      END
  END;
$$;

COMMIT;
