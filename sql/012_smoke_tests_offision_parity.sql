-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- 012: Offision parity 煙霧測試（於 001–011 之後執行；009 示範租戶為前提）
--   T5  合併房間雙向互斥（訂 AB 鎖住 A、B；A 被訂則 AB 訂不到）
--   T6  配額扣減、警示與硬上限拒絕
--   T7  訪客邀請 → 報到 → 統計回寫與事件
--   T8  公告投放與已讀／確認追蹤
--   T9  固定座位阻擋他人預約
--   T10 IoT 下行命令的來源可追溯性
-- 全程 ROLLBACK，不留任何資料。
-- =============================================================================

BEGIN;

SELECT set_config('app.is_platform', 'on', true);

-- -----------------------------------------------------------------------------
-- 前置：把 009 的 401／402 會議室組成一間「401+402 大會議室」
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_parent_node uuid;
  v_parent_res  uuid;
BEGIN
  -- 複合資源需要一個自己的空間節點（合併後的大會議室）
  INSERT INTO fms.spatial_nodes (id, tenant_id, facility_id, parent_id, node_type_code,
                                 code, name, floor_level, floor_label, area_sqm, capacity, is_bookable)
  VALUES ('10000000-0000-4000-8000-0000000000c1',
          'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          '10000000-0000-4000-8000-000000000003', 'MEETING_ROOM',
          'R401_402', '401+402 大會議室', 4, '4F', 66, 18, true)
  RETURNING id INTO v_parent_node;

  INSERT INTO fms.bookable_resources (id, tenant_id, facility_id, resource_type, spatial_node_id,
                                      display_name, min_duration_minutes, max_duration_minutes,
                                      slot_granularity_minutes, advance_booking_days, capacity)
  VALUES ('70000000-0000-4000-8000-0000000000c1',
          'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          'SPATIAL_NODE', v_parent_node, '401+402 大會議室（18人）', 30, 300, 30, 60, 1)
  RETURNING id INTO v_parent_res;

  INSERT INTO fms.resource_compositions (tenant_id, facility_id, parent_resource_id, child_resource_id, sort_order)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          v_parent_res, '70000000-0000-4000-8000-000000000001', 1),
         ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          v_parent_res, '70000000-0000-4000-8000-000000000002', 2);

  IF NOT (SELECT is_composite FROM fms.bookable_resources WHERE id = v_parent_res) THEN
    RAISE EXCEPTION 'SETUP FAILED: is_composite 旗標未由觸發器設定';
  END IF;
  RAISE NOTICE 'SETUP OK: 合併房間 401+402 建立完成';
END;
$$;

-- -----------------------------------------------------------------------------
-- T5  合併房間雙向互斥
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_ab_id     uuid;
  v_shadows   integer;
  v_blocked   boolean := false;
  v_avail     record;
BEGIN
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid,
                          'ffffffff-0000-4000-8000-000000000004'::uuid, true);

  -- (1) 預約合併房間 → 應自動產生 2 筆 shadow 列
  INSERT INTO fms.reservations (tenant_id, facility_id, bookable_resource_id, reservation_no,
                               resource_type, resource_id, organizer_id, title,
                               start_at, end_at, status)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          '70000000-0000-4000-8000-0000000000c1', 'TEST-AB-001',
          'SPATIAL_NODE', '10000000-0000-4000-8000-0000000000c1',
          'ffffffff-0000-4000-8000-000000000004', '全體會議（合併 401+402）',
          '2026-09-10 09:00+08', '2026-09-10 11:00+08', 'CONFIRMED')
  RETURNING id INTO v_ab_id;

  SELECT count(*) INTO v_shadows FROM fms.reservations
   WHERE parent_reservation_id = v_ab_id AND is_shadow;
  IF v_shadows <> 2 THEN
    RAISE EXCEPTION 'T5 FAILED: 預期 2 筆 shadow 預約，實得 %', v_shadows;
  END IF;

  -- (2) 此時 401 應該訂不到（被 shadow 佔用）
  BEGIN
    INSERT INTO fms.reservations (tenant_id, facility_id, bookable_resource_id, reservation_no,
                                 resource_type, resource_id, organizer_id, title,
                                 start_at, end_at, status)
    VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
            '70000000-0000-4000-8000-000000000001', 'TEST-A-001',
            'SPATIAL_NODE', '10000000-0000-4000-8000-000000000005',
            'ffffffff-0000-4000-8000-000000000002', '401 單獨會議（應被拒絕）',
            '2026-09-10 10:00+08', '2026-09-10 10:30+08', 'CONFIRMED');
  EXCEPTION WHEN exclusion_violation THEN
    v_blocked := true;
  END;
  IF NOT v_blocked THEN
    RAISE EXCEPTION 'T5 FAILED: 合併房間已預約，401 仍可被單獨預約';
  END IF;

  -- (3) 可用性判定必須與約束一致地回報「合併房間佔用」
  SELECT * INTO v_avail FROM fms.check_resource_availability(
    '10000000-0000-4000-8000-000000000005'::uuid,
    '2026-09-10 10:00+08'::timestamptz, '2026-09-10 10:30+08'::timestamptz);
  IF v_avail.is_available OR v_avail.conflict_type <> 'RESERVATION_CONFLICT' THEN
    RAISE EXCEPTION 'T5 FAILED: 401 的可用性判定未反映合併房間佔用（回報 %）', v_avail.conflict_type;
  END IF;

  -- (4) 取消合併房間 → shadow 必須同步取消，401 隨即可訂
  UPDATE fms.reservations
     SET status = 'CANCELLED', cancelled_at = clock_timestamp(),
         cancellation_reason = '測試取消'
   WHERE id = v_ab_id;

  IF EXISTS (SELECT 1 FROM fms.reservations
              WHERE parent_reservation_id = v_ab_id AND status <> 'CANCELLED') THEN
    RAISE EXCEPTION 'T5 FAILED: 父預約取消後 shadow 未同步取消';
  END IF;

  INSERT INTO fms.reservations (tenant_id, facility_id, bookable_resource_id, reservation_no,
                               resource_type, resource_id, organizer_id, title,
                               start_at, end_at, status)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          '70000000-0000-4000-8000-000000000001', 'TEST-A-002',
          'SPATIAL_NODE', '10000000-0000-4000-8000-000000000005',
          'ffffffff-0000-4000-8000-000000000002', '401 單獨會議（取消後應可訂）',
          '2026-09-10 10:00+08', '2026-09-10 10:30+08', 'CONFIRMED');

  -- (5) 反向：401 已被訂，合併房間應訂不到
  v_blocked := false;
  BEGIN
    INSERT INTO fms.reservations (tenant_id, facility_id, bookable_resource_id, reservation_no,
                                 resource_type, resource_id, organizer_id, title,
                                 start_at, end_at, status)
    VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
            '70000000-0000-4000-8000-0000000000c1', 'TEST-AB-002',
            'SPATIAL_NODE', '10000000-0000-4000-8000-0000000000c1',
            'ffffffff-0000-4000-8000-000000000004', '合併會議（應被拒絕）',
            '2026-09-10 10:15+08', '2026-09-10 11:00+08', 'CONFIRMED');
  EXCEPTION WHEN exclusion_violation THEN
    v_blocked := true;
  END;
  IF NOT v_blocked THEN
    RAISE EXCEPTION 'T5 FAILED: 子房間已預約，合併房間仍可被預約';
  END IF;

  RAISE NOTICE 'T5 PASSED: 合併房間雙向互斥、shadow 展開與同步取消皆正確';
END;
$$;

-- -----------------------------------------------------------------------------
-- T6  配額
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_policy uuid;
  v_left   numeric;
  v_denied boolean := false;
  v_tx     integer;
BEGIN
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid,
                          'ffffffff-0000-4000-8000-000000000002'::uuid, true);

  INSERT INTO fms.quota_policies (tenant_id, facility_id, code, name, applies_to, applies_to_value,
                                  metric, period, allowance, is_hard_limit, warn_at_pct)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          'TEST_ROOM_HOURS', '測試：每月會議室時數', 'NODE_TYPE', 'MEETING_ROOM',
          'BOOKING_HOURS', 'MONTH', 10, true, 80)
  RETURNING id INTO v_policy;

  INSERT INTO fms.quota_assignments (tenant_id, quota_policy_id, subject_type)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', v_policy, 'ALL_USERS');

  -- 額度解析
  IF fms.resolve_quota_allowance(v_policy, 'ffffffff-0000-4000-8000-000000000004'::uuid) <> 10 THEN
    RAISE EXCEPTION 'T6 FAILED: 額度解析錯誤';
  END IF;

  -- 扣 6 小時 → 餘 4
  v_left := fms.consume_quota(v_policy, 'ffffffff-0000-4000-8000-000000000004'::uuid, 6,
                              NULL, '2026-09-10 09:00+08');
  IF v_left <> 4 THEN
    RAISE EXCEPTION 'T6 FAILED: 扣 6 小時後餘額應為 4，實得 %', v_left;
  END IF;

  -- 再扣 2.5 → 餘 1.5，並應觸發 80% 警示
  v_left := fms.consume_quota(v_policy, 'ffffffff-0000-4000-8000-000000000004'::uuid, 2.5,
                              NULL, '2026-09-15 09:00+08');
  IF v_left <> 1.5 THEN
    RAISE EXCEPTION 'T6 FAILED: 餘額應為 1.5，實得 %', v_left;
  END IF;
  IF (SELECT warned_at FROM fms.quota_usage
       WHERE quota_policy_id = v_policy
         AND user_id = 'ffffffff-0000-4000-8000-000000000004'
         AND period_key = '2026-09') IS NULL THEN
    RAISE EXCEPTION 'T6 FAILED: 超過 80%% 未寫入 warned_at';
  END IF;

  -- 超額 → 硬上限應拒絕（並回滾該次扣減）
  BEGIN
    PERFORM fms.consume_quota(v_policy, 'ffffffff-0000-4000-8000-000000000004'::uuid, 5,
                              NULL, '2026-09-20 09:00+08');
  EXCEPTION WHEN check_violation THEN
    v_denied := true;
  END;
  IF NOT v_denied THEN
    RAISE EXCEPTION 'T6 FAILED: 超過硬上限仍被允許';
  END IF;

  -- 不同月份為獨立週期
  v_left := fms.consume_quota(v_policy, 'ffffffff-0000-4000-8000-000000000004'::uuid, 9,
                              NULL, '2026-10-01 09:00+08');
  IF v_left <> 1 THEN
    RAISE EXCEPTION 'T6 FAILED: 新週期額度未重置（餘額 %）', v_left;
  END IF;

  -- 退還
  v_left := fms.consume_quota(v_policy, 'ffffffff-0000-4000-8000-000000000004'::uuid, -3,
                              NULL, '2026-10-05 09:00+08', 'REFUND');
  IF v_left <> 4 THEN
    RAISE EXCEPTION 'T6 FAILED: 退還 3 後餘額應為 4，實得 %', v_left;
  END IF;

  -- 明細帳完整（4 筆成功交易：consume×3 + refund；被拒絕的那筆不應留下帳）
  SELECT count(*) INTO v_tx FROM fms.quota_transactions
   WHERE quota_policy_id = v_policy;
  IF v_tx <> 4 THEN
    RAISE EXCEPTION 'T6 FAILED: 明細帳筆數應為 4，實得 %', v_tx;
  END IF;

  RAISE NOTICE 'T6 PASSED: 配額扣減、警示、硬上限、週期重置與退還皆正確';
END;
$$;

-- -----------------------------------------------------------------------------
-- T7  訪客
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_visitor uuid;
  v_inv     uuid;
  v_before  integer;
BEGIN
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid,
                          'ffffffff-0000-4000-8000-000000000002'::uuid, true);

  UPDATE fms.facilities SET visitor_management_enabled = true
   WHERE id = 'cccccccc-0000-4000-8000-000000000001';

  INSERT INTO fms.visitors (tenant_id, full_name, email, company)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', '張訪客', 'guest@example.com', '外部顧問公司')
  RETURNING id INTO v_visitor;

  SELECT visit_count INTO v_before FROM fms.visitors WHERE id = v_visitor;

  INSERT INTO fms.visitor_invitations (
    tenant_id, facility_id, invitation_no, visitor_id, visitor_name, visitor_email,
    host_user_id, spatial_node_id, visit_type, purpose,
    expected_arrival_at, expected_departure_at, invitation_code, allowed_node_ids, nda_required)
  VALUES (
    'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
    fms.next_document_no('aaaaaaaa-0000-4000-8000-000000000001', 'VISIT', 'VS'),
    v_visitor, '張訪客', 'guest@example.com',
    'ffffffff-0000-4000-8000-000000000002', '10000000-0000-4000-8000-000000000005',
    'MEETING', '季度檢討會議',
    '2026-09-11 13:30+08', '2026-09-11 16:00+08',
    'INV-TEST-0001', ARRAY['10000000-0000-4000-8000-000000000005'::uuid], true)
  RETURNING id INTO v_inv;

  -- 門禁授權
  INSERT INTO fms.visitor_access_grants (tenant_id, invitation_id, spatial_node_id,
                                         credential_type, valid_from, valid_until)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', v_inv,
          '10000000-0000-4000-8000-000000000005', 'QR',
          '2026-09-11 13:00+08', '2026-09-11 16:30+08');

  -- 報到
  UPDATE fms.visitor_invitations
     SET status = 'CHECKED_IN', checked_in_at = clock_timestamp(),
         check_in_method = 'QR', badge_no = 'B-0001',
         nda_signed_at = clock_timestamp()
   WHERE id = v_inv;

  IF (SELECT visit_count FROM fms.visitors WHERE id = v_visitor) <> v_before + 1 THEN
    RAISE EXCEPTION 'T7 FAILED: 報到後 visitor.visit_count 未累加';
  END IF;

  IF NOT EXISTS (SELECT 1 FROM fms.event_outbox
                  WHERE aggregate_id = v_inv AND event_type = 'visitor.checked_in') THEN
    RAISE EXCEPTION 'T7 FAILED: 未發出 visitor.checked_in 事件';
  END IF;

  -- 到訪時間窗約束
  BEGIN
    INSERT INTO fms.visitor_invitations (
      tenant_id, facility_id, invitation_no, visitor_name, host_user_id,
      expected_arrival_at, expected_departure_at, invitation_code)
    VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
            'VS-BAD-0001', '時間錯誤訪客', 'ffffffff-0000-4000-8000-000000000002',
            '2026-09-11 16:00+08', '2026-09-11 13:00+08', 'INV-TEST-BAD');
    RAISE EXCEPTION 'T7 FAILED: 離場早於到訪的邀請未被拒絕';
  EXCEPTION WHEN check_violation THEN
    NULL;
  END;

  RAISE NOTICE 'T7 PASSED: 訪客邀請、門禁授權、報到統計與事件皆正確';
END;
$$;

-- -----------------------------------------------------------------------------
-- T8  公告
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_ann uuid;
BEGIN
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid,
                          'ffffffff-0000-4000-8000-000000000002'::uuid, true);

  INSERT INTO fms.announcements (tenant_id, facility_id, title, body, summary, category, severity,
                                 channels, show_as_banner, requires_acknowledgement,
                                 publish_at, expire_at, status, created_by)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          '4F 空調保養通知', '9/12（六）09:00-12:00 進行 4F 空調季保養，期間 401、402 會議室暫停使用。',
          '9/12 上午 4F 空調保養，會議室暫停使用', 'MAINTENANCE', 'NOTICE',
          ARRAY['IN_APP','EMAIL','SIGNAGE'], true, true,
          clock_timestamp(), clock_timestamp() + interval '7 days', 'PUBLISHED',
          'ffffffff-0000-4000-8000-000000000002')
  RETURNING id INTO v_ann;

  INSERT INTO fms.announcement_reads (announcement_id, user_id, tenant_id, acknowledged_at)
  VALUES (v_ann, 'ffffffff-0000-4000-8000-000000000004',
          'aaaaaaaa-0000-4000-8000-000000000001', clock_timestamp());

  IF NOT EXISTS (SELECT 1 FROM fms.announcement_reads
                  WHERE announcement_id = v_ann AND acknowledged_at IS NOT NULL) THEN
    RAISE EXCEPTION 'T8 FAILED: 已讀／確認未寫入';
  END IF;

  -- 同一人重複標記已讀不應產生第二列
  BEGIN
    INSERT INTO fms.announcement_reads (announcement_id, user_id, tenant_id)
    VALUES (v_ann, 'ffffffff-0000-4000-8000-000000000004', 'aaaaaaaa-0000-4000-8000-000000000001');
    RAISE EXCEPTION 'T8 FAILED: 重複已讀未被主鍵擋下';
  EXCEPTION WHEN unique_violation THEN
    NULL;
  END;

  -- 到期時間必須晚於發布時間
  BEGIN
    INSERT INTO fms.announcements (tenant_id, title, body, publish_at, expire_at)
    VALUES ('aaaaaaaa-0000-4000-8000-000000000001', '時間錯誤公告', 'x',
            clock_timestamp(), clock_timestamp() - interval '1 day');
    RAISE EXCEPTION 'T8 FAILED: 到期早於發布的公告未被拒絕';
  EXCEPTION WHEN check_violation THEN
    NULL;
  END;

  RAISE NOTICE 'T8 PASSED: 公告投放、已讀確認與時間窗約束皆正確';
END;
$$;

-- -----------------------------------------------------------------------------
-- T9  固定座位
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_avail  record;
  v_dup    boolean := false;
  -- 查詢時點必須落在該工位的 advance_booking_days 窗口內（009 為 HD-401 設 30 天），
  -- 否則 check_resource_availability 會先回 OUTSIDE_BOOKING_WINDOW 而根本走不到
  -- DESK_ASSIGNED 判定。原本寫死 '2026-09-15' 會隨執行日期而失效，故改為相對日期。
  v_probe_start timestamptz := date_trunc('day', clock_timestamp()) + interval '10 days 9 hours';
  v_probe_end   timestamptz := date_trunc('day', clock_timestamp()) + interval '10 days 18 hours';
BEGIN
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid,
                          'ffffffff-0000-4000-8000-000000000002'::uuid, true);

  INSERT INTO fms.desk_assignments (tenant_id, facility_id, spatial_node_id, user_id,
                                    assignment_type, valid_from, valid_until)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          '10000000-0000-4000-8000-000000000008', 'ffffffff-0000-4000-8000-000000000004',
          'PERMANENT', (clock_timestamp() - interval '30 days')::date,
                       (clock_timestamp() + interval '120 days')::date);

  -- 他人查詢該座位 → 應顯示已被固定指派
  SELECT * INTO v_avail FROM fms.check_resource_availability(
    '10000000-0000-4000-8000-000000000008'::uuid,
    v_probe_start, v_probe_end,
    NULL, 'ffffffff-0000-4000-8000-000000000003'::uuid);
  IF v_avail.is_available OR v_avail.conflict_type <> 'DESK_ASSIGNED' THEN
    RAISE EXCEPTION 'T9 FAILED: 固定座位未阻擋他人（回報 %）', v_avail.conflict_type;
  END IF;

  -- 指派本人查詢 → 應可用
  SELECT * INTO v_avail FROM fms.check_resource_availability(
    '10000000-0000-4000-8000-000000000008'::uuid,
    v_probe_start, v_probe_end,
    NULL, 'ffffffff-0000-4000-8000-000000000004'::uuid);
  IF NOT v_avail.is_available THEN
    RAISE EXCEPTION 'T9 FAILED: 固定座位對指派本人不可用（回報 %）', v_avail.conflict_type;
  END IF;

  -- 同座位同期間的第二個 PERMANENT 指派必須被排他約束擋下
  BEGIN
    INSERT INTO fms.desk_assignments (tenant_id, facility_id, spatial_node_id, user_id,
                                      assignment_type, valid_from, valid_until)
    VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
            '10000000-0000-4000-8000-000000000008', 'ffffffff-0000-4000-8000-000000000003',
            'PERMANENT', (clock_timestamp() + interval '60 days')::date,
                         (clock_timestamp() + interval '180 days')::date);
  EXCEPTION WHEN exclusion_violation THEN
    v_dup := true;
  END;
  IF NOT v_dup THEN
    RAISE EXCEPTION 'T9 FAILED: 同座位重疊期間的固定指派未被拒絕';
  END IF;

  RAISE NOTICE 'T9 PASSED: 固定座位阻擋他人、對本人開放、重複指派被拒絕';
END;
$$;

-- -----------------------------------------------------------------------------
-- T10  IoT 下行命令可追溯性
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_res_id uuid;
  v_cmd    bigint;
  v_row    fms.device_commands;
BEGIN
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid,
                          'ffffffff-0000-4000-8000-000000000004'::uuid, true);

  INSERT INTO fms.reservations (tenant_id, facility_id, bookable_resource_id, reservation_no,
                               resource_type, resource_id, organizer_id, title,
                               start_at, end_at, status, requires_check_in)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          '70000000-0000-4000-8000-000000000002', 'TEST-DOOR-001',
          'SPATIAL_NODE', '10000000-0000-4000-8000-000000000006',
          'ffffffff-0000-4000-8000-000000000004', '402 會議（門禁測試）',
          '2026-09-12 14:00+08', '2026-09-12 15:00+08', 'CONFIRMED', true)
  RETURNING id INTO v_res_id;

  INSERT INTO fms.device_commands (tenant_id, facility_id, device_id, command, payload,
                                   source, reservation_id, requested_by)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          'a2000000-0000-4000-8000-000000000001', 'UNLOCK',
          '{"duration_seconds":8}'::jsonb, 'RESERVATION', v_res_id,
          'ffffffff-0000-4000-8000-000000000004')
  RETURNING id INTO v_cmd;

  SELECT * INTO v_row FROM fms.device_commands WHERE id = v_cmd;

  IF v_row.status <> 'PENDING' OR v_row.reservation_id IS NULL OR v_row.requested_by IS NULL THEN
    RAISE EXCEPTION 'T10 FAILED: 開門命令缺少來源可追溯資訊';
  END IF;
  IF v_row.expires_at <= v_row.created_at THEN
    RAISE EXCEPTION 'T10 FAILED: 命令未設定有效期限';
  END IF;

  -- 不合法的命令字必須被 CHECK 擋下
  BEGIN
    INSERT INTO fms.device_commands (tenant_id, facility_id, device_id, command)
    VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
            'a2000000-0000-4000-8000-000000000001', 'DROP_TABLE');
    RAISE EXCEPTION 'T10 FAILED: 未定義的命令字未被拒絕';
  EXCEPTION WHEN check_violation THEN
    NULL;
  END;

  RAISE NOTICE 'T10 PASSED: 下行命令具來源追溯、有效期限與命令字白名單';
END;
$$;

ROLLBACK;

BEGIN;

-- -----------------------------------------------------------------------------
-- T12  RBAC：user_permission_codes 與 user_has_permission 必須完全等價
-- -----------------------------------------------------------------------------
-- 016 把 user_has_permission 改成以集合版實作，讓 scope 判定
-- （TENANT／FACILITY／ORG ltree）只有一份。這個測試是那次重構的安全網：
-- 它自己持有 002 原始判定式的**參考複本**，逐一比對整個
-- (使用者 × 權限 × 場域) 交叉乘積。任一格不一致就代表重構改了行為。
--
-- 這個測試留在煙霧測試裡而不是只放在 migration 內，是因為它要在**每次 CI**
-- 都跑：日後若有人「順手優化」其中一支函式，這裡會立刻紅。
DO $$
DECLARE
  v_mismatch bigint;
  v_pairs    bigint;
  v_set_bad  bigint;
BEGIN
  PERFORM set_config('app.is_platform', 'on', true);

  SELECT count(*), count(*) FILTER (WHERE actual IS DISTINCT FROM reference)
    INTO v_pairs, v_mismatch
  FROM (
    SELECT fms.user_has_permission(u.id, p.code, f.id) AS actual,
           EXISTS (
             -- 002 的原始判定式，原樣抄錄作為參考實作
             SELECT 1
             FROM fms.v_user_effective_permissions ep
             LEFT JOIN fms.facilities f2 ON f2.id = f.id
             LEFT JOIN fms.organizations o_target ON o_target.id = f2.org_id
             LEFT JOIN fms.organizations o_scope  ON o_scope.id = ep.scope_id
             WHERE ep.user_id = u.id
               AND ep.permission_code = p.code
               AND (
                     ep.scope_type = 'TENANT'
                 OR (ep.scope_type = 'FACILITY' AND ep.scope_id = f.id)
                 OR (ep.scope_type = 'ORG'
                     AND o_scope.org_path IS NOT NULL
                     AND o_target.org_path IS NOT NULL
                     AND o_target.org_path OPERATOR(public.<@) o_scope.org_path)
               )
           ) AS reference
    FROM fms.users u
    CROSS JOIN fms.permissions p
    CROSS JOIN fms.facilities f
    WHERE u.deleted_at IS NULL AND f.deleted_at IS NULL
  ) q;

  IF v_pairs = 0 THEN
    RAISE EXCEPTION 'T12 FAILED: 交叉乘積為空，測試沒有實際比對任何東西';
  END IF;
  IF v_mismatch > 0 THEN
    RAISE EXCEPTION 'T12 FAILED: % / % 組判定與參考實作不一致', v_mismatch, v_pairs;
  END IF;

  -- 集合版必須恰好等於「所有回 true 的權限碼」，不多也不少。
  -- 只比對「集合 ⊆ true」會漏掉集合缺項；因此用對稱差。
  SELECT count(*) INTO v_set_bad
  FROM fms.users u
  CROSS JOIN fms.facilities f
  CROSS JOIN LATERAL (
    SELECT
      (SELECT array_agg(c ORDER BY c)
         FROM fms.user_permission_codes(u.id, f.id) AS c) AS from_set,
      (SELECT array_agg(p.code ORDER BY p.code)
         FROM fms.permissions p
        WHERE fms.user_has_permission(u.id, p.code, f.id)) AS from_scalar
  ) s
  WHERE u.deleted_at IS NULL AND f.deleted_at IS NULL
    AND s.from_set IS DISTINCT FROM s.from_scalar;

  IF v_set_bad > 0 THEN
    RAISE EXCEPTION 'T12 FAILED: % 組 (使用者, 場域) 的集合版與單一版結果不同', v_set_bad;
  END IF;

  RAISE NOTICE 'T12 PASSED: RBAC 判定等價（比對 % 組），集合版與單一版一致', v_pairs;
END;
$$;

ROLLBACK;
