-- =============================================================================
-- Down migration 059：移除證照到期提醒
-- =============================================================================
-- 回退後 idx_user_skills_expiring 又回到「沒有讀者」的狀態，
-- 而 ENDPOINTS.md 的「含到期提醒」再度只是一句宣稱。
--
-- **已經建立的通知列不動** —— 那是業務紀錄。
-- 而 `reminded_for_expiry` 會連同欄位一起消失，因此重新套用 059 之後
-- **每一張還在窗內的證照都會再提醒一次**。那是刻意的：寧可重複一封，
-- 不要讓一張快到期的執業證照因為回退而靜默地沒有人被通知。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DROP FUNCTION IF EXISTS fms.sweep_certification_expiry();
DROP FUNCTION IF EXISTS fms.cert_expiry_vars(text, text, date, int, text);

DELETE FROM fms.notification_templates WHERE code = 'CERT_EXPIRING';

ALTER TABLE fms.user_skills DROP COLUMN IF EXISTS reminded_for_expiry;
ALTER TABLE fms.skills DROP CONSTRAINT IF EXISTS ck_skills_reminder_days;
ALTER TABLE fms.skills DROP COLUMN IF EXISTS reminder_days_before;

DO $$
BEGIN
  IF to_regprocedure('fms.sweep_certification_expiry()') IS NOT NULL THEN
    RAISE EXCEPTION 'down 059 FAILED: 掃描函式仍然存在';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_attribute
              WHERE attrelid = 'fms.skills'::regclass
                AND attname = 'reminder_days_before' AND NOT attisdropped) THEN
    RAISE EXCEPTION 'down 059 FAILED: reminder_days_before 仍然存在';
  END IF;
  IF EXISTS (SELECT 1 FROM fms.notification_templates WHERE code = 'CERT_EXPIRING') THEN
    RAISE EXCEPTION 'down 059 FAILED: 範本仍然存在';
  END IF;
  RAISE NOTICE 'down 059 OK（提醒：重新套用後窗內的證照會各再提醒一次）';
END;
$$;

COMMIT;
