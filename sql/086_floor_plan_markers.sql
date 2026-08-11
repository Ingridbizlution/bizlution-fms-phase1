-- =============================================================================
-- 086：2.5D 樓層平面圖的設備標點（floor_plan_markers）
-- =============================================================================
-- 背景：客戶提供的 2D 平面圖（JPG/PNG，PDF 前端先轉成 PNG）當底圖，疊加設備
-- 標點做成前端的輕量 3D 示意檢視——不是 BIM 幾何，不追求真實牆體。
--
-- 平面圖影像本身刻意不進這支 migration：每個樓層本身就是一列
-- `node_type_code = 'FLOOR'` 的 `spatial_nodes`（見 003），而 `fms.attachments`
-- 已經支援 `entity_type = 'SPATIAL_NODE'` 的直傳上傳、S3/MinIO 儲存、presigned
-- 下載 URL（見 001）。平面圖影像就是「該 FLOOR 節點的一筆
-- `purpose = 'FLOOR_PLAN_IMAGE'` 附件」，重用既有機制，這支 migration 不重造。
--
-- 這裡真正缺的、attachments 沒地方放的，只有「設備在那張圖上的位置」。
--
-- 座標存相對比例（0.0000–1.0000），不是真實世界公尺——這不是疊在 BIM 幾何上
-- 校準，純粹是「貼圖 + 標點」，前端拿比例乘上圖片顯示寬高還原像素位置即可。
--
-- `floor_node_id` 必須指向 `node_type_code = 'FLOOR'` 的節點，但這裡不加
-- CHECK：要驗證得 join `spatial_node_types`，且型別目錄是租戶可擴充的
-- （003），資料庫層的靜態 CHECK 做不到。留給 API handler 在寫入前查一次
-- （比照 077 對 directory_role_mappings.scope_type 的驗證方式，做在 Rust
-- 端，不做在 DB 端）。
--
-- `entity_type`/`entity_id` 沿用 `fms.attachments` 的 polymorphic 寫法——不加
-- FK，因為指向的表（assets／devices／spatial_nodes）依 entity_type 而不同。
--
-- 沒有 `updated_at`／`deleted_at`：這張表只有「新增一個標點」跟「移除一個
-- 標點」兩種操作，沒有「編輯」，比照 `user_role_assignments`（002）的
-- 硬刪除慣例，不做軟刪除。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

CREATE TABLE fms.floor_plan_markers (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  floor_node_id uuid NOT NULL REFERENCES fms.spatial_nodes(id) ON DELETE CASCADE,
  entity_type   varchar(20) NOT NULL CHECK (entity_type IN ('ASSET','DEVICE','SPATIAL_NODE')),
  entity_id     uuid NOT NULL,
  x_ratio       numeric(6,4) NOT NULL CHECK (x_ratio BETWEEN 0 AND 1),
  y_ratio       numeric(6,4) NOT NULL CHECK (y_ratio BETWEEN 0 AND 1),
  -- 3D 視覺用的懸浮高度微調（例如天花板感測器），不是真實樓高。
  z_offset      numeric(4,2) NOT NULL DEFAULT 0,
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  created_by    uuid REFERENCES fms.users(id) ON DELETE SET NULL
);

CREATE INDEX idx_floor_plan_markers_floor_node
  ON fms.floor_plan_markers (floor_node_id);

ALTER TABLE fms.floor_plan_markers ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.floor_plan_markers FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON fms.floor_plan_markers FOR ALL
  USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
  WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

-- 這張表沒有自己的 facility_id，透過 floor_node_id 間接算出場域——跟
-- calendar_resource_mappings（083）同一類洩漏：只掛 tenant_isolation，一個
-- 場域受限的讀者會看不到父列（spatial_nodes 已經場域收斂），但仍讀得到子列。
-- EXISTS 子查詢對 spatial_nodes 的 SELECT 會自動套用 spatial_nodes 自己的
-- RLS，因此這裡不用重複寫場域比對邏輯。
CREATE POLICY facility_scope_via_parent ON fms.floor_plan_markers
  AS RESTRICTIVE FOR ALL
  USING (fms.is_platform_context()
         OR EXISTS (SELECT 1 FROM fms.spatial_nodes p
                     WHERE p.id = fms.floor_plan_markers.floor_node_id));

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_class
     WHERE oid = 'fms.floor_plan_markers'::regclass AND relforcerowsecurity
  ) THEN
    RAISE EXCEPTION '086 FAILED: floor_plan_markers 沒有 FORCE ROW LEVEL SECURITY';
  END IF;
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
     WHERE schemaname = 'fms' AND tablename = 'floor_plan_markers'
       AND policyname = 'facility_scope_via_parent'
  ) THEN
    RAISE EXCEPTION '086 FAILED: floor_plan_markers 缺少 facility_scope_via_parent 政策';
  END IF;

  RAISE NOTICE '086 OK: floor_plan_markers（FORCE RLS + facility_scope_via_parent）';
END;
$$;

COMMIT;
