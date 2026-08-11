-- =============================================================================
-- 071：告警抑制真的生效（`POST /alarms/{id}/suppress` 的前提）
-- =============================================================================
-- 006 建了 `alarms.status = 'SUPPRESSED'` 與 `alarms.suppressed_until`，
-- 而在這支 migration 之前**全 codebase 沒有任何程式碼讀它們**。
--
-- -----------------------------------------------------------------------------
-- 為什麼「只把狀態設成 SUPPRESSED」比不抑制更糟
-- -----------------------------------------------------------------------------
-- `fms.raise_alarm()` 尋找既有告警的條件是
-- `status IN ('ACTIVE','ACKNOWLEDGED')`。一筆 SUPPRESSED 的告警**不在那個集合
-- 裡**，因此下一次門檻觸發時：
--
--   1. 既有那筆不會被更新（`occurrence_count` 不會累加）；
--   2. **新增一筆告警**（`uq_alarms_open_per_point` 也只涵蓋 ACTIVE／ACKNOWLEDGED，
--      擋不住）；
--   3. 發出 `alarm.raised` → 通知扇出 → 該安靜的人收到信；
--   4. 規則若開了 `auto_create_work_order`，還會再開一張工單。
--
-- 也就是說：原本一筆告警安靜地累加次數，抑制之後變成每次觸發都產生一筆新告警
-- 加一封通知。**使用者按了「抑制」，得到的是更多噪音。**
--
-- 所以這支 migration 不是「加一個欄位的讀者」，它是讓那個按鈕不再造成反效果。
--
-- -----------------------------------------------------------------------------
-- 抑制一定有期限
-- -----------------------------------------------------------------------------
-- `ck_alarms_suppression_bounded` 要求 `status = 'SUPPRESSED'` 時
-- `suppressed_until IS NOT NULL`。
--
-- 無限期抑制是告警消失的方式：沒有人會記得回來解除，而一個被永久靜音的
-- 感測器與一個沒有感測器沒有差別 —— 但前者在畫面上看起來是有監控的。
-- 期限到了之後 `raise_alarm` 會把它放回 ACTIVE 並正常發報（見下）。
--
-- -----------------------------------------------------------------------------
-- 儲存層的去重要涵蓋 SUPPRESSED
-- -----------------------------------------------------------------------------
-- 006 對 `uq_alarms_open_per_point` 的註解寫著「dedupe at the storage layer」。
-- 那個保證在抑制期間是破的（見上面第 2 點）。索引因此擴到含 SUPPRESSED ——
-- 應用層的邏輯錯了還有資料庫兜著，而這正是那條註解的用意。
--
-- 這也是為什麼抑制必須有期限：一筆永久 SUPPRESSED 的列會永久佔住那個
-- 唯一鍵，於是連「解除抑制後重新觸發」都會撞號。
--
-- 依賴：006（alarms／alarm_rules／raise_alarm）、016（permissions）、
--       056（create_work_order_from_alarm，供 :reconcile-work-orders 使用）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 1. 抑制必須有期限
-- -----------------------------------------------------------------------------
-- 既有資料可能已有 status='SUPPRESSED' 而 suppressed_until 為 NULL
-- （在此之前沒有任何程式碼會寫出這種列，但手動 SQL 可能有）。
-- 先補一個期限再上約束 —— 直接加約束會讓這支 migration 在那種資料庫上失敗，
-- 而失敗的原因與這次變更無關。
--
-- `alarms` 掛了 029 的稽核觸發器，而那個觸發器寫 `audit_log`（有 RLS）。
-- migration 沒有租戶情境，因此需要平台情境才寫得進去 ——
-- 少了這一行，這支 migration 會以
-- 「new row violates row-level security policy for table "audit_log"」失敗，
-- 而那個訊息完全指不到真正的原因。**第一次執行時就是這樣失敗的。**
SELECT set_config('app.is_platform', 'on', true);

UPDATE fms.alarms
   SET suppressed_until = clock_timestamp()
 WHERE status = 'SUPPRESSED' AND suppressed_until IS NULL;

ALTER TABLE fms.alarms DROP CONSTRAINT IF EXISTS ck_alarms_suppression_bounded;
ALTER TABLE fms.alarms
  ADD CONSTRAINT ck_alarms_suppression_bounded
  CHECK (status <> 'SUPPRESSED' OR suppressed_until IS NOT NULL);

COMMENT ON COLUMN fms.alarms.suppressed_until IS
  '抑制到什麼時候。status = SUPPRESSED 時必填（ck_alarms_suppression_bounded）。'
  ' raise_alarm() 在期限內只累加次數、不發事件也不自動建單；期限過了會把狀態'
  ' 放回 ACTIVE 並恢復正常發報。無限期抑制是告警消失的方式，因此不允許。';

-- -----------------------------------------------------------------------------
-- 2. 去重索引擴到含 SUPPRESSED
-- -----------------------------------------------------------------------------
DROP INDEX IF EXISTS fms.uq_alarms_open_per_point;
CREATE UNIQUE INDEX uq_alarms_open_per_point
  ON fms.alarms (alarm_rule_id,
                 coalesce(telemetry_point_id, '00000000-0000-0000-0000-000000000000'::uuid))
  WHERE status IN ('ACTIVE', 'ACKNOWLEDGED', 'SUPPRESSED');

-- -----------------------------------------------------------------------------
-- 3. raise_alarm 尊重抑制
-- -----------------------------------------------------------------------------
-- 與 006 的版本只有三處不同，其餘原樣保留（含自動建單那一整段）：
--
--   a. 既有告警的查詢條件加入 SUPPRESSED；
--   b. 抑制期限內：累加次數，**不發 alarm.raised、不自動建單**，直接回傳；
--   c. 抑制期限已過：放回 ACTIVE、清掉 suppressed_until，然後走原本的流程。
--
-- (b) 的「不發事件」是抑制的**全部意義**。少了它，抑制只是換一個狀態字串。
CREATE OR REPLACE FUNCTION fms.raise_alarm(
  p_alarm_rule_id      uuid,
  p_telemetry_point_id uuid DEFAULT NULL,
  p_trigger_value      numeric DEFAULT NULL,
  p_message            varchar DEFAULT NULL,
  p_observed_at        timestamptz DEFAULT NULL
) RETURNS uuid
LANGUAGE plpgsql
AS $$
DECLARE
  v_rule   fms.alarm_rules;
  v_device fms.devices;
  v_alarm  fms.alarms;
  v_alarm_id uuid;
  v_asset_id uuid;
  v_node_id  uuid;
  v_facility_id uuid;
  v_wo_id  uuid;
  v_at     timestamptz := coalesce(p_observed_at, clock_timestamp());
BEGIN
  SELECT * INTO v_rule FROM fms.alarm_rules WHERE id = p_alarm_rule_id AND is_active;
  IF v_rule.id IS NULL THEN
    RAISE EXCEPTION 'alarm rule % not found or inactive', p_alarm_rule_id USING ERRCODE = 'P0002';
  END IF;

  IF p_telemetry_point_id IS NOT NULL THEN
    SELECT d.* INTO v_device
      FROM fms.devices d
      JOIN fms.telemetry_points tp ON tp.device_id = d.id
     WHERE tp.id = p_telemetry_point_id;
    v_asset_id := v_device.asset_id;
    v_node_id  := v_device.spatial_node_id;
    v_facility_id := v_device.facility_id;
  ELSE
    v_facility_id := v_rule.facility_id;
  END IF;

  -- (a) SUPPRESSED 也算「已存在的開啟告警」。少了它，被抑制的告警會被當成
  --     不存在，於是下面的 INSERT 會產生一筆重複的告警與一封通知。
  SELECT * INTO v_alarm
    FROM fms.alarms
   WHERE alarm_rule_id = p_alarm_rule_id
     AND coalesce(telemetry_point_id, '00000000-0000-0000-0000-000000000000'::uuid)
         = coalesce(p_telemetry_point_id, '00000000-0000-0000-0000-000000000000'::uuid)
     AND status IN ('ACTIVE','ACKNOWLEDGED','SUPPRESSED')
   FOR UPDATE;

  -- (b) 抑制期限內：只留痕跡，不驚動任何人。
  --
  --     仍然累加 occurrence_count 與 last_seen_at 是刻意的：抑制的是**通知**，
  --     不是事實。解除之後有人要能看出「這段時間它其實響了 400 次」——
  --     不記的話，抑制就變成把證據也一起關掉。
  IF v_alarm.id IS NOT NULL
     AND v_alarm.status = 'SUPPRESSED'
     AND v_alarm.suppressed_until > v_at THEN
    UPDATE fms.alarms
       SET last_seen_at = v_at,
           occurrence_count = occurrence_count + 1,
           trigger_value = coalesce(p_trigger_value, trigger_value),
           updated_at = clock_timestamp()
     WHERE id = v_alarm.id;
    RETURN v_alarm.id;
  END IF;

  -- (c) 抑制期限已過：放回 ACTIVE，然後照正常流程走完（含發事件與自動建單）。
  --     不放回的話，`suppressed_until` 過期的告警會永遠停在 SUPPRESSED，
  --     而它佔著 uq_alarms_open_per_point 的唯一鍵 —— 新告警也建不出來，
  --     於是那個測點從此靜音。
  IF v_alarm.id IS NOT NULL AND v_alarm.status = 'SUPPRESSED' THEN
    UPDATE fms.alarms
       SET status = 'ACTIVE',
           suppressed_until = NULL,
           updated_at = clock_timestamp()
     WHERE id = v_alarm.id;
    v_alarm.status := 'ACTIVE';
  END IF;

  IF v_alarm.id IS NOT NULL THEN
    UPDATE fms.alarms
       SET last_seen_at = v_at,
           occurrence_count = occurrence_count + 1,
           trigger_value = coalesce(p_trigger_value, trigger_value),
           updated_at = clock_timestamp()
     WHERE id = v_alarm.id;
    v_alarm_id := v_alarm.id;
  ELSE
    INSERT INTO fms.alarms (
      tenant_id, facility_id, alarm_no, alarm_rule_id, device_id, telemetry_point_id,
      asset_id, spatial_node_id, severity, message, trigger_value,
      first_seen_at, last_seen_at)
    VALUES (
      v_rule.tenant_id, v_facility_id,
      fms.next_document_no(v_rule.tenant_id, 'ALARM', 'AL'),
      v_rule.id, v_device.id, p_telemetry_point_id,
      v_asset_id, v_node_id, v_rule.severity,
      coalesce(p_message, v_rule.name), p_trigger_value, v_at, v_at)
    RETURNING id INTO v_alarm_id;

    PERFORM fms.emit_event(v_rule.tenant_id, 'alarm.raised', 'ALARM', v_alarm_id,
      jsonb_build_object('rule_code', v_rule.code, 'severity', v_rule.severity,
                         'asset_id', v_asset_id, 'facility_id', v_facility_id,
                         'trigger_value', p_trigger_value));
  END IF;

  -- 自動建單。原樣沿用 006 的邏輯（含 dedupe_window_minutes 的去重）。
  IF v_rule.auto_create_work_order THEN
    SELECT wo.id INTO v_wo_id
      FROM fms.work_orders wo
     WHERE wo.tenant_id = v_rule.tenant_id
       AND wo.deleted_at IS NULL
       AND wo.status NOT IN ('COMPLETED','CLOSED','CANCELLED','REJECTED')
       AND (
             (v_asset_id IS NOT NULL AND wo.asset_id = v_asset_id)
          OR (v_asset_id IS NULL AND wo.spatial_node_id = v_node_id)
           )
       AND wo.created_at > clock_timestamp() - (v_rule.dedupe_window_minutes || ' minutes')::interval
     ORDER BY wo.created_at DESC
     LIMIT 1;

    IF v_wo_id IS NULL THEN
      INSERT INTO fms.work_orders (
        tenant_id, facility_id, wo_no, work_order_type, source, title, description,
        asset_id, spatial_node_id, service_item_id, alarm_id,
        priority, status, team_id, sla_policy_id)
      VALUES (
        v_rule.tenant_id, v_facility_id,
        fms.next_document_no(v_rule.tenant_id, 'WORK_ORDER', 'WO'),
        coalesce(v_rule.wo_work_order_type, 'CORRECTIVE'), 'IOT_ALARM',
        coalesce(p_message, v_rule.name),
        'Raised automatically by alarm rule ' || v_rule.code,
        v_asset_id, v_node_id, v_rule.wo_service_item_id, v_alarm_id,
        coalesce(v_rule.wo_priority, 'NORMAL'), 'SUBMITTED',
        v_rule.wo_team_id, v_rule.wo_sla_policy_id)
      RETURNING id INTO v_wo_id;
    END IF;

    UPDATE fms.alarms
       SET work_order_id = v_wo_id,
           work_order_created_at = coalesce(work_order_created_at, clock_timestamp()),
           updated_at = clock_timestamp()
     WHERE id = v_alarm_id AND work_order_id IS NULL;
  END IF;

  RETURN v_alarm_id;
END;
$$;

-- -----------------------------------------------------------------------------
-- 4. alarm:suppress
-- -----------------------------------------------------------------------------
-- **不沿用 `alarm:acknowledge`。** 「確認」只是留下「有人看到了」的紀錄；
-- 「抑制」讓系統在一段時間內對某個條件閉嘴。持有前者的角色包含 TECHNICIAN
-- 與 SERVICE_STAFF —— 現場人員能確認告警是對的，但不該能讓監控靜音。
INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous)
VALUES ('alarm:suppress', 'alarm', 'suppress', 'IOT',
        '在指定期限內抑制告警的通知（次數仍會累計）。比 alarm:acknowledge 強：'
        '它讓監控在那段時間內不再發報', 'FACILITY', false)
ON CONFLICT (code) DO UPDATE
  SET resource = EXCLUDED.resource,
      action = EXCLUDED.action,
      module = EXCLUDED.module,
      description = EXCLUDED.description,
      min_scope_level = EXCLUDED.min_scope_level;

-- 授予對象由 `alarm_rule:write` 的持有者推導，不是另寫一份會漂移的名單
-- （060 的同一個做法）。
--
-- 為什麼是這個權限：能改門檻的人**已經**可以讓告警安靜（把門檻調高就行）。
-- 抑制沒有給出新的能力，只是給了一個有期限、留痕跡的做法 —— 因此授予範圍
-- 一致是對的。而它不該落到只能 acknowledge 的人手上。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT DISTINCT rp.role_id, 'alarm:suppress'
  FROM fms.role_permissions rp
  JOIN fms.roles r ON r.id = rp.role_id
 WHERE rp.permission_code = 'alarm_rule:write'
   AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 5. 抑制時長的上限是租戶政策
-- -----------------------------------------------------------------------------
-- 「一個操作員最多能讓某個條件安靜多久」是管理者定義的條件，不是程式碼的事實。
-- 走 067 建好的 `tenants.settings` 機制（070 已經在這裡加過 password_min_length，
-- 同一個做法）。
--
-- 下界 5 分鐘：比這更短的抑制沒有用途，而一個設成 0 的值會讓抑制端點永遠失敗。
-- 上界 10080（7 天）：超過一週的靜音實際上就是關掉那個測點，
-- 而那應該走「停用規則」而不是「抑制一筆告警」——後者在畫面上看起來還在監控。
CREATE OR REPLACE FUNCTION fms.tenant_settings_are_valid(p jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE
    WHEN p IS NULL THEN true
    WHEN jsonb_typeof(p) <> 'object' THEN false
    ELSE
      (CASE
        WHEN NOT (p ? 'satisfaction_editable_days') THEN true
        WHEN jsonb_typeof(p -> 'satisfaction_editable_days') <> 'number' THEN false
        WHEN (p ->> 'satisfaction_editable_days')::numeric
               <> trunc((p ->> 'satisfaction_editable_days')::numeric) THEN false
        WHEN (p ->> 'satisfaction_editable_days')::numeric NOT BETWEEN 0 AND 365
          THEN false
        ELSE true
      END)
      AND
      (CASE
        WHEN NOT (p ? 'password_min_length') THEN true
        WHEN jsonb_typeof(p -> 'password_min_length') <> 'number' THEN false
        WHEN (p ->> 'password_min_length')::numeric
               <> trunc((p ->> 'password_min_length')::numeric) THEN false
        WHEN (p ->> 'password_min_length')::numeric NOT BETWEEN 8 AND 128
          THEN false
        ELSE true
      END)
      AND
      (CASE
        WHEN NOT (p ? 'alarm_max_suppress_minutes') THEN true
        WHEN jsonb_typeof(p -> 'alarm_max_suppress_minutes') <> 'number' THEN false
        WHEN (p ->> 'alarm_max_suppress_minutes')::numeric
               <> trunc((p ->> 'alarm_max_suppress_minutes')::numeric) THEN false
        WHEN (p ->> 'alarm_max_suppress_minutes')::numeric NOT BETWEEN 5 AND 10080
          THEN false
        ELSE true
      END)
  END;
$$;
COMMENT ON FUNCTION fms.tenant_settings_are_valid(jsonb) IS
  'tenants.settings 的形狀。只驗已知的鍵（未知的放行，這個欄位會長大）；'
  '已知的鍵型別錯了會在讀設定的地方炸，而那離設定它的人三層之外。';

ALTER TABLE fms.tenants DROP CONSTRAINT IF EXISTS ck_tenants_settings;
ALTER TABLE fms.tenants
  ADD CONSTRAINT ck_tenants_settings
  CHECK (fms.tenant_settings_are_valid(settings));

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構的。**抑制期間不再發事件、不再產生重複告警、期限過後恢復 —— 全部在
--   alarm_suppression_slice.rs。** 那些需要裝置、測點與規則，屬於行為驗證。
DO $$
DECLARE
  v_src text;
BEGIN
  -- (1) 抑制必須有期限。少了它，一筆永久 SUPPRESSED 的列會永久佔住
  --     uq_alarms_open_per_point 的唯一鍵，那個測點從此靜音。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_alarms_suppression_bounded'
       AND conrelid = 'fms.alarms'::regclass
       AND pg_get_constraintdef(oid) LIKE '%suppressed_until%'
  ) THEN
    RAISE EXCEPTION '071 FAILED: ck_alarms_suppression_bounded 不存在或沒有指名 suppressed_until';
  END IF;

  -- (2) 去重索引要涵蓋 SUPPRESSED。這是「應用層寫錯時資料庫兜著」的那一層。
  IF NOT EXISTS (
    SELECT 1 FROM pg_indexes
     WHERE schemaname = 'fms' AND indexname = 'uq_alarms_open_per_point'
       AND indexdef LIKE '%SUPPRESSED%'
  ) THEN
    RAISE EXCEPTION
      '071 FAILED: uq_alarms_open_per_point 沒有涵蓋 SUPPRESSED —— '
      '抑制期間可以插入重複告警';
  END IF;

  -- (3) raise_alarm 必須真的讀 suppressed_until。
  --     **先去掉註解再比對**：一段解釋抑制的註解會讓這一格通過，
  --     而那正是它要防的事（065 踩過）。
  SELECT regexp_replace(prosrc, '--[^\n]*', '', 'g') INTO v_src
    FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
   WHERE n.nspname = 'fms' AND p.proname = 'raise_alarm';

  IF v_src IS NULL THEN
    RAISE EXCEPTION '071 FAILED: 找不到 fms.raise_alarm';
  END IF;
  IF v_src NOT LIKE '%suppressed_until%' THEN
    RAISE EXCEPTION
      '071 FAILED: raise_alarm 沒有讀 suppressed_until —— 抑制不會生效';
  END IF;
  -- 既有告警的查詢必須含 SUPPRESSED，否則被抑制的告警會被當成不存在。
  IF v_src NOT LIKE '%''ACTIVE'',''ACKNOWLEDGED'',''SUPPRESSED''%'
     AND v_src NOT LIKE '%''ACTIVE'', ''ACKNOWLEDGED'', ''SUPPRESSED''%' THEN
    RAISE EXCEPTION
      '071 FAILED: raise_alarm 尋找既有告警時沒有把 SUPPRESSED 算進去';
  END IF;

  -- (4) 權限存在，而且**不是**給 alarm:acknowledge 的持有者。
  --     兩者混同的話，現場人員就能讓監控靜音。
  IF NOT EXISTS (SELECT 1 FROM fms.permissions WHERE code = 'alarm:suppress') THEN
    RAISE EXCEPTION '071 FAILED: alarm:suppress 未建立';
  END IF;
  IF EXISTS (
    SELECT 1 FROM fms.role_permissions rp
      JOIN fms.roles r ON r.id = rp.role_id
     WHERE rp.permission_code = 'alarm:suppress'
       AND r.tenant_id IS NULL
       AND r.code IN ('TECHNICIAN', 'SERVICE_STAFF', 'VIEWER', 'REQUESTER')
  ) THEN
    RAISE EXCEPTION
      '071 FAILED: alarm:suppress 落到了只該能 acknowledge 的角色手上';
  END IF;
  -- 也不能一個都沒授出去 —— 那樣這個端點沒有人叫得動。
  IF NOT EXISTS (
    SELECT 1 FROM fms.role_permissions WHERE permission_code = 'alarm:suppress'
  ) THEN
    RAISE EXCEPTION '071 FAILED: alarm:suppress 沒有授予任何角色';
  END IF;

  -- (5) 抑制上限的形狀約束真的在守，而且 070／067 的鍵沒有因為這次重寫失效
  --     （三個鍵是 AND，寫錯很容易變成只驗最新的那一個）。
  IF fms.tenant_settings_are_valid('{"alarm_max_suppress_minutes": 0}'::jsonb) THEN
    RAISE EXCEPTION '071 FAILED: settings 放行了 alarm_max_suppress_minutes = 0';
  END IF;
  IF fms.tenant_settings_are_valid('{"alarm_max_suppress_minutes": 20000}'::jsonb) THEN
    RAISE EXCEPTION
      '071 FAILED: settings 放行了超過 7 天的抑制上限 —— 那實際上是關掉監控';
  END IF;
  IF NOT fms.tenant_settings_are_valid('{"alarm_max_suppress_minutes": 240}'::jsonb) THEN
    RAISE EXCEPTION '071 FAILED: settings 擋掉了合法的 240 分鐘';
  END IF;
  IF fms.tenant_settings_are_valid('{"password_min_length": 2}'::jsonb) THEN
    RAISE EXCEPTION '071 FAILED: 重寫之後 password_min_length（070）不再被驗證';
  END IF;
  IF fms.tenant_settings_are_valid('{"satisfaction_editable_days": 400}'::jsonb) THEN
    RAISE EXCEPTION
      '071 FAILED: 重寫之後 satisfaction_editable_days（067）不再被驗證';
  END IF;
  -- 三鍵並存、只有最後一個錯 —— AND 短路寫錯時這格會過。
  IF fms.tenant_settings_are_valid(
       '{"satisfaction_editable_days": 14, "password_min_length": 12,
          "alarm_max_suppress_minutes": 1}'::jsonb) THEN
    RAISE EXCEPTION '071 FAILED: 三鍵並存時 alarm_max_suppress_minutes 沒有被驗證';
  END IF;

  RAISE NOTICE '071 OK：抑制有期限、去重索引涵蓋 SUPPRESSED、raise_alarm 讀'
               ' suppressed_until、alarm:suppress 授予 alarm_rule:write 的持有者'
               '（行為驗證在 alarm_suppression_slice.rs）';
END;
$$;

COMMIT;
