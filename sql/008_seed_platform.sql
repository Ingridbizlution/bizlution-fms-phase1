-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 008: Platform seed data (idempotent)
--   permissions · system roles · work order statuses & state machine ·
--   spatial node types · asset taxonomy · notification templates
-- =============================================================================
-- FORCE ROW LEVEL SECURITY is on, so the seed runs in platform context.
-- =============================================================================

BEGIN;

SELECT set_config('app.is_platform', 'on', true);

-- -----------------------------------------------------------------------------
-- 1. Permission catalogue
-- -----------------------------------------------------------------------------

INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous) VALUES
-- CORE / admin
('tenant:read',            'tenant',        'read',       'CORE',        '讀取租戶設定',              'TENANT', false),
('tenant:update',          'tenant',        'update',     'CORE',        '修改租戶設定與功能開關',    'TENANT', true),
('organization:read',      'organization',  'read',       'CORE',        '讀取組織架構',              'TENANT', false),
('organization:write',     'organization',  'write',      'CORE',        '維護組織架構',              'TENANT', true),
('facility:read',          'facility',      'read',       'CORE',        '讀取設施資料',              'FACILITY', false),
('facility:write',         'facility',      'write',      'CORE',        '維護設施資料',              'FACILITY', false),
('user:read',              'user',          'read',       'ADMIN',       '讀取使用者',                'TENANT', false),
('user:write',             'user',          'write',      'ADMIN',       '建立與修改使用者',          'TENANT', true),
('user:impersonate',       'user',          'impersonate','ADMIN',       '模擬使用者身分（支援用）',  'TENANT', true),
('role:read',              'role',          'read',       'ADMIN',       '讀取角色與權限',            'TENANT', false),
('role:assign',            'role',          'assign',     'ADMIN',       '指派角色',                  'ORG', true),
('role:write',             'role',          'write',      'ADMIN',       '維護角色定義',              'TENANT', true),
('identity_provider:read', 'identity_provider','read',    'ADMIN',       '讀取身分來源設定',          'TENANT', false),
('identity_provider:write','identity_provider','write',   'ADMIN',       '設定 AD / SSO 身分來源',    'TENANT', true),
('directory:sync',         'directory',     'sync',       'ADMIN',       '觸發目錄同步',              'TENANT', false),
('audit:read',             'audit',         'read',       'ADMIN',       '讀取稽核日誌',              'TENANT', false),
('audit:export',           'audit',         'export',     'ADMIN',       '匯出稽核日誌',              'TENANT', true),
-- SPATIAL
('spatial_node:read',      'spatial_node',  'read',       'CORE',        '讀取空間節點',              'FACILITY', false),
('spatial_node:write',     'spatial_node',  'write',      'CORE',        '維護空間節點',              'FACILITY', false),
('bim_model:read',         'bim_model',     'read',       'CORE',        '讀取 BIM 模型',             'FACILITY', false),
('bim_model:write',        'bim_model',     'write',      'CORE',        '上傳與對應 BIM 模型',       'FACILITY', false),
-- ASSET
('asset:read',             'asset',         'read',       'ASSET',       '讀取設備資產',              'FACILITY', false),
('asset:write',            'asset',         'write',      'ASSET',       '建立與修改設備資產',        'FACILITY', false),
('asset:delete',           'asset',         'delete',     'ASSET',       '刪除／報廢設備資產',        'FACILITY', true),
('asset_model:read',       'asset_model',   'read',       'ASSET',       '讀取設備型錄',              'TENANT', false),
('asset_model:write',      'asset_model',   'write',      'ASSET',       '維護設備型錄',              'TENANT', false),
('meter:read',             'meter',         'read',       'ASSET',       '讀取計量值',                'FACILITY', false),
('meter:write',            'meter',         'write',      'ASSET',       '登錄計量讀數',              'FACILITY', false),
-- MAINTENANCE
('maintenance_plan:read',  'maintenance_plan','read',     'MAINTENANCE', '讀取預防性維護計畫',        'FACILITY', false),
('maintenance_plan:write', 'maintenance_plan','write',    'MAINTENANCE', '維護預防性維護計畫',        'FACILITY', false),
('maintenance_template:write','maintenance_template','write','MAINTENANCE','維護保養範本',            'TENANT', false),
-- WORK ORDER
('work_order:read',        'work_order',    'read',       'SERVICE',     '讀取工單',                  'FACILITY', false),
('work_order:read_own',    'work_order',    'read_own',   'SERVICE',     '讀取自己相關的工單',        'FACILITY', false),
('work_order:create',      'work_order',    'create',     'SERVICE',     '建立工單／服務請求',        'FACILITY', false),
('work_order:update',      'work_order',    'update',     'SERVICE',     '修改工單內容',              'FACILITY', false),
('work_order:assign',      'work_order',    'assign',     'SERVICE',     '派工',                      'FACILITY', false),
('work_order:execute',     'work_order',    'execute',    'SERVICE',     '執行工單（開始／完成）',    'FACILITY', false),
('work_order:approve',     'work_order',    'approve',    'SERVICE',     '審核工單',                  'FACILITY', false),
('work_order:reject',      'work_order',    'reject',     'SERVICE',     '駁回工單',                  'FACILITY', false),
('work_order:cancel',      'work_order',    'cancel',     'SERVICE',     '取消工單',                  'FACILITY', false),
('work_order:close',       'work_order',    'close',      'SERVICE',     '結案工單',                  'FACILITY', false),
('work_order:reopen',      'work_order',    'reopen',     'SERVICE',     '重啟工單',                  'FACILITY', false),
('service_item:read',      'service_item',  'read',       'SERVICE',     '讀取服務目錄',              'FACILITY', false),
('service_item:write',     'service_item',  'write',      'SERVICE',     '維護服務目錄',              'FACILITY', false),
('team:read',              'team',          'read',       'SERVICE',     '讀取團隊與技師',            'FACILITY', false),
('team:write',             'team',          'write',      'SERVICE',     '維護團隊與班表',            'FACILITY', false),
('part:read',              'part',          'read',       'SERVICE',     '讀取備品庫存',              'FACILITY', false),
('part:write',             'part',          'write',      'SERVICE',     '維護備品庫存',              'FACILITY', false),
-- RESERVATION
('reservation:read',       'reservation',   'read',       'RESERVATION', '讀取預約',                  'FACILITY', false),
('reservation:read_own',   'reservation',   'read_own',   'RESERVATION', '讀取自己的預約',            'FACILITY', false),
('reservation:create',     'reservation',   'create',     'RESERVATION', '建立預約',                  'FACILITY', false),
('reservation:update',     'reservation',   'update',     'RESERVATION', '修改預約',                  'FACILITY', false),
('reservation:cancel_any', 'reservation',   'cancel_any', 'RESERVATION', '取消他人預約',              'FACILITY', false),
('reservation:approve',    'reservation',   'approve',    'RESERVATION', '審核預約',                  'FACILITY', false),
('reservation:override',   'reservation',   'override',   'RESERVATION', '強制覆蓋預約衝突',          'FACILITY', true),
('bookable_resource:write','bookable_resource','write',   'RESERVATION', '設定可預約資源規則',        'FACILITY', false),
('blackout:write',         'blackout',      'write',      'RESERVATION', '設定封鎖時段',              'FACILITY', false),
-- IOT
('device:read',            'device',        'read',       'IOT',         '讀取設備感測器',            'FACILITY', false),
('device:write',           'device',        'write',      'IOT',         '維護感測器與通訊點',        'FACILITY', false),
('telemetry:read',         'telemetry',     'read',       'IOT',         '讀取遙測資料',              'FACILITY', false),
('telemetry:ingest',       'telemetry',     'ingest',     'IOT',         '寫入遙測資料（機器帳號）',  'FACILITY', false),
('alarm:read',             'alarm',         'read',       'IOT',         '讀取告警',                  'FACILITY', false),
('alarm:acknowledge',      'alarm',         'acknowledge','IOT',         '確認告警',                  'FACILITY', false),
('alarm_rule:write',       'alarm_rule',    'write',      'IOT',         '維護告警規則',              'FACILITY', false),
-- REPORTING
('report:read',            'report',        'read',       'CORE',        '檢視報表',                  'FACILITY', false),
('report:export',          'report',        'export',     'CORE',        '匯出報表',                  'FACILITY', false)
ON CONFLICT (code) DO UPDATE
  SET description = EXCLUDED.description,
      module = EXCLUDED.module,
      min_scope_level = EXCLUDED.min_scope_level;

-- -----------------------------------------------------------------------------
-- 2. System roles (tenant_id IS NULL → inherited by every tenant)
-- -----------------------------------------------------------------------------

INSERT INTO fms.roles (id, tenant_id, code, name, description, is_system, scope_level) VALUES
('11111111-0000-4000-8000-000000000001', NULL, 'PLATFORM_ADMIN',  '平台管理員',     'Bizlution 內部維運', true, 'TENANT'),
('11111111-0000-4000-8000-000000000002', NULL, 'TENANT_ADMIN',    '租戶管理員',     '集團層級系統管理者', true, 'TENANT'),
('11111111-0000-4000-8000-000000000003', NULL, 'ORG_MANAGER',     '組織主管',       '事業部/區域主管',    true, 'ORG'),
('11111111-0000-4000-8000-000000000004', NULL, 'FACILITY_ADMIN',  '設施管理員',     '單一場館負責人',     true, 'FACILITY'),
('11111111-0000-4000-8000-000000000005', NULL, 'MAINTENANCE_SUPERVISOR', '維護主管', '派工與PM規劃',      true, 'FACILITY'),
('11111111-0000-4000-8000-000000000006', NULL, 'TECHNICIAN',      '技師',           '執行維修與保養工單', true, 'FACILITY'),
('11111111-0000-4000-8000-000000000007', NULL, 'SERVICE_STAFF',   '服務人員',       '執行軟性服務工單',   true, 'FACILITY'),
('11111111-0000-4000-8000-000000000008', NULL, 'DISPATCHER',      '派工員',         '接單與分派',         true, 'FACILITY'),
('11111111-0000-4000-8000-000000000009', NULL, 'REQUESTER',       '一般使用者',     '報修與預約',         true, 'FACILITY'),
('11111111-0000-4000-8000-00000000000a', NULL, 'VIEWER',          '唯讀觀察者',     '報表與儀表板檢視',   true, 'FACILITY'),
('11111111-0000-4000-8000-00000000000b', NULL, 'IOT_INGEST',      'IoT 資料寫入',   '機器帳號專用',       true, 'TENANT')
ON CONFLICT (id) DO NOTHING;

-- Role → permission matrix
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
  (VALUES ('organization:read'),('facility:read'),('facility:write'),('user:read'),
          ('role:assign'),('spatial_node:read'),('spatial_node:write'),('bim_model:read'),
          ('asset:read'),('asset:write'),('asset_model:read'),('meter:read'),
          ('maintenance_plan:read'),('maintenance_plan:write'),
          ('work_order:read'),('work_order:create'),('work_order:update'),('work_order:assign'),
          ('work_order:approve'),('work_order:reject'),('work_order:cancel'),('work_order:close'),
          ('service_item:read'),('service_item:write'),('team:read'),('team:write'),
          ('reservation:read'),('reservation:create'),('reservation:update'),
          ('reservation:cancel_any'),('reservation:approve'),
          ('bookable_resource:write'),('blackout:write'),
          ('device:read'),('telemetry:read'),('alarm:read'),('alarm:acknowledge'),
          ('report:read'),('report:export')) AS c(code)
WHERE r.code IN ('ORG_MANAGER','FACILITY_ADMIN')
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code FROM fms.roles r,
  (VALUES ('facility:read'),('spatial_node:read'),('asset:read'),('asset:write'),
          ('asset_model:read'),('meter:read'),('meter:write'),
          ('maintenance_plan:read'),('maintenance_plan:write'),('maintenance_template:write'),
          ('work_order:read'),('work_order:create'),('work_order:update'),('work_order:assign'),
          ('work_order:execute'),('work_order:close'),('work_order:cancel'),('work_order:reopen'),
          ('team:read'),('team:write'),('part:read'),('part:write'),
          ('device:read'),('telemetry:read'),('alarm:read'),('alarm:acknowledge'),
          ('alarm_rule:write'),('report:read')) AS c(code)
WHERE r.code = 'MAINTENANCE_SUPERVISOR'
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code FROM fms.roles r,
  (VALUES ('facility:read'),('spatial_node:read'),('asset:read'),('meter:read'),('meter:write'),
          ('work_order:read_own'),('work_order:execute'),('work_order:update'),
          ('maintenance_plan:read'),('part:read'),('device:read'),('telemetry:read'),
          ('alarm:read'),('alarm:acknowledge')) AS c(code)
WHERE r.code IN ('TECHNICIAN','SERVICE_STAFF')
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code FROM fms.roles r,
  (VALUES ('facility:read'),('spatial_node:read'),('asset:read'),
          ('work_order:read'),('work_order:create'),('work_order:assign'),('work_order:update'),
          ('team:read'),('alarm:read'),('alarm:acknowledge'),('report:read')) AS c(code)
WHERE r.code = 'DISPATCHER'
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code FROM fms.roles r,
  (VALUES ('facility:read'),('spatial_node:read'),('asset:read'),('service_item:read'),
          ('work_order:read_own'),('work_order:create'),
          ('reservation:read_own'),('reservation:create'),('reservation:update')) AS c(code)
WHERE r.code = 'REQUESTER'
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, p.code FROM fms.roles r JOIN fms.permissions p
  ON p.action IN ('read','read_own')
WHERE r.code = 'VIEWER'
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, c.code FROM fms.roles r,
  (VALUES ('telemetry:ingest'),('telemetry:read'),('device:read'),('alarm:read')) AS c(code)
WHERE r.code = 'IOT_INGEST'
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 3. Work order statuses
-- -----------------------------------------------------------------------------

INSERT INTO fms.work_order_statuses (code, name_zh, name_en, category, is_terminal, display_order) VALUES
('DRAFT',            '草稿',       'Draft',            'OPEN',        false, 10),
('SUBMITTED',        '已送出',     'Submitted',        'OPEN',        false, 20),
('PENDING_APPROVAL', '待審核',     'Pending Approval', 'WAITING',     false, 30),
('APPROVED',         '已核准',     'Approved',         'OPEN',        false, 40),
('ASSIGNED',         '已派工',     'Assigned',         'OPEN',        false, 50),
('SCHEDULED',        '已排程',     'Scheduled',        'OPEN',        false, 55),
('IN_PROGRESS',      '執行中',     'In Progress',      'IN_PROGRESS', false, 60),
('ON_HOLD',          '暫停',       'On Hold',          'WAITING',     false, 70),
('WAITING_PARTS',    '待料',       'Waiting for Parts','WAITING',     false, 75),
('WAITING_VENDOR',   '待廠商',     'Waiting on Vendor','WAITING',     false, 78),
('COMPLETED',        '已完成',     'Completed',        'IN_PROGRESS', false, 80),
('VERIFIED',         '已驗收',     'Verified',         'IN_PROGRESS', false, 85),
('CLOSED',           '已結案',     'Closed',           'TERMINAL',    true,  90),
('CANCELLED',        '已取消',     'Cancelled',        'TERMINAL',    true,  95),
('REJECTED',         '已駁回',     'Rejected',         'TERMINAL',    true,  96),
('SLA_BREACHED',     'SLA 逾期',   'SLA Breached',     'IN_PROGRESS', false, 99)
ON CONFLICT (code) DO UPDATE SET name_zh = EXCLUDED.name_zh, name_en = EXCLUDED.name_en;

-- -----------------------------------------------------------------------------
-- 4. Platform default state machine (tenant_id NULL, work_order_type NULL)
-- -----------------------------------------------------------------------------

INSERT INTO fms.work_order_transitions_allowed
  (tenant_id, work_order_type, from_status, action, to_status, required_permission, required_fields, side_effects) VALUES
(NULL, NULL, 'DRAFT',            'SUBMIT',        'SUBMITTED',        'work_order:create',  '{title}',        '{"emit":"work_order.submitted","notify":["DISPATCHER"]}'),
(NULL, NULL, 'DRAFT',            'CANCEL',        'CANCELLED',        'work_order:cancel',  '{}',             '{"emit":"work_order.cancelled"}'),
(NULL, NULL, 'SUBMITTED',        'REQUEST_APPROVAL','PENDING_APPROVAL','work_order:create', '{}',             '{"emit":"work_order.approval_requested","notify":["APPROVER"]}'),
(NULL, NULL, 'SUBMITTED',        'ASSIGN',        'ASSIGNED',         'work_order:assign',  '{assignee_id}',  '{"emit":"work_order.assigned","notify":["ASSIGNEE"],"set_responded":true,"compute_sla":true}'),
(NULL, NULL, 'SUBMITTED',        'AUTO_ASSIGN',   'ASSIGNED',         NULL,                 '{}',             '{"emit":"work_order.assigned","actor":"SYSTEM","set_responded":true,"compute_sla":true}'),
(NULL, NULL, 'SUBMITTED',        'REJECT',        'REJECTED',         'work_order:reject',  '{reason}',       '{"emit":"work_order.rejected","notify":["REQUESTER"]}'),
(NULL, NULL, 'SUBMITTED',        'CANCEL',        'CANCELLED',        'work_order:cancel',  '{reason}',       '{"emit":"work_order.cancelled"}'),
(NULL, NULL, 'PENDING_APPROVAL', 'APPROVE',       'APPROVED',         'work_order:approve', '{}',             '{"emit":"work_order.approved"}'),
(NULL, NULL, 'PENDING_APPROVAL', 'REJECT',        'REJECTED',         'work_order:reject',  '{reason}',       '{"emit":"work_order.rejected","notify":["REQUESTER"]}'),
(NULL, NULL, 'APPROVED',         'ASSIGN',        'ASSIGNED',         'work_order:assign',  '{assignee_id}',  '{"emit":"work_order.assigned","notify":["ASSIGNEE"],"set_responded":true,"compute_sla":true}'),
(NULL, NULL, 'APPROVED',         'CANCEL',        'CANCELLED',        'work_order:cancel',  '{reason}',       '{"emit":"work_order.cancelled"}'),
(NULL, NULL, 'ASSIGNED',         'SCHEDULE',      'SCHEDULED',        'work_order:update',  '{scheduled_start_at}','{"emit":"work_order.scheduled","notify":["ASSIGNEE"]}'),
(NULL, NULL, 'ASSIGNED',         'REASSIGN',      'ASSIGNED',         'work_order:assign',  '{assignee_id}',  '{"emit":"work_order.reassigned","notify":["ASSIGNEE"]}'),
(NULL, NULL, 'ASSIGNED',         'START_WORK',    'IN_PROGRESS',      'work_order:execute', '{}',             '{"emit":"work_order.started","set_actual_start":true}'),
(NULL, NULL, 'ASSIGNED',         'CANCEL',        'CANCELLED',        'work_order:cancel',  '{reason}',       '{"emit":"work_order.cancelled","release_assignee":true}'),
(NULL, NULL, 'SCHEDULED',        'START_WORK',    'IN_PROGRESS',      'work_order:execute', '{}',             '{"emit":"work_order.started","set_actual_start":true}'),
(NULL, NULL, 'SCHEDULED',        'REASSIGN',      'ASSIGNED',         'work_order:assign',  '{assignee_id}',  '{"emit":"work_order.reassigned"}'),
(NULL, NULL, 'SCHEDULED',        'CANCEL',        'CANCELLED',        'work_order:cancel',  '{reason}',       '{"emit":"work_order.cancelled","release_assignee":true}'),
(NULL, NULL, 'IN_PROGRESS',      'HOLD',          'ON_HOLD',          'work_order:execute', '{reason}',       '{"emit":"work_order.held"}'),
(NULL, NULL, 'IN_PROGRESS',      'WAIT_PARTS',    'WAITING_PARTS',    'work_order:execute', '{reason}',       '{"emit":"work_order.waiting_parts","notify":["MAINTENANCE_SUPERVISOR"]}'),
(NULL, NULL, 'IN_PROGRESS',      'WAIT_VENDOR',   'WAITING_VENDOR',   'work_order:execute', '{reason}',       '{"emit":"work_order.waiting_vendor"}'),
(NULL, NULL, 'IN_PROGRESS',      'COMPLETE',      'COMPLETED',        'work_order:execute', '{resolution_notes}','{"emit":"work_order.completed","set_actual_end":true,"notify":["REQUESTER"],"request_satisfaction":true,"release_reservation_step":true}'),
(NULL, NULL, 'IN_PROGRESS',      'CANCEL',        'CANCELLED',        'work_order:cancel',  '{reason}',       '{"emit":"work_order.cancelled"}'),
(NULL, NULL, 'ON_HOLD',          'RESUME',        'IN_PROGRESS',      'work_order:execute', '{}',             '{"emit":"work_order.resumed"}'),
(NULL, NULL, 'ON_HOLD',          'CANCEL',        'CANCELLED',        'work_order:cancel',  '{reason}',       '{"emit":"work_order.cancelled"}'),
(NULL, NULL, 'WAITING_PARTS',    'RESUME',        'IN_PROGRESS',      'work_order:execute', '{}',             '{"emit":"work_order.resumed"}'),
(NULL, NULL, 'WAITING_VENDOR',   'RESUME',        'IN_PROGRESS',      'work_order:execute', '{}',             '{"emit":"work_order.resumed"}'),
(NULL, NULL, 'COMPLETED',        'VERIFY',        'VERIFIED',         'work_order:approve', '{}',             '{"emit":"work_order.verified"}'),
(NULL, NULL, 'COMPLETED',        'REOPEN',        'IN_PROGRESS',      'work_order:reopen',  '{reason}',       '{"emit":"work_order.reopened","increment_reopen":true}'),
(NULL, NULL, 'COMPLETED',        'CLOSE',         'CLOSED',           'work_order:close',   '{}',             '{"emit":"work_order.closed","update_asset_status":true}'),
(NULL, NULL, 'VERIFIED',         'CLOSE',         'CLOSED',           'work_order:close',   '{}',             '{"emit":"work_order.closed","update_asset_status":true}'),
(NULL, NULL, 'VERIFIED',         'REOPEN',        'IN_PROGRESS',      'work_order:reopen',  '{reason}',       '{"emit":"work_order.reopened","increment_reopen":true}'),
(NULL, NULL, 'CLOSED',           'REOPEN',        'IN_PROGRESS',      'work_order:reopen',  '{reason}',       '{"emit":"work_order.reopened","increment_reopen":true}'),
-- SLA escalation is a system-driven transition, reversible once work resumes.
(NULL, NULL, 'ASSIGNED',         'BREACH_SLA',    'SLA_BREACHED',     NULL,                 '{}',             '{"emit":"work_order.sla_breached","actor":"SYSTEM","notify":["FACILITY_ADMIN","MAINTENANCE_SUPERVISOR"]}'),
(NULL, NULL, 'IN_PROGRESS',      'BREACH_SLA',    'SLA_BREACHED',     NULL,                 '{}',             '{"emit":"work_order.sla_breached","actor":"SYSTEM","notify":["FACILITY_ADMIN","MAINTENANCE_SUPERVISOR"]}'),
(NULL, NULL, 'SLA_BREACHED',     'RESUME',        'IN_PROGRESS',      'work_order:execute', '{}',             '{"emit":"work_order.resumed"}'),
(NULL, NULL, 'SLA_BREACHED',     'COMPLETE',      'COMPLETED',        'work_order:execute', '{resolution_notes}','{"emit":"work_order.completed","set_actual_end":true}'),
(NULL, NULL, 'SLA_BREACHED',     'CANCEL',        'CANCELLED',        'work_order:cancel',  '{reason}',       '{"emit":"work_order.cancelled"}')
ON CONFLICT DO NOTHING;

-- SERVICE-type work orders skip DRAFT: a Soft FM request is submitted directly.
INSERT INTO fms.work_order_transitions_allowed
  (tenant_id, work_order_type, from_status, action, to_status, required_permission, required_fields, side_effects) VALUES
(NULL, 'SERVICE', 'SUBMITTED', 'ACCEPT',  'ASSIGNED',  'work_order:assign',  '{assignee_id}', '{"emit":"service_request.accepted","notify":["ASSIGNEE"],"set_responded":true,"compute_sla":true}'),
(NULL, 'SERVICE', 'COMPLETED', 'CLOSE',   'CLOSED',    'work_order:close',   '{}',            '{"emit":"service_request.closed","request_satisfaction":true}')
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 5. Spatial node types
-- -----------------------------------------------------------------------------

INSERT INTO fms.spatial_node_types (tenant_id, code, name, level_hint, is_bookable, is_leaf_default, allowed_child_codes) VALUES
(NULL, 'SITE',       '基地',     0, false, false, '{BUILDING,ZONE,PARKING}'),
(NULL, 'BUILDING',   '棟',       1, false, false, '{FLOOR,ZONE}'),
(NULL, 'FLOOR',      '樓層',     2, false, false, '{ZONE,ROOM,AUDITORIUM,CLASSROOM,LAB,DESK_AREA,CORRIDOR,SHAFT}'),
(NULL, 'ZONE',       '區域',     3, false, false, '{ROOM,DESK_AREA,AUDITORIUM}'),
(NULL, 'ROOM',       '房間',     4, true,  true,  '{DESK,SEAT}'),
(NULL, 'MEETING_ROOM','會議室',  4, true,  true,  '{SEAT}'),
(NULL, 'AUDITORIUM', '影廳',     4, true,  false, '{SEAT}'),
(NULL, 'CLASSROOM',  '教室',     4, true,  false, '{SEAT}'),
(NULL, 'LAB',        '實驗室',   4, true,  false, '{BENCH,SEAT}'),
(NULL, 'DESK_AREA',  '工位區',   4, false, false, '{DESK}'),
(NULL, 'DESK',       '工位',     5, true,  true,  '{}'),
(NULL, 'SEAT',       '座位',     5, true,  true,  '{}'),
(NULL, 'BENCH',      '實驗檯',   5, true,  true,  '{}'),
(NULL, 'CORRIDOR',   '走廊',     4, false, true,  '{}'),
(NULL, 'SHAFT',      '管道間',   4, false, true,  '{}'),
(NULL, 'PARKING',    '停車場',   2, false, false, '{PARKING_SPACE}'),
(NULL, 'PARKING_SPACE','車位',   3, true,  true,  '{}'),
(NULL, 'MACHINE_ROOM','機房',    4, false, true,  '{}')
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 6. Platform asset taxonomy (two levels; tenants extend it)
-- -----------------------------------------------------------------------------

INSERT INTO fms.asset_categories (id, tenant_id, parent_id, code, name, category_path, domain, default_criticality) VALUES
('22222222-0000-4000-8000-000000000001', NULL, NULL, 'HVAC',        '空調系統',   'HVAC',        'HVAC',          'HIGH'),
('22222222-0000-4000-8000-000000000002', NULL, NULL, 'ELECTRICAL',  '電力系統',   'ELECTRICAL',  'ELECTRICAL',    'CRITICAL'),
('22222222-0000-4000-8000-000000000003', NULL, NULL, 'FIRE_SAFETY', '消防系統',   'FIRE_SAFETY', 'FIRE_SAFETY',   'CRITICAL'),
('22222222-0000-4000-8000-000000000004', NULL, NULL, 'ELEVATOR',    '昇降設備',   'ELEVATOR',    'ELEVATOR',      'CRITICAL'),
('22222222-0000-4000-8000-000000000005', NULL, NULL, 'PLUMBING',    '給排水',     'PLUMBING',    'PLUMBING',      'MEDIUM'),
('22222222-0000-4000-8000-000000000006', NULL, NULL, 'AV',          '影音放映',   'AV',          'AV_PROJECTION', 'HIGH'),
('22222222-0000-4000-8000-000000000007', NULL, NULL, 'IT_NETWORK',  '網路資訊',   'IT_NETWORK',  'IT_NETWORK',    'HIGH'),
('22222222-0000-4000-8000-000000000008', NULL, NULL, 'SECURITY',    '安全門禁',   'SECURITY',    'SECURITY',      'HIGH'),
('22222222-0000-4000-8000-000000000009', NULL, NULL, 'LAB_EQUIP',   '實驗設備',   'LAB_EQUIP',   'LAB',           'HIGH'),
('22222222-0000-4000-8000-00000000000a', NULL, NULL, 'PRODUCTION',  '生產設備',   'PRODUCTION',  'PRODUCTION',    'CRITICAL'),
('22222222-0000-4000-8000-00000000000b', NULL, NULL, 'ENVELOPE',    '建築外殼',   'ENVELOPE',    'ENVELOPE',      'MEDIUM')
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.asset_categories (tenant_id, parent_id, code, name, category_path, domain, default_criticality) VALUES
(NULL, '22222222-0000-4000-8000-000000000001', 'AHU',        '空調箱',     'HVAC.AHU',        'HVAC', 'HIGH'),
(NULL, '22222222-0000-4000-8000-000000000001', 'CHILLER',    '冷水主機',   'HVAC.CHILLER',    'HVAC', 'CRITICAL'),
(NULL, '22222222-0000-4000-8000-000000000001', 'FCU',        '風機盤管',   'HVAC.FCU',        'HVAC', 'MEDIUM'),
(NULL, '22222222-0000-4000-8000-000000000001', 'EXHAUST_FAN','排風機',     'HVAC.EXHAUST_FAN','HVAC', 'MEDIUM'),
(NULL, '22222222-0000-4000-8000-000000000002', 'UPS',        '不斷電系統', 'ELECTRICAL.UPS',  'ELECTRICAL', 'CRITICAL'),
(NULL, '22222222-0000-4000-8000-000000000002', 'GENERATOR',  '發電機',     'ELECTRICAL.GENERATOR','ELECTRICAL','CRITICAL'),
(NULL, '22222222-0000-4000-8000-000000000002', 'SWITCHGEAR', '配電盤',     'ELECTRICAL.SWITCHGEAR','ELECTRICAL','CRITICAL'),
(NULL, '22222222-0000-4000-8000-000000000003', 'FIRE_ALARM', '火警受信總機','FIRE_SAFETY.FIRE_ALARM','FIRE_SAFETY','CRITICAL'),
(NULL, '22222222-0000-4000-8000-000000000003', 'SPRINKLER',  '撒水系統',   'FIRE_SAFETY.SPRINKLER','FIRE_SAFETY','CRITICAL'),
(NULL, '22222222-0000-4000-8000-000000000006', 'PROJECTOR',  '投影機',     'AV.PROJECTOR',    'AV_PROJECTION','HIGH'),
(NULL, '22222222-0000-4000-8000-000000000006', 'SOUND_SYSTEM','音響系統',  'AV.SOUND_SYSTEM', 'AV_PROJECTION','HIGH'),
(NULL, '22222222-0000-4000-8000-000000000006', 'SCREEN',     '銀幕',       'AV.SCREEN',       'AV_PROJECTION','MEDIUM'),
(NULL, '22222222-0000-4000-8000-000000000006', 'CONF_HARDWARE','視訊會議設備','AV.CONF_HARDWARE','AV_PROJECTION','HIGH'),
(NULL, '22222222-0000-4000-8000-000000000007', 'SWITCH',     '網路交換器', 'IT_NETWORK.SWITCH','IT_NETWORK','HIGH'),
(NULL, '22222222-0000-4000-8000-000000000007', 'AP',         '無線基地台', 'IT_NETWORK.AP',   'IT_NETWORK','MEDIUM'),
(NULL, '22222222-0000-4000-8000-000000000008', 'ACCESS_CONTROL','門禁控制器','SECURITY.ACCESS_CONTROL','SECURITY','HIGH'),
(NULL, '22222222-0000-4000-8000-000000000008', 'CCTV',       '監視攝影機', 'SECURITY.CCTV',   'SECURITY','MEDIUM')
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 7. Notification templates
-- -----------------------------------------------------------------------------

INSERT INTO fms.notification_templates (tenant_id, code, channel, locale, subject_template, body_template) VALUES
(NULL, 'WO_ASSIGNED', 'EMAIL', 'zh-TW', '【工單派工】{{wo_no}} — {{title}}',
 '您好 {{assignee_name}}，'||chr(10)||'工單 {{wo_no}}（{{title}}）已派給您。'||chr(10)||
 '地點：{{facility_name}} / {{location_name}}'||chr(10)||'優先度：{{priority}}'||chr(10)||
 '要求完成時間：{{resolution_due_at}}'||chr(10)||'請於系統中確認並開始作業。'),
(NULL, 'WO_COMPLETED', 'EMAIL', 'zh-TW', '【工單完成】{{wo_no}} — {{title}}',
 '您回報的 {{wo_no}}（{{title}}）已於 {{completed_at}} 完成。'||chr(10)||
 '處理說明：{{resolution_notes}}'||chr(10)||'歡迎於系統中給予滿意度評價。'),
(NULL, 'WO_SLA_BREACH', 'EMAIL', 'zh-TW', '【SLA 逾期】{{wo_no}} 已超過要求完成時間',
 '工單 {{wo_no}}（{{title}}）已於 {{resolution_due_at}} 逾期，目前狀態 {{status}}，負責人 {{assignee_name}}。'),
(NULL, 'RESERVATION_CONFIRMED', 'EMAIL', 'zh-TW', '【預約確認】{{resource_name}} {{start_at}}',
 '您的預約已確認。'||chr(10)||'資源：{{resource_name}}'||chr(10)||
 '時間：{{start_at}} ~ {{end_at}}'||chr(10)||'附加服務：{{services_summary}}'),
(NULL, 'RESERVATION_APPROVAL_REQUIRED', 'EMAIL', 'zh-TW', '【待審核】{{organizer_name}} 申請 {{resource_name}}',
 '{{organizer_name}} 申請使用 {{resource_name}}，時間 {{start_at}} ~ {{end_at}}，請前往系統審核。'),
(NULL, 'ALARM_RAISED', 'EMAIL', 'zh-TW', '【{{severity}} 告警】{{message}}',
 '設備：{{asset_name}}（{{location_name}}）'||chr(10)||'告警：{{message}}'||chr(10)||
 '觸發值：{{trigger_value}}（門檻 {{threshold_value}}）'||chr(10)||'關聯工單：{{wo_no}}')
ON CONFLICT DO NOTHING;

COMMIT;
