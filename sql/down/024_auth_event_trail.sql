-- 回退 024。移除政策後，登入路徑對 auth_events 的 INSERT 會再度被 RLS 擋掉
-- （應用層會降級為只記 log，登入本身仍然可用 —— 見 fms-identity 的 record_login_event）。
--
-- 不刪除已經寫入的列：那是稽核資料，且 023 讓 fms_app 連 DELETE 都沒有。
-- 「回退一個 migration」不該包含「抹掉它期間產生的軌跡」。
BEGIN;
SET search_path = fms, public;

DROP POLICY IF EXISTS auth_events_preauth_append ON fms.auth_events;

-- 002 沒有給這張表 COMMENT（只有 SQL 註解），因此還原成「沒有 comment」
-- 而不是寫回一段 002 從未設定過的文字。
COMMENT ON TABLE fms.auth_events IS NULL;

COMMIT;
