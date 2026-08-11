-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 056: 由告警人工補建工單
-- =============================================================================
-- `POST /alarms/{alarmId}/work-order` 的落地處。契約說它是給
-- 「規則未設定自動建單、或歷史未串接的告警」用的一鍵補建。
--
-- -----------------------------------------------------------------------------
-- 為什麼是資料庫函式，而不是 handler 裡的 INSERT
-- -----------------------------------------------------------------------------
-- **重複開單的防護必須是原子的。** 契約明寫 `409 該告警已關聯工單`，
-- 而在 handler 裡「先讀 work_order_id、是 NULL 才 INSERT」在並發下會失效：
-- 兩個請求都讀到 NULL，兩張工單都建出來，第二次的 UPDATE 才發現撞了 ——
-- 但那時第一張已經送進派工流程。
--
-- 這裡的判定是一次 `UPDATE ... WHERE work_order_id IS NULL RETURNING`：
-- 沒搶到的那一方拿不到列，函式回 NULL，handler 據此回 409。
-- 工單是在搶到之後才建的，所以輸掉的請求不會留下任何東西。
--
-- 006 的 `raise_alarm` 用的是同一條述詞
-- （`UPDATE fms.alarms ... WHERE id = v_alarm_id AND work_order_id IS NULL`），
-- 因此**自動建單與人工補建互相之間也是安全的** —— 不是只有人工那一側。
--
-- -----------------------------------------------------------------------------
-- 欄位取值刻意跟著 raise_alarm
-- -----------------------------------------------------------------------------
-- `source` 仍然是 `IOT_ALARM`：這張工單的起因是告警，不是有人手動開的。
-- 若標成 MANUAL，`GET /alarms?unlinked_only=true` 那種串接稽核就會把兩種
-- 來源混在一起，而報表也會低估 IoT 觸發的工單量。
--
-- **人工補建與自動建單的差別只在時間點**，不在來源。
--
-- 依賴：006（alarms／raise_alarm／emit_event）、004（work_orders）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- 回傳新建工單的 id；告警已經有工單（或不存在／不可見）時回 NULL。
--
-- 不是 SECURITY DEFINER：呼叫端是 `fms_app` 且已注入租戶與場域情境，
-- RLS 因此照常生效 —— 看不到的告警在這裡就是「找不到」。
-- 那是刻意的：這支函式不該成為繞過場域範圍的後門。
CREATE OR REPLACE FUNCTION fms.create_work_order_from_alarm(
  p_alarm_id         uuid,
  p_work_order_type  text DEFAULT NULL,
  p_priority         text DEFAULT NULL,
  p_team_id          uuid DEFAULT NULL,
  p_assignee_id      uuid DEFAULT NULL,
  p_title            text DEFAULT NULL
) RETURNS uuid
LANGUAGE plpgsql
AS $$
DECLARE
  v_alarm fms.alarms;
  v_wo_id uuid;
BEGIN
  SELECT * INTO v_alarm FROM fms.alarms WHERE id = p_alarm_id;
  IF NOT FOUND THEN
    RETURN NULL;                      -- 不存在，或 RLS 讓它不可見
  END IF;
  IF v_alarm.work_order_id IS NOT NULL THEN
    RETURN NULL;                      -- 已經有工單 —— handler 回 409
  END IF;

  INSERT INTO fms.work_orders (
    tenant_id, facility_id, wo_no, work_order_type, source, title, description,
    asset_id, spatial_node_id, alarm_id, priority, status, team_id, assignee_id)
  VALUES (
    v_alarm.tenant_id, v_alarm.facility_id,
    fms.next_document_no(v_alarm.tenant_id, 'WORK_ORDER', 'WO'),
    coalesce(p_work_order_type, 'CORRECTIVE'),
    -- 來源仍是 IOT_ALARM：起因是告警，不是有人手動開的。見檔頭。
    'IOT_ALARM',
    coalesce(p_title, v_alarm.message),
    format('由告警 %s 人工補建（嚴重度 %s，發生 %s 次）',
           v_alarm.alarm_no, v_alarm.severity, v_alarm.occurrence_count),
    v_alarm.asset_id, v_alarm.spatial_node_id, v_alarm.id,
    coalesce(p_priority, 'HIGH'), 'SUBMITTED', p_team_id, p_assignee_id)
  RETURNING id INTO v_wo_id;

  -- **這一步是防護本身。** 條件式 UPDATE：沒搶到就代表別人先綁上了。
  UPDATE fms.alarms
     SET work_order_id = v_wo_id,
         work_order_created_at = clock_timestamp()
   WHERE id = p_alarm_id AND work_order_id IS NULL;

  IF NOT FOUND THEN
    -- 輸掉了併發。把剛建的工單收回去 —— 留著它就會有兩張工單指向同一個告警，
    -- 而其中一張沒有人知道它存在。
    RAISE EXCEPTION 'ALARM_ALREADY_LINKED'
      USING ERRCODE = '40001';        -- serialization_failure：呼叫端可重試
  END IF;

  PERFORM fms.emit_event(v_alarm.tenant_id, 'work_order.created', 'WORK_ORDER', v_wo_id,
    jsonb_build_object('source', 'IOT_ALARM', 'alarm_id', p_alarm_id, 'manual_link', true));

  RETURN v_wo_id;
END;
$$;

COMMENT ON FUNCTION fms.create_work_order_from_alarm(uuid, text, text, uuid, uuid, text) IS
  '由告警人工補建工單。回 NULL 代表告警不存在／不可見／已有工單（handler 回 409）。'
  ' 防重複是條件式 UPDATE，與 006 的 raise_alarm 用同一條述詞，因此自動與人工互相安全。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構的：行為驗證需要一個真的告警，而 CORE 階段沒有資料
--（053／054 記過同一個層次問題）。行為在 `alarm_slice.rs`。
DO $$
DECLARE v_src text;
BEGIN
  IF to_regprocedure('fms.create_work_order_from_alarm(uuid,text,text,uuid,uuid,text)') IS NULL THEN
    RAISE EXCEPTION '056 FAILED: 函式不存在';
  END IF;

  v_src := pg_get_functiondef(
    'fms.create_work_order_from_alarm(uuid,text,text,uuid,uuid,text)'::regprocedure);

  -- (1) 防重複必須是條件式 UPDATE。少了 `AND work_order_id IS NULL`，
  --     並發下會有兩張工單指向同一個告警。
  IF v_src NOT LIKE '%work_order_id IS NULL%' THEN
    RAISE EXCEPTION
      '056 FAILED: 沒有條件式 UPDATE —— 並發下會建出兩張指向同一告警的工單';
  END IF;

  -- (2) 來源必須是 IOT_ALARM。標成 MANUAL 會讓串接稽核與報表把兩種來源混在一起。
  IF v_src NOT LIKE '%''IOT_ALARM''%' THEN
    RAISE EXCEPTION '056 FAILED: source 不是 IOT_ALARM';
  END IF;

  -- (3) **不可以是 SECURITY DEFINER。** 那會讓這支函式變成繞過場域範圍的後門。
  IF EXISTS (SELECT 1 FROM pg_proc p
              WHERE p.oid = 'fms.create_work_order_from_alarm(uuid,text,text,uuid,uuid,text)'::regprocedure
                AND p.prosecdef) THEN
    RAISE EXCEPTION
      '056 FAILED: 函式是 SECURITY DEFINER —— 它會繞過 alarms 的 facility_scope';
  END IF;

  RAISE NOTICE '056 OK：條件式 UPDATE 防重複、來源 IOT_ALARM、非 SECURITY DEFINER';
END;
$$;

COMMIT;
