-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 047: 補上缺的通知文案，並讓 APPROVER 變成權限代號
-- =============================================================================
-- 041 給了 `side_effects.notify` 一個讀取點，並開始回報「宣告了要通知、
-- 卻沒有文案可送」的規則數（`no_template`）。實測是 **8 條規則、7 種 emit**：
--
--     ACCEPT            service_request.accepted      → ASSIGNEE
--     REASSIGN          work_order.reassigned         → ASSIGNEE
--     REJECT ×2         work_order.rejected           → REQUESTER
--     REQUEST_APPROVAL  work_order.approval_requested → APPROVER
--     SCHEDULE          work_order.scheduled          → ASSIGNEE
--     SUBMIT            work_order.submitted          → DISPATCHER
--     WAIT_PARTS        work_order.waiting_parts      → MAINTENANCE_SUPERVISOR
--
-- 其中 `REASSIGN` **不需要新文案**：內容就是「這張工單派給你了」，與
-- `WO_ASSIGNED` 完全相同。041 沒有配到它，只因為當初綁 template 的條件看的是
-- `emit = 'work_order.assigned'`，而 REASSIGN 發的是 `work_order.reassigned`。
-- 因此新增的是 **6 份**文案，不是 7 份。
--
-- 通道一律 `EMAIL`，與既有的 `WO_ASSIGNED`／`WO_COMPLETED` 一致。
-- 要 IN_APP 版本的租戶用 042 的 CRUD 自己加 —— 那正是 042 存在的理由，
-- 而在這裡替所有人決定「哪幾種該進站內信」會是又一個寫死的條件。
--
-- -----------------------------------------------------------------------------
-- APPROVER：那是權限，不是角色，也不是欄位
-- -----------------------------------------------------------------------------
-- 041 把它歸進「解析不到任何人」那一類並計入 `unresolved`。它該指的是
-- **能核准這張工單的人**，而那件事目錄裡已經有權威定義：`APPROVE` 這個
-- 動作的 `required_permission` 就是 `work_order:approve`。
--
-- 因此加一種代號形式：**`PERM:<權限碼>`**，解析成「持有該權限、且範圍涵蓋
-- 這張工單場域的人」。這比在 notify 清單裡列舉角色碼好，理由是後者會把
-- 「權限 → 角色」那份對應**複製**進狀態機目錄，而複製品之後不會跟著改
-- （目前持有它的是 PLATFORM_ADMIN／TENANT_ADMIN／ORG_MANAGER／FACILITY_ADMIN
-- 四個角色；045 才剛動過角色目錄，這種複製品正是那次要清掉的東西）。
--
-- 解析走 `v_user_effective_permissions`（026 補過範圍述詞的視圖），而不是
-- 逐一呼叫 `user_has_permission` —— 後者是 O(租戶使用者數) 次函式呼叫。
--
-- -----------------------------------------------------------------------------
-- 順帶補上一個「靠得住但沒說出來」的隔離
-- -----------------------------------------------------------------------------
-- 041 的角色分支**沒有租戶條件**：
--
--     JOIN fms.user_role_assignments ura ON ura.role_id = r.id
--     ... AND (wo.facility_id IS NULL OR wo.facility_id IN (…ura.user_id…))
--
-- 而 notifier 是以 `begin_platform_tx` 呼叫扇出的（要跨租戶處理 outbox），
-- 所以 RLS **不會**幫忙過濾 `user_role_assignments`。唯一擋住跨租戶的是那個
-- 場域包含條件 —— 它成立只因為 `work_orders.facility_id` 是 NOT NULL（004），
-- 使 `wo.facility_id IS NULL` 那個分支不可達。
--
-- 也就是說：**今天沒有洩漏，但擋住它的是三個檔案外的一個 NOT NULL 約束，
-- 不是這段查詢本身。** 一旦有人為了別的理由讓 facility_id 可為空，
-- 這裡會安靜地開始跨租戶群發，而且沒有任何測試會亮。
--
-- 因此：加上明確的 `ura.tenant_id = wo.tenant_id`（`PERM:` 分支同理），
-- 並移除那個不可達的 `IS NULL` 分支（一個永遠不成立的條件只會讓讀的人
-- 以為有那種情況要處理）。
--
-- **那個 tenant_id 條件是縱深防禦，不是承重牆 —— 這點是量出來的。**
-- 突變測試（拿掉 `ep.tenant_id = wo.tenant_id`）**沒有**造成跨租戶洩漏，
-- 因為場域包含條件本來就擋住了：他租戶使用者的 `user_accessible_facilities`
-- 回的是他自己租戶的場域，永遠不含這張工單的場域。
--
-- 也就是說，在 `facility_id` 是 NOT NULL 的前提下，**沒有任何可達狀態**
-- 會讓這個條件改變結果。留著它的理由因此不是「修掉一個洞」，而是
-- 「讓這段查詢的隔離性讀得出來，而不用先去查另一個檔案的欄位約束」。
-- 下面的自我驗證 (6) 相應地是**結構斷言**（那行條件還在），不是行為斷言 ——
-- 行為上它抓不到東西，因為抓不到的東西不存在。把它寫成行為測試會是一個
-- 永遠通過的測試。
--
-- -----------------------------------------------------------------------------
-- 寫文案時才發現變數不夠
-- -----------------------------------------------------------------------------
-- `notification_vars` 缺三個這些文案真的需要的值：
--
--   * `reason`               駁回要說明為什麼（`work_orders.cancelled_reason`）
--   * `scheduled_start_at`   排程通知不說時間就沒有意義
--   * `scheduled_end_at`
--
-- 這是「先寫收件人會看到的字，再回頭看資料夠不夠」的直接結果。反過來做
-- （先看有哪些欄位、再想能寫什麼）會漏掉 `reason` —— 而一封不說原因的
-- 駁回通知會直接變成一通客訴電話。
--
-- 041 的慣例是「取不到值就讓 `{{placeholder}}` 原樣留著」，因為那是
-- 「看得見的壞」，比默默變成空字串好。**這一版把那個慣例講清楚成一條規則**，
-- 因為寫這六份文案時它不夠用了：
--
--     缺值代表「有東西壞了」 → 留著 {{placeholder}}（讓人看見）
--     缺值是**正常情況**     → coalesce 兜底
--
-- 判斷「是不是正常情況」有權威來源，不用猜 —— 而那個來源是**目錄的
-- `required_fields`**，不是 `transition_work_order` 的參數預設值：
--
--   * `reason` —— 目錄裡 REJECT 的 `required_fields` 是 `{reason}`
--     → **保證有值** → 不兜底。
--     （第一版兜了底，理由是「`p_reason` 的簽章是 `DEFAULT NULL`，
--      所以不強制」。那是看錯了地方：函式簽章的預設值管的是「呼叫時可以
--      省略這個參數」，強制與否由目錄那一欄決定。實測 REJECT 少了 reason
--      會被擋在 422。）
--   * `scheduled_start_at` —— SCHEDULE 的 `required_fields` 有它
--     → 保證有值 → 不兜底。而它在 `WO_SCHEDULED` 的**主旨行**，
--     主旨裡的大括號是最顯眼的一種壞。
--   * `scheduled_end_at` —— 同一列**沒有**它 → 選填 → 兜底。
--     否則「時間：09:00 ~ {{scheduled_end_at}}」會是常態而不是例外。
--   * `location_name` —— `ck_wo_target` 只要求 `asset_id` 或
--     `spatial_node_id` 其中之一，所以「工單只綁設備、沒有空間節點」是
--     **合法且常見**的 → 兜底。而這裡的兜底不是填死字串：對只綁設備的
--     工單來說，該說的地點就是那台設備，因此 `coalesce(節點, 設備, '未指定')`。
--     一封說不出「東西在哪」的維修通知等於沒發。
--
-- 下面的自我驗證 (8)(9) 綁住上面那兩個「保證」—— 文案的正確性靠的是目錄
-- 裡的規則，而那些規則跟文案在不同的檔案裡，改的人不會想到這裡。
--
-- **`WAIT_PARTS` 拿不到原因**：那個 reason 寫在轉移列上，不在工單上
-- （只有 CANCELLED／REJECTED 才寫進 `cancelled_reason`），而 032 的
-- `emit_event` 沒有把它帶進 payload。因此那份文案只說「等待零件」，
-- 不引用自由文字。記在這裡，而不是硬湊一個看起來有值的變數。
--
-- 依賴：004（facility_id NOT NULL、cancelled_reason）、026（範圍述詞視圖）、
--       041（扇出）、042（覆寫優先序、template_placeholders）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 1. 範本變數：補上 reason 與排程時刻
-- -----------------------------------------------------------------------------
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
    -- 047：排程通知不說時間就沒有意義。
    --
    -- start 不兜底：目錄裡 SCHEDULE 的 required_fields 保證它有值，
    -- 所以這裡缺值代表真的壞了，該讓人看見（它在主旨行）。
    'scheduled_start_at', to_char(wo.scheduled_start_at AT TIME ZONE
                            coalesce(f.timezone, fms.partition_boundary_timezone()),
                            'YYYY-MM-DD HH24:MI'),
    -- end 兜底：同一份目錄沒有要求它，所以缺值是正常情況。
    'scheduled_end_at',   coalesce(to_char(wo.scheduled_end_at AT TIME ZONE
                            coalesce(f.timezone, fms.partition_boundary_timezone()),
                            'YYYY-MM-DD HH24:MI'), '未指定'),
    'facility_name',      f.name,
    -- 047 改成兜底：`ck_wo_target` 允許「只綁設備、沒有空間節點」，
    -- 所以缺空間節點是正常情況。而對那種工單來說，該說的地點就是那台設備
    -- —— 一封說不出「東西在哪」的維修通知等於沒發。
    'location_name',      coalesce(sn.name, a.name, '未指定'),
    'assignee_name',      au.display_name,
    'requester_name',     ru.display_name,
    'resolution_notes',   wo.resolution_notes,
    -- 047：駁回要說明為什麼。**不兜底** —— 目錄裡 REJECT 的 required_fields
    -- 是 `{reason}`，所以有值是被保證的（自我驗證 (9) 綁住那個保證）。
    'reason',             wo.cancelled_reason
  ))
    FROM fms.work_orders wo
    LEFT JOIN fms.facilities f     ON f.id  = wo.facility_id
    LEFT JOIN fms.spatial_nodes sn ON sn.id = wo.spatial_node_id
    LEFT JOIN fms.assets a         ON a.id  = wo.asset_id
    LEFT JOIN fms.users au         ON au.id = wo.assignee_id
    LEFT JOIN fms.users ru         ON ru.id = wo.requester_id
   WHERE wo.id = p_work_order_id;
$$;

COMMENT ON FUNCTION fms.notification_vars(uuid) IS
  '工單的範本變數。時刻以該場域的時區格式化 —— 收件人看到的應該是他當地的'
  '時間。jsonb_strip_nulls：沒有值的變數不出現，render_template 會把對應的'
  '{{placeholder}} 原樣留下（041 檔頭說明了為什麼那比換成空字串好）。'
  '例外：reason 用 coalesce 兜底，因為它出現在唯一寄給報修人的文案裡，'
  '而狀態機不強制填原因（047）。';

-- -----------------------------------------------------------------------------
-- 2. 收件人解析：加上 PERM:<權限碼>，並把租戶條件寫出來
-- -----------------------------------------------------------------------------
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
    SELECT id, tenant_id, facility_id, assignee_id, requester_id
      FROM fms.work_orders WHERE id = p_work_order_id
  )
  -- 關係代號優先。`REQUESTER` 與 `DISPATCHER` 也是角色碼，而 REQUESTER
  -- 在 notify 清單裡指的是「報修的那個人」—— 當成角色會變成群發。
  SELECT t.tok, wo.assignee_id
    FROM tokens t, wo WHERE t.tok = 'ASSIGNEE'
  UNION ALL
  SELECT t.tok, wo.requester_id
    FROM tokens t, wo WHERE t.tok = 'REQUESTER'
  UNION ALL
  -- `PERM:<權限碼>` → 持有該權限、且範圍涵蓋這張工單場域的人。
  --
  -- 走 026 補過範圍述詞的視圖，而不是逐一呼叫 `user_has_permission`
  -- （那是 O(租戶使用者數) 次函式呼叫）。最後的場域判斷仍用 021 的權威
  -- 函式 —— 把 scope_type/scope_id 展開成場域清單是它的工作，不在這裡重寫。
  --
  -- `ep.tenant_id = wo.tenant_id`：扇出跑在平台情境下，這個視圖不受 RLS
  -- 過濾。實測它是**縱深防禦而非承重牆** —— 場域包含條件已經擋住跨租戶
  -- （見檔頭的突變結果）。留著是為了讓隔離性在這段查詢裡讀得出來。
  SELECT t.tok, ep.user_id
    FROM tokens t
    CROSS JOIN wo
    JOIN fms.v_user_effective_permissions ep
      ON ep.permission_code = substring(t.tok FROM 6)
     AND ep.tenant_id = wo.tenant_id
   WHERE t.tok LIKE 'PERM:%'
     AND wo.facility_id IN (
           SELECT af FROM fms.user_accessible_facilities(ep.user_id) af)
  UNION ALL
  -- 其餘當角色碼：持有該角色、且可存取範圍涵蓋這個工單的場域。
  --
  -- 047 加上 `ura.tenant_id = wo.tenant_id`（原本只靠場域包含條件，
  -- 而那成立只因為 facility_id 是 NOT NULL），並移除不可達的
  -- `wo.facility_id IS NULL` 分支。
  SELECT t.tok, ura.user_id
    FROM tokens t
    JOIN fms.roles r ON r.code = t.tok
    JOIN fms.user_role_assignments ura ON ura.role_id = r.id
    CROSS JOIN wo
   WHERE t.tok NOT IN ('ASSIGNEE', 'REQUESTER')
     AND t.tok NOT LIKE 'PERM:%'
     AND ura.tenant_id = wo.tenant_id
     AND wo.facility_id IN (
           SELECT af FROM fms.user_accessible_facilities(ura.user_id) af)
  UNION ALL
  -- 解析不到任何人 → 回一列 user_id 為 NULL，計入 `unresolved`，不靜默丟掉。
  --
  -- **`PERM:` 也必須走這裡。** 第一版把 `PERM:%` 從這個分支整個排除掉，
  -- 於是打錯的權限碼（`PERM:work_order:aprove`）回的是**空集合** ——
  -- 也就是安靜地誰都不通知，而那正是 041 的 `APPROVER` 的行為、
  -- 也正是這整個 migration 存在的理由。差點把要修的東西換個地方重寫一遍。
  --
  -- migration 的自我驗證 (3) 只擋得住「目錄裡的代號打錯」；
  -- 這裡擋的是執行期真的解析到空集合。
  --
  -- **判準是「有沒有解析到人」，不是「代號長得對不對」。**
  -- 041 的版本問的是後者（`NOT EXISTS ... roles WHERE code = tok`），
  -- 於是「角色存在、但這個場域範圍內沒有人持有它」會回空集合 ——
  -- 三個計數器全是 0，看起來像成功。
  --
  -- 這不是假想的：種子裡 `MAINTENANCE_SUPERVISOR` 與 `DISPATCHER`
  -- **一個持有者都沒有**，而 047 剛補的 `WO_WAITING_PARTS` 與
  -- `WO_SUBMITTED` 正是發給這兩個角色的。也就是說，光補文案並不會讓那兩種
  -- 通知送得出去，而舊的判準會讓那件事完全看不見。
  SELECT t.tok, NULL::uuid
    FROM tokens t, wo
   WHERE t.tok NOT IN ('ASSIGNEE', 'REQUESTER')
     AND CASE
           WHEN t.tok LIKE 'PERM:%' THEN NOT EXISTS (
             SELECT 1
               FROM fms.v_user_effective_permissions ep
              WHERE ep.permission_code = substring(t.tok FROM 6)
                AND ep.tenant_id = wo.tenant_id
                AND wo.facility_id IN (
                      SELECT af FROM fms.user_accessible_facilities(ep.user_id) af))
           ELSE NOT EXISTS (
             SELECT 1
               FROM fms.roles r
               JOIN fms.user_role_assignments ura ON ura.role_id = r.id
              WHERE r.code = t.tok
                AND ura.tenant_id = wo.tenant_id
                AND wo.facility_id IN (
                      SELECT af FROM fms.user_accessible_facilities(ura.user_id) af))
         END;
$$;

COMMENT ON FUNCTION fms.notification_recipients(uuid, jsonb) IS
  'notify 清單 → 使用者。三種形式：關係代號（ASSIGNEE／REQUESTER）、'
  'PERM:<權限碼>、角色碼。關係代號優先於角色碼，因為 REQUESTER 兩者都是'
  '而語意不同（會變成群發）。兩個群體分支都明寫 tenant_id 條件：'
  '扇出跑在平台情境下，RLS 不會幫忙過濾（047）。'
  'user_id 為 NULL = 解析不到，由 fan_out_notifications 計入 unresolved；'
  '判準是「有沒有解析到人」而非「代號長得對不對」，因此「角色存在但這個'
  '場域沒有人持有」也會被計數（047）。';

-- -----------------------------------------------------------------------------
-- 3. 六份平台文案
-- -----------------------------------------------------------------------------
-- `tenant_id IS NULL` = 平台預設，租戶可用 042 的 CRUD 覆寫。
-- ON CONFLICT 用的是 uq_notification_templates 那個運算式索引的完整運算式，
-- 少一項就會變成「約束不存在」而報錯（migration 要可重跑）。
INSERT INTO fms.notification_templates
  (tenant_id, code, channel, locale, subject_template, body_template, is_active)
VALUES
  -- ACCEPT：報修受理並同時派工。內容近似 WO_ASSIGNED，但收件人在意的是
  -- 「這件事被受理了，而且是我要處理」這兩件事一起發生。
  (NULL, 'SR_ACCEPTED', 'EMAIL', 'zh-TW',
   '【已受理派工】{{wo_no}} — {{title}}',
   '您好 {{assignee_name}}，'||chr(10)||
   '報修 {{wo_no}}（{{title}}）已受理，並派給您處理。'||chr(10)||
   '地點：{{facility_name}} / {{location_name}}'||chr(10)||
   '優先度：{{priority}}'||chr(10)||
   '要求完成時間：{{resolution_due_at}}'||chr(10)||
   '請於系統中確認並開始作業。',
   true),

  -- REJECT：唯一寄給報修人的一份。用字避開內部術語（不說「駁回」的
  -- 兩種來源差異）—— 兩條規則（SUBMITTED→REJECTED 與
  -- PENDING_APPROVAL→REJECTED）共用這一份，措辭必須兩種都成立。
  (NULL, 'WO_REJECTED', 'EMAIL', 'zh-TW',
   '【報修未通過】{{wo_no}} — {{title}}',
   '您好 {{requester_name}}，'||chr(10)||
   '您回報的 {{wo_no}}（{{title}}）未能受理。'||chr(10)||
   '原因：{{reason}}'||chr(10)||
   '若情況仍未解決，請補充說明後重新提出，或直接聯繫設施管理員。',
   true),

  -- REQUEST_APPROVAL：收件人是「能核准的人」，一份待辦。
  -- 主旨沿用 RESERVATION_APPROVAL_REQUIRED 的【待審核】。
  (NULL, 'WO_APPROVAL_REQUESTED', 'EMAIL', 'zh-TW',
   '【待審核】{{wo_no}} — {{title}}',
   '工單 {{wo_no}}（{{title}}）已送出審核，請前往系統核准或駁回。'||chr(10)||
   '地點：{{facility_name}} / {{location_name}}'||chr(10)||
   '優先度：{{priority}}'||chr(10)||
   '申請人：{{requester_name}}'||chr(10)||
   '要求完成時間：{{resolution_due_at}}',
   true),

  -- SCHEDULE：主旨帶時間，因為這封的唯一資訊就是時間。
  (NULL, 'WO_SCHEDULED', 'EMAIL', 'zh-TW',
   '【工單排程】{{wo_no}} {{scheduled_start_at}}',
   '您好 {{assignee_name}}，'||chr(10)||
   '工單 {{wo_no}}（{{title}}）已排定作業時間。'||chr(10)||
   '時間：{{scheduled_start_at}} ~ {{scheduled_end_at}}'||chr(10)||
   '地點：{{facility_name}} / {{location_name}}'||chr(10)||
   '優先度：{{priority}}',
   true),

  -- SUBMIT：收件人是派工者，缺的資訊是「該派給誰」要的判斷依據。
  (NULL, 'WO_SUBMITTED', 'EMAIL', 'zh-TW',
   '【新工單待派工】{{wo_no}} — {{title}}',
   '工單 {{wo_no}}（{{title}}）已送出，等待派工。'||chr(10)||
   '地點：{{facility_name}} / {{location_name}}'||chr(10)||
   '優先度：{{priority}}'||chr(10)||
   '申請人：{{requester_name}}'||chr(10)||
   '要求完成時間：{{resolution_due_at}}',
   true),

  -- WAIT_PARTS：不引用自由文字的原因 —— 拿不到（見檔頭）。
  (NULL, 'WO_WAITING_PARTS', 'EMAIL', 'zh-TW',
   '【等待零件】{{wo_no}} — {{title}}',
   '工單 {{wo_no}}（{{title}}）已暫停，等待零件到料。'||chr(10)||
   '地點：{{facility_name}} / {{location_name}}'||chr(10)||
   '負責人：{{assignee_name}}'||chr(10)||
   '要求完成時間：{{resolution_due_at}}'||chr(10)||
   '請確認備品庫存或採購進度 —— SLA 時鐘不會因為等料而停。',
   true)
ON CONFLICT (COALESCE(tenant_id, '00000000-0000-0000-0000-000000000000'::uuid),
             lower(code::text), channel, locale)
DO UPDATE SET subject_template = EXCLUDED.subject_template,
              body_template    = EXCLUDED.body_template,
              is_active        = EXCLUDED.is_active;

-- -----------------------------------------------------------------------------
-- 4. 把文案接到規則上，並把 APPROVER 換成權限代號
-- -----------------------------------------------------------------------------
UPDATE fms.work_order_transitions_allowed
   SET side_effects = side_effects || jsonb_build_object('template', v.tmpl)
  FROM (VALUES
    ('service_request.accepted',      'SR_ACCEPTED'),
    ('work_order.reassigned',         'WO_ASSIGNED'),   -- 重用，見檔頭
    ('work_order.rejected',           'WO_REJECTED'),
    ('work_order.approval_requested', 'WO_APPROVAL_REQUESTED'),
    ('work_order.scheduled',          'WO_SCHEDULED'),
    ('work_order.submitted',          'WO_SUBMITTED'),
    ('work_order.waiting_parts',      'WO_WAITING_PARTS')
  ) AS v(emit, tmpl)
 WHERE side_effects ->> 'emit' = v.emit
   AND side_effects ? 'notify';

-- APPROVER → PERM:work_order:approve。
-- 右邊的值刻意**不寫死**成角色清單：見檔頭。
UPDATE fms.work_order_transitions_allowed
   SET side_effects = jsonb_set(side_effects, '{notify}',
                                '["PERM:work_order:approve"]'::jsonb)
 WHERE side_effects -> 'notify' @> '["APPROVER"]'::jsonb;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- CORE 位置：**跑在 009 之前**，因此不能引用任何租戶資料。
-- 以下全部只看目錄（tenant_id IS NULL）與純函式。
DO $$
DECLARE
  v_bad  text;
  v_n    bigint;
  v_ph   text;
  v_def  text;
BEGIN
  -- (1) 041 那個 no_template 計數器該歸零了：每一條有 notify 的規則都要有
  --     template 鍵，而且那個 code 要真的有平台文案存在。
  --
  --     這一格是整個 migration 的目的。它不是「我改的那幾條對了」，
  --     而是「沒有任何一條宣告了要通知卻沒有字可送」。
  SELECT string_agg(DISTINCT action || '(' || from_status || '→' || to_status || ')',
                    '、' ORDER BY action || '(' || from_status || '→' || to_status || ')')
    INTO v_bad
    FROM fms.work_order_transitions_allowed w
   WHERE w.is_active
     AND w.side_effects ? 'notify'
     AND NOT EXISTS (
           SELECT 1 FROM fms.notification_templates t
            WHERE t.tenant_id IS NULL
              AND lower(t.code) = lower(w.side_effects ->> 'template')
              AND t.is_active);
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION '047 FAILED: 這些規則宣告了 notify 卻沒有可用的平台文案：%', v_bad;
  END IF;

  -- (2) 沒有規則還在用 APPROVER。
  --     直接驗語意，而不是驗「我跑過那句 UPDATE」。
  IF EXISTS (SELECT 1 FROM fms.work_order_transitions_allowed
              WHERE side_effects -> 'notify' @> '["APPROVER"]'::jsonb) THEN
    RAISE EXCEPTION '047 FAILED: 仍有規則的 notify 含 APPROVER（解析不到任何人）';
  END IF;

  -- (3) PERM: 代號指向的權限真的存在於目錄裡。
  --     打錯字的症狀是「安靜地誰都不通知」—— 那正是 041 之前 APPROVER 的
  --     行為，而這個 migration 就是為了消掉它。
  SELECT string_agg(DISTINCT tok, '、' ORDER BY tok) INTO v_bad
    FROM fms.work_order_transitions_allowed w,
         jsonb_array_elements_text(w.side_effects -> 'notify') AS tok
   WHERE tok LIKE 'PERM:%'
     AND NOT EXISTS (SELECT 1 FROM fms.permissions p
                      WHERE p.code = substring(tok FROM 6));
  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION '047 FAILED: 這些 PERM: 代號指向不存在的權限：%', v_bad;
  END IF;

  -- (4) 而且真的有角色持有它 —— 否則 PERM: 解析出來仍然是空集合，
  --     只是換了個安靜失敗的方式。
  SELECT count(*) INTO v_n
    FROM fms.role_permissions rp
   WHERE rp.permission_code = 'work_order:approve';
  IF v_n = 0 THEN
    RAISE EXCEPTION '047 FAILED: 沒有任何角色持有 work_order:approve，'
                    'PERM:work_order:approve 會解析成空集合';
  END IF;

  -- (5) **每一個文案用到的 {{變數}}，notification_vars 都要生得出來。**
  --
  --     這一格抓的是我自己這次差點犯的錯：先寫了 {{scheduled_start_at}}，
  --     才發現那個變數不存在。少一個變數的症狀是收件人看到大括號 ——
  --     沒有任何錯誤，也沒有任何計數器會動。
  --
  --     檢查方式是「那個鍵名有出現在 notification_vars 的原始碼裡」。
  --     是文字比對，不是語意比對 —— 但它抓得到「忘了加」，
  --     而那是唯一實際發生過的失效模式。
  -- 以下三格都是「對函式原始碼做文字比對」，因此**必須先把 `--` 註解拿掉**。
  --
  -- 第一版沒有拿掉，於是 (7) 立刻失敗 —— 因為那一格檢查的是
  -- 「程式碼裡不該再出現某個述詞」，而**函式裡描述這件事的那句註解本身
  -- 就含有那段文字**。pg_get_functiondef 會把註解一起回傳。
  --
  -- 換句話說：說明不變量的句子違反了那個不變量。改註解措辭可以繞過，
  -- 但下一個人寫註解時又會踩到，所以修在檢查這一邊。
  v_def := regexp_replace(
             pg_get_functiondef('fms.notification_vars(uuid)'::regprocedure),
             '--[^' || chr(10) || ']*', '', 'g');
  FOR v_ph IN
    -- template_placeholders 回 text[]（不是 setof），所以要 unnest。
    SELECT DISTINCT ph
      FROM fms.notification_templates t,
           unnest(fms.template_placeholders(
             coalesce(t.subject_template, '') || ' ' || t.body_template)) AS ph
     WHERE t.tenant_id IS NULL
       AND t.code IN ('SR_ACCEPTED', 'WO_REJECTED', 'WO_APPROVAL_REQUESTED',
                      'WO_SCHEDULED', 'WO_SUBMITTED', 'WO_WAITING_PARTS')
  LOOP
    IF v_def NOT LIKE '%''' || v_ph || '''%' THEN
      RAISE EXCEPTION
        '047 FAILED: 文案用了 {{%}}，但 notification_vars 生不出這個變數 —— '
        '收件人會看到大括號', v_ph;
    END IF;
  END LOOP;

  -- (6) 兩個群體分支都明寫了 tenant_id 條件。
  --
  --     **這是結構斷言，不是行為斷言。** 拿掉那個條件不會造成洩漏
  --     （場域包含條件已經擋住了，見檔頭的突變結果），所以沒有任何
  --     行為測試抓得到它。這一格保的是「隔離性在這段查詢裡讀得出來」，
  --     而不是「這裡有個洞被補上」—— 兩者的價值不同，別當成同一件事。
  v_def := regexp_replace(
             pg_get_functiondef('fms.notification_recipients(uuid, jsonb)'::regprocedure),
             '--[^' || chr(10) || ']*', '', 'g');
  IF v_def NOT LIKE '%ep.tenant_id = wo.tenant_id%'
     OR v_def NOT LIKE '%ura.tenant_id = wo.tenant_id%' THEN
    RAISE EXCEPTION '047 FAILED: notification_recipients 的群體分支必須明寫 '
                    'tenant_id 條件（RLS 在平台情境下不會過濾）';
  END IF;

  -- (7) 不可達的 IS NULL 分支已經移除。
  IF v_def LIKE '%facility_id IS NULL%' THEN
    RAISE EXCEPTION '047 FAILED: work_orders.facility_id 是 NOT NULL，'
                    '那個分支不可達 —— 留著只會讓人以為有那種情況要處理';
  END IF;

  -- (8) `WO_SCHEDULED` 的主旨用了 {{scheduled_start_at}} 而**沒有兜底**，
  --     依據是目錄保證 SCHEDULE 那條規則會要求這個欄位有值。
  --     那個依賴跨檔案且完全隱含 —— 有人把 required_fields 清空的話，
  --     症狀是派工對象收到主旨「【工單排程】WO-123 {{scheduled_start_at}}」，
  --     沒有任何錯誤、沒有任何計數器會動。這一格把它綁起來。
  IF NOT EXISTS (
    SELECT 1 FROM fms.work_order_transitions_allowed
     WHERE action = 'SCHEDULE'
       AND required_fields @> ARRAY['scheduled_start_at']
  ) THEN
    RAISE EXCEPTION
      '047 FAILED: SCHEDULE 的 required_fields 不再包含 scheduled_start_at，'
      '而 WO_SCHEDULED 的主旨靠那個保證才沒有兜底 —— 收件人會在主旨看到大括號';
  END IF;

  -- (9) `WO_REJECTED` 的內文用了 {{reason}} 而**沒有兜底**，依據是目錄
  --     保證 REJECT 那條規則會要求填原因。這是這批唯一寄給**報修人**的
  --     文案，所以「有人把 required_fields 清空」的症狀是一般使用者收到
  --     「原因：{{reason}}」。跟 (8) 同一類的跨檔案隱含依賴。
  IF NOT EXISTS (
    SELECT 1 FROM fms.work_order_transitions_allowed
     WHERE action = 'REJECT'
       AND required_fields @> ARRAY['reason']
  ) THEN
    RAISE EXCEPTION
      '047 FAILED: REJECT 的 required_fields 不再包含 reason，'
      '而 WO_REJECTED 的內文靠那個保證才沒有兜底 —— 報修人會看到大括號';
  END IF;

  RAISE NOTICE '047 OK: 有 notify 的規則全部有文案；APPROVER → PERM:work_order:approve';
END;
$$;

COMMIT;
