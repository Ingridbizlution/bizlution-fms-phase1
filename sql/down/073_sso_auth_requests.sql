-- 回退 073。
--
-- 進行中的 SSO 登入會失敗（state 查不到），而使用者只要重新點一次登入就好 ——
-- 那是 10 分鐘內的暫時影響，不需要特別處置。
BEGIN;
DROP FUNCTION IF EXISTS fms.consume_sso_state(text);
DROP FUNCTION IF EXISTS fms.purge_expired_sso_requests();
DROP TABLE IF EXISTS fms.sso_auth_requests;
COMMIT;
