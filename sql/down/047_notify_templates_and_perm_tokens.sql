-- =============================================================================
-- Down: 047_notify_templates_and_perm_tokens
-- =============================================================================
-- 回到 041／042 的狀態：8 條規則沒有文案，APPROVER 解析不到任何人。
-- 也就是把 `no_template` 與 `unresolved` 兩個計數器**放回去** ——
-- 那是 041 刻意留下的、可觀測的缺口。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- 1. 規則：拔掉 template 鍵，APPROVER 放回去
UPDATE fms.work_order_transitions_allowed
   SET side_effects = jsonb_set(side_effects, '{notify}', '["APPROVER"]'::jsonb)
 WHERE side_effects -> 'notify' @> '["PERM:work_order:approve"]'::jsonb;

UPDATE fms.work_order_transitions_allowed
   SET side_effects = side_effects - 'template'
 WHERE side_effects ->> 'emit' IN (
         'service_request.accepted', 'work_order.reassigned', 'work_order.rejected',
         'work_order.approval_requested', 'work_order.scheduled',
         'work_order.submitted', 'work_order.waiting_parts')
   AND side_effects ? 'notify';

-- 2. 六份平台文案。只刪 tenant_id IS NULL 的 —— 租戶自己用 042 的 CRUD
--    加的覆寫版本不是這個 migration 建的，不該由它刪。
DELETE FROM fms.notification_templates
 WHERE tenant_id IS NULL
   AND code IN ('SR_ACCEPTED', 'WO_REJECTED', 'WO_APPROVAL_REQUESTED',
                'WO_SCHEDULED', 'WO_SUBMITTED', 'WO_WAITING_PARTS');

-- 3. notification_vars 回到 041 的版本（去掉 reason 與兩個排程時刻）
CREATE OR REPLACE FUNCTION fms.notification_vars(p_work_order_id uuid)
RETURNS jsonb
LANGUAGE sql
STABLE
AS $$
  SELECT jsonb_strip_nulls(jsonb_build_object(
    'wo_no',              wo.wo_no,
    'title',              wo.title,
    'status',             wo.status,
    'priority',           wo.priority,
    'resolution_due_at',  to_char(wo.resolution_due_at AT TIME ZONE
                            coalesce(f.timezone, fms.partition_boundary_timezone()),
                            'YYYY-MM-DD HH24:MI'),
    'response_due_at',    to_char(wo.response_due_at AT TIME ZONE
                            coalesce(f.timezone, fms.partition_boundary_timezone()),
                            'YYYY-MM-DD HH24:MI'),
    'completed_at',       to_char(wo.completed_at AT TIME ZONE
                            coalesce(f.timezone, fms.partition_boundary_timezone()),
                            'YYYY-MM-DD HH24:MI'),
    'facility_name',      f.name,
    'location_name',      sn.name,
    'assignee_name',      au.display_name,
    'requester_name',     ru.display_name,
    'resolution_notes',   wo.resolution_notes
  ))
    FROM fms.work_orders wo
    LEFT JOIN fms.facilities f     ON f.id  = wo.facility_id
    LEFT JOIN fms.spatial_nodes sn ON sn.id = wo.spatial_node_id
    LEFT JOIN fms.users au         ON au.id = wo.assignee_id
    LEFT JOIN fms.users ru         ON ru.id = wo.requester_id
   WHERE wo.id = p_work_order_id;
$$;

-- 4. notification_recipients 回到 041 的版本
--    （沒有 PERM: 分支、沒有明寫的 tenant_id 條件、有那個不可達的 IS NULL）
CREATE OR REPLACE FUNCTION fms.notification_recipients(
  p_work_order_id uuid,
  p_notify        jsonb
) RETURNS TABLE (token text, user_id uuid)
LANGUAGE sql
STABLE
AS $$
  WITH tokens AS (
    SELECT jsonb_array_elements_text(coalesce(p_notify, '[]'::jsonb)) AS tok
  ), wo AS (
    SELECT id, facility_id, assignee_id, requester_id
      FROM fms.work_orders WHERE id = p_work_order_id
  )
  SELECT t.tok, wo.assignee_id
    FROM tokens t, wo WHERE t.tok = 'ASSIGNEE'
  UNION ALL
  SELECT t.tok, wo.requester_id
    FROM tokens t, wo WHERE t.tok = 'REQUESTER'
  UNION ALL
  SELECT t.tok, ura.user_id
    FROM tokens t
    JOIN fms.roles r ON r.code = t.tok
    JOIN fms.user_role_assignments ura ON ura.role_id = r.id
    CROSS JOIN wo
   WHERE t.tok NOT IN ('ASSIGNEE', 'REQUESTER')
     AND (wo.facility_id IS NULL
          OR wo.facility_id IN (
               SELECT af FROM fms.user_accessible_facilities(ura.user_id) af))
  UNION ALL
  SELECT t.tok, NULL::uuid
    FROM tokens t
   WHERE t.tok NOT IN ('ASSIGNEE', 'REQUESTER')
     AND NOT EXISTS (SELECT 1 FROM fms.roles r WHERE r.code = t.tok);
$$;

COMMIT;
