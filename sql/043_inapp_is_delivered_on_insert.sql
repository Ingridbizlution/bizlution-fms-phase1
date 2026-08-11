-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 043: 讓 QUEUED 只代表「在等傳輸層」
-- =============================================================================
-- 041 把所有通知列都以預設的 `status = 'QUEUED'` 插入，包含 `IN_APP` 的。
-- 但 `IN_APP` 的列**就是**通知 —— 它一寫進來就已經送達了
-- （`GET /notifications` 讀得到，而收件匣用 `read_at IS NULL` 判斷未讀，
--  所以功能上沒有壞）。
--
-- 問題在**監控**：041／042 的檔頭都寫了「EMAIL/PUSH 的列會停在 QUEUED，
-- 監控方式是查 status='QUEUED' 且超過門檻的筆數」。而 IN_APP 也在裡面，
-- 於是那個查詢從第一天就回一個持續成長的數字 —— 一個永遠在響的警報
-- 等於沒有警報。
--
-- 043 讓 `IN_APP` 在插入時就是 `SENT`，於是：
--
--     QUEUED  ＝ 有東西在等傳輸層
--     SENT    ＝ 已送達（IN_APP 立即、EMAIL 由 dispatcher）
--     READ    ＝ 收件人讀過（只有 IN_APP 會到這個狀態）
--
-- `idx_notifications_queue` 是 `WHERE status IN ('QUEUED','FAILED')` 的部分
-- 索引 —— schema 從一開始就是這樣設計的（第十四個「宣告了沒人讀」：
-- 那個索引在 dispatcher 出現前沒有任何查詢用得到它）。
--
-- 依賴：041（扇出）、042（覆寫優先序）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 回填
-- -----------------------------------------------------------------------------
-- 已經建立的 IN_APP 列現在是 QUEUED，而它們早就送達了。
-- `sent_at` 用 `created_at`：那才是它真的可讀的時刻。
--
-- **不碰 `READ` 的列**：那些已經被讀過，狀態比 SENT 更晚。
UPDATE fms.notifications
   SET status = 'SENT',
       sent_at = coalesce(sent_at, created_at)
 WHERE channel = 'IN_APP'
   AND status = 'QUEUED';

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

  IF v_ev.aggregate_type <> 'WORK_ORDER' THEN
    RETURN QUERY SELECT 0, 0, 0;
    RETURN;
  END IF;

  SELECT side_effects INTO v_rule
    FROM fms.work_order_transitions_allowed t
   WHERE t.is_active
     AND t.from_status = v_ev.payload ->> 'from'
     AND t.action = v_ev.payload ->> 'action'
     AND (t.tenant_id IS NULL OR t.tenant_id = v_ev.tenant_id)
   ORDER BY t.tenant_id NULLS LAST
   LIMIT 1;

  IF v_rule.side_effects IS NULL OR NOT (v_rule.side_effects ? 'notify') THEN
    RETURN QUERY SELECT 0, 0, 0;
    RETURN;
  END IF;

  v_vars := fms.notification_vars(v_ev.aggregate_id);

  IF NOT (v_rule.side_effects ? 'template') THEN
    RETURN QUERY SELECT 0, 1, 0;
    RETURN;
  END IF;

  WITH people AS (
    SELECT DISTINCT r.user_id
      FROM fms.notification_recipients(v_ev.aggregate_id,
                                       v_rule.side_effects -> 'notify') r
     WHERE r.user_id IS NOT NULL
  ), tpl AS (
    -- **每個頻道只取一個範本。** 041 沒有這個限制，於是租戶建了覆寫版本之後
    -- 平台版與租戶版都會匹配，而唯一索引讓其中一個任意勝出（見本檔檔頭）。
    --
    -- 優先序：租戶版 > 平台版；同層再以 locale 字母序決勝
    -- （目前沒有使用者語系偏好可以據此選擇 —— 不確定的答案換成確定的）。
    SELECT DISTINCT ON (nt.channel)
           nt.channel, nt.code, nt.subject_template, nt.body_template
      FROM fms.notification_templates nt
     WHERE lower(nt.code) = lower(v_rule.side_effects ->> 'template')
       AND nt.is_active
       AND (nt.tenant_id IS NULL OR nt.tenant_id = v_ev.tenant_id)
     ORDER BY nt.channel, (nt.tenant_id IS NOT NULL) DESC, nt.locale
  ), inserted AS (
    INSERT INTO fms.notifications
      (tenant_id, recipient_user_id, channel, template_code,
       subject, body, entity_type, entity_id, priority, source_event_id,
       status, sent_at)
    SELECT v_ev.tenant_id, p.user_id, t.channel, t.code,
           fms.render_template(t.subject_template, v_vars),
           fms.render_template(t.body_template, v_vars),
           'WORK_ORDER', v_ev.aggregate_id,
           CASE WHEN v_ev.event_type = 'work_order.sla_breached'
                THEN 'HIGH' ELSE 'NORMAL' END,
           p_event_id,
           -- IN_APP 的列**就是**通知：它一寫進來就已經送達了
           -- （`GET /notifications` 讀得到）。因此直接標 SENT，
           -- 讓 QUEUED 只代表「在等傳輸層」。
           CASE WHEN t.channel = 'IN_APP' THEN 'SENT' ELSE 'QUEUED' END,
           CASE WHEN t.channel = 'IN_APP' THEN clock_timestamp() END
      FROM people p CROSS JOIN tpl t
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

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 扇出的行為在 `notification_slice.rs`（本檔在 CORE 裡、早於 009）。
DO $$
DECLARE v_n bigint;
BEGIN
  -- 回填之後不該再有 QUEUED 的 IN_APP 列。這一格擋的是「回填漏了某些狀態」
  -- —— 而漏掉的症狀是那個監控查詢繼續回不斷成長的數字。
  SELECT count(*) INTO v_n
    FROM fms.notifications WHERE channel = 'IN_APP' AND status = 'QUEUED';
  IF v_n > 0 THEN
    RAISE EXCEPTION '043 FAILED: 仍有 % 筆 IN_APP 通知停在 QUEUED', v_n;
  END IF;

  -- 撈取用的部分索引還在。dispatcher 的 claim 查詢完全依賴它，
  -- 而少了它的症狀是「能跑，但每輪全表掃描」。
  IF NOT EXISTS (
    SELECT 1 FROM pg_index i
     WHERE i.indexrelid = 'fms.idx_notifications_queue'::regclass
       AND i.indpred IS NOT NULL
  ) THEN
    RAISE EXCEPTION '043 FAILED: 缺少 idx_notifications_queue 部分索引';
  END IF;

  RAISE NOTICE '043 OK: QUEUED 現在只代表在等傳輸層';
END;
$$;

COMMIT;
