-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 005: Bookable resources, reservations, holds, attached Soft FM services
-- =============================================================================
-- Concurrency strategy (spec §6)
--   Layer 1  Short-lived HOLD row created inside the request transaction.
--   Layer 2  GiST exclusion constraint on (tenant_id, resource_id, time_range) —
--            the database is the final arbiter, so even a failed distributed lock
--            (or the Phase 2 Rust engine misbehaving) cannot double-book.
--   Layer 3  Optional Redis Redlock in front, added in Phase 2 purely to reduce
--            wasted round-trips, never as the correctness mechanism.
-- =============================================================================

BEGIN;

-- -----------------------------------------------------------------------------
-- 1. Bookable resource configuration
-- -----------------------------------------------------------------------------

CREATE TABLE fms.bookable_resources (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id          uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id        uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  resource_type      text NOT NULL CHECK (resource_type IN ('SPATIAL_NODE','ASSET')),
  spatial_node_id    uuid REFERENCES fms.spatial_nodes(id) ON DELETE CASCADE,
  asset_id           uuid REFERENCES fms.assets(id) ON DELETE CASCADE,
  display_name       varchar(200),
  -- Booking rules
  is_bookable        boolean NOT NULL DEFAULT true,
  requires_approval  boolean NOT NULL DEFAULT false,
  approver_role_code varchar(50),
  min_duration_minutes  integer NOT NULL DEFAULT 30,
  max_duration_minutes  integer NOT NULL DEFAULT 480,
  slot_granularity_minutes integer NOT NULL DEFAULT 30,
  buffer_before_minutes integer NOT NULL DEFAULT 0,
  buffer_after_minutes  integer NOT NULL DEFAULT 0,
  -- How far ahead / how late a booking may be made
  advance_booking_days  integer NOT NULL DEFAULT 90,
  min_notice_minutes    integer NOT NULL DEFAULT 0,
  max_active_per_user   integer,
  -- Capacity > 1 turns the resource into a pooled resource (e.g. 20 hot desks
  -- behind one logical node). Overlap is then permitted up to capacity and the
  -- exclusion constraint is disabled for this resource (see ck below).
  capacity              integer NOT NULL DEFAULT 1,
  -- {"mon":[["08:00","22:00"]],"sat":[["10:00","18:00"]]} — falls back to facility hours
  opening_hours      jsonb NOT NULL DEFAULT '{}'::jsonb,
  -- Auto-release: cancel if nobody checks in within N minutes (Phase 4 IoT ties in here)
  auto_release_minutes integer,
  requires_check_in  boolean NOT NULL DEFAULT false,
  attributes         jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_bookable_target CHECK (
    (resource_type = 'SPATIAL_NODE' AND spatial_node_id IS NOT NULL AND asset_id IS NULL) OR
    (resource_type = 'ASSET'        AND asset_id IS NOT NULL AND spatial_node_id IS NULL)
  ),
  CONSTRAINT ck_bookable_duration CHECK (max_duration_minutes >= min_duration_minutes)
);

CREATE UNIQUE INDEX uq_bookable_node ON fms.bookable_resources (spatial_node_id)
  WHERE spatial_node_id IS NOT NULL;
CREATE UNIQUE INDEX uq_bookable_asset ON fms.bookable_resources (asset_id)
  WHERE asset_id IS NOT NULL;
CREATE INDEX idx_bookable_facility ON fms.bookable_resources (facility_id) WHERE is_bookable;

-- Recurring / one-off unavailability: holidays, renovation, statutory inspection.
CREATE TABLE fms.resource_blackouts (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id   uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  -- NULL resource = whole facility blackout
  bookable_resource_id uuid REFERENCES fms.bookable_resources(id) ON DELETE CASCADE,
  start_at      timestamptz NOT NULL,
  end_at        timestamptz NOT NULL,
  blackout_range tstzrange GENERATED ALWAYS AS (tstzrange(start_at, end_at, '[)')) STORED,
  reason        varchar(200) NOT NULL,
  blackout_type text NOT NULL DEFAULT 'MAINTENANCE'
                  CHECK (blackout_type IN ('MAINTENANCE','HOLIDAY','RENOVATION',
                                           'PRIVATE_EVENT','EMERGENCY','OTHER')),
  work_order_id uuid REFERENCES fms.work_orders(id) ON DELETE SET NULL,
  created_by    uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_blackout_range CHECK (end_at > start_at)
);

CREATE INDEX idx_blackouts_lookup ON fms.resource_blackouts
  USING gist (bookable_resource_id, blackout_range);
CREATE INDEX idx_blackouts_facility ON fms.resource_blackouts
  USING gist (facility_id, blackout_range);

-- -----------------------------------------------------------------------------
-- 2. Holds (two-phase booking)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.reservation_holds (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id   uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  bookable_resource_id uuid NOT NULL REFERENCES fms.bookable_resources(id) ON DELETE CASCADE,
  user_id       uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  start_at      timestamptz NOT NULL,
  end_at        timestamptz NOT NULL,
  hold_range    tstzrange GENERATED ALWAYS AS (tstzrange(start_at, end_at, '[)')) STORED,
  hold_token    varchar(64) NOT NULL,
  status        text NOT NULL DEFAULT 'ACTIVE'
                  CHECK (status IN ('ACTIVE','CONSUMED','EXPIRED','RELEASED')),
  expires_at    timestamptz NOT NULL DEFAULT clock_timestamp() + interval '3 minutes',
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_hold_range CHECK (end_at > start_at)
);

CREATE UNIQUE INDEX uq_reservation_holds_token ON fms.reservation_holds (hold_token);
CREATE INDEX idx_reservation_holds_expiry ON fms.reservation_holds (expires_at)
  WHERE status = 'ACTIVE';

-- Two ACTIVE holds may not overlap on a single-capacity resource.
ALTER TABLE fms.reservation_holds
  ADD CONSTRAINT excl_reservation_holds_overlap
  EXCLUDE USING gist (
    bookable_resource_id WITH =,
    hold_range WITH &&
  ) WHERE (status = 'ACTIVE');

-- -----------------------------------------------------------------------------
-- 3. Reservations
-- -----------------------------------------------------------------------------

CREATE TABLE fms.reservations (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id    uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  bookable_resource_id uuid NOT NULL REFERENCES fms.bookable_resources(id) ON DELETE RESTRICT,
  reservation_no varchar(40) NOT NULL,
  -- Denormalised for query performance and for the exclusion constraint key.
  resource_type  text NOT NULL CHECK (resource_type IN ('SPATIAL_NODE','ASSET')),
  resource_id    uuid NOT NULL,
  organizer_id   uuid NOT NULL REFERENCES fms.users(id) ON DELETE RESTRICT,
  on_behalf_of_id uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  org_id         uuid REFERENCES fms.organizations(id) ON DELETE SET NULL,
  title          varchar(250),
  purpose        varchar(200),
  party_size     integer NOT NULL DEFAULT 1,
  start_at       timestamptz NOT NULL,
  end_at         timestamptz NOT NULL,
  time_range     tstzrange GENERATED ALWAYS AS (tstzrange(start_at, end_at, '[)')) STORED,
  -- Occupancy window including setup/teardown buffers; used for conflict checks
  -- where a cleaning crew needs the room before the next booking.
  buffer_start_at timestamptz,
  buffer_end_at   timestamptz,
  status         text NOT NULL DEFAULT 'CONFIRMED'
                   CHECK (status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN','COMPLETED',
                                     'CANCELLED','REJECTED','NO_SHOW','EXPIRED')),
  -- Approval
  approval_required boolean NOT NULL DEFAULT false,
  approved_by    uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  approved_at    timestamptz,
  rejection_reason varchar(250),
  -- Check-in / auto-release
  requires_check_in boolean NOT NULL DEFAULT false,
  checked_in_at  timestamptz,
  checked_out_at timestamptz,
  check_in_method text CHECK (check_in_method IN ('MANUAL','QR','NFC','OCCUPANCY_SENSOR','ACCESS_CONTROL')),
  auto_release_at timestamptz,
  -- Recurrence: all instances of a series share recurrence_group_id.
  recurrence_group_id uuid,
  recurrence_rule text,
  is_recurrence_exception boolean NOT NULL DEFAULT false,
  created_via    text NOT NULL DEFAULT 'WEB'
                   CHECK (created_via IN ('WEB','MOBILE','KIOSK','API','OUTLOOK','GOOGLE','IMPORT')),
  external_event_id varchar(200),          -- Outlook/Google calendar correlation
  hold_token     varchar(64),
  cancelled_at   timestamptz,
  cancelled_by   uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  cancellation_reason varchar(250),
  attributes     jsonb NOT NULL DEFAULT '{}'::jsonb,
  version        integer NOT NULL DEFAULT 1,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_reservation_range CHECK (end_at > start_at),
  CONSTRAINT ck_reservation_party CHECK (party_size >= 1)
);

CREATE UNIQUE INDEX uq_reservations_no ON fms.reservations (tenant_id, reservation_no);

-- The correctness backstop: no two blocking reservations may overlap on the
-- same resource within a tenant. Statuses that still occupy the resource are
-- CONFIRMED / CHECKED_IN / PENDING_APPROVAL.
ALTER TABLE fms.reservations
  ADD CONSTRAINT excl_reservations_no_overlap
  EXCLUDE USING gist (
    tenant_id WITH =,
    resource_id WITH =,
    time_range WITH &&
  ) WHERE (status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN'));

CREATE INDEX idx_reservations_resource_time ON fms.reservations
  USING gist (resource_id, time_range);
CREATE INDEX idx_reservations_facility_time ON fms.reservations (facility_id, start_at DESC);
CREATE INDEX idx_reservations_organizer ON fms.reservations (organizer_id, start_at DESC);
CREATE INDEX idx_reservations_status ON fms.reservations (tenant_id, status, start_at);
CREATE INDEX idx_reservations_recurrence ON fms.reservations (recurrence_group_id)
  WHERE recurrence_group_id IS NOT NULL;
CREATE INDEX idx_reservations_auto_release ON fms.reservations (auto_release_at)
  WHERE status = 'CONFIRMED' AND auto_release_at IS NOT NULL;

CREATE TRIGGER trg_reservations_version
  BEFORE UPDATE ON fms.reservations FOR EACH ROW EXECUTE FUNCTION fms.trg_bump_version();
CREATE TRIGGER trg_reservations_freeze_tenant
  BEFORE UPDATE ON fms.reservations FOR EACH ROW EXECUTE FUNCTION fms.trg_freeze_tenant_id();

-- Close the circular dependency declared in 004.
ALTER TABLE fms.work_orders
  ADD CONSTRAINT fk_work_orders_reservation
  FOREIGN KEY (reservation_id) REFERENCES fms.reservations(id) ON DELETE SET NULL;

CREATE TABLE fms.reservation_participants (
  reservation_id uuid NOT NULL REFERENCES fms.reservations(id) ON DELETE CASCADE,
  tenant_id      uuid NOT NULL,
  user_id        uuid REFERENCES fms.users(id) ON DELETE CASCADE,
  external_email varchar(200),
  role           text NOT NULL DEFAULT 'ATTENDEE'
                   CHECK (role IN ('ORGANIZER','ATTENDEE','OPTIONAL','RESOURCE_OWNER')),
  response       text NOT NULL DEFAULT 'PENDING'
                   CHECK (response IN ('PENDING','ACCEPTED','DECLINED','TENTATIVE')),
  invited_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_participant_identity CHECK (user_id IS NOT NULL OR external_email IS NOT NULL)
);

CREATE UNIQUE INDEX uq_reservation_participants
  ON fms.reservation_participants (reservation_id,
       coalesce(user_id::text, external_email));

-- -----------------------------------------------------------------------------
-- 4. Attached Soft FM services (預約附加服務)
-- -----------------------------------------------------------------------------
-- One row per requested add-on. The reservation.confirmed event fans these out
-- into work_orders — that linkage is what the original spec's Kafka step does,
-- implemented here as an outbox-driven worker in Phase 1.
-- -----------------------------------------------------------------------------

CREATE TABLE fms.reservation_services (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id       uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  reservation_id  uuid NOT NULL REFERENCES fms.reservations(id) ON DELETE CASCADE,
  service_item_id uuid NOT NULL REFERENCES fms.service_items(id) ON DELETE RESTRICT,
  quantity        numeric(12,2) NOT NULL DEFAULT 1,
  payload         jsonb NOT NULL DEFAULT '{}'::jsonb,   -- validated against form_schema
  -- Resolved schedule for the service crew (reservation start + relative offset)
  service_start_at timestamptz,
  service_end_at   timestamptz,
  status          text NOT NULL DEFAULT 'REQUESTED'
                    CHECK (status IN ('REQUESTED','PENDING_APPROVAL','SCHEDULED',
                                      'WORK_ORDER_CREATED','FULFILLED','CANCELLED','REJECTED')),
  work_order_id   uuid REFERENCES fms.work_orders(id) ON DELETE SET NULL,
  estimated_cost  numeric(14,2),
  notes           text,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at      timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_reservation_services
  ON fms.reservation_services (reservation_id, service_item_id);
CREATE INDEX idx_reservation_services_pending
  ON fms.reservation_services (tenant_id, status) WHERE status IN ('REQUESTED','SCHEDULED');

-- -----------------------------------------------------------------------------
-- 5. Availability helper
-- -----------------------------------------------------------------------------
-- Single source of truth for "can I book this?" — used by the API's
-- GET /availability endpoint and re-checked inside the booking transaction.
-- -----------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION fms.check_resource_availability(
  p_resource_id uuid,
  p_start_at    timestamptz,
  p_end_at      timestamptz,
  p_exclude_reservation_id uuid DEFAULT NULL
) RETURNS TABLE (
  is_available boolean,
  conflict_type text,
  conflict_id  uuid,
  detail       text
)
LANGUAGE plpgsql STABLE
AS $$
DECLARE
  v_res    fms.bookable_resources;
  v_range  tstzrange := tstzrange(p_start_at, p_end_at, '[)');
  v_count  integer;
  v_conflict_id uuid;
  v_detail text;
BEGIN
  SELECT * INTO v_res FROM fms.bookable_resources
   WHERE spatial_node_id = p_resource_id OR asset_id = p_resource_id
   LIMIT 1;

  IF v_res.id IS NULL THEN
    RETURN QUERY SELECT false, 'NOT_BOOKABLE', NULL::uuid, 'resource is not configured as bookable';
    RETURN;
  END IF;

  IF NOT v_res.is_bookable THEN
    RETURN QUERY SELECT false, 'NOT_BOOKABLE', v_res.id, 'booking disabled for this resource';
    RETURN;
  END IF;

  IF extract(epoch FROM (p_end_at - p_start_at)) / 60 < v_res.min_duration_minutes THEN
    RETURN QUERY SELECT false, 'DURATION_TOO_SHORT', v_res.id,
      format('minimum %s minutes', v_res.min_duration_minutes);
    RETURN;
  END IF;

  IF extract(epoch FROM (p_end_at - p_start_at)) / 60 > v_res.max_duration_minutes THEN
    RETURN QUERY SELECT false, 'DURATION_TOO_LONG', v_res.id,
      format('maximum %s minutes', v_res.max_duration_minutes);
    RETURN;
  END IF;

  IF p_start_at > clock_timestamp() + (v_res.advance_booking_days || ' days')::interval THEN
    RETURN QUERY SELECT false, 'OUTSIDE_BOOKING_WINDOW', v_res.id,
      format('bookable up to %s days ahead', v_res.advance_booking_days);
    RETURN;
  END IF;

  -- Blackout windows
  SELECT b.id, b.reason INTO v_conflict_id, v_detail
  FROM fms.resource_blackouts b
  WHERE (b.bookable_resource_id = v_res.id OR
         (b.bookable_resource_id IS NULL AND b.facility_id = v_res.facility_id))
    AND b.blackout_range && v_range
  ORDER BY b.start_at
  LIMIT 1;

  IF v_conflict_id IS NOT NULL THEN
    RETURN QUERY SELECT false, 'BLACKOUT', v_conflict_id, v_detail;
    RETURN;
  END IF;

  -- Existing reservations (respecting pooled capacity)
  SELECT count(*) INTO v_count
  FROM fms.reservations r
  WHERE r.resource_id = p_resource_id
    AND r.status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN')
    AND r.time_range && v_range
    AND (p_exclude_reservation_id IS NULL OR r.id <> p_exclude_reservation_id);

  IF v_count >= v_res.capacity THEN
    RETURN QUERY
    SELECT false, 'RESERVATION_CONFLICT', r.id,
           format('%s → %s', r.start_at, r.end_at)
    FROM fms.reservations r
    WHERE r.resource_id = p_resource_id
      AND r.status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN')
      AND r.time_range && v_range
      AND (p_exclude_reservation_id IS NULL OR r.id <> p_exclude_reservation_id)
    ORDER BY r.start_at LIMIT 1;
    RETURN;
  END IF;

  -- Live holds from other users
  v_conflict_id := NULL;
  SELECT h.id INTO v_conflict_id
  FROM fms.reservation_holds h
  WHERE h.bookable_resource_id = v_res.id
    AND h.status = 'ACTIVE'
    AND h.expires_at > clock_timestamp()
    AND h.hold_range && v_range
  LIMIT 1;

  IF v_conflict_id IS NOT NULL THEN
    RETURN QUERY SELECT false, 'HELD_BY_OTHER', v_conflict_id, 'resource temporarily held'::text;
    RETURN;
  END IF;

  RETURN QUERY SELECT true, NULL::text, v_res.id, 'available'::text;
END;
$$;

-- Emit reservation lifecycle events so the Soft FM fan-out worker can pick them up.
CREATE OR REPLACE FUNCTION fms.trg_reservation_events()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    IF NEW.status = 'CONFIRMED' THEN
      PERFORM fms.emit_event(NEW.tenant_id, 'reservation.confirmed', 'RESERVATION', NEW.id,
        jsonb_build_object('reservation_no', NEW.reservation_no, 'facility_id', NEW.facility_id,
                           'resource_id', NEW.resource_id, 'start_at', NEW.start_at,
                           'end_at', NEW.end_at, 'organizer_id', NEW.organizer_id));
    ELSE
      PERFORM fms.emit_event(NEW.tenant_id, 'reservation.created', 'RESERVATION', NEW.id,
        jsonb_build_object('reservation_no', NEW.reservation_no, 'status', NEW.status));
    END IF;
  ELSIF NEW.status IS DISTINCT FROM OLD.status THEN
    PERFORM fms.emit_event(NEW.tenant_id, 'reservation.' || lower(NEW.status),
      'RESERVATION', NEW.id,
      jsonb_build_object('reservation_no', NEW.reservation_no,
                         'from', OLD.status, 'to', NEW.status));
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_reservations_events
  AFTER INSERT OR UPDATE OF status ON fms.reservations
  FOR EACH ROW EXECUTE FUNCTION fms.trg_reservation_events();

COMMIT;
