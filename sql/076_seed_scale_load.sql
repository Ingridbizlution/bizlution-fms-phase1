-- =============================================================================
-- 076：壓力測試的規模夾具 —— 250 帳號／100 教室／1000 裝置／200 工單
-- =============================================================================
-- 目標規模由客戶給定。這個檔案**只建資料**，不改任何 schema、函式或政策。
--
-- -----------------------------------------------------------------------------
-- 為什麼是獨立的 migrate 模式（`MIGRATE_MODE=scale`）
-- -----------------------------------------------------------------------------
-- 它把 users 從 7 筆變成 257 筆、bookable_resources 從 4 變成 104。
-- 若它進了 `all`（測試 template 用的模式），**每一個整合測試的斷言都會改變** ——
-- 那些「清單回幾筆」、「分頁第二頁是什麼」的斷言會全部失效，而症狀是
-- 幾十格測試同時紅掉，看起來像是別的東西壞了。
--
-- 因此它既不在 `all` 也不在 `demo`，只在 `scale`。
--
-- -----------------------------------------------------------------------------
-- 「1000 裝置」的解讀
-- -----------------------------------------------------------------------------
-- 建成 `fms.devices`（IoT 受監控裝置）＋ 每台一個 `telemetry_points`，
-- 而不是 `fms.assets`。理由：壓力測試需要 `POST /telemetry:batch-ingest`
-- 有 1000 個真實的目標點，而那條路徑（讀值 → 規則 → 告警 → 工單）
-- 是這個系統寫入量最大的一條。
--
-- 若客戶說的「裝置」其實是設備台帳（assets），對負載特性沒有差別：
-- 兩者都是「1000 列被監控的東西」，而工單的目標用教室節點就足夠。
-- 這個解讀寫在這裡而不是留給讀者猜。
--
-- -----------------------------------------------------------------------------
-- 200 張工單是**資料量**，不是吞吐目標
-- -----------------------------------------------------------------------------
-- 「200 張工單」建成 200 筆未結案工單。**這是一個小表**：查詢規劃器在 200 列
-- 上看到的是一個可以整表掃完的東西，因此量出來的延遲反映的是**併發與鎖競爭**，
-- 不是索引在真實資料量下的效率。
--
-- 要量後者需要 10 倍以上的資料量，而那是另一次量測（`SCALE_FACTOR`）。
-- 不在這裡偷偷加大：客戶給的是 200，報告要對得上那個數字。
--
-- -----------------------------------------------------------------------------
-- 固定的鹽與共用的雜湊
-- -----------------------------------------------------------------------------
-- 250 個帳號共用同一個 argon2id 雜湊（與 075 相同，密碼 `Demo1234!`）。
-- argon2 的驗證成本與鹽無關，因此對「量登入成本」這件事沒有影響，
-- 而產生 250 個不同的雜湊要在 PostgreSQL 裡跑 250 次 argon2 —— 做不到
-- （pgcrypto 沒有 argon2，見 075 的說明）。
--
-- **這是負載夾具，不是可交付的資料。** 與 075 同一個前提：密碼是公開的。
-- =============================================================================

\set ON_ERROR_STOP on

-- **整個檔案在一個交易裡。** `set_config(..., true)` 是交易範圍的，而 psql
-- 的每一個語句各自是一個隱含交易 —— 少了 BEGIN，平台情境只對緊接的那一句
-- 有效，後面每一句都會被 RLS 擋成 0 列（而 INSERT 不會報錯，只是沒插進去）。
-- 009 與 075 都是這個形狀。
BEGIN;

SELECT set_config('app.is_platform', 'on', true);

-- 前置條件：009 的示範租戶與場域必須存在。缺了就停 —— 一個把資料建到
-- 不存在的場域上的夾具，會在負載測試跑到一半才以 500 的形式出現。
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM fms.facilities
                  WHERE id = 'cccccccc-0000-4000-8000-000000000001') THEN
    RAISE EXCEPTION '076 需要 009 的示範租戶與台北總部場域。請先跑 MIGRATE_MODE=demo';
  END IF;
END $$;

-- -----------------------------------------------------------------------------
-- 0. 重設負載帳號建立過的預約
-- -----------------------------------------------------------------------------
-- **重跑這個檔案就是把負載夾具重設回初始狀態，而「還沒有任何預約」是那個
-- 狀態的一部分。**
--
-- 為什麼需要：時段空間是有限的（每個 worker 60 天 × 3 個時段），
-- 而 076 的資料是持久的。連續量測幾次之後，新一次執行會撞到前幾次留下的
-- 預約 —— 實測第六次執行時 `reservations:create` 有 11.3% 是
-- 409 `RESERVATION_CONFLICT`，也就是那一格開始量衝突處理而不是建立成本。
--
-- 只刪負載帳號的預約（`7a000000-` 前綴）。009／075 的示範預約留著 ——
-- 那些是給前端與示範用的，不屬於這個夾具。
--
-- **硬刪除，不是軟刪除。** 排他約束的 WHERE 子句涵蓋未取消的狀態，
-- 而 `deleted_at` 不在其中 —— 軟刪除的列仍然佔著時段，這一段就白做了。
DELETE FROM fms.reservations
 WHERE organizer_id IN (
   SELECT id FROM fms.users WHERE username LIKE 'load%'
 );

-- -----------------------------------------------------------------------------
-- 1. 校舍：1 棟 + 5 層 + 100 間教室
-- -----------------------------------------------------------------------------
-- 100 間平均分佈在 5 層（每層 20 間）。分層不是裝飾：`floor-view` 與
-- 佔用地圖都以樓層為單位查詢，全部塞在同一層會讓那兩支端點的負載
-- 落在一個不真實的形狀上。
INSERT INTO fms.spatial_nodes (id, tenant_id, facility_id, parent_id, node_type_code,
                               code, name, floor_level, floor_label, area_sqm,
                               capacity, is_bookable)
VALUES ('1c000000-0000-4000-8000-000000000000',
        'aaaaaaaa-0000-4000-8000-000000000001',
        'cccccccc-0000-4000-8000-000000000001', NULL,
        'BUILDING', 'BLDG_EDU', '教學大樓', NULL, NULL, 24000, 0, false)
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.spatial_nodes (id, tenant_id, facility_id, parent_id, node_type_code,
                               code, name, floor_level, floor_label, area_sqm,
                               capacity, is_bookable)
SELECT ('1c000000-0000-4000-8000-0000000000' || lpad(f::text, 2, '0'))::uuid,
       'aaaaaaaa-0000-4000-8000-000000000001',
       'cccccccc-0000-4000-8000-000000000001',
       '1c000000-0000-4000-8000-000000000000',
       'FLOOR', 'EDU_' || f || 'F', '教學大樓 ' || f || '樓',
       f, f || 'F', 4800, 0, false
FROM generate_series(1, 5) AS f
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.spatial_nodes (id, tenant_id, facility_id, parent_id, node_type_code,
                               code, name, floor_level, floor_label, area_sqm,
                               capacity, is_bookable)
SELECT ('1c001000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
       'aaaaaaaa-0000-4000-8000-000000000001',
       'cccccccc-0000-4000-8000-000000000001',
       -- 1..20 → 1F、21..40 → 2F …… 100 → 5F
       ('1c000000-0000-4000-8000-0000000000'
        || lpad((((n - 1) / 20) + 1)::text, 2, '0'))::uuid,
       'CLASSROOM',
       'RM_' || lpad(n::text, 3, '0'),
       (((n - 1) / 20) + 1) || lpad((((n - 1) % 20) + 1)::text, 2, '0') || ' 教室',
       (((n - 1) / 20) + 1),
       (((n - 1) / 20) + 1) || 'F',
       -- 三種尺寸交替：容量影響 availability 的篩選，全部一樣會讓那個
       -- 參數在負載中永遠無效。
       (CASE n % 3 WHEN 0 THEN 96 WHEN 1 THEN 60 ELSE 45 END),
       (CASE n % 3 WHEN 0 THEN 60 WHEN 1 THEN 40 ELSE 30 END),
       true
FROM generate_series(1, 100) AS n
ON CONFLICT (id) DO NOTHING;

-- 100 間教室 → 100 個可訂資源。
--
-- `min_notice_minutes = 0`：負載測試要能立刻訂下一個時段。011 的
-- `TOO_LATE` 檢查是 `p_start_at < clock_timestamp() + min_notice`，
-- 給一個正值會讓負載腳本每一次建立都變成 422 —— 那量到的是驗證路徑，
-- 不是預約路徑。
--
-- `requires_approval = false`：需審核的預約停在 PENDING_APPROVAL，
-- 不會進入排他約束，因此**量不到預約真正的鎖競爭**。
INSERT INTO fms.bookable_resources (id, tenant_id, facility_id, resource_type,
                                    spatial_node_id, display_name,
                                    min_duration_minutes, max_duration_minutes,
                                    slot_granularity_minutes, buffer_after_minutes,
                                    advance_booking_days, min_notice_minutes,
                                    capacity, requires_approval, requires_check_in,
                                    auto_release_minutes)
SELECT ('7c000000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
       'aaaaaaaa-0000-4000-8000-000000000001',
       'cccccccc-0000-4000-8000-000000000001',
       'SPATIAL_NODE',
       ('1c001000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
       (((n - 1) / 20) + 1) || lpad((((n - 1) % 20) + 1)::text, 2, '0') || ' 教室',
       50, 200, 10, 10, 90, 0,
       (CASE n % 3 WHEN 0 THEN 60 WHEN 1 THEN 40 ELSE 30 END),
       false,
       -- 五分之一要報到。全部要報到會讓 no-show 掃描器的負載被高估，
       -- 全部不要則那條路徑完全沒有被走到。
       (n % 5 = 0),
       (CASE WHEN n % 5 = 0 THEN 15 ELSE NULL END)
FROM generate_series(1, 100) AS n
ON CONFLICT (id) DO NOTHING;

-- -----------------------------------------------------------------------------
-- 2. 1000 台裝置（每間教室 10 台）+ 1000 個計量點
-- -----------------------------------------------------------------------------
-- 每間教室 10 台，型別輪替。`status` 不是全 ONLINE：離線裝置是
-- `DEVICE_OFFLINE` 類規則與裝置清單篩選的唯一資料來源。
INSERT INTO fms.devices (id, tenant_id, facility_id, spatial_node_id,
                         device_code, name, device_type, status)
SELECT ('7d000000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
       'aaaaaaaa-0000-4000-8000-000000000001',
       'cccccccc-0000-4000-8000-000000000001',
       ('1c001000-0000-4000-8000-' || lpad((((n - 1) / 10) + 1)::text, 12, '0'))::uuid,
       'DEV_' || lpad(n::text, 4, '0'),
       'DEV-' || lpad(n::text, 4, '0'),
       (ARRAY['SENSOR','METER','OCCUPANCY','ENVIRONMENT','CONTROLLER'])[(n % 5) + 1],
       (CASE WHEN n % 50 = 0 THEN 'OFFLINE' ELSE 'ONLINE' END)
FROM generate_series(1, 1000) AS n
ON CONFLICT (id) DO NOTHING;

-- 一台裝置一個點。**`point_code` 在裝置內唯一**，因此一台一個點時
-- code 可以固定；要一台多點就得帶序號。
INSERT INTO fms.telemetry_points (id, tenant_id, device_id, point_code, name,
                                  data_type, unit, valid_min, valid_max)
SELECT ('7e000000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
       'aaaaaaaa-0000-4000-8000-000000000001',
       ('7d000000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
       'POINT_TEMP', '室溫',
       -- `NUMBER`，不是 `NUMERIC`。CHECK 只認 NUMBER／BOOLEAN／STRING／ENUM。
       'NUMBER', '°C', -20, 60
FROM generate_series(1, 1000) AS n
ON CONFLICT (id) DO NOTHING;

-- -----------------------------------------------------------------------------
-- 3. 200 張未結案工單
-- -----------------------------------------------------------------------------
-- 目標一律是教室節點（`ck_wo_target` 要 asset_id 或 spatial_node_id 其一）。
-- **不用 SERVICE 型別**：`ck_wo_service_item` 要求它必須帶 service_item_id。
--
-- 狀態刻意分佈在四個未結案狀態上。全部 SUBMITTED 會讓
-- `available-actions` 每次都回同一組動作，而狀態機的負載形狀就消失了。
INSERT INTO fms.work_orders (id, tenant_id, facility_id, spatial_node_id, wo_no,
                             work_order_type, title, description,
                             priority, status, source, requested_start_at)
SELECT ('7f000000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
       'aaaaaaaa-0000-4000-8000-000000000001',
       'cccccccc-0000-4000-8000-000000000001',
       ('1c001000-0000-4000-8000-' || lpad((((n - 1) % 100) + 1)::text, 12, '0'))::uuid,
       -- 前綴與 WO-2026-NNNNNN 的序號區隔開：負載夾具的列要一眼看得出來，
       -- 而且不會與端點產生的號碼相撞。
       'LOAD-' || lpad(n::text, 6, '0'),
       (ARRAY['MAINTENANCE','INSPECTION','CORRECTIVE','PROJECT'])[(n % 4) + 1],
       '教室設備異常 #' || n,
       '負載夾具（076）。',
       (ARRAY['LOW','MEDIUM','HIGH','URGENT','CRITICAL'])[(n % 5) + 1],
       (ARRAY['SUBMITTED','APPROVED','ASSIGNED','IN_PROGRESS'])[(n % 4) + 1],
       'MANUAL',
       -- 分散在過去 30 天。全部同一時刻會讓 `created_from`／`created_to`
       -- 這類時間篩選在負載中永遠命中全部或全不中。
       clock_timestamp() - ((n % 30) || ' days')::interval
FROM generate_series(1, 200) AS n
ON CONFLICT (id) DO NOTHING;

-- -----------------------------------------------------------------------------
-- 4. 250 個帳號
-- -----------------------------------------------------------------------------
-- 角色分佈按真實校園：多數是老師／職員（REQUESTER），少數技師。
--
--   1..225   REQUESTER    —— 訂教室、報修、看自己的工單
--   226..245 TECHNICIAN   —— 執行工單
--   246..250 FACILITY_ADMIN —— 看儀表板與全部工單
--
-- 全部指派在**台北總部場域**（FACILITY 範圍）。若給 TENANT 範圍，
-- 場域收斂就不會生效，而負載測試量到的 RLS 成本會偏低。
INSERT INTO fms.users (id, tenant_id, primary_org_id, default_facility_id,
                       employee_no, username, email, display_name,
                       user_type, job_title,
                       password_hash, password_updated_at, must_change_password)
SELECT ('7a000000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
       'aaaaaaaa-0000-4000-8000-000000000001',
       'bbbbbbbb-0000-4000-8000-000000000002',
       'cccccccc-0000-4000-8000-000000000001',
       'L' || lpad(n::text, 4, '0'),
       'load' || lpad(n::text, 3, '0'),
       'load' || lpad(n::text, 3, '0') || '@load.bizlution.test',
       '負載使用者 ' || n,
       'EMPLOYEE',
       (CASE WHEN n > 245 THEN '場域管理員'
             WHEN n > 225 THEN '技師'
             ELSE '教職員' END),
       -- 與 075 同一個雜湊（密碼 Demo1234!）。**不依賴 075 的 UPDATE**：
       -- 那支只補 password_hash IS NULL 的列，若模式順序改變就會漏掉這 250 個。
       '$argon2id$v=19$m=19456,t=2,p=1$+mGdGSIpgsm0qS7GZk2LbA$bJwigrRUlRV5RnN4OiBq7Ypmcs3Y4HJNn31/pX22KjU',
       clock_timestamp(),
       false
FROM generate_series(1, 250) AS n
ON CONFLICT (id) DO NOTHING;

INSERT INTO fms.user_role_assignments (tenant_id, user_id, role_id, scope_type,
                                       scope_id, source)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001',
       ('7a000000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
       r.id,
       'FACILITY',
       'cccccccc-0000-4000-8000-000000000001'::uuid,
       'MANUAL'
FROM generate_series(1, 250) AS n
JOIN fms.roles r
  ON r.code = (CASE WHEN n > 245 THEN 'FACILITY_ADMIN'
                    WHEN n > 225 THEN 'TECHNICIAN'
                    ELSE 'REQUESTER' END)
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 4b. 把已派工的工單指給技師
-- -----------------------------------------------------------------------------
-- **沒有這一段，技師視角完全量不到。** TECHNICIAN 只有 `work_order:read_own`
-- （不是 `work_order:read`），所以一張沒有負責人的工單對他來說是 404 ——
-- 那個 404 是正確的授權行為，但它讓「打開一張工單」這個操作在負載測試裡
-- 變成純錯誤路徑。
--
-- 只指派 ASSIGNED 與 IN_PROGRESS 那兩種狀態（100 張）。給 SUBMITTED 的工單
-- 一個負責人在語意上是錯的 —— 還沒核准就有人在做。
-- **序號要在「符合條件的那批」上算，不是在 wo_no 上算。**
-- 第一版寫的是 `substring(wo_no from 6)::int % 20`，而狀態是由
-- `(n % 4) + 1` 決定的 —— ASSIGNED 與 IN_PROGRESS 只落在 n ≡ 2,3 (mod 4)。
-- 那個子集的 `% 20` 只到得了 10 個餘數，於是**只有 10 個技師分到工單**，
-- 另外 10 個的每一次請求都會是 404。自我檢查 (f) 抓到了這件事。
WITH eligible AS (
  SELECT id, (row_number() OVER (ORDER BY wo_no) - 1) % 20 AS idx
    FROM fms.work_orders
   WHERE wo_no LIKE 'LOAD-%' AND status IN ('ASSIGNED', 'IN_PROGRESS')
)
UPDATE fms.work_orders w
   SET assignee_id = ('7a000000-0000-4000-8000-'
                      || lpad((226 + e.idx)::text, 12, '0'))::uuid
  FROM eligible e
 WHERE w.id = e.id;

-- -----------------------------------------------------------------------------
-- 5. 自我檢查
-- -----------------------------------------------------------------------------
-- 每一格都是**精確的數量**，不是「至少」。
-- 「至少一筆」曾經讓一筆落在錯場域的預約通過檢查，而佔用端點全回 FREE
-- （見 075 的教訓）—— 負載夾具若數量不對，量出來的一切都對不上報告。
DO $$
DECLARE
  n_rooms int; n_res int; n_dev int; n_pt int; n_wo int; n_user int;
  n_grant int; n_pwd int; n_status int; n_floor int; n_ingestable int;
  n_assigned int; n_assignees int; n_findable int; n_left int;
BEGIN
  SELECT count(*) INTO n_rooms FROM fms.spatial_nodes
   WHERE node_type_code = 'CLASSROOM' AND code LIKE 'RM_%';
  SELECT count(*) INTO n_res FROM fms.bookable_resources
   WHERE id::text LIKE '7c000000-%';
  SELECT count(*) INTO n_dev FROM fms.devices WHERE device_code LIKE 'DEV_%';
  SELECT count(*) INTO n_pt FROM fms.telemetry_points
   WHERE id::text LIKE '7e000000-%';
  SELECT count(*) INTO n_wo FROM fms.work_orders WHERE wo_no LIKE 'LOAD-%';
  SELECT count(*) INTO n_user FROM fms.users WHERE username LIKE 'load%';

  IF n_rooms <> 100 THEN RAISE EXCEPTION '076：教室 %，預期 100', n_rooms; END IF;
  IF n_res  <> 100 THEN RAISE EXCEPTION '076：可訂資源 %，預期 100', n_res; END IF;
  IF n_dev  <> 1000 THEN RAISE EXCEPTION '076：裝置 %，預期 1000', n_dev; END IF;
  IF n_pt   <> 1000 THEN RAISE EXCEPTION '076：計量點 %，預期 1000', n_pt; END IF;
  IF n_wo   <> 200 THEN RAISE EXCEPTION '076：工單 %，預期 200', n_wo; END IF;
  IF n_user <> 250 THEN RAISE EXCEPTION '076：帳號 %，預期 250', n_user; END IF;

  -- (a) 每個帳號都要有角色。少了角色的帳號登入得了但每一支端點都回 403 ——
  -- 而那會被誤讀成「系統在高負載下開始拒絕請求」。
  SELECT count(*) INTO n_grant
    FROM fms.users u
   WHERE u.username LIKE 'load%'
     AND EXISTS (SELECT 1 FROM fms.user_role_assignments a WHERE a.user_id = u.id);
  IF n_grant <> 250 THEN
    RAISE EXCEPTION '076：只有 % 個負載帳號有角色指派，預期 250', n_grant;
  END IF;

  -- (b) 每個帳號都要有密碼。這一格存在的理由是它真的發生過：
  -- 示範帳號的 password_hash 全是 NULL，而 528 個測試沒有一個看得見。
  SELECT count(*) INTO n_pwd FROM fms.users
   WHERE username LIKE 'load%' AND password_hash IS NOT NULL;
  IF n_pwd <> 250 THEN
    RAISE EXCEPTION '076：只有 % 個負載帳號有密碼，預期 250', n_pwd;
  END IF;

  -- (c) 工單要橫跨四個未結案狀態。全部同一狀態的話 available-actions
  -- 每次都回同一組動作，狀態機的負載形狀就不存在了。
  SELECT count(DISTINCT status) INTO n_status
    FROM fms.work_orders WHERE wo_no LIKE 'LOAD-%';
  IF n_status <> 4 THEN
    RAISE EXCEPTION '076：負載工單只有 % 種狀態，預期 4', n_status;
  END IF;

  -- (d) 教室要真的分佈在 5 層。全部掛在同一層時 floor-view 與佔用地圖
  -- 的負載形狀是錯的，而數量檢查看不出來。
  SELECT count(DISTINCT floor_level) INTO n_floor
    FROM fms.spatial_nodes WHERE code LIKE 'RM_%';
  IF n_floor <> 5 THEN
    RAISE EXCEPTION '076：教室只分佈在 % 層，預期 5', n_floor;
  END IF;

  -- (e) **行為驗證**：batch-ingest 靠 (device_code, point_code) 定位計量點。
  -- 上面的數量檢查各自都可能通過，而兩張表對不上 —— 那時負載腳本的每一次
  -- 讀值上傳都會失敗，而症狀是「吞吐很高、錯誤率 100%」。
  SELECT count(*) INTO n_ingestable
    FROM fms.telemetry_points p
    JOIN fms.devices d ON d.id = p.device_id
   WHERE d.device_code LIKE 'DEV_%' AND p.point_code = 'POINT_TEMP';
  IF n_ingestable <> 1000 THEN
    RAISE EXCEPTION '076：只有 % 個計量點接得上裝置，預期 1000', n_ingestable;
  END IF;

  -- (f) 已派工的工單要真的有負責人，而且每個技師都要分到。
  -- 「總數對」不代表分佈對：全部指給同一個技師時，其他 19 個技師的
  -- 每一次請求都是 404，而那會被誤讀成系統在拒絕請求。
  SELECT count(*) INTO n_assigned
    FROM fms.work_orders
   WHERE wo_no LIKE 'LOAD-%' AND status IN ('ASSIGNED','IN_PROGRESS')
     AND assignee_id IS NOT NULL;
  IF n_assigned <> 100 THEN
    RAISE EXCEPTION '076：只有 % 張已派工工單有負責人，預期 100', n_assigned;
  END IF;

  SELECT count(DISTINCT assignee_id) INTO n_assignees
    FROM fms.work_orders WHERE wo_no LIKE 'LOAD-%' AND assignee_id IS NOT NULL;
  IF n_assignees <> 20 THEN
    RAISE EXCEPTION '076：工單只分給 % 個技師，預期 20', n_assignees;
  END IF;

  -- (h) 負載帳號名下不能有任何預約。少了這一格，「重跑就重設」這句話
  -- 沒有守衛 —— 而排他約束不看 `deleted_at`，軟刪除會讓時段照樣被佔住。
  SELECT count(*) INTO n_left FROM fms.reservations r
    JOIN fms.users u ON u.id = r.organizer_id
   WHERE u.username LIKE 'load%';
  IF n_left <> 0 THEN
    RAISE EXCEPTION '076：負載帳號還有 % 筆預約沒清掉', n_left;
  END IF;

  -- (g) **行為驗證**：`find_bookable` 用的是
  -- `spatial_node_id = $1 OR asset_id = $1`，也就是說 API 的 `resource_id`
  -- 是**底層節點的 id**，不是 bookable_resources 那一列的 id。
  -- 這一格確認 100 個資源都用它們的節點 id 找得到 ——
  -- 少了它，夾具的數量全對而每一次建立預約都會回 404
  -- 「resource is not bookable」，而錯誤訊息指向一個錯的方向。
  SELECT count(*) INTO n_findable
    FROM fms.spatial_nodes sn
   WHERE sn.code LIKE 'RM_%'
     AND EXISTS (SELECT 1 FROM fms.bookable_resources br
                  WHERE (br.spatial_node_id = sn.id OR br.asset_id = sn.id)
                    AND br.is_bookable = true);
  IF n_findable <> 100 THEN
    RAISE EXCEPTION '076：只有 % 間教室用節點 id 找得到可訂資源，預期 100', n_findable;
  END IF;

  RAISE NOTICE '076 通過：100 教室／1000 裝置＋計量點／200 工單（100 張已派給 20 位技師）／250 帳號';
END $$;

COMMIT;
