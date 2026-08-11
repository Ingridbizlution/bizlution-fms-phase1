-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 002: Identity, hybrid AD / directory federation, scoped RBAC
-- =============================================================================
-- Hybrid AD model (spec §3)
--   A. Cloud-first  : Entra ID (Azure AD) per tenant via OIDC or SAML2.
--   B. On-prem AD   : LDAPS bind for authentication + scheduled group sync,
--                     used by group clients that have not federated to cloud.
--   C. Local accounts: outsourced technicians / vendors / kiosks that will never
--                     exist in the customer's directory.
--   All three converge on fms.users; fms.user_identities holds the external
--   subject bindings so one human can be reachable through several providers.
-- =============================================================================

BEGIN;

-- -----------------------------------------------------------------------------
-- 1. Identity providers (per tenant, multiple allowed)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.identity_providers (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id          uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  code               varchar(50) NOT NULL,          -- 'entra-hq', 'ad-fab1', 'local'
  name               varchar(150) NOT NULL,
  provider_type      text NOT NULL
                       CHECK (provider_type IN ('OIDC','SAML2','LDAP','LOCAL')),
  -- Which org subtree this provider governs; lets one group client run
  -- Entra ID at HQ and on-prem AD at a subsidiary.
  scope_org_path     ltree,
  -- OIDC / SAML2 -------------------------------------------------------------
  issuer             text,
  discovery_url      text,
  jwks_uri           text,
  client_id          text,
  client_secret_ref  text,             -- pointer into the secret manager, never the secret
  metadata_xml_ref   text,
  audience           text,
  -- LDAP / on-prem AD --------------------------------------------------------
  ldap_host          varchar(255),
  ldap_port          integer,
  ldap_use_tls       boolean NOT NULL DEFAULT true,
  ldap_base_dn       text,
  ldap_bind_dn       text,
  ldap_bind_secret_ref text,
  ldap_user_filter   text DEFAULT '(&(objectClass=user)(sAMAccountName={username}))',
  ldap_group_filter  text DEFAULT '(&(objectClass=group))',
  -- Claim / attribute mapping to fms.users columns
  attribute_mapping  jsonb NOT NULL DEFAULT
    '{"external_subject":"sub","email":"email","display_name":"name","employee_no":"employeeId","groups":"groups"}'::jsonb,
  group_claim_name   varchar(60) NOT NULL DEFAULT 'groups',
  -- Provisioning behaviour
  jit_provisioning   boolean NOT NULL DEFAULT true,   -- create user on first login
  jit_default_role_code varchar(50),
  auto_deprovision   boolean NOT NULL DEFAULT false,  -- suspend users absent from last sync
  sync_enabled       boolean NOT NULL DEFAULT false,
  sync_cron          varchar(40) DEFAULT '0 */4 * * *',
  scim_enabled       boolean NOT NULL DEFAULT false,
  scim_token_ref     text,
  is_default         boolean NOT NULL DEFAULT false,
  status             text NOT NULL DEFAULT 'ACTIVE'
                       CHECK (status IN ('ACTIVE','DISABLED','TESTING')),
  last_sync_at       timestamptz,
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at         timestamptz,
  CONSTRAINT ck_idp_oidc_fields CHECK (
    provider_type <> 'OIDC' OR (issuer IS NOT NULL AND client_id IS NOT NULL)
  ),
  CONSTRAINT ck_idp_ldap_fields CHECK (
    provider_type <> 'LDAP' OR (ldap_host IS NOT NULL AND ldap_base_dn IS NOT NULL)
  )
);

CREATE UNIQUE INDEX uq_identity_providers_code
  ON fms.identity_providers (tenant_id, lower(code)) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX uq_identity_providers_default
  ON fms.identity_providers (tenant_id) WHERE is_default AND deleted_at IS NULL;

-- -----------------------------------------------------------------------------
-- 2. Users
-- -----------------------------------------------------------------------------

CREATE TABLE fms.users (
  id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id           uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  primary_org_id      uuid REFERENCES fms.organizations(id) ON DELETE SET NULL,
  default_facility_id uuid REFERENCES fms.facilities(id) ON DELETE SET NULL,
  employee_no         varchar(50),
  username            citext NOT NULL,
  email               citext,
  display_name        varchar(150) NOT NULL,
  given_name          varchar(80),
  family_name         varchar(80),
  phone               varchar(40),
  job_title           varchar(120),
  user_type           text NOT NULL DEFAULT 'EMPLOYEE'
                        CHECK (user_type IN ('EMPLOYEE','CONTRACTOR','VENDOR','TENANT_USER',
                                             'SERVICE_ACCOUNT','KIOSK')),
  -- Only populated for LOCAL provider accounts (argon2id / bcrypt).
  password_hash       text,
  password_updated_at timestamptz,
  must_change_password boolean NOT NULL DEFAULT false,
  mfa_enabled         boolean NOT NULL DEFAULT false,
  locale              varchar(16),
  timezone            varchar(64),
  avatar_url          text,
  -- Raw directory snapshot for troubleshooting sync issues.
  directory_snapshot  jsonb NOT NULL DEFAULT '{}'::jsonb,
  attributes          jsonb NOT NULL DEFAULT '{}'::jsonb,
  status              text NOT NULL DEFAULT 'ACTIVE'
                        CHECK (status IN ('INVITED','ACTIVE','SUSPENDED','DEPROVISIONED')),
  last_login_at       timestamptz,
  created_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at          timestamptz
);

CREATE UNIQUE INDEX uq_users_tenant_username
  ON fms.users (tenant_id, username) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX uq_users_tenant_email
  ON fms.users (tenant_id, email) WHERE email IS NOT NULL AND deleted_at IS NULL;
CREATE UNIQUE INDEX uq_users_tenant_employee_no
  ON fms.users (tenant_id, employee_no) WHERE employee_no IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX idx_users_org ON fms.users (primary_org_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_users_name_trgm ON fms.users USING gin (display_name gin_trgm_ops);

ALTER TABLE fms.organizations
  ADD CONSTRAINT fk_organizations_manager
  FOREIGN KEY (manager_user_id) REFERENCES fms.users(id) ON DELETE SET NULL;

-- One user ←→ many external identities (Entra sub, AD objectGUID, LDAP DN…)
CREATE TABLE fms.user_identities (
  id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id            uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  user_id              uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  identity_provider_id uuid NOT NULL REFERENCES fms.identity_providers(id) ON DELETE CASCADE,
  external_subject     text NOT NULL,       -- OIDC sub / SAML NameID / AD objectGUID
  external_upn         text,                -- user@corp.example.com
  external_dn          text,                -- CN=Wang,OU=IT,DC=corp,DC=local
  raw_claims           jsonb NOT NULL DEFAULT '{}'::jsonb,
  is_primary           boolean NOT NULL DEFAULT false,
  linked_at            timestamptz NOT NULL DEFAULT clock_timestamp(),
  last_seen_at         timestamptz
);

CREATE UNIQUE INDEX uq_user_identities_subject
  ON fms.user_identities (identity_provider_id, external_subject);
CREATE INDEX idx_user_identities_user ON fms.user_identities (user_id);

-- -----------------------------------------------------------------------------
-- 3. Directory groups + automatic role mapping
-- -----------------------------------------------------------------------------

CREATE TABLE fms.directory_groups (
  id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id            uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  identity_provider_id uuid NOT NULL REFERENCES fms.identity_providers(id) ON DELETE CASCADE,
  external_group_id    text NOT NULL,       -- AD objectGUID / Entra group id
  name                 varchar(200) NOT NULL,
  distinguished_name   text,
  description          text,
  member_count         integer NOT NULL DEFAULT 0,
  last_synced_at       timestamptz,
  created_at           timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_directory_groups_external
  ON fms.directory_groups (identity_provider_id, external_group_id);

CREATE TABLE fms.user_directory_groups (
  user_id            uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  directory_group_id uuid NOT NULL REFERENCES fms.directory_groups(id) ON DELETE CASCADE,
  tenant_id          uuid NOT NULL,
  synced_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (user_id, directory_group_id)
);

-- Directory sync runs (observability for the AD integration)
CREATE TABLE fms.directory_sync_runs (
  id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id            uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  identity_provider_id uuid NOT NULL REFERENCES fms.identity_providers(id) ON DELETE CASCADE,
  run_type             text NOT NULL CHECK (run_type IN ('FULL','DELTA','SCIM_PUSH','MANUAL')),
  status               text NOT NULL DEFAULT 'RUNNING'
                         CHECK (status IN ('RUNNING','SUCCEEDED','PARTIAL','FAILED')),
  users_created        integer NOT NULL DEFAULT 0,
  users_updated        integer NOT NULL DEFAULT 0,
  users_suspended      integer NOT NULL DEFAULT 0,
  groups_synced        integer NOT NULL DEFAULT 0,
  roles_granted        integer NOT NULL DEFAULT 0,
  roles_revoked        integer NOT NULL DEFAULT 0,
  error_summary        text,
  details              jsonb NOT NULL DEFAULT '{}'::jsonb,
  started_at           timestamptz NOT NULL DEFAULT clock_timestamp(),
  finished_at          timestamptz
);

CREATE INDEX idx_directory_sync_runs_provider
  ON fms.directory_sync_runs (identity_provider_id, started_at DESC);

-- -----------------------------------------------------------------------------
-- 4. Permissions / roles / scoped assignments
-- -----------------------------------------------------------------------------

-- Global permission catalogue (seeded in 008). Format: resource:action
CREATE TABLE fms.permissions (
  code        varchar(80) PRIMARY KEY,
  resource    varchar(40) NOT NULL,
  action      varchar(40) NOT NULL,
  module      varchar(40) NOT NULL,     -- CORE, ASSET, MAINTENANCE, SERVICE, RESERVATION, IOT, ADMIN
  description text,
  -- Lowest scope level at which this permission can be granted.
  min_scope_level text NOT NULL DEFAULT 'FACILITY'
                    CHECK (min_scope_level IN ('TENANT','ORG','FACILITY','SPATIAL_NODE')),
  is_dangerous boolean NOT NULL DEFAULT false
);

CREATE TABLE fms.roles (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,  -- NULL = system role
  code          varchar(50) NOT NULL,
  name          varchar(120) NOT NULL,
  description   text,
  is_system     boolean NOT NULL DEFAULT false,
  is_assignable boolean NOT NULL DEFAULT true,
  -- Highest scope at which this role is intended to be granted.
  scope_level   text NOT NULL DEFAULT 'FACILITY'
                  CHECK (scope_level IN ('TENANT','ORG','FACILITY','SPATIAL_NODE')),
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_roles_code
  ON fms.roles (coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid), lower(code));

CREATE TABLE fms.role_permissions (
  role_id         uuid NOT NULL REFERENCES fms.roles(id) ON DELETE CASCADE,
  permission_code varchar(80) NOT NULL REFERENCES fms.permissions(code) ON DELETE CASCADE,
  PRIMARY KEY (role_id, permission_code)
);

-- A grant always carries a scope. scope_type=TENANT means "whole tenant".
CREATE TABLE fms.user_role_assignments (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  user_id     uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  role_id     uuid NOT NULL REFERENCES fms.roles(id) ON DELETE CASCADE,
  scope_type  text NOT NULL
                CHECK (scope_type IN ('TENANT','ORG','FACILITY','SPATIAL_NODE')),
  scope_id    uuid,                      -- NULL only when scope_type = TENANT
  source      text NOT NULL DEFAULT 'MANUAL'
                CHECK (source IN ('MANUAL','DIRECTORY_SYNC','SCIM','JIT','SYSTEM')),
  granted_by  uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  -- Populated when source = DIRECTORY_SYNC so revocation can be automated.
  origin_directory_group_id uuid REFERENCES fms.directory_groups(id) ON DELETE CASCADE,
  valid_from  timestamptz NOT NULL DEFAULT clock_timestamp(),
  valid_until timestamptz,
  created_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_ura_scope CHECK (
    (scope_type = 'TENANT' AND scope_id IS NULL) OR
    (scope_type <> 'TENANT' AND scope_id IS NOT NULL)
  )
);

CREATE UNIQUE INDEX uq_user_role_assignments
  ON fms.user_role_assignments (user_id, role_id, scope_type,
       coalesce(scope_id, '00000000-0000-0000-0000-000000000000'::uuid));
CREATE INDEX idx_ura_user ON fms.user_role_assignments (user_id);
CREATE INDEX idx_ura_scope ON fms.user_role_assignments (tenant_id, scope_type, scope_id);
CREATE INDEX idx_ura_origin_group ON fms.user_role_assignments (origin_directory_group_id)
  WHERE origin_directory_group_id IS NOT NULL;

-- AD/Entra group  →  FMS role @ scope. Evaluated on every login and every sync.
CREATE TABLE fms.directory_role_mappings (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id          uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  directory_group_id uuid REFERENCES fms.directory_groups(id) ON DELETE CASCADE,
  -- Intended as an alternative to a synced group row: match the raw claim value
  -- directly, for the window before the first group sync has run.
  --
  -- **從來沒有任何消費者，077 因此禁止把它當成唯一來源。** 058 的對帳是對
  -- directory_groups 的內連接，只填這個欄位的對應會被靜默丟掉 —— 建得起來、
  -- 回 201、永遠不授予任何角色、沒有症狀。要啟用它需要三件現在不存在的東西：
  -- 一條會寫入 user_identities.raw_claims 的 OIDC／SAML 登入流程、
  -- 一個以 claim 為鍵的成員關係存放處，以及一個以 claim 為鍵的收回身分。
  -- 完整量測與取捨見 migration 077。
  claim_value        text,
  role_id            uuid NOT NULL REFERENCES fms.roles(id) ON DELETE CASCADE,
  scope_type         text NOT NULL
                       CHECK (scope_type IN ('TENANT','ORG','FACILITY','SPATIAL_NODE')),
  scope_id           uuid,
  priority           integer NOT NULL DEFAULT 100,
  is_active          boolean NOT NULL DEFAULT true,
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  -- 077 把它換成 ck_drm_group_required：「二選一」允許建立永遠不會生效的對應。
  -- 這一行保留為歷史定義（077 的 down 會把它還原回來）。
  CONSTRAINT ck_drm_source CHECK (directory_group_id IS NOT NULL OR claim_value IS NOT NULL),
  CONSTRAINT ck_drm_scope CHECK (
    (scope_type = 'TENANT' AND scope_id IS NULL) OR
    (scope_type <> 'TENANT' AND scope_id IS NOT NULL)
  )
);

CREATE INDEX idx_drm_tenant ON fms.directory_role_mappings (tenant_id) WHERE is_active;

-- -----------------------------------------------------------------------------
-- 5. Machine identities (IoT gateway, BIM importer, customer ERP)
-- -----------------------------------------------------------------------------

CREATE TABLE fms.api_clients (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  client_id      varchar(64) NOT NULL,
  name           varchar(150) NOT NULL,
  client_type    text NOT NULL DEFAULT 'INTEGRATION'
                   CHECK (client_type IN ('INTEGRATION','IOT_GATEWAY','BFF','MOBILE','REPORTING')),
  secret_hash    text NOT NULL,
  scopes         text[] NOT NULL DEFAULT '{}',
  allowed_facility_ids uuid[],
  allowed_cidrs  cidr[],
  rate_limit_rps integer,
  status         text NOT NULL DEFAULT 'ACTIVE'
                   CHECK (status IN ('ACTIVE','DISABLED','REVOKED')),
  last_used_at   timestamptz,
  expires_at     timestamptz,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at     timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_api_clients_client_id ON fms.api_clients (client_id);

-- Authentication event trail (separate from audit_log: high volume, short retention)
CREATE TABLE fms.auth_events (
  id             bigserial PRIMARY KEY,
  tenant_id      uuid,
  user_id        uuid,
  identity_provider_id uuid,
  event_type     varchar(40) NOT NULL,   -- LOGIN_SUCCESS, LOGIN_FAILED, TOKEN_REFRESH, LOGOUT, MFA_CHALLENGE
  result         text NOT NULL DEFAULT 'SUCCESS' CHECK (result IN ('SUCCESS','FAILURE')),
  failure_reason varchar(120),
  ip_address     inet,
  user_agent     text,
  occurred_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX idx_auth_events_user_time ON fms.auth_events (user_id, occurred_at DESC);
CREATE INDEX idx_auth_events_tenant_time ON fms.auth_events (tenant_id, occurred_at DESC);

-- -----------------------------------------------------------------------------
-- 6. Effective permission resolution
-- -----------------------------------------------------------------------------
-- Returns every permission the user holds together with the scope it applies at.
-- ORG / FACILITY scopes are expanded downwards using the org and spatial trees,
-- so a grant at 事業部 level automatically covers all its facilities.
-- -----------------------------------------------------------------------------

CREATE OR REPLACE VIEW fms.v_user_effective_permissions AS
SELECT
  ura.tenant_id,
  ura.user_id,
  rp.permission_code,
  p.module,
  p.resource,
  p.action,
  ura.scope_type,
  ura.scope_id,
  r.code AS role_code
FROM fms.user_role_assignments ura
JOIN fms.roles r            ON r.id = ura.role_id
JOIN fms.role_permissions rp ON rp.role_id = r.id
JOIN fms.permissions p       ON p.code = rp.permission_code
WHERE ura.valid_from <= clock_timestamp()
  AND (ura.valid_until IS NULL OR ura.valid_until > clock_timestamp());

CREATE OR REPLACE FUNCTION fms.user_has_permission(
  p_user_id     uuid,
  p_permission  varchar,
  p_facility_id uuid DEFAULT NULL,
  p_org_id      uuid DEFAULT NULL
) RETURNS boolean
LANGUAGE sql STABLE
AS $$
  SELECT EXISTS (
    SELECT 1
    FROM fms.v_user_effective_permissions ep
    LEFT JOIN fms.facilities f ON f.id = p_facility_id
    LEFT JOIN fms.organizations o_target ON o_target.id = coalesce(p_org_id, f.org_id)
    LEFT JOIN fms.organizations o_scope  ON o_scope.id = ep.scope_id
    WHERE ep.user_id = p_user_id
      AND ep.permission_code = p_permission
      AND (
            ep.scope_type = 'TENANT'
        OR (ep.scope_type = 'FACILITY' AND ep.scope_id = p_facility_id)
        OR (ep.scope_type = 'ORG'
            AND o_scope.org_path IS NOT NULL
            AND o_target.org_path IS NOT NULL
            AND o_target.org_path OPERATOR(public.<@) o_scope.org_path)
      )
  );
$$;

COMMENT ON FUNCTION fms.user_has_permission IS
  'Authoritative authorisation check. The API layer calls this (or its cached equivalent) before any mutating operation.';

-- Facility list the user can see — used by the API to build list filters and by
-- optional facility-level RLS policies.
CREATE OR REPLACE FUNCTION fms.user_accessible_facilities(p_user_id uuid)
RETURNS TABLE (facility_id uuid)
LANGUAGE sql STABLE
AS $$
  SELECT DISTINCT f.id
  FROM fms.facilities f
  JOIN fms.organizations o ON o.id = f.org_id
  WHERE f.deleted_at IS NULL
    AND EXISTS (
      SELECT 1
      FROM fms.user_role_assignments ura
      LEFT JOIN fms.organizations os ON os.id = ura.scope_id
      WHERE ura.user_id = p_user_id
        AND ura.tenant_id = f.tenant_id
        AND (ura.valid_until IS NULL OR ura.valid_until > clock_timestamp())
        AND (
              ura.scope_type = 'TENANT'
          OR (ura.scope_type = 'FACILITY' AND ura.scope_id = f.id)
          OR (ura.scope_type = 'ORG' AND os.org_path IS NOT NULL
              AND o.org_path OPERATOR(public.<@) os.org_path)
        )
    );
$$;

COMMIT;
