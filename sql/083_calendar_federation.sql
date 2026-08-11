-- =============================================================================
-- 083：多租戶日曆聯邦的資料模型（ADR-14）
-- =============================================================================
-- 見 docs/adr/ADR-14-calendar-federation.md。這支 migration 只做結構 ——
-- 三張新表、`reservations` 的一個去重索引、兩個新權限碼——不依賴示範租戶，
-- 因此放 CORE。CalendarSyncWatchdog 的服務帳號（比照 078／080）是獨立的
-- 084，因為那才是真正依賴示範租戶存在的部分。
--
-- -----------------------------------------------------------------------------
-- `fms.reservations` 早就留了位置
-- -----------------------------------------------------------------------------
-- `external_event_id varchar(200)` 與 `created_via` 的 OUTLOOK／GOOGLE 兩個值
-- 從 005 就存在，只是從來沒有程式碼寫入過。這支 migration 只補一個索引，
-- 不改欄位——雙重預約的判定繼續交給既有的 GiST exclusion constraint，
-- 外部事件只要也寫成一列 `reservations` 就自動被擋，不需要新的判斷邏輯。
--
-- -----------------------------------------------------------------------------
-- 為什麼三張表，不是塞進 identity_providers
-- -----------------------------------------------------------------------------
-- ADR-14 決定 K：這不是身分整合（`fms-identity` 的邊界），也不是預約業務
-- 規則本身（`fms-reservation` 的邊界）——是一個獨立的外部系統整合邊界，
-- 有自己的 OAuth 生命週期、自己的比對佇列、自己的衝突處理。
--
--   * `calendar_integrations`   —— 租戶對 MS365／Google 的連線設定
--   * `calendar_resource_mappings` —— 外部房間資源 ↔ 內部 spatial_node
--   * `calendar_sync_conflicts`   —— 雙向同步兩邊都改了同一筆時的待審佇列
--     （形狀比照 BIM 的 unresolved_elements：不自動選邊，讓管理者決定）
--
-- -----------------------------------------------------------------------------
-- `app_client_secret`（Microsoft 那份全平台共用的憑證）刻意不進這張表
-- -----------------------------------------------------------------------------
-- 它跟 `google_refresh_token_ref` 不一樣：後者是 per-tenant 資料，前者是
-- 平台層級、與租戶數量無關的一份設定。硬塞進一張以 tenant_id 為主鍵概念的表
-- 只會製造「N 個租戶各存一份本該相同的值」的漂移風險。它由 Rust 端直接讀
-- 環境變數解析（比照 ADR-13 決定 A 的 SecretResolver 慣例），不進資料庫。
--
-- -----------------------------------------------------------------------------
-- sync_direction 預設 BIDIRECTIONAL
-- -----------------------------------------------------------------------------
-- 使用者已經拍板同步方向選 C（雙向），這裡把它做成預設值而非唯一值——
-- 保留欄位讓個別資源退回單向（例如某個會議室政策上只允許在 Outlook 直接訂，
-- FMS 這邊只顯示不受理）。
--
-- 依賴：003（spatial_nodes）、005（reservations、GiST exclusion constraint）、
--       007（RLS 慣例）、008（permissions 種子）。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

-- 決定 6 的 role_permissions 授予需要它：`is_platform_context()` 是雙條件
-- 判定（app.is_platform=on **且** current_user 屬於 fms_platform），
-- role_permissions 又掛了 029 的稽核觸發器，寫進 audit_log 一樣受
-- tenant_isolation 政策管——沒有平台情境，role_permissions 沒有
-- tenant_id 可比對，會被 RLS 擋下（症狀是這支 migration 卡在稽核觸發器，
-- 不是卡在看起來的那個 INSERT 本身）。
SELECT set_config('app.is_platform', 'on', true);

-- -----------------------------------------------------------------------------
-- 1. calendar_integrations
-- -----------------------------------------------------------------------------

CREATE TABLE fms.calendar_integrations (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id       uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  -- NULL = 租戶全域（適用所有場域）。
  facility_id     uuid REFERENCES fms.facilities(id) ON DELETE CASCADE,
  provider        text NOT NULL CHECK (provider IN ('MS365', 'GOOGLE')),
  status          text NOT NULL DEFAULT 'PENDING_CONSENT'
                    CHECK (status IN ('PENDING_CONSENT', 'ACTIVE', 'REVOKED', 'ERROR')),
  -- MS365 專用，明文——這不是密鑰，是客戶的 Azure AD 租戶 GUID，
  -- app-only client-credentials flow 認證時的第二個參數（見 ADR-14 決定 G）。
  ms_tenant_id    text,
  -- GOOGLE 專用，per-tenant refresh token 的密鑰管理服務參照，
  -- 透過 fms_shared::SecretResolver 解析（ADR-13 決定 A）。
  google_refresh_token_ref text,
  sync_cron       text NOT NULL DEFAULT '*/5 * * * *',
  last_synced_at  timestamptz,
  last_sync_error text,
  created_by      uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  -- 只在 ACTIVE 時要求對應欄位存在：PENDING_CONSENT 期間 ms_tenant_id
  -- 可能還沒從 consent 回呼裡拿到。
  CONSTRAINT ck_calendar_integrations_ms_tenant CHECK (
    status <> 'ACTIVE' OR provider <> 'MS365' OR ms_tenant_id IS NOT NULL
  ),
  CONSTRAINT ck_calendar_integrations_google_ref CHECK (
    status <> 'ACTIVE' OR provider <> 'GOOGLE' OR google_refresh_token_ref IS NOT NULL
  )
);

-- 一個租戶對同一個 provider、同一個範圍（租戶全域或特定場域）只能有一個連線。
CREATE UNIQUE INDEX uq_calendar_integrations_scope
  ON fms.calendar_integrations (
    tenant_id,
    coalesce(facility_id, '00000000-0000-0000-0000-000000000000'::uuid),
    provider
  );

-- CalendarSyncWatchdog 找到期列用。
CREATE INDEX idx_calendar_integrations_active
  ON fms.calendar_integrations (status) WHERE status = 'ACTIVE';

ALTER TABLE fms.calendar_integrations ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.calendar_integrations FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON fms.calendar_integrations FOR ALL
  USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
  WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

CREATE TRIGGER trg_calendar_integrations_updated_at
  BEFORE UPDATE ON fms.calendar_integrations
  FOR EACH ROW EXECUTE FUNCTION fms.trg_set_updated_at();

CREATE TRIGGER trg_calendar_integrations_freeze_tenant
  BEFORE UPDATE ON fms.calendar_integrations
  FOR EACH ROW EXECUTE FUNCTION fms.trg_freeze_tenant_id();

COMMENT ON TABLE fms.calendar_integrations IS
  '租戶對 Microsoft 365／Google Workspace 的日曆連線設定（ADR-14）。'
  'app_client_secret 不在這張表——那是平台層級共用的一份設定，見檔頭。';

COMMENT ON COLUMN fms.calendar_integrations.ms_tenant_id IS
  'Azure AD 租戶 GUID，明文——不是密鑰，是 client-credentials flow 的第二個參數。';

COMMENT ON COLUMN fms.calendar_integrations.google_refresh_token_ref IS
  '密鑰管理服務的參照，透過 SecretResolver 解析（ADR-13 決定 A）。'
  '與 identity_providers.client_secret_ref 同一個慣例：只存參照，不存明文。';

-- -----------------------------------------------------------------------------
-- 2. calendar_resource_mappings
-- -----------------------------------------------------------------------------

CREATE TABLE fms.calendar_resource_mappings (
  id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id               uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  calendar_integration_id uuid NOT NULL REFERENCES fms.calendar_integrations(id) ON DELETE CASCADE,
  spatial_node_id         uuid NOT NULL REFERENCES fms.spatial_nodes(id) ON DELETE CASCADE,
  -- Microsoft 房間信箱 UPN，或 Google Calendar 資源 email。
  external_resource_id    text NOT NULL,
  external_resource_name  text,
  sync_direction          text NOT NULL DEFAULT 'BIDIRECTIONAL'
                            CHECK (sync_direction IN ('PULL_ONLY', 'PUSH_ONLY', 'BIDIRECTIONAL')),
  -- UNRESOLVED：自動比對比不到，等人工掛上（見 ADR-14 決定 J）。
  status                  text NOT NULL DEFAULT 'UNRESOLVED'
                            CHECK (status IN ('ACTIVE', 'UNRESOLVED', 'DISABLED')),
  -- Graph／Google 的 delta 查詢游標（`@odata.deltaLink` 或 `syncToken`）。
  -- NULL = 從未同步過，下一次要做一次全量的初始 delta 查詢，不是增量。
  -- 兩家平台的權杖格式不同，因此存不透明字串，由 provider 實作自行解讀。
  delta_token             text,
  created_at              timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at              timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- 同一個整合裡，一個外部資源只對應一個內部節點。
CREATE UNIQUE INDEX uq_calendar_resource_mappings_external
  ON fms.calendar_resource_mappings (calendar_integration_id, external_resource_id);

-- 同一個整合裡，一個內部節點只能有一個生效中的對應——
-- 少了這個，一個房間可能同時對到兩個外部資源，雙向同步時無法判斷該推去哪一邊。
CREATE UNIQUE INDEX uq_calendar_resource_mappings_node
  ON fms.calendar_resource_mappings (calendar_integration_id, spatial_node_id)
  WHERE status = 'ACTIVE';

CREATE INDEX idx_calendar_resource_mappings_unresolved
  ON fms.calendar_resource_mappings (calendar_integration_id) WHERE status = 'UNRESOLVED';

ALTER TABLE fms.calendar_resource_mappings ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.calendar_resource_mappings FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON fms.calendar_resource_mappings FOR ALL
  USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
  WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

-- 這張表沒有自己的 facility_id，但透過 spatial_node_id 可以間接算出場域——
-- 跟 062 收斂的 20 張子表同一類洩漏：`spatial_nodes` 已經場域收斂，
-- 這張表若只掛 tenant_isolation，一個場域受限的讀者會看不到父列（空間節點）
-- 卻看得到指向別的場域空間節點的對應列。RESTRICTIVE，用父表的可見性判斷。
CREATE POLICY facility_scope_via_parent ON fms.calendar_resource_mappings
  AS RESTRICTIVE FOR ALL
  USING (fms.is_platform_context()
         OR EXISTS (SELECT 1 FROM fms.spatial_nodes p
                     WHERE p.id = fms.calendar_resource_mappings.spatial_node_id));

CREATE TRIGGER trg_calendar_resource_mappings_updated_at
  BEFORE UPDATE ON fms.calendar_resource_mappings
  FOR EACH ROW EXECUTE FUNCTION fms.trg_set_updated_at();

CREATE TRIGGER trg_calendar_resource_mappings_freeze_tenant
  BEFORE UPDATE ON fms.calendar_resource_mappings
  FOR EACH ROW EXECUTE FUNCTION fms.trg_freeze_tenant_id();

COMMENT ON TABLE fms.calendar_resource_mappings IS
  '外部房間資源 ↔ 內部 spatial_node 的對應（ADR-14 決定 J，比照 BIM 的比對/'
  '待審佇列形狀）。UNRESOLVED 狀態的列在管理端顯示為待人工處理。';

-- -----------------------------------------------------------------------------
-- 3. calendar_sync_conflicts
-- -----------------------------------------------------------------------------

CREATE TABLE fms.calendar_sync_conflicts (
  id                           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id                    uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  reservation_id               uuid NOT NULL REFERENCES fms.reservations(id) ON DELETE CASCADE,
  calendar_resource_mapping_id uuid NOT NULL REFERENCES fms.calendar_resource_mappings(id) ON DELETE CASCADE,
  external_event_id            varchar(200) NOT NULL,
  detected_at                  timestamptz NOT NULL DEFAULT clock_timestamp(),
  -- 偵測當下兩邊各自的狀態快照，供管理者比對——不是用來自動判斷的輸入。
  fms_side_snapshot            jsonb NOT NULL,
  external_side_snapshot       jsonb NOT NULL,
  status                       text NOT NULL DEFAULT 'OPEN'
                                 CHECK (status IN ('OPEN', 'RESOLVED')),
  resolved_at                  timestamptz,
  resolved_by                  uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  resolution                   text,
  updated_at                   timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_calendar_sync_conflicts_resolution CHECK (
    (resolved_at IS NULL) = (resolved_by IS NULL)
  )
);

-- 管理端的待審清單查詢。
CREATE INDEX idx_calendar_sync_conflicts_open
  ON fms.calendar_sync_conflicts (tenant_id) WHERE status = 'OPEN';

ALTER TABLE fms.calendar_sync_conflicts ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.calendar_sync_conflicts FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON fms.calendar_sync_conflicts FOR ALL
  USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
  WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

-- 同一類洩漏，見 calendar_resource_mappings 上方的說明——這裡的父表是
-- reservations（也已經場域收斂），錨點是 reservation_id。
CREATE POLICY facility_scope_via_parent ON fms.calendar_sync_conflicts
  AS RESTRICTIVE FOR ALL
  USING (fms.is_platform_context()
         OR EXISTS (SELECT 1 FROM fms.reservations p
                     WHERE p.id = fms.calendar_sync_conflicts.reservation_id));

CREATE TRIGGER trg_calendar_sync_conflicts_updated_at
  BEFORE UPDATE ON fms.calendar_sync_conflicts
  FOR EACH ROW EXECUTE FUNCTION fms.trg_set_updated_at();

CREATE TRIGGER trg_calendar_sync_conflicts_freeze_tenant
  BEFORE UPDATE ON fms.calendar_sync_conflicts
  FOR EACH ROW EXECUTE FUNCTION fms.trg_freeze_tenant_id();

COMMENT ON TABLE fms.calendar_sync_conflicts IS
  '雙向同步中兩邊都改了同一筆事件時的待審佇列（ADR-14 決定 I）。'
  '不自動選邊——跟 STALE_VERSION（412）同一個哲學：偵測到分歧就surface給人，不覆蓋。';

-- -----------------------------------------------------------------------------
-- 4. reservations 的去重索引（ADR-14 決定 I）
-- -----------------------------------------------------------------------------
-- 拉入 worker 用這個索引判斷「這個外部事件是不是已經對應到一列預約」，
-- 不沿用 fms.idempotency_keys——那張表是 HTTP 請求重放用的，24 小時就過期，
-- 形狀對不上「外部事件要跟內部預約永久關聯」這個需求。
CREATE UNIQUE INDEX uq_reservations_external_event
  ON fms.reservations (tenant_id, external_event_id)
  WHERE external_event_id IS NOT NULL;

COMMENT ON COLUMN fms.reservations.external_event_id IS
  'Outlook/Google calendar correlation。ADR-14 起真的有消費者——'
  'CalendarSyncWatchdog 用 uq_reservations_external_event 去重，'
  '推出時由 calendar 的 outbox 消費者寫回。';

-- -----------------------------------------------------------------------------
-- 5. 權限碼
-- -----------------------------------------------------------------------------
-- TENANT 範圍：日曆整合是租戶級的外部連線設定，不屬於任何單一場域
-- （即使 facility_id 有值也只是縮小適用範圍，不改變誰能設定它）。
-- is_dangerous = true：授予後能讀寫客戶的 Microsoft/Google 日曆資料，
-- 與 identity_provider:write 同一個危險等級。
--
-- 資源對應（POST .../resource-mappings）沿用同一個權限碼，不新增——
-- 能設定整合連線的人本來就該能決定它對應到哪些內部空間。
INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous)
VALUES
  ('calendar_integration:read',  'calendar_integration', 'read',  'ADMIN',
   '讀取日曆整合設定與同步狀態', 'TENANT', false),
  ('calendar_integration:write', 'calendar_integration', 'write', 'ADMIN',
   '設定 Microsoft 365／Google Workspace 日曆整合與資源對應', 'TENANT', true);

-- **新權限碼不會自動被既有角色持有** —— 008 建立 TENANT_ADMIN 時的萬用授予
-- （`p.code <> 'user:impersonate'`）只在那次 INSERT 當下對已存在的權限碼
-- 生效，083 是後來才加的，不會被追溯授予。與 071 的 alarm:suppress 同一個
-- 手法：搭在語意最接近的既有權限上（「能管其他外部系統連線的人，也能管
-- 日曆整合」），而不是列舉角色代碼。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT DISTINCT rp.role_id, c.code
  FROM fms.role_permissions rp
  JOIN fms.roles r ON r.id = rp.role_id
  CROSS JOIN (VALUES ('calendar_integration:read'), ('calendar_integration:write')) AS c(code)
 WHERE rp.permission_code = 'identity_provider:write'
   AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
BEGIN
  -- (1) 三張表都是 FORCE RLS。
  IF NOT EXISTS (
    SELECT 1 FROM pg_class
     WHERE oid = 'fms.calendar_integrations'::regclass AND relforcerowsecurity
  ) THEN
    RAISE EXCEPTION '083 FAILED: calendar_integrations 沒有 FORCE ROW LEVEL SECURITY';
  END IF;
  IF NOT EXISTS (
    SELECT 1 FROM pg_class
     WHERE oid = 'fms.calendar_resource_mappings'::regclass AND relforcerowsecurity
  ) THEN
    RAISE EXCEPTION '083 FAILED: calendar_resource_mappings 沒有 FORCE ROW LEVEL SECURITY';
  END IF;
  IF NOT EXISTS (
    SELECT 1 FROM pg_class
     WHERE oid = 'fms.calendar_sync_conflicts'::regclass AND relforcerowsecurity
  ) THEN
    RAISE EXCEPTION '083 FAILED: calendar_sync_conflicts 沒有 FORCE ROW LEVEL SECURITY';
  END IF;

  -- (1b) 兩張透過父表間接場域收斂的表都要有 facility_scope_via_parent——
  --      這是 rls_scope_audit_slice.rs 會抓到的洩漏（062 收斂的同一類）。
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
     WHERE schemaname = 'fms' AND tablename = 'calendar_resource_mappings'
       AND policyname = 'facility_scope_via_parent' AND permissive = 'RESTRICTIVE'
  ) THEN
    RAISE EXCEPTION
      '083 FAILED: calendar_resource_mappings 沒有 facility_scope_via_parent —— '
      '場域受限的讀者會看到指向別的場域空間節點的對應列';
  END IF;
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
     WHERE schemaname = 'fms' AND tablename = 'calendar_sync_conflicts'
       AND policyname = 'facility_scope_via_parent' AND permissive = 'RESTRICTIVE'
  ) THEN
    RAISE EXCEPTION
      '083 FAILED: calendar_sync_conflicts 沒有 facility_scope_via_parent —— '
      '場域受限的讀者會看到指向別的場域預約的衝突列';
  END IF;

  -- (2) 一個範圍一個 provider 只有一個連線。
  IF NOT EXISTS (
    SELECT 1 FROM pg_indexes
     WHERE schemaname = 'fms' AND indexname = 'uq_calendar_integrations_scope'
  ) THEN
    RAISE EXCEPTION '083 FAILED: uq_calendar_integrations_scope 不存在';
  END IF;

  -- (3) 一個內部節點在同一整合裡只能有一個生效中的對應。
  IF NOT EXISTS (
    SELECT 1 FROM pg_indexes
     WHERE schemaname = 'fms' AND indexname = 'uq_calendar_resource_mappings_node'
       AND indexdef LIKE '%ACTIVE%'
  ) THEN
    RAISE EXCEPTION '083 FAILED: uq_calendar_resource_mappings_node 不存在或沒有部分條件';
  END IF;

  -- (4) 衝突解決的一致性：resolved_at 與 resolved_by 必須同進退。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_calendar_sync_conflicts_resolution'
       AND conrelid = 'fms.calendar_sync_conflicts'::regclass
  ) THEN
    RAISE EXCEPTION '083 FAILED: ck_calendar_sync_conflicts_resolution 不存在';
  END IF;

  -- (5) reservations 的去重索引——CalendarSyncWatchdog 防迴圈的關鍵。
  IF NOT EXISTS (
    SELECT 1 FROM pg_indexes
     WHERE schemaname = 'fms' AND indexname = 'uq_reservations_external_event'
       AND indexdef LIKE '%external_event_id IS NOT NULL%'
  ) THEN
    RAISE EXCEPTION
      '083 FAILED: uq_reservations_external_event 不存在或沒有部分條件——'
      '拉入 worker 無法判斷外部事件是否已經對應到一列預約';
  END IF;

  -- (6) 權限碼存在，範圍與危險等級正確。
  IF NOT EXISTS (
    SELECT 1 FROM fms.permissions
     WHERE code = 'calendar_integration:write'
       AND min_scope_level = 'TENANT' AND is_dangerous
  ) THEN
    RAISE EXCEPTION
      '083 FAILED: calendar_integration:write 不存在、範圍不是 TENANT，或沒有標成危險';
  END IF;
  IF NOT EXISTS (
    SELECT 1 FROM fms.permissions
     WHERE code = 'calendar_integration:read' AND min_scope_level = 'TENANT'
  ) THEN
    RAISE EXCEPTION '083 FAILED: calendar_integration:read 不存在或範圍不是 TENANT';
  END IF;

  -- (7) TENANT_ADMIN 真的拿到新權限——新權限碼不會自動被既有角色持有，
  --     少了這個授予步驟，端點對租戶管理員也會回 403，而那個症狀
  --     完全不會指向「忘了授予」。
  IF NOT EXISTS (
    SELECT 1 FROM fms.role_permissions rp
    JOIN fms.roles r ON r.id = rp.role_id
    WHERE r.code = 'TENANT_ADMIN' AND r.tenant_id IS NULL
      AND rp.permission_code = 'calendar_integration:write'
  ) THEN
    RAISE EXCEPTION
      '083 FAILED: TENANT_ADMIN 沒有 calendar_integration:write —— '
      '新權限碼不會自動被既有角色持有，端點會對租戶管理員也回 403';
  END IF;

  RAISE NOTICE '083 OK: calendar_integrations／calendar_resource_mappings／'
               'calendar_sync_conflicts（FORCE RLS、去重索引）、'
               'reservations 的 external_event_id 去重索引、'
               'calendar_integration:read／write 權限碼';
END;
$$;

COMMIT;
