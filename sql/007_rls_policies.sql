-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 007: Row-Level Security, application role, grants
-- =============================================================================
-- Threat model
--   The single most likely breach in a shared-schema SaaS is a forgotten
--   "WHERE tenant_id = ..." in application code. RLS moves that filter into the
--   database so a missing predicate degrades to "no rows", never to another
--   tenant's data.
--
-- Two roles
--   fms_owner  — owns the objects, runs migrations, BYPASSRLS implicitly as owner.
--   fms_app    — the role the API connects as. NOT the owner, so policies apply.
--                On Supabase, map this to a dedicated database role used by the
--                API service; never let the API connect as postgres/service_role.
--
-- FORCE ROW LEVEL SECURITY is also enabled so that even a table owner is
-- filtered, which protects against accidental use of an over-privileged DSN.
-- =============================================================================

BEGIN;

-- -----------------------------------------------------------------------------
-- 1. Application role
-- -----------------------------------------------------------------------------

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'fms_app') THEN
    CREATE ROLE fms_app NOLOGIN;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'fms_readonly') THEN
    CREATE ROLE fms_readonly NOLOGIN;
  END IF;
EXCEPTION WHEN insufficient_privilege THEN
  RAISE NOTICE 'skipping role creation (insufficient privilege) — create fms_app / fms_readonly manually';
END;
$$;

GRANT USAGE ON SCHEMA fms TO fms_app, fms_readonly;

-- -----------------------------------------------------------------------------
-- 2. Enable RLS + tenant isolation policy on every tenant-scoped table
-- -----------------------------------------------------------------------------
-- Tables listed in v_catalog additionally permit reads of platform rows
-- (tenant_id IS NULL), which is how the shared equipment catalogue works.
-- -----------------------------------------------------------------------------

DO $$
DECLARE
  r record;
  -- Tables where tenant_id IS NULL means "platform-global, readable by all".
  catalog_tables text[] := ARRAY[
    'attribute_definitions','spatial_node_types','asset_categories','asset_models',
    'asset_model_compatibility','maintenance_templates','skills','roles',
    'work_order_transitions_allowed','notification_templates'
  ];
BEGIN
  FOR r IN
    SELECT c.relname AS table_name
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_attribute a ON a.attrelid = c.oid
    WHERE n.nspname = 'fms'
      AND c.relkind IN ('r','p')                 -- ordinary + partitioned tables
      AND a.attname = 'tenant_id'
      AND a.attnum > 0 AND NOT a.attisdropped
      AND c.relispartition = false               -- policies inherit to partitions
    ORDER BY c.relname
  LOOP
    EXECUTE format('ALTER TABLE fms.%I ENABLE ROW LEVEL SECURITY', r.table_name);
    EXECUTE format('ALTER TABLE fms.%I FORCE ROW LEVEL SECURITY', r.table_name);

    IF r.table_name = ANY (catalog_tables) THEN
      -- Read: own tenant rows + platform rows. Write: own tenant rows only.
      EXECUTE format($f$
        CREATE POLICY tenant_read ON fms.%I FOR SELECT
        USING (fms.is_platform_context()
               OR tenant_id IS NULL
               OR tenant_id = fms.current_tenant_id())
      $f$, r.table_name);

      EXECUTE format($f$
        CREATE POLICY tenant_write ON fms.%I FOR ALL
        USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
        WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
      $f$, r.table_name);
    ELSE
      EXECUTE format($f$
        CREATE POLICY tenant_isolation ON fms.%I FOR ALL
        USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
        WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
      $f$, r.table_name);
    END IF;
  END LOOP;
END;
$$;

-- Tenants table is special: a tenant may read only its own row.
ALTER TABLE fms.tenants ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.tenants FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_self_read ON fms.tenants FOR SELECT
  USING (fms.is_platform_context() OR id = fms.current_tenant_id());

CREATE POLICY tenant_self_update ON fms.tenants FOR UPDATE
  USING (fms.is_platform_context() OR id = fms.current_tenant_id())
  WITH CHECK (fms.is_platform_context() OR id = fms.current_tenant_id());

CREATE POLICY tenant_platform_insert ON fms.tenants FOR INSERT
  WITH CHECK (fms.is_platform_context());

CREATE POLICY tenant_platform_delete ON fms.tenants FOR DELETE
  USING (fms.is_platform_context());

-- Global catalogues with no tenant_id column at all: read-only for the app.
ALTER TABLE fms.permissions ENABLE ROW LEVEL SECURITY;
CREATE POLICY permissions_read ON fms.permissions FOR SELECT USING (true);
CREATE POLICY permissions_admin ON fms.permissions FOR ALL
  USING (fms.is_platform_context()) WITH CHECK (fms.is_platform_context());

ALTER TABLE fms.work_order_statuses ENABLE ROW LEVEL SECURITY;
CREATE POLICY wo_statuses_read ON fms.work_order_statuses FOR SELECT USING (true);
CREATE POLICY wo_statuses_admin ON fms.work_order_statuses FOR ALL
  USING (fms.is_platform_context()) WITH CHECK (fms.is_platform_context());

-- Join tables without tenant_id inherit isolation through their parent FK.
ALTER TABLE fms.role_permissions ENABLE ROW LEVEL SECURITY;
CREATE POLICY role_permissions_scoped ON fms.role_permissions FOR ALL
  USING (
    fms.is_platform_context() OR EXISTS (
      SELECT 1 FROM fms.roles r
      WHERE r.id = role_permissions.role_id
        AND (r.tenant_id IS NULL OR r.tenant_id = fms.current_tenant_id())
    )
  )
  WITH CHECK (
    fms.is_platform_context() OR EXISTS (
      SELECT 1 FROM fms.roles r
      WHERE r.id = role_permissions.role_id
        AND r.tenant_id = fms.current_tenant_id()
    )
  );

ALTER TABLE fms.telemetry_latest ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.telemetry_latest FORCE ROW LEVEL SECURITY;
CREATE POLICY telemetry_latest_isolation ON fms.telemetry_latest FOR ALL
  USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
  WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

-- -----------------------------------------------------------------------------
-- 3. Optional second ring: facility-level visibility
-- -----------------------------------------------------------------------------
-- Enable per deployment when a group client requires that a facility manager
-- cannot even read sibling facilities. The API sets app.facility_ids to a
-- comma-separated allow-list derived from fms.user_accessible_facilities().
-- Left as RESTRICTIVE policies so they compose with tenant_isolation (AND).
-- -----------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION fms.current_facility_ids()
RETURNS uuid[]
LANGUAGE sql STABLE PARALLEL SAFE
AS $$
  SELECT CASE
    WHEN coalesce(current_setting('app.facility_ids', true), '') = '' THEN NULL
    ELSE string_to_array(current_setting('app.facility_ids', true), ',')::uuid[]
  END;
$$;

CREATE OR REPLACE FUNCTION fms.facility_in_scope(p_facility_id uuid)
RETURNS boolean
LANGUAGE sql STABLE PARALLEL SAFE
AS $$
  SELECT fms.current_facility_ids() IS NULL
      OR p_facility_id IS NULL
      OR p_facility_id = ANY (fms.current_facility_ids());
$$;

DO $$
DECLARE
  r record;
BEGIN
  FOR r IN
    SELECT c.relname AS table_name
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_attribute a ON a.attrelid = c.oid
    WHERE n.nspname = 'fms'
      AND c.relkind IN ('r','p')
      AND a.attname = 'facility_id'
      AND a.attnum > 0 AND NOT a.attisdropped
      AND c.relispartition = false
      AND c.relname <> 'facilities'
    ORDER BY c.relname
  LOOP
    EXECUTE format($f$
      CREATE POLICY facility_scope ON fms.%I AS RESTRICTIVE FOR ALL
      USING (fms.is_platform_context() OR fms.facility_in_scope(facility_id))
    $f$, r.table_name);
  END LOOP;

  EXECUTE $f$
    CREATE POLICY facility_scope ON fms.facilities AS RESTRICTIVE FOR ALL
    USING (fms.is_platform_context() OR fms.facility_in_scope(id))
  $f$;
END;
$$;

-- -----------------------------------------------------------------------------
-- 4. Grants
-- -----------------------------------------------------------------------------

GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA fms TO fms_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA fms TO fms_app;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA fms TO fms_app;

GRANT SELECT ON ALL TABLES IN SCHEMA fms TO fms_readonly;

ALTER DEFAULT PRIVILEGES IN SCHEMA fms
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO fms_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA fms
  GRANT USAGE, SELECT ON SEQUENCES TO fms_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA fms
  GRANT EXECUTE ON FUNCTIONS TO fms_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA fms
  GRANT SELECT ON TABLES TO fms_readonly;

-- The audit trail must not be rewritten by the application.
REVOKE UPDATE, DELETE ON fms.audit_log FROM fms_app;
REVOKE UPDATE, DELETE ON fms.work_order_transitions FROM fms_app;
REVOKE UPDATE, DELETE ON fms.auth_events FROM fms_app;

COMMIT;
