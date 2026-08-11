-- =============================================================================
-- 067：工單滿意度評價
-- =============================================================================
-- `POST /work-orders/{id}/satisfaction` 的落地處。
--
-- -----------------------------------------------------------------------------
-- 這一支不是新功能，是把三個已經存在的宣告接起來
-- -----------------------------------------------------------------------------
-- 004 就有欄位（`work_orders.satisfaction_score` CHECK 1–5、
-- `satisfaction_comment`），`fms-workorder` 的 DTO 已經在回傳它們，而 008 的
-- 狀態機**在資料裡宣告了觸發點**：
--
--   244  IN_PROGRESS → COMPLETED  '{"request_satisfaction": true, ...}'
--   268  COMPLETED   → CLOSED     '{"request_satisfaction": true}'   （SERVICE）
--
-- 但 `apply_side_effects` 明確地不執行 `request_satisfaction`（它的註解寫
-- 「同樣缺模組」），而且**沒有任何寫入者**。所以那兩欄從 004 到現在一直是
-- NULL，而狀態機每次結案都宣告了一件不會發生的事。
--
-- 這支 migration 補的是「請他評分」那一半（通知範本 + 一個發通知的函式）；
-- 寫入端點在 `fms-workorder::satisfaction`。少了通知那一半，端點會存在但
-- 永遠沒有流量 —— 沒有人知道可以評分。
--
-- -----------------------------------------------------------------------------
-- 「評完之後還能改幾天」由管理者定義
-- -----------------------------------------------------------------------------
-- 那是合約性質的條件（有的客戶要求評價一經送出即定案，有的允許冷靜期），
-- 所以不寫死也不放 CHECK 的預設值裡，而是放 `tenants.settings`。
--
-- **這是 `tenants.settings` 的第一個讀者。** 那個欄位從 001 建立到現在
-- 讀者數是 0（量過：33 個 `settings` 命中全是無關的 `fms_shared::Settings`）。
-- 一個沒有讀者的自由 jsonb 等於一個許願池，所以這裡同時：
--
--   1. 加形狀約束（只認已知的鍵與型別），理由與 038 對 `operating_hours`
--      加約束一樣 —— 這個欄位開始有後果了，壞掉的值要在寫入時擋，
--      而不是在評分時才炸。
--   2. 用 `fms.tenant_setting_int()` 讀，缺鍵時回退到預設值並讓呼叫端知道
--      用的是哪一個（端點的 meta 會說）。
--
-- 預設 14 天：一週太短（週末請假就過了），一個月太長（那時工單早已對帳）。
-- 0 代表「一經送出即定案」，也就是完全不可修改 —— 那是一個合法的政策，
-- 不是「沒設定」。
--
-- 依賴：001（tenants.settings）、004（satisfaction 欄位）、
--       006／041（notifications、render_template）、008（狀態機宣告）。
-- =============================================================================

-- 寫入 `notification_templates` 的平台範本（tenant_id IS NULL）需要平台情境
-- —— 031 記過這條規則，042 的政策讓租戶寫不到平台範本。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) tenants.settings 的形狀
-- -----------------------------------------------------------------------------
-- 只驗**已知的鍵**：未知的鍵放行，因為這個欄位會長大，而每加一個鍵就改一次
-- 約束會讓 migration 變成設定檔。已知的鍵一旦出現，型別與範圍就要對 ——
-- `"satisfaction_editable_days": "十四"` 會讓評分在讀設定時炸，
-- 而那是離設定它的人三層之外的地方。
CREATE OR REPLACE FUNCTION fms.tenant_settings_are_valid(p jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE
    WHEN p IS NULL THEN true
    WHEN jsonb_typeof(p) <> 'object' THEN false
    ELSE
      -- satisfaction_editable_days：整數 0–365。
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

COMMENT ON FUNCTION fms.tenant_settings_are_valid(jsonb) IS
  'tenants.settings 的形狀。只驗已知的鍵（未知的放行，這個欄位會長大）；'
  '已知的鍵型別錯了會在讀設定的地方炸，而那離設定它的人三層之外。';

ALTER TABLE fms.tenants DROP CONSTRAINT IF EXISTS ck_tenants_settings;
ALTER TABLE fms.tenants
  ADD CONSTRAINT ck_tenants_settings
  CHECK (fms.tenant_settings_are_valid(settings));

-- 讀一個整數設定，缺鍵時回退。
--
-- `STABLE` 而不是 `IMMUTABLE`：它讀表。`SECURITY INVOKER`，所以只讀得到
-- 呼叫者自己租戶的那一列（tenants 有 FORCE RLS）。
CREATE OR REPLACE FUNCTION fms.tenant_setting_int(
  p_key     text,
  p_default int
) RETURNS int
LANGUAGE sql
STABLE
AS $$
  SELECT coalesce(
    (SELECT (t.settings ->> p_key)::int FROM fms.tenants t
      WHERE t.id = fms.current_tenant_id()),
    p_default);
$$;

COMMENT ON FUNCTION fms.tenant_setting_int(text, int) IS
  '讀 tenants.settings 的整數鍵，缺鍵時回退到 p_default。'
  'SECURITY INVOKER —— 只讀得到呼叫者自己的租戶（tenants 有 FORCE RLS）。';

-- -----------------------------------------------------------------------------
-- (2) 通知範本
-- -----------------------------------------------------------------------------
-- 兩個管道，與 059 的 CERT_EXPIRING 同一個做法：EMAIL 給不會登入系統的申請人
-- （軟性服務的申請人常常是一般員工），IN_APP 給會登入的。
INSERT INTO fms.notification_templates
  (tenant_id, code, channel, locale, subject_template, body_template) VALUES
(NULL, 'SATISFACTION_REQUEST', 'EMAIL', 'zh-TW',
 '【請評分】您的服務申請已完成：{{wo_no}}',
 E'{{display_name}} 您好：\n\n'
 '您於 {{created_date}} 提出的「{{title}}」已於 {{completed_date}} 完成。\n\n'
 '請花十秒為這次服務評分（1–5 分），這會影響我們如何配置人力。\n'
 '評分後 {{editable_days}} 天內仍可修改。\n'),
(NULL, 'SATISFACTION_REQUEST', 'IN_APP', 'zh-TW',
 '請為 {{wo_no}} 評分',
 '「{{title}}」已完成。請評分（1–5 分）；{{editable_days}} 天內可修改。')
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- (3) 發出評分邀請
-- -----------------------------------------------------------------------------
-- 回傳建立的通知筆數。**0 是有意義的答案**，而且有三種原因，呼叫端要分得開：
--   * 沒有範本（租戶把平台範本停用了）
--   * 工單沒有申請人（`created_by` 是 NULL —— 背景產生的 PM 工單就是這樣）
--   * 已經邀請過（不重複發：結案再重開再結案會走到這裡兩次）
--
-- 與 041／059 同一個判斷：不拋錯（一封信發不出去不該讓結案失敗），
-- 但必須看得見。
CREATE OR REPLACE FUNCTION fms.request_satisfaction(p_work_order_id uuid)
RETURNS int
LANGUAGE plpgsql
AS $$
DECLARE
  v_wo    record;
  v_tpl   record;
  v_vars  jsonb;
  v_days  int;
  v_n     int := 0;
BEGIN
  SELECT w.id, w.tenant_id, w.wo_no, w.title, w.created_by, w.created_at,
         w.completed_at, u.display_name, u.email
    INTO v_wo
    FROM fms.work_orders w
    LEFT JOIN fms.users u ON u.id = w.created_by
   WHERE w.id = p_work_order_id;

  -- 沒有申請人 → 沒有人可以評分。PM 產生的工單就是這樣，那不是錯誤。
  IF v_wo.created_by IS NULL THEN
    RETURN 0;
  END IF;

  -- 已經邀請過就不再發。判斷依據是通知本身而不是一個旗標欄位 ——
  -- 旗標會與通知不同步，而通知是那件事真正發生過的證據。
  IF EXISTS (
    SELECT 1 FROM fms.notifications n
     WHERE n.template_code = 'SATISFACTION_REQUEST'
       AND n.entity_type = 'WORK_ORDER' AND n.entity_id = p_work_order_id
  ) THEN
    RETURN 0;
  END IF;

  v_days := fms.tenant_setting_int('satisfaction_editable_days', 14);
  v_vars := jsonb_build_object(
    'wo_no',          v_wo.wo_no,
    'title',          v_wo.title,
    'display_name',   coalesce(v_wo.display_name, ''),
    'created_date',   to_char(v_wo.created_at, 'YYYY-MM-DD'),
    'completed_date', to_char(coalesce(v_wo.completed_at, clock_timestamp()), 'YYYY-MM-DD'),
    'editable_days',  v_days::text);

  FOR v_tpl IN
    SELECT * FROM fms.notification_templates
     WHERE code = 'SATISFACTION_REQUEST' AND is_active
       AND (tenant_id IS NULL OR tenant_id = v_wo.tenant_id)
     ORDER BY tenant_id NULLS LAST
  LOOP
    INSERT INTO fms.notifications
      (tenant_id, recipient_user_id, recipient_address, channel, template_code,
       subject, body, entity_type, entity_id, priority)
    VALUES (
      v_wo.tenant_id, v_wo.created_by,
      CASE WHEN v_tpl.channel = 'EMAIL' THEN v_wo.email END,
      v_tpl.channel, 'SATISFACTION_REQUEST',
      fms.render_template(v_tpl.subject_template, v_vars),
      fms.render_template(v_tpl.body_template, v_vars),
      'WORK_ORDER', p_work_order_id,
      -- LOW：這不是需要立刻處理的事，不該與告警搶同一個佇列位置。
      'LOW');
    v_n := v_n + 1;
  END LOOP;

  RETURN v_n;
END;
$$;

COMMENT ON FUNCTION fms.request_satisfaction(uuid) IS
  '發出評分邀請。008 的狀態機在兩個轉換宣告 request_satisfaction，'
  '而在這支 migration 之前那個宣告從未被執行。回 0 有三種原因'
  '（無範本／無申請人／已邀請過），呼叫端要分得開。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構的（跑在 CORE 階段，seed 009 還沒進來）。
-- **行為驗證在 satisfaction_slice.rs**：誰能寫、什麼時候能寫、期限、
-- 重複邀請不重發。
DO $$
DECLARE v_ok boolean;
BEGIN
  -- (1) 形狀約束真的擋得住。這一格可以在 CORE 跑，因為它只呼叫函式，
  --     不需要任何一列資料。
  IF fms.tenant_settings_are_valid('{"satisfaction_editable_days": "十四"}'::jsonb) THEN
    RAISE EXCEPTION '067 FAILED: settings 的形狀約束放行了字串型別的天數';
  END IF;
  IF fms.tenant_settings_are_valid('{"satisfaction_editable_days": 1.5}'::jsonb) THEN
    RAISE EXCEPTION '067 FAILED: settings 的形狀約束放行了非整數的天數';
  END IF;
  IF fms.tenant_settings_are_valid('{"satisfaction_editable_days": 400}'::jsonb) THEN
    RAISE EXCEPTION '067 FAILED: settings 的形狀約束放行了超過 365 的天數';
  END IF;
  IF fms.tenant_settings_are_valid('{"satisfaction_editable_days": -1}'::jsonb) THEN
    RAISE EXCEPTION '067 FAILED: settings 的形狀約束放行了負的天數';
  END IF;
  -- 0 是合法的政策（一經送出即定案），不是「沒設定」。
  IF NOT fms.tenant_settings_are_valid('{"satisfaction_editable_days": 0}'::jsonb) THEN
    RAISE EXCEPTION '067 FAILED: 0 天該是合法的（一經送出即定案）';
  END IF;
  -- 未知的鍵放行 —— 這個欄位會長大。
  IF NOT fms.tenant_settings_are_valid('{"future_key": {"a": 1}}'::jsonb) THEN
    RAISE EXCEPTION '067 FAILED: 未知的鍵被擋了，那會讓每加一個設定都要改 migration';
  END IF;

  -- (2) 約束掛上去了。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_tenants_settings'
       AND conrelid = 'fms.tenants'::regclass
  ) THEN
    RAISE EXCEPTION '067 FAILED: tenants 沒有 ck_tenants_settings';
  END IF;

  -- (3) 兩個管道的範本都在。少了 EMAIL 那一個，不登入系統的申請人
  --     永遠不知道可以評分 —— 而端點會存在且沒有流量。
  IF (SELECT count(*) FROM fms.notification_templates
       WHERE code = 'SATISFACTION_REQUEST' AND tenant_id IS NULL) <> 2 THEN
    RAISE EXCEPTION
      '067 FAILED: SATISFACTION_REQUEST 該有 EMAIL 與 IN_APP 兩個平台範本';
  END IF;

  -- (4) 狀態機真的宣告了這個 side effect。**反向守衛**：
  --     若有人把那兩個宣告拿掉，這支 migration 就沒有觸發點了，
  --     而端點會安靜地沒有流量。
  -- 表是 `work_order_transitions_allowed`（規則），不是
  -- `work_order_transitions`（實際發生過的轉換紀錄）。兩者名字只差一個字，
  -- 而查錯那一張會讓這個守衛在剛建好的系統上永遠通不過。
  SELECT EXISTS (
    SELECT 1 FROM fms.work_order_transitions_allowed
     WHERE side_effects ? 'request_satisfaction'
  ) INTO v_ok;
  IF NOT v_ok THEN
    RAISE EXCEPTION
      '067 FAILED: 沒有任何轉換宣告 request_satisfaction —— '
      '評分邀請沒有觸發點，端點會存在但永遠沒有流量';
  END IF;

  RAISE NOTICE '067 OK：settings 有形狀約束與第一個讀者、SATISFACTION_REQUEST '
               '兩個管道、狀態機的 request_satisfaction 宣告仍在'
               '（行為驗證在 satisfaction_slice.rs）';
END;
$$;

COMMIT;
