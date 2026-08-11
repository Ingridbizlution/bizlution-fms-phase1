-- =============================================================================
-- 075（OPTIONAL）— 示範活動資料：讓前端的每一個畫面都不是空的
-- =============================================================================
--
-- 009 建立了組織骨架（2 個場館、7 個使用者、4 個可預約資源、4 台資產），
-- 但**沒有任何活動資料**：交付前實測 `work_orders=0`、`reservations=0`。
--
-- 後果不是「資料少」，而是前端做出來的工單列表、預約行事曆、佔用地圖與
-- 四張報表**全部是空畫面**。他們可以自己打 API 建，但那表示每個工程師
-- 開工前要先寫一套 fixture 腳本 —— 而那套腳本會有七個版本，各自對狀態機
-- 的理解不同。
--
-- -----------------------------------------------------------------------------
-- 這份資料刻意涵蓋「邊界」，不只是「量」
-- -----------------------------------------------------------------------------
-- 一份只有 30 筆 NEW 狀態工單的示範資料，對前端的價值不比 0 筆高多少 ——
-- 他們要驗的是狀態徽章、逾期樣式、空值處理、分頁。因此這裡刻意放進：
--
--   * **16 種工單狀態全部至少一筆**（含 SLA_BREACHED、CANCELLED、ON_HOLD）
--   * **逾期與未逾期的 SLA** —— `response_due_at` 已過而 `first_responded_at`
--     為 NULL 的那幾筆，是「紅色徽章」的唯一資料來源
--   * **可為空的欄位真的有空的** —— 沒有 assignee、沒有 asset、沒有描述的工單。
--     一份每個欄位都填滿的示範資料會讓前端漏掉 null 處理，而那要到真實
--     資料進來才炸
--   * **跨越「現在」的預約** —— 佔用地圖只看此刻正在進行的預約，
--     沒有一筆跨越 now 的話那支端點永遠回 FREE
--   * **一筆私人預約** —— 讓 011 的遮罩在示範環境裡真的看得到效果
--   * **一個週期系列** —— `recurrence_group_id` 的 UI 分組需要它
--
-- -----------------------------------------------------------------------------
-- 為什麼在 SEED 群組而不是 CORE
-- -----------------------------------------------------------------------------
-- `migrate.sh` 的 `CORE` 到處都跑（含生產），`SEED` 只在
-- `MIGRATE_MODE=seed-only`／`all` 時跑。**這份資料絕對不能進生產環境** ——
-- 60 筆假預約與 32 張假工單混進真實資料裡，之後沒有可靠的方法分辨。
--
-- -----------------------------------------------------------------------------
-- 可重跑
-- -----------------------------------------------------------------------------
-- 每一段都以固定的 uuid 前綴 + `ON CONFLICT DO NOTHING` 寫入，與 009 同一個
-- 慣例。**時間欄位相對 `current_date` 計算**，因此重跑會把資料「移到今天」——
-- 那正是想要的行為：一份三個月前產生的示範資料，它的「逾期工單」會全部變成
-- 逾期三個月，而「未來的預約」會全部變成過去的。
--
-- 相對時間的代價：資料內容會隨執行日改變，因此**不要**把這裡的列數或
-- 時間寫進測試斷言。測試有自己的 fixture（`common/mod.rs`）。
-- =============================================================================

BEGIN;

SELECT set_config('app.is_platform', 'on', true);

-- 009 的固定 id，這裡全部沿用。
--   租戶 aaaaaaaa-…0001
--   場館 cccccccc-…0001（台北總部）／…0002（信義影城）
--   使用者 ffffffff-…0001 admin.chen ／…0002 fm.lin ／…0003 tech.liu
--          …0004 tech.wang ／…0005 user.huang
-- 075 自己的列一律用 7500…／7501…／7502… 前綴，與 009 不會撞。

-- -----------------------------------------------------------------------------
-- 0. 密碼 —— 沒有這一段，示範環境**沒有任何人能登入**
-- -----------------------------------------------------------------------------
-- 009 的七個使用者 `password_hash` 全部是 NULL（它們設計上是目錄來源的帳號）。
-- 後果：`make fresh` 之後起服務，`POST /auth/token` 的 password grant 對每一個
-- 帳號都失敗 —— 而那是前端做的第一件事。
--
-- **測試沒有暴露這件事**：`common/mod.rs` 自己呼叫 `password::hash` 設密碼
-- （見該檔的 `TEST_USERS`）。也就是說整個測試套件跑在一份「有密碼」的資料上，
-- 而示範環境跑在一份「沒有密碼」的資料上。兩者的差別從來沒有被任何東西檢查。
--
-- 密碼是 **`Demo1234!`**，對所有示範帳號相同，並寫在
-- `docs/FRONTEND-GETTING-STARTED.md` 裡。
--
-- 雜湊是用專案自己的 `fms_identity::password::hash()` 產生的 argon2id PHC 字串
-- （argon2id / m=19456,t=2,p=1，與生產設定同一組參數）。**寫死一個常數而不是
-- 在這裡即時計算**：PostgreSQL 沒有 argon2 實作，pgcrypto 只有 bcrypt/des/md5，
-- 而 `password::verify` 只認 argon2id —— 用 pgcrypto 產的雜湊會通過 INSERT
-- 但永遠驗不過，那正是最糟的失敗方式。
--
-- 固定的鹽對示範資料是可接受的：密碼本身是公開的。
UPDATE fms.users
   SET password_hash = '$argon2id$v=19$m=19456,t=2,p=1$+mGdGSIpgsm0qS7GZk2LbA$bJwigrRUlRV5RnN4OiBq7Ypmcs3Y4HJNn31/pX22KjU',
       password_updated_at = clock_timestamp(),
       must_change_password = false
 WHERE tenant_id = 'aaaaaaaa-0000-4000-8000-000000000001'
   AND password_hash IS NULL
   AND deleted_at IS NULL;

-- -----------------------------------------------------------------------------
-- 1. 資產：4 → 20
-- -----------------------------------------------------------------------------
-- 資產列表的篩選器（狀態、關鍵度、場館、類別）在只有 4 筆時看不出作用。
-- `warranty_end_date` 刻意有三筆落在未來 60 天內 —— 那是「保固即將到期」
-- 這類提醒畫面的唯一資料來源。
INSERT INTO fms.assets (id, tenant_id, facility_id, category_id, asset_code, name,
                        criticality, status, install_date, warranty_end_date,
                        purchase_cost, vendor_name, health_score)
SELECT
  ('75000000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
  'aaaaaaaa-0000-4000-8000-000000000001'::uuid,
  CASE WHEN n % 3 = 0 THEN 'cccccccc-0000-4000-8000-000000000002'::uuid
       ELSE 'cccccccc-0000-4000-8000-000000000001'::uuid END,
  (SELECT id FROM fms.asset_categories
    WHERE tenant_id IS NULL OR tenant_id = 'aaaaaaaa-0000-4000-8000-000000000001'
    ORDER BY code OFFSET (n % 12) LIMIT 1),
  'DEMO-AST-' || lpad(n::text, 3, '0'),
  (ARRAY['冰水主機','空調箱','排風機','電梯','發電機','消防泵','變壓器','熱泵',
         '污水泵','冷卻塔','UPS','送風機','抽水馬達','電動門','空壓機','照明迴路'])[n],
  (ARRAY['CRITICAL','HIGH','MEDIUM','LOW'])[1 + (n % 4)],
  -- 五種狀態的樣本。合法值是 PLANNED／IN_STORAGE／INSTALLING／OPERATIONAL／
  -- DEGRADED／DOWN／UNDER_MAINTENANCE／DECOMMISSIONED（002 的 CHECK）——
  -- 不是 ACTIVE。第一版寫了 ACTIVE／RETIRED／UNDER_REPAIR，三個都不存在，
  -- 而 CHECK 立刻擋下來：這就是為什麼 seed 要真的跑一次，不能只是寫出來。
  CASE WHEN n = 15 THEN 'DECOMMISSIONED'
       WHEN n = 8  THEN 'UNDER_MAINTENANCE'
       WHEN n = 3  THEN 'DEGRADED'
       WHEN n = 14 THEN 'DOWN'
       WHEN n = 12 THEN 'IN_STORAGE'
       ELSE 'OPERATIONAL' END,
  current_date - ((n * 97) % 2000),
  -- n = 2, 5, 11 落在未來 60 天內
  CASE WHEN n IN (2, 5, 11) THEN current_date + (n * 9)
       ELSE current_date + ((n * 131) % 1400) - 300 END,
  (120000 + n * 37000)::numeric,
  (ARRAY['大金','日立','三菱電機','台達電','西門子'])[1 + (n % 5)],
  -- 健康分數：三筆刻意偏低，供「需關注設備」清單
  CASE WHEN n IN (3, 8, 14) THEN 40 + n ELSE 75 + (n % 25) END
FROM generate_series(1, 16) AS n
ON CONFLICT (id) DO NOTHING;

-- -----------------------------------------------------------------------------
-- 2. 工單：32 筆，16 種狀態全部至少一筆
-- -----------------------------------------------------------------------------
-- `wo_no` 走 `fms.next_document_no()` 而不是自己編：那個函式是規格書定義的
-- 單一入口（001），自己編會產生與真實資料不同形狀的單號，
-- 而前端的排序與搜尋是照那個形狀寫的。
--
-- **`status` 逐一列出而不是隨機**：前端要能穩定地找到「一筆 ON_HOLD 的工單」
-- 來驗那個狀態的畫面。隨機分配會讓那件事每次重跑都不一樣。
INSERT INTO fms.work_orders (
  id, tenant_id, facility_id, wo_no, work_order_type, source, title, description,
  asset_id, spatial_node_id, requester_id, assignee_id, priority, status,
  requested_start_at, scheduled_start_at, actual_start_at, actual_end_at,
  response_due_at, resolution_due_at, first_responded_at, sla_state,
  labor_minutes, close_code, resolution_notes, completed_at, created_at)
SELECT
  ('75010000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
  'aaaaaaaa-0000-4000-8000-000000000001'::uuid,
  CASE WHEN n % 4 = 0 THEN 'cccccccc-0000-4000-8000-000000000002'::uuid
       ELSE 'cccccccc-0000-4000-8000-000000000001'::uuid END,
  fms.next_document_no('aaaaaaaa-0000-4000-8000-000000000001'::uuid, 'WORK_ORDER', 'WO'),
  -- **不用 `SERVICE`**：`ck_wo_service_item` 要求該型別必須有 `service_item_id`，
  -- 而軟性服務的目錄不在這份 seed 的範圍。放進來只會讓整批插入失敗。
  (ARRAY['MAINTENANCE','INSPECTION','CORRECTIVE','PROJECT'])[1 + (n % 4)],
  -- 合法值是 MANUAL／PM_PLAN／IOT_ALARM／RESERVATION／API／IMPORT／
  -- INSPECTION_FINDING（004 的 CHECK）。第一版寫了 `PM_SCHEDULE`，不存在。
  (ARRAY['MANUAL','MANUAL','IOT_ALARM','PM_PLAN','RESERVATION'])[1 + (n % 5)],
  (ARRAY['冰水主機異音','會議室燈具不亮','電梯定期檢查','廁所漏水','空調不冷',
         '門禁讀卡機故障','消防灑水頭滴水','地下室積水','發電機月測','變頻器過熱告警',
         '玻璃帷幕清洗','停車場照明更換','影廳座椅破損','投影機燈泡衰減','排風異味',
         '冷卻塔補水異常'])[1 + (n % 16)] || '（示範 #' || n || '）',
  -- **四筆刻意沒有描述**：前端要能處理 null，而那要有樣本才驗得到。
  CASE WHEN n % 8 = 0 THEN NULL
       ELSE '示範資料。回報時間 ' || (current_date - (n % 45)) || '，由前端示範資料集產生。' END,
  -- 一半掛資產、一半掛空間節點。**`ck_wo_target` 要求至少有一個** ——
  -- 「兩者都空」的工單在這個 schema 裡不存在（一張不知道對象的工單無法派工）。
  -- 因此「沒有資產」在前端看到的是「有地點、沒設備」，不是兩者都空。
  CASE WHEN n % 2 = 0
       THEN ('75000000-0000-4000-8000-' || lpad(((n % 16) + 1)::text, 12, '0'))::uuid END,
  CASE WHEN n % 2 = 1
       THEN (SELECT sn.id FROM fms.spatial_nodes sn
              WHERE sn.tenant_id = 'aaaaaaaa-0000-4000-8000-000000000001'
              ORDER BY sn.code OFFSET (n % 10) LIMIT 1) END,
  'ffffffff-0000-4000-8000-000000000005'::uuid,      -- user.huang 報修
  -- **五筆刻意沒有受理人**：那是「待派工」清單的資料來源。
  CASE WHEN n % 6 = 0 THEN NULL
       WHEN n % 2 = 0 THEN 'ffffffff-0000-4000-8000-000000000003'::uuid   -- tech.liu
       ELSE 'ffffffff-0000-4000-8000-000000000004'::uuid END,             -- tech.wang
  (ARRAY['LOW','MEDIUM','HIGH','URGENT','CRITICAL'])[1 + (n % 5)],
  s.code,
  clock_timestamp() - ((n % 45) * interval '1 day'),
  clock_timestamp() - ((n % 45) * interval '1 day') + interval '4 hours',
  CASE WHEN s.code IN ('IN_PROGRESS','ON_HOLD','COMPLETED','VERIFIED','CLOSED',
                       'WAITING_PARTS','WAITING_VENDOR','SLA_BREACHED')
       THEN clock_timestamp() - ((n % 45) * interval '1 day') + interval '5 hours' END,
  CASE WHEN s.code IN ('COMPLETED','VERIFIED','CLOSED')
       THEN clock_timestamp() - ((n % 45) * interval '1 day') + interval '9 hours' END,
  -- SLA：對每一筆都算，這樣「回應期限」欄位永遠有值可顯示
  clock_timestamp() - ((n % 45) * interval '1 day') + interval '2 hours',
  clock_timestamp() - ((n % 45) * interval '1 day') + interval '1 day',
  -- **SLA_BREACHED 那幾筆刻意留 NULL 的 first_responded_at**：
  -- 「已逾期且從未回應」是最紅的那個狀態，而它需要 NULL 才成立。
  CASE WHEN s.code = 'SLA_BREACHED' THEN NULL
       WHEN s.code IN ('DRAFT','SUBMITTED','PENDING_APPROVAL') THEN NULL
       ELSE clock_timestamp() - ((n % 45) * interval '1 day') + interval '90 minutes' END,
  -- 合法值是 NOT_APPLICABLE／ON_TRACK／AT_RISK／RESPONSE_BREACHED／
  -- RESOLUTION_BREACHED／MET（032）。第一版寫了 BREACHED／PENDING，兩個都不存在。
  --
  -- `RESPONSE_BREACHED` 而不是 `RESOLUTION_BREACHED`：配的是
  -- `first_responded_at IS NULL`，也就是「從未回應」。兩者在前端是不同的徽章。
  CASE WHEN s.code = 'SLA_BREACHED' THEN 'RESPONSE_BREACHED'
       WHEN s.code IN ('COMPLETED','VERIFIED','CLOSED') THEN 'MET'
       WHEN s.code IN ('DRAFT','SUBMITTED') THEN 'NOT_APPLICABLE'
       WHEN n % 7 = 0 THEN 'AT_RISK'
       ELSE 'ON_TRACK' END,
  -- **不是 `CASE … END`（會回 NULL）**：`labor_minutes` 是 NOT NULL DEFAULT 0，
  -- 而顯式傳 NULL 會違反約束 —— DEFAULT 只在「欄位沒出現在 INSERT 裡」時生效。
  -- 這是明確列出欄位清單的 INSERT 常見的一個誤解。
  CASE WHEN s.code IN ('COMPLETED','VERIFIED','CLOSED') THEN 60 + (n * 13) % 240 ELSE 0 END,
  CASE WHEN s.code IN ('COMPLETED','VERIFIED','CLOSED')
       THEN (ARRAY['REPAIRED','REPLACED','ADJUSTED','NO_FAULT_FOUND'])[1 + (n % 4)] END,
  CASE WHEN s.code IN ('COMPLETED','VERIFIED','CLOSED')
       THEN '已處理完成（示範資料）。' END,
  CASE WHEN s.code IN ('COMPLETED','VERIFIED','CLOSED')
       THEN clock_timestamp() - ((n % 45) * interval '1 day') + interval '9 hours' END,
  clock_timestamp() - ((n % 45) * interval '1 day')
FROM generate_series(1, 32) AS n
-- 16 種狀態各兩筆：`n` 對 16 取模去對 `work_order_statuses`，
-- 因此「每一種狀態都有樣本」是結構保證的，不是碰巧。
JOIN LATERAL (
  SELECT code FROM fms.work_order_statuses ORDER BY code OFFSET ((n - 1) % 16) LIMIT 1
) s ON true
ON CONFLICT (id) DO NOTHING;

-- 工單的檢查項目。只給 6 筆工單 —— 執行面的畫面需要「有項目」與
-- 「沒有項目」兩種樣本，全部都有反而少了一種情況。
INSERT INTO fms.work_order_tasks (id, tenant_id, work_order_id, seq, title,
                                  input_type, unit, min_value, max_value,
                                  is_required, result_value, is_pass, completed_at)
SELECT
  ('75020000-0000-4000-8000-' || lpad((w * 10 + t)::text, 12, '0'))::uuid,
  'aaaaaaaa-0000-4000-8000-000000000001'::uuid,
  ('75010000-0000-4000-8000-' || lpad(w::text, 12, '0'))::uuid,
  t,
  (ARRAY['確認電源已隔離','量測出口溫度','檢查皮帶張力','清潔濾網','紀錄運轉電流'])[t],
  -- 合法值是 CHECKBOX／NUMBER／TEXT／PHOTO／SIGNATURE／SELECT（004）。
  -- 第一版寫了 `BOOLEAN`，不存在 —— 勾選項在這個 schema 裡叫 CHECKBOX。
  -- 順帶放進 PHOTO 與 SIGNATURE：那兩種的輸入元件在前端完全不同，
  -- 沒有樣本的話會被漏掉。
  (ARRAY['CHECKBOX','NUMBER','CHECKBOX','PHOTO','NUMBER'])[t],
  CASE WHEN t IN (2, 5) THEN (ARRAY['°C','','','','A'])[t] END,
  CASE WHEN t = 2 THEN 5 WHEN t = 5 THEN 0 END,
  CASE WHEN t = 2 THEN 15 WHEN t = 5 THEN 60 END,
  t <= 3,
  -- 前兩筆工單的項目已完成，其餘留空 —— 「未完成的檢查項」是執行畫面的主體。
  -- `result_value` 是 **jsonb**（不是 text）：數值項存數字、布林項存布林，
  -- 那讓前端不必猜「'true' 是字串還是布林」。
  CASE WHEN w <= 2 THEN
    CASE WHEN t = 2 THEN '8.5'::jsonb WHEN t = 5 THEN '23.1'::jsonb ELSE 'true'::jsonb END
  END,
  CASE WHEN w <= 2 THEN true END,
  CASE WHEN w <= 2 THEN clock_timestamp() - interval '2 days' END
FROM generate_series(1, 6) AS w, generate_series(1, 5) AS t
ON CONFLICT (id) DO NOTHING;

-- -----------------------------------------------------------------------------
-- 3. 預約：前後兩週，跨越「現在」
-- -----------------------------------------------------------------------------
-- **排列方式是為了避開 005 的排除約束**（`excl_reservations_no_overlap`
-- 涵蓋 PENDING_APPROVAL／CONFIRMED／CHECKED_IN）：每個資源每天只有一筆，
-- 起始時刻由資源序號決定。這樣「不重疊」是排列本身保證的，
-- 不依賴任何隨機性 —— 一份會偶爾插入失敗的示範資料比沒有更糟。
INSERT INTO fms.reservations (
  id, tenant_id, facility_id, bookable_resource_id, reservation_no,
  resource_type, resource_id, organizer_id, title, purpose, party_size,
  start_at, end_at, status, approval_required, requires_check_in,
  checked_in_at, is_private, created_via, created_at)
SELECT
  ('75030000-0000-4000-8000-' || lpad((r.rn * 100 + (d + 8))::text, 12, '0'))::uuid,
  'aaaaaaaa-0000-4000-8000-000000000001'::uuid,
  r.facility_id,
  r.id,
  fms.next_document_no('aaaaaaaa-0000-4000-8000-000000000001'::uuid, 'RESERVATION', 'RSV'),
  r.resource_type,
  coalesce(r.spatial_node_id, r.asset_id),
  -- **`d + 8` 而不是 `d`。** `d` 從 −7 開始，而 **Postgres 的 `%` 保留被除數的
  -- 正負號** —— `(1 + (-7)) % 8` 是 −6，不是 2。後果有兩種，而只有一種會報錯：
  --
  --   * `party_size` 變成負數 → `ck_reservation_party` 擋下來（會報錯）
  --   * `ARRAY[...][0]` 或負索引 → **回 NULL 而不報錯**，於是主辦人是 NULL
  --     （NOT NULL 才擋）、標題是 NULL（不擋，靜默變成沒有標題的預約）
  --
  -- 第二種是這裡真正的陷阱：一份「大部分預約沒有標題」的示範資料會通過所有
  -- 約束，而症狀要到前端把它畫出來才看得到。因此每一個模數運算都先加偏移量。
  --
  -- 三個不同的主辦人：「我的預約」篩選器要有東西可篩
  (ARRAY['ffffffff-0000-4000-8000-000000000001',
         'ffffffff-0000-4000-8000-000000000005',
         'ffffffff-0000-4000-8000-000000000002'])[1 + ((r.rn + d + 8) % 3)]::uuid,
  (ARRAY['週會','客戶簡報','技術評審','面談','部門月會','教育訓練','專案討論'])
    [1 + ((r.rn * 3 + d + 8) % 7)],
  CASE WHEN (r.rn + d + 8) % 4 = 0 THEN NULL ELSE '示範資料' END,
  2 + ((r.rn + d + 8) % 8),
  base.start_at,
  base.start_at + interval '1 hour',
  CASE
    -- 今天、跨越此刻的那一筆：**佔用地圖唯一的資料來源**。
    -- 沒有它，`GET /facilities/{id}/occupancy` 永遠全部回 FREE。
    --
    -- `frn`（場館內序號）而不是 `rn`：**每個場館各要有一筆**，
    -- 否則前端打開哪個場館看得到東西變成一件碰運氣的事。
    WHEN d = 0 AND r.frn = 1 THEN 'CHECKED_IN'
    WHEN base.start_at < clock_timestamp() THEN 'COMPLETED'
    WHEN d IN (2, 5) AND r.rn = 2 THEN 'PENDING_APPROVAL'
    WHEN d = 3 AND r.rn = 3 THEN 'NO_SHOW'
    ELSE 'CONFIRMED'
  END,
  d IN (2, 5) AND r.rn = 2,
  r.rn <= 2,
  CASE WHEN d = 0 AND r.frn = 1 THEN clock_timestamp() - interval '20 minutes' END,
  -- 一筆私人預約：讓 011 的遮罩在示範環境裡真的看得到效果
  (d = 1 AND r.rn = 2),
  'WEB',
  base.start_at - interval '3 days'
FROM (
  SELECT b.id, b.facility_id, b.resource_type, b.spatial_node_id, b.asset_id,
         row_number() OVER (ORDER BY b.display_name) AS rn,
         -- **場館內**的序號。跨場館的 `rn` 不夠用：`rn = 1` 依名稱排序是
         -- 「1 廳（210席）」，而那是影城的資源 —— 於是「跨越此刻的預約」
         -- 全部落在影城，而總部的佔用地圖一片 FREE。
         -- 實測發現：CORS 與登入都通了之後，佔用地圖仍然全部回 FREE。
         row_number() OVER (PARTITION BY b.facility_id ORDER BY b.display_name) AS frn
    FROM fms.bookable_resources b
   WHERE b.tenant_id = 'aaaaaaaa-0000-4000-8000-000000000001' AND b.is_bookable
) r
CROSS JOIN generate_series(-7, 7) AS d
CROSS JOIN LATERAL (
  SELECT CASE
           -- 今天那一筆刻意跨越此刻（前 20 分鐘開始）。
           -- 不與同資源其他日期的列重疊：那些相隔整天，而這一筆只有 1 小時。
           WHEN d = 0 AND r.frn = 1 THEN date_trunc('hour', clock_timestamp()) - interval '20 minutes'
           ELSE (current_date + d)::timestamptz + ((8 + r.rn * 2) * interval '1 hour')
         END AS start_at
) base
ON CONFLICT (id) DO NOTHING;

-- 一個週期系列（每週三次）。`recurrence_group_id` 的 UI 分組需要它，
-- 而單筆預約永遠產不出那個欄位。
--
-- 時段刻意選 07:00 —— 上面的排列用的是 10:00 起，因此不會撞到排除約束。
INSERT INTO fms.reservations (
  id, tenant_id, facility_id, bookable_resource_id, reservation_no,
  resource_type, resource_id, organizer_id, title, party_size,
  start_at, end_at, status, requires_check_in, recurrence_group_id, recurrence_rule,
  created_via, created_at)
SELECT
  ('75040000-0000-4000-8000-' || lpad(k::text, 12, '0'))::uuid,
  'aaaaaaaa-0000-4000-8000-000000000001'::uuid,
  r.facility_id, r.id,
  fms.next_document_no('aaaaaaaa-0000-4000-8000-000000000001'::uuid, 'RESERVATION', 'RSV'),
  r.resource_type, coalesce(r.spatial_node_id, r.asset_id),
  'ffffffff-0000-4000-8000-000000000002'::uuid,
  '每週工務協調會',
  6,
  (current_date + (k * 7))::timestamptz + interval '7 hours',
  (current_date + (k * 7))::timestamptz + interval '8 hours',
  'CONFIRMED', true,
  '75040000-0000-4000-8000-0000000000ff'::uuid,
  'FREQ=WEEKLY;BYDAY=MO;COUNT=3',
  'WEB',
  clock_timestamp() - interval '10 days'
FROM generate_series(0, 2) AS k
CROSS JOIN LATERAL (
  SELECT b.id, b.facility_id, b.resource_type, b.spatial_node_id, b.asset_id
    FROM fms.bookable_resources b
   WHERE b.tenant_id = 'aaaaaaaa-0000-4000-8000-000000000001' AND b.is_bookable
   ORDER BY b.display_name LIMIT 1
) r
ON CONFLICT (id) DO NOTHING;

-- -----------------------------------------------------------------------------
-- 4. 告警：6 筆，涵蓋嚴重度與狀態
-- -----------------------------------------------------------------------------
-- 兩筆 ACTIVE + CRITICAL（告警看板的主體）、一筆已確認、一筆已解除、
-- 一筆已開工單（`work_order_id` 有值，供「告警 → 工單」的連結）。
INSERT INTO fms.alarms (id, tenant_id, facility_id, alarm_no, source, severity, status,
                        message, trigger_value, threshold_value, occurrence_count,
                        first_seen_at, last_seen_at, acknowledged_at, acknowledged_by,
                        cleared_at, work_order_id, asset_id)
SELECT
  ('75050000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid,
  'aaaaaaaa-0000-4000-8000-000000000001'::uuid,
  'cccccccc-0000-4000-8000-000000000001'::uuid,
  fms.next_document_no('aaaaaaaa-0000-4000-8000-000000000001'::uuid, 'ALARM', 'ALM'),
  (ARRAY['RULE_ENGINE','BMS','RULE_ENGINE','MANUAL','RULE_ENGINE','BMS'])[n],
  (ARRAY['CRITICAL','CRITICAL','MAJOR','WARNING','MINOR','INFO'])[n],
  (ARRAY['ACTIVE','ACTIVE','ACKNOWLEDGED','ACTIVE','CLEARED','CLOSED'])[n],
  (ARRAY['冰水出水溫度過高','機房濕度超標','排風機電流異常','手動回報異音',
         '冷卻塔水位偏低','例行自檢通過'])[n],
  (ARRAY[14.8, 78.5, 11.2, NULL, 32.0, 1.0])[n],
  (ARRAY[12.0, 65.0, 9.0, NULL, 40.0, 1.0])[n],
  (ARRAY[7, 3, 1, 1, 2, 1])[n],
  clock_timestamp() - (n * interval '5 hours'),
  clock_timestamp() - (n * interval '12 minutes'),
  CASE WHEN n IN (3, 5, 6) THEN clock_timestamp() - (n * interval '3 hours') END,
  CASE WHEN n IN (3, 5, 6) THEN 'ffffffff-0000-4000-8000-000000000002'::uuid END,
  CASE WHEN n IN (5, 6) THEN clock_timestamp() - (n * interval '2 hours') END,
  -- 第 3 筆已開工單：供前端驗「告警 → 工單」的跳轉
  CASE WHEN n = 3 THEN '75010000-0000-4000-8000-000000000001'::uuid END,
  ('75000000-0000-4000-8000-' || lpad(n::text, 12, '0'))::uuid
FROM generate_series(1, 6) AS n
ON CONFLICT (id) DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證：每一個畫面真的有東西
-- -----------------------------------------------------------------------------
-- 這幾格不是形式。這份 seed 的**全部價值**就是「前端的畫面不是空的」，
-- 而那件事只能用列數驗。少了自我驗證，一個因為約束衝突而插入 0 筆的
-- seed 會靜默成功 —— 而症狀是三個月後有人問「為什麼示範環境沒有資料」。
DO $$
DECLARE
  v_wo      int;
  v_status  int;
  v_breach  int;
  v_rsv     int;
  v_now     int;
  v_private int;
  v_recur   int;
  v_alarm   int;
  v_asset   int;
BEGIN
  -- **最先驗這一格。** 其他每一格驗的都是「畫面有資料」，而這一格驗的是
  -- 「前端能不能登入」—— 登不進去的話後面全部都看不到。
  IF EXISTS (SELECT 1 FROM fms.users
              WHERE tenant_id = 'aaaaaaaa-0000-4000-8000-000000000001'
                AND deleted_at IS NULL AND password_hash IS NULL) THEN
    RAISE EXCEPTION
      '075 FAILED: 還有示範帳號沒有密碼 —— password grant 會對它們全部失敗，'
      '而那是前端做的第一件事';
  END IF;

  SELECT count(*) INTO v_wo FROM fms.work_orders WHERE deleted_at IS NULL;
  IF v_wo < 30 THEN
    RAISE EXCEPTION '075 FAILED: 只有 % 張工單 —— 工單列表仍然近乎空白', v_wo;
  END IF;

  -- 16 種狀態全部有樣本。前端要驗每一種狀態徽章。
  SELECT count(DISTINCT status) INTO v_status FROM fms.work_orders;
  IF v_status < 16 THEN
    RAISE EXCEPTION
      '075 FAILED: 工單只涵蓋 % 種狀態（共 16 種）—— 有狀態徽章驗不到', v_status;
  END IF;

  -- 逾期且從未回應的那幾筆是「最紅的徽章」的唯一資料來源。
  SELECT count(*) INTO v_breach FROM fms.work_orders
   WHERE sla_state = 'RESPONSE_BREACHED' AND first_responded_at IS NULL;
  IF v_breach = 0 THEN
    RAISE EXCEPTION '075 FAILED: 沒有「已逾期且從未回應」的工單 —— 逾期樣式驗不到';
  END IF;

  SELECT count(*) INTO v_rsv FROM fms.reservations;
  IF v_rsv < 40 THEN
    RAISE EXCEPTION '075 FAILED: 只有 % 筆預約 —— 行事曆仍然近乎空白', v_rsv;
  END IF;

  -- **佔用地圖：每一個有可預約資源的場館都要有一筆跨越此刻的預約。**
  --
  -- 第一版只驗「存在至少一筆」，而那太弱：那一筆落在影城，於是總部的佔用地圖
  -- 一片 FREE 而自我驗證照樣通過。實測（真的打那支端點）才發現。
  -- 「至少有一個」與「每一個都有」的差別，在示範資料裡就是「碰運氣」與
  -- 「打開哪個場館都看得到」的差別。
  SELECT count(*) INTO v_now
    FROM (SELECT DISTINCT b.facility_id
            FROM fms.bookable_resources b
           WHERE b.tenant_id = 'aaaaaaaa-0000-4000-8000-000000000001' AND b.is_bookable) f
   WHERE NOT EXISTS (
     SELECT 1 FROM fms.reservations r
       JOIN fms.bookable_resources b2 ON b2.id = r.bookable_resource_id
      WHERE b2.facility_id = f.facility_id
        AND r.status = 'CHECKED_IN'
        AND clock_timestamp() >= r.start_at AND clock_timestamp() < r.end_at);
  IF v_now > 0 THEN
    RAISE EXCEPTION
      '075 FAILED: 有 % 個場館沒有跨越此刻的 CHECKED_IN 預約 —— '
      '那些場館的 GET /facilities/{id}/occupancy 會全部回 FREE', v_now;
  END IF;

  -- 供下方 NOTICE 用：實際的進行中筆數。
  SELECT count(*) INTO v_now FROM fms.reservations
   WHERE status = 'CHECKED_IN'
     AND clock_timestamp() >= start_at AND clock_timestamp() < end_at;

  SELECT count(*) INTO v_private FROM fms.reservations WHERE is_private;
  IF v_private = 0 THEN
    RAISE EXCEPTION '075 FAILED: 沒有私人預約 —— 011 的遮罩在示範環境看不到效果';
  END IF;

  SELECT count(DISTINCT recurrence_group_id) INTO v_recur
    FROM fms.reservations WHERE recurrence_group_id IS NOT NULL;
  IF v_recur = 0 THEN
    RAISE EXCEPTION '075 FAILED: 沒有週期系列 —— recurrence_group_id 的分組 UI 驗不到';
  END IF;

  SELECT count(*) INTO v_alarm FROM fms.alarms WHERE status = 'ACTIVE';
  IF v_alarm < 2 THEN
    RAISE EXCEPTION '075 FAILED: 只有 % 筆 ACTIVE 告警 —— 告警看板近乎空白', v_alarm;
  END IF;

  SELECT count(*) INTO v_asset FROM fms.assets WHERE deleted_at IS NULL;
  IF v_asset < 18 THEN
    RAISE EXCEPTION '075 FAILED: 只有 % 台資產 —— 篩選器看不出作用', v_asset;
  END IF;

  RAISE NOTICE '075 OK：工單 %（% 種狀態、% 筆已逾期未回應）、預約 %'
               '（% 筆進行中、% 筆私人、% 個週期系列）、告警 % 筆 ACTIVE、資產 % 台',
               v_wo, v_status, v_breach, v_rsv, v_now, v_private, v_recur, v_alarm, v_asset;
END;
$$;

COMMIT;
