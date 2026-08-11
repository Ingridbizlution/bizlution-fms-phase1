-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 011: Offision 功能對標補強（Offision parity pack）
-- =============================================================================
-- 本檔補上與 Offision 會議室管理系統對照後確認的五個結構性缺口，以及
-- 一組讓前端能完整重現其使用者體驗的欄位與小型資料表。
--
--   A. 合併房間（Combinable Room）        — resource_compositions + shadow 預約
--   B. 視訊會議與外部行事曆整合            — integrations + reservations 欄位
--   C. 使用者預約配額（Quota）            — quota_policies / usage / transactions
--   D. 訪客管理（Visitor Management）      — visitors / invitations / access grants
--   E. 公告與廣播（Announcement/Broadcast） — announcements + reads
--
--   F. 週邊補強：附屬設備字典、資源收藏、私人／全天／即場預約、QR、
--      個人偏好、MFA 與信任裝置、固定座位、IoT 下行命令
--
-- 依賴：001–007（008、009 為種子，可在本檔之後重跑）
-- =============================================================================

BEGIN;

-- 本檔尾段會寫入 tenant_id IS NULL 的平台目錄列（amenities、notification_templates、
-- permissions）。這些列受本檔自己新建的 catalog RLS 政策約束
-- （WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())），
-- 未開平台情境會被拒絕（tenant_id IS NULL 不等於 NULL 的 current_tenant_id()）。
-- 008／009／012 都有同一行；本檔原先遺漏，導致 make migrate 在 amenities 失敗。
SELECT set_config('app.is_platform', 'on', true);

-- =============================================================================
-- A. 合併房間（Combinable Room）
-- =============================================================================
-- Offision 的行為：預約「A+B 大會議室」時，A 與 B 必須同時被鎖住；反之亦然。
--
-- 實作方式：組合關係存於 resource_compositions；預約父資源時，由 AFTER INSERT
-- 觸發器在同一交易內為每個子資源插入 shadow 預約列。既有的 GiST 排他約束
-- 因此自動生效 —— 不需要新的鎖，也不需要應用層記得檢查。
--
-- 兩個方向都被涵蓋：
--   訂 AB → 產生 A、B 的 shadow 列 → 若 A 已被訂走，shadow 插入違反排他約束，
--            整筆交易回滾（使用者得到 409，不會出現半套預約）
--   訂 A  → A 已有列 → 之後訂 AB 時 shadow 插入即失敗
-- =============================================================================

CREATE TABLE fms.resource_compositions (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id          uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id        uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  -- 複合資源（例：401+402 大會議室）
  parent_resource_id uuid NOT NULL REFERENCES fms.bookable_resources(id) ON DELETE CASCADE,
  -- 被佔用的實體子資源（例：401、402）
  child_resource_id  uuid NOT NULL REFERENCES fms.bookable_resources(id) ON DELETE CASCADE,
  sort_order         smallint NOT NULL DEFAULT 100,
  -- 合併後是否需要人員實際移除隔板；供服務工單自動連動使用
  requires_setup     boolean NOT NULL DEFAULT false,
  setup_service_item_id uuid REFERENCES fms.service_items(id) ON DELETE SET NULL,
  setup_minutes      integer NOT NULL DEFAULT 0,
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_composition_distinct CHECK (parent_resource_id <> child_resource_id)
);

CREATE UNIQUE INDEX uq_resource_compositions
  ON fms.resource_compositions (parent_resource_id, child_resource_id);
CREATE INDEX idx_resource_compositions_child
  ON fms.resource_compositions (child_resource_id);

COMMENT ON TABLE fms.resource_compositions IS
  '合併房間關係。父資源被預約時，觸發器會為每個子資源建立 shadow 預約，由排他約束保證互斥。';

-- 一個資源不可同時是父與子（避免多層組合造成 shadow 遞迴）
CREATE OR REPLACE FUNCTION fms.trg_composition_no_nesting()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF EXISTS (SELECT 1 FROM fms.resource_compositions
              WHERE child_resource_id = NEW.parent_resource_id) THEN
    RAISE EXCEPTION '資源 % 已是其他複合資源的子項，不支援多層組合', NEW.parent_resource_id
      USING ERRCODE = '23514';
  END IF;
  IF EXISTS (SELECT 1 FROM fms.resource_compositions
              WHERE parent_resource_id = NEW.child_resource_id) THEN
    RAISE EXCEPTION '資源 % 已是複合資源，不可再作為子項', NEW.child_resource_id
      USING ERRCODE = '23514';
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_resource_compositions_no_nesting
  BEFORE INSERT OR UPDATE ON fms.resource_compositions
  FOR EACH ROW EXECUTE FUNCTION fms.trg_composition_no_nesting();

-- 標記與 shadow 欄位
ALTER TABLE fms.bookable_resources
  ADD COLUMN is_composite boolean NOT NULL DEFAULT false,
  ADD COLUMN qr_payload   text,
  ADD COLUMN photo_attachment_id uuid REFERENCES fms.attachments(id) ON DELETE SET NULL;

COMMENT ON COLUMN fms.bookable_resources.is_composite IS
  '是否為合併房間（由 resource_compositions 維護，供列表查詢避免 EXISTS 子查詢）。';

ALTER TABLE fms.reservations
  ADD COLUMN parent_reservation_id uuid REFERENCES fms.reservations(id) ON DELETE CASCADE,
  ADD COLUMN is_shadow boolean NOT NULL DEFAULT false,
  ADD CONSTRAINT ck_reservation_shadow CHECK (
    (is_shadow AND parent_reservation_id IS NOT NULL)
    OR (NOT is_shadow AND parent_reservation_id IS NULL)
  );

CREATE INDEX idx_reservations_parent ON fms.reservations (parent_reservation_id)
  WHERE parent_reservation_id IS NOT NULL;

-- shadow 列不應出現在使用者的預約清單中；查詢一律加 WHERE NOT is_shadow，
-- 或直接使用此檢視。
CREATE OR REPLACE VIEW fms.v_reservations_visible AS
SELECT * FROM fms.reservations WHERE NOT is_shadow;

-- 建立父預約時，自動展開子資源 shadow 列
CREATE OR REPLACE FUNCTION fms.trg_expand_composition_shadows()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  c record;
  n smallint := 0;
BEGIN
  IF NEW.is_shadow THEN
    RETURN NULL;                     -- shadow 不再展開
  END IF;

  FOR c IN
    SELECT rc.child_resource_id, br.resource_type, br.spatial_node_id, br.asset_id
    FROM fms.resource_compositions rc
    JOIN fms.bookable_resources br ON br.id = rc.child_resource_id
    WHERE rc.parent_resource_id = NEW.bookable_resource_id
    ORDER BY rc.sort_order
  LOOP
    n := n + 1;
    INSERT INTO fms.reservations (
      tenant_id, facility_id, bookable_resource_id, reservation_no,
      resource_type, resource_id, organizer_id, org_id,
      title, purpose, party_size, start_at, end_at, status,
      approval_required, requires_check_in, created_via,
      parent_reservation_id, is_shadow)
    VALUES (
      NEW.tenant_id, NEW.facility_id, c.child_resource_id,
      NEW.reservation_no || '-C' || n::text,
      c.resource_type,
      coalesce(c.spatial_node_id, c.asset_id),
      NEW.organizer_id, NEW.org_id,
      NEW.title, NEW.purpose, NEW.party_size, NEW.start_at, NEW.end_at, NEW.status,
      false, false, NEW.created_via,
      NEW.id, true);
  END LOOP;

  RETURN NULL;
END;
$$;

CREATE TRIGGER trg_reservations_expand_shadows
  AFTER INSERT ON fms.reservations
  FOR EACH ROW EXECUTE FUNCTION fms.trg_expand_composition_shadows();

-- 父預約的狀態與時間變更必須同步到 shadow，否則排他約束會失去意義
CREATE OR REPLACE FUNCTION fms.trg_sync_composition_shadows()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF NEW.is_shadow THEN
    RETURN NULL;
  END IF;

  UPDATE fms.reservations
     SET status     = NEW.status,
         start_at   = NEW.start_at,
         end_at     = NEW.end_at,
         party_size = NEW.party_size,
         title      = NEW.title,
         cancelled_at = NEW.cancelled_at,
         cancelled_by = NEW.cancelled_by
   WHERE parent_reservation_id = NEW.id
     AND (status IS DISTINCT FROM NEW.status
       OR start_at IS DISTINCT FROM NEW.start_at
       OR end_at IS DISTINCT FROM NEW.end_at);

  RETURN NULL;
END;
$$;

CREATE TRIGGER trg_reservations_sync_shadows
  AFTER UPDATE OF status, start_at, end_at ON fms.reservations
  FOR EACH ROW EXECUTE FUNCTION fms.trg_sync_composition_shadows();

-- 維護 is_composite 旗標
CREATE OR REPLACE FUNCTION fms.trg_mark_composite_resource()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    UPDATE fms.bookable_resources br
       SET is_composite = EXISTS (
             SELECT 1 FROM fms.resource_compositions
             WHERE parent_resource_id = br.id)
     WHERE br.id = OLD.parent_resource_id;
    RETURN OLD;
  END IF;
  UPDATE fms.bookable_resources SET is_composite = true WHERE id = NEW.parent_resource_id;
  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_resource_compositions_flag
  AFTER INSERT OR DELETE ON fms.resource_compositions
  FOR EACH ROW EXECUTE FUNCTION fms.trg_mark_composite_resource();

-- =============================================================================
-- B. 視訊會議與外部行事曆整合
-- =============================================================================

CREATE TABLE fms.integrations (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id    uuid REFERENCES fms.facilities(id) ON DELETE CASCADE,  -- NULL = 全租戶
  provider       text NOT NULL
                   CHECK (provider IN ('MS_TEAMS','MS_365_CONTACTS','MS_EXCHANGE',
                                       'GOOGLE_MEET','GOOGLE_CALENDAR','ZOOM','WEBEX',
                                       'SLACK','LINE_NOTIFY','WEBHOOK','SMTP','SMS_GATEWAY')),
  display_name   varchar(150) NOT NULL,
  auth_type      text NOT NULL DEFAULT 'OAUTH2_APP'
                   CHECK (auth_type IN ('OAUTH2_APP','OAUTH2_USER','API_KEY','JWT','BASIC','NONE')),
  -- 密鑰一律只存參照，實際值放在 Secret Manager（與 identity_providers 同一原則）
  client_id      text,
  client_secret_ref text,
  external_tenant_ref text,
  scopes         text[] NOT NULL DEFAULT '{}',
  -- {"default_meeting_provider":true,"auto_create_link":true,"lobby_bypass":"organization"}
  settings       jsonb NOT NULL DEFAULT '{}'::jsonb,
  is_default_meeting_provider boolean NOT NULL DEFAULT false,
  status         text NOT NULL DEFAULT 'ACTIVE'
                   CHECK (status IN ('ACTIVE','DISABLED','ERROR','PENDING_AUTH')),
  last_success_at timestamptz,
  last_error     text,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at     timestamptz
);

CREATE UNIQUE INDEX uq_integrations_provider
  ON fms.integrations (tenant_id,
                       coalesce(facility_id, '00000000-0000-0000-0000-000000000000'::uuid),
                       provider)
  WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX uq_integrations_default_meeting
  ON fms.integrations (tenant_id)
  WHERE is_default_meeting_provider AND deleted_at IS NULL;

-- 個別使用者的委派授權（OAUTH2_USER，例如以本人身分建立 Teams 會議）
CREATE TABLE fms.integration_user_grants (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  integration_id uuid NOT NULL REFERENCES fms.integrations(id) ON DELETE CASCADE,
  user_id        uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  external_account varchar(200),
  token_ref      text NOT NULL,
  scopes         text[] NOT NULL DEFAULT '{}',
  expires_at     timestamptz,
  revoked_at     timestamptz,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_integration_user_grants
  ON fms.integration_user_grants (integration_id, user_id) WHERE revoked_at IS NULL;

ALTER TABLE fms.reservations
  ADD COLUMN online_meeting_provider text
       CHECK (online_meeting_provider IN ('MS_TEAMS','GOOGLE_MEET','ZOOM','WEBEX','OTHER')),
  ADD COLUMN online_meeting_id        varchar(200),
  ADD COLUMN online_meeting_join_url  text,
  ADD COLUMN online_meeting_passcode  varchar(60),
  ADD COLUMN online_meeting_dial_in   text,
  ADD COLUMN online_meeting_created_at timestamptz,
  -- 外部行事曆雙向同步狀態
  ADD COLUMN external_calendar_provider text
       CHECK (external_calendar_provider IN ('MS_EXCHANGE','GOOGLE_CALENDAR','OTHER')),
  ADD COLUMN external_sync_state text NOT NULL DEFAULT 'NOT_SYNCED'
       CHECK (external_sync_state IN ('NOT_SYNCED','PENDING','SYNCED','CONFLICT','FAILED')),
  ADD COLUMN external_sync_error text,
  ADD COLUMN external_synced_at timestamptz;

CREATE INDEX idx_reservations_online_meeting ON fms.reservations (tenant_id, online_meeting_provider)
  WHERE online_meeting_provider IS NOT NULL;
CREATE INDEX idx_reservations_sync_pending ON fms.reservations (external_sync_state)
  WHERE external_sync_state IN ('PENDING','FAILED');

COMMENT ON COLUMN fms.reservations.online_meeting_provider IS
  '線上會議平台。「在線會議率」績效指標即以此欄位非空的比率計算。';

-- =============================================================================
-- C. 使用者預約配額（Quota）
-- =============================================================================
-- 對應 Offision 的「配額使用歷史記錄」。適用收費場地與有次數限制的資源。
-- 設計為政策 + 指派 + 週期用量 + 明細帳三層，因此「為什麼我的配額不夠」永遠可回溯。
-- =============================================================================

CREATE TABLE fms.quota_policies (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id   uuid REFERENCES fms.facilities(id) ON DELETE CASCADE,
  code          varchar(50) NOT NULL,
  name          varchar(150) NOT NULL,
  description   text,
  -- 適用範圍：資源類型／空間型別／分類／指定資源
  applies_to    text NOT NULL DEFAULT 'RESOURCE_TYPE'
                  CHECK (applies_to IN ('ALL','RESOURCE_TYPE','NODE_TYPE','SERVICE_CATEGORY','RESOURCE')),
  applies_to_value varchar(60),
  bookable_resource_id uuid REFERENCES fms.bookable_resources(id) ON DELETE CASCADE,
  -- 計量方式
  metric        text NOT NULL DEFAULT 'BOOKING_COUNT'
                  CHECK (metric IN ('BOOKING_COUNT','BOOKING_HOURS','SERVICE_AMOUNT','NO_SHOW_COUNT')),
  period        text NOT NULL DEFAULT 'MONTH'
                  CHECK (period IN ('DAY','WEEK','MONTH','QUARTER','YEAR','ROLLING_30D')),
  allowance     numeric(14,2) NOT NULL,
  -- 硬上限：超額直接拒絕；軟上限：允許但標記並通知
  is_hard_limit boolean NOT NULL DEFAULT true,
  allow_carry_over boolean NOT NULL DEFAULT false,
  warn_at_pct   numeric(5,2) NOT NULL DEFAULT 80,
  -- 取消是否退還配額，以及提前多久取消才退還
  refund_on_cancel boolean NOT NULL DEFAULT true,
  refund_cutoff_minutes integer NOT NULL DEFAULT 60,
  -- 缺席是否仍然計入（Offision 的 no-show 治理常搭配此設定）
  charge_on_no_show boolean NOT NULL DEFAULT true,
  is_active     boolean NOT NULL DEFAULT true,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_quota_allowance CHECK (allowance >= 0)
);

CREATE UNIQUE INDEX uq_quota_policies_code ON fms.quota_policies (tenant_id, lower(code));
CREATE INDEX idx_quota_policies_active ON fms.quota_policies (tenant_id, facility_id)
  WHERE is_active;

-- 政策指派給誰：使用者、組織子樹或角色
CREATE TABLE fms.quota_assignments (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id       uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  quota_policy_id uuid NOT NULL REFERENCES fms.quota_policies(id) ON DELETE CASCADE,
  subject_type    text NOT NULL CHECK (subject_type IN ('USER','ORG','ROLE','ALL_USERS')),
  subject_id      uuid,
  role_code       varchar(50),
  -- 個別覆寫額度（VIP、專案期間加倍等）
  override_allowance numeric(14,2),
  valid_from      timestamptz NOT NULL DEFAULT clock_timestamp(),
  valid_until     timestamptz,
  priority        integer NOT NULL DEFAULT 100,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_quota_assignment_subject CHECK (
    (subject_type = 'ALL_USERS' AND subject_id IS NULL AND role_code IS NULL)
    OR (subject_type = 'ROLE' AND role_code IS NOT NULL)
    OR (subject_type IN ('USER','ORG') AND subject_id IS NOT NULL)
  )
);

CREATE INDEX idx_quota_assignments_lookup
  ON fms.quota_assignments (tenant_id, subject_type, subject_id);

-- 週期用量（upsert 熱點，供即時判定）
CREATE TABLE fms.quota_usage (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id       uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  quota_policy_id uuid NOT NULL REFERENCES fms.quota_policies(id) ON DELETE CASCADE,
  user_id         uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  period_key      varchar(20) NOT NULL,          -- '2026-08'、'2026-W32'、'2026-08-05'
  allowance_snapshot numeric(14,2) NOT NULL,
  used            numeric(14,2) NOT NULL DEFAULT 0,
  warned_at       timestamptz,
  updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_quota_usage_nonneg CHECK (used >= 0)
);

CREATE UNIQUE INDEX uq_quota_usage
  ON fms.quota_usage (quota_policy_id, user_id, period_key);
CREATE INDEX idx_quota_usage_user ON fms.quota_usage (tenant_id, user_id, period_key);

-- 明細帳（append-only）— 這就是 Offision「配額使用歷史記錄」頁面的資料來源
CREATE TABLE fms.quota_transactions (
  id              bigserial PRIMARY KEY,
  tenant_id       uuid NOT NULL,
  quota_policy_id uuid NOT NULL REFERENCES fms.quota_policies(id) ON DELETE CASCADE,
  user_id         uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  period_key      varchar(20) NOT NULL,
  reason          text NOT NULL
                    CHECK (reason IN ('CONSUME','REFUND','ADJUST','NO_SHOW_CHARGE','PERIOD_RESET')),
  delta           numeric(14,2) NOT NULL,
  balance_after   numeric(14,2) NOT NULL,
  reservation_id  uuid REFERENCES fms.reservations(id) ON DELETE SET NULL,
  work_order_id   uuid REFERENCES fms.work_orders(id) ON DELETE SET NULL,
  note            varchar(250),
  actor_user_id   uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  occurred_at     timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX idx_quota_tx_user ON fms.quota_transactions (tenant_id, user_id, occurred_at DESC);
CREATE INDEX idx_quota_tx_reservation ON fms.quota_transactions (reservation_id)
  WHERE reservation_id IS NOT NULL;

ALTER TABLE fms.bookable_resources
  ADD COLUMN quota_policy_id uuid REFERENCES fms.quota_policies(id) ON DELETE SET NULL;

-- 期間鍵：把時間點正規化成配額週期
CREATE OR REPLACE FUNCTION fms.quota_period_key(p_period text, p_at timestamptz)
RETURNS varchar
LANGUAGE sql STABLE
AS $$
  SELECT CASE p_period
    WHEN 'DAY'         THEN to_char(p_at, 'YYYY-MM-DD')
    WHEN 'WEEK'        THEN to_char(p_at, 'IYYY"-W"IW')
    WHEN 'MONTH'       THEN to_char(p_at, 'YYYY-MM')
    WHEN 'QUARTER'     THEN to_char(p_at, 'YYYY"-Q"Q')
    WHEN 'YEAR'        THEN to_char(p_at, 'YYYY')
    WHEN 'ROLLING_30D' THEN 'ROLLING'
    ELSE to_char(p_at, 'YYYY-MM')
  END;
$$;

-- 解析某使用者在某政策下的有效額度（取最高優先序的指派，允許覆寫）
CREATE OR REPLACE FUNCTION fms.resolve_quota_allowance(
  p_policy_id uuid,
  p_user_id   uuid
) RETURNS numeric
LANGUAGE sql STABLE
AS $$
  SELECT coalesce(
    (SELECT coalesce(qa.override_allowance, qp.allowance)
       FROM fms.quota_assignments qa
       JOIN fms.quota_policies qp ON qp.id = qa.quota_policy_id
       LEFT JOIN fms.users u ON u.id = p_user_id
       LEFT JOIN fms.organizations o_user  ON o_user.id = u.primary_org_id
       LEFT JOIN fms.organizations o_scope ON o_scope.id = qa.subject_id
      WHERE qa.quota_policy_id = p_policy_id
        AND qa.valid_from <= clock_timestamp()
        AND (qa.valid_until IS NULL OR qa.valid_until > clock_timestamp())
        AND (
              qa.subject_type = 'ALL_USERS'
          OR (qa.subject_type = 'USER' AND qa.subject_id = p_user_id)
          OR (qa.subject_type = 'ORG'  AND o_scope.org_path IS NOT NULL
              AND o_user.org_path IS NOT NULL
              AND o_user.org_path OPERATOR(public.<@) o_scope.org_path)
          OR (qa.subject_type = 'ROLE' AND EXISTS (
                SELECT 1 FROM fms.user_role_assignments ura
                JOIN fms.roles r ON r.id = ura.role_id
                WHERE ura.user_id = p_user_id AND r.code = qa.role_code))
        )
      ORDER BY qa.priority, qa.override_allowance DESC NULLS LAST
      LIMIT 1),
    NULL);
$$;

-- 扣減配額。硬上限超額時拋出例外，讓整筆預約交易回滾。
CREATE OR REPLACE FUNCTION fms.consume_quota(
  p_policy_id     uuid,
  p_user_id       uuid,
  p_amount        numeric,
  p_reservation_id uuid DEFAULT NULL,
  p_at            timestamptz DEFAULT NULL,
  p_reason        text DEFAULT 'CONSUME'
) RETURNS numeric
LANGUAGE plpgsql
AS $$
DECLARE
  v_policy    fms.quota_policies;
  v_key       varchar(20);
  v_allowance numeric(14,2);
  v_used      numeric(14,2);
BEGIN
  SELECT * INTO v_policy FROM fms.quota_policies WHERE id = p_policy_id AND is_active;
  IF v_policy.id IS NULL THEN
    RETURN NULL;                       -- 未設定配額 = 不限制
  END IF;

  v_allowance := fms.resolve_quota_allowance(p_policy_id, p_user_id);
  IF v_allowance IS NULL THEN
    RETURN NULL;                       -- 此使用者不在該政策的適用對象內
  END IF;

  v_key := fms.quota_period_key(v_policy.period, coalesce(p_at, clock_timestamp()));

  INSERT INTO fms.quota_usage AS qu (tenant_id, quota_policy_id, user_id, period_key,
                                    allowance_snapshot, used)
  VALUES (v_policy.tenant_id, p_policy_id, p_user_id, v_key, v_allowance, greatest(p_amount, 0))
  ON CONFLICT (quota_policy_id, user_id, period_key) DO UPDATE
    SET used = qu.used + p_amount,
        allowance_snapshot = v_allowance,
        updated_at = clock_timestamp()
  RETURNING used INTO v_used;

  IF v_policy.is_hard_limit AND v_used > v_allowance THEN
    RAISE EXCEPTION '配額不足：政策 % 於 % 期間已用 %/%（本次需 %）',
      v_policy.code, v_key, v_used - p_amount, v_allowance, p_amount
      USING ERRCODE = '23514', HINT = 'QUOTA_EXCEEDED';
  END IF;

  INSERT INTO fms.quota_transactions (tenant_id, quota_policy_id, user_id, period_key,
                                      reason, delta, balance_after, reservation_id, actor_user_id)
  VALUES (v_policy.tenant_id, p_policy_id, p_user_id, v_key,
          p_reason, p_amount, v_allowance - v_used, p_reservation_id, fms.current_user_id());

  IF v_used >= v_allowance * v_policy.warn_at_pct / 100 THEN
    UPDATE fms.quota_usage SET warned_at = coalesce(warned_at, clock_timestamp())
     WHERE quota_policy_id = p_policy_id AND user_id = p_user_id AND period_key = v_key;
  END IF;

  RETURN v_allowance - v_used;
END;
$$;

COMMENT ON FUNCTION fms.consume_quota IS
  '配額扣減的唯一入口。在預約交易內呼叫；硬上限超額時以 ERRCODE 23514 + HINT QUOTA_EXCEEDED 中斷整筆交易。';

-- =============================================================================
-- D. 訪客管理（Visitor Management）
-- =============================================================================
-- 注意：不儲存證件號碼等個資明文，只保留 *_ref 參照與到訪必要欄位。
-- =============================================================================

ALTER TABLE fms.facilities
  ADD COLUMN visitor_management_enabled boolean NOT NULL DEFAULT false,
  ADD COLUMN visitor_policy jsonb NOT NULL DEFAULT '{}'::jsonb;

COMMENT ON COLUMN fms.facilities.visitor_management_enabled IS
  '逐棟開關訪客管理（對應 Offision 的 per-building 訪客控制）。';

CREATE TABLE fms.visitors (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  full_name      varchar(150) NOT NULL,
  email          citext,
  phone          varchar(40),
  company        varchar(200),
  -- 證件僅存參照與末四碼，實際影像／號碼放在受控儲存體
  id_document_type varchar(30),
  id_document_ref  text,
  id_last4       varchar(8),
  photo_attachment_id uuid REFERENCES fms.attachments(id) ON DELETE SET NULL,
  is_blocklisted boolean NOT NULL DEFAULT false,
  blocklist_reason varchar(250),
  visit_count    integer NOT NULL DEFAULT 0,
  last_visit_at  timestamptz,
  notes          text,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at     timestamptz
);

CREATE INDEX idx_visitors_email ON fms.visitors (tenant_id, email) WHERE email IS NOT NULL;
CREATE INDEX idx_visitors_name_trgm ON fms.visitors USING gin (full_name gin_trgm_ops);
CREATE INDEX idx_visitors_blocklist ON fms.visitors (tenant_id) WHERE is_blocklisted;

CREATE TABLE fms.visitor_invitations (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id       uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id     uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  invitation_no   varchar(40) NOT NULL,
  visitor_id      uuid REFERENCES fms.visitors(id) ON DELETE SET NULL,
  -- 走進來的臨時訪客（walk-in）可能還沒有 visitor 主檔
  visitor_name    varchar(150) NOT NULL,
  visitor_email   citext,
  visitor_phone   varchar(40),
  visitor_company varchar(200),
  host_user_id    uuid NOT NULL REFERENCES fms.users(id) ON DELETE RESTRICT,
  reservation_id  uuid REFERENCES fms.reservations(id) ON DELETE SET NULL,
  spatial_node_id uuid REFERENCES fms.spatial_nodes(id) ON DELETE SET NULL,
  visit_type      text NOT NULL DEFAULT 'MEETING'
                    CHECK (visit_type IN ('MEETING','INTERVIEW','DELIVERY','CONTRACTOR',
                                          'AUDIT','EVENT','TOUR','OTHER')),
  purpose         varchar(250),
  expected_arrival_at   timestamptz NOT NULL,
  expected_departure_at timestamptz,
  -- QR / 通行碼（報到與門禁用）
  invitation_code varchar(40) NOT NULL,
  qr_payload      text,
  -- 可通行的空間範圍；空陣列表示僅接待區
  allowed_node_ids uuid[] NOT NULL DEFAULT '{}',
  status          text NOT NULL DEFAULT 'INVITED'
                    CHECK (status IN ('DRAFT','INVITED','PRE_REGISTERED','ARRIVED','CHECKED_IN',
                                      'CHECKED_OUT','NO_SHOW','CANCELLED','DENIED')),
  -- 報到 / 離場
  pre_registered_at timestamptz,
  checked_in_at   timestamptz,
  checked_in_by   uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  check_in_method text CHECK (check_in_method IN ('RECEPTION','KIOSK','QR','MOBILE','ACCESS_CONTROL')),
  checked_out_at  timestamptz,
  badge_no        varchar(40),
  -- 合規：保密協議與安全宣導
  nda_required    boolean NOT NULL DEFAULT false,
  nda_signed_at   timestamptz,
  nda_attachment_id uuid REFERENCES fms.attachments(id) ON DELETE SET NULL,
  safety_briefing_at timestamptz,
  host_notified_at timestamptz,
  denial_reason   varchar(250),
  attributes      jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_by      uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_visit_window CHECK (expected_departure_at IS NULL
                                    OR expected_departure_at >= expected_arrival_at)
);

CREATE UNIQUE INDEX uq_visitor_invitations_no ON fms.visitor_invitations (tenant_id, invitation_no);
CREATE UNIQUE INDEX uq_visitor_invitations_code ON fms.visitor_invitations (invitation_code);
CREATE INDEX idx_visitor_invitations_day
  ON fms.visitor_invitations (facility_id, expected_arrival_at);
CREATE INDEX idx_visitor_invitations_host ON fms.visitor_invitations (host_user_id, expected_arrival_at DESC);
CREATE INDEX idx_visitor_invitations_onsite ON fms.visitor_invitations (facility_id)
  WHERE status IN ('CHECKED_IN','ARRIVED');
CREATE INDEX idx_visitor_invitations_reservation ON fms.visitor_invitations (reservation_id)
  WHERE reservation_id IS NOT NULL;

-- 門禁授權（與 device_commands 搭配，實現「報到後自動開門」）
CREATE TABLE fms.visitor_access_grants (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id       uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  invitation_id   uuid NOT NULL REFERENCES fms.visitor_invitations(id) ON DELETE CASCADE,
  spatial_node_id uuid REFERENCES fms.spatial_nodes(id) ON DELETE CASCADE,
  device_id       uuid REFERENCES fms.devices(id) ON DELETE CASCADE,
  credential_type text NOT NULL DEFAULT 'QR'
                    CHECK (credential_type IN ('QR','PIN','NFC_CARD','FACE','MOBILE_BLE')),
  credential_ref  text,
  valid_from      timestamptz NOT NULL,
  valid_until     timestamptz NOT NULL,
  status          text NOT NULL DEFAULT 'ACTIVE'
                    CHECK (status IN ('ACTIVE','USED','EXPIRED','REVOKED')),
  used_at         timestamptz,
  revoked_at      timestamptz,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_grant_window CHECK (valid_until > valid_from)
);

CREATE INDEX idx_visitor_grants_invitation ON fms.visitor_access_grants (invitation_id);
CREATE INDEX idx_visitor_grants_active ON fms.visitor_access_grants (valid_until)
  WHERE status = 'ACTIVE';

-- 訪客到訪統計回寫
CREATE OR REPLACE FUNCTION fms.trg_visitor_visit_stats()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF NEW.status = 'CHECKED_IN' AND OLD.status IS DISTINCT FROM 'CHECKED_IN'
     AND NEW.visitor_id IS NOT NULL THEN
    UPDATE fms.visitors
       SET visit_count = visit_count + 1,
           last_visit_at = coalesce(NEW.checked_in_at, clock_timestamp())
     WHERE id = NEW.visitor_id;
    PERFORM fms.emit_event(NEW.tenant_id, 'visitor.checked_in', 'VISITOR_INVITATION', NEW.id,
      jsonb_build_object('visitor_name', NEW.visitor_name, 'host_user_id', NEW.host_user_id,
                         'facility_id', NEW.facility_id, 'reservation_id', NEW.reservation_id));
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER trg_visitor_invitations_stats
  AFTER UPDATE OF status ON fms.visitor_invitations
  FOR EACH ROW EXECUTE FUNCTION fms.trg_visitor_visit_stats();

CREATE TRIGGER trg_visitor_invitations_updated_at
  BEFORE UPDATE ON fms.visitor_invitations
  FOR EACH ROW EXECUTE FUNCTION fms.trg_set_updated_at();

-- =============================================================================
-- E. 公告與廣播（Announcement / Broadcast）
-- =============================================================================

CREATE TABLE fms.announcements (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  -- 投放範圍：NULL facility = 全租戶；org_scope_path 可限定組織子樹
  facility_id   uuid REFERENCES fms.facilities(id) ON DELETE CASCADE,
  org_scope_path ltree,
  target_node_ids uuid[] NOT NULL DEFAULT '{}',
  target_role_codes text[] NOT NULL DEFAULT '{}',
  title         varchar(250) NOT NULL,
  body          text NOT NULL,
  summary       varchar(400),
  category      text NOT NULL DEFAULT 'GENERAL'
                  CHECK (category IN ('GENERAL','MAINTENANCE','EMERGENCY','EVENT',
                                      'POLICY','IT','FACILITY','HR')),
  severity      text NOT NULL DEFAULT 'INFO'
                  CHECK (severity IN ('INFO','NOTICE','WARNING','CRITICAL')),
  -- 投放通道：站內、Email、電子標牌、門口面板
  channels      text[] NOT NULL DEFAULT '{IN_APP}',
  show_as_banner boolean NOT NULL DEFAULT false,
  is_pinned     boolean NOT NULL DEFAULT false,
  requires_acknowledgement boolean NOT NULL DEFAULT false,
  attachment_ids uuid[] NOT NULL DEFAULT '{}',
  publish_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  expire_at     timestamptz,
  status        text NOT NULL DEFAULT 'DRAFT'
                  CHECK (status IN ('DRAFT','SCHEDULED','PUBLISHED','EXPIRED','ARCHIVED')),
  -- 統計快取，由 worker 更新
  read_count    integer NOT NULL DEFAULT 0,
  ack_count     integer NOT NULL DEFAULT 0,
  audience_size integer,
  created_by    uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_announcement_window CHECK (expire_at IS NULL OR expire_at > publish_at)
);

CREATE INDEX idx_announcements_live
  ON fms.announcements (tenant_id, publish_at DESC)
  WHERE status = 'PUBLISHED';
CREATE INDEX idx_announcements_facility ON fms.announcements (facility_id, publish_at DESC);
CREATE INDEX idx_announcements_scope ON fms.announcements USING gist (org_scope_path)
  WHERE org_scope_path IS NOT NULL;
CREATE INDEX idx_announcements_schedule ON fms.announcements (publish_at)
  WHERE status = 'SCHEDULED';

CREATE TABLE fms.announcement_reads (
  announcement_id uuid NOT NULL REFERENCES fms.announcements(id) ON DELETE CASCADE,
  user_id         uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  tenant_id       uuid NOT NULL,
  read_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  acknowledged_at timestamptz,
  PRIMARY KEY (announcement_id, user_id)
);

CREATE INDEX idx_announcement_reads_user ON fms.announcement_reads (user_id, read_at DESC);

CREATE TRIGGER trg_announcements_updated_at
  BEFORE UPDATE ON fms.announcements
  FOR EACH ROW EXECUTE FUNCTION fms.trg_set_updated_at();

-- =============================================================================
-- F. 週邊補強
-- =============================================================================

-- F1. 附屬設備字典（投影機、螢幕、Wi-Fi、視訊會議、電視、麥克風…）
CREATE TABLE fms.amenities (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid REFERENCES fms.tenants(id) ON DELETE CASCADE,   -- NULL = 平台預設
  code        varchar(50) NOT NULL,
  name        varchar(120) NOT NULL,
  name_en     varchar(120),
  category    text NOT NULL DEFAULT 'AV'
                CHECK (category IN ('AV','CONNECTIVITY','FURNITURE','ACCESSIBILITY',
                                    'COMFORT','SAFETY','CATERING','OTHER')),
  icon        varchar(60),
  -- 若該設備同時是需要保養的資產分類，可連結以便報修時自動帶入
  asset_category_id uuid REFERENCES fms.asset_categories(id) ON DELETE SET NULL,
  is_filterable boolean NOT NULL DEFAULT true,
  display_order integer NOT NULL DEFAULT 100,
  is_active   boolean NOT NULL DEFAULT true,
  created_at  timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_amenities_code
  ON fms.amenities (coalesce(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid), lower(code));

CREATE TABLE fms.resource_amenities (
  bookable_resource_id uuid NOT NULL REFERENCES fms.bookable_resources(id) ON DELETE CASCADE,
  amenity_id  uuid NOT NULL REFERENCES fms.amenities(id) ON DELETE CASCADE,
  tenant_id   uuid NOT NULL,
  quantity    smallint NOT NULL DEFAULT 1,
  -- 對應的實體資產（若該設備需要維護）
  asset_id    uuid REFERENCES fms.assets(id) ON DELETE SET NULL,
  is_operational boolean NOT NULL DEFAULT true,
  note        varchar(200),
  PRIMARY KEY (bookable_resource_id, amenity_id)
);

CREATE INDEX idx_resource_amenities_amenity ON fms.resource_amenities (amenity_id)
  WHERE is_operational;

COMMENT ON TABLE fms.resource_amenities IS
  '資源↔附屬設備。以 join table 而非 text[] 實作，因此設施篩選、i18n 與「投影機故障→該房間暫不可用」的連動都能一致處理。';

-- F2. 資源收藏
CREATE TABLE fms.user_favorites (
  user_id     uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  tenant_id   uuid NOT NULL,
  entity_type text NOT NULL CHECK (entity_type IN ('BOOKABLE_RESOURCE','SPATIAL_NODE','ASSET','SERVICE_ITEM')),
  entity_id   uuid NOT NULL,
  sort_order  smallint NOT NULL DEFAULT 100,
  created_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (user_id, entity_type, entity_id)
);

CREATE INDEX idx_user_favorites_tenant ON fms.user_favorites (tenant_id, entity_type);

-- F3. 預約旗標與備註（私人 / 全天 / 即場）
ALTER TABLE fms.reservations
  ADD COLUMN is_private   boolean NOT NULL DEFAULT false,
  ADD COLUMN is_all_day   boolean NOT NULL DEFAULT false,
  ADD COLUMN is_walk_in   boolean NOT NULL DEFAULT false,
  ADD COLUMN description  text,
  ADD COLUMN quota_transaction_id bigint;

COMMENT ON COLUMN fms.reservations.is_private IS
  '私人預約：非本人／非管理員只看得到「已預約」與時段，看不到標題、與會者與備註。由 API 層遮罩，並在 audit_log 記錄任何越權查看嘗試。';
COMMENT ON COLUMN fms.reservations.is_walk_in IS
  '即場預約（面板現場點選）。Dashboard 的「預約類型」環形圖以 一般／重複／即場 分類，即以此欄位與 recurrence_group_id 判定。';

CREATE INDEX idx_reservations_walk_in ON fms.reservations (tenant_id, start_at)
  WHERE is_walk_in;

-- F4. 空間節點 QR（掃碼快速預約 / 報修）
ALTER TABLE fms.spatial_nodes
  ADD COLUMN qr_payload text,
  ADD COLUMN floor_plan_attachment_id uuid REFERENCES fms.attachments(id) ON DELETE SET NULL;

CREATE UNIQUE INDEX uq_spatial_nodes_qr ON fms.spatial_nodes (qr_payload)
  WHERE qr_payload IS NOT NULL;

-- F5. 個人偏好
ALTER TABLE fms.users
  ADD COLUMN ui_preferences jsonb NOT NULL DEFAULT
    '{"theme":"system","accent_color":null,"font_scale":1.0,"time_format":"24H","week_start":1,"default_view":"home"}'::jsonb;

-- F6. MFA 因子與信任裝置
CREATE TABLE fms.user_mfa_factors (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  user_id     uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  factor_type text NOT NULL CHECK (factor_type IN ('TOTP','SMS','EMAIL','PUSH','RECOVERY_CODES')),
  -- 種子與備援碼一律只存參照
  secret_ref  text,
  label       varchar(80),
  is_primary  boolean NOT NULL DEFAULT false,
  confirmed_at timestamptz,
  last_used_at timestamptz,
  revoked_at  timestamptz,
  created_at  timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_user_mfa_primary ON fms.user_mfa_factors (user_id)
  WHERE is_primary AND revoked_at IS NULL;
CREATE INDEX idx_user_mfa_user ON fms.user_mfa_factors (user_id) WHERE revoked_at IS NULL;

CREATE TABLE fms.user_trusted_devices (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  user_id       uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  device_hash   char(64) NOT NULL,
  device_label  varchar(120),
  user_agent    text,
  last_ip       inet,
  trusted_until timestamptz NOT NULL,
  last_used_at  timestamptz,
  revoked_at    timestamptz,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_trusted_devices ON fms.user_trusted_devices (user_id, device_hash)
  WHERE revoked_at IS NULL;

-- F7. 固定座位（Fixed Desk）
CREATE TABLE fms.desk_assignments (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id       uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id     uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  spatial_node_id uuid NOT NULL REFERENCES fms.spatial_nodes(id) ON DELETE CASCADE,
  user_id         uuid REFERENCES fms.users(id) ON DELETE CASCADE,
  org_id          uuid REFERENCES fms.organizations(id) ON DELETE CASCADE,
  assignment_type text NOT NULL DEFAULT 'PERMANENT'
                    CHECK (assignment_type IN ('PERMANENT','TEMPORARY','SHARED','RESERVED_POOL')),
  valid_from      date NOT NULL DEFAULT current_date,
  valid_until     date,
  valid_range     daterange GENERATED ALWAYS AS
                    (daterange(valid_from, valid_until, '[)')) STORED,
  note            varchar(200),
  created_by      uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT ck_desk_assignment_subject CHECK (user_id IS NOT NULL OR org_id IS NOT NULL),
  CONSTRAINT ck_desk_assignment_range CHECK (valid_until IS NULL OR valid_until > valid_from)
);

-- 同一座位在同一期間只能有一個 PERMANENT 指派
ALTER TABLE fms.desk_assignments
  ADD CONSTRAINT excl_desk_assignment_overlap
  EXCLUDE USING gist (
    spatial_node_id WITH =,
    valid_range WITH &&
  ) WHERE (assignment_type = 'PERMANENT');

CREATE INDEX idx_desk_assignments_user ON fms.desk_assignments (user_id, valid_from DESC);
CREATE INDEX idx_desk_assignments_node ON fms.desk_assignments (spatial_node_id);

-- F8. IoT 下行命令（門鎖、燈光、空調、標牌）
CREATE TABLE fms.device_commands (
  id             bigserial PRIMARY KEY,
  tenant_id      uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id    uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  device_id      uuid NOT NULL REFERENCES fms.devices(id) ON DELETE CASCADE,
  command        text NOT NULL
                   CHECK (command IN ('UNLOCK','LOCK','SET_TEMPERATURE','SET_LIGHT',
                                      'SET_HVAC_MODE','DISPLAY_MESSAGE','CLEAR_DISPLAY',
                                      'REBOOT','IDENTIFY','SYNC_SCHEDULE')),
  payload        jsonb NOT NULL DEFAULT '{}'::jsonb,
  -- 命令來源可追溯：誰／哪張預約／哪條告警觸發了開門
  source         text NOT NULL DEFAULT 'USER'
                   CHECK (source IN ('USER','RESERVATION','VISITOR','SCHEDULE','ALARM','WORK_ORDER','SYSTEM')),
  reservation_id uuid REFERENCES fms.reservations(id) ON DELETE SET NULL,
  visitor_invitation_id uuid REFERENCES fms.visitor_invitations(id) ON DELETE SET NULL,
  work_order_id  uuid REFERENCES fms.work_orders(id) ON DELETE SET NULL,
  requested_by   uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  status         text NOT NULL DEFAULT 'PENDING'
                   CHECK (status IN ('PENDING','SENT','ACKED','FAILED','EXPIRED','CANCELLED')),
  attempt_count  smallint NOT NULL DEFAULT 0,
  expires_at     timestamptz NOT NULL DEFAULT clock_timestamp() + interval '2 minutes',
  sent_at        timestamptz,
  acked_at       timestamptz,
  error_message  text,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX idx_device_commands_queue ON fms.device_commands (device_id, status, created_at)
  WHERE status IN ('PENDING','SENT');
CREATE INDEX idx_device_commands_audit ON fms.device_commands (tenant_id, created_at DESC);
CREATE INDEX idx_device_commands_reservation ON fms.device_commands (reservation_id)
  WHERE reservation_id IS NOT NULL;

COMMENT ON TABLE fms.device_commands IS
  '設備下行命令佇列（append-only）。門鎖開啟等具安全意義的動作必須留下來源與請求者，因此不允許 UPDATE 覆寫歷史，只更新狀態欄位。';

-- 電子標牌 / 門口面板的顯示內容來源
CREATE TABLE fms.signage_displays (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  facility_id   uuid NOT NULL REFERENCES fms.facilities(id) ON DELETE CASCADE,
  device_id     uuid REFERENCES fms.devices(id) ON DELETE SET NULL,
  spatial_node_id uuid REFERENCES fms.spatial_nodes(id) ON DELETE SET NULL,
  display_type  text NOT NULL DEFAULT 'ROOM_PANEL'
                  CHECK (display_type IN ('ROOM_PANEL','DESK_PANEL','WAYFINDING','LOBBY_BOARD','KIOSK')),
  code          varchar(60) NOT NULL,
  name          varchar(150) NOT NULL,
  -- 面板能力：現場預約、延長、提早釋放、報修
  allow_walk_in_booking boolean NOT NULL DEFAULT true,
  allow_extend    boolean NOT NULL DEFAULT true,
  allow_release   boolean NOT NULL DEFAULT true,
  allow_report_issue boolean NOT NULL DEFAULT true,
  allow_door_unlock boolean NOT NULL DEFAULT false,
  -- 顯示設定與輪播內容
  layout        jsonb NOT NULL DEFAULT '{}'::jsonb,
  announcement_channel boolean NOT NULL DEFAULT true,
  pairing_code  varchar(20),
  paired_at     timestamptz,
  last_seen_at  timestamptz,
  status        text NOT NULL DEFAULT 'UNPAIRED'
                  CHECK (status IN ('UNPAIRED','ONLINE','OFFLINE','DISABLED')),
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE UNIQUE INDEX uq_signage_displays_code ON fms.signage_displays (tenant_id, lower(code));
CREATE INDEX idx_signage_displays_node ON fms.signage_displays (spatial_node_id);

-- =============================================================================
-- G. 可用性判定：納入合併房間與固定座位
-- =============================================================================
-- 取代 005 的版本。新增行為：
--   * 父資源（合併房間）會一併檢查所有子資源
--   * 子資源會受到父資源 shadow 列的阻擋（既有排他約束已保證，此處僅為一致回報）
--   * 固定座位在指派期間內不接受他人預約
-- =============================================================================

-- 簽章改變（新增 p_user_id），必須先移除 005 的四參數版本，否則四引數呼叫會變成歧義
DROP FUNCTION IF EXISTS fms.check_resource_availability(uuid, timestamptz, timestamptz, uuid);

CREATE FUNCTION fms.check_resource_availability(
  p_resource_id uuid,
  p_start_at    timestamptz,
  p_end_at      timestamptz,
  p_exclude_reservation_id uuid DEFAULT NULL,
  p_user_id     uuid DEFAULT NULL
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
  v_ids    uuid[];
  v_count  integer;
  v_conflict_id uuid;
  v_detail text;
BEGIN
  SELECT * INTO v_res FROM fms.bookable_resources
   WHERE spatial_node_id = p_resource_id OR asset_id = p_resource_id
   LIMIT 1;

  IF v_res.id IS NULL THEN
    RETURN QUERY SELECT false, 'NOT_BOOKABLE'::text, NULL::uuid, '資源未設定為可預約'::text;
    RETURN;
  END IF;

  IF NOT v_res.is_bookable THEN
    RETURN QUERY SELECT false, 'NOT_BOOKABLE'::text, v_res.id, '該資源已停用預約'::text;
    RETURN;
  END IF;

  -- 需要一併檢查的實體資源集合：自己 + （若為合併房間）所有子資源
  SELECT array_agg(x) INTO v_ids FROM (
    SELECT p_resource_id AS x
    UNION
    SELECT coalesce(cb.spatial_node_id, cb.asset_id)
      FROM fms.resource_compositions rc
      JOIN fms.bookable_resources cb ON cb.id = rc.child_resource_id
     WHERE rc.parent_resource_id = v_res.id
  ) s;

  IF extract(epoch FROM (p_end_at - p_start_at)) / 60 < v_res.min_duration_minutes THEN
    RETURN QUERY SELECT false, 'DURATION_TOO_SHORT'::text, v_res.id,
      format('最短 %s 分鐘', v_res.min_duration_minutes);
    RETURN;
  END IF;

  IF extract(epoch FROM (p_end_at - p_start_at)) / 60 > v_res.max_duration_minutes THEN
    RETURN QUERY SELECT false, 'DURATION_TOO_LONG'::text, v_res.id,
      format('最長 %s 分鐘', v_res.max_duration_minutes);
    RETURN;
  END IF;

  IF p_start_at > clock_timestamp() + (v_res.advance_booking_days || ' days')::interval THEN
    RETURN QUERY SELECT false, 'OUTSIDE_BOOKING_WINDOW'::text, v_res.id,
      format('僅可預約未來 %s 天內', v_res.advance_booking_days);
    RETURN;
  END IF;

  IF p_start_at < clock_timestamp() + (v_res.min_notice_minutes || ' minutes')::interval THEN
    RETURN QUERY SELECT false, 'TOO_LATE'::text, v_res.id,
      format('需提前 %s 分鐘預約', v_res.min_notice_minutes);
    RETURN;
  END IF;

  -- 封鎖時段
  SELECT b.id, b.reason INTO v_conflict_id, v_detail
  FROM fms.resource_blackouts b
  WHERE (b.bookable_resource_id = v_res.id
         OR (b.bookable_resource_id IS NULL AND b.facility_id = v_res.facility_id))
    AND b.blackout_range && v_range
  ORDER BY b.start_at LIMIT 1;

  IF v_conflict_id IS NOT NULL THEN
    RETURN QUERY SELECT false, 'BLACKOUT'::text, v_conflict_id, v_detail;
    RETURN;
  END IF;

  -- 固定座位：指派期間內不接受他人預約
  v_conflict_id := NULL;
  SELECT da.id INTO v_conflict_id
  FROM fms.desk_assignments da
  WHERE da.spatial_node_id = p_resource_id
    AND da.assignment_type IN ('PERMANENT','TEMPORARY')
    AND da.valid_range @> p_start_at::date
    AND (p_user_id IS NULL OR da.user_id IS DISTINCT FROM p_user_id)
  LIMIT 1;

  IF v_conflict_id IS NOT NULL THEN
    RETURN QUERY SELECT false, 'DESK_ASSIGNED'::text, v_conflict_id, '該座位已為固定指派'::text;
    RETURN;
  END IF;

  -- 既有預約（含合併房間的子資源，以及父資源產生的 shadow 列）
  SELECT count(*) INTO v_count
  FROM fms.reservations r
  WHERE r.resource_id = ANY (v_ids)
    AND r.status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN')
    AND r.time_range && v_range
    AND (p_exclude_reservation_id IS NULL
         OR (r.id <> p_exclude_reservation_id
             AND r.parent_reservation_id IS DISTINCT FROM p_exclude_reservation_id));

  IF v_count >= v_res.capacity THEN
    SELECT r.id,
           format('%s ~ %s%s', r.start_at, r.end_at,
                  CASE WHEN r.is_shadow THEN '（合併房間佔用）' ELSE '' END)
      INTO v_conflict_id, v_detail
    FROM fms.reservations r
    WHERE r.resource_id = ANY (v_ids)
      AND r.status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN')
      AND r.time_range && v_range
      AND (p_exclude_reservation_id IS NULL
           OR (r.id <> p_exclude_reservation_id
               AND r.parent_reservation_id IS DISTINCT FROM p_exclude_reservation_id))
    ORDER BY r.start_at LIMIT 1;

    RETURN QUERY SELECT false, 'RESERVATION_CONFLICT'::text, v_conflict_id, v_detail;
    RETURN;
  END IF;

  -- 他人的短期佔位
  v_conflict_id := NULL;
  SELECT h.id INTO v_conflict_id
  FROM fms.reservation_holds h
  WHERE h.bookable_resource_id = v_res.id
    AND h.status = 'ACTIVE'
    AND h.expires_at > clock_timestamp()
    AND h.hold_range && v_range
    AND (p_user_id IS NULL OR h.user_id IS DISTINCT FROM p_user_id)
  LIMIT 1;

  IF v_conflict_id IS NOT NULL THEN
    RETURN QUERY SELECT false, 'HELD_BY_OTHER'::text, v_conflict_id, '資源暫時被他人佔位'::text;
    RETURN;
  END IF;

  RETURN QUERY SELECT true, NULL::text, v_res.id, '可預約'::text;
END;
$$;

COMMENT ON FUNCTION fms.check_resource_availability IS
  '可預約性判定的單一真相。已納入合併房間（父子雙向）、固定座位、封鎖時段、短期佔位與預約窗規則。';

-- =============================================================================
-- H. RLS 與授權（沿用 007 的模式套用到新表）
-- =============================================================================

DO $$
DECLARE
  r record;
  catalog_tables text[] := ARRAY['amenities'];
  new_tables text[] := ARRAY[
    'resource_compositions','integrations','integration_user_grants',
    'quota_policies','quota_assignments','quota_usage','quota_transactions',
    'visitors','visitor_invitations','visitor_access_grants',
    'announcements','announcement_reads',
    'amenities','resource_amenities','user_favorites',
    'user_mfa_factors','user_trusted_devices','desk_assignments',
    'device_commands','signage_displays'
  ];
BEGIN
  FOR r IN
    SELECT c.relname AS table_name
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_attribute a ON a.attrelid = c.oid
    WHERE n.nspname = 'fms'
      AND c.relkind IN ('r','p')
      AND a.attname = 'tenant_id'
      AND a.attnum > 0 AND NOT a.attisdropped
      AND c.relispartition = false
      AND c.relname = ANY (new_tables)
    ORDER BY c.relname
  LOOP
    EXECUTE format('ALTER TABLE fms.%I ENABLE ROW LEVEL SECURITY', r.table_name);
    EXECUTE format('ALTER TABLE fms.%I FORCE ROW LEVEL SECURITY', r.table_name);

    IF r.table_name = ANY (catalog_tables) THEN
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

    -- 設施級加固（與 007 相同的 RESTRICTIVE 疊加）
    IF EXISTS (
      SELECT 1 FROM information_schema.columns
      WHERE table_schema = 'fms' AND table_name = r.table_name AND column_name = 'facility_id'
    ) THEN
      EXECUTE format($f$
        CREATE POLICY facility_scope ON fms.%I AS RESTRICTIVE FOR ALL
        USING (fms.is_platform_context() OR fms.facility_in_scope(facility_id))
      $f$, r.table_name);
    END IF;
  END LOOP;
END;
$$;

GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA fms TO fms_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA fms TO fms_app;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA fms TO fms_app;
GRANT SELECT ON ALL TABLES IN SCHEMA fms TO fms_readonly;

-- 明細帳與命令佇列不得被應用層改寫歷史
REVOKE UPDATE, DELETE ON fms.quota_transactions FROM fms_app;
REVOKE DELETE ON fms.device_commands FROM fms_app;

-- =============================================================================
-- I. 新增權限與平台種子
-- =============================================================================

INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous) VALUES
('visitor:read',        'visitor',      'read',      'VISITOR',     '讀取訪客與到訪紀錄',        'FACILITY', false),
('visitor:invite',      'visitor',      'invite',    'VISITOR',     '邀請訪客',                  'FACILITY', false),
('visitor:checkin',     'visitor',      'checkin',   'VISITOR',     '訪客報到與離場（櫃台）',    'FACILITY', false),
('visitor:manage',      'visitor',      'manage',    'VISITOR',     '訪客主檔與黑名單管理',      'FACILITY', true),
('announcement:read',   'announcement', 'read',      'CORE',        '讀取公告',                  'FACILITY', false),
('announcement:publish','announcement', 'publish',   'CORE',        '發布公告與廣播',            'FACILITY', true),
('quota:read',          'quota',        'read',      'RESERVATION', '查看配額與使用紀錄',        'FACILITY', false),
('quota:manage',        'quota',        'manage',    'RESERVATION', '設定配額政策與額度覆寫',    'TENANT',   true),
('quota:adjust',        'quota',        'adjust',    'RESERVATION', '人工調整配額餘額',          'FACILITY', true),
('integration:read',    'integration',  'read',      'ADMIN',       '查看整合設定',              'TENANT',   false),
('integration:write',   'integration',  'write',     'ADMIN',       '設定視訊與行事曆整合',      'TENANT',   true),
('amenity:write',       'amenity',      'write',     'RESERVATION', '維護附屬設備字典與配置',    'FACILITY', false),
('desk_assignment:write','desk_assignment','write',  'RESERVATION', '指派固定座位',              'FACILITY', false),
('device:command',      'device',       'command',   'IOT',         '下行控制設備（含門鎖）',    'FACILITY', true),
('signage:write',       'signage',      'write',     'IOT',         '設定電子標牌與門口面板',    'FACILITY', false),
('reservation:view_private','reservation','view_private','RESERVATION','查看私人預約詳情',      'FACILITY', true)
ON CONFLICT (code) DO UPDATE
  SET description = EXCLUDED.description, module = EXCLUDED.module;

-- 角色補授
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, p.code FROM fms.roles r, fms.permissions p
WHERE r.code = 'PLATFORM_ADMIN'
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, p.code FROM fms.roles r, fms.permissions p
WHERE r.code = 'TENANT_ADMIN' AND p.code <> 'user:impersonate'
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code FROM fms.roles r,
  (VALUES ('visitor:read'),('visitor:invite'),('visitor:checkin'),('visitor:manage'),
          ('announcement:read'),('announcement:publish'),
          ('quota:read'),('quota:adjust'),('amenity:write'),
          ('desk_assignment:write'),('device:command'),('signage:write'),
          ('reservation:view_private')) AS c(code)
WHERE r.code IN ('ORG_MANAGER','FACILITY_ADMIN')
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code FROM fms.roles r,
  (VALUES ('visitor:read'),('visitor:invite'),('announcement:read'),('quota:read')) AS c(code)
WHERE r.code IN ('REQUESTER','DISPATCHER','SERVICE_STAFF','TECHNICIAN')
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code FROM fms.roles r,
  (VALUES ('visitor:read'),('visitor:checkin'),('announcement:read')) AS c(code)
WHERE r.code = 'VIEWER'
ON CONFLICT DO NOTHING;

-- 平台附屬設備字典（對應 Offision 的設施篩選項目）
INSERT INTO fms.amenities (tenant_id, code, name, name_en, category, icon, display_order) VALUES
(NULL, 'PROJECTOR',      '投影機',       'Projector',            'AV',           'projector', 10),
(NULL, 'PROJECTOR_SCREEN','投影屏幕',    'Projector Screen',     'AV',           'screen',    20),
(NULL, 'TV_DISPLAY',     '電視／顯示器', 'TV / Display',         'AV',           'tv',        30),
(NULL, 'VIDEO_CONF',     '視像會議設備', 'Video Conferencing',   'AV',           'video',     40),
(NULL, 'AUDIO_CONF',     '話音會議設備', 'Audio Conferencing',   'AV',           'phone',     50),
(NULL, 'MICROPHONE',     '麥克風',       'Microphone',           'AV',           'mic',       60),
(NULL, 'SPEAKER',        '喇叭音響',     'Speakers',             'AV',           'speaker',   70),
(NULL, 'WHITEBOARD',     '白板',         'Whiteboard',           'FURNITURE',    'board',     80),
(NULL, 'WIFI',           'Wi-Fi',        'Wi-Fi',                'CONNECTIVITY', 'wifi',      90),
(NULL, 'WIRED_NETWORK',  '有線網路',     'Wired Network',        'CONNECTIVITY', 'ethernet', 100),
(NULL, 'POWER_OUTLET',   '電源插座',     'Power Outlets',        'CONNECTIVITY', 'power',    110),
(NULL, 'HDMI',           'HDMI 連接',    'HDMI',                 'CONNECTIVITY', 'hdmi',     120),
(NULL, 'WIRELESS_CAST',  '無線投放',     'Wireless Casting',     'CONNECTIVITY', 'cast',     130),
(NULL, 'HEIGHT_DESK',    '升降桌',       'Height-adjustable Desk','FURNITURE',   'desk',     140),
(NULL, 'DUAL_MONITOR',   '雙螢幕',       'Dual Monitors',        'FURNITURE',    'monitor',  150),
(NULL, 'WHEELCHAIR',     '無障礙通行',   'Wheelchair Accessible','ACCESSIBILITY','wheelchair',160),
(NULL, 'HEARING_LOOP',   '聽力輔助',     'Hearing Loop',         'ACCESSIBILITY','ear',      170),
(NULL, 'AIR_CON',        '獨立空調',     'Individual A/C',       'COMFORT',      'ac',       180),
(NULL, 'NATURAL_LIGHT',  '自然採光',     'Natural Light',        'COMFORT',      'sun',      190),
(NULL, 'BLACKOUT_BLIND', '遮光窗簾',     'Blackout Blinds',      'COMFORT',      'blind',    200),
(NULL, 'WATER_DISPENSER','飲水設備',     'Water Dispenser',      'CATERING',     'water',    210),
(NULL, 'COFFEE_MACHINE', '咖啡機',       'Coffee Machine',       'CATERING',     'coffee',   220),
(NULL, 'CATERING_READY', '可供餐',       'Catering Ready',       'CATERING',     'food',     230),
(NULL, 'AED',            'AED 設備',     'AED',                  'SAFETY',       'aed',      240)
ON CONFLICT DO NOTHING;

-- 預約相關的公告與訪客通知範本
INSERT INTO fms.notification_templates (tenant_id, code, channel, locale, subject_template, body_template) VALUES
(NULL, 'VISITOR_INVITED', 'EMAIL', 'zh-TW', '【訪客邀請】{{host_name}} 邀請您於 {{expected_arrival_at}} 到訪',
 '您好 {{visitor_name}}，'||chr(10)||'{{host_name}} 邀請您到訪 {{facility_name}}。'||chr(10)||
 '時間：{{expected_arrival_at}}'||chr(10)||'地點：{{location_name}}'||chr(10)||
 '報到碼：{{invitation_code}}（請於櫃台或自助機出示 QR）'||chr(10)||'事由：{{purpose}}'),
(NULL, 'VISITOR_ARRIVED', 'PUSH', 'zh-TW', '訪客已到達',
 '{{visitor_name}}（{{visitor_company}}）已於 {{checked_in_at}} 完成報到，請前往接待。'),
(NULL, 'QUOTA_WARNING', 'EMAIL', 'zh-TW', '【配額提醒】{{policy_name}} 已使用 {{used_pct}}%',
 '您在 {{period_key}} 期間的 {{policy_name}} 配額已使用 {{used}}/{{allowance}}（{{used_pct}}%）。'),
(NULL, 'QUOTA_EXCEEDED', 'EMAIL', 'zh-TW', '【配額已用盡】{{policy_name}}',
 '您在 {{period_key}} 期間的 {{policy_name}} 配額已用盡（{{allowance}}）。如需調整請聯繫設施管理員。'),
(NULL, 'ANNOUNCEMENT_PUBLISHED', 'IN_APP', 'zh-TW', '{{title}}',
 '{{summary}}'),
(NULL, 'COMBINED_ROOM_SETUP', 'PUSH', 'zh-TW', '合併房間佈置',
 '{{resource_name}} 需於 {{setup_at}} 前完成合併佈置（移除隔板），預約時間 {{start_at}}。')
ON CONFLICT DO NOTHING;

COMMIT;
