-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 085: 預約提醒通知 —— 開會前提醒，同 059 的證照到期提醒同一個模式
-- =============================================================================
-- 前端稽核文件（docs/BACKEND-REQUIREMENTS.md）P1：`notification_templates`／
-- `notifications` 框架都有，但沒有「會議開始前 N 分鐘」這種預約專屬的觸發
-- 規則與範本。跟 059 的證照到期提醒是同一類問題——「時間到了」沒有發生點，
-- 沒有事件可以 fan-out（041 的 `fan_out_notifications` 也明講只認
-- `aggregate_type = 'WORK_ORDER'`），因此同樣用定期掃描，不是事件驅動。
--
-- -----------------------------------------------------------------------------
-- 為什麼「開會前 15 分鐘」先寫死，不放進 bookable_resources
-- -----------------------------------------------------------------------------
-- 059 把門檻放在 `skills` 上，因為換證前置期**是證照類型的性質**（電氣執照
-- 60 天、急救證 7 天，兩者天差地遠）。這裡沒有對應的證據：目前沒有任何
-- 「這間會議室該提前 30 分鐘、那間該提前 5 分鐘」的需求被提出過。放一個
-- 沒有人會去調的旋鈕，跟寫死沒有差別，只是多一個要維護的欄位。等真的
-- 出現「不同資源要不同前置期」的需求，再照 059 的模式把它挪到
-- `bookable_resources`，不是現在就預先開放。
--
-- -----------------------------------------------------------------------------
-- 幂等狀態存 `start_at` 而不是 timestamp 或 boolean
-- -----------------------------------------------------------------------------
-- 同 `user_skills.reminded_for_expiry` 的理由：改期（`start_at` 換了）
-- 應該自動重新武裝提醒，而不需要任何程式碼記得清掉一個「已提醒」的旗標。
--
-- -----------------------------------------------------------------------------
-- 提醒對象只有 `organizer_id`，不含 `on_behalf_of_id`
-- -----------------------------------------------------------------------------
-- 代訂（`on_behalf_of_id`）的實際出席者可能跟主辦人（建立者）不是同一個人。
-- 兩者都提醒是更完整的做法，但目前沒有案例確認代訂情境下「誰該收到會前
-- 提醒」的業務期待，先只提醒 organizer_id（跟既有的通知慣例一致），
-- 這是已知的簡化，不是遺漏。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 幂等的狀態
-- -----------------------------------------------------------------------------
ALTER TABLE fms.reservations
  ADD COLUMN IF NOT EXISTS reminded_for_start_at timestamptz;

COMMENT ON COLUMN fms.reservations.reminded_for_start_at IS
  '已針對**這個 start_at** 提醒過。存時刻而不是 boolean：改期後 start_at
   換了，下一輪掃描自動重新武裝，不需要任何程式碼記得清掉舊旗標。';

-- 讓掃描的 start_at 範圍查詢 sargable：只索引還可能需要提醒的狀態，
-- 已取消／已完成／no-show 的預約永遠不會進這支掃描，排除它們讓索引更小。
CREATE INDEX IF NOT EXISTS idx_reservations_reminder_pending
  ON fms.reservations (start_at)
  WHERE status IN ('CONFIRMED', 'PENDING_APPROVAL');

-- -----------------------------------------------------------------------------
-- (2) 通知範本
-- -----------------------------------------------------------------------------
-- 兩個管道都給：IN_APP 給正在看系統的人，EMAIL 給沒開著系統的人——
-- 會議前 15 分鐘的提醒本來就該假設收件人不在盯著這個系統。
INSERT INTO fms.notification_templates
  (tenant_id, code, channel, locale, subject_template, body_template) VALUES
(NULL, 'RESERVATION_REMINDER', 'EMAIL', 'zh-TW',
 '【會議提醒】{{title}} 於 {{minutes_left}} 分鐘後開始',
 E'{{display_name}} 您好：\n\n'
 '您預約的「{{title}}」將於 {{start_at}}（{{minutes_left}} 分鐘後）開始，'
 '地點：{{resource_name}}。\n'),
(NULL, 'RESERVATION_REMINDER', 'IN_APP', 'zh-TW',
 '{{title}} 將於 {{minutes_left}} 分鐘後開始',
 '{{resource_name}} · {{start_at}}')
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- (3) 掃描
-- -----------------------------------------------------------------------------
-- 跨租戶掃描，因此需要平台情境（與 059 的 sweep_certification_expiry 相同）。
-- 通知列的 tenant_id 從 reservations 帶，不是從情境——情境是平台的。
CREATE OR REPLACE FUNCTION fms.sweep_reservation_reminders()
RETURNS TABLE (reminded int, no_template int, already_reminded int)
LANGUAGE plpgsql
AS $$
DECLARE
  v_row     record;
  v_tpl     record;
  v_created int := 0;
  v_missing int := 0;
  v_skipped int := 0;
BEGIN
  FOR v_row IN
    SELECT r.id, r.tenant_id, r.title, r.start_at, r.reminded_for_start_at,
           coalesce(br.display_name::text, '(unknown)') AS resource_name,
           u.id AS user_id, u.display_name, u.email::text AS email,
           extract(epoch FROM (r.start_at - clock_timestamp()))::int / 60 AS minutes_left
      FROM fms.reservations r
      JOIN fms.users u ON u.id = r.organizer_id
      LEFT JOIN fms.bookable_resources br ON br.id = r.bookable_resource_id
     WHERE r.status IN ('CONFIRMED', 'PENDING_APPROVAL')
       AND r.start_at BETWEEN clock_timestamp() AND clock_timestamp() + interval '15 minutes'
       AND u.deleted_at IS NULL
       AND u.status NOT IN ('SUSPENDED', 'DEPROVISIONED')
     ORDER BY r.start_at
  LOOP
    -- 幂等：NULL 安全的比對，理由同 059——條件若被反寫，
    -- `<>` 會讓從未提醒過的（NULL）安靜地被跳過。
    IF v_row.reminded_for_start_at IS NOT DISTINCT FROM v_row.start_at THEN
      v_skipped := v_skipped + 1;
      CONTINUE;
    END IF;

    FOR v_tpl IN
      SELECT * FROM fms.notification_templates
       WHERE code = 'RESERVATION_REMINDER' AND is_active
         AND (tenant_id IS NULL OR tenant_id = v_row.tenant_id)
       ORDER BY tenant_id NULLS LAST
    LOOP
      INSERT INTO fms.notifications
        (tenant_id, recipient_user_id, recipient_address, channel, template_code,
         subject, body, entity_type, entity_id, priority)
      VALUES (
        v_row.tenant_id, v_row.user_id,
        CASE WHEN v_tpl.channel = 'EMAIL' THEN v_row.email END,
        v_tpl.channel, 'RESERVATION_REMINDER',
        fms.render_template(v_tpl.subject_template, fms.reservation_reminder_vars(
          v_row.title, v_row.display_name, v_row.start_at, v_row.minutes_left, v_row.resource_name)),
        fms.render_template(v_tpl.body_template, fms.reservation_reminder_vars(
          v_row.title, v_row.display_name, v_row.start_at, v_row.minutes_left, v_row.resource_name)),
        'RESERVATION', v_row.id, 'NORMAL');
      v_created := v_created + 1;
    END LOOP;

    IF NOT FOUND THEN
      -- 「該通知但沒有範本」→ 沒有人會收到。同 059／041 的判斷：
      -- 不拋錯（一輪掃描不該因為範本缺失而整批失敗），但必須被計數。
      v_missing := v_missing + 1;
    END IF;

    UPDATE fms.reservations
       SET reminded_for_start_at = v_row.start_at
     WHERE id = v_row.id;
  END LOOP;

  RETURN QUERY SELECT v_created, v_missing, v_skipped;
END;
$$;

-- 變數表。抽成函式是因為 subject 與 body 各要一份，兩處手寫必然漂移。
CREATE OR REPLACE FUNCTION fms.reservation_reminder_vars(
  p_title text, p_display_name text, p_start_at timestamptz,
  p_minutes_left int, p_resource_name text
) RETURNS jsonb
LANGUAGE sql IMMUTABLE
AS $$
  SELECT jsonb_build_object(
    'title', coalesce(p_title, '（未命名）'),
    'display_name', p_display_name,
    'start_at', to_char(p_start_at, 'YYYY-MM-DD HH24:MI'),
    'minutes_left', greatest(p_minutes_left, 0),
    'resource_name', coalesce(p_resource_name, '（未命名資源）'));
$$;

GRANT EXECUTE ON FUNCTION fms.sweep_reservation_reminders() TO fms_owner;

COMMENT ON FUNCTION fms.sweep_reservation_reminders() IS
  '會議前提醒的掃描（固定 15 分鐘窗，見本 migration 檔頭的說明）。幂等靠
   reservations.reminded_for_start_at（存時刻而非 boolean，改期後自動重新
   武裝）。no_template 計數「該通知但沒有範本」——那代表沒有人會收到，
   必須看得見。只提醒 organizer_id，不含 on_behalf_of_id（已知的簡化）。';

-- -----------------------------------------------------------------------------
-- 自我驗證：結構的（行為驗證需要預約資料，見 cert_expiry_slice.rs 起的慣例）
-- -----------------------------------------------------------------------------
DO $$
DECLARE v_src text; v_n int;
BEGIN
  IF to_regprocedure('fms.sweep_reservation_reminders()') IS NULL THEN
    RAISE EXCEPTION '085 FAILED: 掃描函式不存在';
  END IF;
  v_src := pg_get_functiondef('fms.sweep_reservation_reminders()'::regprocedure);

  IF v_src NOT LIKE '%IS NOT DISTINCT FROM%' THEN
    RAISE EXCEPTION
      '085 FAILED: 幂等比對不是 NULL 安全的形式 —— '
      '條件反寫時會讓從未提醒過的（NULL）安靜地被跳過';
  END IF;

  IF v_src NOT LIKE '%v_missing := v_missing + 1%' THEN
    RAISE EXCEPTION '085 FAILED: 缺範本沒有被計數 —— 「沒有人會收到」會看不見';
  END IF;

  SELECT count(DISTINCT channel) INTO v_n
    FROM fms.notification_templates WHERE code = 'RESERVATION_REMINDER';
  IF v_n < 2 THEN
    RAISE EXCEPTION '085 FAILED: RESERVATION_REMINDER 只有 % 個管道的範本', v_n;
  END IF;

  RAISE NOTICE '085 OK：幂等用 IS NOT DISTINCT FROM、缺範本會計數、兩個管道都有範本'
               '（行為驗證在 reservation_reminder 的整合測試）';
END;
$$;

COMMIT;
