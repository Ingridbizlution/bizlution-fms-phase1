-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 042: 通知範本可由管理者維護（並修掉 041 的覆寫競爭）
-- =============================================================================
-- 041 讓範本有了讀取點，但**只有 migration 能改範本**。而 041 自己就報出了
-- 缺口：十條有 `notify` 的轉移沒有對應範本
-- （`work_order.submitted`／`rejected`／`waiting_parts`／`scheduled`／
--  `reassigned`／`approval_requested`／`service_request.accepted`…）。
--
-- 那十份文案是**內容工作**，不是工程工作。把它們寫進 migration 等於讓
-- 「改一句通知的措辭」變成一次部署 —— 與 037（SLA 政策）、040（假日行事曆）
-- 完全相同的問題。
--
-- -----------------------------------------------------------------------------
-- 041 的覆寫競爭（本檔一併修掉）
-- -----------------------------------------------------------------------------
-- `notification_templates.tenant_id` 可為 NULL，而 007 的 RLS 已經把模型
-- 定好了：
--
--     tenant_read  ： is_platform_context() OR tenant_id IS NULL
--                                          OR tenant_id = current_tenant_id()
--     tenant_write ： is_platform_context() OR tenant_id = current_tenant_id()
--
-- 也就是**租戶讀得到平台範本，但改不了它** —— 客製的方式是建一個同
-- `(code, channel, locale)` 的租戶版本。009 種的 13 個範本全部是平台的。
--
-- 041 的查詢是：
--
--     WHERE lower(code) = ... AND is_active
--       AND (tenant_id IS NULL OR tenant_id = v_ev.tenant_id)
--
-- **沒有優先序。** 一旦租戶建了覆寫版本，平台版與租戶版都會匹配，
-- `CROSS JOIN` 產出兩列，而 `uq_notifications_event_recipient`
-- （source_event_id, recipient, channel）讓其中一個以 `ON CONFLICT DO NOTHING`
-- 被丟掉 —— **哪一個勝出不確定**。
--
-- 這與 037 修掉的 `resolve_sla_policy` 的 `code` 決勝是同一類陷阱：
-- 管理者建了覆寫，而系統有時候用它、有時候不用。
--
-- 修法是 `DISTINCT ON (channel)` 加上「租戶版優先」的排序 ——
-- 與 `resolve_sla_policy`／`business_windows` 同一個模式。
--
-- `locale` 也可能造成同樣的相乘（同 code 同 channel 兩種語系）。目前沒有
-- 使用者語系偏好欄位可以據此選擇，因此以 `locale` 字母序當最後的決勝 ——
-- **不確定的答案換成一個確定但可能不理想的答案**，而後者至少查得出來。
--
-- 依賴：007（RLS）、026（min_scope_level）、041（扇出）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 1. 權限
-- -----------------------------------------------------------------------------
-- `min_scope_level = TENANT`：範本沒有場域維度（表上沒有 facility_id），
-- 而一句措辭的改動會套用到整個租戶收到的每一封通知。
--
-- 這與 037（`sla_policy:write` 是 FACILITY）不同，理由也不同：那裡刻意讓
-- 場域級可寫，因為解析順序讓場域專屬的政策優先。這裡沒有那個維度。
INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous)
VALUES
  ('notification_template:read',  'notification_template', 'read',  'CORE',
   '查詢通知範本', 'FACILITY', false),
  ('notification_template:write', 'notification_template', 'write', 'CORE',
   '維護通知範本（措辭會套用到整個租戶的每一封通知）', 'TENANT', true)
ON CONFLICT (code) DO UPDATE
  SET resource = EXCLUDED.resource,
      action = EXCLUDED.action,
      module = EXCLUDED.module,
      description = EXCLUDED.description,
      min_scope_level = EXCLUDED.min_scope_level,
      is_dangerous = EXCLUDED.is_dangerous;

-- 自己補列（027 檔頭：008 的萬用 INSERT 不會因為新增權限碼而重跑）。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code
FROM fms.roles r,
     (VALUES ('notification_template:read'), ('notification_template:write')) AS c(code)
WHERE r.code IN ('PLATFORM_ADMIN', 'TENANT_ADMIN')
ON CONFLICT DO NOTHING;

-- 讀取放寬：想知道「使用者會收到什麼字」的人不只租戶管理員。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'notification_template:read'
FROM fms.roles r
WHERE r.code IN ('FACILITY_ADMIN', 'ORG_MANAGER', 'MAINTENANCE_SUPERVISOR')
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 2. 範本裡用到哪些變數
-- -----------------------------------------------------------------------------
-- 打錯一個 placeholder 的後果是**收件人看到 `{{assignee}}` 這串字**
-- —— `render_template` 刻意原樣留下找不到的變數（041 檔頭說明了理由）。
--
-- 因此讓 API 回報「這個範本用到哪些變數」，客戶端就能把它與可用清單
-- 對照。這比在寫入時拒絕好：不同事件家族的可用變數不同，而目前只有
-- 工單家族有產生器（`notification_vars`）。
CREATE OR REPLACE FUNCTION fms.template_placeholders(p_template text)
RETURNS text[]
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT coalesce(array_agg(DISTINCT m[1] ORDER BY m[1]), '{}')
    FROM regexp_matches(coalesce(p_template, ''), '\{\{([a-zA-Z0-9_]+)\}\}', 'g') m;
$$;

COMMENT ON FUNCTION fms.template_placeholders(text) IS
  '範本裡用到的 {{變數}} 名稱。給 API 回報用 —— 打錯的變數會原樣出現在'
  '收件人眼前（render_template 刻意不吞掉它），因此要讓客戶端看得到。';

-- -----------------------------------------------------------------------------
-- 3. 修掉覆寫競爭
-- -----------------------------------------------------------------------------
-- 只改 041 的 `tpl` CTE，其餘逐字相同。
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
       subject, body, entity_type, entity_id, priority, source_event_id)
    SELECT v_ev.tenant_id, p.user_id, t.channel, t.code,
           fms.render_template(t.subject_template, v_vars),
           fms.render_template(t.body_template, v_vars),
           'WORK_ORDER', v_ev.aggregate_id,
           CASE WHEN v_ev.event_type = 'work_order.sla_breached'
                THEN 'HIGH' ELSE 'NORMAL' END,
           p_event_id
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
-- 範本與角色是 009 種的，而本檔在 CORE 裡執行、早於 009。
-- 覆寫優先序的行為在 `notification_template_slice.rs`。
DO $$
DECLARE v_n bigint;
BEGIN
  -- (1) 權限的範圍宣告。write 是 TENANT —— 一句措辭套用到整個租戶。
  IF NOT EXISTS (
    SELECT 1 FROM fms.permissions
     WHERE code = 'notification_template:write'
       AND min_scope_level = 'TENANT' AND is_dangerous
  ) THEN
    RAISE EXCEPTION '042 FAILED: notification_template:write 應是 TENANT 範圍且 dangerous';
  END IF;

  SELECT count(DISTINCT r.code) INTO v_n
    FROM fms.roles r
    JOIN fms.role_permissions rp ON rp.role_id = r.id
   WHERE rp.permission_code = 'notification_template:write';
  IF v_n < 2 THEN
    RAISE EXCEPTION '042 FAILED: notification_template:write 只有 % 個角色持有', v_n;
  END IF;

  -- (2) placeholder 抽取。用 IS DISTINCT FROM ——「斷言本身犯了它要抓的錯」
  --     那件事在 041 已經發生過一次（`<>` 遇到 NULL 靜默通過）。
  IF fms.template_placeholders('工單 {{wo_no}}（{{title}}）由 {{wo_no}} 處理')
     IS DISTINCT FROM ARRAY['title', 'wo_no'] THEN
    RAISE EXCEPTION '042 FAILED: placeholder 應去重且排序，實際 %',
      fms.template_placeholders('工單 {{wo_no}}（{{title}}）由 {{wo_no}} 處理');
  END IF;
  IF fms.template_placeholders('沒有變數') IS DISTINCT FROM '{}'::text[] THEN
    RAISE EXCEPTION '042 FAILED: 沒有變數應回空陣列而非 NULL';
  END IF;
  IF fms.template_placeholders(NULL) IS DISTINCT FROM '{}'::text[] THEN
    RAISE EXCEPTION '042 FAILED: NULL 範本（subject 可為 NULL）應回空陣列';
  END IF;

  RAISE NOTICE '042 OK: 通知範本權限與覆寫優先序就緒';
END;
$$;

COMMIT;
