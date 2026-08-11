-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 009 (OPTIONAL): demo group client so front-end teams have real data
--   Tenant: DEMO_GROUP — 一個集團、兩間子公司、兩個場館（辦公大樓 + 影城）
-- Run only in dev / staging. Safe to re-run.
-- =============================================================================

BEGIN;

SELECT set_config('app.is_platform', 'on', true);

-- Fixed UUIDs so the front-end can hard-code fixtures.
-- tenant
INSERT INTO fms.tenants (id, code, name, legal_name, industry, plan_tier, feature_flags)
VALUES ('aaaaaaaa-0000-4000-8000-000000000001', 'DEMO_GROUP', 'Bizlution 示範集團',
        'Bizlution Demo Group Co., Ltd.', 'ENTERPRISE', 'ENTERPRISE',
        '{"soft_fm":true,"reservation":true,"iot":true,"bim":true,"chargeback":false}'::jsonb)
ON CONFLICT (id) DO NOTHING;

-- organizations: 集團 → 兩間子公司
INSERT INTO fms.organizations (id, tenant_id, parent_id, code, name, org_type) VALUES
('bbbbbbbb-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001', NULL,
 'GRP', 'Bizlution 集團總部', 'GROUP'),
('bbbbbbbb-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000001', 'PROP', '不動產事業部', 'BUSINESS_UNIT'),
('bbbbbbbb-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000001', 'CINEMA', '影城事業部', 'BUSINESS_UNIT')
ON CONFLICT (id) DO NOTHING;

-- facilities
INSERT INTO fms.facilities (id, tenant_id, org_id, code, name, facility_type, city, timezone,
                            gross_area_sqm, operating_hours) VALUES
('cccccccc-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000002', 'HQ_TPE', '台北總部大樓', 'OFFICE', '台北市', 'Asia/Taipei',
 18500.00, '{"mon":[["08:00","21:00"]],"tue":[["08:00","21:00"]],"wed":[["08:00","21:00"]],"thu":[["08:00","21:00"]],"fri":[["08:00","21:00"]],"sat":[["09:00","17:00"]]}'::jsonb),
('cccccccc-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000003', 'CIN_XY', '信義影城', 'CINEMA', '台北市', 'Asia/Taipei',
 9200.00, '{"mon":[["10:00","24:00"]],"sun":[["10:00","24:00"]]}'::jsonb)
ON CONFLICT (id) DO NOTHING;

-- identity providers: 混合式 — 總部走 Entra ID，影城走本地帳號
INSERT INTO fms.identity_providers (id, tenant_id, code, name, provider_type, issuer, client_id,
                                    jit_provisioning, jit_default_role_code, is_default, sync_enabled)
VALUES
('dddddddd-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'entra-hq', 'Entra ID（集團總部）', 'OIDC',
 'https://login.microsoftonline.com/00000000-1111-2222-3333-444444444444/v2.0',
 'demo-client-id', true, 'REQUESTER', true, true),
('dddddddd-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'local', '本地帳號（外包技師／影城現場）', 'LOCAL', NULL, NULL, false, NULL, false, false)
ON CONFLICT (id) DO NOTHING;

-- `ldap_bind_secret_ref` 一併填上。原本只有 `ldap_bind_dn` ——
-- 那組設定永遠無法認證（有帳號沒密碼），而示範租戶應該是一份**可運作的**
-- 設定，否則對它跑 `POST /identity-providers/{id}/test-connection` 會回報一個
-- 種子自己造成的錯誤，於是那支端點的輸出開始被當成雜訊。
-- 值是密鑰管理服務裡的參照名稱，不是密鑰本身（見 identity_providers.rs 檔頭）。
INSERT INTO fms.identity_providers (id, tenant_id, code, name, provider_type,
                                    ldap_host, ldap_port, ldap_base_dn, ldap_bind_dn,
                                    ldap_bind_secret_ref,
                                    sync_enabled, sync_cron, auto_deprovision)
VALUES
('dddddddd-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'ad-cinema', '地端 AD（影城事業部）', 'LDAP',
 'dc01.cinema.local', 636, 'DC=cinema,DC=local', 'CN=svc_fms,OU=Service,DC=cinema,DC=local',
 'kv/fms/ad-cinema-bind',
 true, '0 */4 * * *', true)
ON CONFLICT (id) DO NOTHING;

-- directory groups + role mapping (AD 群組 → FMS 角色)
INSERT INTO fms.directory_groups (id, tenant_id, identity_provider_id, external_group_id, name, distinguished_name)
VALUES
('eeeeeeee-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'dddddddd-0000-4000-8000-000000000003', 'S-1-5-21-1001', 'FMS-Facility-Admins',
 'CN=FMS-Facility-Admins,OU=Groups,DC=cinema,DC=local'),
('eeeeeeee-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'dddddddd-0000-4000-8000-000000000003', 'S-1-5-21-1002', 'FMS-Technicians',
 'CN=FMS-Technicians,OU=Groups,DC=cinema,DC=local')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.directory_role_mappings (tenant_id, directory_group_id, role_id, scope_type, scope_id)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001', 'eeeeeeee-0000-4000-8000-000000000001',
       r.id, 'FACILITY', 'cccccccc-0000-4000-8000-000000000002'
FROM fms.roles r WHERE r.code = 'FACILITY_ADMIN'
ON CONFLICT DO NOTHING;

INSERT INTO fms.directory_role_mappings (tenant_id, directory_group_id, role_id, scope_type, scope_id)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001', 'eeeeeeee-0000-4000-8000-000000000002',
       r.id, 'FACILITY', 'cccccccc-0000-4000-8000-000000000002'
FROM fms.roles r WHERE r.code = 'TECHNICIAN'
ON CONFLICT DO NOTHING;

-- users
INSERT INTO fms.users (id, tenant_id, primary_org_id, default_facility_id, employee_no,
                       username, email, display_name, user_type, job_title) VALUES
('ffffffff-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001', 'E0001',
 'admin.chen', 'admin.chen@demo.bizlution.com', '陳系統', 'EMPLOYEE', '資訊部經理'),
('ffffffff-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000002', 'cccccccc-0000-4000-8000-000000000001', 'E0002',
 'fm.lin', 'fm.lin@demo.bizlution.com', '林設施', 'EMPLOYEE', '設施管理主任'),
('ffffffff-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000003', 'cccccccc-0000-4000-8000-000000000002', 'E0101',
 'tech.wang', 'tech.wang@demo.bizlution.com', '王技師', 'EMPLOYEE', '機電技師'),
('ffffffff-0000-4000-8000-000000000004', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000002', 'cccccccc-0000-4000-8000-000000000001', 'E0003',
 'user.huang', 'user.huang@demo.bizlution.com', '黃同事', 'EMPLOYEE', '業務專員'),
('ffffffff-0000-4000-8000-000000000005', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000003', 'cccccccc-0000-4000-8000-000000000002', NULL,
 'clean.vendor01', NULL, '潔美清潔（外包）', 'CONTRACTOR', '清潔班長'),
-- 台北總部大樓的機電技師。
--
-- **加這個人是為了修一個種子本身的不一致。** 在他之前，示範租戶的設備
-- 3 個在總部、1 個在影城，但兩個執行者（tech.wang TECHNICIAN、
-- clean.vendor01 SERVICE_STAFF）都在影城 —— 總部只有場域管理員與申請人，
-- **沒有任何場域級的執行者**。
--
-- 那個不一致有實際後果：022 讓 transition_work_order 自行執行
-- required_permission 之後，010 的 T3（工單狀態機）在總部的工單上
-- 找不到任何能 START_WORK 的場域級使用者，只好改用租戶管理員當執行者，
-- 於是「場域級執行者能不能執行工單」這條最常見的路徑沒有被任何東西走過。
--
-- 選「加人」而不是「把 tech.wang 也指派到總部」，理由是模型而不是相容性：
--
-- 我原本以為擴他會弄壞 notification_perm_token_slice 那格
-- （它斷言 PERM token 解析不會解到 tech.wang）。**查過之後那是錯的** ——
-- 那一格排除他的理由是「技師沒有 work_order:approve」，與場域無關；
-- 而全部 TECH_WANG 的用法都只是 assignee_id，沒有任何測試依賴他的場域。
-- 也就是說擴他不會弄壞任何東西。
--
-- 仍然加人的兩個理由：
--   * 示範租戶模擬的是**兩個各有駐點人員的場地**。一個技師橫跨兩地是另一種
--     合理的形狀，但它會抹掉「每個場域有自己的人」這個結構。
--   * tech.wang 是目前唯一「範圍只在單一場域的技師」。今天沒有測試用到那個
--     性質，但它是驗場域收斂時現成的對照組，擴他就沒有了。
('ffffffff-0000-4000-8000-000000000006', 'aaaaaaaa-0000-4000-8000-000000000001',
 'bbbbbbbb-0000-4000-8000-000000000002', 'cccccccc-0000-4000-8000-000000000001', 'E0004',
 'tech.liu', 'tech.liu@demo.bizlution.com', '劉技師', 'EMPLOYEE', '機電技師')
ON CONFLICT (id) DO NOTHING;

-- role grants
INSERT INTO fms.user_role_assignments (tenant_id, user_id, role_id, scope_type, scope_id, source)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001', u.id, r.id, s.scope_type, s.scope_id, 'MANUAL'
FROM (VALUES
  ('ffffffff-0000-4000-8000-000000000001'::uuid, 'TENANT_ADMIN',   'TENANT',   NULL::uuid),
  ('ffffffff-0000-4000-8000-000000000002'::uuid, 'FACILITY_ADMIN', 'FACILITY', 'cccccccc-0000-4000-8000-000000000001'::uuid),
  ('ffffffff-0000-4000-8000-000000000003'::uuid, 'TECHNICIAN',     'FACILITY', 'cccccccc-0000-4000-8000-000000000002'::uuid),
  ('ffffffff-0000-4000-8000-000000000004'::uuid, 'REQUESTER',      'FACILITY', 'cccccccc-0000-4000-8000-000000000001'::uuid),
  ('ffffffff-0000-4000-8000-000000000005'::uuid, 'SERVICE_STAFF',  'FACILITY', 'cccccccc-0000-4000-8000-000000000002'::uuid),
  -- 總部的技師。設備與工單多數在這裡，卻一直沒有場域級的執行者（見上）。
  ('ffffffff-0000-4000-8000-000000000006'::uuid, 'TECHNICIAN',     'FACILITY', 'cccccccc-0000-4000-8000-000000000001'::uuid)
) AS s(user_id, role_code, scope_type, scope_id)
JOIN fms.users u ON u.id = s.user_id
JOIN fms.roles r ON r.code = s.role_code AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- spatial tree: HQ 大樓 B1 / 1F / 4F，含會議室與工位
INSERT INTO fms.spatial_nodes (id, tenant_id, facility_id, parent_id, node_type_code, code, name,
                               floor_level, floor_label, area_sqm, capacity, is_bookable) VALUES
('10000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001', NULL,
 'BUILDING', 'BLDG_A', 'A 棟', NULL, NULL, 18500, 0, false),
('10000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
 '10000000-0000-4000-8000-000000000001', 'FLOOR', 'B1', '地下一樓', -1, 'B1', 3200, 0, false),
('10000000-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
 '10000000-0000-4000-8000-000000000001', 'FLOOR', 'FL04', '四樓', 4, '4F', 1450, 0, false),
('10000000-0000-4000-8000-000000000004', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
 '10000000-0000-4000-8000-000000000002', 'MACHINE_ROOM', 'B1_MR', 'B1 機房', -1, 'B1', 180, 0, false),
('10000000-0000-4000-8000-000000000005', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
 '10000000-0000-4000-8000-000000000003', 'MEETING_ROOM', 'R401', '401 會議室', 4, '4F', 42, 12, true),
('10000000-0000-4000-8000-000000000006', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
 '10000000-0000-4000-8000-000000000003', 'MEETING_ROOM', 'R402', '402 會議室', 4, '4F', 24, 6, true),
('10000000-0000-4000-8000-000000000007', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
 '10000000-0000-4000-8000-000000000003', 'DESK_AREA', 'FL04_HD', '4F 共享工位區', 4, '4F', 320, 24, false),
('10000000-0000-4000-8000-000000000008', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000001',
 '10000000-0000-4000-8000-000000000007', 'DESK', 'HD_401', '工位 HD-401', 4, '4F', 4, 1, true),
-- 影城：1F 兩個影廳
('10000000-0000-4000-8000-000000000011', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000002', NULL,
 'BUILDING', 'CIN_MAIN', '影城主館', NULL, NULL, 9200, 0, false),
('10000000-0000-4000-8000-000000000012', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000002',
 '10000000-0000-4000-8000-000000000011', 'FLOOR', 'FL01', '一樓', 1, '1F', 4100, 0, false),
('10000000-0000-4000-8000-000000000013', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000002',
 '10000000-0000-4000-8000-000000000012', 'AUDITORIUM', 'HALL_1', '1 廳', 1, '1F', 480, 210, true),
('10000000-0000-4000-8000-000000000014', 'aaaaaaaa-0000-4000-8000-000000000001', 'cccccccc-0000-4000-8000-000000000002',
 '10000000-0000-4000-8000-000000000012', 'AUDITORIUM', 'HALL_2', '2 廳', 1, '1F', 320, 140, true)
ON CONFLICT (id) DO NOTHING;

-- assets: UPS（B1 機房）、AHU（4F）、投影機（1 廳），含跨系統依賴
INSERT INTO fms.assets (id, tenant_id, facility_id, spatial_node_id, category_id, asset_code, name,
                        serial_no, criticality, status, install_date, warranty_end_date) VALUES
('20000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000004',
 (SELECT id FROM fms.asset_categories WHERE code = 'UPS' AND tenant_id IS NULL),
 'HQ-UPS-B1-01', 'B1 不斷電系統 #1', 'SN-UPS-88231', 'CRITICAL', 'OPERATIONAL', '2024-03-15', '2027-03-14'),
('20000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000003',
 (SELECT id FROM fms.asset_categories WHERE code = 'AHU' AND tenant_id IS NULL),
 'HQ-AHU-4F-01', '4F 空調箱 #1', 'SN-AHU-40112', 'HIGH', 'OPERATIONAL', '2023-08-01', '2026-07-31'),
('20000000-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000002', '10000000-0000-4000-8000-000000000013',
 (SELECT id FROM fms.asset_categories WHERE code = 'PROJECTOR' AND tenant_id IS NULL),
 'CIN-PRJ-H1-01', '1 廳雷射投影機', 'SN-PRJ-70045', 'CRITICAL', 'OPERATIONAL', '2025-01-20', '2028-01-19'),
('20000000-0000-4000-8000-000000000004', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000005',
 (SELECT id FROM fms.asset_categories WHERE code = 'CONF_HARDWARE' AND tenant_id IS NULL),
 'HQ-VC-R401-01', '401 視訊會議系統', 'SN-VC-11298', 'MEDIUM', 'OPERATIONAL', '2025-06-10', '2028-06-09')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.asset_relations (tenant_id, from_asset_id, to_asset_id, relation_type, impact_level) VALUES
('aaaaaaaa-0000-4000-8000-000000000001', '20000000-0000-4000-8000-000000000002',
 '20000000-0000-4000-8000-000000000001', 'DEPENDS_ON', 'CRITICAL')
ON CONFLICT DO NOTHING;

-- meters on the projector (lamp hours drives meter-based PM)
INSERT INTO fms.asset_meters (id, tenant_id, asset_id, meter_code, name, unit, reading_type, last_value)
VALUES ('30000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
        '20000000-0000-4000-8000-000000000003', 'LAMP_HOURS', '光源使用時數', 'h', 'CUMULATIVE', 4820)
ON CONFLICT (id) DO NOTHING;

-- teams
INSERT INTO fms.teams (id, tenant_id, facility_id, code, name, team_type, lead_user_id) VALUES
('40000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'HQ_MECH', '總部機電班', 'MAINTENANCE',
 'ffffffff-0000-4000-8000-000000000002'),
('40000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000002', 'CIN_CLEAN', '影城清潔班', 'CLEANING',
 'ffffffff-0000-4000-8000-000000000005'),
('40000000-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'HQ_IT', '總部 IT 支援', 'IT_SUPPORT',
 'ffffffff-0000-4000-8000-000000000001')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.team_members (team_id, user_id, tenant_id, role_in_team) VALUES
('40000000-0000-4000-8000-000000000001', 'ffffffff-0000-4000-8000-000000000003',
 'aaaaaaaa-0000-4000-8000-000000000001', 'MEMBER'),
('40000000-0000-4000-8000-000000000002', 'ffffffff-0000-4000-8000-000000000005',
 'aaaaaaaa-0000-4000-8000-000000000001', 'LEAD')
ON CONFLICT DO NOTHING;

-- SLA policies
INSERT INTO fms.sla_policies (id, tenant_id, code, name, applies_to_priority,
                              response_minutes, resolution_minutes, business_hours_only, escalation_rules) VALUES
('50000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'SLA_CRITICAL', '關鍵設備 SLA', 'CRITICAL', 15, 120, false,
 '[{"at_pct":75,"notify":["TEAM_LEAD"]},{"at_pct":100,"notify":["FACILITY_ADMIN"],"escalate_priority":true}]'::jsonb),
('50000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'SLA_STANDARD', '一般服務 SLA', 'MEDIUM', 60, 480, true,
 '[{"at_pct":80,"notify":["TEAM_LEAD"]}]'::jsonb),
('50000000-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'SLA_CLEANING', '清潔服務 SLA', 'HIGH', 15, 60, false,
 '[{"at_pct":100,"notify":["FACILITY_ADMIN"]}]'::jsonb)
ON CONFLICT (id) DO NOTHING;

-- Soft FM service catalogue
INSERT INTO fms.service_items (id, tenant_id, facility_id, category, code, name, description,
                               lead_time_minutes, default_duration_minutes, relative_offset_minutes,
                               is_attachable_to_reservation, chargeable, unit_price, currency, unit_label,
                               default_team_id, sla_policy_id, form_schema) VALUES
('60000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'CATERING', 'TEA_SETUP', '會議茶水佈置',
 '會議開始前完成茶水與點心擺設', 120, 20, -15, true, true, 60.00, 'TWD', '每人',
 '40000000-0000-4000-8000-000000000003', '50000000-0000-4000-8000-000000000002',
 '{"type":"object","required":["headcount"],"properties":{"headcount":{"type":"integer","minimum":1,"maximum":50,"title":"人數"},"beverage":{"type":"string","enum":["COFFEE","TEA","WATER","MIXED"],"title":"飲品"},"snack":{"type":"boolean","title":"附點心"}}}'::jsonb),
('60000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'ROOM_SETUP', 'ROOM_LAYOUT', '會議室桌椅佈置',
 '依指定型式排列桌椅', 240, 30, -30, true, false, NULL, NULL, NULL,
 '40000000-0000-4000-8000-000000000003', '50000000-0000-4000-8000-000000000002',
 '{"type":"object","required":["layout"],"properties":{"layout":{"type":"string","enum":["U_SHAPE","BOARDROOM","THEATER","CLASSROOM","ISLAND"],"title":"排列方式"},"extra_chairs":{"type":"integer","minimum":0,"title":"加椅數"}}}'::jsonb),
('60000000-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'AV_SUPPORT', 'AV_PRECHECK', '視訊設備預先調試',
 '會議前 15 分鐘完成投影與視訊測試', 60, 15, -15, true, false, NULL, NULL, NULL,
 '40000000-0000-4000-8000-000000000003', '50000000-0000-4000-8000-000000000002',
 '{"type":"object","properties":{"platform":{"type":"string","enum":["TEAMS","ZOOM","GOOGLE_MEET","WEBEX"],"title":"會議平台"},"external_guests":{"type":"boolean","title":"有外部來賓"}}}'::jsonb),
('60000000-0000-4000-8000-000000000004', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000002', 'CLEANING', 'SPILL_CLEAN', '影廳緊急清潔',
 '飲料傾倒等突發狀況的立即清潔', 0, 20, 0, false, false, NULL, NULL, NULL,
 '40000000-0000-4000-8000-000000000002', '50000000-0000-4000-8000-000000000003',
 '{"type":"object","required":["severity"],"properties":{"severity":{"type":"string","enum":["MINOR","MODERATE","MAJOR"],"title":"污染程度"},"seat_range":{"type":"string","title":"座位區間"},"blocking_show":{"type":"boolean","title":"影響下一場次"}}}'::jsonb),
('60000000-0000-4000-8000-000000000005', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000002', 'CLEANING', 'DEEP_CLEAN', '影廳深層清潔',
 '每日末場後的深層清潔', 480, 90, 0, false, false, NULL, NULL, NULL,
 '40000000-0000-4000-8000-000000000002', '50000000-0000-4000-8000-000000000002',
 '{"type":"object","properties":{"include_screen":{"type":"boolean","title":"含銀幕清潔"},"include_seats":{"type":"boolean","title":"含座椅清潔"}}}'::jsonb)
ON CONFLICT (id) DO NOTHING;

-- bookable resources
INSERT INTO fms.bookable_resources (id, tenant_id, facility_id, resource_type, spatial_node_id,
                                    display_name, min_duration_minutes, max_duration_minutes,
                                    slot_granularity_minutes, buffer_after_minutes,
                                    advance_booking_days, capacity, requires_check_in,
                                    auto_release_minutes) VALUES
('70000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'SPATIAL_NODE', '10000000-0000-4000-8000-000000000005',
 '401 會議室（12人）', 30, 240, 30, 15, 60, 1, true, 15),
('70000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'SPATIAL_NODE', '10000000-0000-4000-8000-000000000006',
 '402 會議室（6人）', 30, 180, 30, 10, 60, 1, false, NULL),
('70000000-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'SPATIAL_NODE', '10000000-0000-4000-8000-000000000008',
 '共享工位 HD-401', 60, 600, 60, 0, 30, 1, true, 30),
('70000000-0000-4000-8000-000000000004', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000002', 'SPATIAL_NODE', '10000000-0000-4000-8000-000000000013',
 '1 廳（210席）', 60, 300, 15, 30, 180, 1, false, NULL)
ON CONFLICT (id) DO NOTHING;

-- maintenance template + plans (calendar and meter based)
INSERT INTO fms.maintenance_templates (id, tenant_id, code, name, maintenance_type,
                                       applies_to_category_id, checklist, estimated_minutes,
                                       required_skill_codes, requires_shutdown) VALUES
('80000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'AHU_QUARTERLY', '空調箱季保養', 'PREVENTIVE',
 (SELECT id FROM fms.asset_categories WHERE code = 'AHU' AND tenant_id IS NULL),
 '[{"seq":1,"title":"更換初級濾網","input_type":"CHECKBOX","is_required":true},
   {"seq":2,"title":"皮帶張力檢查","input_type":"CHECKBOX","is_required":true},
   {"seq":3,"title":"進風溫度","input_type":"NUMBER","unit":"°C","min_value":10,"max_value":40,"is_required":true},
   {"seq":4,"title":"出風溫度","input_type":"NUMBER","unit":"°C","min_value":8,"max_value":30,"is_required":true},
   {"seq":5,"title":"保養後照片","input_type":"PHOTO","is_required":true}]'::jsonb,
 90, '{HVAC_BASIC}', true),
('80000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'PRJ_LAMP', '投影機光源更換', 'PREVENTIVE',
 (SELECT id FROM fms.asset_categories WHERE code = 'PROJECTOR' AND tenant_id IS NULL),
 '[{"seq":1,"title":"記錄舊光源時數","input_type":"NUMBER","unit":"h","is_required":true},
   {"seq":2,"title":"更換光源模組","input_type":"CHECKBOX","is_required":true},
   {"seq":3,"title":"色彩校正","input_type":"CHECKBOX","is_required":true},
   {"seq":4,"title":"投影測試照片","input_type":"PHOTO","is_required":true}]'::jsonb,
 120, '{AV_CERTIFIED}', true)
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.maintenance_plans (id, tenant_id, facility_id, template_id, code, name,
                                   asset_id, trigger_type, rrule, generate_lead_days, priority,
                                   assigned_team_id, sla_policy_id, next_due_at) VALUES
('90000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', '80000000-0000-4000-8000-000000000001',
 'PM_AHU_4F', '4F 空調箱季保養', '20000000-0000-4000-8000-000000000002',
 'CALENDAR', 'FREQ=MONTHLY;INTERVAL=3;BYMONTHDAY=5', 7, 'MEDIUM',
 '40000000-0000-4000-8000-000000000001', '50000000-0000-4000-8000-000000000002',
 '2026-08-05 09:00:00+08')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.maintenance_plans (id, tenant_id, facility_id, template_id, code, name,
                                   asset_id, trigger_type, meter_code, meter_threshold,
                                   generate_lead_days, priority, assigned_team_id) VALUES
('90000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000002', '80000000-0000-4000-8000-000000000002',
 'PM_PRJ_LAMP_H1', '1 廳投影機光源更換（5000h）', '20000000-0000-4000-8000-000000000003',
 'METER', 'LAMP_HOURS', 5000, 3, 'HIGH', '40000000-0000-4000-8000-000000000001')
ON CONFLICT (id) DO NOTHING;

-- IoT: gateway, device, points, alarm rule that auto-creates a work order
INSERT INTO fms.iot_gateways (id, tenant_id, facility_id, code, name, protocol, status)
VALUES ('a1000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
        'cccccccc-0000-4000-8000-000000000001', 'GW_HQ_01', '總部 BMS 閘道', 'MQTT', 'ONLINE')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.devices (id, tenant_id, facility_id, gateway_id, asset_id, device_code, name,
                         device_type, address, status) VALUES
('a2000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'a1000000-0000-4000-8000-000000000001',
 '20000000-0000-4000-8000-000000000002', 'SNS_AHU_4F_01', '4F 空調箱感測器組', 'ENVIRONMENT',
 'fms/hq/4f/ahu01', 'ONLINE'),
('a2000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'a1000000-0000-4000-8000-000000000001',
 '20000000-0000-4000-8000-000000000001', 'SNS_UPS_B1_01', 'B1 UPS 監測', 'METER',
 'fms/hq/b1/ups01', 'ONLINE')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.telemetry_points (id, tenant_id, device_id, point_code, name, unit, valid_min, valid_max) VALUES
('a3000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'a2000000-0000-4000-8000-000000000001', 'TEMP_SUPPLY', '出風溫度', '°C', -10, 60),
('a3000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'a2000000-0000-4000-8000-000000000001', 'FILTER_DP', '濾網壓差', 'Pa', 0, 1000),
('a3000000-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'a2000000-0000-4000-8000-000000000002', 'BATTERY_SOC', '電池電量', '%', 0, 100)
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.alarm_rules (id, tenant_id, facility_id, code, name, telemetry_point_id, rule_type,
                             condition, severity, debounce_seconds, auto_create_work_order,
                             wo_work_order_type, wo_priority, wo_team_id, wo_sla_policy_id,
                             dedupe_window_minutes, notify_role_codes) VALUES
('a4000000-0000-4000-8000-000000000001', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'AHU_FILTER_DP_HIGH', '空調濾網壓差過高',
 'a3000000-0000-4000-8000-000000000002', 'THRESHOLD',
 '{"op":">","value":450,"for_seconds":600}'::jsonb, 'MAJOR', 300, true,
 'CORRECTIVE', 'HIGH', '40000000-0000-4000-8000-000000000001',
 '50000000-0000-4000-8000-000000000002', 120, '{FACILITY_ADMIN,MAINTENANCE_SUPERVISOR}'),
('a4000000-0000-4000-8000-000000000002', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'UPS_SOC_LOW', 'UPS 電量過低',
 'a3000000-0000-4000-8000-000000000003', 'THRESHOLD',
 '{"op":"<","value":40}'::jsonb, 'CRITICAL', 60, true,
 'CORRECTIVE', 'URGENT', '40000000-0000-4000-8000-000000000001',
 '50000000-0000-4000-8000-000000000001', 60, '{FACILITY_ADMIN}'),
('a4000000-0000-4000-8000-000000000003', 'aaaaaaaa-0000-4000-8000-000000000001',
 'cccccccc-0000-4000-8000-000000000001', 'DEVICE_OFFLINE', '感測器離線',
 NULL, 'DEVICE_OFFLINE',
 '{"offline_seconds":900}'::jsonb, 'WARNING', 0, false,
 NULL, 'MEDIUM', NULL, NULL, 240, '{FACILITY_ADMIN}')
ON CONFLICT (id) DO NOTHING;

COMMIT;
