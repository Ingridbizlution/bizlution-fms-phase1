-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 041: 通知扇出（讓 side_effects.notify 與範本真的有人讀）
-- =============================================================================
-- 035 讓掃描自動觸發 `BREACH_SLA`，而那條規則宣告了
-- `notify: [FACILITY_ADMIN, MAINTENANCE_SUPERVISOR]`。當時的檔頭寫得很明白：
-- **沒有任何程式碼寫 `fms.notifications`**，因此升級不會通知任何人。
--
-- 那一句底下其實有兩個「宣告了沒人讀」：
--   * `work_order_transitions_allowed.side_effects.notify` —— 13 條規則宣告
--   * `fms.notification_templates` —— 009 種了 **12 個範本**，零個讀取點
--     （含現成的 `WO_SLA_BREACH`）
--
-- 這個 migration 建的是**扇出**：事件 → 收件人 → 渲染 → `notifications` 列。
-- **不含投遞**：EMAIL／PUSH 需要 SMTP／推播的傳輸層，那是獨立的一件事。
-- 因此 EMAIL 的列會停在 `QUEUED`，而 `IN_APP` 的列本身就是通知
-- （`GET /notifications` 讀得到）。
--
-- -----------------------------------------------------------------------------
-- notify 清單裡有兩種東西，而其中一個是危險的碰撞
-- -----------------------------------------------------------------------------
-- 實測 13 條規則用到六個值：
--
--     ASSIGNEE   REQUESTER   APPROVER   DISPATCHER
--     FACILITY_ADMIN   MAINTENANCE_SUPERVISOR
--
-- 而 `fms.roles` 裡**同時存在** `REQUESTER` 與 `DISPATCHER` 兩個角色碼。
--
-- `COMPLETE` 的 `notify: ["REQUESTER"]` 顯然是「通知報修的那個人」；
-- 若解析成角色，一張工單完成會群發給場域內每一個 REQUESTER 角色的使用者。
-- **那是一次群發事故，而它看起來完全像正常運作。**
--
-- 因此解析順序是：**關係代號優先，其餘才當角色碼。**
--   `ASSIGNEE`  → `work_orders.assignee_id`
--   `REQUESTER` → `work_orders.requester_id`（不是角色）
--   其他        → `roles.code`，且範圍要涵蓋該工單的場域
--
-- `APPROVER` 兩邊都不是（沒有那個角色、也沒有 approver 欄位）——
-- 它**解析不到任何人**。那不會被靜默丟掉：`fan_out_notifications` 把它計入
-- `unresolved` 回傳，worker 記成 warn。一個宣告了要通知卻沒有人收到的規則，
-- 必須看得見。
--
-- -----------------------------------------------------------------------------
-- 範圍用 021 的權威函式，不自己再寫一份
-- -----------------------------------------------------------------------------
-- 角色碼要解析成「持有該角色、且範圍涵蓋這個場域的人」。那個判斷已經有
-- 權威實作：`fms.user_accessible_facilities(user_id)`（021）。
-- 在這裡重寫一次 scope_type/scope_id 的展開，就是同一條規則的第二份實作。
--
-- 依賴：009（範本與角色）、015（side_effects）、021（可存取場域）、035（升級）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 0. 扇出必須是幂等的
-- -----------------------------------------------------------------------------
-- relay 的交付保證是 **at-least-once**（`fms-worker` 檔頭寫明了：COMMIT 前
-- 崩潰的事件會被重新取用），而 handler「必須自行具備幂等性」。
--
-- 扇出天生不是幂等的：重跑一次就多一批通知列。收件人收到兩封同樣的
-- 「工單逾期」是小事，但那是**每次 relay 重啟都可能發生**的小事。
--
-- 因此把來源事件記在通知上，並用唯一索引擋住重放。這比在應用層記
-- 「處理過哪些 event_id」好：那份紀錄本身又需要幂等。
ALTER TABLE fms.notifications
  ADD COLUMN IF NOT EXISTS source_event_id bigint;

COMMENT ON COLUMN fms.notifications.source_event_id IS
  '產生這筆通知的 event_outbox.id。用於扇出的幂等（relay 是 at-least-once）。'
  '刻意不加外鍵：outbox 會被歸檔清理，而通知的歷史要留下來。';

-- 同一個事件、同一個人、同一個頻道只會有一筆。
-- 部分索引（`WHERE source_event_id IS NOT NULL`）讓不是由事件產生的通知
-- （日後的公告、手動發送）不受這個約束。
CREATE UNIQUE INDEX IF NOT EXISTS uq_notifications_event_recipient
  ON fms.notifications (source_event_id, recipient_user_id, channel)
  WHERE source_event_id IS NOT NULL;

-- -----------------------------------------------------------------------------
-- 1. 範本渲染
-- -----------------------------------------------------------------------------
-- `{{key}}` → `p_vars ->> 'key'`。
--
-- **找不到的 placeholder 原樣留下**，不換成空字串。理由：一個收到
-- 「工單 {{wo_no}} 已逾期」的人會回報這件事；而收到「工單 已逾期」的人
-- 只會覺得系統有點爛，沒有人會去查那個缺的變數是什麼。
-- 留著它是為了讓資料缺口有聲音。
CREATE OR REPLACE FUNCTION fms.render_template(p_template text, p_vars jsonb)
RETURNS text
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
  v_out text := p_template;
  v_key text;
BEGIN
  IF p_template IS NULL THEN
    RETURN NULL;
  END IF;
  FOR v_key IN SELECT jsonb_object_keys(coalesce(p_vars, '{}'::jsonb)) LOOP
    IF p_vars ->> v_key IS NOT NULL THEN
      v_out := replace(v_out, '{{' || v_key || '}}', p_vars ->> v_key);
    END IF;
  END LOOP;
  RETURN v_out;
END;
$$;

COMMENT ON FUNCTION fms.render_template(text, jsonb) IS
  '把 {{key}} 換成 p_vars 的值。找不到的 placeholder 原樣留下 —— '
  '換成空字串會讓資料缺口沒有聲音。';

-- -----------------------------------------------------------------------------
-- 2. 工單的範本變數
-- -----------------------------------------------------------------------------
-- 範本要的變數（`wo_no`／`title`／`resolution_due_at`／`status`／
-- `assignee_name`…）**不在事件的 payload 裡** —— 032 的 `emit_event` 只帶了
-- 狀態機關心的欄位。刻意不去擴充那個 payload：事件是事件，不是視圖模型，
-- 而每個範本要的變數不同。
--
-- 代價是這裡讀的是**現在**的工單，而不是事件發生那一刻的。對通知而言
-- 那是對的：收件人要看的是他現在該處理什麼。
CREATE OR REPLACE FUNCTION fms.notification_vars(p_work_order_id uuid)
RETURNS jsonb
LANGUAGE sql
STABLE
AS $$
  SELECT jsonb_strip_nulls(jsonb_build_object(
    'wo_no',              wo.wo_no,
    'title',              wo.title,
    'status',             wo.status,
    'priority',           wo.priority,
    'resolution_due_at',  to_char(wo.resolution_due_at AT TIME ZONE
                            coalesce(f.timezone, fms.partition_boundary_timezone()),
                            'YYYY-MM-DD HH24:MI'),
    'response_due_at',    to_char(wo.response_due_at AT TIME ZONE
                            coalesce(f.timezone, fms.partition_boundary_timezone()),
                            'YYYY-MM-DD HH24:MI'),
    'completed_at',       to_char(wo.completed_at AT TIME ZONE
                            coalesce(f.timezone, fms.partition_boundary_timezone()),
                            'YYYY-MM-DD HH24:MI'),
    'facility_name',      f.name,
    'location_name',      sn.name,
    'assignee_name',      au.display_name,
    'requester_name',     ru.display_name,
    'resolution_notes',   wo.resolution_notes
  ))
    FROM fms.work_orders wo
    LEFT JOIN fms.facilities f     ON f.id  = wo.facility_id
    LEFT JOIN fms.spatial_nodes sn ON sn.id = wo.spatial_node_id
    LEFT JOIN fms.users au         ON au.id = wo.assignee_id
    LEFT JOIN fms.users ru         ON ru.id = wo.requester_id
   WHERE wo.id = p_work_order_id;
$$;

COMMENT ON FUNCTION fms.notification_vars(uuid) IS
  '工單的範本變數。時刻以該場域的時區格式化 —— 收件人看到的應該是他當地的時間。';

-- -----------------------------------------------------------------------------
-- 3. 收件人解析
-- -----------------------------------------------------------------------------
-- `user_id` 為 NULL 代表那個代號解析不到任何人 —— 呼叫端據此計數，
-- 而不是靜默丟掉。
CREATE OR REPLACE FUNCTION fms.notification_recipients(
  p_work_order_id uuid,
  p_notify        jsonb
) RETURNS TABLE (token text, user_id uuid)
LANGUAGE sql
STABLE
AS $$
  WITH tokens AS (
    SELECT jsonb_array_elements_text(coalesce(p_notify, '[]'::jsonb)) AS tok
  ), wo AS (
    SELECT id, facility_id, assignee_id, requester_id
      FROM fms.work_orders WHERE id = p_work_order_id
  )
  -- 關係代號優先。`REQUESTER` 與 `DISPATCHER` 也是角色碼，而 REQUESTER
  -- 在 notify 清單裡指的是「報修的那個人」—— 當成角色會變成群發。
  SELECT t.tok, wo.assignee_id
    FROM tokens t, wo WHERE t.tok = 'ASSIGNEE'
  UNION ALL
  SELECT t.tok, wo.requester_id
    FROM tokens t, wo WHERE t.tok = 'REQUESTER'
  UNION ALL
  -- 其餘當角色碼：持有該角色、且可存取範圍涵蓋這個工單的場域。
  -- 範圍判斷用 021 的權威函式，不在這裡重寫一份。
  SELECT t.tok, ura.user_id
    FROM tokens t
    JOIN fms.roles r ON r.code = t.tok
    JOIN fms.user_role_assignments ura ON ura.role_id = r.id
    CROSS JOIN wo
   WHERE t.tok NOT IN ('ASSIGNEE', 'REQUESTER')
     AND (wo.facility_id IS NULL
          OR wo.facility_id IN (
               SELECT af FROM fms.user_accessible_facilities(ura.user_id) af))
  UNION ALL
  -- 既不是關係代號也不是角色碼 → 解析不到。`APPROVER` 就是這一類。
  SELECT t.tok, NULL::uuid
    FROM tokens t
   WHERE t.tok NOT IN ('ASSIGNEE', 'REQUESTER')
     AND NOT EXISTS (SELECT 1 FROM fms.roles r WHERE r.code = t.tok);
$$;

COMMENT ON FUNCTION fms.notification_recipients(uuid, jsonb) IS
  'notify 清單 → 使用者。關係代號（ASSIGNEE/REQUESTER）優先於角色碼，'
  '因為 REQUESTER 兩者都是而語意不同（群發事故）。user_id 為 NULL = 解析不到。';

-- -----------------------------------------------------------------------------
-- 4. 扇出
-- -----------------------------------------------------------------------------
-- 一個 outbox 事件 → N 筆 `notifications`。回三個計數，因為它們在維運上是
-- 三種不同的訊息：
--   `created`         實際建了幾筆
--   `no_template`     規則宣告了 notify 但沒有對應範本 → **沒有人會收到**
--   `unresolved`      有代號解析不到人 → **也沒有人會收到**
--
-- 後兩者都是「宣告了要通知但不會發生」的情況。它們不拋錯（一封通知發不出去
-- 不該讓工單的狀態變更失敗），但必須被計數 —— 否則就是另一個沉默失效。
CREATE OR REPLACE FUNCTION fms.fan_out_notifications(p_event_id bigint)
RETURNS TABLE (created int, no_template int, unresolved int)
LANGUAGE plpgsql
AS $$
DECLARE
  v_ev        record;
  v_rule      record;
  v_vars      jsonb;
  v_created   int := 0;
  v_missing   int := 0;
  v_unres     int := 0;
BEGIN
  SELECT tenant_id, event_type, aggregate_type, aggregate_id, payload
    INTO v_ev
    FROM fms.event_outbox WHERE id = p_event_id;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'event % not found', p_event_id USING ERRCODE = 'P0002';
  END IF;

  -- 目前只有工單的狀態機會宣告 notify。其他 aggregate（告警、預約）
  -- 也有範本，但它們的 notify 清單還不存在 —— 那是另一件事。
  IF v_ev.aggregate_type <> 'WORK_ORDER' THEN
    RETURN QUERY SELECT 0, 0, 0;
    RETURN;
  END IF;

  -- 事件的 payload 帶了 `from` 與 `action`，因此可以找回當初那條規則
  -- （notify 與 template 都住在它的 side_effects 裡）。
  SELECT side_effects INTO v_rule
    FROM fms.work_order_transitions_allowed t
   WHERE t.is_active
     AND t.from_status = v_ev.payload ->> 'from'
     AND t.action = v_ev.payload ->> 'action'
     AND (t.tenant_id IS NULL OR t.tenant_id = v_ev.tenant_id)
   ORDER BY t.tenant_id NULLS LAST
   LIMIT 1;

  IF v_rule.side_effects IS NULL OR NOT (v_rule.side_effects ? 'notify') THEN
    RETURN QUERY SELECT 0, 0, 0;   -- 這條轉移不通知任何人，正常情況
    RETURN;
  END IF;

  v_vars := fms.notification_vars(v_ev.aggregate_id);

  -- 沒有 template 鍵 → 找不到要用哪個範本 → 沒有人會收到。
  IF NOT (v_rule.side_effects ? 'template') THEN
    RETURN QUERY SELECT 0, 1, 0;
    RETURN;
  END IF;

  -- 每個 (收件人 × 範本頻道) 一筆。同一個範本碼可以有多個頻道
  -- （EMAIL 給外部、IN_APP 給站內），而收件人不必選 —— 兩者都建。
  WITH people AS (
    SELECT DISTINCT r.user_id
      FROM fms.notification_recipients(v_ev.aggregate_id,
                                       v_rule.side_effects -> 'notify') r
     WHERE r.user_id IS NOT NULL
  ), tpl AS (
    SELECT nt.channel, nt.code, nt.subject_template, nt.body_template
      FROM fms.notification_templates nt
     WHERE lower(nt.code) = lower(v_rule.side_effects ->> 'template')
       AND nt.is_active
       AND (nt.tenant_id IS NULL OR nt.tenant_id = v_ev.tenant_id)
  ), inserted AS (
    INSERT INTO fms.notifications
      (tenant_id, recipient_user_id, channel, template_code,
       subject, body, entity_type, entity_id, priority, source_event_id)
    SELECT v_ev.tenant_id, p.user_id, t.channel, t.code,
           fms.render_template(t.subject_template, v_vars),
           fms.render_template(t.body_template, v_vars),
           'WORK_ORDER', v_ev.aggregate_id,
           CASE WHEN v_ev.event_type = 'work_order.sla_breached'
                THEN 'HIGH' ELSE 'NORMAL' END,
           p_event_id
      FROM people p CROSS JOIN tpl t
    -- 重放時什麼都不做（見本檔第 0 節）。因此 `created` 在重放時是 0，
    -- 而那是正確的答案：這一輪沒有建立新的通知。
    --
    -- `WHERE source_event_id IS NOT NULL` 必須重述：索引是部分索引，
    -- 而 ON CONFLICT 的推斷要看得到同一個述詞才對得上。少了它，
    -- PostgreSQL 回「there is no unique or exclusion constraint matching
    -- the ON CONFLICT specification」—— 而那個錯誤只會在真的有事件要扇出時
    -- 才出現（空資料庫上的自我驗證看不到）。
    ON CONFLICT (source_event_id, recipient_user_id, channel)
      WHERE source_event_id IS NOT NULL
      DO NOTHING
    RETURNING 1
  )
  SELECT count(*)::int INTO v_created FROM inserted;

  SELECT count(*)::int INTO v_unres
    FROM fms.notification_recipients(v_ev.aggregate_id,
                                     v_rule.side_effects -> 'notify') r
   WHERE r.user_id IS NULL;

  -- 範本碼在 notification_templates 裡不存在 → 沒有人會收到。
  --
  -- **不能用 `v_created = 0` 判斷**：重放時 ON CONFLICT 讓 created 也是 0，
  -- 而那不是缺範本。直接問範本存不存在。
  IF NOT EXISTS (
    SELECT 1 FROM fms.notification_templates nt
     WHERE lower(nt.code) = lower(v_rule.side_effects ->> 'template') AND nt.is_active
  ) THEN
    v_missing := 1;
  END IF;

  RETURN QUERY SELECT v_created, v_missing, v_unres;
END;
$$;

REVOKE ALL ON FUNCTION fms.fan_out_notifications(bigint) FROM PUBLIC, fms_app;
GRANT EXECUTE ON FUNCTION fms.fan_out_notifications(bigint) TO fms_owner;

COMMENT ON FUNCTION fms.fan_out_notifications(bigint) IS
  '一個 outbox 事件 → N 筆 notifications。回 (created, no_template, unresolved)：'
  '後兩者是「宣告了要通知但不會有人收到」的計數，不拋錯但必須看得見。'
  '呼叫者需在平台情境內（notifications 是 FORCE RLS）；刻意不用 DEFINER。';

-- -----------------------------------------------------------------------------
-- 5. 把範本接到規則上
-- -----------------------------------------------------------------------------
-- 009 種的 12 個範本裡，只有三個對得上有 notify 的轉移。其餘十條規則
-- （`service_request.accepted`、`work_order.reassigned`、`rejected`、
--  `approval_requested`、`scheduled`、`submitted`、`waiting_parts`）
-- **沒有對應範本** —— 那是內容工作，不是這個 migration 該憑空生出來的。
-- 它們會被計入 `no_template`，因此看得見。
UPDATE fms.work_order_transitions_allowed
   SET side_effects = side_effects || '{"template": "WO_SLA_BREACH"}'::jsonb
 WHERE action = 'BREACH_SLA' AND is_active;

UPDATE fms.work_order_transitions_allowed
   SET side_effects = side_effects || '{"template": "WO_ASSIGNED"}'::jsonb
 WHERE action IN ('ASSIGN', 'REASSIGN') AND is_active
   AND side_effects ->> 'emit' = 'work_order.assigned';

UPDATE fms.work_order_transitions_allowed
   SET side_effects = side_effects || '{"template": "WO_COMPLETED"}'::jsonb
 WHERE action = 'COMPLETE' AND is_active
   AND side_effects ->> 'emit' = 'work_order.completed';

-- -----------------------------------------------------------------------------
-- 6. SLA 逾期的站內版本
-- -----------------------------------------------------------------------------
-- `WO_SLA_BREACH` 只有 EMAIL 版本，而**這個 migration 不含投遞** ——
-- EMAIL 的列會停在 QUEUED。加一個 IN_APP 版本，讓升級通知在
-- `GET /notifications` 上立刻看得到，不必等 SMTP。
--
-- 文字與 EMAIL 版相同（同一件事的同一句話），因此這不是憑空發明內容。
INSERT INTO fms.notification_templates
  (tenant_id, code, channel, locale, subject_template, body_template, is_active)
SELECT nt.tenant_id, nt.code, 'IN_APP', nt.locale, nt.subject_template, nt.body_template, true
  FROM fms.notification_templates nt
 WHERE nt.code = 'WO_SLA_BREACH' AND nt.channel = 'EMAIL'
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 範本與角色是 009 種的，而本檔在 CORE 裡執行、早於 009（032／036／037／038
-- 都記過）。因此只驗不依賴租戶資料的部分；行為在 `notification_slice.rs`。
DO $$
BEGIN
  -- (1) 渲染：找得到的換掉、找不到的留著。
  --
  -- **三個比較都用 `IS DISTINCT FROM` 而不是 `<>`。** 第一版寫了 `<>`，
  -- 而 `replace(x, y, NULL)` 回 NULL —— `NULL <> '...'` 的結果是 NULL 而不是
  -- TRUE，於是 IF 不會觸發、斷言靜默通過。突變測試（拿掉那個 NULL 保護）
  -- 就是這樣溜過去的：**斷言本身犯了它要抓的那個錯。**
  IF fms.render_template('工單 {{wo_no}} 逾期', '{"wo_no": "WO-1"}'::jsonb)
     IS DISTINCT FROM '工單 WO-1 逾期' THEN
    RAISE EXCEPTION '041 FAILED: 渲染沒有替換';
  END IF;
  IF fms.render_template('工單 {{wo_no}}（{{title}}）', '{"wo_no": "WO-1"}'::jsonb)
     IS DISTINCT FROM '工單 WO-1（{{title}}）' THEN
    RAISE EXCEPTION '041 FAILED: 找不到的 placeholder 應原樣留下 —— '
      '換成空字串會讓資料缺口沒有聲音';
  END IF;

  -- 值是 JSON null 的變數不能把整個範本抹成 NULL。
  --
  -- 這一格守的是渲染迴圈裡那個 `IS NOT NULL` 判斷：`replace(x, y, NULL)`
  -- 在 SQL 裡回 **NULL**，也就是整封通知變成空的。`notification_vars` 有
  -- `jsonb_strip_nulls` 所以正常路徑碰不到，但這個函式是公開的。
  IF fms.render_template('工單 {{wo_no}} 逾期', '{"wo_no": null}'::jsonb)
     IS DISTINCT FROM '工單 {{wo_no}} 逾期' THEN
    RAISE EXCEPTION '041 FAILED: 值為 JSON null 的變數會讓 replace() 回 NULL，'
      '把整個範本抹掉 —— 必須跳過';
  END IF;

  -- (2) 不是 DEFINER、不給 fms_app（033／036 的同一組斷言；DROP+CREATE 會讓
  --     007 的 ALTER DEFAULT PRIVILEGES 把 EXECUTE 自動給回 fms_app）。
  IF (SELECT prosecdef FROM pg_proc
       WHERE pronamespace = 'fms'::regnamespace AND proname = 'fan_out_notifications') THEN
    RAISE EXCEPTION '041 FAILED: fan_out_notifications 不該是 SECURITY DEFINER';
  END IF;
  IF has_function_privilege('fms_app', 'fms.fan_out_notifications(bigint)', 'EXECUTE') THEN
    RAISE EXCEPTION '041 FAILED: fms_app 不該能執行 fan_out_notifications';
  END IF;

  -- (3) 幂等用的唯一索引在，而且是部分索引 —— 不是由事件產生的通知
  --     （日後的公告、手動發送）不該被它約束。
  IF NOT EXISTS (
    SELECT 1 FROM pg_index i
     WHERE i.indexrelid = 'fms.uq_notifications_event_recipient'::regclass
       AND i.indisunique AND i.indpred IS NOT NULL
  ) THEN
    RAISE EXCEPTION '041 FAILED: uq_notifications_event_recipient 必須是部分唯一索引';
  END IF;

  RAISE NOTICE '041 OK: 通知扇出就緒（不含 EMAIL/PUSH 投遞）';
END;
$$;

COMMIT;
