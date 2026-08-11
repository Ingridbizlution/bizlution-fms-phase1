-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 003: Spatial hierarchy, BIM linkage, asset registry (Hard FM)
-- =============================================================================

BEGIN;

-- -----------------------------------------------------------------------------
-- 1. Spatial node types (tenant-extensible so 影廳 / 教室 / Hot Desk coexist)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.spatial_node_types (
  id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id           uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,  -- NULL = platform default
  code                varchar(40) NOT NULL,       -- SITE, BUILDING, FLOOR, ZONE, ROOM, AUDITORIUM, DESK
  name                varchar(100) NOT NULL,
  level_hint          smallint NOT NULL DEFAULT 0,
  is_bookable         boolean NOT NULL DEFAULT false,
  is_leaf_default     boolean NOT NULL DEFAULT false,
  allowed_child_codes text[] NOT NULL DEFAULT '{}',
  icon                varchar(60),
  created_at          timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_spatial_node_types_code
  ON fms.spatial_node_types (coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid), lower(code));

-- -----------------------------------------------------------------------------
-- 2. BIM models (source of truth for the BIM 整合中心)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.bim_models (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id         uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id       uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  name              varchar(200) NOT NULL,
  source_format     text NOT NULL DEFAULT 'IFC'
                      CHECK (source_format IN ('IFC','RVT','NWD','DWG','GLTF','OTHER')),
  version_label     varchar(50),
  storage_bucket    varchar(80) NOT NULL DEFAULT 'fms-bim',
  storage_key       text NOT NULL,
  viewer_urn        text,                          -- e.g. APS/Forge translated URN
  discipline        varchar(40),                   -- ARCH, MEP, STRUCT
  status            text NOT NULL DEFAULT 'UPLOADED'
                      CHECK (status IN ('UPLOADED','PARSING','PARSED','PARSE_FAILED','SUPERSEDED')),
  element_count     integer NOT NULL DEFAULT 0,
  mapped_node_count integer NOT NULL DEFAULT 0,
  mapped_asset_count integer NOT NULL DEFAULT 0,
  -- Unresolved elements land here — this is what surfaced "B1 有 2 台未識別設備".
  unresolved_elements jsonb NOT NULL DEFAULT '[]'::jsonb,
  parse_report      jsonb NOT NULL DEFAULT '{}'::jsonb,
  parsed_at         timestamptz,
  uploaded_by       uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  created_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at        timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX idx_bim_models_facility ON fms.bim_models (facility_id, status);

-- -----------------------------------------------------------------------------
-- 3. Spatial nodes
-- -----------------------------------------------------------------------------

CREATE TABLE fms.spatial_nodes (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id    uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  parent_id      uuid REFERENCES fms.spatial_nodes(id) ON DELETE RESTRICT,
  node_type_code varchar(40) NOT NULL,
  code           varchar(60)  NOT NULL,
  name           varchar(200) NOT NULL,
  -- Materialised path relative to the facility root, e.g. F1.BLDG_A.FL03.R301
  node_path      ltree NOT NULL,
  depth          smallint NOT NULL DEFAULT 0,
  floor_level    integer,                     -- -1 = B1, 4 = 4F (sortable)
  floor_label    varchar(20),                 -- 'B1', '4F', 'RF'
  area_sqm       numeric(12,2),
  capacity       integer NOT NULL DEFAULT 0,
  -- Booking behaviour (a floor is not bookable, an auditorium or hot desk is)
  is_bookable    boolean NOT NULL DEFAULT false,
  is_active      boolean NOT NULL DEFAULT true,
  -- BIM linkage
  bim_model_id   uuid REFERENCES fms.bim_models(id) ON DELETE SET NULL,
  bim_element_id varchar(120),                -- IfcGloballyUniqueId
  geometry       jsonb NOT NULL DEFAULT '{}'::jsonb,   -- bbox / polygon / centroid for the 3D view
  -- Cached rollups maintained by a worker; never trust for billing.
  health_score   numeric(5,2),
  utilization_pct numeric(5,2),
  attributes     jsonb NOT NULL DEFAULT '{}'::jsonb,
  status         text NOT NULL DEFAULT 'AVAILABLE'
                   CHECK (status IN ('AVAILABLE','OCCUPIED','OUT_OF_SERVICE','UNDER_RENOVATION','RESERVED')),
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at     timestamptz
);

CREATE UNIQUE INDEX uq_spatial_nodes_facility_code
  ON fms.spatial_nodes (facility_id, lower(code)) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX uq_spatial_nodes_path
  ON fms.spatial_nodes (facility_id, node_path) WHERE deleted_at IS NULL;
CREATE INDEX idx_spatial_nodes_path_gist ON fms.spatial_nodes USING gist (node_path);
CREATE INDEX idx_spatial_nodes_parent ON fms.spatial_nodes (parent_id);
CREATE INDEX idx_spatial_nodes_tenant_facility ON fms.spatial_nodes (tenant_id, facility_id)
  WHERE deleted_at IS NULL;
CREATE INDEX idx_spatial_nodes_bookable ON fms.spatial_nodes (facility_id, is_bookable)
  WHERE is_bookable AND deleted_at IS NULL;
CREATE INDEX idx_spatial_nodes_bim ON fms.spatial_nodes (bim_model_id, bim_element_id)
  WHERE bim_element_id IS NOT NULL;

-- Keeps node_path / depth consistent with parent_id, including subtree moves.
CREATE OR REPLACE FUNCTION fms.trg_spatial_node_path()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_parent_path ltree;
  v_old_path    ltree;
BEGIN
  IF NEW.parent_id IS NULL THEN
    NEW.node_path := text2ltree(regexp_replace(NEW.code, '[^A-Za-z0-9_]', '_', 'g'));
  ELSE
    SELECT node_path INTO v_parent_path FROM fms.spatial_nodes WHERE id = NEW.parent_id;
    IF v_parent_path IS NULL THEN
      RAISE EXCEPTION 'parent spatial node % not found', NEW.parent_id USING ERRCODE = '23503';
    END IF;
    NEW.node_path := v_parent_path || text2ltree(regexp_replace(NEW.code, '[^A-Za-z0-9_]', '_', 'g'));
  END IF;

  NEW.depth := nlevel(NEW.node_path) - 1;

  IF TG_OP = 'UPDATE' THEN
    v_old_path := OLD.node_path;
    IF NEW.parent_id IS NOT NULL AND NEW.parent_id = OLD.id THEN
      RAISE EXCEPTION 'a spatial node cannot be its own parent' USING ERRCODE = '23514';
    END IF;
    IF v_old_path IS DISTINCT FROM NEW.node_path THEN
      -- Re-parent the whole subtree in one statement.
      UPDATE fms.spatial_nodes
         SET node_path = NEW.node_path || subpath(node_path, nlevel(v_old_path)),
             depth     = nlevel(NEW.node_path || subpath(node_path, nlevel(v_old_path))) - 1
       WHERE facility_id = NEW.facility_id
         AND node_path OPERATOR(public.<@) v_old_path
         AND id <> NEW.id;
    END IF;
  END IF;

  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_spatial_nodes_path
  BEFORE INSERT OR UPDATE OF parent_id, code ON fms.spatial_nodes
  FOR EACH ROW EXECUTE FUNCTION fms.trg_spatial_node_path();

CREATE TRIGGER trg_spatial_nodes_updated_at
  BEFORE UPDATE ON fms.spatial_nodes
  FOR EACH ROW EXECUTE FUNCTION fms.trg_set_updated_at();

-- -----------------------------------------------------------------------------
-- 4. Asset categories + equipment catalogue (設備技術中心)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.asset_categories (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,  -- NULL = platform taxonomy
  parent_id     uuid REFERENCES fms.asset_categories(id) ON DELETE RESTRICT,
  code          varchar(50) NOT NULL,
  name          varchar(150) NOT NULL,
  category_path ltree NOT NULL,
  -- Which discipline owns it — drives default team routing.
  domain        text NOT NULL DEFAULT 'GENERAL'
                  CHECK (domain IN ('GENERAL','HVAC','ELECTRICAL','PLUMBING','FIRE_SAFETY',
                                    'ELEVATOR','AV_PROJECTION','IT_NETWORK','SECURITY',
                                    'LAB','KITCHEN','PRODUCTION','ENVELOPE')),
  default_criticality text NOT NULL DEFAULT 'MEDIUM'
                  CHECK (default_criticality IN ('LOW','MEDIUM','HIGH','CRITICAL')),
  attributes    jsonb NOT NULL DEFAULT '{}'::jsonb,
  is_active     boolean NOT NULL DEFAULT true,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_asset_categories_code
  ON fms.asset_categories (coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid), lower(code));
CREATE INDEX idx_asset_categories_path ON fms.asset_categories USING gist (category_path);

CREATE OR REPLACE FUNCTION fms.trg_asset_category_path()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_parent_path ltree;
  v_label       text := regexp_replace(NEW.code, '[^A-Za-z0-9_]', '_', 'g');
BEGIN
  IF NEW.parent_id IS NULL THEN
    NEW.category_path := text2ltree(v_label);
  ELSE
    SELECT category_path INTO v_parent_path FROM fms.asset_categories WHERE id = NEW.parent_id;
    IF v_parent_path IS NULL THEN
      RAISE EXCEPTION 'parent asset category % not found', NEW.parent_id USING ERRCODE = '23503';
    END IF;
    NEW.category_path := v_parent_path || text2ltree(v_label);
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_asset_categories_path
  BEFORE INSERT OR UPDATE OF parent_id, code ON fms.asset_categories
  FOR EACH ROW EXECUTE FUNCTION fms.trg_asset_category_path();

-- Manufacturer/model catalogue. tenant_id NULL rows are the shared platform
-- catalogue every group client inherits; a tenant may add private models.
CREATE TABLE fms.asset_models (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id          uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,
  category_id        uuid NOT NULL REFERENCES fms.asset_categories(id) ON DELETE RESTRICT,
  manufacturer       varchar(120) NOT NULL,
  model_no           varchar(120) NOT NULL,
  name               varchar(200) NOT NULL,
  description        text,
  specifications     jsonb NOT NULL DEFAULT '{}'::jsonb,
  -- Interface/protocol capability used by the compatibility checker.
  supported_protocols text[] NOT NULL DEFAULT '{}',   -- MODBUS_TCP, BACNET_IP, MQTT, SNMP, HTTP
  power_rating_w     integer,
  expected_life_months integer,
  mtbf_hours         integer,
  spare_part_codes   text[] NOT NULL DEFAULT '{}',
  documentation_urls jsonb NOT NULL DEFAULT '[]'::jsonb,
  is_active          boolean NOT NULL DEFAULT true,
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at         timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_asset_models_key
  ON fms.asset_models (coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid),
                       lower(manufacturer), lower(model_no));
CREATE INDEX idx_asset_models_category ON fms.asset_models (category_id) WHERE is_active;

-- Model-to-model compatibility results (相容性測試)
CREATE TABLE fms.asset_model_compatibility (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,
  model_a_id     uuid NOT NULL REFERENCES fms.asset_models(id) ON DELETE CASCADE,
  model_b_id     uuid NOT NULL REFERENCES fms.asset_models(id) ON DELETE CASCADE,
  relation_type  text NOT NULL DEFAULT 'INTEROPERABLE'
                   CHECK (relation_type IN ('INTEROPERABLE','REPLACEMENT','ACCESSORY','CONFLICTING')),
  verdict        text NOT NULL DEFAULT 'UNKNOWN'
                   CHECK (verdict IN ('COMPATIBLE','CONDITIONAL','INCOMPATIBLE','UNKNOWN')),
  notes          text,
  verified_at    timestamptz,
  verified_by    uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  CONSTRAINT ck_amc_distinct CHECK (model_a_id <> model_b_id)
);

CREATE UNIQUE INDEX uq_asset_model_compat
  ON fms.asset_model_compatibility (model_a_id, model_b_id, relation_type);

-- -----------------------------------------------------------------------------
-- 5. Assets
-- -----------------------------------------------------------------------------

CREATE TABLE fms.assets (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id         uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id       uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  spatial_node_id   uuid REFERENCES fms.spatial_nodes(id) ON DELETE SET NULL,
  owner_org_id      uuid REFERENCES fms.organizations(id) ON DELETE SET NULL,
  asset_model_id    uuid REFERENCES fms.asset_models(id) ON DELETE SET NULL,
  category_id       uuid NOT NULL REFERENCES fms.asset_categories(id) ON DELETE RESTRICT,
  parent_asset_id   uuid REFERENCES fms.assets(id) ON DELETE SET NULL,  -- 子設備展開
  asset_code        varchar(60) NOT NULL,
  name              varchar(200) NOT NULL,
  serial_no         varchar(120),
  tag_rfid          varchar(120),
  qr_payload        text,
  criticality       text NOT NULL DEFAULT 'MEDIUM'
                      CHECK (criticality IN ('LOW','MEDIUM','HIGH','CRITICAL')),
  status            text NOT NULL DEFAULT 'OPERATIONAL'
                      CHECK (status IN ('PLANNED','IN_STORAGE','INSTALLING','OPERATIONAL',
                                        'DEGRADED','DOWN','UNDER_MAINTENANCE','DECOMMISSIONED')),
  -- Lifecycle / finance
  install_date      date,
  commission_date   date,
  warranty_end_date date,
  expected_replacement_date date,
  purchase_cost     numeric(14,2),
  currency          char(3),
  vendor_name       varchar(150),
  service_contract_no varchar(80),
  custodian_user_id uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  -- Operational telemetry rollups (written by the IoT ingest worker)
  health_score      numeric(5,2),
  last_telemetry_at timestamptz,
  runtime_hours     numeric(12,2),
  -- BIM linkage
  bim_model_id      uuid REFERENCES fms.bim_models(id) ON DELETE SET NULL,
  bim_element_id    varchar(120),
  specifications    jsonb NOT NULL DEFAULT '{}'::jsonb,
  attributes        jsonb NOT NULL DEFAULT '{}'::jsonb,
  version           integer NOT NULL DEFAULT 1,
  created_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at        timestamptz
);

CREATE UNIQUE INDEX uq_assets_tenant_code
  ON fms.assets (tenant_id, lower(asset_code)) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX uq_assets_serial
  ON fms.assets (tenant_id, lower(serial_no)) WHERE serial_no IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX idx_assets_facility_status ON fms.assets (facility_id, status) WHERE deleted_at IS NULL;
CREATE INDEX idx_assets_node ON fms.assets (spatial_node_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_assets_category ON fms.assets (category_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_assets_parent ON fms.assets (parent_asset_id) WHERE parent_asset_id IS NOT NULL;
CREATE INDEX idx_assets_criticality ON fms.assets (tenant_id, criticality, status) WHERE deleted_at IS NULL;
CREATE INDEX idx_assets_specs_gin ON fms.assets USING gin (specifications jsonb_path_ops);
CREATE INDEX idx_assets_name_trgm ON fms.assets USING gin (name gin_trgm_ops);

CREATE TRIGGER trg_assets_updated_at
  BEFORE UPDATE ON fms.assets FOR EACH ROW EXECUTE FUNCTION fms.trg_bump_version();
CREATE TRIGGER trg_assets_freeze_tenant
  BEFORE UPDATE ON fms.assets FOR EACH ROW EXECUTE FUNCTION fms.trg_freeze_tenant_id();

-- Cross-system dependency graph (跨系統依賴): e.g. projector DEPENDS_ON UPS-B1.
CREATE TABLE fms.asset_relations (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  from_asset_id  uuid NOT NULL REFERENCES fms.assets(id) ON DELETE CASCADE,
  to_asset_id    uuid NOT NULL REFERENCES fms.assets(id) ON DELETE CASCADE,
  relation_type  text NOT NULL
                   CHECK (relation_type IN ('DEPENDS_ON','FEEDS','BACKUP_OF','CONTROLS',
                                            'MONITORS','CONNECTED_TO')),
  impact_level   text NOT NULL DEFAULT 'MEDIUM'
                   CHECK (impact_level IN ('LOW','MEDIUM','HIGH','CRITICAL')),
  notes          text,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_asset_relations_distinct CHECK (from_asset_id <> to_asset_id)
);

CREATE UNIQUE INDEX uq_asset_relations
  ON fms.asset_relations (from_asset_id, to_asset_id, relation_type);
CREATE INDEX idx_asset_relations_to ON fms.asset_relations (to_asset_id);

CREATE TABLE fms.asset_status_history (
  id           bigserial PRIMARY KEY,
  tenant_id    uuid NOT NULL,
  asset_id     uuid NOT NULL REFERENCES fms.assets(id) ON DELETE CASCADE,
  from_status  varchar(30),
  to_status    varchar(30) NOT NULL,
  reason       varchar(200),
  work_order_id uuid,
  changed_by   uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  changed_at   timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX idx_asset_status_history_asset ON fms.asset_status_history (asset_id, changed_at DESC);

CREATE OR REPLACE FUNCTION fms.trg_asset_status_history()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF NEW.status IS DISTINCT FROM OLD.status THEN
    INSERT INTO fms.asset_status_history (tenant_id, asset_id, from_status, to_status, changed_by)
    VALUES (NEW.tenant_id, NEW.id, OLD.status, NEW.status, fms.current_user_id());
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_assets_status_history
  AFTER UPDATE OF status ON fms.assets
  FOR EACH ROW EXECUTE FUNCTION fms.trg_asset_status_history();

-- -----------------------------------------------------------------------------
-- 6. Meters (runtime hours, lamp hours, kWh) — drives meter-based PM
-- -----------------------------------------------------------------------------

CREATE TABLE fms.asset_meters (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  asset_id      uuid NOT NULL REFERENCES fms.assets(id) ON DELETE CASCADE,
  meter_code    varchar(50) NOT NULL,        -- RUNTIME_HOURS, LAMP_HOURS, KWH, FILTER_DAYS
  name          varchar(120) NOT NULL,
  unit          varchar(20) NOT NULL,
  reading_type  text NOT NULL DEFAULT 'CUMULATIVE'
                  CHECK (reading_type IN ('CUMULATIVE','GAUGE','DELTA')),
  last_value    numeric(18,4),
  last_read_at  timestamptz,
  rollover_at   numeric(18,4),
  is_active     boolean NOT NULL DEFAULT true,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_asset_meters ON fms.asset_meters (asset_id, lower(meter_code));

CREATE TABLE fms.asset_meter_readings (
  id            bigserial,
  tenant_id     uuid NOT NULL,
  asset_meter_id uuid NOT NULL,
  reading_at    timestamptz NOT NULL,
  value         numeric(18,4) NOT NULL,
  source        text NOT NULL DEFAULT 'MANUAL'
                  CHECK (source IN ('MANUAL','IOT','IMPORT','ESTIMATED')),
  recorded_by   uuid,
  PRIMARY KEY (reading_at, id)
) PARTITION BY RANGE (reading_at);

CREATE TABLE fms.asset_meter_readings_2026m07 PARTITION OF fms.asset_meter_readings
  FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE fms.asset_meter_readings_2026m08 PARTITION OF fms.asset_meter_readings
  FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE fms.asset_meter_readings_default PARTITION OF fms.asset_meter_readings DEFAULT;

CREATE INDEX idx_meter_readings_meter_time
  ON fms.asset_meter_readings (asset_meter_id, reading_at DESC);

COMMIT;
