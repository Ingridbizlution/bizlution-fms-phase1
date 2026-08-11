-- 回退 079：移除目錄同步的示範服務帳號與指派。
BEGIN;
SET search_path = fms, public;
SELECT set_config('app.is_platform', 'on', true);

DELETE FROM fms.user_role_assignments
 WHERE user_id = 'f5000000-0000-4000-8000-000000000002';

DELETE FROM fms.users WHERE id = 'f5000000-0000-4000-8000-000000000002';

COMMIT;
