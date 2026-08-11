-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 001: Foundation (schema / extensions / session context / shared infra)
-- Target: PostgreSQL 16+ (validated on PG17, Supabase compatible)
-- =============================================================================
-- Conventions
--   * All application objects live in schema "fms"; Supabase's auth/storage/public
--     schemas are left untouched.
--   * Every tenant-scoped table carries tenant_id UUID NOT NULL and is protected
--     by Row-Level Security (see 007_rls_policies.sql).
--   * Lookup / catalogue tables allow tenant_id IS NULL to represent
--     platform-global (system) rows shared by all tenants.
--   * Soft delete via deleted_at; unique indexes are partial (WHERE deleted_at IS NULL).
--   * Status / type columns use TEXT + CHECK rather than native ENUM so that
--     values can be extended without an exclusive table lock.
-- =============================================================================

BEGIN;

CREATE SCHEMA IF NOT EXISTS fms;

-- Extensions MUST be pinned to public with an explicit SCHEMA clause. Without it
-- they land in the first schema on search_path, and docker/postgres/initdb/00-roles.sql
-- sets `search_path = fms, public` for fms_owner — so they would land in fms and
-- every explicit OPERATOR(public.<@) reference in 002/003/011 would fail with
-- "operator does not exist: ltree public.<@ ltree".
CREATE EXTENSION IF NOT EXISTS pgcrypto   SCHEMA public;  -- gen_random_uuid(), digest()
CREATE EXTENSION IF NOT EXISTS ltree      SCHEMA public;  -- hierarchical paths (org / spatial / category)
CREATE EXTENSION IF NOT EXISTS btree_gist SCHEMA public;  -- uuid = + range && exclusion constraints
CREATE EXTENSION IF NOT EXISTS pg_trgm    SCHEMA public;  -- fuzzy search on codes / names
CREATE EXTENSION IF NOT EXISTS citext     SCHEMA public;  -- case-insensitive email / username

-- -----------------------------------------------------------------------------
-- 1. Request-scoped session context
-- -----------------------------------------------------------------------------
-- The API layer MUST execute, on every connection checkout, inside the request
-- transaction:
--     SELECT fms.set_context(:tenant_id, :user_id, :role_codes);
-- SET LOCAL semantics guarantee the value is discarded when the transaction ends,
-- which is what makes connection pooling safe.
-- -----------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION fms.current_tenant_id()
RETURNS uuid
LANGUAGE sql STABLE PARALLEL SAFE
AS $$
  SELECT NULLIF(current_setting('app.tenant_id', true), '')::uuid;
$$;

CREATE OR REPLACE FUNCTION fms.current_user_id()
RETURNS uuid
LANGUAGE sql STABLE PARALLEL SAFE
AS $$
  SELECT NULLIF(current_setting('app.user_id', true), '')::uuid;
$$;

-- Platform operators (support console, migration runner, ETL) set app.is_platform
-- to 'on'. This is the ONLY sanctioned way to read across tenants.
CREATE OR REPLACE FUNCTION fms.is_platform_context()
RETURNS boolean
LANGUAGE sql STABLE PARALLEL SAFE
AS $$
  SELECT coalesce(current_setting('app.is_platform', true), 'off') = 'on';
$$;

CREATE OR REPLACE FUNCTION fms.set_context(
  p_tenant_id  uuid,
  p_user_id    uuid DEFAULT NULL,
  p_is_platform boolean DEFAULT false
) RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
  PERFORM set_config('app.tenant_id',   coalesce(p_tenant_id::text, ''), true);
  PERFORM set_config('app.user_id',     coalesce(p_user_id::text, ''),   true);
  PERFORM set_config('app.is_platform', CASE WHEN p_is_platform THEN 'on' ELSE 'off' END, true);
END;
$$;

COMMENT ON FUNCTION fms.set_context(uuid, uuid, boolean) IS
  'Sets request-scoped tenant/user context (transaction-local). Called by the RLS middleware before any business query.';

-- -----------------------------------------------------------------------------
-- 2. Shared triggers
-- -----------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION fms.trg_set_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.updated_at := clock_timestamp();
  RETURN NEW;
END;
$$;

-- tenant_id must never change after insert: a mis-scoped UPDATE would silently
-- move a row across the isolation boundary.
CREATE OR REPLACE FUNCTION fms.trg_freeze_tenant_id()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id THEN
    RAISE EXCEPTION 'tenant_id is immutable (table %, row %)', TG_TABLE_NAME, OLD.id
      USING ERRCODE = '42501';
  END IF;
  RETURN NEW;
END;
$$;

-- Optimistic concurrency: bump version on every UPDATE so the API can enforce
-- If-Match / ETag on state transitions.
CREATE OR REPLACE FUNCTION fms.trg_bump_version()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.version := OLD.version + 1;
  NEW.updated_at := clock_timestamp();
  RETURN NEW;
END;
$$;

-- Convenience installer used by later migrations.
CREATE OR REPLACE FUNCTION fms.install_standard_triggers(p_table regclass)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
  v_name text := replace(p_table::text, 'fms.', '');
BEGIN
  EXECUTE format(
    'CREATE TRIGGER %I BEFORE UPDATE ON %s FOR EACH ROW EXECUTE FUNCTION fms.trg_set_updated_at()',
    'trg_' || v_name || '_updated_at', p_table);

  IF EXISTS (
    SELECT 1 FROM information_schema.columns
    WHERE table_schema = 'fms' AND table_name = v_name AND column_name = 'tenant_id'
  ) THEN
    EXECUTE format(
      'CREATE TRIGGER %I BEFORE UPDATE ON %s FOR EACH ROW EXECUTE FUNCTION fms.trg_freeze_tenant_id()',
      'trg_' || v_name || '_freeze_tenant', p_table);
  END IF;
END;
$$;

-- -----------------------------------------------------------------------------
-- 3. Tenant  →  Organization  →  Facility
-- -----------------------------------------------------------------------------

CREATE TABLE fms.tenants (
  id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  code                varchar(50)  NOT NULL,
  name                varchar(200) NOT NULL,
  legal_name          varchar(200),
  -- Group-client vertical, drives which BFF / default catalogue is provisioned.
  industry            text NOT NULL DEFAULT 'GENERIC'
                        CHECK (industry IN ('GENERIC','CINEMA','EDUCATION','ENTERPRISE',
                                            'MANUFACTURING','HEALTHCARE','RETAIL','PUBLIC_SECTOR')),
  -- Shared pool by default; DEDICATED reserved for high-compliance group clients
  -- (same codebase, separate database instance) — see spec §2.4.
  isolation_mode      text NOT NULL DEFAULT 'SHARED'
                        CHECK (isolation_mode IN ('SHARED','DEDICATED')),
  plan_tier           text NOT NULL DEFAULT 'STANDARD'
                        CHECK (plan_tier IN ('TRIAL','STANDARD','PROFESSIONAL','ENTERPRISE')),
  status              text NOT NULL DEFAULT 'ACTIVE'
                        CHECK (status IN ('PROVISIONING','ACTIVE','SUSPENDED','TERMINATED')),
  default_timezone    varchar(64) NOT NULL DEFAULT 'Asia/Taipei',
  default_locale      varchar(16) NOT NULL DEFAULT 'zh-TW',
  default_currency    char(3)     NOT NULL DEFAULT 'TWD',
  -- Per-tenant switches consumed by the API layer (module on/off, workflow toggles).
  feature_flags       jsonb NOT NULL DEFAULT '{}'::jsonb,
  settings            jsonb NOT NULL DEFAULT '{}'::jsonb,
  -- Contract / quota metadata used by the gateway rate limiter.
  contract_start_date date,
  contract_end_date   date,
  quota_api_rps       integer NOT NULL DEFAULT 50,
  quota_assets        integer,
  quota_users         integer,
  created_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at          timestamptz
);

CREATE UNIQUE INDEX uq_tenants_code ON fms.tenants (lower(code)) WHERE deleted_at IS NULL;
CREATE INDEX idx_tenants_status ON fms.tenants (status) WHERE deleted_at IS NULL;

COMMENT ON TABLE fms.tenants IS 'Top isolation boundary — one row per group client (e.g. 威秀影城集團, 台積電).';

-- Organizations form a tree so a group client can model
-- 集團 → 子公司 → 事業部 → 部門 without schema changes.
CREATE TABLE fms.organizations (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  parent_id     uuid REFERENCES fms.organizations(id) ON DELETE RESTRICT,
  code          varchar(50)  NOT NULL,
  name          varchar(200) NOT NULL,
  org_type      text NOT NULL DEFAULT 'DEPARTMENT'
                  CHECK (org_type IN ('GROUP','COMPANY','BUSINESS_UNIT','REGION','DEPARTMENT','TEAM')),
  -- Materialised path, e.g. GRP.TW.NORTH.FAB1 — enables single-index subtree rollups.
  org_path      ltree NOT NULL,
  cost_center   varchar(50),
  manager_user_id uuid,
  attributes    jsonb NOT NULL DEFAULT '{}'::jsonb,
  status        text NOT NULL DEFAULT 'ACTIVE'
                  CHECK (status IN ('ACTIVE','INACTIVE')),
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at    timestamptz
);

CREATE UNIQUE INDEX uq_organizations_tenant_code
  ON fms.organizations (tenant_id, lower(code)) WHERE deleted_at IS NULL;
CREATE INDEX idx_organizations_path ON fms.organizations USING gist (org_path);
CREATE INDEX idx_organizations_parent ON fms.organizations (parent_id);
CREATE INDEX idx_organizations_tenant ON fms.organizations (tenant_id) WHERE deleted_at IS NULL;

-- Maintains org_path from parent_id + code, and re-paths the subtree on moves.
CREATE OR REPLACE FUNCTION fms.trg_organization_path()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_parent_path ltree;
  v_old_path    ltree;
  v_label       text := regexp_replace(NEW.code, '[^A-Za-z0-9_]', '_', 'g');
BEGIN
  IF NEW.parent_id IS NULL THEN
    NEW.org_path := text2ltree(v_label);
  ELSE
    SELECT org_path INTO v_parent_path FROM fms.organizations WHERE id = NEW.parent_id;
    IF v_parent_path IS NULL THEN
      RAISE EXCEPTION 'parent organization % not found', NEW.parent_id USING ERRCODE = '23503';
    END IF;
    NEW.org_path := v_parent_path || text2ltree(v_label);
  END IF;

  IF TG_OP = 'UPDATE' THEN
    IF NEW.parent_id = OLD.id THEN
      RAISE EXCEPTION 'an organization cannot be its own parent' USING ERRCODE = '23514';
    END IF;
    v_old_path := OLD.org_path;
    IF v_old_path IS DISTINCT FROM NEW.org_path THEN
      UPDATE fms.organizations
         SET org_path = NEW.org_path || subpath(org_path, nlevel(v_old_path))
       WHERE tenant_id = NEW.tenant_id
         AND org_path OPERATOR(public.<@) v_old_path
         AND id <> NEW.id;
    END IF;
  END IF;

  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_organizations_path
  BEFORE INSERT OR UPDATE OF parent_id, code ON fms.organizations
  FOR EACH ROW EXECUTE FUNCTION fms.trg_organization_path();

CREATE TRIGGER trg_organizations_updated_at
  BEFORE UPDATE ON fms.organizations
  FOR EACH ROW EXECUTE FUNCTION fms.trg_set_updated_at();

CREATE TABLE fms.facilities (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id       uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  org_id          uuid NOT NULL REFERENCES fms.organizations(id) ON DELETE RESTRICT,
  code            varchar(50)  NOT NULL,
  name            varchar(200) NOT NULL,
  facility_type   text NOT NULL DEFAULT 'OFFICE'
                    CHECK (facility_type IN ('OFFICE','CINEMA','CAMPUS','FACTORY','WAREHOUSE',
                                             'HOSPITAL','MALL','DATACENTER','MIXED','OTHER')),
  address_line1   text,
  address_line2   text,
  city            varchar(100),
  region          varchar(100),
  postal_code     varchar(20),
  country_code    char(2) NOT NULL DEFAULT 'TW',
  latitude        numeric(9,6),
  longitude       numeric(9,6),
  timezone        varchar(64) NOT NULL DEFAULT 'Asia/Taipei',
  gross_area_sqm  numeric(12,2),
  -- {"mon":[["08:00","22:00"]], ...} — default booking / SLA business hours.
  operating_hours jsonb NOT NULL DEFAULT '{}'::jsonb,
  attributes      jsonb NOT NULL DEFAULT '{}'::jsonb,
  status          text NOT NULL DEFAULT 'ACTIVE'
                    CHECK (status IN ('PLANNED','ACTIVE','UNDER_RENOVATION','CLOSED')),
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at      timestamptz
);

CREATE UNIQUE INDEX uq_facilities_tenant_code
  ON fms.facilities (tenant_id, lower(code)) WHERE deleted_at IS NULL;
CREATE INDEX idx_facilities_org ON fms.facilities (org_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_facilities_tenant ON fms.facilities (tenant_id) WHERE deleted_at IS NULL;

-- -----------------------------------------------------------------------------
-- 4. Tenant-scoped document numbering (WO-2026-000123 etc.)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.document_sequences (
  tenant_id   uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  doc_type    varchar(30) NOT NULL,   -- WORK_ORDER, RESERVATION, ASSET, ALARM
  period_key  varchar(10) NOT NULL,   -- '2026' or '2026-07' or 'ALL'
  last_value  bigint NOT NULL DEFAULT 0,
  prefix      varchar(20) NOT NULL DEFAULT '',
  pad_width   smallint NOT NULL DEFAULT 6,
  PRIMARY KEY (tenant_id, doc_type, period_key)
);

CREATE OR REPLACE FUNCTION fms.next_document_no(
  p_tenant_id uuid,
  p_doc_type  varchar,
  p_prefix    varchar DEFAULT NULL,
  p_period    varchar DEFAULT NULL
) RETURNS text
LANGUAGE plpgsql
AS $$
DECLARE
  v_period varchar(10) := coalesce(p_period, to_char(clock_timestamp(), 'YYYY'));
  v_prefix varchar(20) := coalesce(p_prefix, upper(left(p_doc_type, 2)));
  v_next   bigint;
  v_pad    smallint;
BEGIN
  INSERT INTO fms.document_sequences (tenant_id, doc_type, period_key, last_value, prefix)
  VALUES (p_tenant_id, p_doc_type, v_period, 1, v_prefix)
  ON CONFLICT (tenant_id, doc_type, period_key)
  DO UPDATE SET last_value = fms.document_sequences.last_value + 1
  RETURNING last_value, pad_width INTO v_next, v_pad;

  RETURN v_prefix || '-' || v_period || '-' || lpad(v_next::text, v_pad, '0');
END;
$$;

COMMENT ON FUNCTION fms.next_document_no IS
  'Gap-free, tenant-scoped human readable document numbers. Serialises on the sequence row — acceptable at FMS write volumes.';

-- -----------------------------------------------------------------------------
-- 5. Dynamic attribute definitions (vertical customisation without DDL)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.attribute_definitions (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id         uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,  -- NULL = platform default
  target_entity     text NOT NULL
                      CHECK (target_entity IN ('FACILITY','SPATIAL_NODE','ASSET','ASSET_MODEL',
                                               'SERVICE_ITEM','WORK_ORDER','RESERVATION','USER')),
  -- Optional narrowing, e.g. only assets in category PROJECTOR get "lamp_hours".
  applies_to_type   varchar(60),
  attribute_key     varchar(60) NOT NULL,
  label             varchar(120) NOT NULL,
  data_type         text NOT NULL
                      CHECK (data_type IN ('STRING','TEXT','NUMBER','INTEGER','BOOLEAN',
                                           'DATE','DATETIME','ENUM','MULTI_ENUM','JSON')),
  is_required       boolean NOT NULL DEFAULT false,
  is_searchable     boolean NOT NULL DEFAULT false,
  default_value     jsonb,
  -- Standard JSON Schema fragment; the API validates attributes/payload against it.
  validation_schema jsonb NOT NULL DEFAULT '{}'::jsonb,
  ui_hints          jsonb NOT NULL DEFAULT '{}'::jsonb,   -- {"widget":"select","group":"規格","order":10}
  display_order     integer NOT NULL DEFAULT 100,
  is_active         boolean NOT NULL DEFAULT true,
  created_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at        timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_attribute_definitions_key
  ON fms.attribute_definitions (coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid),
                                target_entity, coalesce(applies_to_type, '*'), attribute_key);

-- -----------------------------------------------------------------------------
-- 6. Attachments (Supabase Storage / S3 metadata)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.attachments (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  entity_type   varchar(40) NOT NULL,   -- ASSET, WORK_ORDER, SPATIAL_NODE, RESERVATION, BIM_MODEL
  entity_id     uuid NOT NULL,
  purpose       varchar(40) NOT NULL DEFAULT 'GENERAL',  -- BEFORE_PHOTO, AFTER_PHOTO, MANUAL, SIGNATURE
  file_name     varchar(255) NOT NULL,
  mime_type     varchar(120),
  size_bytes    bigint,
  checksum_sha256 char(64),
  storage_bucket varchar(80) NOT NULL DEFAULT 'fms',
  storage_key   text NOT NULL,
  uploaded_by   uuid,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at    timestamptz
);

CREATE INDEX idx_attachments_entity ON fms.attachments (tenant_id, entity_type, entity_id)
  WHERE deleted_at IS NULL;

-- -----------------------------------------------------------------------------
-- 7. Audit log (append-only)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.audit_log (
  id            bigserial,
  tenant_id     uuid,
  occurred_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  actor_user_id uuid,
  actor_type    text NOT NULL DEFAULT 'USER'
                  CHECK (actor_type IN ('USER','SERVICE_ACCOUNT','SYSTEM','DIRECTORY_SYNC')),
  action        varchar(60) NOT NULL,          -- CREATE, UPDATE, DELETE, ASSIGN, LOGIN, EXPORT
  entity_type   varchar(40) NOT NULL,
  entity_id     uuid,
  facility_id   uuid,
  before_data   jsonb,
  after_data    jsonb,
  diff_keys     text[],
  request_id    varchar(64),
  ip_address    inet,
  user_agent    text,
  PRIMARY KEY (occurred_at, id)
) PARTITION BY RANGE (occurred_at);

-- Create the first partitions; a scheduled job rolls these forward monthly.
CREATE TABLE fms.audit_log_2026m07 PARTITION OF fms.audit_log
  FOR VALUES FROM ('2026-07-01') TO ('2026-08-01');
CREATE TABLE fms.audit_log_2026m08 PARTITION OF fms.audit_log
  FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE fms.audit_log_default PARTITION OF fms.audit_log DEFAULT;

CREATE INDEX idx_audit_log_tenant_time ON fms.audit_log (tenant_id, occurred_at DESC);
CREATE INDEX idx_audit_log_entity ON fms.audit_log (tenant_id, entity_type, entity_id);

CREATE OR REPLACE FUNCTION fms.ensure_monthly_partitions(
  p_parent regclass,
  p_months integer DEFAULT 3
) RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
  v_base  date := date_trunc('month', clock_timestamp())::date;
  v_start date;
  v_end   date;
  v_name  text;
  i integer;
BEGIN
  FOR i IN 0..p_months LOOP
    v_start := (v_base + (i || ' month')::interval)::date;
    v_end   := (v_start + interval '1 month')::date;
    v_name  := replace(p_parent::text, 'fms.', '') || '_' || to_char(v_start, 'YYYY"m"MM');
    IF NOT EXISTS (
      SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = 'fms' AND c.relname = v_name
    ) THEN
      EXECUTE format('CREATE TABLE fms.%I PARTITION OF %s FOR VALUES FROM (%L) TO (%L)',
                     v_name, p_parent, v_start, v_end);
    END IF;
  END LOOP;
END;
$$;

-- -----------------------------------------------------------------------------
-- 8. Transactional outbox  (Phase 1: polled by a worker.
--    Phase 2: the same rows are relayed to Kafka without any producer changes.)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.event_outbox (
  id             bigserial PRIMARY KEY,
  tenant_id      uuid NOT NULL,
  event_type     varchar(80) NOT NULL,   -- work_order.created, reservation.confirmed, alarm.raised
  aggregate_type varchar(40) NOT NULL,
  aggregate_id   uuid NOT NULL,
  payload        jsonb NOT NULL,
  headers        jsonb NOT NULL DEFAULT '{}'::jsonb,
  status         text NOT NULL DEFAULT 'PENDING'
                   CHECK (status IN ('PENDING','PUBLISHED','FAILED','SKIPPED')),
  attempt_count  smallint NOT NULL DEFAULT 0,
  last_error     text,
  available_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  published_at   timestamptz,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- Worker claim pattern: SELECT ... WHERE status='PENDING' AND available_at <= now()
--                        ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 100;
CREATE INDEX idx_event_outbox_claim ON fms.event_outbox (status, available_at, id)
  WHERE status IN ('PENDING','FAILED');

CREATE OR REPLACE FUNCTION fms.emit_event(
  p_tenant_id uuid,
  p_event_type varchar,
  p_aggregate_type varchar,
  p_aggregate_id uuid,
  p_payload jsonb DEFAULT '{}'::jsonb
) RETURNS bigint
LANGUAGE sql
AS $$
  INSERT INTO fms.event_outbox (tenant_id, event_type, aggregate_type, aggregate_id, payload)
  VALUES (p_tenant_id, p_event_type, p_aggregate_type, p_aggregate_id, p_payload)
  RETURNING id;
$$;

-- -----------------------------------------------------------------------------
-- 9. Idempotency keys (POST retry safety for reservations / work orders)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.idempotency_keys (
  tenant_id       uuid NOT NULL,
  idempotency_key varchar(120) NOT NULL,
  endpoint        varchar(160) NOT NULL,
  request_hash    char(64) NOT NULL,
  response_status smallint,
  response_body   jsonb,
  state           text NOT NULL DEFAULT 'IN_FLIGHT'
                    CHECK (state IN ('IN_FLIGHT','COMPLETED','FAILED')),
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at      timestamptz NOT NULL DEFAULT clock_timestamp() + interval '24 hours',
  PRIMARY KEY (tenant_id, idempotency_key, endpoint)
);

CREATE INDEX idx_idempotency_keys_expiry ON fms.idempotency_keys (expires_at);

COMMIT;
