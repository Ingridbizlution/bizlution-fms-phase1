-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- 010: Smoke tests (run after 001–009 on a dev database)
--   Verifies the four invariants Phase 1 must never lose:
--     T1  tenant isolation via RLS
--     T2  no double booking (exclusion constraint)
--     T3  illegal work order transitions are rejected
--     T4  IoT alarm automatically opens and links a work order
--   Every assertion raises an exception on failure, so a clean run = all green.
--   The whole script rolls back at the end: it leaves no data behind.
-- =============================================================================

BEGIN;

SELECT set_config('app.is_platform', 'on', true);

-- A second tenant to prove isolation against the demo tenant from 009.
INSERT INTO fms.tenants (id, code, name, industry)
VALUES ('aaaaaaaa-0000-4000-8000-0000000000ff', 'TEST_OTHER', '測試租戶 B', 'GENERIC')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.organizations (id, tenant_id, code, name, org_type)
VALUES ('bbbbbbbb-0000-4000-8000-0000000000ff', 'aaaaaaaa-0000-4000-8000-0000000000ff',
        'ORG_B', '租戶 B 總部', 'GROUP')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.facilities (id, tenant_id, org_id, code, name)
VALUES ('cccccccc-0000-4000-8000-0000000000ff', 'aaaaaaaa-0000-4000-8000-0000000000ff',
        'bbbbbbbb-0000-4000-8000-0000000000ff', 'FAC_B', '租戶 B 大樓')
ON CONFLICT (id) DO NOTHING;

-- -----------------------------------------------------------------------------
-- T1  Tenant isolation
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_count integer;
  v_super boolean := coalesce((SELECT rolsuper FROM pg_roles WHERE rolname = current_user), false);
BEGIN
  -- Act as tenant A with platform context OFF. FORCE ROW LEVEL SECURITY means the
  -- policies apply even to the table owner, so this assertion is meaningful for
  -- fms_owner and fms_app alike. Only a superuser (BYPASSRLS) is exempt.
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid,
                          'ffffffff-0000-4000-8000-000000000001'::uuid, false);

  SELECT count(*) INTO v_count FROM fms.facilities
   WHERE id = 'cccccccc-0000-4000-8000-0000000000ff';

  IF v_count <> 0 AND NOT v_super THEN
    RAISE EXCEPTION 'T1 FAILED: tenant A can read tenant B facility (RLS not effective for role %)', current_user;
  END IF;

  IF v_super THEN
    RAISE WARNING 'T1 SKIPPED: current role % is a superuser and bypasses RLS. Re-run as fms_owner or fms_app.', current_user;
  END IF;

  SELECT count(*) INTO v_count FROM fms.facilities
   WHERE tenant_id = 'aaaaaaaa-0000-4000-8000-000000000001';
  IF v_count < 2 THEN
    RAISE EXCEPTION 'T1 FAILED: tenant A cannot see its own 2 facilities (saw %)', v_count;
  END IF;

  RAISE NOTICE 'T1 PASSED: tenant isolation behaves as configured';
END;
$$;

-- -----------------------------------------------------------------------------
-- T2  No double booking
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_conflicted boolean := false;
  v_avail      record;
BEGIN
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid,
                          'ffffffff-0000-4000-8000-000000000004'::uuid, true);

  INSERT INTO fms.reservations (tenant_id, facility_id, bookable_resource_id, reservation_no,
                               resource_type, resource_id, organizer_id, title, start_at, end_at, status)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          '70000000-0000-4000-8000-000000000001', 'TEST-RSV-001',
          'SPATIAL_NODE', '10000000-0000-4000-8000-000000000005',
          'ffffffff-0000-4000-8000-000000000004', '測試會議 A',
          '2026-09-01 10:00+08', '2026-09-01 11:00+08', 'CONFIRMED');

  -- Overlapping booking on the same room must be rejected by the database.
  BEGIN
    INSERT INTO fms.reservations (tenant_id, facility_id, bookable_resource_id, reservation_no,
                                 resource_type, resource_id, organizer_id, title, start_at, end_at, status)
    VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
            '70000000-0000-4000-8000-000000000001', 'TEST-RSV-002',
            'SPATIAL_NODE', '10000000-0000-4000-8000-000000000005',
            'ffffffff-0000-4000-8000-000000000002', '測試會議 B（應被拒絕）',
            '2026-09-01 10:30+08', '2026-09-01 11:30+08', 'CONFIRMED');
  EXCEPTION WHEN exclusion_violation THEN
    v_conflicted := true;
  END;

  IF NOT v_conflicted THEN
    RAISE EXCEPTION 'T2 FAILED: overlapping reservation was accepted';
  END IF;

  -- Adjacent (non-overlapping, half-open range) booking must succeed.
  INSERT INTO fms.reservations (tenant_id, facility_id, bookable_resource_id, reservation_no,
                               resource_type, resource_id, organizer_id, title, start_at, end_at, status)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          '70000000-0000-4000-8000-000000000001', 'TEST-RSV-003',
          'SPATIAL_NODE', '10000000-0000-4000-8000-000000000005',
          'ffffffff-0000-4000-8000-000000000002', '測試會議 C（相鄰時段）',
          '2026-09-01 11:00+08', '2026-09-01 12:00+08', 'CONFIRMED');

  -- The availability helper must agree with the constraint.
  SELECT * INTO v_avail FROM fms.check_resource_availability(
    '10000000-0000-4000-8000-000000000005'::uuid,
    '2026-09-01 10:15+08'::timestamptz, '2026-09-01 10:45+08'::timestamptz);

  IF v_avail.is_available THEN
    RAISE EXCEPTION 'T2 FAILED: check_resource_availability reported a busy slot as free';
  END IF;

  RAISE NOTICE 'T2 PASSED: overlap rejected, adjacent slot accepted, availability helper agrees (conflict=%)',
    v_avail.conflict_type;
END;
$$;

-- -----------------------------------------------------------------------------
-- T3  Work order state machine
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_wo_id  uuid;
  v_wo     fms.work_orders;
  v_blocked boolean := false;
BEGIN
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid,
                          'ffffffff-0000-4000-8000-000000000002'::uuid, false);

  INSERT INTO fms.work_orders (tenant_id, facility_id, wo_no, work_order_type, source, title,
                               asset_id, spatial_node_id, requester_id, priority, status)
  VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
          fms.next_document_no('aaaaaaaa-0000-4000-8000-000000000001', 'WORK_ORDER', 'TESTWO'),
          'CORRECTIVE', 'MANUAL', '4F 空調異音（測試）',
          '20000000-0000-4000-8000-000000000002', '10000000-0000-4000-8000-000000000003',
          'ffffffff-0000-4000-8000-000000000004', 'HIGH', 'SUBMITTED')
  RETURNING id INTO v_wo_id;

  -- Legal path: SUBMITTED → ASSIGNED → IN_PROGRESS → COMPLETED
  --
  -- 執行者用 tech.liu（…006）—— 台北總部大樓的技師。
  --
  -- **這一格曾經改用 admin.chen（租戶管理員）繞過一個種子的不一致。**
  -- 022 讓 `transition_work_order` 自行執行 `required_permission` 之後，
  -- `START_WORK` 要 `work_order:execute`，而該權限是對**這張工單的場域**
  -- 判定的。這張工單在台北總部（cccccccc-…-001），而當時種子裡唯一的
  -- TECHNICIAN（tech.wang）範圍在信義影城 —— 他在總部沒有這個權限，
  -- 判定回 false 是正確的；總部根本沒有任何場域級的執行者。
  --
  -- 用租戶管理員當執行者可以讓 T3 通過，但代價是**「場域級執行者能不能
  -- 執行工單」這條最常見的路徑沒有被任何東西走過** —— 租戶級授權涵蓋所有
  -- 場域，它通過不代表場域級的會通過。
  --
  -- 009 現在補了 tech.liu（TECHNICIAN @ 台北總部大樓），因此這裡改回
  -- 場域級執行者。T3 仍然只驗狀態機（RBAC 由 T12 用 6942 組比對守），
  -- 但它現在走的是真實的那條路。
  UPDATE fms.work_orders SET assignee_id = 'ffffffff-0000-4000-8000-000000000006' WHERE id = v_wo_id;
  v_wo := fms.transition_work_order(v_wo_id, 'ASSIGN',  'ffffffff-0000-4000-8000-000000000002');
  IF v_wo.status <> 'ASSIGNED' THEN RAISE EXCEPTION 'T3 FAILED: ASSIGN did not reach ASSIGNED'; END IF;

  v_wo := fms.transition_work_order(v_wo_id, 'START_WORK', 'ffffffff-0000-4000-8000-000000000006');
  IF v_wo.status <> 'IN_PROGRESS' OR v_wo.actual_start_at IS NULL THEN
    RAISE EXCEPTION 'T3 FAILED: START_WORK did not set IN_PROGRESS + actual_start_at';
  END IF;

  -- Illegal action for the current status must be refused.
  BEGIN
    PERFORM fms.transition_work_order(v_wo_id, 'APPROVE', 'ffffffff-0000-4000-8000-000000000002');
  EXCEPTION WHEN check_violation THEN
    v_blocked := true;
  END;
  IF NOT v_blocked THEN
    RAISE EXCEPTION 'T3 FAILED: illegal action APPROVE from IN_PROGRESS was accepted';
  END IF;

  -- Direct UPDATE bypassing the function must also be refused by the trigger.
  v_blocked := false;
  BEGIN
    UPDATE fms.work_orders SET status = 'CLOSED' WHERE id = v_wo_id;
  EXCEPTION WHEN check_violation THEN
    v_blocked := true;
  END;
  IF NOT v_blocked THEN
    RAISE EXCEPTION 'T3 FAILED: raw UPDATE to CLOSED from IN_PROGRESS was accepted';
  END IF;

  v_wo := fms.transition_work_order(v_wo_id, 'COMPLETE', 'ffffffff-0000-4000-8000-000000000006',
                                    '更換軸承並測試');
  IF v_wo.status <> 'COMPLETED' OR v_wo.completed_at IS NULL THEN
    RAISE EXCEPTION 'T3 FAILED: COMPLETE did not finalise the work order';
  END IF;

  -- Transition audit trail must contain every step.
  IF (SELECT count(*) FROM fms.work_order_transitions WHERE work_order_id = v_wo_id) < 3 THEN
    RAISE EXCEPTION 'T3 FAILED: transition log incomplete';
  END IF;

  RAISE NOTICE 'T3 PASSED: legal path allowed, illegal transitions blocked, audit trail written';
END;
$$;

-- -----------------------------------------------------------------------------
-- T4  IoT alarm → work order linkage
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_alarm_id uuid;
  v_alarm    fms.alarms;
  v_wo_count integer;
BEGIN
  PERFORM fms.set_context('aaaaaaaa-0000-4000-8000-000000000001'::uuid, NULL, false);

  -- Ingest a reading, then raise the rule that watches it.
  PERFORM fms.ingest_telemetry('a3000000-0000-4000-8000-000000000002'::uuid,
                               clock_timestamp(), 512.0);

  v_alarm_id := fms.raise_alarm('a4000000-0000-4000-8000-000000000001'::uuid,
                                'a3000000-0000-4000-8000-000000000002'::uuid,
                                512.0, '4F 空調濾網壓差 512Pa 超過門檻 450Pa');

  SELECT * INTO v_alarm FROM fms.alarms WHERE id = v_alarm_id;

  IF v_alarm.work_order_id IS NULL THEN
    RAISE EXCEPTION 'T4 FAILED: alarm did not create/link a work order';
  END IF;

  IF (SELECT source FROM fms.work_orders WHERE id = v_alarm.work_order_id) <> 'IOT_ALARM' THEN
    RAISE EXCEPTION 'T4 FAILED: linked work order has the wrong source';
  END IF;

  -- Re-raising inside the dedupe window must NOT create a second work order.
  PERFORM fms.raise_alarm('a4000000-0000-4000-8000-000000000001'::uuid,
                          'a3000000-0000-4000-8000-000000000002'::uuid, 530.0);

  SELECT count(*) INTO v_wo_count FROM fms.work_orders
   WHERE alarm_id = v_alarm_id AND deleted_at IS NULL;
  IF v_wo_count <> 1 THEN
    RAISE EXCEPTION 'T4 FAILED: dedupe window produced % work orders', v_wo_count;
  END IF;

  IF (SELECT occurrence_count FROM fms.alarms WHERE id = v_alarm_id) < 2 THEN
    RAISE EXCEPTION 'T4 FAILED: repeated alarm did not increment occurrence_count';
  END IF;

  -- The outbox must carry the events the workers rely on.
  IF NOT EXISTS (SELECT 1 FROM fms.event_outbox
                  WHERE aggregate_id = v_alarm_id AND event_type = 'alarm.raised') THEN
    RAISE EXCEPTION 'T4 FAILED: alarm.raised event not written to the outbox';
  END IF;

  RAISE NOTICE 'T4 PASSED: alarm opened one work order, deduped repeats, emitted events';
END;
$$;

-- Nothing is persisted: the whole test suite is a single rolled-back transaction.
ROLLBACK;
