-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 059: 證照到期提醒 —— 讓 idx_user_skills_expiring 第一次有讀者
-- =============================================================================
-- 004 為到期掃描建了 `idx_user_skills_expiring`
-- （`(tenant_id, expires_at)`，部分索引 `WHERE expires_at IS NOT NULL`）。
--
-- 做 `/skills` 時確認過：**那個索引到現在沒有任何讀者**。本模組的查詢都是
-- 單一使用者（`WHERE user_id = $1`），走不到它。ENDPOINTS.md 的描述寫著
-- 「技能與證照（**含到期提醒**）」，而提醒那一半從來沒有實作。
--
-- 這個 migration 是那一半的資料層。
--
-- -----------------------------------------------------------------------------
-- 門檻放在 `skills` 上，因為換證前置期是**證照類型的性質**
-- -----------------------------------------------------------------------------
-- 電氣執照的換發要送審、可能要補訓，實務上要留 60 天；急救證重考一次就好，
-- 7 天夠。用一個全租戶共用的數字會讓其中一種永遠太早或太晚。
--
-- **考慮過而否決的兩個位置：**
--
--   * `tenants.settings`（jsonb）—— 它存在，但**沒有讀者也沒有寫者**，
--     而且這個 repo 根本沒有 `/tenants` 端點。放進去等於再造一個
--     沒有人能設定的欄位，那正是這一輪要修的缺陷類型。
--   * worker 的設定參數 —— `sla_watchdog` 的檔頭已經定了規矩：
--     「門檻類的設定全部在資料庫裡由管理者定義，這裡只留排程參數」。
--     worker 開一個旋鈕就是一個能蓋掉管理者設定的東西。
--
-- `POST /skills` 與 `GET /skills` 同步加上這個欄位 —— 一個管理員設定不了的
-- 門檻等於寫死的門檻。
--
-- -----------------------------------------------------------------------------
-- 幂等：記「已針對**這個到期日**提醒過」
-- -----------------------------------------------------------------------------
-- 掃描每天跑，而一張 60 天後到期的證照會連續 60 天落在窗內。
-- 沒有狀態的話會寄 60 封。
--
-- `user_skills.reminded_for_expiry` 存的是**當時提醒的那個 `expires_at`**，
-- 不是「提醒時間」。差別是實的：
--
--   * 同一張證照重複掃描 → 值相同 → 不再寄（幂等）
--   * **續證之後 `expires_at` 變了** → 對不上 → 下一個窗期自動再提醒
--
-- 若存的是 `reminded_at timestamptz`，續證後就得有人記得清掉它 ——
-- 而沒有人會記得。這個設計自己會復原。
--
-- -----------------------------------------------------------------------------
-- 收件人是本人。「也通知主管」需要一個這張表沒有的維度
-- -----------------------------------------------------------------------------
-- `user_skills` 只有 `(user_id, skill_id, tenant_id)` —— **沒有場域、
-- 沒有團隊**。047 的 `PERM:<code>` 收件人代號是對「工單的場域」解析的，
-- 這裡沒有工單也沒有場域，套不上。
--
-- 因此 v1 只通知本人，而這個限制寫在這裡而不是留一個空白。
-- 要通知主管，得先決定「一個人的主管」在這個資料模型裡是什麼
-- （`users.primary_org_id` 的 ORG_MANAGER？`teams` 的 leader？），
-- 那是獨立的決定。
--
-- 依賴：004（skills／user_skills／idx_user_skills_expiring）、
--       006 或 008（notification_templates／notifications）。
-- =============================================================================

-- 寫入 `notification_templates`（tenant_id 為 NULL 的平台範本）需要平台情境
-- —— 031 記過這條規則，而 042 的政策讓租戶寫不到平台範本。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 門檻
-- -----------------------------------------------------------------------------
-- 預設 30 天：與 `GET /users/{id}/skills` 的 `expiring_within_days` 預設一致，
-- 讓「畫面上顯示為 EXPIRING」與「會收到提醒」在未設定時是同一個窗。
-- 上界 365：超過一年的提醒沒有行動意義，而打錯一個 0 會讓每張證照
-- 從發證那天就開始提醒。
ALTER TABLE fms.skills
  ADD COLUMN IF NOT EXISTS reminder_days_before int NOT NULL DEFAULT 30;

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint
                  WHERE conrelid = 'fms.skills'::regclass
                    AND conname = 'ck_skills_reminder_days') THEN
    ALTER TABLE fms.skills
      ADD CONSTRAINT ck_skills_reminder_days
      CHECK (reminder_days_before BETWEEN 1 AND 365);
  END IF;
END;
$$;

COMMENT ON COLUMN fms.skills.reminder_days_before IS
  '到期前幾天開始提醒。**是證照類型的性質**（電氣執照換發要 60 天、'
  '急救證 7 天），因此放在 skills 而不是租戶設定或 worker 參數。';

-- 平台目錄的實際前置期。055 種的九項裡有五項需要證照，
-- 而它們的換證難度差很多 —— 全用預設值等於沒有這個欄位。
UPDATE fms.skills SET reminder_days_before = 60
 WHERE tenant_id IS NULL AND code IN ('ELECTRICAL', 'ELEVATOR', 'BOILER');
UPDATE fms.skills SET reminder_days_before = 45
 WHERE tenant_id IS NULL AND code IN ('FIRE_SAFETY', 'WORK_AT_HEIGHT');

-- -----------------------------------------------------------------------------
-- (2) 幂等的狀態
-- -----------------------------------------------------------------------------
ALTER TABLE fms.user_skills
  ADD COLUMN IF NOT EXISTS reminded_for_expiry date;

COMMENT ON COLUMN fms.user_skills.reminded_for_expiry IS
  '已針對**這個 expires_at** 提醒過。存到期日而不是提醒時間：'
  ' 續證後 expires_at 改變就對不上，下一個窗期自動再提醒 ——'
  ' 存 timestamptz 的話續證後得有人記得清掉它，而沒有人會記得。';

-- -----------------------------------------------------------------------------
-- (3) 通知範本
-- -----------------------------------------------------------------------------
-- 兩個管道都給：IN_APP 是值班畫面看得到的，EMAIL 是離開系統也收得到的。
-- 一張快到期的執業證照屬於後者 —— 過期執業是違規，不能只放在一個
-- 要登入才看得到的地方。
INSERT INTO fms.notification_templates
  (tenant_id, code, channel, locale, subject_template, body_template) VALUES
(NULL, 'CERT_EXPIRING', 'EMAIL', 'zh-TW',
 '【證照即將到期】{{skill_name}} 於 {{expires_at}} 到期',
 E'{{display_name}} 您好：\n\n'
 '您的「{{skill_name}}」證照將於 {{expires_at}} 到期（剩餘 {{days_left}} 天）。\n'
 '證照號碼：{{certificate_no}}\n\n'
 '**過期後不得執業。** 請儘早辦理換發；若已完成換發，'
 '請通知管理員更新系統中的到期日，否則會再次收到提醒。\n'),
(NULL, 'CERT_EXPIRING', 'IN_APP', 'zh-TW',
 '{{skill_name}} 證照 {{days_left}} 天後到期',
 '{{skill_name}}（證號 {{certificate_no}}）於 {{expires_at}} 到期。過期後不得執業。')
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- (4) 掃描
-- -----------------------------------------------------------------------------
-- **這支函式是 `idx_user_skills_expiring` 的第一個讀者。**
-- 述詞是 `expires_at <= current_date + interval` 而不是對 `expires_at` 做運算
-- —— 後者會讓索引用不到（不是 sargable）。
--
-- 跨租戶掃描，因此需要平台情境（與 `sweep_sla_states` 相同）。
-- 通知列的 `tenant_id` 從 `user_skills` 帶，不是從情境 —— 情境是平台的。
CREATE OR REPLACE FUNCTION fms.sweep_certification_expiry()
RETURNS TABLE (reminded int, no_template int, already_reminded int)
LANGUAGE plpgsql
AS $$
DECLARE
  v_row       record;
  v_tpl       record;
  v_created   int := 0;
  v_missing   int := 0;
  v_skipped   int := 0;
BEGIN
  FOR v_row IN
    SELECT us.user_id, us.skill_id, us.tenant_id, us.expires_at, us.certificate_no,
           s.name AS skill_name,
           u.display_name, u.email::text AS email,
           (us.expires_at - current_date)::int AS days_left,
           us.reminded_for_expiry
      FROM fms.user_skills us
      JOIN fms.skills s ON s.id = us.skill_id
      JOIN fms.users u ON u.id = us.user_id
     WHERE us.expires_at IS NOT NULL
       AND s.requires_certification
       AND u.deleted_at IS NULL
       -- 已離職的人不必提醒 —— 他不會去換證，而那封信只會製造噪音。
       AND u.status NOT IN ('SUSPENDED', 'DEPROVISIONED')
       -- sargable：讓 idx_user_skills_expiring 用得到。
       AND us.expires_at <= current_date + (s.reminder_days_before || ' days')::interval
     ORDER BY us.expires_at
  LOOP
    -- 幂等。用 NULL 安全的比對形式：這個判斷寫成「相同就跳過」，
    -- 而 `=` 在 NULL 時回 NULL、plpgsql 的 IF 把 NULL 當 false，
    -- 所以**在這個方向上兩者行為相同**（量過：`NULL = current_date` → NULL）。
    -- 用 `IS NOT DISTINCT FROM` 是為了讓它與方向無關 —— 若哪天有人把條件
    -- 反寫成「不同就寄」，`<>` 會讓從未提醒過的（NULL）安靜地一封都不寄。
    --
    -- 真正承重的不是這個運算子，是**這一欄存的是到期日而不是 bool**；
    -- 那件事由 cert_expiry_slice.rs 的 `c_` 釘住。
    IF v_row.reminded_for_expiry IS NOT DISTINCT FROM v_row.expires_at THEN
      v_skipped := v_skipped + 1;
      CONTINUE;
    END IF;

    FOR v_tpl IN
      SELECT * FROM fms.notification_templates
       WHERE code = 'CERT_EXPIRING' AND is_active
         AND (tenant_id IS NULL OR tenant_id = v_row.tenant_id)
       ORDER BY tenant_id NULLS LAST
    LOOP
      INSERT INTO fms.notifications
        (tenant_id, recipient_user_id, recipient_address, channel, template_code,
         subject, body, entity_type, entity_id, priority)
      VALUES (
        v_row.tenant_id, v_row.user_id,
        CASE WHEN v_tpl.channel = 'EMAIL' THEN v_row.email END,
        v_tpl.channel, 'CERT_EXPIRING',
        fms.render_template(v_tpl.subject_template, fms.cert_expiry_vars(v_row.skill_name,
          v_row.display_name, v_row.expires_at, v_row.days_left, v_row.certificate_no)),
        fms.render_template(v_tpl.body_template, fms.cert_expiry_vars(v_row.skill_name,
          v_row.display_name, v_row.expires_at, v_row.days_left, v_row.certificate_no)),
        'USER_SKILLS', v_row.user_id,
        -- 已經過期的用 HIGH：那不是提醒，是違規狀態。
        CASE WHEN v_row.days_left < 0 THEN 'HIGH' ELSE 'NORMAL' END);
      v_created := v_created + 1;
    END LOOP;

    IF NOT FOUND THEN
      -- 「該通知但沒有範本」→ **沒有人會收到**。與 041 同一個判斷：
      -- 不拋錯（一封信發不出去不該讓掃描停擺），但必須被計數。
      v_missing := v_missing + 1;
    END IF;

    UPDATE fms.user_skills
       SET reminded_for_expiry = v_row.expires_at
     WHERE user_id = v_row.user_id AND skill_id = v_row.skill_id;
  END LOOP;

  RETURN QUERY SELECT v_created, v_missing, v_skipped;
END;
$$;

-- 變數表。抽成函式是因為 subject 與 body 各要一份，而兩處手寫必然漂移。
CREATE OR REPLACE FUNCTION fms.cert_expiry_vars(
  p_skill_name text, p_display_name text, p_expires_at date,
  p_days_left int, p_certificate_no text
) RETURNS jsonb
LANGUAGE sql IMMUTABLE
AS $$
  SELECT jsonb_build_object(
    'skill_name', p_skill_name,
    'display_name', p_display_name,
    'expires_at', to_char(p_expires_at, 'YYYY-MM-DD'),
    'days_left', p_days_left,
    -- 沒有證號的紀錄是資料缺漏，但那不該讓信件出現 "null"。
    'certificate_no', coalesce(p_certificate_no, '（未登錄）'));
$$;

GRANT EXECUTE ON FUNCTION fms.sweep_certification_expiry() TO fms_owner;

COMMENT ON FUNCTION fms.sweep_certification_expiry() IS
  '證照到期提醒的掃描。**idx_user_skills_expiring 的第一個讀者**。'
  ' 幂等靠 user_skills.reminded_for_expiry（存到期日而非提醒時間，'
  ' 因此續證後會自動再提醒）。no_template 計數「該通知但沒有範本」——'
  ' 那代表沒有人會收到，必須看得見。';

-- -----------------------------------------------------------------------------
-- 自我驗證：結構的（行為驗證需要使用者與證照資料，見 053 起的慣例）
-- -----------------------------------------------------------------------------
DO $$
DECLARE v_src text; v_n int;
BEGIN
  IF to_regprocedure('fms.sweep_certification_expiry()') IS NULL THEN
    RAISE EXCEPTION '059 FAILED: 掃描函式不存在';
  END IF;
  v_src := pg_get_functiondef('fms.sweep_certification_expiry()'::regprocedure);

  -- (1) 述詞必須 sargable，否則那個索引仍然用不到 —— 而這個 migration
  --     存在的理由就是讓它有讀者。
  IF v_src NOT LIKE '%us.expires_at <= current_date%' THEN
    RAISE EXCEPTION
      '059 FAILED: 述詞對 expires_at 做了運算 —— idx_user_skills_expiring 用不到，'
      '而這個 migration 的理由就是讓它有讀者';
  END IF;

  -- (2) 幂等比對保持 NULL 安全的形式。在目前的方向（相同就跳過）`=` 也可行，
  --     但條件一被反寫，`<>` 就會讓從未提醒過的（NULL）一封都不寄。
  IF v_src NOT LIKE '%IS NOT DISTINCT FROM%' THEN
    RAISE EXCEPTION
      '059 FAILED: 幂等比對不是 NULL 安全的形式 —— '
      '條件反寫時會讓從未提醒過的（NULL）安靜地被跳過';
  END IF;

  -- (3) 缺範本要被計數，不是靜默。
  IF v_src NOT LIKE '%v_missing := v_missing + 1%' THEN
    RAISE EXCEPTION '059 FAILED: 缺範本沒有被計數 —— 「沒有人會收到」會看不見';
  END IF;

  -- (4) 兩個管道的範本都在。少了 EMAIL 的話，一張快到期的執業證照
  --     只會出現在要登入才看得到的地方。
  SELECT count(DISTINCT channel) INTO v_n
    FROM fms.notification_templates WHERE code = 'CERT_EXPIRING';
  IF v_n < 2 THEN
    RAISE EXCEPTION '059 FAILED: CERT_EXPIRING 只有 % 個管道的範本', v_n;
  END IF;

  -- (5) 平台目錄的前置期要有差異。全用預設值等於沒有這個欄位。
  SELECT count(DISTINCT reminder_days_before) INTO v_n
    FROM fms.skills WHERE tenant_id IS NULL AND requires_certification;
  IF v_n < 2 THEN
    RAISE EXCEPTION
      '059 FAILED: 需要證照的平台技能前置期全部相同（% 種）—— '
      '那個欄位失去意義', v_n;
  END IF;

  RAISE NOTICE '059 OK：述詞 sargable、幂等用 IS NOT DISTINCT FROM、'
               '缺範本會計數、兩個管道都有範本、前置期有差異'
               '（行為驗證在 cert_expiry_slice.rs）';
END;
$$;

COMMIT;
