-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 006: Devices & telemetry ingest, alarm engine with automatic work
--                order linkage, notification delivery
-- =============================================================================
-- Phase 1 keeps ingest in PostgreSQL with monthly partitions — adequate up to a
-- few thousand points at 1-minute resolution. The write path is deliberately
-- isolated behind fms.ingest_telemetry() so Phase 4 can swap in the Rust
-- MQTT broker / TimescaleDB without touching any business query.
-- =============================================================================

BEGIN;

-- -----------------------------------------------------------------------------
-- 1. Gateways and devices
-- -----------------------------------------------------------------------------

CREATE TABLE fms.iot_gateways (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id   uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  code          varchar(60) NOT NULL,
  name          varchar(150) NOT NULL,
  api_client_id uuid REFERENCES fms.api_clients(id) ON DELETE SET NULL,
  protocol      text NOT NULL DEFAULT 'MQTT'
                  CHECK (protocol IN ('MQTT','HTTP','MODBUS_TCP','BACNET_IP','OPC_UA','SNMP')),
  endpoint      varchar(250),
  heartbeat_interval_seconds integer NOT NULL DEFAULT 60,
  last_heartbeat_at timestamptz,
  status        text NOT NULL DEFAULT 'UNKNOWN'
                  CHECK (status IN ('ONLINE','OFFLINE','DEGRADED','UNKNOWN','DISABLED')),
  firmware_version varchar(60),
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_iot_gateways_code ON fms.iot_gateways (tenant_id, lower(code));

CREATE TABLE fms.devices (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id       uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id     uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  gateway_id      uuid REFERENCES fms.iot_gateways(id) ON DELETE SET NULL,
  -- A device usually monitors an asset, sometimes only a space (occupancy sensor).
  asset_id        uuid REFERENCES fms.assets(id) ON DELETE SET NULL,
  spatial_node_id uuid REFERENCES fms.spatial_nodes(id) ON DELETE SET NULL,
  device_code     varchar(80) NOT NULL,
  name            varchar(150) NOT NULL,
  device_type     text NOT NULL
                    CHECK (device_type IN ('SENSOR','METER','CONTROLLER','ACCESS_PANEL',
                                           'CAMERA','OCCUPANCY','ENVIRONMENT','GATEWAY')),
  -- MQTT topic or Modbus unit/register base
  address         varchar(250),
  heartbeat_interval_seconds integer NOT NULL DEFAULT 300,
  last_seen_at    timestamptz,
  status          text NOT NULL DEFAULT 'UNKNOWN'
                    CHECK (status IN ('ONLINE','OFFLINE','FAULT','MAINTENANCE','UNKNOWN','DISABLED')),
  -- Grace period before an OFFLINE device raises a DEVICE_OFFLINE alarm.
  offline_alarm_after_seconds integer NOT NULL DEFAULT 900,
  attributes      jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at      timestamptz,
  CONSTRAINT ck_device_target CHECK (asset_id IS NOT NULL OR spatial_node_id IS NOT NULL)
);

CREATE UNIQUE INDEX uq_devices_code ON fms.devices (tenant_id, lower(device_code))
  WHERE deleted_at IS NULL;
CREATE INDEX idx_devices_asset ON fms.devices (asset_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_devices_node ON fms.devices (spatial_node_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_devices_stale ON fms.devices (last_seen_at)
  WHERE status <> 'DISABLED' AND deleted_at IS NULL;

CREATE TABLE fms.telemetry_points (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  device_id      uuid NOT NULL REFERENCES fms.devices(id) ON DELETE CASCADE,
  point_code     varchar(60) NOT NULL,       -- TEMP_SUPPLY, PRESSURE_DP, LAMP_HOURS, CO2
  name           varchar(150) NOT NULL,
  data_type      text NOT NULL DEFAULT 'NUMBER'
                   CHECK (data_type IN ('NUMBER','BOOLEAN','STRING','ENUM')),
  unit           varchar(20),
  scale_factor   numeric(18,6) NOT NULL DEFAULT 1,
  offset_value   numeric(18,6) NOT NULL DEFAULT 0,
  valid_min      numeric(18,4),
  valid_max      numeric(18,4),
  -- Feeds an asset meter so meter-based PM stays in sync with telemetry.
  asset_meter_id uuid REFERENCES fms.asset_meters(id) ON DELETE SET NULL,
  is_active      boolean NOT NULL DEFAULT true,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_telemetry_points ON fms.telemetry_points (device_id, lower(point_code));

-- -----------------------------------------------------------------------------
-- 2. Raw telemetry (partitioned, append only)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.telemetry_readings (
  id                 bigserial,
  tenant_id          uuid NOT NULL,
  telemetry_point_id uuid NOT NULL,
  observed_at        timestamptz NOT NULL,
  value_num          numeric(18,4),
  value_bool         boolean,
  value_text         text,
  quality            text NOT NULL DEFAULT 'GOOD'
                       CHECK (quality IN ('GOOD','UNCERTAIN','BAD','STALE')),
  ingested_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (observed_at, id)
) PARTITION BY RANGE (observed_at);

CREATE TABLE fms.telemetry_readings_2026m07 PARTITION OF fms.telemetry_readings
  FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE fms.telemetry_readings_2026m08 PARTITION OF fms.telemetry_readings
  FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE fms.telemetry_readings_default PARTITION OF fms.telemetry_readings DEFAULT;

CREATE INDEX idx_telemetry_readings_point_time
  ON fms.telemetry_readings (telemetry_point_id, observed_at DESC);

-- Latest value per point — hot path for dashboards, kept small.
CREATE TABLE fms.telemetry_latest (
  telemetry_point_id uuid PRIMARY KEY REFERENCES fms.telemetry_points(id) ON DELETE CASCADE,
  tenant_id          uuid NOT NULL,
  device_id          uuid NOT NULL,
  observed_at        timestamptz NOT NULL,
  value_num          numeric(18,4),
  value_bool         boolean,
  value_text         text,
  quality            varchar(12) NOT NULL DEFAULT 'GOOD',
  updated_at         timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX idx_telemetry_latest_device ON fms.telemetry_latest (device_id);

-- -----------------------------------------------------------------------------
-- 3. Alarm rules and alarms
-- -----------------------------------------------------------------------------

CREATE TABLE fms.alarm_rules (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id          uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id        uuid REFERENCES fms.facilities(id) ON DELETE CASCADE,
  code               varchar(60) NOT NULL,
  name               varchar(200) NOT NULL,
  description        text,
  -- Scope: a specific point, or every point with this code in a category/facility.
  telemetry_point_id uuid REFERENCES fms.telemetry_points(id) ON DELETE CASCADE,
  point_code         varchar(60),
  asset_category_id  uuid REFERENCES fms.asset_categories(id) ON DELETE CASCADE,
  rule_type          text NOT NULL DEFAULT 'THRESHOLD'
                       CHECK (rule_type IN ('THRESHOLD','RATE_OF_CHANGE','DEVIATION',
                                            'FLATLINE','DEVICE_OFFLINE','BOOLEAN_STATE','COMPOSITE')),
  -- {"op":">","value":28,"for_seconds":300} | {"op":"outside","min":18,"max":26}
  condition          jsonb NOT NULL,
  severity           text NOT NULL DEFAULT 'WARNING'
                       CHECK (severity IN ('INFO','WARNING','MINOR','MAJOR','CRITICAL')),
  debounce_seconds   integer NOT NULL DEFAULT 60,
  auto_clear         boolean NOT NULL DEFAULT true,
  -- Automatic work order creation — this is the switch that closes the gap
  -- between the alarm log and the work order backlog.
  auto_create_work_order boolean NOT NULL DEFAULT false,
  wo_work_order_type varchar(20) DEFAULT 'CORRECTIVE',
  wo_service_item_id uuid REFERENCES fms.service_items(id) ON DELETE SET NULL,
  wo_maintenance_template_id uuid REFERENCES fms.maintenance_templates(id) ON DELETE SET NULL,
  wo_priority        text DEFAULT 'HIGH'
                       CHECK (wo_priority IN ('LOW','MEDIUM','HIGH','URGENT','CRITICAL')),
  wo_team_id         uuid REFERENCES fms.teams(id) ON DELETE SET NULL,
  wo_sla_policy_id   uuid REFERENCES fms.sla_policies(id) ON DELETE SET NULL,
  -- Do not open a second work order while an open one already exists for the asset.
  dedupe_window_minutes integer NOT NULL DEFAULT 120,
  notify_role_codes  text[] NOT NULL DEFAULT '{}',
  is_active          boolean NOT NULL DEFAULT true,
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_alarm_rule_scope CHECK (
    telemetry_point_id IS NOT NULL OR point_code IS NOT NULL OR rule_type = 'DEVICE_OFFLINE'
  )
);

CREATE UNIQUE INDEX uq_alarm_rules_code ON fms.alarm_rules (tenant_id, lower(code));
CREATE INDEX idx_alarm_rules_active ON fms.alarm_rules (tenant_id, facility_id) WHERE is_active;

CREATE TABLE fms.alarms (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id          uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id        uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  alarm_no           varchar(40) NOT NULL,
  alarm_rule_id      uuid REFERENCES fms.alarm_rules(id) ON DELETE SET NULL,
  device_id          uuid REFERENCES fms.devices(id) ON DELETE SET NULL,
  telemetry_point_id uuid REFERENCES fms.telemetry_points(id) ON DELETE SET NULL,
  asset_id           uuid REFERENCES fms.assets(id) ON DELETE SET NULL,
  spatial_node_id    uuid REFERENCES fms.spatial_nodes(id) ON DELETE SET NULL,
  source             text NOT NULL DEFAULT 'RULE_ENGINE'
                       CHECK (source IN ('RULE_ENGINE','BMS','MANUAL','EXTERNAL_API','SELF_TEST')),
  severity           text NOT NULL DEFAULT 'WARNING'
                       CHECK (severity IN ('INFO','WARNING','MINOR','MAJOR','CRITICAL')),
  status             text NOT NULL DEFAULT 'ACTIVE'
                       CHECK (status IN ('ACTIVE','ACKNOWLEDGED','SUPPRESSED','CLEARED','CLOSED')),
  message            varchar(400) NOT NULL,
  trigger_value      numeric(18,4),
  threshold_value    numeric(18,4),
  occurrence_count   integer NOT NULL DEFAULT 1,
  first_seen_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  last_seen_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  acknowledged_at    timestamptz,
  acknowledged_by    uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  cleared_at         timestamptz,
  closed_at          timestamptz,
  suppressed_until   timestamptz,
  -- The linkage that turns an alarm log into actionable work.
  work_order_id      uuid REFERENCES fms.work_orders(id) ON DELETE SET NULL,
  work_order_created_at timestamptz,
  context            jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at         timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_alarms_no ON fms.alarms (tenant_id, alarm_no);
CREATE INDEX idx_alarms_open ON fms.alarms (tenant_id, facility_id, severity, first_seen_at DESC)
  WHERE status IN ('ACTIVE','ACKNOWLEDGED');
CREATE INDEX idx_alarms_asset ON fms.alarms (asset_id, first_seen_at DESC);
CREATE INDEX idx_alarms_device ON fms.alarms (device_id, first_seen_at DESC);
-- Alarms that should have produced a work order but have not — the reconciliation
-- query behind the "unlinked alarms" health check.
CREATE INDEX idx_alarms_unlinked ON fms.alarms (tenant_id, first_seen_at)
  WHERE work_order_id IS NULL AND status IN ('ACTIVE','ACKNOWLEDGED');
-- Only one open alarm per rule+point at a time (dedupe at the storage layer).
CREATE UNIQUE INDEX uq_alarms_open_per_point
  ON fms.alarms (alarm_rule_id, coalesce(telemetry_point_id, '00000000-0000-0000-0000-000000000000'::uuid))
  WHERE status IN ('ACTIVE','ACKNOWLEDGED');

ALTER TABLE fms.work_orders
  ADD CONSTRAINT fk_work_orders_alarm
  FOREIGN KEY (alarm_id) REFERENCES fms.alarms(id) ON DELETE SET NULL;

-- -----------------------------------------------------------------------------
-- 4. Ingest + alarm evaluation entry points
-- -----------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION fms.ingest_telemetry(
  p_telemetry_point_id uuid,
  p_observed_at        timestamptz,
  p_value_num          numeric DEFAULT NULL,
  p_value_bool         boolean DEFAULT NULL,
  p_value_text         text DEFAULT NULL,
  p_quality            text DEFAULT 'GOOD'
) RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
  v_point fms.telemetry_points;
BEGIN
  SELECT * INTO v_point FROM fms.telemetry_points WHERE id = p_telemetry_point_id;
  IF v_point.id IS NULL THEN
    RAISE EXCEPTION 'telemetry point % not found', p_telemetry_point_id USING ERRCODE = 'P0002';
  END IF;

  INSERT INTO fms.telemetry_readings
    (tenant_id, telemetry_point_id, observed_at, value_num, value_bool, value_text, quality)
  VALUES
    (v_point.tenant_id, p_telemetry_point_id, p_observed_at,
     p_value_num, p_value_bool, p_value_text, p_quality);

  INSERT INTO fms.telemetry_latest
    (telemetry_point_id, tenant_id, device_id, observed_at, value_num, value_bool, value_text, quality)
  VALUES
    (p_telemetry_point_id, v_point.tenant_id, v_point.device_id, p_observed_at,
     p_value_num, p_value_bool, p_value_text, p_quality)
  ON CONFLICT (telemetry_point_id) DO UPDATE
    SET observed_at = EXCLUDED.observed_at,
        value_num   = EXCLUDED.value_num,
        value_bool  = EXCLUDED.value_bool,
        value_text  = EXCLUDED.value_text,
        quality     = EXCLUDED.quality,
        updated_at  = clock_timestamp()
    WHERE fms.telemetry_latest.observed_at <= EXCLUDED.observed_at;

  UPDATE fms.devices
     SET last_seen_at = greatest(coalesce(last_seen_at, p_observed_at), p_observed_at),
         status = CASE WHEN status IN ('OFFLINE','UNKNOWN') THEN 'ONLINE' ELSE status END
   WHERE id = v_point.device_id;

  -- Keep the asset meter aligned so meter-triggered PM works off live data.
  IF v_point.asset_meter_id IS NOT NULL AND p_value_num IS NOT NULL THEN
    UPDATE fms.asset_meters
       SET last_value = p_value_num, last_read_at = p_observed_at
     WHERE id = v_point.asset_meter_id
       AND (last_read_at IS NULL OR last_read_at <= p_observed_at);

    INSERT INTO fms.asset_meter_readings
      (tenant_id, asset_meter_id, reading_at, value, source)
    VALUES (v_point.tenant_id, v_point.asset_meter_id, p_observed_at, p_value_num, 'IOT');
  END IF;
END;
$$;

COMMENT ON FUNCTION fms.ingest_telemetry IS
  'Single write path for device data. Phase 4 replaces the internals with the Rust broker; callers and downstream queries stay unchanged.';

-- Raise or refresh an alarm, and (when the rule says so) open the work order in
-- the same transaction. Deduplicated per rule+point and per open work order.
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

  -- Refresh an existing open alarm rather than creating a duplicate.
  SELECT * INTO v_alarm
    FROM fms.alarms
   WHERE alarm_rule_id = p_alarm_rule_id
     AND coalesce(telemetry_point_id, '00000000-0000-0000-0000-000000000000'::uuid)
         = coalesce(p_telemetry_point_id, '00000000-0000-0000-0000-000000000000'::uuid)
     AND status IN ('ACTIVE','ACKNOWLEDGED')
   FOR UPDATE;

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

  -- Automatic work order creation with de-duplication.
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
        format('Auto-generated from alarm rule %s (value %s)', v_rule.code, p_trigger_value),
        v_asset_id, v_node_id, v_rule.wo_service_item_id, v_alarm_id,
        coalesce(v_rule.wo_priority, 'HIGH'), 'SUBMITTED',
        v_rule.wo_team_id, v_rule.wo_sla_policy_id)
      RETURNING id INTO v_wo_id;

      PERFORM fms.emit_event(v_rule.tenant_id, 'work_order.created', 'WORK_ORDER', v_wo_id,
        jsonb_build_object('source', 'IOT_ALARM', 'alarm_id', v_alarm_id));
    END IF;

    UPDATE fms.alarms
       SET work_order_id = v_wo_id,
           work_order_created_at = coalesce(work_order_created_at, clock_timestamp())
     WHERE id = v_alarm_id AND work_order_id IS NULL;
  END IF;

  RETURN v_alarm_id;
END;
$$;

-- -----------------------------------------------------------------------------
-- 5. Notifications
-- -----------------------------------------------------------------------------

CREATE TABLE fms.notification_templates (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,
  code          varchar(60) NOT NULL,
  channel       text NOT NULL CHECK (channel IN ('EMAIL','SMS','PUSH','WEBHOOK','IN_APP','LINE')),
  locale        varchar(16) NOT NULL DEFAULT 'zh-TW',
  subject_template text,
  body_template text NOT NULL,
  is_active     boolean NOT NULL DEFAULT true
);

CREATE UNIQUE INDEX uq_notification_templates
  ON fms.notification_templates (coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid),
                                 lower(code), channel, locale);

CREATE TABLE fms.notifications (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  recipient_user_id uuid REFERENCES fms.users(id) ON DELETE CASCADE,
  recipient_address varchar(250),
  channel       text NOT NULL CHECK (channel IN ('EMAIL','SMS','PUSH','WEBHOOK','IN_APP','LINE')),
  template_code varchar(60),
  subject       varchar(250),
  body          text NOT NULL,
  entity_type   varchar(40),
  entity_id     uuid,
  priority      text NOT NULL DEFAULT 'NORMAL' CHECK (priority IN ('LOW','NORMAL','HIGH')),
  status        text NOT NULL DEFAULT 'QUEUED'
                  CHECK (status IN ('QUEUED','SENDING','SENT','FAILED','SUPPRESSED','READ')),
  attempt_count smallint NOT NULL DEFAULT 0,
  last_error    text,
  scheduled_for timestamptz NOT NULL DEFAULT clock_timestamp(),
  sent_at       timestamptz,
  read_at       timestamptz,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX idx_notifications_queue ON fms.notifications (status, scheduled_for)
  WHERE status IN ('QUEUED','FAILED');
CREATE INDEX idx_notifications_inbox ON fms.notifications (recipient_user_id, created_at DESC)
  WHERE channel = 'IN_APP';

COMMIT;
