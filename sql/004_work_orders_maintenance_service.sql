-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 004: Teams & skills, SLA, preventive maintenance, Soft FM service
--                catalogue, unified work orders with a data-driven state machine
-- =============================================================================

BEGIN;

-- -----------------------------------------------------------------------------
-- 1. Teams, skills, shifts (技師中心)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.teams (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id   uuid REFERENCES fms.facilities(id) ON DELETE CASCADE,  -- NULL = tenant-wide team
  code          varchar(50) NOT NULL,
  name          varchar(150) NOT NULL,
  team_type     text NOT NULL DEFAULT 'MAINTENANCE'
                  CHECK (team_type IN ('MAINTENANCE','CLEANING','CATERING','IT_SUPPORT',
                                       'SECURITY','LANDSCAPING','FRONT_OF_HOUSE','VENDOR')),
  vendor_name   varchar(150),
  lead_user_id  uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  -- {"strategy":"ROUND_ROBIN"|"LEAST_LOADED"|"SKILL_MATCH","fallback_user_id":"..."}
  dispatch_rule jsonb NOT NULL DEFAULT '{"strategy":"LEAST_LOADED"}'::jsonb,
  contact_email varchar(150),
  contact_phone varchar(40),
  is_active     boolean NOT NULL DEFAULT true,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_teams_code ON fms.teams (tenant_id, lower(code));
CREATE INDEX idx_teams_facility ON fms.teams (facility_id) WHERE is_active;

CREATE TABLE fms.team_members (
  team_id   uuid NOT NULL REFERENCES fms.teams(id) ON DELETE CASCADE,
  user_id   uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  tenant_id uuid NOT NULL,
  role_in_team text NOT NULL DEFAULT 'MEMBER'
                 CHECK (role_in_team IN ('LEAD','DISPATCHER','MEMBER','BACKUP')),
  joined_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (team_id, user_id)
);

CREATE TABLE fms.skills (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,
  code        varchar(50) NOT NULL,
  name        varchar(150) NOT NULL,
  domain      varchar(40),
  requires_certification boolean NOT NULL DEFAULT false,
  created_at  timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_skills_code
  ON fms.skills (coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid), lower(code));

CREATE TABLE fms.user_skills (
  user_id     uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  skill_id    uuid NOT NULL REFERENCES fms.skills(id) ON DELETE CASCADE,
  tenant_id   uuid NOT NULL,
  level       smallint NOT NULL DEFAULT 1 CHECK (level BETWEEN 1 AND 5),
  certified_at date,
  expires_at  date,
  certificate_no varchar(80),
  PRIMARY KEY (user_id, skill_id)
);

CREATE INDEX idx_user_skills_expiring ON fms.user_skills (tenant_id, expires_at)
  WHERE expires_at IS NOT NULL;

-- On-call / shift roster used by AUTO_ASSIGN
CREATE TABLE fms.team_shifts (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  team_id     uuid NOT NULL REFERENCES fms.teams(id) ON DELETE CASCADE,
  user_id     uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  shift_start timestamptz NOT NULL,
  shift_end   timestamptz NOT NULL,
  shift_type  text NOT NULL DEFAULT 'REGULAR'
                CHECK (shift_type IN ('REGULAR','ON_CALL','OVERTIME','LEAVE')),
  shift_range tstzrange GENERATED ALWAYS AS (tstzrange(shift_start, shift_end, '[)')) STORED,
  CONSTRAINT ck_team_shifts_range CHECK (shift_end > shift_start)
);

CREATE INDEX idx_team_shifts_lookup ON fms.team_shifts USING gist (team_id, shift_range);

-- -----------------------------------------------------------------------------
-- 2. SLA policies
-- -----------------------------------------------------------------------------

CREATE TABLE fms.sla_policies (
  id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id           uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id         uuid REFERENCES fms.facilities(id) ON DELETE CASCADE,
  code                varchar(50) NOT NULL,
  name                varchar(150) NOT NULL,
  applies_to_priority text CHECK (applies_to_priority IN ('LOW','MEDIUM','HIGH','URGENT','CRITICAL')),
  response_minutes    integer NOT NULL DEFAULT 60,
  resolution_minutes  integer NOT NULL DEFAULT 480,
  business_hours_only boolean NOT NULL DEFAULT true,
  -- [{"at_pct":80,"notify":["TEAM_LEAD"]},{"at_pct":100,"notify":["FACILITY_ADMIN"],"escalate_priority":true}]
  escalation_rules    jsonb NOT NULL DEFAULT '[]'::jsonb,
  is_active           boolean NOT NULL DEFAULT true,
  created_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at          timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_sla_policies_code ON fms.sla_policies (tenant_id, lower(code));

-- -----------------------------------------------------------------------------
-- 3. Maintenance templates & plans (PM)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.maintenance_templates (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id          uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,  -- NULL = platform template
  code               varchar(50) NOT NULL,
  name               varchar(200) NOT NULL,
  description        text,
  applies_to_category_id uuid REFERENCES fms.asset_categories(id) ON DELETE SET NULL,
  applies_to_model_id    uuid REFERENCES fms.asset_models(id) ON DELETE SET NULL,
  maintenance_type   text NOT NULL DEFAULT 'PREVENTIVE'
                       CHECK (maintenance_type IN ('PREVENTIVE','INSPECTION','CALIBRATION',
                                                   'DEEP_CLEAN','STATUTORY','PREDICTIVE')),
  -- [{"seq":1,"title":"檢查濾網","type":"CHECKBOX","required":true},
  --  {"seq":2,"title":"進風溫度","type":"NUMBER","unit":"°C","min":10,"max":40}]
  checklist          jsonb NOT NULL DEFAULT '[]'::jsonb,
  estimated_minutes  integer NOT NULL DEFAULT 60,
  required_skill_codes text[] NOT NULL DEFAULT '{}',
  required_part_codes  text[] NOT NULL DEFAULT '{}',
  safety_notes       text,
  requires_permit    boolean NOT NULL DEFAULT false,
  requires_shutdown  boolean NOT NULL DEFAULT false,
  is_active          boolean NOT NULL DEFAULT true,
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at         timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_maintenance_templates_code
  ON fms.maintenance_templates (coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid), lower(code));

CREATE TABLE fms.maintenance_plans (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id         uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id       uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  template_id       uuid NOT NULL REFERENCES fms.maintenance_templates(id) ON DELETE RESTRICT,
  code              varchar(50) NOT NULL,
  name              varchar(200) NOT NULL,
  -- Exactly one targeting mode: a single asset, a spatial subtree, or a category.
  asset_id          uuid REFERENCES fms.assets(id) ON DELETE CASCADE,
  spatial_node_id   uuid REFERENCES fms.spatial_nodes(id) ON DELETE CASCADE,
  category_id       uuid REFERENCES fms.asset_categories(id) ON DELETE CASCADE,
  trigger_type      text NOT NULL DEFAULT 'CALENDAR'
                      CHECK (trigger_type IN ('CALENDAR','METER','CONDITION','HYBRID')),
  -- RFC 5545 recurrence, e.g. FREQ=MONTHLY;BYMONTHDAY=1
  rrule             text,
  meter_code        varchar(50),
  meter_threshold   numeric(18,4),
  meter_tolerance_pct numeric(5,2) DEFAULT 10,
  -- Generate the work order this many days before the due date.
  generate_lead_days smallint NOT NULL DEFAULT 7,
  priority          text NOT NULL DEFAULT 'MEDIUM'
                      CHECK (priority IN ('LOW','MEDIUM','HIGH','URGENT','CRITICAL')),
  assigned_team_id  uuid REFERENCES fms.teams(id) ON DELETE SET NULL,
  sla_policy_id     uuid REFERENCES fms.sla_policies(id) ON DELETE SET NULL,
  next_due_at       timestamptz,
  last_generated_at timestamptz,
  is_active         boolean NOT NULL DEFAULT true,
  created_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_plan_target CHECK (
    (asset_id IS NOT NULL)::int + (spatial_node_id IS NOT NULL)::int + (category_id IS NOT NULL)::int = 1
  ),
  CONSTRAINT ck_plan_trigger CHECK (
    (trigger_type = 'CALENDAR' AND rrule IS NOT NULL)
    OR (trigger_type = 'METER' AND meter_code IS NOT NULL AND meter_threshold IS NOT NULL)
    OR trigger_type IN ('CONDITION','HYBRID')
  )
);

CREATE UNIQUE INDEX uq_maintenance_plans_code ON fms.maintenance_plans (tenant_id, lower(code));
CREATE INDEX idx_maintenance_plans_due ON fms.maintenance_plans (next_due_at)
  WHERE is_active;

-- One row per generated (or skipped) occurrence — makes PM compliance auditable.
CREATE TABLE fms.maintenance_occurrences (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  plan_id       uuid NOT NULL REFERENCES fms.maintenance_plans(id) ON DELETE CASCADE,
  asset_id      uuid REFERENCES fms.assets(id) ON DELETE CASCADE,
  scheduled_for timestamptz NOT NULL,
  status        text NOT NULL DEFAULT 'PLANNED'
                  CHECK (status IN ('PLANNED','GENERATED','SKIPPED','COMPLETED','MISSED')),
  work_order_id uuid,
  skip_reason   varchar(200),
  generated_at  timestamptz,
  completed_at  timestamptz,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_maintenance_occurrences
  ON fms.maintenance_occurrences (plan_id, coalesce(asset_id, '00000000-0000-0000-0000-000000000000'::uuid), scheduled_for);
CREATE INDEX idx_maintenance_occurrences_pending
  ON fms.maintenance_occurrences (tenant_id, status, scheduled_for);

-- -----------------------------------------------------------------------------
-- 4. Soft FM service catalogue
-- -----------------------------------------------------------------------------

CREATE TABLE fms.service_items (
  id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id           uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id         uuid REFERENCES fms.facilities(id) ON DELETE CASCADE,  -- NULL = all facilities
  category            text NOT NULL
                        CHECK (category IN ('CLEANING','CATERING','IT_SUPPORT','ROOM_SETUP',
                                            'SECURITY','MOVING','WASTE','LANDSCAPING',
                                            'AV_SUPPORT','RECEPTION','OTHER')),
  code                varchar(50) NOT NULL,
  name                varchar(200) NOT NULL,
  description         text,
  -- Booking economics
  lead_time_minutes   integer NOT NULL DEFAULT 0,     -- how far in advance it must be requested
  default_duration_minutes integer NOT NULL DEFAULT 30,
  -- Offset relative to a linked reservation: -15 = start 15 min before the meeting.
  relative_offset_minutes integer NOT NULL DEFAULT 0,
  is_attachable_to_reservation boolean NOT NULL DEFAULT true,
  is_standalone_requestable    boolean NOT NULL DEFAULT true,
  requires_approval   boolean NOT NULL DEFAULT false,
  approver_role_code  varchar(50),
  chargeable          boolean NOT NULL DEFAULT false,
  unit_price          numeric(12,2),
  currency            char(3),
  unit_label          varchar(30),                    -- 'per person', 'per cup', 'per room'
  max_quantity        integer,
  -- JSON Schema validated by the API against work_orders.payload
  form_schema         jsonb NOT NULL DEFAULT '{"type":"object","properties":{}}'::jsonb,
  default_team_id     uuid REFERENCES fms.teams(id) ON DELETE SET NULL,
  sla_policy_id       uuid REFERENCES fms.sla_policies(id) ON DELETE SET NULL,
  -- {"mon":[["07:00","20:00"]], "blackout_dates":["2026-01-01"]}
  availability        jsonb NOT NULL DEFAULT '{}'::jsonb,
  icon                varchar(60),
  display_order       integer NOT NULL DEFAULT 100,
  is_active           boolean NOT NULL DEFAULT true,
  created_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at          timestamptz
);

CREATE UNIQUE INDEX uq_service_items_code
  ON fms.service_items (tenant_id,
                        coalesce(facility_id, '00000000-0000-0000-0000-000000000000'::uuid),
                        lower(code))
  WHERE deleted_at IS NULL;
CREATE INDEX idx_service_items_catalogue
  ON fms.service_items (tenant_id, facility_id, category) WHERE is_active AND deleted_at IS NULL;

-- -----------------------------------------------------------------------------
-- 5. Unified work orders  (Hard FM maintenance + Soft FM service requests)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.work_orders (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id         uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id       uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  wo_no             varchar(40) NOT NULL,
  work_order_type   text NOT NULL
                      CHECK (work_order_type IN ('MAINTENANCE','SERVICE','INSPECTION',
                                                 'CORRECTIVE','PROJECT')),
  -- Provenance matters for reporting: how much work is reactive vs planned vs IoT-driven.
  source            text NOT NULL DEFAULT 'MANUAL'
                      CHECK (source IN ('MANUAL','PM_PLAN','IOT_ALARM','RESERVATION',
                                        'API','IMPORT','INSPECTION_FINDING')),
  title             varchar(250) NOT NULL,
  description       text,
  -- Targets — at least one of asset / spatial_node must be present.
  asset_id          uuid REFERENCES fms.assets(id) ON DELETE SET NULL,
  spatial_node_id   uuid REFERENCES fms.spatial_nodes(id) ON DELETE SET NULL,
  service_item_id   uuid REFERENCES fms.service_items(id) ON DELETE SET NULL,
  maintenance_plan_id uuid REFERENCES fms.maintenance_plans(id) ON DELETE SET NULL,
  maintenance_occurrence_id uuid REFERENCES fms.maintenance_occurrences(id) ON DELETE SET NULL,
  reservation_id    uuid,   -- FK added in 005 (circular dependency)
  alarm_id          uuid,   -- FK added in 006
  parent_work_order_id uuid REFERENCES fms.work_orders(id) ON DELETE SET NULL,
  -- People
  requester_id      uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  requester_contact varchar(120),
  assignee_id       uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  team_id           uuid REFERENCES fms.teams(id) ON DELETE SET NULL,
  -- Classification
  priority          text NOT NULL DEFAULT 'MEDIUM'
                      CHECK (priority IN ('LOW','MEDIUM','HIGH','URGENT','CRITICAL')),
  status            text NOT NULL DEFAULT 'DRAFT',
  -- Dynamic per-service-item fields, validated against service_items.form_schema
  payload           jsonb NOT NULL DEFAULT '{}'::jsonb,
  -- Scheduling
  requested_start_at timestamptz,
  scheduled_start_at timestamptz,
  scheduled_end_at   timestamptz,
  actual_start_at    timestamptz,
  actual_end_at      timestamptz,
  -- SLA
  sla_policy_id     uuid REFERENCES fms.sla_policies(id) ON DELETE SET NULL,
  response_due_at   timestamptz,
  resolution_due_at timestamptz,
  first_responded_at timestamptz,
  sla_state         text NOT NULL DEFAULT 'ON_TRACK'
                      CHECK (sla_state IN ('NOT_APPLICABLE','ON_TRACK','AT_RISK',
                                           'RESPONSE_BREACHED','RESOLUTION_BREACHED','MET')),
  -- Effort & cost
  labor_minutes     integer NOT NULL DEFAULT 0,
  labor_cost        numeric(14,2) NOT NULL DEFAULT 0,
  parts_cost        numeric(14,2) NOT NULL DEFAULT 0,
  other_cost        numeric(14,2) NOT NULL DEFAULT 0,
  currency          char(3),
  is_chargeback     boolean NOT NULL DEFAULT false,
  chargeback_org_id uuid REFERENCES fms.organizations(id) ON DELETE SET NULL,
  -- Closure
  close_code        varchar(50),
  root_cause        varchar(120),
  resolution_notes  text,
  failure_code      varchar(50),
  satisfaction_score smallint CHECK (satisfaction_score BETWEEN 1 AND 5),
  satisfaction_comment text,
  reopened_count    smallint NOT NULL DEFAULT 0,
  cancelled_reason  varchar(250),
  version           integer NOT NULL DEFAULT 1,
  created_by        uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  created_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  completed_at      timestamptz,
  closed_at         timestamptz,
  deleted_at        timestamptz,
  CONSTRAINT ck_wo_target CHECK (asset_id IS NOT NULL OR spatial_node_id IS NOT NULL),
  CONSTRAINT ck_wo_service_item CHECK (work_order_type <> 'SERVICE' OR service_item_id IS NOT NULL),
  CONSTRAINT ck_wo_schedule CHECK (scheduled_end_at IS NULL OR scheduled_start_at IS NULL
                                   OR scheduled_end_at >= scheduled_start_at)
);

CREATE UNIQUE INDEX uq_work_orders_no ON fms.work_orders (tenant_id, wo_no);
CREATE INDEX idx_wo_facility_status ON fms.work_orders (facility_id, status)
  WHERE deleted_at IS NULL;
CREATE INDEX idx_wo_assignee_open ON fms.work_orders (assignee_id, status)
  WHERE deleted_at IS NULL AND status NOT IN ('COMPLETED','CLOSED','CANCELLED','REJECTED');
CREATE INDEX idx_wo_team_open ON fms.work_orders (team_id, priority, resolution_due_at)
  WHERE deleted_at IS NULL AND status NOT IN ('COMPLETED','CLOSED','CANCELLED','REJECTED');
CREATE INDEX idx_wo_asset ON fms.work_orders (asset_id, created_at DESC) WHERE deleted_at IS NULL;
CREATE INDEX idx_wo_node ON fms.work_orders (spatial_node_id, created_at DESC) WHERE deleted_at IS NULL;
CREATE INDEX idx_wo_reservation ON fms.work_orders (reservation_id) WHERE reservation_id IS NOT NULL;
CREATE INDEX idx_wo_alarm ON fms.work_orders (alarm_id) WHERE alarm_id IS NOT NULL;
CREATE INDEX idx_wo_sla_watch ON fms.work_orders (resolution_due_at)
  WHERE sla_state IN ('ON_TRACK','AT_RISK');
CREATE INDEX idx_wo_payload_gin ON fms.work_orders USING gin (payload jsonb_path_ops);
CREATE INDEX idx_wo_tenant_created ON fms.work_orders (tenant_id, created_at DESC)
  WHERE deleted_at IS NULL;

CREATE TRIGGER trg_work_orders_version
  BEFORE UPDATE ON fms.work_orders FOR EACH ROW EXECUTE FUNCTION fms.trg_bump_version();
CREATE TRIGGER trg_work_orders_freeze_tenant
  BEFORE UPDATE ON fms.work_orders FOR EACH ROW EXECUTE FUNCTION fms.trg_freeze_tenant_id();

ALTER TABLE fms.maintenance_occurrences
  ADD CONSTRAINT fk_occurrence_work_order
  FOREIGN KEY (work_order_id) REFERENCES fms.work_orders(id) ON DELETE SET NULL;

ALTER TABLE fms.asset_status_history
  ADD CONSTRAINT fk_asset_status_history_wo
  FOREIGN KEY (work_order_id) REFERENCES fms.work_orders(id) ON DELETE SET NULL;

-- -----------------------------------------------------------------------------
-- 6. Data-driven state machine
-- -----------------------------------------------------------------------------
-- Transitions live in a table, not in code, so each vertical (cinema / campus /
-- factory) can extend its workflow without a deployment, and the database
-- refuses any transition the table does not describe.
-- -----------------------------------------------------------------------------

CREATE TABLE fms.work_order_statuses (
  code        varchar(30) PRIMARY KEY,
  name_zh     varchar(60) NOT NULL,
  name_en     varchar(60) NOT NULL,
  category    text NOT NULL CHECK (category IN ('OPEN','IN_PROGRESS','WAITING','TERMINAL')),
  is_terminal boolean NOT NULL DEFAULT false,
  display_order smallint NOT NULL DEFAULT 100
);

-- Referential integrity for work_orders.status (statuses are seeded in 008).
ALTER TABLE fms.work_orders
  ADD CONSTRAINT fk_work_orders_status
  FOREIGN KEY (status) REFERENCES fms.work_order_statuses(code);

CREATE TABLE fms.work_order_transitions_allowed (
  id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id           uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,  -- NULL = platform default
  work_order_type     varchar(20),         -- NULL = applies to every type
  from_status         varchar(30) NOT NULL REFERENCES fms.work_order_statuses(code),
  action              varchar(40) NOT NULL,
  to_status           varchar(30) NOT NULL REFERENCES fms.work_order_statuses(code),
  required_permission varchar(80) REFERENCES fms.permissions(code),
  -- Fields the API must supply for this action (checked before the UPDATE).
  required_fields     text[] NOT NULL DEFAULT '{}',
  -- Declarative effects executed by the service layer after a successful transition.
  -- e.g. {"emit":"work_order.assigned","notify":["ASSIGNEE"],"set_actual_start":true}
  side_effects        jsonb NOT NULL DEFAULT '{}'::jsonb,
  is_active           boolean NOT NULL DEFAULT true
);

CREATE UNIQUE INDEX uq_wo_transitions
  ON fms.work_order_transitions_allowed (
       coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid),
       coalesce(work_order_type, '*'), from_status, action);

CREATE TABLE fms.work_order_transitions (
  id            bigserial PRIMARY KEY,
  tenant_id     uuid NOT NULL,
  work_order_id uuid NOT NULL REFERENCES fms.work_orders(id) ON DELETE CASCADE,
  from_status   varchar(30),
  action        varchar(40) NOT NULL,
  to_status     varchar(30) NOT NULL,
  actor_user_id uuid,
  actor_type    text NOT NULL DEFAULT 'USER'
                  CHECK (actor_type IN ('USER','SYSTEM','SERVICE_ACCOUNT')),
  reason        varchar(250),
  metadata      jsonb NOT NULL DEFAULT '{}'::jsonb,
  occurred_at   timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX idx_wo_transitions_wo ON fms.work_order_transitions (work_order_id, occurred_at);

-- Guard: any status change must correspond to an active allowed transition.
-- The API is expected to call fms.transition_work_order(); this trigger is the
-- backstop that catches direct UPDATEs.
CREATE OR REPLACE FUNCTION fms.trg_enforce_wo_transition()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_allowed boolean;
BEGIN
  IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
    RETURN NEW;
  END IF;

  -- Escape hatch for data migrations / platform support operations.
  IF fms.is_platform_context() THEN
    RETURN NEW;
  END IF;

  SELECT EXISTS (
    SELECT 1 FROM fms.work_order_transitions_allowed t
    WHERE t.is_active
      AND (t.tenant_id IS NULL OR t.tenant_id = NEW.tenant_id)
      AND (t.work_order_type IS NULL OR t.work_order_type = NEW.work_order_type)
      AND t.from_status = OLD.status
      AND t.to_status   = NEW.status
  ) INTO v_allowed;

  IF NOT v_allowed THEN
    RAISE EXCEPTION 'illegal work order transition % -> % (type %, wo %)',
      OLD.status, NEW.status, NEW.work_order_type, NEW.wo_no
      USING ERRCODE = '23514';
  END IF;

  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_work_orders_transition_guard
  BEFORE UPDATE OF status ON fms.work_orders
  FOR EACH ROW EXECUTE FUNCTION fms.trg_enforce_wo_transition();

-- Canonical transition entry point: validates, applies, logs, emits.
CREATE OR REPLACE FUNCTION fms.transition_work_order(
  p_work_order_id uuid,
  p_action        varchar,
  p_actor_user_id uuid DEFAULT NULL,
  p_reason        varchar DEFAULT NULL,
  p_metadata      jsonb DEFAULT '{}'::jsonb
) RETURNS fms.work_orders
LANGUAGE plpgsql
AS $$
DECLARE
  v_wo    fms.work_orders;
  v_rule  fms.work_order_transitions_allowed;
  v_actor uuid := coalesce(p_actor_user_id, fms.current_user_id());
BEGIN
  SELECT * INTO v_wo FROM fms.work_orders WHERE id = p_work_order_id FOR UPDATE;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'work order % not found', p_work_order_id USING ERRCODE = 'P0002';
  END IF;

  SELECT * INTO v_rule
  FROM fms.work_order_transitions_allowed t
  WHERE t.is_active
    AND (t.tenant_id IS NULL OR t.tenant_id = v_wo.tenant_id)
    AND (t.work_order_type IS NULL OR t.work_order_type = v_wo.work_order_type)
    AND t.from_status = v_wo.status
    AND t.action = p_action
  ORDER BY t.tenant_id NULLS LAST, t.work_order_type NULLS LAST
  LIMIT 1;

  IF v_rule.id IS NULL THEN
    RAISE EXCEPTION 'action % is not allowed from status % (wo %)',
      p_action, v_wo.status, v_wo.wo_no USING ERRCODE = '23514';
  END IF;

  UPDATE fms.work_orders
     SET status = v_rule.to_status,
         first_responded_at = CASE
           WHEN first_responded_at IS NULL AND (v_rule.side_effects ->> 'set_responded') = 'true'
           THEN clock_timestamp() ELSE first_responded_at END,
         actual_start_at = CASE
           WHEN (v_rule.side_effects ->> 'set_actual_start') = 'true'
           THEN coalesce(actual_start_at, clock_timestamp()) ELSE actual_start_at END,
         actual_end_at = CASE
           WHEN (v_rule.side_effects ->> 'set_actual_end') = 'true'
           THEN clock_timestamp() ELSE actual_end_at END,
         completed_at = CASE
           WHEN v_rule.to_status = 'COMPLETED' THEN clock_timestamp() ELSE completed_at END,
         closed_at = CASE
           WHEN v_rule.to_status = 'CLOSED' THEN clock_timestamp() ELSE closed_at END,
         cancelled_reason = CASE
           WHEN v_rule.to_status IN ('CANCELLED','REJECTED') THEN p_reason ELSE cancelled_reason END,
         sla_state = CASE
           WHEN v_rule.to_status IN ('COMPLETED','CLOSED')
                AND (resolution_due_at IS NULL OR clock_timestamp() <= resolution_due_at)
                AND sla_state NOT IN ('RESPONSE_BREACHED','RESOLUTION_BREACHED')
           THEN 'MET' ELSE sla_state END
   WHERE id = p_work_order_id
   RETURNING * INTO v_wo;

  INSERT INTO fms.work_order_transitions
    (tenant_id, work_order_id, from_status, action, to_status, actor_user_id, reason, metadata)
  VALUES
    (v_wo.tenant_id, v_wo.id, v_rule.from_status, p_action, v_rule.to_status,
     v_actor, p_reason, p_metadata);

  PERFORM fms.emit_event(
    v_wo.tenant_id,
    coalesce(v_rule.side_effects ->> 'emit', 'work_order.status_changed'),
    'WORK_ORDER', v_wo.id,
    jsonb_build_object(
      'wo_no', v_wo.wo_no, 'from', v_rule.from_status, 'to', v_rule.to_status,
      'action', p_action, 'actor_user_id', v_actor,
      'facility_id', v_wo.facility_id, 'assignee_id', v_wo.assignee_id));

  RETURN v_wo;
END;
$$;

COMMENT ON FUNCTION fms.transition_work_order IS
  'Single sanctioned path for work order status changes. Validates against work_order_transitions_allowed, writes the audit row and the outbox event atomically.';

-- -----------------------------------------------------------------------------
-- 7. Work order detail: checklist results, comments, labour, parts
-- -----------------------------------------------------------------------------

CREATE TABLE fms.work_order_tasks (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL,
  work_order_id uuid NOT NULL REFERENCES fms.work_orders(id) ON DELETE CASCADE,
  seq           smallint NOT NULL,
  title         varchar(250) NOT NULL,
  input_type    text NOT NULL DEFAULT 'CHECKBOX'
                  CHECK (input_type IN ('CHECKBOX','NUMBER','TEXT','PHOTO','SIGNATURE','SELECT')),
  unit          varchar(20),
  min_value     numeric(18,4),
  max_value     numeric(18,4),
  options       jsonb,
  is_required   boolean NOT NULL DEFAULT false,
  result_value  jsonb,
  is_pass       boolean,
  completed_by  uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  completed_at  timestamptz,
  notes         text
);

CREATE UNIQUE INDEX uq_wo_tasks_seq ON fms.work_order_tasks (work_order_id, seq);

CREATE TABLE fms.work_order_comments (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL,
  work_order_id uuid NOT NULL REFERENCES fms.work_orders(id) ON DELETE CASCADE,
  author_id     uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  visibility    text NOT NULL DEFAULT 'INTERNAL'
                  CHECK (visibility IN ('INTERNAL','REQUESTER_VISIBLE','PUBLIC')),
  body          text NOT NULL,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  edited_at     timestamptz
);

CREATE INDEX idx_wo_comments_wo ON fms.work_order_comments (work_order_id, created_at);

CREATE TABLE fms.work_order_labor (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL,
  work_order_id uuid NOT NULL REFERENCES fms.work_orders(id) ON DELETE CASCADE,
  user_id       uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  started_at    timestamptz NOT NULL,
  ended_at      timestamptz,
  minutes       integer,
  hourly_rate   numeric(10,2),
  cost          numeric(14,2),
  is_overtime   boolean NOT NULL DEFAULT false,
  notes         text,
  CONSTRAINT ck_labor_range CHECK (ended_at IS NULL OR ended_at >= started_at)
);

CREATE INDEX idx_wo_labor_wo ON fms.work_order_labor (work_order_id);

CREATE TABLE fms.parts (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  part_code     varchar(60) NOT NULL,
  name          varchar(200) NOT NULL,
  category_id   uuid REFERENCES fms.asset_categories(id) ON DELETE SET NULL,
  unit          varchar(20) NOT NULL DEFAULT 'PCS',
  unit_cost     numeric(14,2),
  currency      char(3),
  manufacturer  varchar(120),
  manufacturer_part_no varchar(120),
  is_consumable boolean NOT NULL DEFAULT true,
  is_active     boolean NOT NULL DEFAULT true,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_parts_code ON fms.parts (tenant_id, lower(part_code));

CREATE TABLE fms.part_stock (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL,
  part_id       uuid NOT NULL REFERENCES fms.parts(id) ON DELETE CASCADE,
  facility_id   uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  storage_node_id uuid REFERENCES fms.spatial_nodes(id) ON DELETE SET NULL,
  quantity_on_hand numeric(14,3) NOT NULL DEFAULT 0,
  quantity_reserved numeric(14,3) NOT NULL DEFAULT 0,
  reorder_point   numeric(14,3),
  reorder_quantity numeric(14,3),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_part_stock_nonneg CHECK (quantity_on_hand >= 0 AND quantity_reserved >= 0)
);

CREATE UNIQUE INDEX uq_part_stock ON fms.part_stock (part_id, facility_id,
  coalesce(storage_node_id, '00000000-0000-0000-0000-000000000000'::uuid));
CREATE INDEX idx_part_stock_reorder ON fms.part_stock (tenant_id, facility_id)
  WHERE reorder_point IS NOT NULL;

CREATE TABLE fms.work_order_parts (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL,
  work_order_id uuid NOT NULL REFERENCES fms.work_orders(id) ON DELETE CASCADE,
  part_id       uuid NOT NULL REFERENCES fms.parts(id) ON DELETE RESTRICT,
  quantity_planned numeric(14,3) NOT NULL DEFAULT 0,
  quantity_used    numeric(14,3) NOT NULL DEFAULT 0,
  unit_cost     numeric(14,2),
  total_cost    numeric(14,2),
  issued_from_stock_id uuid REFERENCES fms.part_stock(id) ON DELETE SET NULL,
  issued_at     timestamptz,
  issued_by     uuid REFERENCES fms.users(id) ON DELETE SET NULL
);

CREATE INDEX idx_wo_parts_wo ON fms.work_order_parts (work_order_id);

COMMIT;
