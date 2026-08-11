// Dev-only stand-in for fms-server's Phase 0/1 surface.
//
// The real backend needs Postgres/Redis/MinIO (docker-compose) plus a `cargo run`,
// which this sandbox can't reach (Docker Hub pulls are blocked by network policy
// here). This mock implements auth plus enough of assets / work-orders /
// reservations / reports / notifications — with the demo accounts and permission
// shapes documented in docs/FRONTEND-GETTING-STARTED.md — so Phase 0 and Phase 1
// can be exercised end-to-end in a real browser. Swap VITE_API_BASE_URL back to
// the real server once one is reachable; nothing in src/ knows this mock exists.
import { createServer } from "node:http";
import { randomUUID as uuid } from "node:crypto";

const PORT = 8080;
const TENANT_ID = "aaaaaaaa-0000-4000-8000-000000000001";
const TENANT_CODE = "DEMO_GROUP";
const FACILITIES = [
  { id: "cccccccc-0000-4000-8000-000000000001", code: "TPE-HQ", name: "Taipei HQ", org_id: "bbbbbbbb-0000-4000-8000-000000000001", facility_type: "OFFICE", city: "台北", status: "ACTIVE" },
  { id: "cccccccc-0000-4000-8000-000000000002", code: "TPE-CINEMA", name: "Taipei Cinema", org_id: "bbbbbbbb-0000-4000-8000-000000000002", facility_type: "CINEMA", city: "台北", status: "ACTIVE" },
];
const [HQ, CINEMA] = FACILITIES;

const ADMIN_PERMS = [
  "asset:read", "asset:write", "asset_model:read", "asset_model:write", "facility:read", "facility:create", "facility:update", "maintenance_plan:read", "maintenance_plan:write", "maintenance_template:write",
  "service_item:read", "service_item:write", "work_order:read", "work_order:assign", "work_order:create", "work_order:execute", "work_order:update",
  "reservation:read", "reservation:create", "reservation:approve", "bookable_resource:write", "blackout:write", "device:read", "device:write",
  "calendar_integration:read", "calendar_integration:write",
  "alarm:read", "alarm:acknowledge", "alarm:suppress", "alarm_rule:read", "alarm_rule:write",
  "spatial_node:read", "spatial_node:write", "bim_model:read", "bim_model:write",
  "user:read", "user:write", "role:read", "role:write", "role:assign", "organization:read", "organization:write",
  "identity_provider:read", "identity_provider:write", "notification_template:read", "notification_template:write",
  "tenant:update", "team:read", "team:write", "audit:read", "report:read",
];
const FM_PERMS = [HQ].flatMap((f) => [
  `asset:read@FACILITY:${f.id}`, `facility:read@FACILITY:${f.id}`, `work_order:read@FACILITY:${f.id}`,
  `work_order:assign@FACILITY:${f.id}`, `reservation:read@FACILITY:${f.id}`, `reservation:approve@FACILITY:${f.id}`, `device:read@FACILITY:${f.id}`,
]);
const TECH_PERMS = ["asset:read", "work_order:read", "work_order:execute", "reservation:read"];
const REQUESTER_PERMS = ["work_order:create", "work_order:read_own", "reservation:read_own", "reservation:create"];

const USERS = {
  "admin.chen": { id: "d0000000-0000-4000-8000-000000000001", display_name: "陳岳峰", role: "TENANT_ADMIN", perms: ADMIN_PERMS, facilities: FACILITIES },
  "fm.lin": { id: "d0000000-0000-4000-8000-000000000002", display_name: "林家瑋", role: "FACILITY_ADMIN", perms: FM_PERMS, facilities: [HQ] },
  "tech.liu": { id: "d0000000-0000-4000-8000-000000000003", display_name: "劉建志", role: "TECHNICIAN", perms: TECH_PERMS, facilities: FACILITIES },
  "tech.wang": { id: "d0000000-0000-4000-8000-000000000004", display_name: "王思婷", role: "TECHNICIAN", perms: TECH_PERMS, facilities: FACILITIES },
  "user.huang": { id: "d0000000-0000-4000-8000-000000000005", display_name: "黃志明", role: "REQUESTER", perms: REQUESTER_PERMS, facilities: FACILITIES },
};
const PASSWORD = "Demo1234!";

// ---------------------------------------------------------------- seed data ----
const CATEGORIES = [
  { id: "e0000000-0000-4000-8000-000000000001", parent_id: null, code: "HVAC", name: "空調", category_path: "HVAC", depth: 0, domain: "MEP", default_criticality: "HIGH", is_active: true, asset_count: 3 },
  { id: "e0000000-0000-4000-8000-000000000002", parent_id: "e0000000-0000-4000-8000-000000000001", code: "HVAC.AHU", name: "空氣處理機", category_path: "HVAC.AHU", depth: 1, domain: "MEP", default_criticality: "HIGH", is_active: true, asset_count: 2 },
  { id: "e0000000-0000-4000-8000-000000000003", parent_id: "e0000000-0000-4000-8000-000000000001", code: "HVAC.CHILLER", name: "冰水機", category_path: "HVAC.CHILLER", depth: 1, domain: "MEP", default_criticality: "CRITICAL", is_active: true, asset_count: 1 },
  { id: "e0000000-0000-4000-8000-000000000004", parent_id: null, code: "ELEVATOR", name: "電梯", category_path: "ELEVATOR", depth: 0, domain: "VERTICAL_TRANSPORT", default_criticality: "CRITICAL", is_active: true, asset_count: 2 },
  { id: "e0000000-0000-4000-8000-000000000005", parent_id: null, code: "FIRE", name: "消防", category_path: "FIRE", depth: 0, domain: "SAFETY", default_criticality: "CRITICAL", is_active: true, asset_count: 1 },
];

const ASSET_MODELS = [
  { id: "f0000000-0000-4000-8000-000000000001", is_platform: true, category_code: "HVAC.AHU", manufacturer: "York", model_no: "YK-AHU-200", name: "YK-AHU-200 空氣處理機", specifications: {}, supported_protocols: ["MODBUS_TCP"], expected_life_months: 180, is_active: true },
  { id: "f0000000-0000-4000-8000-000000000002", is_platform: true, category_code: "HVAC.CHILLER", manufacturer: "Carrier", model_no: "CR-CHW-500", name: "CR-CHW-500 冰水機", specifications: {}, supported_protocols: ["BACNET_IP", "MODBUS_TCP"], expected_life_months: 240, is_active: true },
  { id: "f0000000-0000-4000-8000-000000000003", is_platform: false, category_code: "ELEVATOR", manufacturer: "Otis", model_no: "OT-GEN2", name: "OT-GEN2 客梯", specifications: {}, supported_protocols: [], expected_life_months: 300, is_active: false },
];

function makeAsset(overrides) {
  const now = "2026-08-01T00:00:00Z";
  return {
    id: uuid(),
    facility_id: HQ.id,
    spatial_node_id: null,
    spatial_node_path: null,
    serial_no: null,
    asset_model_id: null,
    parent_asset_id: null,
    install_date: "2022-03-01",
    warranty_end_date: "2026-09-01",
    health_score: 90,
    last_telemetry_at: null,
    open_work_order_count: 0,
    active_alarm_count: 0,
    specifications: {},
    attributes: {},
    version: 1,
    created_at: now,
    updated_at: now,
    ...overrides,
  };
}

const ASSETS = [
  makeAsset({ asset_code: "AHU-01", name: "3F 空氣處理機", category_code: "HVAC.AHU", criticality: "HIGH", status: "ACTIVE", health_score: 88, spatial_node_path: "TPE-HQ / 3F / 機房" }),
  makeAsset({ asset_code: "AHU-02", name: "5F 空氣處理機", category_code: "HVAC.AHU", criticality: "HIGH", status: "DEGRADED", health_score: 52, spatial_node_path: "TPE-HQ / 5F / 機房" }),
  makeAsset({ asset_code: "CHW-01", name: "主冰水機", category_code: "HVAC.CHILLER", criticality: "CRITICAL", status: "ACTIVE", health_score: 95, warranty_end_date: "2026-10-15", spatial_node_path: "TPE-HQ / B1 / 機電室" }),
  makeAsset({ asset_code: "ELEV-01", name: "客梯 A", category_code: "ELEVATOR", criticality: "CRITICAL", status: "DOWN", health_score: 30, spatial_node_path: "TPE-HQ / 1F / 大廳" }),
  makeAsset({ asset_code: "ELEV-02", name: "客梯 B", category_code: "ELEVATOR", criticality: "CRITICAL", status: "ACTIVE", health_score: 91, spatial_node_path: "TPE-HQ / 1F / 大廳" }),
  makeAsset({ asset_code: "SPR-01", name: "灑水系統主幹", category_code: "FIRE", criticality: "CRITICAL", status: "ACTIVE", health_score: 97, spatial_node_path: "TPE-HQ / 全棟" }),
  makeAsset({ asset_code: "SCR-01", name: "1 廳投影機", category_code: "HVAC.AHU", criticality: "MEDIUM", status: "ACTIVE", health_score: 80, facility_id: CINEMA.id, warranty_end_date: "2026-08-20", spatial_node_path: "TPE-CINEMA / 1 廳" }),
];

const STATUS_DICT = [
  { code: "NEW", name_zh: "新建立", name_en: "New", category: "OPEN", is_terminal: false, display_order: 1 },
  { code: "ASSIGNED", name_zh: "已派工", name_en: "Assigned", category: "OPEN", is_terminal: false, display_order: 2 },
  { code: "IN_PROGRESS", name_zh: "執行中", name_en: "In progress", category: "IN_PROGRESS", is_terminal: false, display_order: 3 },
  { code: "WAITING_PARTS", name_zh: "待料", name_en: "Waiting on parts", category: "WAITING", is_terminal: false, display_order: 4 },
  { code: "COMPLETED", name_zh: "已完成", name_en: "Completed", category: "TERMINAL", is_terminal: true, display_order: 5 },
  { code: "CANCELLED", name_zh: "已取消", name_en: "Cancelled", category: "TERMINAL", is_terminal: true, display_order: 6 },
];

const TRANSITIONS = {
  NEW: [{ action: "ASSIGN", to_status: "ASSIGNED", label_zh: "派工" }],
  ASSIGNED: [{ action: "START_WORK", to_status: "IN_PROGRESS", label_zh: "開始作業" }],
  IN_PROGRESS: [
    { action: "COMPLETE", to_status: "COMPLETED", label_zh: "完成" },
    { action: "PAUSE", to_status: "WAITING_PARTS", label_zh: "待料" },
  ],
  WAITING_PARTS: [{ action: "RESUME", to_status: "IN_PROGRESS", label_zh: "恢復作業" }],
  COMPLETED: [],
  CANCELLED: [],
};

let woCounter = 480;
function makeWorkOrder(overrides) {
  const now = new Date().toISOString();
  const status = overrides.status ?? "NEW";
  return {
    id: uuid(),
    wo_no: `WO-2026-${String(++woCounter).padStart(6, "0")}`,
    facility_id: HQ.id,
    work_order_type: "CORRECTIVE",
    source: "MANUAL",
    description: null,
    status,
    status_category: STATUS_DICT.find((s) => s.code === status)?.category ?? "OPEN",
    priority: "MEDIUM",
    asset: null,
    location: null,
    service_item_id: null,
    reservation_id: null,
    alarm_id: null,
    requester: { id: USERS["user.huang"].id, display_name: USERS["user.huang"].display_name },
    assignee: null,
    team_id: null,
    payload: {},
    scheduled_start_at: null,
    scheduled_end_at: null,
    actual_start_at: null,
    actual_end_at: null,
    response_due_at: null,
    resolution_due_at: null,
    sla_state: "ON_TRACK",
    labor_minutes: 0,
    total_cost: null,
    satisfaction_score: null,
    version: 1,
    created_at: now,
    updated_at: now,
    completed_at: null,
    tasks: [],
    comments: [],
    transitions: [],
    ...overrides,
  };
}

function assetRef(asset) {
  return asset && { id: asset.id, asset_code: asset.asset_code, name: asset.name };
}

const WORK_ORDERS = [
  makeWorkOrder({ title: "客梯 A 異常噪音", description: "客戶反映電梯上下樓時有明顯異音。", status: "IN_PROGRESS", priority: "URGENT", asset: assetRef(ASSETS[3]), assignee: { id: USERS["tech.liu"].id, display_name: USERS["tech.liu"].display_name }, sla_state: "RESPONSE_DUE_SOON" }),
  makeWorkOrder({ title: "5F AHU 效率下降，需檢修", description: "健康分數持續下滑，疑似濾網堵塞。", status: "ASSIGNED", priority: "HIGH", work_order_type: "MAINTENANCE", asset: assetRef(ASSETS[1]), assignee: { id: USERS["tech.wang"].id, display_name: USERS["tech.wang"].display_name } }),
  makeWorkOrder({ title: "3F 會議室冷氣太冷", description: "使用者報修，溫度設定疑似異常。", status: "NEW", priority: "MEDIUM", work_order_type: "SERVICE" }),
  makeWorkOrder({
    title: "灑水系統年度巡檢", status: "WAITING_PARTS", priority: "HIGH", work_order_type: "INSPECTION", asset: assetRef(ASSETS[5]),
    assignee: { id: USERS["tech.liu"].id, display_name: USERS["tech.liu"].display_name }, sla_state: "RESOLUTION_BREACHED",
    tasks: [
      { id: uuid(), seq: 1, title: "確認灑水頭無阻塞", input_type: "CHECKBOX", is_required: true, result_value: true, is_pass: true, completed_at: "2026-08-01T09:00:00Z" },
      { id: uuid(), seq: 2, title: "測試消防泵啟動", input_type: "CHECKBOX", is_required: true, result_value: null, is_pass: null, completed_at: null },
    ],
  }),
  makeWorkOrder({ title: "1 廳投影機更換燈泡", status: "COMPLETED", priority: "LOW", work_order_type: "CORRECTIVE", asset: assetRef(ASSETS[6]), facility_id: CINEMA.id, assignee: { id: USERS["tech.wang"].id, display_name: USERS["tech.wang"].display_name }, completed_at: "2026-07-28T10:00:00Z" }),
  makeWorkOrder({ title: "冰水機年度保養", status: "CANCELLED", priority: "MEDIUM", work_order_type: "MAINTENANCE", asset: assetRef(ASSETS[2]) }),
];
ASSETS[3].open_work_order_count = 1;
ASSETS[1].open_work_order_count = 1;
ASSETS[5].open_work_order_count = 1;

const RESOURCES = [
  { resource_id: "f0000000-0000-4000-8000-000000000001", facility_id: HQ.id, resource_type: "SPATIAL_NODE", display_name: "3F 會議室 A", capacity: 8, is_bookable: true, rules: { min_duration_minutes: 30, max_duration_minutes: 240, slot_granularity_minutes: 60, requires_approval: false, advance_booking_days: 30, buffer_before_minutes: 0, buffer_after_minutes: 0, min_notice_minutes: 0, requires_check_in: false, auto_release_minutes: null, approver_role_code: null, max_active_per_user: null, opening_hours: {} } },
  { resource_id: "f0000000-0000-4000-8000-000000000002", facility_id: HQ.id, resource_type: "SPATIAL_NODE", display_name: "3F 會議室 B", capacity: 4, is_bookable: true, rules: { min_duration_minutes: 30, max_duration_minutes: 120, slot_granularity_minutes: 60, requires_approval: true, advance_booking_days: 14, buffer_before_minutes: 15, buffer_after_minutes: 15, min_notice_minutes: 60, requires_check_in: true, auto_release_minutes: 15, approver_role_code: "FACILITY_ADMIN", max_active_per_user: 3, opening_hours: {} } },
  { resource_id: "f0000000-0000-4000-8000-000000000003", facility_id: CINEMA.id, resource_type: "SPATIAL_NODE", display_name: "1 廳", capacity: 120, is_bookable: true, rules: { min_duration_minutes: 60, max_duration_minutes: 180, slot_granularity_minutes: 60, requires_approval: true, advance_booking_days: 60, buffer_before_minutes: 30, buffer_after_minutes: 30, min_notice_minutes: 1440, requires_check_in: false, auto_release_minutes: null, approver_role_code: null, max_active_per_user: null, opening_hours: {} } },
];

const BLACKOUTS = [];

const CALENDAR_INTEGRATIONS = [];
const CALENDAR_RESOURCE_MAPPINGS = [];
// 尚未對應到任何空間節點的外部房間——模擬 Microsoft Graph 的 list_resources()。
const CALENDAR_EXTERNAL_ROOMS = [
  { external_id: "room-3f-a@contoso.com", display_name: "3F Conference Room A" },
  { external_id: "room-3f-b@contoso.com", display_name: "3F Conference Room B" },
];
let blackoutCounter = 1;

function toBookableResource(r) {
  return {
    id: r.resource_id,
    facility_id: r.facility_id,
    resource_type: r.resource_type,
    resource_id: r.resource_id,
    display_name: r.display_name,
    is_bookable: r.is_bookable !== false,
    requires_approval: !!r.rules.requires_approval,
    approver_role_code: r.rules.approver_role_code ?? null,
    requires_check_in: !!r.rules.requires_check_in,
    auto_release_minutes: r.rules.auto_release_minutes ?? null,
    min_duration_minutes: r.rules.min_duration_minutes,
    max_duration_minutes: r.rules.max_duration_minutes,
    slot_granularity_minutes: r.rules.slot_granularity_minutes,
    buffer_before_minutes: r.rules.buffer_before_minutes ?? 0,
    buffer_after_minutes: r.rules.buffer_after_minutes ?? 0,
    advance_booking_days: r.rules.advance_booking_days,
    min_notice_minutes: r.rules.min_notice_minutes ?? 0,
    max_active_per_user: r.rules.max_active_per_user ?? null,
    capacity: r.capacity,
    opening_hours: r.rules.opening_hours ?? {},
    attributes: r.rules.attributes ?? {},
  };
}

let reservationCounter = 1200;
function makeReservation(overrides) {
  return {
    id: uuid(),
    reservation_no: `RSV-2026-${String(++reservationCounter).padStart(6, "0")}`,
    facility_id: HQ.id,
    resource_id: RESOURCES[0].resource_id,
    resource_name: RESOURCES[0].display_name,
    resource_type: "SPATIAL_NODE",
    title: "Team sync",
    purpose: null,
    party_size: 2,
    status: "CONFIRMED",
    organizer: { id: USERS["admin.chen"].id, display_name: USERS["admin.chen"].display_name },
    approval_required: false,
    requires_check_in: true,
    checked_in_at: null,
    auto_release_at: null,
    recurrence_group_id: null,
    created_via: "APP",
    version: 1,
    is_private: false,
    services: [],
    ...overrides,
  };
}

function expandRecurrenceRule(rule, startAt, endAt) {
  const parts = Object.fromEntries(rule.split(";").filter(Boolean).map((p) => p.split("=")));
  const count = Math.min(Number(parts.COUNT) || 1, 52);
  const stepMs = parts.FREQ === "WEEKLY" ? 7 * 86400_000 : 86400_000;
  const start = new Date(startAt).getTime();
  const end = new Date(endAt).getTime();
  return Array.from({ length: count }, (_, i) => ({
    start_at: new Date(start + i * stepMs).toISOString(),
    end_at: new Date(end + i * stepMs).toISOString(),
  }));
}

const todayAt = (h, m = 0) => {
  const d = new Date();
  d.setHours(h, m, 0, 0);
  return d.toISOString();
};

const RESERVATIONS = [
  makeReservation({ title: "產品週會", start_at: todayAt(10), end_at: todayAt(11), status: "CONFIRMED" }),
  makeReservation({ title: "供應商洽談", resource_id: RESOURCES[1].resource_id, resource_name: RESOURCES[1].display_name, start_at: todayAt(14), end_at: todayAt(15, 30), status: "PENDING_APPROVAL", approval_required: true, organizer: { id: USERS["user.huang"].id, display_name: USERS["user.huang"].display_name } }),
  makeReservation({ title: "私人面談", start_at: todayAt(16), end_at: todayAt(16, 30), is_private: true, organizer: { id: USERS["fm.lin"].id, display_name: USERS["fm.lin"].display_name } }),
  makeReservation({ title: "包場試片", resource_id: RESOURCES[2].resource_id, resource_name: RESOURCES[2].display_name, facility_id: CINEMA.id, party_size: 40, start_at: "2026-07-20T13:00:00+08:00", end_at: "2026-07-20T15:00:00+08:00", status: "COMPLETED", checked_in_at: "2026-07-20T13:05:00+08:00" }),
];

const HOLDS = new Map();

let notificationCounter = 0;
function makeNotification(username, overrides) {
  notificationCounter += 1;
  return {
    id: uuid(),
    username,
    subject: null,
    priority: "NORMAL",
    entity_type: null,
    entity_id: null,
    template_code: null,
    created_at: new Date(Date.now() - notificationCounter * 3600_000).toISOString(),
    read_at: null,
    ...overrides,
  };
}

const NOTIFICATIONS = [
  makeNotification("admin.chen", { subject: "客梯 A 異常噪音已派工", body: "WO-2026-000481 已指派給 劉建志。", entity_type: "WORK_ORDER" }),
  makeNotification("admin.chen", { subject: "供應商洽談 待審核", body: "黃志明 申請預約 3F 會議室 B，等待核准。", entity_type: "RESERVATION", read_at: new Date().toISOString() }),
  makeNotification("user.huang", { subject: "您的預約已送出", body: "供應商洽談 已送出待審核。", entity_type: "RESERVATION" }),
  makeNotification("tech.liu", { subject: "新工單指派給您", body: "WO-2026-000481 客梯 A 異常噪音。", priority: "HIGH", entity_type: "WORK_ORDER" }),
];

// -------------------------------------------------------- Phase 2 seed data ----
const NODE_TYPES = [
  { id: "n0000000-0000-4000-8000-000000000001", code: "BUILDING", name: "建築", level_hint: 1, is_bookable: false, is_platform: true },
  { id: "n0000000-0000-4000-8000-000000000002", code: "FLOOR", name: "樓層", level_hint: 2, is_bookable: false, is_platform: true },
  { id: "n0000000-0000-4000-8000-000000000003", code: "ROOM", name: "房間", level_hint: 3, is_bookable: true, is_platform: true },
  { id: "n0000000-0000-4000-8000-000000000004", code: "ZONE", name: "區域", level_hint: 3, is_bookable: false, is_platform: true },
];

function makeSpatialNode(overrides) {
  return {
    id: uuid(),
    facility_id: HQ.id,
    parent_id: null,
    depth: 2,
    area_sqm: null,
    capacity: 0,
    is_bookable: false,
    status: "ACTIVE",
    health_score: null,
    utilization_pct: null,
    bim_element_id: null,
    asset_count: 0,
    open_work_order_count: 0,
    geometry: {},
    ...overrides,
  };
}

const SPATIAL_NODES = [
  // FLOOR 容器節點：真實後端每個樓層本身就是一列 spatial_nodes（node_type_code
  // = 'FLOOR'），平面圖影像／標點都掛在這一列上，不是掛在樓層裡的房間上。
  makeSpatialNode({ code: "3F", name: "3樓", node_type_code: "FLOOR", node_path: "TPE-HQ/3F", depth: 1, floor_level: 3, floor_label: "3F" }),
  makeSpatialNode({ code: "1F", name: "1樓", node_type_code: "FLOOR", node_path: "TPE-HQ/1F", depth: 1, floor_level: 1, floor_label: "1F" }),
  makeSpatialNode({ code: "5F", name: "5樓", node_type_code: "FLOOR", node_path: "TPE-HQ/5F", depth: 1, floor_level: 5, floor_label: "5F" }),
  makeSpatialNode({ code: "B1", name: "地下一樓", node_type_code: "FLOOR", node_path: "TPE-HQ/B1", depth: 1, floor_level: -1, floor_label: "B1" }),
  makeSpatialNode({ code: "CIN-1F", name: "1樓", node_type_code: "FLOOR", node_path: "TPE-CINEMA/1F", depth: 1, floor_level: 1, floor_label: "1F", facility_id: CINEMA.id }),
  makeSpatialNode({ id: RESOURCES[0].resource_id, code: "3F-MTG-A", name: "3F 會議室 A", node_type_code: "ROOM", node_path: "TPE-HQ/3F/3F-MTG-A", floor_level: 3, floor_label: "3F", is_bookable: true, capacity: 8, geometry: { type: "bbox", min: [0, 0], max: [8, 6] } }),
  makeSpatialNode({ id: RESOURCES[1].resource_id, code: "3F-MTG-B", name: "3F 會議室 B", node_type_code: "ROOM", node_path: "TPE-HQ/3F/3F-MTG-B", floor_level: 3, floor_label: "3F", is_bookable: true, capacity: 4, geometry: { type: "bbox", min: [9, 0], max: [14, 5] } }),
  makeSpatialNode({ code: "3F-CORRIDOR", name: "3F 走廊", node_type_code: "ZONE", node_path: "TPE-HQ/3F/3F-CORRIDOR", floor_level: 3, floor_label: "3F", geometry: { type: "bbox", min: [0, 7], max: [14, 9] } }),
  makeSpatialNode({ code: "1F-LOBBY", name: "1F 大廳", node_type_code: "ZONE", node_path: "TPE-HQ/1F/1F-LOBBY", floor_level: 1, floor_label: "1F", asset_count: 2, open_work_order_count: 1, geometry: { type: "bbox", min: [0, 0], max: [12, 10] } }),
  makeSpatialNode({ code: "5F-MECH", name: "5F 機房", node_type_code: "ROOM", node_path: "TPE-HQ/5F/5F-MECH", floor_level: 5, floor_label: "5F", asset_count: 1, open_work_order_count: 1, geometry: { type: "bbox", min: [0, 0], max: [6, 5] } }),
  makeSpatialNode({ code: "B1-MECH", name: "B1 機電室", node_type_code: "ROOM", node_path: "TPE-HQ/B1/B1-MECH", floor_level: -1, floor_label: "B1", asset_count: 1, geometry: { type: "bbox", min: [0, 0], max: [10, 8] } }),
  makeSpatialNode({ id: RESOURCES[2].resource_id, code: "SCREEN-1", name: "1 廳", node_type_code: "ROOM", node_path: "TPE-CINEMA/1F/SCREEN-1", floor_level: 1, floor_label: "1F", facility_id: CINEMA.id, is_bookable: true, capacity: 120, geometry: { type: "bbox", min: [0, 0], max: [20, 15] } }),
];

const FLOOR_PLAN_MARKERS = [];
function floorPlanMarkerDto(m) {
  const entity =
    m.entity_type === "ASSET" ? ASSETS.find((a) => a.id === m.entity_id) :
    m.entity_type === "DEVICE" ? DEVICES.find((d) => d.id === m.entity_id) :
    SPATIAL_NODES.find((n) => n.id === m.entity_id);
  return { ...m, entity_label: entity?.name ?? null, entity_status: entity?.status ?? null };
}

const BIM_MODELS = new Map(); // id -> { ...BimModel, registeredAt }

const MAINT_TEMPLATES = [
  { id: "t0000000-0000-4000-8000-000000000001", tenant_id: null, code: "TPL-AHU-FILTER", name: "AHU 濾網更換", description: "更換空氣處理機濾網並檢查風量。", maintenance_type: "PREVENTIVE", checklist: [], estimated_minutes: 60, required_skill_codes: [], required_part_codes: [], is_active: true, plan_count: 1, created_at: "2026-01-01T00:00:00Z" },
  { id: "t0000000-0000-4000-8000-000000000002", tenant_id: null, code: "TPL-ELEV-ANNUAL", name: "電梯年度檢驗", description: "依法規進行年度電梯安全檢驗。", maintenance_type: "STATUTORY", checklist: [], estimated_minutes: 180, required_skill_codes: ["ELEV_CERT"], required_part_codes: [], is_active: true, plan_count: 1, created_at: "2026-01-01T00:00:00Z" },
];

let planCounter = 0;
function makeMaintenancePlan(overrides) {
  planCounter += 1;
  return {
    id: uuid(),
    facility_id: HQ.id,
    generate_lead_days: 7,
    priority: "MEDIUM",
    assigned_team_id: null,
    is_active: true,
    ...overrides,
  };
}

const MAINTENANCE_PLANS = [
  makeMaintenancePlan({ code: "PM-AHU02-M", name: "5F AHU 每月濾網保養", template_id: MAINT_TEMPLATES[0].id, template_name: MAINT_TEMPLATES[0].name, target: { type: "ASSET", id: ASSETS[1].id, label: ASSETS[1].name }, asset_id: ASSETS[1].id, trigger_type: "CALENDAR", rrule: "FREQ=MONTHLY;INTERVAL=1", next_due_at: new Date(Date.now() + 5 * 86400_000).toISOString() }),
  makeMaintenancePlan({ code: "PM-ELEV02-Y", name: "客梯 B 年度檢驗", template_id: MAINT_TEMPLATES[1].id, template_name: MAINT_TEMPLATES[1].name, target: { type: "ASSET", id: ASSETS[4].id, label: ASSETS[4].name }, asset_id: ASSETS[4].id, trigger_type: "CALENDAR", rrule: "FREQ=YEARLY;INTERVAL=1", next_due_at: new Date(Date.now() + 40 * 86400_000).toISOString() }),
];

let occurrenceCounter = 0;
function makeOccurrence(overrides) {
  occurrenceCounter += 1;
  const scheduled = overrides.scheduled_for ? new Date(overrides.scheduled_for) : new Date();
  const deadline = new Date(scheduled.getTime() + (overrides.grace_days ?? 3) * 86400_000);
  return {
    id: uuid(),
    facility_id: HQ.id,
    work_order_id: null,
    work_order_no: null,
    skip_reason: null,
    generated_at: null,
    completed_at: null,
    grace_days: 3,
    deadline: deadline.toISOString(),
    is_missed: false,
    is_late: false,
    days_late: null,
    ...overrides,
  };
}

const MAINTENANCE_OCCURRENCES = [
  makeOccurrence({ plan_id: MAINTENANCE_PLANS[0].id, plan_code: MAINTENANCE_PLANS[0].code, plan_name: MAINTENANCE_PLANS[0].name, asset_id: ASSETS[1].id, asset_code: ASSETS[1].asset_code, scheduled_for: new Date(Date.now() + 5 * 86400_000).toISOString(), status: "PLANNED" }),
  makeOccurrence({ plan_id: MAINTENANCE_PLANS[0].id, plan_code: MAINTENANCE_PLANS[0].code, plan_name: MAINTENANCE_PLANS[0].name, asset_id: ASSETS[1].id, asset_code: ASSETS[1].asset_code, scheduled_for: new Date(Date.now() - 25 * 86400_000).toISOString(), status: "COMPLETED", completed_at: new Date(Date.now() - 24 * 86400_000).toISOString() }),
  makeOccurrence({ plan_id: MAINTENANCE_PLANS[0].id, plan_code: MAINTENANCE_PLANS[0].code, plan_name: MAINTENANCE_PLANS[0].name, asset_id: ASSETS[1].id, asset_code: ASSETS[1].asset_code, scheduled_for: new Date(Date.now() - 55 * 86400_000).toISOString(), status: "MISSED", is_missed: true, grace_days: 3, days_late: 20 }),
  makeOccurrence({ plan_id: MAINTENANCE_PLANS[1].id, plan_code: MAINTENANCE_PLANS[1].code, plan_name: MAINTENANCE_PLANS[1].name, asset_id: ASSETS[4].id, asset_code: ASSETS[4].asset_code, scheduled_for: new Date(Date.now() + 40 * 86400_000).toISOString(), status: "PLANNED" }),
];

const DEVICES = [
  { id: uuid(), facility_id: HQ.id, facility_name: HQ.name, gateway_id: null, asset_id: ASSETS[0].id, asset_code: ASSETS[0].asset_code, spatial_node_id: null, location_name: "3F 機房", device_code: "DEV-AHU01-T", name: "AHU-01 溫度感測器", device_type: "SENSOR", heartbeat_interval_seconds: 300, offline_alarm_after_seconds: 900, last_seen_at: new Date(Date.now() - 60_000).toISOString(), status: "ONLINE", connectivity: "ONLINE", seconds_since_seen: 60, point_count: 1, created_at: "2026-01-01T00:00:00Z" },
  { id: uuid(), facility_id: HQ.id, facility_name: HQ.name, gateway_id: null, asset_id: ASSETS[1].id, asset_code: ASSETS[1].asset_code, spatial_node_id: null, location_name: "5F 機房", device_code: "DEV-AHU02-T", name: "AHU-02 溫度感測器", device_type: "SENSOR", heartbeat_interval_seconds: 300, offline_alarm_after_seconds: 900, last_seen_at: new Date(Date.now() - 120_000).toISOString(), status: "ONLINE", connectivity: "ONLINE", seconds_since_seen: 120, point_count: 1, created_at: "2026-01-01T00:00:00Z" },
  { id: uuid(), facility_id: HQ.id, facility_name: HQ.name, gateway_id: null, asset_id: ASSETS[2].id, asset_code: ASSETS[2].asset_code, spatial_node_id: null, location_name: "B1 機電室", device_code: "DEV-CHW01-F", name: "冰水機流量計", device_type: "METER", heartbeat_interval_seconds: 300, offline_alarm_after_seconds: 900, last_seen_at: new Date(Date.now() - 90_000).toISOString(), status: "ONLINE", connectivity: "ONLINE", seconds_since_seen: 90, point_count: 1, created_at: "2026-01-01T00:00:00Z" },
  { id: uuid(), facility_id: HQ.id, facility_name: HQ.name, gateway_id: null, asset_id: ASSETS[3].id, asset_code: ASSETS[3].asset_code, spatial_node_id: null, location_name: "1F 大廳", device_code: "DEV-ELEV01-V", name: "客梯 A 震動感測器", device_type: "SENSOR", heartbeat_interval_seconds: 60, offline_alarm_after_seconds: 300, last_seen_at: new Date(Date.now() - 40 * 60_000).toISOString(), status: "OFFLINE", connectivity: "OFFLINE", seconds_since_seen: 40 * 60, point_count: 1, created_at: "2026-01-01T00:00:00Z" },
  { id: uuid(), facility_id: HQ.id, facility_name: HQ.name, gateway_id: null, asset_id: null, asset_code: null, spatial_node_id: null, location_name: "頂樓", device_code: "DEV-GW-01", name: "頂樓閘道器", device_type: "GATEWAY", heartbeat_interval_seconds: 60, offline_alarm_after_seconds: 300, last_seen_at: null, status: "UNKNOWN", connectivity: "NEVER_SEEN", seconds_since_seen: null, point_count: 0, created_at: "2026-01-01T00:00:00Z" },
];

const TELEMETRY_POINTS = [
  { telemetry_point_id: uuid(), point_code: "TEMP", point_name: "AHU-01 出風溫度", unit: "°C", device_id: DEVICES[0].id, device_code: DEVICES[0].device_code, facility_id: HQ.id, asset_id: ASSETS[0].id, base: 22, amplitude: 1.5 },
  { telemetry_point_id: uuid(), point_code: "TEMP", point_name: "AHU-02 出風溫度", unit: "°C", device_id: DEVICES[1].id, device_code: DEVICES[1].device_code, facility_id: HQ.id, asset_id: ASSETS[1].id, base: 29, amplitude: 2 },
  { telemetry_point_id: uuid(), point_code: "FLOW", point_name: "冰水機流量", unit: "L/min", device_id: DEVICES[2].id, device_code: DEVICES[2].device_code, facility_id: HQ.id, asset_id: ASSETS[2].id, base: 480, amplitude: 20 },
];

function telemetryNow(point) {
  const noise = Math.sin(Date.now() / 900_000) * point.amplitude;
  return Math.round((point.base + noise) * 10) / 10;
}

const ALARM_RULES = [
  { id: uuid(), facility_id: HQ.id, facility_name: HQ.name, code: "AHU-OVERTEMP", name: "AHU 出風溫度過高", description: "出風溫度超過 28°C", telemetry_point_id: null, point_code: "TEMP", asset_category_id: null, rule_type: "THRESHOLD", condition: { op: ">", value: 28 }, severity: "MAJOR", debounce_seconds: 60, auto_clear: true, auto_create_work_order: false, wo_work_order_type: null, wo_priority: null, wo_team_id: null, wo_sla_policy_id: null, dedupe_window_minutes: 120, notify_role_codes: [], is_active: true, covered_point_count: 1, evaluable: true, created_at: "2026-01-01T00:00:00Z" },
  { id: uuid(), facility_id: HQ.id, facility_name: HQ.name, code: "ELEV-OFFLINE", name: "客梯感測器離線", description: "震動感測器超過 30 分鐘沒回報", telemetry_point_id: null, point_code: "VIBRATION", asset_category_id: null, rule_type: "DEVICE_OFFLINE", condition: { op: "offline_minutes", value: 30 }, severity: "CRITICAL", debounce_seconds: 60, auto_clear: false, auto_create_work_order: true, wo_work_order_type: "CORRECTIVE", wo_priority: "URGENT", wo_team_id: null, wo_sla_policy_id: null, dedupe_window_minutes: 120, notify_role_codes: [], is_active: true, covered_point_count: 0, evaluable: false, created_at: "2026-01-01T00:00:00Z" },
];

function makeAlarm(overrides) {
  return {
    id: uuid(),
    alarm_no: `ALM-2026-${String(Math.floor(Math.random() * 9000) + 100)}`,
    facility_id: HQ.id,
    rule_code: null,
    trigger_value: null,
    threshold_value: null,
    occurrence_count: 1,
    asset: null,
    location: null,
    work_order: null,
    first_seen_at: new Date(Date.now() - 3600_000).toISOString(),
    last_seen_at: new Date().toISOString(),
    acknowledged_at: null,
    cleared_at: null,
    suppressed_until: null,
    ...overrides,
  };
}

const ALARMS = [
  makeAlarm({ severity: "MAJOR", status: "ACTIVE", rule_code: "AHU-OVERTEMP", message: "5F AHU 出風溫度 29.8°C，超過門檻 28°C。", trigger_value: 29.8, threshold_value: 28, asset: assetRef(ASSETS[1]), location: { spatial_node_id: SPATIAL_NODES[4].id, name: SPATIAL_NODES[4].name } }),
  makeAlarm({ severity: "CRITICAL", status: "ACTIVE", rule_code: "ELEV-OFFLINE", device_id: DEVICES[3].id, message: "客梯 A 震動感測器離線超過 30 分鐘。", asset: assetRef(ASSETS[3]), location: { spatial_node_id: SPATIAL_NODES[3].id, name: SPATIAL_NODES[3].name }, work_order: { id: WORK_ORDERS[0].id, wo_no: WORK_ORDERS[0].wo_no, status: WORK_ORDERS[0].status } }),
  makeAlarm({ severity: "WARNING", status: "ACKNOWLEDGED", rule_code: "AHU-OVERTEMP", message: "冰水機流量偏低。", trigger_value: 410, threshold_value: 450, asset: assetRef(ASSETS[2]), location: { spatial_node_id: SPATIAL_NODES[5].id, name: SPATIAL_NODES[5].name }, acknowledged_at: new Date(Date.now() - 1800_000).toISOString() }),
  makeAlarm({ severity: "MINOR", status: "CLEARED", rule_code: "AHU-OVERTEMP", message: "3F 走廊煙霧感測器誤報。", asset: null, location: { spatial_node_id: SPATIAL_NODES[2].id, name: SPATIAL_NODES[2].name }, cleared_at: new Date(Date.now() - 86400_000).toISOString() }),
];

// -------------------------------------------------------- Phase 3 seed data ----
const MOCK_USERS = Object.entries(USERS).map(([username, u]) => ({
  id: u.id,
  username,
  employee_no: null,
  email: `${username}@demo.bizlution.ai`,
  display_name: u.display_name,
  phone: null,
  job_title: u.role.replaceAll("_", " "),
  user_type: "EMPLOYEE",
  primary_org_id: u.facilities[0]?.org_id ?? null,
  default_facility_id: u.facilities[0]?.id ?? null,
  status: "ACTIVE",
  identity_sources: [],
  skills: [],
  last_login_at: new Date(Date.now() - 3600_000).toISOString(),
}));

const ROLE_ASSIGNMENTS = new Map(); // userId -> RoleAssignment[]
for (const [username, u] of Object.entries(USERS)) {
  const scopeType = u.role === "TENANT_ADMIN" ? "TENANT" : "FACILITY";
  ROLE_ASSIGNMENTS.set(u.id, [
    { id: uuid(), user_id: u.id, role_code: u.role, role_name: u.role.replaceAll("_", " "), scope_type: scopeType, scope_id: scopeType === "FACILITY" ? u.facilities[0]?.id ?? null : null, scope_label: scopeType === "FACILITY" ? u.facilities[0]?.name ?? null : "Tenant-wide", source: "MANUAL", valid_until: null },
  ]);
  void username;
}

const ROLES = [
  { id: uuid(), tenant_id: null, code: "TENANT_ADMIN", name: "Tenant admin", description: "Full access across the tenant.", is_system: true, is_assignable: true, scope_level: "TENANT", permissions: ADMIN_PERMS },
  { id: uuid(), tenant_id: null, code: "FACILITY_ADMIN", name: "Facility admin", description: "Manages one facility end to end.", is_system: true, is_assignable: true, scope_level: "FACILITY", permissions: FM_PERMS.map((p) => p.split("@")[0]) },
  { id: uuid(), tenant_id: null, code: "TECHNICIAN", name: "Technician", description: "Executes assigned work.", is_system: true, is_assignable: true, scope_level: "FACILITY", permissions: TECH_PERMS },
  { id: uuid(), tenant_id: null, code: "REQUESTER", name: "Requester", description: "Raises requests, books spaces.", is_system: true, is_assignable: true, scope_level: "FACILITY", permissions: REQUESTER_PERMS },
];

const PERMISSION_DEFS = [
  ["asset:read", "ASSET", false], ["asset:write", "ASSET", false], ["asset:delete", "ASSET", true],
  ["facility:read", "CORE", false], ["tenant:update", "CORE", true],
  ["spatial_node:read", "ASSET", false], ["spatial_node:write", "ASSET", false],
  ["bim_model:read", "ASSET", false], ["bim_model:write", "ASSET", false],
  ["maintenance_plan:read", "MAINTENANCE", false], ["maintenance_plan:write", "MAINTENANCE", false], ["maintenance_template:write", "MAINTENANCE", true],
  ["service_item:read", "SERVICE", false], ["service_item:write", "SERVICE", false],
  ["work_order:read", "SERVICE", false], ["work_order:create", "SERVICE", false], ["work_order:assign", "SERVICE", false], ["work_order:execute", "SERVICE", false],
  ["reservation:read", "RESERVATION", false], ["reservation:create", "RESERVATION", false], ["reservation:approve", "RESERVATION", false],
  ["device:read", "IOT", false], ["device:write", "IOT", true],
  ["alarm:read", "IOT", false], ["alarm:acknowledge", "IOT", false], ["alarm:suppress", "IOT", false],
  ["alarm_rule:read", "IOT", false], ["alarm_rule:write", "IOT", true],
  ["user:read", "ADMIN", false], ["user:write", "ADMIN", true],
  ["role:read", "ADMIN", false], ["role:write", "ADMIN", true], ["role:assign", "ADMIN", true],
  ["identity_provider:read", "ADMIN", false], ["identity_provider:write", "ADMIN", true],
  ["notification_template:read", "ADMIN", false], ["notification_template:write", "ADMIN", false],
  ["team:read", "ADMIN", false], ["team:write", "ADMIN", false],
  ["audit:read", "ADMIN", false], ["report:read", "CORE", false],
];
const PERMISSIONS = PERMISSION_DEFS.map(([code, module, is_dangerous]) => ({
  code, module, is_dangerous, resource: code.split(":")[0], action: code.split(":")[1],
  description: `${code.split(":")[1]} on ${code.split(":")[0]}`, min_scope_level: "FACILITY",
}));

let auditCounter = 0;
function makeAuditEntry(overrides) {
  auditCounter += 1;
  return { id: auditCounter, occurred_at: new Date(Date.now() - auditCounter * 1800_000).toISOString(), actor_type: "USER", diff_keys: [], request_id: uuid(), ip_address: "10.0.0.1", ...overrides };
}
const AUDIT_LOG = [
  makeAuditEntry({ actor_user_id: USERS["admin.chen"].id, actor_name: "陳岳峰", action: "work_order.transition", entity_type: "WORK_ORDER", entity_id: WORK_ORDERS[0]?.id, diff_keys: ["status"] }),
  makeAuditEntry({ actor_user_id: USERS["fm.lin"].id, actor_name: "林家瑋", action: "reservation.approve", entity_type: "RESERVATION", diff_keys: ["status"] }),
  makeAuditEntry({ actor_user_id: USERS["admin.chen"].id, actor_name: "陳岳峰", action: "asset.create", entity_type: "ASSET", diff_keys: ["asset_code", "name"] }),
  makeAuditEntry({ actor_user_id: null, actor_name: null, actor_type: "SYSTEM", action: "maintenance.occurrence_generated", entity_type: "MAINTENANCE_OCCURRENCE" }),
  makeAuditEntry({ actor_user_id: USERS["admin.chen"].id, actor_name: "陳岳峰", action: "role.assign", entity_type: "ROLE_ASSIGNMENT", diff_keys: ["role_code"] }),
];

const IDENTITY_PROVIDERS = [
  { id: uuid(), code: "LOCAL", name: "本機帳號", provider_type: "LOCAL", scope_org_path: null, issuer: null, jit_provisioning: false, sync_enabled: false, scim_enabled: false, is_default: true, status: "ACTIVE", last_sync_at: null },
  { id: uuid(), code: "ENTRA", name: "Microsoft Entra ID", provider_type: "OIDC", scope_org_path: null, issuer: "https://login.microsoftonline.com/demo-tenant/v2.0", jit_provisioning: true, sync_enabled: true, scim_enabled: false, is_default: false, status: "ACTIVE", last_sync_at: new Date(Date.now() - 86400_000).toISOString() },
];

const DIRECTORY_GROUPS = [
  { id: uuid(), identity_provider_id: IDENTITY_PROVIDERS[1].id, external_group_id: "grp-fm-admins", name: "FM-Admins", distinguished_name: "CN=FM-Admins,OU=Groups,DC=demo,DC=local", description: "設施管理員", member_count: 4, member_count_in_fms: 3, last_synced_at: new Date(Date.now() - 86400_000).toISOString(), created_at: new Date(Date.now() - 30 * 86400_000).toISOString() },
  { id: uuid(), identity_provider_id: IDENTITY_PROVIDERS[1].id, external_group_id: "grp-technicians", name: "Technicians", distinguished_name: "CN=Technicians,OU=Groups,DC=demo,DC=local", description: "維修技師", member_count: 12, member_count_in_fms: 10, last_synced_at: new Date(Date.now() - 86400_000).toISOString(), created_at: new Date(Date.now() - 30 * 86400_000).toISOString() },
];

const DIRECTORY_ROLE_MAPPINGS = [
  { id: uuid(), directory_group_id: DIRECTORY_GROUPS[0].id, role_code: "FACILITY_ADMIN", scope_type: "TENANT", scope_id: null, priority: 100, is_active: true },
];

function directoryGroupDto(g) {
  return { ...g, role_mapping_count: DIRECTORY_ROLE_MAPPINGS.filter((m) => m.directory_group_id === g.id).length };
}
function directoryMappingDto(m) {
  const role = ROLES.find((r) => r.code === m.role_code);
  const group = DIRECTORY_GROUPS.find((g) => g.id === m.directory_group_id);
  return {
    id: m.id, directory_group_id: m.directory_group_id, directory_group_name: group?.name ?? null, claim_value: null,
    role_code: m.role_code, role_name: role?.name ?? m.role_code, scope_type: m.scope_type, scope_id: m.scope_id ?? null,
    scope_label: m.scope_type === "TENANT" ? "Tenant-wide" : m.scope_id, priority: m.priority ?? 100, is_active: m.is_active !== false,
  };
}

const ATTACHMENTS = [];
function attachmentDto(a) {
  return { id: a.id, purpose: a.purpose, file_name: a.file_name, mime_type: a.mime_type, size_bytes: a.size_bytes, download_url: `http://localhost:${PORT}/api/v1/_mock-upload/${a.id}`, created_at: a.created_at };
}

const NOTIFICATION_TEMPLATES = [
  { id: uuid(), is_platform: true, code: "WORK_ORDER_ASSIGNED", channel: "EMAIL", locale: "zh-TW", subject_template: "新工單指派：{{wo_no}}", body_template: "您被指派了工單 {{wo_no}}：{{title}}。", is_active: true, placeholders: ["wo_no", "title"], is_overridden: false },
  { id: uuid(), is_platform: true, code: "RESERVATION_CONFIRMED", channel: "EMAIL", locale: "zh-TW", subject_template: "預約已確認：{{reservation_no}}", body_template: "您的預約 {{reservation_no}} 已確認。", is_active: true, placeholders: ["reservation_no"], is_overridden: false },
  { id: uuid(), is_platform: true, code: "ALARM_RAISED", channel: "IN_APP", locale: "zh-TW", subject_template: null, body_template: "{{message}}", is_active: true, placeholders: ["message"], is_overridden: false },
];

const WEBHOOKS = [];

const SKILLS = [
  { id: uuid(), tenant_id: null, code: "ELEV_CERT", name: "電梯操作證照", domain: "SAFETY", requires_certification: true, reminder_days_before: 60 },
  { id: uuid(), tenant_id: null, code: "HVAC_BASIC", name: "空調基礎維修", domain: "MEP", requires_certification: false, reminder_days_before: 30 },
];
const USER_SKILLS = new Map(); // user_id -> [{ skill_id, level, certified_at, expires_at, certificate_no }]

function skillStatus(skill, expiresAt) {
  if (!expiresAt) return skill.requires_certification ? "EXPIRED" : "NOT_APPLICABLE";
  const days = Math.floor((new Date(expiresAt) - new Date()) / 86400000);
  if (days < 0) return "EXPIRED";
  if (days <= 30) return "EXPIRING";
  return "VALID";
}

function userSkillDto(record) {
  const skill = SKILLS.find((s) => s.id === record.skill_id);
  return {
    skill_id: record.skill_id,
    skill_code: skill?.code,
    skill_name: skill?.name,
    requires_certification: skill?.requires_certification ?? false,
    level: record.level,
    certified_at: record.certified_at ?? null,
    expires_at: record.expires_at ?? null,
    certificate_no: record.certificate_no ?? null,
    days_until_expiry: record.expires_at ? Math.floor((new Date(record.expires_at) - new Date()) / 86400000) : null,
    status: skillStatus(skill ?? {}, record.expires_at),
  };
}

let serviceItemCounter = 0;
function makeServiceItem(overrides) {
  serviceItemCounter += 1;
  return {
    id: uuid(), facility_id: HQ.id, description: null, lead_time_minutes: 0, default_duration_minutes: 30,
    relative_offset_minutes: 0, is_attachable_to_reservation: true, is_standalone_requestable: true,
    requires_approval: false, chargeable: false, unit_price: null, currency: null, unit_label: null,
    max_quantity: null, form_schema: {}, display_order: serviceItemCounter, icon: null, ...overrides,
  };
}
const SERVICE_ITEMS = [
  makeServiceItem({ category: "ROOM_SETUP", code: "SVC-ROOMSETUP", name: "會議室場地佈置", description: "會議前佈置桌椅與視訊設備。", default_duration_minutes: 20 }),
  makeServiceItem({ category: "CATERING", code: "SVC-CATERING", name: "會議茶水／餐點", description: "會議餐點與茶水服務。", chargeable: true, unit_price: 150, currency: "TWD", unit_label: "每人", requires_approval: true }),
  makeServiceItem({ category: "IT_SUPPORT", code: "SVC-AV", name: "視訊設備支援", description: "派 IT 人員協助視訊會議設備。", is_standalone_requestable: false, facility_id: CINEMA.id }),
];

// -------------------------------------------------------- Phase 4 seed data ----
function rand(min, max) {
  return Math.random() * (max - min) + min;
}

function slaRow(label) {
  const responseTotal = Math.floor(rand(8, 40));
  const responseBreached = Math.floor(rand(0, responseTotal * 0.15));
  const resolutionTotal = Math.floor(rand(6, responseTotal));
  const resolutionBreached = Math.floor(rand(0, resolutionTotal * 0.2));
  return {
    group_key: label, group_label: label,
    response_total: responseTotal, response_met: responseTotal - responseBreached, response_breached: responseBreached,
    response_compliance_pct: responseTotal ? Math.round(((responseTotal - responseBreached) / responseTotal) * 1000) / 10 : null,
    avg_response_minutes: Math.round(rand(8, 90)),
    resolution_total: resolutionTotal, resolution_met: resolutionTotal - resolutionBreached, resolution_breached: resolutionBreached,
    resolution_compliance_pct: resolutionTotal ? Math.round(((resolutionTotal - resolutionBreached) / resolutionTotal) * 1000) / 10 : null,
    avg_resolution_minutes: Math.round(rand(120, 1400)),
  };
}

function pmRow(label) {
  const scheduled = Math.floor(rand(4, 20));
  const late = Math.floor(rand(0, scheduled * 0.2));
  const missed = Math.floor(rand(0, scheduled * 0.1));
  const onTime = scheduled - late - missed;
  return {
    group_key: label, group_label: label, scheduled_total: scheduled, completed_on_time: onTime, completed_late: late,
    missed, excluded_in_window: Math.floor(rand(0, 3)), excluded_skipped: Math.floor(rand(0, 2)), skip_reasons: {},
    avg_days_late: late ? Math.round(rand(1, 10) * 10) / 10 : null,
    on_time_rate: scheduled ? Math.round((onTime / scheduled) * 1000) / 1000 : null,
  };
}

const ORG_TYPES = ["GROUP", "COMPANY", "BUSINESS_UNIT", "REGION", "DEPARTMENT", "TEAM"];
const ORGS = [
  { org_id: "bbbbbbbb-0000-4000-8000-000000000000", parent_org_id: null, code: "GRP", org_name: "Bizlution 集團", org_type: "GROUP", org_path: "bizlution", depth: 0, cost_center: null, status: "ACTIVE", facility_count: 2 },
  { org_id: HQ.org_id, parent_org_id: "bbbbbbbb-0000-4000-8000-000000000000", code: "BU-OFFICE", org_name: "商辦事業群", org_type: "BUSINESS_UNIT", org_path: "bizlution.hq", depth: 1, cost_center: "CC-100", status: "ACTIVE", facility_count: 1 },
  { org_id: CINEMA.org_id, parent_org_id: "bbbbbbbb-0000-4000-8000-000000000000", code: "BU-CINEMA", org_name: "影城事業群", org_type: "BUSINESS_UNIT", org_path: "bizlution.cinema", depth: 1, cost_center: "CC-200", status: "ACTIVE", facility_count: 1 },
];
function orgDto(o) {
  return { id: o.org_id, parent_id: o.parent_org_id, code: o.code, name: o.org_name, org_type: o.org_type, org_path: o.org_path, depth: o.depth, cost_center: o.cost_center, facility_count: o.facility_count, status: o.status };
}

const REPORT_EXPORTS = new Map(); // id -> { ...ReportExport, queuedAt }

// ------------------------------------------------------------- http plumbing ----
function problem(status, code, detail, extra = {}) {
  return {
    type: `https://api.fms.bizlution.ai/problems/${code.toLowerCase().replaceAll("_", "-")}`,
    title: status === 401 ? "Unauthorized" : status === 403 ? "Forbidden" : status === 404 ? "Not Found" : status === 409 ? "Conflict" : status === 412 ? "Precondition Failed" : status === 422 ? "Validation error" : "Error",
    status,
    code,
    detail,
    request_id: uuid(),
    ...extra,
  };
}

function issueTokens(username) {
  const accessToken = uuid();
  const refreshToken = uuid();
  accessTokens.set(accessToken, username);
  refreshTokens.set(refreshToken, username);
  return {
    access_token: accessToken,
    token_type: "Bearer",
    expires_in: 3600,
    refresh_token: refreshToken,
    tenant_id: TENANT_ID,
    user_id: USERS[username].id,
    must_change_password: false,
  };
}

function currentUserPayload(username) {
  const u = USERS[username];
  return {
    user: { id: u.id, username, display_name: u.display_name, email: `${username}@demo.bizlution.ai`, user_type: "EMPLOYEE", status: "ACTIVE" },
    tenant: { id: TENANT_ID, code: TENANT_CODE, name: "Bizlution 示範集團", industry: "MIXED_USE", feature_flags: {} },
    accessible_facilities: u.facilities,
    roles: [{ role_code: u.role, scope_type: u.role === "TENANT_ADMIN" ? "TENANT" : "FACILITY", scope_id: u.role === "TENANT_ADMIN" ? null : u.facilities[0]?.id }],
    permissions: u.perms,
  };
}

const accessTokens = new Map();
const refreshTokens = new Map();

function paginate(list, { cursor, limit = 50 }) {
  const start = cursor ? Number(cursor) : 0;
  const lim = Math.min(Number(limit) || 50, 200);
  const page = list.slice(start, start + lim);
  const next = start + lim < list.length ? String(start + lim) : null;
  return { data: page, page: { next_cursor: next, limit: lim, total_estimate: list.length } };
}

function send(res, status, body, extraHeaders = {}) {
  const origin = extraHeaders.__origin ?? "*";
  res.writeHead(status, {
    "Content-Type": status === 204 ? "text/plain" : "application/json",
    "Access-Control-Allow-Origin": origin,
    "Access-Control-Allow-Headers": "Authorization, Content-Type, X-Tenant-ID, X-Facility-ID, X-Org-ID, X-Request-ID, If-Match, Idempotency-Key",
    "Access-Control-Allow-Methods": "GET,POST,PATCH,PUT,DELETE,OPTIONS",
    "Access-Control-Expose-Headers": "ETag, X-Request-ID",
    "X-Request-ID": extraHeaders["X-Request-ID"] ?? uuid(),
  });
  res.end(status === 204 || body === undefined ? undefined : JSON.stringify(body));
}

function readBody(req) {
  return new Promise((resolve) => {
    let data = "";
    req.on("data", (chunk) => (data += chunk));
    req.on("end", () => resolve(data ? JSON.parse(data) : {}));
  });
}

/** Minimal multipart/form-data parser — enough to pull out text fields and the one file part
 *  the attachments upload sends; doesn't need to persist bytes since the mock never serves them back. */
function readMultipart(req) {
  return new Promise((resolve) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const buf = Buffer.concat(chunks);
      const boundaryMatch = (req.headers["content-type"] || "").match(/boundary=(?:"([^"]+)"|([^;]+))/);
      const boundary = boundaryMatch ? boundaryMatch[1] || boundaryMatch[2] : null;
      const fields = {};
      let file = null;
      if (!boundary) return resolve({ fields, file });
      const boundaryBuf = Buffer.from(`--${boundary}`);
      const parts = [];
      let start = buf.indexOf(boundaryBuf);
      while (start !== -1) {
        const next = buf.indexOf(boundaryBuf, start + boundaryBuf.length);
        if (next === -1) break;
        parts.push(buf.slice(start + boundaryBuf.length, next));
        start = next;
      }
      for (const part of parts) {
        const headerEnd = part.indexOf("\r\n\r\n");
        if (headerEnd === -1) continue;
        const headerStr = part.slice(0, headerEnd).toString("utf8");
        let content = part.slice(headerEnd + 4);
        if (content.slice(-2).toString() === "\r\n") content = content.slice(0, -2);
        const nameMatch = headerStr.match(/name="([^"]+)"/);
        if (!nameMatch) continue;
        const filenameMatch = headerStr.match(/filename="([^"]*)"/);
        if (filenameMatch && filenameMatch[1]) {
          const typeMatch = headerStr.match(/Content-Type:\s*(.+)/i);
          file = { filename: filenameMatch[1], contentType: typeMatch ? typeMatch[1].trim() : "application/octet-stream", size: content.length };
        } else {
          fields[nameMatch[1]] = content.toString("utf8");
        }
      }
      resolve({ fields, file });
    });
  });
}

function requireAuth(req) {
  const authHeader = req.headers.authorization;
  const token = authHeader?.startsWith("Bearer ") ? authHeader.slice(7) : null;
  const username = token ? accessTokens.get(token) : null;
  if (!username) return { error: problem(401, "UNAUTHENTICATED", "Missing or invalid access token.") };
  const tenantHeader = req.headers["x-tenant-id"];
  if (!tenantHeader) return { error: problem(400, "BAD_REQUEST", "Missing X-Tenant-ID header.") };
  if (tenantHeader !== TENANT_ID) return { error: problem(403, "TENANT_MISMATCH", "X-Tenant-ID doesn't match the token's tenant.") };
  return { username };
}

function match(pattern, path) {
  const p = pattern.split("/").filter(Boolean);
  const a = path.split("/").filter(Boolean);
  if (p.length !== a.length) return null;
  const params = {};
  for (let i = 0; i < p.length; i++) {
    if (p[i].startsWith(":")) params[p[i].slice(1)] = decodeURIComponent(a[i]);
    else if (p[i] !== a[i]) return null;
  }
  return params;
}

const server = createServer(async (req, res) => {
  const origin = req.headers.origin ?? "*";
  const url = new URL(req.url, "http://localhost");
  const path = url.pathname.replace(/^\/api\/v1/, "");
  const q = url.searchParams;
  const reply = (status, body) => send(res, status, body, { __origin: origin });

  if (req.method === "OPTIONS") return reply(204, undefined);

  try {
    // ---- public auth ----
    if (path === "/auth/token" && req.method === "POST") {
      const body = await readBody(req);
      if (body.grant_type !== "password") return reply(501, problem(501, "NOT_IMPLEMENTED", `grant_type ${body.grant_type} isn't wired up in this mock`));
      const user = USERS[body.username];
      if (body.tenant_code !== TENANT_CODE) return reply(403, problem(403, "TENANT_MISMATCH", "Unknown tenant_code."));
      if (!user || body.password !== PASSWORD) return reply(401, problem(401, "UNAUTHENTICATED", "Invalid username or password."));
      return reply(200, issueTokens(body.username));
    }
    if (path === "/auth/token/refresh" && req.method === "POST") {
      const body = await readBody(req);
      const username = refreshTokens.get(body.refresh_token);
      if (!username) return reply(401, problem(401, "UNAUTHENTICATED", "Refresh token is invalid or expired."));
      refreshTokens.delete(body.refresh_token);
      return reply(200, issueTokens(username));
    }
    if (path === "/auth/logout" && req.method === "POST") {
      const body = await readBody(req);
      const already = !refreshTokens.has(body.refresh_token);
      refreshTokens.delete(body.refresh_token);
      return reply(200, { revoked: true, already_revoked: already, access_token_remains_valid_for_seconds: 3600 });
    }
    // 模擬直傳：真正的預簽網址本來就不帶 bearer token（見
    // fms-tenancy::bim::presign_upload 的說明），這裡跟著不查——放在
    // requireAuth 之後會讓前端「先取網址、再不帶 token 直傳」的正常流程在
    // mock 上 401。
    if (match("/_mock-upload/:key", path) && req.method === "PUT") return reply(200, { ok: true });

    // ---- everything below requires auth ----
    const auth = requireAuth(req);
    if (auth.error) return reply(auth.error.status, auth.error);
    const { username } = auth;

    if (path === "/auth/me" && req.method === "GET") return reply(200, currentUserPayload(username));

    if (path === "/asset-categories" && req.method === "GET") return reply(200, { data: CATEGORIES });

    if (path === "/asset-models" && req.method === "GET") {
      const wantActive = q.get("is_active") === null ? true : q.get("is_active") === "true";
      let list = ASSET_MODELS.filter((m) => m.is_active === wantActive);
      if (q.get("category_code")) list = list.filter((m) => m.category_code === q.get("category_code"));
      if (q.get("manufacturer")) list = list.filter((m) => m.manufacturer === q.get("manufacturer"));
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (path === "/asset-models" && req.method === "POST") {
      const body = await readBody(req);
      const category = CATEGORIES.find((c) => c.id === body.category_id);
      if (!category) return reply(422, problem(422, "VALIDATION_ERROR", "category_id 不存在。"));
      const model = {
        id: uuid(),
        is_platform: false,
        category_code: category.code,
        manufacturer: body.manufacturer,
        model_no: body.model_no,
        name: body.name,
        specifications: {},
        supported_protocols: body.supported_protocols ?? [],
        expected_life_months: body.expected_life_months ?? null,
        is_active: true,
      };
      ASSET_MODELS.push(model);
      return reply(201, model);
    }
    let params = match("/asset-models/:id", path);
    if (params && req.method === "PATCH") {
      const model = ASSET_MODELS.find((m) => m.id === params.id);
      if (!model) return reply(404, problem(404, "NOT_FOUND", "No such model."));
      const body = await readBody(req);
      Object.assign(model, body, { category_code: model.category_code, manufacturer: model.manufacturer, model_no: model.model_no });
      return reply(200, model);
    }
    if (params && req.method === "DELETE") {
      const model = ASSET_MODELS.find((m) => m.id === params.id);
      if (!model) return reply(404, problem(404, "NOT_FOUND", "No such model."));
      const inUse = ASSETS.filter((a) => a.asset_model_id === model.id).length;
      if (inUse > 0) return reply(409, problem(409, "CONFLICT", `${inUse} asset(s) still use this model — set is_active:false instead.`));
      ASSET_MODELS.splice(ASSET_MODELS.indexOf(model), 1);
      return reply(204, null);
    }

    if (path === "/organizations" && req.method === "GET") {
      return reply(200, paginate(ORGS.map(orgDto), { limit: q.get("limit") }));
    }
    if (path === "/organizations" && req.method === "POST") {
      const body = await readBody(req);
      if (!body.code || !body.name || !body.org_type) return reply(422, problem(422, "VALIDATION_ERROR", "code/name/org_type 為必填。"));
      if (!ORG_TYPES.includes(body.org_type)) return reply(422, problem(422, "VALIDATION_ERROR", `org_type 必須是 ${ORG_TYPES.join("/")} 之一。`));
      const parent = body.parent_id ? ORGS.find((o) => o.org_id === body.parent_id) : null;
      if (body.parent_id && !parent) return reply(422, problem(422, "VALIDATION_ERROR", "找不到上層組織。"));
      const org = {
        org_id: uuid(),
        parent_org_id: parent?.org_id ?? null,
        code: body.code,
        org_name: body.name,
        org_type: body.org_type,
        org_path: parent ? `${parent.org_path}.${body.code.toLowerCase()}` : body.code.toLowerCase(),
        depth: parent ? parent.depth + 1 : 0,
        cost_center: body.cost_center ?? null,
        status: "ACTIVE",
        facility_count: 0,
      };
      ORGS.push(org);
      return reply(201, orgDto(org));
    }
    params = match("/organizations/:id", path);
    if (params && req.method === "GET") {
      const org = ORGS.find((o) => o.org_id === params.id);
      if (!org) return reply(404, problem(404, "NOT_FOUND", "No such organization."));
      return reply(200, orgDto(org));
    }
    if (params && req.method === "PATCH") {
      const org = ORGS.find((o) => o.org_id === params.id);
      if (!org) return reply(404, problem(404, "NOT_FOUND", "No such organization."));
      const body = await readBody(req);
      if ("org_type" in body && !ORG_TYPES.includes(body.org_type)) return reply(422, problem(422, "VALIDATION_ERROR", `org_type 必須是 ${ORG_TYPES.join("/")} 之一。`));
      if ("code" in body) org.code = body.code;
      if ("name" in body) org.org_name = body.name;
      if ("org_type" in body) org.org_type = body.org_type;
      if ("cost_center" in body) org.cost_center = body.cost_center;
      if ("status" in body) org.status = body.status;
      return reply(200, orgDto(org));
    }
    if (params && req.method === "DELETE") {
      const org = ORGS.find((o) => o.org_id === params.id);
      if (!org) return reply(404, problem(404, "NOT_FOUND", "No such organization."));
      const children = ORGS.filter((o) => o.parent_org_id === org.org_id).length;
      const facilities = FACILITIES.filter((f) => f.org_id === org.org_id).length;
      if (children > 0 || facilities > 0) {
        return reply(409, problem(409, "CONFLICT", `Blocked by ${children} child organization(s) and ${facilities} facility(ies).`));
      }
      ORGS.splice(ORGS.indexOf(org), 1);
      const usersStillReferencing = MOCK_USERS.filter((u) => u.primary_org_id === org.org_id).length;
      return reply(200, { data: { id: org.org_id, deleted: true }, meta: { soft_delete: true, users_still_referencing: usersStillReferencing } });
    }

    if (path === "/facilities" && req.method === "GET") {
      return reply(200, paginate(FACILITIES, { limit: q.get("limit") }));
    }
    if (path === "/facilities" && req.method === "POST") {
      const body = await readBody(req);
      if (!body.org_id || !body.code || !body.name) return reply(422, problem(422, "VALIDATION_ERROR", "org_id/code/name 為必填。"));
      const facility = {
        id: uuid(),
        code: body.code,
        name: body.name,
        org_id: body.org_id,
        facility_type: body.facility_type ?? "OFFICE",
        city: body.city ?? null,
        status: "ACTIVE",
      };
      FACILITIES.push(facility);
      return reply(201, facility);
    }
    params = match("/facilities/:id", path);
    if (params && req.method === "PATCH") {
      const facility = FACILITIES.find((f) => f.id === params.id);
      if (!facility) return reply(404, problem(404, "NOT_FOUND", "No such facility."));
      const body = await readBody(req);
      Object.assign(facility, body, { id: facility.id, org_id: facility.org_id, code: facility.code });
      return reply(200, facility);
    }
    if (params && req.method === "DELETE") {
      const facility = FACILITIES.find((f) => f.id === params.id);
      if (!facility) return reply(404, problem(404, "NOT_FOUND", "No such facility."));
      // mock 沒有空間節點/工單/裝置的關聯資料可查，跟 resource-blackout 一樣直接放行。
      FACILITIES.splice(FACILITIES.indexOf(facility), 1);
      return reply(200, { data: { id: facility.id, deleted: true }, meta: { soft_delete: true } });
    }

    if (path === "/assets" && req.method === "GET") {
      let list = ASSETS;
      if (q.get("facility_id")) list = list.filter((a) => a.facility_id === q.get("facility_id"));
      if (q.get("status")) list = list.filter((a) => a.status === q.get("status"));
      if (q.get("category_code")) list = list.filter((a) => a.category_code === q.get("category_code"));
      if (q.get("q")) {
        const needle = q.get("q").toLowerCase();
        list = list.filter((a) => a.asset_code.toLowerCase().includes(needle) || a.name.toLowerCase().includes(needle));
      }
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (path === "/assets" && req.method === "POST") {
      const body = await readBody(req);
      const asset = makeAsset({ ...body });
      ASSETS.unshift(asset);
      return reply(201, asset);
    }
    params = match("/assets/:id", path);
    if (params && req.method === "GET") {
      const asset = ASSETS.find((a) => a.id === params.id);
      if (!asset) return reply(404, problem(404, "NOT_FOUND", "No such asset."));
      const include = (q.get("include") ?? "").split(",");
      const detail = { ...asset };
      if (include.includes("open_work_orders")) detail.open_work_orders = WORK_ORDERS.filter((w) => w.asset?.id === asset.id && w.status_category !== "TERMINAL");
      if (include.includes("meters")) detail.meters = asset.category_code.startsWith("HVAC") ? [{ meter_code: "RUNTIME", name: "運轉時數", unit: "hr", last_value: 12480, last_read_at: "2026-08-05T08:00:00Z" }] : [];
      if (include.includes("relations")) detail.relations = asset.asset_code === "AHU-01" ? [{ relation_type: "POWERS", direction: "upstream", asset: assetRef(ASSETS[2]), impact_level: "HIGH" }] : [];
      if (include.includes("children")) detail.children = [];
      return reply(200, detail);
    }

    if (path === "/assets:bulk-import" && req.method === "POST") {
      const body = await readBody(req);
      const dryRun = !!body.dry_run;
      const rows = body.rows ?? [];
      const seenCodes = new Set();
      const results = rows.map((row, index) => {
        if (!row.asset_code || !row.name || !row.category_code) {
          return { index, asset_code: row.asset_code ?? null, outcome: "REJECTED", asset_id: null, error_code: "VALIDATION_ERROR", error: "asset_code, name and category_code are required." };
        }
        if (seenCodes.has(row.asset_code) || ASSETS.some((a) => a.asset_code === row.asset_code)) {
          return { index, asset_code: row.asset_code, outcome: "REJECTED", asset_id: null, error_code: "DUPLICATE_ASSET_CODE", error: `Asset code ${row.asset_code} already exists.` };
        }
        if (!CATEGORIES.some((c) => c.code === row.category_code)) {
          return { index, asset_code: row.asset_code, outcome: "REJECTED", asset_id: null, error_code: "UNKNOWN_CATEGORY", error: `Unknown category_code ${row.category_code}.` };
        }
        seenCodes.add(row.asset_code);
        if (dryRun) return { index, asset_code: row.asset_code, outcome: "WOULD_CREATE", asset_id: null, error_code: null, error: null };
        const asset = makeAsset({
          asset_code: row.asset_code,
          name: row.name,
          facility_id: row.facility_id ?? HQ.id,
          category_code: row.category_code,
          status: row.status || "ACTIVE",
          criticality: row.criticality || "MEDIUM",
        });
        ASSETS.unshift(asset);
        return { index, asset_code: row.asset_code, outcome: "CREATED", asset_id: asset.id, error_code: null, error: null };
      });
      const rejected = results.filter((r) => r.outcome === "REJECTED").length;
      return reply(200, { dry_run: dryRun, total: rows.length, accepted: rows.length - rejected, rejected, rows: results });
    }

    if (path === "/work-order-statuses" && req.method === "GET") return reply(200, { data: STATUS_DICT, meta: { terminal_source: "status_category", count: STATUS_DICT.length } });

    if (path === "/work-orders" && req.method === "GET") {
      let list = WORK_ORDERS;
      if (q.get("facility_id")) list = list.filter((w) => w.facility_id === q.get("facility_id"));
      if (q.get("status_category")) list = list.filter((w) => w.status_category === q.get("status_category"));
      if (q.get("priority")) list = list.filter((w) => w.priority === q.get("priority"));
      if (q.get("mine") === "true") list = list.filter((w) => w.assignee?.id === USERS[username].id || w.requester?.id === USERS[username].id);
      const stripped = list.map(({ tasks, comments, transitions, ...rest }) => rest);
      return reply(200, paginate(stripped, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (path === "/work-orders" && req.method === "POST") {
      const body = await readBody(req);
      const asset = body.asset_id ? ASSETS.find((a) => a.id === body.asset_id) : null;
      const wo = makeWorkOrder({
        title: body.title,
        description: body.description ?? null,
        work_order_type: body.work_order_type,
        priority: body.priority,
        facility_id: body.facility_id,
        asset: assetRef(asset),
        requester: { id: USERS[username].id, display_name: USERS[username].display_name },
        status: "NEW",
      });
      WORK_ORDERS.unshift(wo);
      if (asset) asset.open_work_order_count = (asset.open_work_order_count ?? 0) + 1;
      const { tasks, comments, transitions, ...rest } = wo;
      return reply(201, rest);
    }
    params = match("/work-orders/:id", path);
    if (params && req.method === "GET") {
      const wo = WORK_ORDERS.find((w) => w.id === params.id);
      if (!wo) return reply(404, problem(404, "NOT_FOUND", "No such work order."));
      return reply(200, wo);
    }
    if (params && req.method === "PATCH") {
      const wo = WORK_ORDERS.find((w) => w.id === params.id);
      if (!wo) return reply(404, problem(404, "NOT_FOUND", "No such work order."));
      const ifMatch = req.headers["if-match"];
      if (!ifMatch) return reply(428, problem(428, "PRECONDITION_REQUIRED", "Missing If-Match header."));
      if (Number(ifMatch) !== wo.version) return reply(412, problem(412, "STALE_VERSION", `Current version is ${wo.version}.`));
      const body = await readBody(req);
      for (const field of ["title", "description", "priority", "team_id", "scheduled_start_at", "scheduled_end_at", "payload", "is_chargeback", "chargeback_org_id"]) {
        if (field in body) wo[field] = body[field];
      }
      wo.version += 1;
      wo.updated_at = new Date().toISOString();
      const { tasks, comments, transitions, ...rest } = wo;
      return reply(200, rest);
    }
    params = match("/work-orders/:id/available-actions", path);
    if (params && req.method === "GET") {
      const wo = WORK_ORDERS.find((w) => w.id === params.id);
      if (!wo) return reply(404, problem(404, "NOT_FOUND", "No such work order."));
      const actions = (TRANSITIONS[wo.status] ?? []).map((t) => ({ ...t, required_fields: [], permitted: true }));
      return reply(200, { data: actions });
    }
    params = match("/work-orders/:id/tasks/:taskId", path);
    if (params && req.method === "PATCH") {
      const wo = WORK_ORDERS.find((w) => w.id === params.id);
      if (!wo) return reply(404, problem(404, "NOT_FOUND", "No such work order."));
      const task = wo.tasks.find((tk) => tk.id === params.taskId);
      if (!task) return reply(404, problem(404, "NOT_FOUND", "No such checklist task."));
      const body = await readBody(req);
      if ("result_value" in body) {
        task.result_value = body.result_value;
        task.completed_at = new Date().toISOString();
      }
      if ("is_pass" in body) task.is_pass = body.is_pass;
      return reply(200, task);
    }
    params = match("/work-orders/:id/transitions", path);
    if (params && req.method === "POST") {
      const wo = WORK_ORDERS.find((w) => w.id === params.id);
      if (!wo) return reply(404, problem(404, "NOT_FOUND", "No such work order."));
      const ifMatch = req.headers["if-match"];
      if (!ifMatch) return reply(428, problem(428, "PRECONDITION_REQUIRED", "Missing If-Match header."));
      if (Number(ifMatch) !== wo.version) return reply(412, problem(412, "STALE_VERSION", `Current version is ${wo.version}.`));
      const body = await readBody(req);
      const transition = (TRANSITIONS[wo.status] ?? []).find((t) => t.action === body.action);
      if (!transition) return reply(409, problem(409, "WORK_ORDER_ILLEGAL_TRANSITION", `Cannot ${body.action} a work order in status ${wo.status}.`));
      wo.transitions.push({ from_status: wo.status, action: body.action, to_status: transition.to_status, actor_name: USERS[username].display_name });
      wo.status = transition.to_status;
      wo.status_category = STATUS_DICT.find((s) => s.code === wo.status)?.category ?? wo.status_category;
      wo.version += 1;
      wo.updated_at = new Date().toISOString();
      if (transition.action === "ASSIGN" && body.assignee_id) wo.assignee = { id: body.assignee_id, display_name: "Assigned tech" };
      if (wo.status === "COMPLETED") wo.completed_at = new Date().toISOString();
      const { tasks, comments, transitions, ...rest } = wo;
      return reply(200, rest);
    }
    params = match("/work-orders/:id/comments", path);
    if (params && req.method === "POST") {
      const wo = WORK_ORDERS.find((w) => w.id === params.id);
      if (!wo) return reply(404, problem(404, "NOT_FOUND", "No such work order."));
      const body = await readBody(req);
      const c = { id: uuid(), author_name: USERS[username].display_name, visibility: body.visibility ?? "INTERNAL", body: body.body, created_at: new Date().toISOString() };
      wo.comments.push(c);
      return reply(201, c);
    }

    if (path === "/notifications" && req.method === "GET") {
      let list = NOTIFICATIONS.filter((n) => n.username === username);
      if (q.get("unread_only") === "true") list = list.filter((n) => !n.read_at);
      const unread_count = NOTIFICATIONS.filter((n) => n.username === username && !n.read_at).length;
      const data = list.slice(0, Number(q.get("limit")) || 50).map(({ username: _u, ...rest }) => rest);
      return reply(200, { data, meta: { unread_count } });
    }
    params = match("/notifications/:id/read", path);
    if (params && req.method === "POST") {
      const n = NOTIFICATIONS.find((n) => n.id === params.id && n.username === username);
      if (!n) return reply(404, problem(404, "NOT_FOUND", "No such notification."));
      n.read_at = new Date().toISOString();
      return reply(204, undefined);
    }

    params = match("/facilities/:id/availability", path);
    if (params && req.method === "GET") {
      const from = new Date(q.get("from"));
      const to = new Date(q.get("to"));
      const resources = RESOURCES.filter((r) => r.facility_id === params.id);
      const data = resources.map((r) => {
        const busy = RESERVATIONS.filter((rv) => rv.resource_id === r.resource_id && rv.status !== "CANCELLED" && rv.status !== "REJECTED" && new Date(rv.start_at) < to && new Date(rv.end_at) > from).map((rv) => ({ start_at: rv.start_at, end_at: rv.end_at, kind: "RESERVATION", reason: null }));
        for (const [, hold] of HOLDS) {
          if (hold.resource_id === r.resource_id && new Date(hold.start_at) < to && new Date(hold.end_at) > from) {
            busy.push({ start_at: hold.start_at, end_at: hold.end_at, kind: "HOLD", reason: null });
          }
        }
        const free_slots = [];
        const dayStart = new Date(from);
        dayStart.setHours(9, 0, 0, 0);
        for (let h = 9; h < 18; h++) {
          const slotStart = new Date(dayStart);
          slotStart.setHours(h);
          const slotEnd = new Date(slotStart.getTime() + 60 * 60_000);
          const overlaps = busy.some((b) => new Date(b.start_at) < slotEnd && new Date(b.end_at) > slotStart);
          if (!overlaps && slotStart > new Date()) free_slots.push({ start_at: slotStart.toISOString(), end_at: slotEnd.toISOString() });
        }
        return { resource_id: r.resource_id, resource_type: r.resource_type, display_name: r.display_name, capacity: r.capacity, opening_hours: {}, rules: r.rules, busy, free_slots };
      });
      return reply(200, { data });
    }

    if (path === "/reservations/holds" && req.method === "POST") {
      const body = await readBody(req);
      const token = uuid();
      const expires_at = new Date(Date.now() + (body.ttl_seconds ?? 180) * 1000).toISOString();
      HOLDS.set(token, { resource_id: body.resource_id, start_at: body.start_at, end_at: body.end_at, expires_at });
      return reply(201, { hold_token: token, expires_at });
    }
    params = match("/reservations/holds/:token", path);
    if (params && req.method === "DELETE") {
      HOLDS.delete(params.token);
      return reply(204, undefined);
    }

    if (path === "/reservations" && req.method === "GET") {
      let list = RESERVATIONS;
      if (q.get("facility_id")) list = list.filter((r) => r.facility_id === q.get("facility_id"));
      if (q.get("mine") === "true") list = list.filter((r) => r.organizer?.id === USERS[username].id);
      const data = list.map((r) => maskPrivate(r, username));
      return reply(200, paginate(data, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (path === "/reservations" && req.method === "POST") {
      const body = await readBody(req);
      if (body.hold_token) HOLDS.delete(body.hold_token);
      const conflict = RESERVATIONS.find(
        (r) => r.resource_id === body.resource_id && r.status !== "CANCELLED" && r.status !== "REJECTED" && new Date(r.start_at) < new Date(body.end_at) && new Date(r.end_at) > new Date(body.start_at),
      );
      if (conflict) return reply(409, problem(409, "RESERVATION_CONFLICT", `That resource is already booked ${conflict.start_at} – ${conflict.end_at}.`, { conflicts: [{ reservation_id: conflict.id, start_at: conflict.start_at, end_at: conflict.end_at }] }));
      const resource = RESOURCES.find((r) => r.resource_id === body.resource_id);
      const occurrences = body.recurrence_rule ? expandRecurrenceRule(body.recurrence_rule, body.start_at, body.end_at) : [{ start_at: body.start_at, end_at: body.end_at }];
      const recurrenceGroupId = body.recurrence_rule && occurrences.length > 1 ? uuid() : null;
      const created = [];
      for (const occ of occurrences) {
        const clashes = RESERVATIONS.some(
          (r) => r.resource_id === body.resource_id && r.status !== "CANCELLED" && r.status !== "REJECTED" && new Date(r.start_at) < new Date(occ.end_at) && new Date(r.end_at) > new Date(occ.start_at),
        );
        if (clashes) continue;
        const rv = makeReservation({
          resource_id: body.resource_id,
          resource_name: resource?.display_name,
          facility_id: resource?.facility_id ?? HQ.id,
          title: body.title,
          purpose: body.purpose ?? null,
          party_size: body.party_size ?? 1,
          start_at: occ.start_at,
          end_at: occ.end_at,
          is_private: body.is_private ?? false,
          status: resource?.rules?.requires_approval ? "PENDING_APPROVAL" : "CONFIRMED",
          approval_required: !!resource?.rules?.requires_approval,
          organizer: { id: USERS[username].id, display_name: USERS[username].display_name },
          recurrence_group_id: recurrenceGroupId,
          services: (body.services ?? []).map((s) => ({ id: uuid(), service_item_id: s.service_item_id, service_name: "Add-on service", quantity: s.quantity ?? 1, payload: s.payload ?? {}, service_start_at: null, status: "REQUESTED", work_order: null })),
        });
        RESERVATIONS.unshift(rv);
        created.push(rv);
      }
      if (!created.length) return reply(409, problem(409, "RESERVATION_CONFLICT", "Every occurrence in that series conflicts with an existing reservation."));
      return reply(201, created[0]);
    }
    params = match("/reservations/:id", path);
    if (params && req.method === "GET") {
      const rv = RESERVATIONS.find((r) => r.id === params.id);
      if (!rv) return reply(404, problem(404, "NOT_FOUND", "No such reservation."));
      return reply(200, maskPrivate(rv, username));
    }
    if (params && req.method === "PATCH") {
      const rv = RESERVATIONS.find((r) => r.id === params.id);
      if (!rv) return reply(404, problem(404, "NOT_FOUND", "No such reservation."));
      const body = await readBody(req);
      for (const key of ["title", "purpose", "party_size", "start_at", "end_at", "is_private"]) {
        if (body[key] !== undefined) rv[key] = body[key];
      }
      return reply(200, maskPrivate(rv, username));
    }
    if (params && req.method === "DELETE") {
      const rv = RESERVATIONS.find((r) => r.id === params.id);
      if (!rv) return reply(404, problem(404, "NOT_FOUND", "No such reservation."));
      rv.status = "CANCELLED";
      return reply(204, undefined);
    }
    params = match("/reservations/:id/check-in", path);
    if (params && req.method === "POST") {
      const rv = RESERVATIONS.find((r) => r.id === params.id);
      if (!rv) return reply(404, problem(404, "NOT_FOUND", "No such reservation."));
      rv.checked_in_at = new Date().toISOString();
      return reply(200, maskPrivate(rv, username));
    }
    params = match("/reservations/:id/check-out", path);
    if (params && req.method === "POST") {
      const rv = RESERVATIONS.find((r) => r.id === params.id);
      if (!rv) return reply(404, problem(404, "NOT_FOUND", "No such reservation."));
      rv.status = "COMPLETED";
      rv.checked_out_at = new Date().toISOString();
      const bookedMinutes = Math.round((new Date(rv.end_at) - new Date(rv.start_at)) / 60000);
      const usedMinutes = rv.checked_in_at ? Math.round((new Date(rv.checked_out_at) - new Date(rv.checked_in_at)) / 60000) : bookedMinutes;
      return reply(200, {
        data: { reservation_id: rv.id, status: rv.status, checked_in_at: rv.checked_in_at, checked_out_at: rv.checked_out_at },
        meta: { used_minutes: usedMinutes, booked_minutes: bookedMinutes, slot_released: true, slot_released_by: "check-out" },
      });
    }
    params = match("/reservations/:id/approve", path);
    if (params && req.method === "POST") {
      const rv = RESERVATIONS.find((r) => r.id === params.id);
      if (!rv) return reply(404, problem(404, "NOT_FOUND", "No such reservation."));
      rv.status = "CONFIRMED";
      return reply(200, maskPrivate(rv, username));
    }
    params = match("/reservations/:id/reject", path);
    if (params && req.method === "POST") {
      const rv = RESERVATIONS.find((r) => r.id === params.id);
      if (!rv) return reply(404, problem(404, "NOT_FOUND", "No such reservation."));
      const body = await readBody(req);
      if (!body.reason) return reply(422, problem(422, "VALIDATION_ERROR", "A rejection reason is required."));
      rv.status = "REJECTED";
      rv.rejection_reason = body.reason;
      return reply(200, maskPrivate(rv, username));
    }

    params = match("/reservation-series/:recurrenceGroupId", path);
    if (params && req.method === "DELETE") {
      const members = RESERVATIONS.filter((r) => r.recurrence_group_id === params.recurrenceGroupId);
      if (!members.length) return reply(404, problem(404, "NOT_FOUND", "No such reservation series."));
      const now = new Date();
      let cancelled = 0, skippedPast = 0, skippedTerminal = 0;
      for (const rv of members) {
        if (["CANCELLED", "COMPLETED", "REJECTED"].includes(rv.status)) {
          skippedTerminal += 1;
        } else if (new Date(rv.start_at) < now) {
          skippedPast += 1;
        } else {
          rv.status = "CANCELLED";
          cancelled += 1;
        }
      }
      return reply(200, { data: { recurrence_group_id: params.recurrenceGroupId, cancelled, skipped_past: skippedPast, skipped_terminal: skippedTerminal, total_in_series: members.length } });
    }

    params = match("/facilities/:facilityId/bookable-resources", path);
    if (params && req.method === "GET") {
      let list = RESOURCES.filter((r) => r.facility_id === params.facilityId);
      if (q.get("include_unbookable") !== "true") list = list.filter((r) => r.is_bookable !== false);
      return reply(200, { data: list.map(toBookableResource), meta: { include_unbookable: q.get("include_unbookable") === "true", unbookable_count: RESOURCES.filter((r) => r.facility_id === params.facilityId && r.is_bookable === false).length } });
    }
    params = match("/bookable-resources/:resourceId", path);
    if (params && req.method === "PATCH") {
      const r = RESOURCES.find((res) => res.resource_id === params.resourceId);
      if (!r) return reply(404, problem(404, "NOT_FOUND", "No such bookable resource."));
      const body = await readBody(req);
      if (body.is_bookable !== undefined) r.is_bookable = body.is_bookable;
      if (body.capacity !== undefined) r.capacity = body.capacity;
      if (body.display_name !== undefined) r.display_name = body.display_name;
      for (const key of [
        "requires_approval", "requires_check_in", "min_duration_minutes", "max_duration_minutes", "slot_granularity_minutes",
        "buffer_before_minutes", "buffer_after_minutes", "advance_booking_days", "min_notice_minutes", "opening_hours",
        "attributes", "auto_release_minutes", "approver_role_code", "max_active_per_user",
      ]) {
        if (body[key] !== undefined) r.rules[key] = body[key];
      }
      return reply(200, toBookableResource(r));
    }

    if (path === "/resource-blackouts" && req.method === "GET") {
      let list = BLACKOUTS;
      if (q.get("facility_id")) list = list.filter((b) => b.facility_id === q.get("facility_id"));
      if (q.get("bookable_resource_id")) list = list.filter((b) => b.bookable_resource_id === q.get("bookable_resource_id"));
      const from = q.get("from") ? new Date(q.get("from")) : new Date();
      const to = q.get("to") ? new Date(q.get("to")) : null;
      list = list.filter((b) => new Date(b.end_at) > from && (!to || new Date(b.start_at) < to));
      return reply(200, { ...paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }), meta: { window: { from: from.toISOString(), to: to?.toISOString() ?? null }, window_default_applied: !q.get("from") } });
    }
    if (path === "/resource-blackouts" && req.method === "POST") {
      const body = await readBody(req);
      const resource = body.bookable_resource_id ? RESOURCES.find((r) => r.resource_id === body.bookable_resource_id) : null;
      const conflicts = RESERVATIONS.filter(
        (r) =>
          ["PENDING_APPROVAL", "CONFIRMED", "CHECKED_IN"].includes(r.status) &&
          (!body.bookable_resource_id || r.resource_id === body.bookable_resource_id) &&
          r.facility_id === body.facility_id &&
          new Date(r.start_at) < new Date(body.end_at) &&
          new Date(r.end_at) > new Date(body.start_at),
      );
      if (conflicts.length && !body.acknowledge_conflicting_reservations) {
        return reply(
          409,
          problem(409, "RESERVATION_CONFLICT", "This window overlaps existing reservations.", {
            errors: [{ message: JSON.stringify(conflicts.map((c) => ({ id: c.id, reservation_no: c.reservation_no, requested_by: c.organizer?.display_name }))) }],
          }),
        );
      }
      const blackout = {
        id: uuid(),
        facility_id: body.facility_id,
        bookable_resource_id: body.bookable_resource_id ?? null,
        resource_name: resource?.display_name ?? null,
        start_at: body.start_at,
        end_at: body.end_at,
        reason: body.reason,
        blackout_type: body.blackout_type ?? "MAINTENANCE",
        work_order_id: body.work_order_id ?? null,
        work_order_no: null,
        created_by: USERS[username].id,
        created_at: new Date().toISOString(),
      };
      BLACKOUTS.unshift(blackout);
      blackoutCounter += 1;
      return reply(201, { data: blackout, meta: { conflicting_reservations: conflicts, conflicting_reservation_count: conflicts.length } });
    }
    params = match("/resource-blackouts/:id", path);
    if (params && req.method === "DELETE") {
      const blackout = BLACKOUTS.find((b) => b.id === params.id);
      if (!blackout) return reply(404, problem(404, "NOT_FOUND", "No such blackout."));
      BLACKOUTS.splice(BLACKOUTS.indexOf(blackout), 1);
      return reply(204, null);
    }

    // ---- calendar federation ----
    params = match("/facilities/:id/calendar-integrations", path);
    if (params && req.method === "GET") {
      const list = CALENDAR_INTEGRATIONS.filter((c) => c.facility_id === params.id);
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (params && req.method === "POST") {
      const body = await readBody(req);
      const provider = (body.provider ?? "").toUpperCase();
      if (!["MS365", "GOOGLE"].includes(provider)) return reply(422, problem(422, "VALIDATION_ERROR", "provider 必須是 MS365 或 GOOGLE"));
      const msTenantId = body.ms_tenant_id?.trim() || null;
      const status = provider === "MS365" && msTenantId ? "ACTIVE" : "PENDING_CONSENT";
      const integration = {
        id: uuid(),
        tenant_id: TENANT_ID,
        facility_id: params.id,
        provider,
        status,
        ms_tenant_id: msTenantId,
        sync_cron: "*/5 * * * *",
        last_synced_at: null,
        last_sync_error: null,
        created_at: new Date().toISOString(),
      };
      CALENDAR_INTEGRATIONS.push(integration);
      const resp = { ...integration };
      if (provider === "MS365") resp.admin_consent_url = "https://login.microsoftonline.com/common/adminconsent?client_id=mock-client-id";
      return reply(201, resp);
    }
    params = match("/calendar-integrations/:id", path);
    if (params && req.method === "PATCH") {
      const integration = CALENDAR_INTEGRATIONS.find((c) => c.id === params.id);
      if (!integration) return reply(404, problem(404, "NOT_FOUND", "No such calendar integration."));
      const body = await readBody(req);
      if (body.status === "ACTIVE" && integration.provider === "MS365" && !integration.ms_tenant_id) {
        return reply(422, problem(422, "VALIDATION_ERROR", "狀態不能是 ACTIVE：這是 MS365 整合，但缺 ms_tenant_id"));
      }
      Object.assign(integration, body, { provider: integration.provider, ms_tenant_id: integration.ms_tenant_id });
      return reply(200, integration);
    }
    if (params && req.method === "DELETE") {
      const integration = CALENDAR_INTEGRATIONS.find((c) => c.id === params.id);
      if (!integration) return reply(404, problem(404, "NOT_FOUND", "No such calendar integration."));
      // mock 沒有建 calendar_sync_conflicts 表——沒有待審衝突可以擋，斷線直接連帶清掉對應。
      const mappingIds = CALENDAR_RESOURCE_MAPPINGS.filter((m) => m.calendar_integration_id === integration.id).map((m) => m.id);
      CALENDAR_INTEGRATIONS.splice(CALENDAR_INTEGRATIONS.indexOf(integration), 1);
      for (let i = CALENDAR_RESOURCE_MAPPINGS.length - 1; i >= 0; i--) {
        if (mappingIds.includes(CALENDAR_RESOURCE_MAPPINGS[i].id)) CALENDAR_RESOURCE_MAPPINGS.splice(i, 1);
      }
      return reply(204, null);
    }
    params = match("/calendar-integrations/:id/unresolved-resources", path);
    if (params && req.method === "GET") {
      const integration = CALENDAR_INTEGRATIONS.find((c) => c.id === params.id);
      if (!integration) return reply(404, problem(404, "NOT_FOUND", "No such calendar integration."));
      if (integration.status !== "ACTIVE") return reply(200, { data: [], meta: { reason: "整合尚未 ACTIVE，還沒有可以比對的外部資源清單" } });
      const mapped = CALENDAR_RESOURCE_MAPPINGS.filter((m) => m.calendar_integration_id === integration.id && m.status === "ACTIVE").map((m) => m.external_resource_id);
      const unresolved = CALENDAR_EXTERNAL_ROOMS.filter((r) => !mapped.includes(r.external_id));
      return reply(200, { data: unresolved });
    }
    params = match("/calendar-integrations/:id/resource-mappings", path);
    if (params && req.method === "GET") {
      const integration = CALENDAR_INTEGRATIONS.find((c) => c.id === params.id);
      if (!integration) return reply(404, problem(404, "NOT_FOUND", "No such calendar integration."));
      const list = CALENDAR_RESOURCE_MAPPINGS.filter((m) => m.calendar_integration_id === params.id);
      return reply(200, { data: list });
    }
    if (params && req.method === "POST") {
      const integration = CALENDAR_INTEGRATIONS.find((c) => c.id === params.id);
      if (!integration) return reply(404, problem(404, "NOT_FOUND", "No such calendar integration."));
      const body = await readBody(req);
      if (!Array.isArray(body.mappings) || body.mappings.length === 0) return reply(422, problem(422, "VALIDATION_ERROR", "mappings 不能是空陣列"));
      const created = [];
      for (const m of body.mappings) {
        const dup = CALENDAR_RESOURCE_MAPPINGS.find(
          (row) => row.calendar_integration_id === params.id && (row.external_resource_id === m.external_resource_id || (row.spatial_node_id === m.spatial_node_id && row.status === "ACTIVE")),
        );
        if (dup) return reply(409, problem(409, "CONFLICT", "這個外部資源或空間節點在這個整合裡已經有對應"));
        const node = SPATIAL_NODES.find((n) => n.id === m.spatial_node_id);
        const mapping = {
          id: uuid(),
          calendar_integration_id: params.id,
          spatial_node_id: m.spatial_node_id,
          node_name: node?.name ?? null,
          external_resource_id: m.external_resource_id,
          external_resource_name: m.external_resource_name ?? null,
          sync_direction: "BIDIRECTIONAL",
          status: "ACTIVE",
          created_at: new Date().toISOString(),
        };
        CALENDAR_RESOURCE_MAPPINGS.push(mapping);
        created.push(mapping.id);
      }
      return reply(200, { data: { created } });
    }
    params = match("/calendar-resource-mappings/:id", path);
    if (params && req.method === "PATCH") {
      const mapping = CALENDAR_RESOURCE_MAPPINGS.find((m) => m.id === params.id);
      if (!mapping) return reply(404, problem(404, "NOT_FOUND", "No such calendar resource mapping."));
      const body = await readBody(req);
      Object.assign(mapping, body, { spatial_node_id: mapping.spatial_node_id, external_resource_id: mapping.external_resource_id });
      return reply(200, mapping);
    }
    if (params && req.method === "DELETE") {
      const mapping = CALENDAR_RESOURCE_MAPPINGS.find((m) => m.id === params.id);
      if (!mapping) return reply(404, problem(404, "NOT_FOUND", "No such calendar resource mapping."));
      CALENDAR_RESOURCE_MAPPINGS.splice(CALENDAR_RESOURCE_MAPPINGS.indexOf(mapping), 1);
      return reply(204, null);
    }

    if (path === "/reports/facility-dashboard" && req.method === "GET") {
      const facilityId = q.get("facility_id");
      const facilityAssets = ASSETS.filter((a) => a.facility_id === facilityId);
      const facilityWos = WORK_ORDERS.filter((w) => w.facility_id === facilityId);
      const facilityReservations = RESERVATIONS.filter((r) => r.facility_id === facilityId);
      const open = facilityWos.filter((w) => w.status_category !== "TERMINAL");
      return reply(200, {
        facility: { id: facilityId, name: FACILITIES.find((f) => f.id === facilityId)?.name },
        work_orders: {
          open: open.length,
          overdue: open.filter((w) => w.sla_state?.includes("BREACHED")).length,
          completed_in_period: facilityWos.filter((w) => w.status === "COMPLETED").length,
          by_status: Object.fromEntries(STATUS_DICT.map((s) => [s.code, facilityWos.filter((w) => w.status === s.code).length])),
          by_source: { MANUAL: facilityWos.length },
          avg_resolution_minutes: 240,
        },
        sla: { compliance_pct: 92.5, at_risk: open.filter((w) => w.sla_state === "RESPONSE_DUE_SOON").length, breached: open.filter((w) => w.sla_state?.includes("BREACHED")).length },
        assets: {
          total: facilityAssets.length,
          down: facilityAssets.filter((a) => a.status === "DOWN").length,
          degraded: facilityAssets.filter((a) => a.status === "DEGRADED").length,
          warranty_expiring_90d: facilityAssets.filter((a) => a.warranty_end_date && new Date(a.warranty_end_date) < new Date(Date.now() + 90 * 86400_000)).length,
          avg_health_score: facilityAssets.length ? facilityAssets.reduce((s, a) => s + (a.health_score ?? 0), 0) / facilityAssets.length : null,
        },
        maintenance: { pm_due_30d: 3, pm_compliance_pct: 88.0, overdue_occurrences: 1 },
        alarms: { active: 2, critical: 1, unlinked_to_work_order: 1 },
        space: { bookable_resources: RESOURCES.filter((r) => r.facility_id === facilityId).length, utilization_pct: 46.5, no_show_pct: 4.2 },
        devices: { total: 12, offline: 1, stale_over_24h: 0 },
        _mock_reservation_count: facilityReservations.length,
      });
    }

    // ---- spatial / BIM ----
    if (path === "/spatial-node-types" && req.method === "GET") return reply(200, { data: NODE_TYPES, meta: { count: NODE_TYPES.length } });

    params = match("/facilities/:id/spatial-nodes", path);
    if (params && req.method === "GET") {
      let list = SPATIAL_NODES.filter((n) => n.facility_id === params.id);
      if (q.get("floor_level")) list = list.filter((n) => String(n.floor_level) === q.get("floor_level"));
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (params && req.method === "POST") {
      const body = await readBody(req);
      const node = makeSpatialNode({ facility_id: params.id, node_path: `.../${body.code}`, floor_level: body.floor_level ?? null, floor_label: body.floor_label ?? null, ...body });
      SPATIAL_NODES.push(node);
      return reply(201, node);
    }

    params = match("/spatial-nodes/:id", path);
    if (params && req.method === "GET") {
      const node = SPATIAL_NODES.find((n) => n.id === params.id);
      if (!node) return reply(404, problem(404, "NOT_FOUND", "No such spatial node."));
      return reply(200, node);
    }
    if (params && req.method === "PATCH") {
      const node = SPATIAL_NODES.find((n) => n.id === params.id);
      if (!node) return reply(404, problem(404, "NOT_FOUND", "No such spatial node."));
      const body = await readBody(req);
      for (const field of ["code", "name", "node_type_code", "capacity", "is_bookable", "is_active", "parent_id", "floor_level", "floor_label", "area_sqm", "bim_element_id"]) {
        if (field in body) node[field] = body[field];
      }
      return reply(200, node);
    }
    if (params && req.method === "DELETE") {
      const node = SPATIAL_NODES.find((n) => n.id === params.id);
      if (!node) return reply(404, problem(404, "NOT_FOUND", "No such spatial node."));
      const children = SPATIAL_NODES.filter((n) => n.parent_id === node.id).length;
      const openWorkOrders = node.open_work_order_count ?? 0;
      if (children > 0 || openWorkOrders > 0 || node.is_bookable) {
        return reply(409, problem(409, "CONFLICT", `Blocked by ${children} child node(s), ${openWorkOrders} open work order(s), or an active bookable resource.`));
      }
      SPATIAL_NODES.splice(SPATIAL_NODES.indexOf(node), 1);
      return reply(200, { data: { id: node.id, deleted: true }, meta: { soft_delete: true, assets_still_referencing: node.asset_count ?? 0, maintenance_plans_still_referencing: 0 } });
    }

    // ---- attachments ----
    if (path === "/attachments" && req.method === "GET") {
      const list = ATTACHMENTS.filter((a) => a.entity_type === q.get("entity_type") && a.entity_id === q.get("entity_id"));
      return reply(200, { data: list.map(attachmentDto) });
    }
    if (path === "/attachments" && req.method === "POST") {
      const { fields, file } = await readMultipart(req);
      if (!fields.entity_type || !fields.entity_id || !file) return reply(422, problem(422, "VALIDATION_ERROR", "entity_type/entity_id/file 為必填。"));
      const att = {
        id: uuid(), entity_type: fields.entity_type, entity_id: fields.entity_id, purpose: fields.purpose || "GENERAL",
        file_name: file.filename, mime_type: file.contentType, size_bytes: file.size, created_at: new Date().toISOString(),
      };
      ATTACHMENTS.push(att);
      return reply(201, attachmentDto(att));
    }
    params = match("/attachments/:id", path);
    if (params && req.method === "GET") {
      const att = ATTACHMENTS.find((a) => a.id === params.id);
      if (!att) return reply(404, problem(404, "NOT_FOUND", "No such attachment."));
      return reply(200, attachmentDto(att));
    }
    if (params && req.method === "DELETE") {
      const att = ATTACHMENTS.find((a) => a.id === params.id);
      if (!att) return reply(404, problem(404, "NOT_FOUND", "No such attachment."));
      ATTACHMENTS.splice(ATTACHMENTS.indexOf(att), 1);
      return reply(204, undefined);
    }

    if (path === "/uploads/presign" && req.method === "POST") {
      const body = await readBody(req);
      const key = `bim/${uuid()}-${body.file_name}`;
      return reply(200, { upload_url: `http://localhost:${PORT}/api/v1/_mock-upload/${encodeURIComponent(key)}`, storage_key: key, content_type: body.content_type, expires_in_seconds: 300 });
    }

    params = match("/facilities/:id/bim-models", path);
    if (params && req.method === "GET") {
      const list = [...BIM_MODELS.values()].filter((m) => m.facility_id === params.id).map(publicBimModel);
      return reply(200, { data: list, page: { next_cursor: null, limit: 50, total_estimate: list.length } });
    }
    if (params && req.method === "POST") {
      const body = await readBody(req);
      const model = {
        id: uuid(),
        facility_id: params.id,
        name: body.name,
        source_format: body.source_format ?? "IFC",
        version_label: body.version_label ?? null,
        discipline: body.discipline ?? null,
        registeredAt: Date.now(),
        viewer_urn: null,
        parsed_at: null,
      };
      BIM_MODELS.set(model.id, model);
      return reply(202, publicBimModel(model));
    }
    params = match("/bim-models/:id", path);
    if (params && req.method === "GET") {
      const model = BIM_MODELS.get(params.id);
      if (!model) return reply(404, problem(404, "NOT_FOUND", "No such BIM model."));
      const view = publicBimModel(model);
      return reply(200, { data: view, meta: { status_explanation: statusExplanation(view.status), awaiting_parse: view.status !== "PARSED" && view.status !== "PARSE_FAILED" } });
    }
    if (params && req.method === "DELETE") {
      if (!BIM_MODELS.has(params.id)) return reply(404, problem(404, "NOT_FOUND", "No such BIM model."));
      BIM_MODELS.delete(params.id);
      return reply(204, null);
    }
    params = match("/bim-models/:id/reset", path);
    if (params && req.method === "POST") {
      const model = BIM_MODELS.get(params.id);
      if (!model) return reply(404, problem(404, "NOT_FOUND", "No such BIM model."));
      model.registeredAt = Date.now();
      model.unresolvedElements = undefined;
      model.mappedNodeCount = 0;
      model.mappedAssetCount = 0;
      return reply(202, publicBimModel(model));
    }
    params = match("/bim-models/:id/unresolved-elements", path);
    if (params && req.method === "GET") {
      const model = BIM_MODELS.get(params.id);
      if (!model) return reply(404, problem(404, "NOT_FOUND", "No such BIM model."));
      const elements = bimStatus(model) === "PARSED" ? ensureBimElements(model) : [];
      return reply(200, { data: elements });
    }
    params = match("/bim-models/:id/mappings", path);
    if (params && req.method === "POST") {
      const model = BIM_MODELS.get(params.id);
      if (!model) return reply(404, problem(404, "NOT_FOUND", "No such BIM model."));
      ensureBimElements(model);
      const body = await readBody(req);
      let applied = 0, rejected = 0;
      const results = (body.mappings ?? []).map((m) => {
        const idx = model.unresolvedElements.findIndex((el) => el.bim_element_id === m.bim_element_id);
        if (idx === -1) {
          rejected += 1;
          return { bim_element_id: m.bim_element_id, target_type: m.target_type, target_id: m.target_id, ok: false, error: "Element already resolved or not found." };
        }
        model.unresolvedElements.splice(idx, 1);
        if (m.target_type === "ASSET") model.mappedAssetCount += 1;
        else model.mappedNodeCount += 1;
        applied += 1;
        return { bim_element_id: m.bim_element_id, target_type: m.target_type, target_id: m.target_id, ok: true, error: null };
      });
      return reply(200, { data: results, meta: { applied, rejected, unresolved_count: model.unresolvedElements.length } });
    }

    params = match("/facilities/:id/floor-view", path);
    if (params && req.method === "GET") {
      let nodes = SPATIAL_NODES.filter((n) => n.facility_id === params.id);
      if (q.get("floor_level")) nodes = nodes.filter((n) => String(n.floor_level) === q.get("floor_level"));
      const data = nodes.map((n) => {
        const activeAlarmsForNode = ALARMS.filter((a) => a.location?.spatial_node_id === n.id && (a.status === "ACTIVE" || a.status === "ACKNOWLEDGED"));
        const worstRank = activeAlarmsForNode.length ? Math.max(...activeAlarmsForNode.map((a) => ({ INFO: 1, WARNING: 2, MINOR: 3, MAJOR: 4, CRITICAL: 5 })[a.severity] ?? 1)) : null;
        const now = new Date();
        const occ = n.is_bookable ? RESERVATIONS.find((r) => r.resource_id === n.id && r.status !== "CANCELLED" && r.status !== "REJECTED" && new Date(r.start_at) <= now && new Date(r.end_at) >= now) : null;
        return {
          id: n.id, parent_id: n.parent_id, code: n.code, name: n.name, node_type_code: n.node_type_code, node_path: n.node_path,
          depth: n.depth, floor_level: n.floor_level, floor_label: n.floor_label, area_sqm: n.area_sqm, capacity: n.capacity,
          is_bookable: n.is_bookable, geometry: n.geometry, bim_element_id: n.bim_element_id,
          asset_count: n.asset_count, open_work_orders: n.open_work_order_count, active_alarms: activeAlarmsForNode.length,
          worst_alarm_severity: activeAlarmsForNode.length ? activeAlarmsForNode.map((a) => a.severity).sort().slice(-1)[0] : null,
          worst_alarm_rank: worstRank,
          occupancy_state: n.is_bookable ? (occ ? "OCCUPIED" : "FREE") : null,
          occupancy_start_at: occ?.start_at ?? null,
          occupancy_end_at: occ?.end_at ?? null,
          device_count: DEVICES.filter((d) => d.spatial_node_id === n.id || (d.asset_id && ASSETS.find((a) => a.id === d.asset_id)?.spatial_node_path === n.node_path)).length,
          devices_offline_count: 0,
        };
      });
      const floors = [...new Set(SPATIAL_NODES.filter((n) => n.facility_id === params.id).map((n) => n.floor_level))].sort((a, b) => a - b);
      return reply(200, { data, meta: { node_count: data.length, floors, nodes_without_geometry: data.filter((n) => !n.geometry?.min).length, geometry_comes_from: "bim_parse_or_manual", alarm_severity_order: ["INFO", "WARNING", "MINOR", "MAJOR", "CRITICAL"] } });
    }

    // ---- 2.5D 樓層平面圖設備標點 ----
    params = match("/spatial-nodes/:id/floor-plan-markers", path);
    if (params && req.method === "GET") {
      const floorNode = SPATIAL_NODES.find((n) => n.id === params.id);
      if (!floorNode) return reply(404, problem(404, "NOT_FOUND", "No such spatial node."));
      if (floorNode.node_type_code !== "FLOOR") return reply(422, problem(422, "VALIDATION_ERROR", `${params.id} 不是樓層節點（node_type_code = ${floorNode.node_type_code}）`));
      const items = FLOOR_PLAN_MARKERS.filter((m) => m.floor_node_id === params.id).map(floorPlanMarkerDto);
      return reply(200, { items });
    }
    if (params && req.method === "POST") {
      const floorNode = SPATIAL_NODES.find((n) => n.id === params.id);
      if (!floorNode) return reply(404, problem(404, "NOT_FOUND", "No such spatial node."));
      if (floorNode.node_type_code !== "FLOOR") return reply(422, problem(422, "VALIDATION_ERROR", `${params.id} 不是樓層節點（node_type_code = ${floorNode.node_type_code}）`));
      const body = await readBody(req);
      if (!["ASSET", "DEVICE", "SPATIAL_NODE"].includes(body.entity_type)) return reply(422, problem(422, "VALIDATION_ERROR", "entity_type 必須是 ASSET／DEVICE／SPATIAL_NODE"));
      const entityExists =
        (body.entity_type === "ASSET" && ASSETS.some((a) => a.id === body.entity_id)) ||
        (body.entity_type === "DEVICE" && DEVICES.some((d) => d.id === body.entity_id)) ||
        (body.entity_type === "SPATIAL_NODE" && SPATIAL_NODES.some((n) => n.id === body.entity_id));
      if (!entityExists) return reply(422, problem(422, "VALIDATION_ERROR", `找不到 entity_id=${body.entity_id}`));
      if (typeof body.x_ratio !== "number" || body.x_ratio < 0 || body.x_ratio > 1) return reply(422, problem(422, "VALIDATION_ERROR", "x_ratio 必須在 0 到 1 之間"));
      if (typeof body.y_ratio !== "number" || body.y_ratio < 0 || body.y_ratio > 1) return reply(422, problem(422, "VALIDATION_ERROR", "y_ratio 必須在 0 到 1 之間"));
      const marker = { id: uuid(), floor_node_id: params.id, entity_type: body.entity_type, entity_id: body.entity_id, x_ratio: body.x_ratio, y_ratio: body.y_ratio, z_offset: body.z_offset ?? 0, created_at: new Date().toISOString() };
      FLOOR_PLAN_MARKERS.push(marker);
      return reply(201, floorPlanMarkerDto(marker));
    }
    params = match("/floor-plan-markers/:id", path);
    if (params && req.method === "DELETE") {
      const marker = FLOOR_PLAN_MARKERS.find((m) => m.id === params.id);
      if (!marker) return reply(404, problem(404, "NOT_FOUND", "No such floor-plan marker."));
      FLOOR_PLAN_MARKERS.splice(FLOOR_PLAN_MARKERS.indexOf(marker), 1);
      return reply(200, { deleted: marker.id });
    }

    // ---- maintenance ----
    if (path === "/maintenance-templates" && req.method === "GET") return reply(200, { data: MAINT_TEMPLATES });
    if (path === "/maintenance-templates" && req.method === "POST") {
      const body = await readBody(req);
      if (!Array.isArray(body.checklist) || body.checklist.length === 0) return reply(422, problem(422, "VALIDATION_ERROR", "checklist must not be empty."));
      const tpl = { id: uuid(), plan_count: 0, is_active: true, estimated_minutes: 60, ...body };
      MAINT_TEMPLATES.push(tpl);
      return reply(201, tpl);
    }
    params = match("/maintenance-templates/:id", path);
    if (params && req.method === "PATCH") {
      const tpl = MAINT_TEMPLATES.find((t) => t.id === params.id);
      if (!tpl) return reply(404, problem(404, "NOT_FOUND", "No such template."));
      const body = await readBody(req);
      if (body.checklist !== undefined && (!Array.isArray(body.checklist) || body.checklist.length === 0)) return reply(422, problem(422, "VALIDATION_ERROR", "checklist must not be empty."));
      Object.assign(tpl, body, { code: tpl.code });
      return reply(200, tpl);
    }
    if (params && req.method === "DELETE") {
      const tpl = MAINT_TEMPLATES.find((t) => t.id === params.id);
      if (!tpl) return reply(404, problem(404, "NOT_FOUND", "No such template."));
      if ((tpl.plan_count ?? 0) > 0) return reply(409, problem(409, "CONFLICT", `${tpl.plan_count} plan(s) still use this template — set is_active:false instead.`));
      MAINT_TEMPLATES.splice(MAINT_TEMPLATES.indexOf(tpl), 1);
      return reply(204, null);
    }

    if (path === "/maintenance-plans" && req.method === "GET") {
      let list = MAINTENANCE_PLANS;
      if (q.get("facility_id")) list = list.filter((p) => p.facility_id === q.get("facility_id"));
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (path === "/maintenance-plans" && req.method === "POST") {
      const body = await readBody(req);
      const template = MAINT_TEMPLATES.find((t) => t.id === body.template_id);
      const asset = body.asset_id ? ASSETS.find((a) => a.id === body.asset_id) : null;
      const plan = makeMaintenancePlan({
        ...body,
        template_name: template?.name,
        target: asset ? { type: "ASSET", id: asset.id, label: asset.name } : { type: "CATEGORY", label: body.category_code },
        next_due_at: new Date(Date.now() + 30 * 86400_000).toISOString(),
      });
      MAINTENANCE_PLANS.push(plan);
      if (template) template.plan_count = (template.plan_count ?? 0) + 1;
      return reply(201, plan);
    }
    params = match("/maintenance-plans/:id", path);
    if (params && req.method === "PATCH") {
      const plan = MAINTENANCE_PLANS.find((p) => p.id === params.id);
      if (!plan) return reply(404, problem(404, "NOT_FOUND", "No such plan."));
      const body = await readBody(req);
      Object.assign(plan, body, { id: plan.id, facility_id: plan.facility_id, template_id: plan.template_id, trigger_type: plan.trigger_type });
      return reply(200, { data: plan });
    }
    params = match("/maintenance-plans/:id/preview-schedule", path);
    if (params && req.method === "GET") {
      const plan = MAINTENANCE_PLANS.find((p) => p.id === params.id);
      if (!plan) return reply(404, problem(404, "NOT_FOUND", "No such plan."));
      const intervalDays = plan.rrule?.includes("YEARLY") ? 365 : plan.rrule?.includes("WEEKLY") ? 7 : 30;
      const base = plan.next_due_at ? new Date(plan.next_due_at) : new Date();
      const data = [0, 1, 2].map((i) => ({ scheduled_for: new Date(base.getTime() + i * intervalDays * 86400_000).toISOString(), asset_id: plan.asset_id, asset_code: ASSETS.find((a) => a.id === plan.asset_id)?.asset_code }));
      return reply(200, { data });
    }
    params = match("/maintenance-plans/:id/generate-now", path);
    if (params && req.method === "POST") {
      const plan = MAINTENANCE_PLANS.find((p) => p.id === params.id);
      if (!plan) return reply(404, problem(404, "NOT_FOUND", "No such plan."));
      if (!plan.is_active) return reply(409, problem(409, "CONFLICT", "This plan is disabled."));
      const asset = plan.asset_id ? ASSETS.find((a) => a.id === plan.asset_id) : null;
      const wo = makeWorkOrder({ title: `[PM] ${plan.name}`, work_order_type: "MAINTENANCE", priority: plan.priority, facility_id: plan.facility_id, asset: assetRef(asset), status: "NEW" });
      WORK_ORDERS.unshift(wo);
      const occurrence = makeOccurrence({ plan_id: plan.id, plan_code: plan.code, plan_name: plan.name, asset_id: plan.asset_id, asset_code: asset?.asset_code, scheduled_for: new Date().toISOString(), status: "GENERATED", work_order_id: wo.id, work_order_no: wo.wo_no, generated_at: new Date().toISOString(), facility_id: plan.facility_id });
      MAINTENANCE_OCCURRENCES.unshift(occurrence);
      plan.next_due_at = new Date(Date.now() + 30 * 86400_000).toISOString();
      return reply(200, { created: 1, work_order_ids: [wo.id], occurrence_ids: [occurrence.id], skipped: 0, scheduled_for: occurrence.scheduled_for });
    }

    if (path === "/maintenance-occurrences" && req.method === "GET") {
      let list = MAINTENANCE_OCCURRENCES;
      if (q.get("facility_id")) list = list.filter((o) => o.facility_id === q.get("facility_id"));
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    params = match("/maintenance-occurrences/:id/skip", path);
    if (params && req.method === "POST") {
      const occ = MAINTENANCE_OCCURRENCES.find((o) => o.id === params.id);
      if (!occ) return reply(404, problem(404, "NOT_FOUND", "No such occurrence."));
      if (occ.status === "COMPLETED" || occ.status === "SKIPPED") return reply(409, problem(409, "CONFLICT", "Already completed or skipped."));
      const body = await readBody(req);
      occ.status = "SKIPPED";
      occ.skip_reason = body.reason;
      return reply(200, occ);
    }

    // ---- IoT ----
    if (path === "/devices" && req.method === "GET") {
      let list = DEVICES;
      if (q.get("facility_id")) list = list.filter((d) => d.facility_id === q.get("facility_id"));
      if (q.get("offline_only") === "true") list = list.filter((d) => d.connectivity === "OFFLINE" || d.connectivity === "NEVER_SEEN");
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (path === "/devices" && req.method === "POST") {
      const body = await readBody(req);
      const facility = FACILITIES.find((f) => f.id === body.facility_id);
      const node = SPATIAL_NODES.find((n) => n.id === body.spatial_node_id);
      const device = {
        id: uuid(),
        facility_id: body.facility_id,
        facility_name: facility?.name ?? null,
        gateway_id: body.gateway_id ?? null,
        asset_id: body.asset_id ?? null,
        asset_code: null,
        spatial_node_id: body.spatial_node_id ?? null,
        location_name: node?.name ?? null,
        device_code: body.device_code,
        name: body.name,
        device_type: (body.device_type ?? "SENSOR").toUpperCase(),
        address: body.address ?? null,
        heartbeat_interval_seconds: body.heartbeat_interval_seconds ?? 300,
        offline_alarm_after_seconds: body.offline_alarm_after_seconds ?? 900,
        last_seen_at: null,
        status: "UNKNOWN",
        connectivity: "NEVER_SEEN",
        seconds_since_seen: null,
        point_count: 0,
        created_at: new Date().toISOString(),
      };
      DEVICES.push(device);
      return reply(201, device);
    }
    params = match("/devices/:id", path);
    if (params && req.method === "PATCH") {
      const device = DEVICES.find((d) => d.id === params.id);
      if (!device) return reply(404, problem(404, "NOT_FOUND", "No such device."));
      const body = await readBody(req);
      Object.assign(device, body, { id: device.id, device_code: device.device_code, facility_id: device.facility_id });
      return reply(200, device);
    }
    if (params && req.method === "DELETE") {
      const device = DEVICES.find((d) => d.id === params.id);
      if (!device) return reply(404, problem(404, "NOT_FOUND", "No such device."));
      const openAlarms = ALARMS.filter((a) => a.device_id === device.id && ["ACTIVE", "ACKNOWLEDGED"].includes(a.status)).length;
      if (openAlarms > 0) return reply(409, problem(409, "CONFLICT", `${openAlarms} open alarm(s) still reference this device — resolve them first.`));
      DEVICES.splice(DEVICES.indexOf(device), 1);
      return reply(200, { data: { id: device.id, deleted: true }, meta: { soft_delete: true } });
    }

    if (path === "/telemetry/latest" && req.method === "GET") {
      const facilityId = q.get("facility_id");
      const points = facilityId ? TELEMETRY_POINTS.filter((p) => p.facility_id === facilityId) : TELEMETRY_POINTS;
      const items = points.map((p) => ({
        telemetry_point_id: p.telemetry_point_id, point_code: p.point_code, point_name: p.point_name, unit: p.unit,
        device_id: p.device_id, device_code: p.device_code, facility_id: p.facility_id, asset_id: p.asset_id,
        observed_at: new Date().toISOString(), value_num: telemetryNow(p), value_bool: null, value_text: null,
        quality: "GOOD", age_seconds: 5, is_stale: false,
      }));
      return reply(200, { items, meta: { stale_count: 0 } });
    }
    if (path === "/telemetry/series" && req.method === "GET") {
      const point = TELEMETRY_POINTS.find((p) => p.device_id === q.get("device_id") && p.point_code === q.get("point_code"));
      if (!point) return reply(200, { items: [], meta: {} });
      const to = q.get("to") ? new Date(q.get("to")) : new Date();
      const from = q.get("from") ? new Date(q.get("from")) : new Date(to.getTime() - 24 * 3600_000);
      const bucketMs = 3600_000;
      const items = [];
      for (let t = from.getTime(); t <= to.getTime(); t += bucketMs) {
        const noise = Math.sin(t / 900_000) * point.amplitude;
        const avg = Math.round((point.base + noise) * 10) / 10;
        items.push({ telemetry_point_id: point.telemetry_point_id, bucket_start: new Date(t).toISOString(), sample_count: 12, avg_value: avg, min_value: avg - 0.5, max_value: avg + 0.8, last_observed_at: new Date(t + bucketMs).toISOString(), last_value_num: avg, has_suspect_quality: false });
      }
      return reply(200, { items, meta: { from: from.toISOString(), to: to.toISOString(), bucket_seconds: bucketMs / 1000, truncated: false } });
    }

    if (path === "/alarms" && req.method === "GET") {
      let list = ALARMS;
      if (q.get("facility_id")) list = list.filter((a) => a.facility_id === q.get("facility_id"));
      if (q.get("status")) list = list.filter((a) => a.status === q.get("status"));
      if (q.get("unlinked_only") === "true") list = list.filter((a) => !a.work_order);
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    params = match("/alarms/:id/acknowledge", path);
    if (params && req.method === "POST") {
      const alarm = ALARMS.find((a) => a.id === params.id);
      if (!alarm) return reply(404, problem(404, "NOT_FOUND", "No such alarm."));
      alarm.status = "ACKNOWLEDGED";
      alarm.acknowledged_at = new Date().toISOString();
      return reply(200, alarm);
    }
    params = match("/alarms/:id/suppress", path);
    if (params && req.method === "POST") {
      const alarm = ALARMS.find((a) => a.id === params.id);
      if (!alarm) return reply(404, problem(404, "NOT_FOUND", "No such alarm."));
      const body = await readBody(req);
      alarm.status = "SUPPRESSED";
      alarm.suppressed_until = new Date(Date.now() + (body.duration_minutes ?? 60) * 60_000).toISOString();
      return reply(200, { data: alarm, meta: { extended_existing_suppression: false, max_minutes_allowed: 1440, policy_source: "tenant_default" } });
    }
    params = match("/alarms/:id/work-order", path);
    if (params && req.method === "POST") {
      const alarm = ALARMS.find((a) => a.id === params.id);
      if (!alarm) return reply(404, problem(404, "NOT_FOUND", "No such alarm."));
      const wo = makeWorkOrder({ title: `[告警] ${alarm.message}`, work_order_type: "CORRECTIVE", priority: alarm.severity === "CRITICAL" ? "URGENT" : "HIGH", facility_id: alarm.facility_id, asset: alarm.asset, alarm_id: alarm.id, status: "NEW" });
      WORK_ORDERS.unshift(wo);
      alarm.work_order = { id: wo.id, wo_no: wo.wo_no, status: wo.status };
      const { tasks, comments, transitions, ...rest } = wo;
      return reply(201, rest);
    }

    if (path === "/alarm-rules" && req.method === "GET") {
      let list = ALARM_RULES;
      if (q.get("facility_id")) list = list.filter((r) => r.facility_id === q.get("facility_id"));
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (path === "/alarm-rules" && req.method === "POST") {
      const body = await readBody(req);
      const point = TELEMETRY_POINTS.find((p) => p.point_code === body.point_code);
      const rule = { id: uuid(), covered_point_count: point ? 1 : 0, evaluable: !!point, created_at: new Date().toISOString(), debounce_seconds: 60, auto_clear: true, is_active: true, dedupe_window_minutes: 120, notify_role_codes: [], facility_name: FACILITIES.find((f) => f.id === body.facility_id)?.name ?? null, ...body };
      ALARM_RULES.push(rule);
      return reply(201, rule);
    }
    params = match("/alarm-rules/:id", path);
    if (params && req.method === "PATCH") {
      const rule = ALARM_RULES.find((r) => r.id === params.id);
      if (!rule) return reply(404, problem(404, "NOT_FOUND", "No such alarm rule."));
      const body = await readBody(req);
      Object.assign(rule, body, { code: rule.code, facility_id: rule.facility_id });
      return reply(200, rule);
    }
    if (params && req.method === "DELETE") {
      const rule = ALARM_RULES.find((r) => r.id === params.id);
      if (!rule) return reply(404, problem(404, "NOT_FOUND", "No such alarm rule."));
      const openAlarms = ALARMS.filter((a) => a.rule_code === rule.code && ["ACTIVE", "ACKNOWLEDGED"].includes(a.status)).length;
      if (openAlarms > 0) return reply(409, problem(409, "CONFLICT", `${openAlarms} open alarm(s) still reference this rule — set is_active:false instead.`));
      ALARM_RULES.splice(ALARM_RULES.indexOf(rule), 1);
      return reply(204, null);
    }
    params = match("/alarm-rules/:id/test", path);
    if (params && req.method === "POST") {
      const rule = ALARM_RULES.find((r) => r.id === params.id);
      if (!rule) return reply(404, problem(404, "NOT_FOUND", "No such alarm rule."));
      return reply(200, { data: { would_have_fired: rule.evaluable ? Math.floor(Math.random() * 5) : 0, sample_triggers: [] } });
    }

    // ---- service catalogue ----
    params = match("/facilities/:id/service-items", path);
    if (params && req.method === "GET") {
      const list = SERVICE_ITEMS.filter((s) => s.facility_id === params.id);
      return reply(200, paginate(list, { cursor: q.get("cursor"), limit: q.get("limit") }));
    }
    if (params && req.method === "POST") {
      const body = await readBody(req);
      const item = makeServiceItem({ facility_id: params.id, ...body });
      SERVICE_ITEMS.push(item);
      return reply(201, item);
    }
    params = match("/service-items/:id", path);
    if (params && req.method === "PATCH") {
      const item = SERVICE_ITEMS.find((s) => s.id === params.id);
      if (!item) return reply(404, problem(404, "NOT_FOUND", "No such service item."));
      Object.assign(item, await readBody(req));
      return reply(200, item);
    }
    if (params && req.method === "DELETE") {
      const item = SERVICE_ITEMS.find((s) => s.id === params.id);
      if (!item) return reply(404, problem(404, "NOT_FOUND", "No such service item."));
      const openWorkOrders = WORK_ORDERS.filter((w) => w.service_item_id === item.id && w.status_category !== "TERMINAL").length;
      SERVICE_ITEMS.splice(SERVICE_ITEMS.indexOf(item), 1);
      return reply(200, { data: { id: item.id, deleted: true, open_work_orders: openWorkOrders }, meta: { soft_delete: true } });
    }

    // ---- admin: users & roles ----
    if (path === "/users" && req.method === "GET") return reply(200, paginate(MOCK_USERS, { cursor: q.get("cursor"), limit: q.get("limit") }));
    if (path === "/users" && req.method === "POST") {
      const body = await readBody(req);
      const user = { id: uuid(), employee_no: null, phone: null, primary_org_id: null, default_facility_id: null, status: "INVITED", identity_sources: [], skills: [], last_login_at: null, ...body };
      MOCK_USERS.push(user);
      ROLE_ASSIGNMENTS.set(user.id, []);
      return reply(201, user);
    }
    params = match("/users/:id/suspend", path);
    if (params && req.method === "POST") {
      const user = MOCK_USERS.find((u) => u.id === params.id);
      if (!user) return reply(404, problem(404, "NOT_FOUND", "No such user."));
      const body = await readBody(req);
      user.status = body.status ?? "SUSPENDED";
      return reply(200, user);
    }
    params = match("/users/:id/role-assignments", path);
    if (params && req.method === "GET") return reply(200, { items: ROLE_ASSIGNMENTS.get(params.id) ?? [] });
    if (params && req.method === "POST") {
      const body = await readBody(req);
      const role = ROLES.find((r) => r.code === body.role_code);
      const facility = FACILITIES.find((f) => f.id === body.scope_id);
      const assignment = { id: uuid(), user_id: params.id, role_code: body.role_code, role_name: role?.name ?? body.role_code, scope_type: body.scope_type, scope_id: body.scope_id ?? null, scope_label: facility?.name ?? (body.scope_type === "TENANT" ? "Tenant-wide" : null), source: "MANUAL", valid_until: body.valid_until ?? null };
      const list = ROLE_ASSIGNMENTS.get(params.id) ?? [];
      list.push(assignment);
      ROLE_ASSIGNMENTS.set(params.id, list);
      return reply(201, assignment);
    }
    params = match("/role-assignments/:id", path);
    if (params && req.method === "DELETE") {
      for (const [userId, list] of ROLE_ASSIGNMENTS) {
        const idx = list.findIndex((a) => a.id === params.id);
        if (idx !== -1) {
          list.splice(idx, 1);
          ROLE_ASSIGNMENTS.set(userId, list);
          return reply(204, undefined);
        }
      }
      return reply(404, problem(404, "NOT_FOUND", "No such role assignment."));
    }

    if (path === "/roles" && req.method === "GET") return reply(200, { items: ROLES });
    if (path === "/roles" && req.method === "POST") {
      const body = await readBody(req);
      if (ROLES.some((r) => r.code === body.code)) return reply(409, problem(409, "CONFLICT", "A role with this code already exists."));
      const role = { id: uuid(), tenant_id: TENANT_ID, is_system: false, is_assignable: true, ...body };
      ROLES.push(role);
      return reply(201, role);
    }
    params = match("/roles/:id", path);
    if (params && req.method === "PATCH") {
      const role = ROLES.find((r) => r.id === params.id);
      if (!role) return reply(404, problem(404, "NOT_FOUND", "No such role."));
      if (role.is_system) return reply(409, problem(409, "CONFLICT", "系統角色不可修改"));
      const body = await readBody(req);
      Object.assign(role, body, { id: role.id, code: role.code, tenant_id: role.tenant_id, is_system: role.is_system });
      return reply(200, role);
    }
    if (params && req.method === "DELETE") {
      const role = ROLES.find((r) => r.id === params.id);
      if (!role) return reply(404, problem(404, "NOT_FOUND", "No such role."));
      if (role.is_system) return reply(409, problem(409, "CONFLICT", "系統角色不可刪除"));
      const holders = [...ROLE_ASSIGNMENTS.values()].flat().filter((a) => a.role_code === role.code).length;
      if (holders > 0) return reply(409, problem(409, "CONFLICT", `還有 ${holders} 個使用者持有這個角色 —— 先撤銷指派。`));
      ROLES.splice(ROLES.indexOf(role), 1);
      return reply(204, null);
    }

    if (path === "/permissions" && req.method === "GET") return reply(200, { items: PERMISSIONS });

    if (path === "/audit-log" && req.method === "GET") return reply(200, paginate(AUDIT_LOG, { cursor: q.get("cursor"), limit: q.get("limit") }));

    // ---- admin: identity providers ----
    if (path === "/identity-providers" && req.method === "GET") return reply(200, paginate(IDENTITY_PROVIDERS, { cursor: q.get("cursor"), limit: q.get("limit") }));
    params = match("/identity-providers/:id/test-connection", path);
    if (params && req.method === "POST") {
      const idp = IDENTITY_PROVIDERS.find((i) => i.id === params.id);
      if (!idp) return reply(404, problem(404, "NOT_FOUND", "No such identity provider."));
      return reply(200, { ok: true, detail: idp.provider_type === "LOCAL" ? "Local auth doesn't need a connection test." : "OIDC discovery document resolved successfully." });
    }
    params = match("/identity-providers/:id/sync", path);
    if (params && req.method === "POST") {
      const idp = IDENTITY_PROVIDERS.find((i) => i.id === params.id);
      if (!idp) return reply(404, problem(404, "NOT_FOUND", "No such identity provider."));
      idp.last_sync_at = new Date().toISOString();
      return reply(202, { status: "queued" });
    }
    params = match("/identity-providers/:id", path);
    if (params && req.method === "DELETE") {
      const idp = IDENTITY_PROVIDERS.find((i) => i.id === params.id);
      if (!idp) return reply(404, problem(404, "NOT_FOUND", "No such identity provider."));
      // mock 沒有 user_identities/directory_groups 可查，跟其他無關聯資料的 mock 一樣直接放行。
      IDENTITY_PROVIDERS.splice(IDENTITY_PROVIDERS.indexOf(idp), 1);
      return reply(200, { data: { id: idp.id, deleted: true }, meta: { soft_delete: true } });
    }

    // ---- admin: directory groups & role mappings ----
    if (path === "/directory-groups" && req.method === "GET") {
      const idpId = q.get("identity_provider_id");
      const groups = idpId ? DIRECTORY_GROUPS.filter((g) => g.identity_provider_id === idpId) : DIRECTORY_GROUPS;
      const page = paginate(groups.map(directoryGroupDto), { cursor: q.get("cursor"), limit: q.get("limit") });
      return reply(200, {
        ...page,
        meta: {
          total_groups: groups.length,
          groups_never_synced: groups.filter((g) => !g.last_synced_at).length,
          groups_not_mapped_to_any_role: groups.filter((g) => !DIRECTORY_ROLE_MAPPINGS.some((m) => m.directory_group_id === g.id)).length,
          populated_by: "sync",
        },
      });
    }
    if (path === "/directory-role-mappings" && req.method === "GET") {
      const isActive = q.get("is_active");
      const items = isActive === null ? DIRECTORY_ROLE_MAPPINGS : DIRECTORY_ROLE_MAPPINGS.filter((m) => String(m.is_active !== false) === isActive);
      return reply(200, { items: items.map(directoryMappingDto) });
    }
    if (path === "/directory-role-mappings" && req.method === "POST") {
      const body = await readBody(req);
      if (!body.directory_group_id || !DIRECTORY_GROUPS.some((g) => g.id === body.directory_group_id)) {
        return reply(422, problem(422, "VALIDATION", "directory_group_id 為必填，且必須是一列已同步的目錄群組。"));
      }
      if (!body.role_code || !ROLES.some((r) => r.code === body.role_code)) return reply(422, problem(422, "VALIDATION", "找不到可指派的角色。"));
      if (!["TENANT", "ORG", "FACILITY"].includes(body.scope_type)) return reply(422, problem(422, "VALIDATION", "scope_type 必須是 TENANT／ORG／FACILITY。"));
      const mapping = { id: uuid(), directory_group_id: body.directory_group_id, role_code: body.role_code, scope_type: body.scope_type, scope_id: body.scope_id ?? null, priority: body.priority ?? 100, is_active: body.is_active !== false };
      DIRECTORY_ROLE_MAPPINGS.push(mapping);
      return reply(201, directoryMappingDto(mapping));
    }
    params = match("/directory-role-mappings/:id", path);
    if (params && req.method === "DELETE") {
      const mapping = DIRECTORY_ROLE_MAPPINGS.find((m) => m.id === params.id);
      if (!mapping) return reply(404, problem(404, "NOT_FOUND", "No such directory role mapping."));
      DIRECTORY_ROLE_MAPPINGS.splice(DIRECTORY_ROLE_MAPPINGS.indexOf(mapping), 1);
      return reply(200, { deleted: true, orphaned_assignments: 0 });
    }

    // ---- admin: notification templates, webhooks, skills ----
    if (path === "/notification-templates" && req.method === "GET") return reply(200, { data: NOTIFICATION_TEMPLATES });
    if (path === "/notification-templates" && req.method === "POST") {
      const body = await readBody(req);
      const placeholders = [...(body.body_template.matchAll(/\{\{(\w+)\}\}/g) ?? [])].map((m) => m[1]);
      const tpl = { id: uuid(), is_platform: false, is_overridden: false, placeholders, is_active: true, ...body };
      const shadowed = NOTIFICATION_TEMPLATES.find((t) => t.is_platform && t.code === tpl.code && t.channel === tpl.channel && t.locale === tpl.locale);
      if (shadowed) shadowed.is_overridden = true;
      NOTIFICATION_TEMPLATES.push(tpl);
      return reply(201, tpl);
    }
    params = match("/notification-templates/:id", path);
    if (params && req.method === "PATCH") {
      const tpl = NOTIFICATION_TEMPLATES.find((t) => t.id === params.id);
      if (!tpl) return reply(404, problem(404, "NOT_FOUND", "No such notification template."));
      if (tpl.is_platform) return reply(409, problem(409, "CONFLICT", "這是平台提供的範本，不能直接修改，請以相同的 (code, channel, locale) 建立一個租戶版本。"));
      const body = await readBody(req);
      if ("subject_template" in body) tpl.subject_template = body.subject_template;
      if ("body_template" in body) {
        tpl.body_template = body.body_template;
        tpl.placeholders = [...(body.body_template.matchAll(/\{\{(\w+)\}\}/g) ?? [])].map((m) => m[1]);
      }
      if ("is_active" in body) tpl.is_active = body.is_active;
      return reply(200, tpl);
    }
    if (params && req.method === "DELETE") {
      const tpl = NOTIFICATION_TEMPLATES.find((t) => t.id === params.id);
      if (!tpl) return reply(404, problem(404, "NOT_FOUND", "No such notification template."));
      if (tpl.is_platform) return reply(409, problem(409, "CONFLICT", "這是平台提供的範本，不能刪除。"));
      const shadowed = NOTIFICATION_TEMPLATES.find((t) => t.is_platform && t.code === tpl.code && t.channel === tpl.channel && t.locale === tpl.locale);
      if (shadowed) shadowed.is_overridden = false;
      NOTIFICATION_TEMPLATES.splice(NOTIFICATION_TEMPLATES.indexOf(tpl), 1);
      return reply(204, undefined);
    }

    if (path === "/webhooks" && req.method === "GET") return reply(200, { data: WEBHOOKS, page: { next_cursor: null, limit: 50, total_estimate: WEBHOOKS.length }, meta: { subscribable_event_types: ["work_order.created", "work_order.status_changed", "reservation.confirmed", "reservation.cancelled", "alarm.raised"], signature_scheme: {}, delivery_semantics: "at_least_once" } });
    if (path === "/webhooks" && req.method === "POST") {
      const body = await readBody(req);
      const existing = WEBHOOKS.find((w) => w.url === body.url);
      if (existing) {
        Object.assign(existing, body, { updated_at: new Date().toISOString() });
        if (existing.is_active) {
          existing.disabled_at = null;
          existing.disabled_reason = null;
        }
        return reply(200, { data: existing });
      }
      const secret = `whsec_${uuid().replace(/-/g, "")}`;
      const created = { id: uuid(), consecutive_failures: 0, disabled_at: null, disabled_reason: null, last_success_at: null, last_failure_at: null, last_error: null, created_at: new Date().toISOString(), updated_at: new Date().toISOString(), is_active: true, ...body };
      WEBHOOKS.push(created);
      return reply(201, { data: created, signing_secret: secret });
    }

    if (path === "/skills" && req.method === "GET") return reply(200, { items: SKILLS });
    if (path === "/skills" && req.method === "POST") {
      const body = await readBody(req);
      const skill = { id: uuid(), tenant_id: TENANT_ID, reminder_days_before: 30, ...body };
      SKILLS.push(skill);
      return reply(201, skill);
    }
    params = match("/skills/:id", path);
    if (params && req.method === "PATCH") {
      const skill = SKILLS.find((s) => s.id === params.id);
      if (!skill) return reply(404, problem(404, "NOT_FOUND", "No such skill."));
      if (!skill.tenant_id) return reply(409, problem(409, "CONFLICT", "平台技能不可修改"));
      const body = await readBody(req);
      Object.assign(skill, body, { id: skill.id, code: skill.code, tenant_id: skill.tenant_id });
      return reply(200, skill);
    }
    if (params && req.method === "DELETE") {
      const skill = SKILLS.find((s) => s.id === params.id);
      if (!skill) return reply(404, problem(404, "NOT_FOUND", "No such skill."));
      if (!skill.tenant_id) return reply(409, problem(409, "CONFLICT", "平台技能不可刪除"));
      const holders = [...USER_SKILLS.values()].flat().filter((r) => r.skill_id === skill.id).length;
      if (holders > 0) return reply(409, problem(409, "CONFLICT", `還有 ${holders} 個使用者持有這項技能的紀錄。`));
      SKILLS.splice(SKILLS.indexOf(skill), 1);
      return reply(204, null);
    }
    params = match("/users/:userId/skills", path);
    if (params && req.method === "GET") {
      const records = USER_SKILLS.get(params.userId) ?? [];
      return reply(200, { items: records.map(userSkillDto) });
    }
    params = match("/users/:userId/skills/:skillId", path);
    if (params && req.method === "PUT") {
      const body = await readBody(req);
      const list = USER_SKILLS.get(params.userId) ?? [];
      const existing = list.find((r) => r.skill_id === params.skillId);
      const record = { skill_id: params.skillId, level: 1, ...existing, ...body };
      if (existing) Object.assign(existing, record);
      else list.push(record);
      USER_SKILLS.set(params.userId, list);
      return reply(200, userSkillDto(record));
    }
    if (params && req.method === "DELETE") {
      const list = USER_SKILLS.get(params.userId) ?? [];
      const existing = list.find((r) => r.skill_id === params.skillId);
      if (!existing) return reply(404, problem(404, "NOT_FOUND", "No such skill record."));
      USER_SKILLS.set(params.userId, list.filter((r) => r !== existing));
      return reply(204, null);
    }

    // ---- reporting ----
    if (path === "/reports/sla-compliance" && req.method === "GET") {
      const groupBy = q.get("group_by") ?? "facility";
      const labels =
        groupBy === "team" ? ["機電組", "IT 支援組", "保全組"]
        : groupBy === "priority" ? ["URGENT", "HIGH", "MEDIUM", "LOW"]
        : groupBy === "service_item" ? SERVICE_ITEMS.map((s) => s.name)
        : FACILITIES.map((f) => f.name);
      return reply(200, { data: labels.map(slaRow), meta: { group_by: groupBy, from: q.get("from"), to: q.get("to"), strictness: q.get("strictness") ?? "strict", minutes_basis: "WALLCLOCK" } });
    }
    if (path === "/reports/pm-compliance" && req.method === "GET") {
      const groupBy = q.get("group_by") ?? "facility";
      const labels = groupBy === "plan" ? MAINTENANCE_PLANS.map((p) => p.name) : groupBy === "none" ? ["全部"] : FACILITIES.map((f) => f.name);
      return reply(200, { data: labels.map(pmRow), meta: { group_by: groupBy, from: q.get("from"), to: q.get("to"), grace_source: "plan_default", missed_is_derived: true } });
    }
    if (path === "/reports/group-rollup" && req.method === "GET") {
      const data = ORGS.map((o) => {
        const facilityIds = FACILITIES.filter((f) => f.org_id === o.org_id).map((f) => f.id);
        const wos = WORK_ORDERS.filter((w) => o.depth === 0 || facilityIds.includes(w.facility_id));
        const open = wos.filter((w) => w.status_category !== "TERMINAL");
        return {
          ...o, work_orders_total: wos.length, work_orders_open: open.length,
          work_orders_overdue: open.filter((w) => w.sla_state?.includes("BREACHED")).length,
          pm_scheduled: Math.floor(rand(4, 12)), pm_on_time: Math.floor(rand(2, 10)),
          pm_on_time_rate: Math.round(rand(0.7, 0.98) * 1000) / 1000,
          total_cost: Math.round(rand(20000, 200000)), chargeback_cost: Math.round(rand(0, 40000)),
        };
      });
      return reply(200, { data, meta: { from: q.get("from"), to: q.get("to"), subtree_of: q.get("subtree_of") ?? null, rows_are_cumulative: true, subtree_basis: "org_path" } });
    }
    if (path === "/reports/asset-reliability" && req.method === "GET") {
      let assets = ASSETS;
      if (q.get("facility_id")) assets = assets.filter((a) => a.facility_id === q.get("facility_id"));
      const data = assets.map((a) => {
        const failures = a.status === "DOWN" ? 3 : a.status === "DEGRADED" ? 2 : Math.floor(rand(0, 2));
        return {
          asset_id: a.id, asset_code: a.asset_code, asset_name: a.name, facility_id: a.facility_id, criticality: a.criticality,
          failure_count: failures, corrective_orders: failures,
          mttr_hours: failures ? Math.round(rand(1, 12) * 10) / 10 : null,
          mtbf_hours: failures > 1 ? Math.round(rand(200, 2000)) : null,
          downtime_hours: Math.round(rand(0, failures * 8) * 10) / 10,
          repair_cost: failures * Math.round(rand(1500, 8000)),
          history_since: "2025-01-01T00:00:00Z",
        };
      });
      return reply(200, { data, meta: { from: q.get("from"), to: q.get("to"), limit: Number(q.get("limit")) || 50, mtbf_source: "asset_status_history", mttr_source: "work_orders", earliest_history_at: "2025-01-01T00:00:00Z", history_covers_full_range: true } });
    }
    if (path === "/reports/space-utilization" && req.method === "GET") {
      let resources = RESOURCES;
      if (q.get("facility_id")) resources = resources.filter((r) => r.facility_id === q.get("facility_id"));
      const from = new Date(q.get("from"));
      const to = new Date(q.get("to"));
      to.setHours(23, 59, 59, 999); // "to" is inclusive of the whole day, per docs/FRONTEND-GETTING-STARTED.md
      const availableHours = Math.max(1, ((to - from) / 3600_000) * (9 / 24));
      const data = resources.map((r) => {
        const rvs = RESERVATIONS.filter((rv) => rv.resource_id === r.resource_id && new Date(rv.start_at) >= from && new Date(rv.start_at) <= to);
        const active = rvs.filter((rv) => rv.status !== "CANCELLED" && rv.status !== "REJECTED");
        const bookedHours = active.reduce((sum, rv) => sum + (new Date(rv.end_at) - new Date(rv.start_at)) / 3600_000, 0);
        const checkinRequired = active.filter((rv) => rv.requires_check_in).length;
        const noShows = active.filter((rv) => rv.status === "NO_SHOW").length;
        return {
          resource_id: r.resource_id, resource_name: r.display_name, resource_type: r.resource_type, facility_id: r.facility_id,
          capacity: r.capacity, reservations_total: rvs.length, booked_hours: Math.round(bookedHours * 10) / 10,
          available_hours: Math.round(availableHours), utilization_rate: Math.min(1, Math.round((bookedHours / availableHours) * 1000) / 1000),
          hours_basis: "resource.opening_hours", checkin_required: checkinRequired, no_shows: noShows,
          no_show_rate: checkinRequired ? Math.round((noShows / checkinRequired) * 1000) / 1000 : null,
          cancelled: rvs.filter((rv) => rv.status === "CANCELLED").length,
        };
      });
      return reply(200, { data, meta: { from: q.get("from"), to: q.get("to"), resources_with_assumed_hours: 0, no_show_denominator: "requires_check_in" } });
    }
    if (path === "/reports/service-volume" && req.method === "GET") {
      const groupBy = q.get("group_by") ?? "service_item";
      const labels = groupBy === "facility" ? FACILITIES.map((f) => f.name) : groupBy === "org" ? ORGS.filter((o) => o.depth === 1).map((o) => o.org_name) : SERVICE_ITEMS.map((s) => s.name);
      const data = labels.map((label) => {
        const requests = Math.floor(rand(3, 30));
        const completed = Math.floor(rand(0, requests));
        const laborCost = Math.round(rand(500, 8000));
        const partsCost = Math.round(rand(0, 3000));
        return { group_key: label, group_label: label, requests, completed, labor_minutes: Math.round(rand(30, 600)), labor_cost: laborCost, parts_cost: partsCost, other_cost: 0, total_cost: laborCost + partsCost, chargeback_requests: Math.floor(rand(0, requests * 0.3)), chargeback_cost: Math.round(rand(0, laborCost * 0.4)), work_orders_without_rate: Math.floor(rand(0, 2)) };
      });
      return reply(200, { data, meta: { from: q.get("from"), to: q.get("to"), group_by: groupBy, work_orders_without_rate: 1, cost_is_lower_bound: true } });
    }

    params = match("/reports/:codeAction", path);
    if (params && req.method === "POST" && params.codeAction.endsWith(":export")) {
      const code = params.codeAction.slice(0, -":export".length);
      const body = await readBody(req);
      const id = uuid();
      REPORT_EXPORTS.set(id, { id, report_code: code, format: body.format ?? "csv", params: body, queuedAt: Date.now(), requested_by: USERS[username].id });
      return reply(202, reportExportView(REPORT_EXPORTS.get(id)));
    }
    params = match("/reports/exports/:id", path);
    if (params && req.method === "GET") {
      const job = REPORT_EXPORTS.get(params.id);
      if (!job) return reply(404, problem(404, "NOT_FOUND", "No such export job."));
      return reply(200, reportExportView(job));
    }
    params = match("/_mock-download/:filename", path);
    if (params && req.method === "GET") {
      res.writeHead(200, { "Content-Type": "text/csv", "Access-Control-Allow-Origin": origin });
      return res.end("group_label,value\nDemo export,mock data — this file is generated by the sandbox mock server\n");
    }

    return reply(404, problem(404, "NOT_FOUND", `No mock handler for ${req.method} ${path}`));
  } catch (err) {
    return reply(500, problem(500, "INTERNAL_ERROR", String(err && err.stack ? err.stack : err)));
  }
});

function bimStatus(model) {
  const elapsed = Date.now() - model.registeredAt;
  if (elapsed < 1500) return "UPLOADED";
  if (elapsed < 4000) return "PARSING";
  return "PARSED";
}

function statusExplanation(status) {
  switch (status) {
    case "UPLOADED":
      return "Registered — waiting for the parse worker to pick it up.";
    case "PARSING":
      return "The bim-worker is parsing this file now.";
    case "PARSED":
      return "Parsing complete. Floor View can now render this model's geometry.";
    default:
      return "";
  }
}

function ensureBimElements(model) {
  if (!model.unresolvedElements) {
    model.unresolvedElements = [
      { bim_element_id: uuid(), element_type: "IfcDoor", name: "門 D-101" },
      { bim_element_id: uuid(), element_type: "IfcSpace", name: "空間 SP-204" },
    ];
    model.mappedNodeCount = 5;
    model.mappedAssetCount = 3;
  }
  return model.unresolvedElements;
}

function publicBimModel(model) {
  const status = bimStatus(model);
  const parsed = status === "PARSED";
  if (parsed) ensureBimElements(model);
  const unresolvedCount = parsed ? model.unresolvedElements.length : 0;
  return {
    id: model.id,
    facility_id: model.facility_id,
    name: model.name,
    source_format: model.source_format,
    version_label: model.version_label,
    discipline: model.discipline,
    status,
    element_count: parsed ? 128 : 0,
    mapped_node_count: parsed ? model.mappedNodeCount : 0,
    mapped_asset_count: parsed ? model.mappedAssetCount : 0,
    unresolved_count: unresolvedCount,
    viewer_urn: model.viewer_urn,
    parsed_at: parsed ? new Date(model.registeredAt + 4000).toISOString() : null,
  };
}

function reportExportView(job) {
  const elapsed = Date.now() - job.queuedAt;
  const status = elapsed < 800 ? "PENDING" : elapsed < 2500 ? "RUNNING" : "COMPLETED";
  const filename = `${job.report_code}-${job.id.slice(0, 8)}.${job.format}`;
  return {
    id: job.id, report_code: job.report_code, format: job.format, status, params: job.params,
    row_count: status === "COMPLETED" ? Math.floor(rand(5, 40)) : null,
    download_url: status === "COMPLETED" ? `http://localhost:${PORT}/api/v1/_mock-download/${filename}` : null,
    error: null, requested_by: job.requested_by,
  };
}

function maskPrivate(reservation, username) {
  const me = USERS[username];
  const canViewPrivate = me.role === "TENANT_ADMIN" || me.role === "FACILITY_ADMIN" || reservation.organizer?.id === me.id;
  if (!reservation.is_private || canViewPrivate) return reservation;
  return { ...reservation, title: null, purpose: null, organizer: null };
}

server.listen(PORT, () => {
  console.log(`Mock FMS API listening on http://localhost:${PORT}/api/v1`);
  console.log(`Demo login: tenant_code=${TENANT_CODE}, username=admin.chen | user.huang, password=${PASSWORD}`);
});
