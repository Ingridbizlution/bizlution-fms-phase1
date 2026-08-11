-- =============================================================================
-- 065：四支報表函式（group-rollup、asset-reliability、space-utilization、
--      service-volume）
-- =============================================================================
-- 形狀比照 034 的 `report_sla_compliance` 與 063 的 `report_pm_compliance`：
-- SQL 函式 + 薄 repo、分母分開、排除項具名。`SECURITY INVOKER`，
-- 所以場域範圍的使用者只會算到自己看得見的列（RLS 完成收斂，
-- 應用層不再過濾一次 —— 兩份實作最後總會分歧）。
--
-- -----------------------------------------------------------------------------
-- 每一支的分母都有一個「看起來對但會說謊」的版本，這裡記下差別
-- -----------------------------------------------------------------------------
--
-- **group-rollup**：`facilities.org_id` → `organizations.org_path` 的 ltree
-- 子樹。用 `<@` 而不是 `parent_id` 逐層爬：後者在中間層會漏掉孫節點的設施，
-- 於是「集團」那一列的數字比它底下各分公司的總和還小。
--
-- **asset-reliability**：MTBF 與 MTTR **來源不同，不能混**。
--   * MTTR 來自工單（`completed_at - created_at`）—— 修復花了多久。
--   * MTBF 來自 `asset_status_history` 的故障間隔 —— 兩次故障之間撐了多久。
--
-- 而 `asset_status_history` 的第一列是 migration 064 裝上觸發器之後才出現的。
-- 在那之前設備的故障歷史**不存在**，所以任何橫跨 064 之前的 MTBF 都是
-- 「我們開始記錄之後的 MTBF」，不是設備真正的 MTBF。
--
-- 函式因此回傳 `history_since` —— 那個時刻早於查詢區間時，讀者才知道
-- 這個數字涵蓋了完整區間。少了它，一個剛裝好記錄的系統會回報極長的 MTBF
-- （因為還沒有第二次故障），而那看起來像設備很可靠。
--
-- **space-utilization**：no-show 的分母**只含 `requires_check_in` 的預約**。
-- 不需要報到的預約永遠不可能是 no-show，把它算進分母會系統性地壓低那個比率
-- —— 而那個數字的用途正是「該不該對 no-show 收費」。
--
-- 而那個旗標要讀**預約自己的那一份**（`reservations.requires_check_in`），
-- 不是資源上的（`bookable_resources.requires_check_in`）。理由是分子的來源：
-- `fms-reservation/src/no_show.rs` 的掃描器只把 `r.requires_check_in` 的預約
-- 標成 `NO_SHOW`，那一欄是建立預約時從資源快照下來的。分母改讀資源上的現值，
-- 分子與分母就來自兩個不同的地方 —— 事後把資源的旗標打開，會把一批**永遠
-- 不可能變成 NO_SHOW** 的舊預約拉進分母，稀釋掉這個比率。
--
-- 使用率的分母是**可預約時數**而不是 24 小時：一間只在上班時間開放的會議室
-- 用 24 小時當分母，永遠不會超過 35%，於是那個百分比無法用來判斷「要不要再
-- 隔一間會議室」。
--
-- 時數的形狀是 038 定下的那一個：`{"mon": [["08:00","21:00"]], ...}`，
-- 星期鍵 → 時段陣列。**不是** `{"hours_per_day": n}` —— 第一版寫成後者，
-- 於是 `?  'hours_per_day'` 永遠是 false，每一列都落到 `assumed_24h`，
-- 整個「分母是可預約時數」的決定變成一段沒有效果的程式碼。
-- （這正是本專案反覆出現的那類缺陷：宣告了、但沒有人真的讀得到。）
--
-- 解析順序：資源自己的 `opening_hours` > 場域的 `fms.business_windows()`
-- （它連假日行事曆一起解，所以休館日不算進分母）> 24 小時並在 `hours_basis`
-- 標成 `assumed_24h`。`assumed_24h` 那幾列的百分比不可與其他列比較。
--
-- **service-volume**：`is_chargeback` 與 `chargeback_org_id` 已經有讀者
-- （`fms-workorder` 的 DTO 與 repo），所以「可 chargeback」不是新概念。
-- 成本分成 labor／parts／other 三欄回傳而不是一個總和 ——
-- 那三者的爭議點不同（工時費率、料件單價、外包發票），一個總和無法對帳。
--
-- **`labor_cost` 為 NULL 的工單單獨計數**（`work_orders_without_rate`）。
-- 那是 #25 記下的取捨：沒有費率來源時 `cost` 留 NULL 而不是 0。
-- 把 NULL 當 0 加總會讓帳單金額安靜地偏低，所以這裡把「有幾張工單的工時
-- 成本未知」變成一個看得見的數字。
-- =============================================================================

BEGIN;

-- -----------------------------------------------------------------------------
-- (1) group-rollup
-- -----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.report_group_rollup(
  p_from       date,
  p_to         date,
  p_subtree_of uuid DEFAULT NULL   -- 只算這個組織的子樹（含自己）
) RETURNS TABLE (
  org_id              uuid,
  org_name            text,
  org_path            text,
  depth               int,
  facility_count      bigint,
  -- 子樹彙總：含所有下層組織的設施。
  work_orders_total   bigint,
  work_orders_open    bigint,
  work_orders_overdue bigint,
  pm_scheduled        bigint,
  pm_on_time          bigint,
  total_cost          double precision,
  chargeback_cost     double precision
)
LANGUAGE sql STABLE
AS $$
  WITH scope AS (
    SELECT o.id, o.name::text AS name, o.org_path,
           (nlevel(o.org_path) - 1)::int AS depth
      FROM fms.organizations o
     WHERE p_subtree_of IS NULL
        OR o.org_path <@ (SELECT p.org_path FROM fms.organizations p
                           WHERE p.id = p_subtree_of)
  ), fac AS (
    -- 每個組織對應的設施：**子樹**而不是直屬。用 `<@` 而不是逐層爬 —— 見檔頭。
    SELECT s.id AS org_id, f.id AS facility_id
      FROM scope s
      JOIN fms.organizations o2 ON o2.org_path <@ s.org_path
      JOIN fms.facilities f ON f.org_id = o2.id AND f.deleted_at IS NULL
  ), wo AS (
    SELECT fa.org_id,
           count(*) AS total,
           count(*) FILTER (WHERE st.is_terminal IS NOT TRUE) AS open_count,
           count(*) FILTER (WHERE w.resolution_due_at < now()
                              AND st.is_terminal IS NOT TRUE) AS overdue,
           sum(coalesce(w.labor_cost, 0) + coalesce(w.parts_cost, 0)
               + coalesce(w.other_cost, 0)) AS cost,
           sum(CASE WHEN w.is_chargeback
                    THEN coalesce(w.labor_cost, 0) + coalesce(w.parts_cost, 0)
                         + coalesce(w.other_cost, 0) END) AS chargeback
      FROM fac fa
      JOIN fms.work_orders w ON w.facility_id = fa.facility_id
                            AND w.deleted_at IS NULL
                            AND w.created_at >= p_from
                            AND w.created_at < (p_to + 1)
      LEFT JOIN fms.work_order_statuses st ON st.code = w.status
     GROUP BY fa.org_id
  ), pm AS (
    SELECT fa.org_id,
           count(*) FILTER (WHERE o.completed_at IS NOT NULL
                               OR now() > o.scheduled_for
                                  + make_interval(days => mp.completion_grace_days))
             AS scheduled,
           count(*) FILTER (WHERE o.completed_at IS NOT NULL
                              AND o.completed_at <= o.scheduled_for
                                  + make_interval(days => mp.completion_grace_days))
             AS on_time
      FROM fac fa
      JOIN fms.maintenance_plans mp ON mp.facility_id = fa.facility_id
      JOIN fms.maintenance_occurrences o ON o.plan_id = mp.id
     WHERE o.scheduled_for >= p_from AND o.scheduled_for < (p_to + 1)
     GROUP BY fa.org_id
  )
  SELECT s.id, s.name, s.org_path::text, s.depth,
         (SELECT count(DISTINCT fa.facility_id) FROM fac fa WHERE fa.org_id = s.id),
         coalesce(wo.total, 0), coalesce(wo.open_count, 0), coalesce(wo.overdue, 0),
         coalesce(pm.scheduled, 0), coalesce(pm.on_time, 0),
         coalesce(wo.cost, 0)::float8, coalesce(wo.chargeback, 0)::float8
    FROM scope s
    LEFT JOIN wo ON wo.org_id = s.id
    LEFT JOIN pm ON pm.org_id = s.id
   ORDER BY s.org_path;
$$;

COMMENT ON FUNCTION fms.report_group_rollup(date, date, uuid) IS
  '集團跨組織彙總。每一列是**子樹**的總和（ltree `<@`），不是直屬設施 —— '
  '逐層爬會讓中間層漏掉孫節點的設施，於是集團那一列比底下各分公司的總和還小。';

-- -----------------------------------------------------------------------------
-- (2) asset-reliability
-- -----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.report_asset_reliability(
  p_from        date,
  p_to          date,
  p_facility_id uuid DEFAULT NULL,
  p_limit       int  DEFAULT 50
) RETURNS TABLE (
  asset_id            uuid,
  asset_code          text,
  asset_name          text,
  facility_id         uuid,
  criticality         text,
  -- 故障次數：進入 DOWN／DEGRADED 的次數（來自 064 的歷程）。
  failure_count       bigint,
  corrective_orders   bigint,
  -- MTTR：修復花多久（來自工單）。
  mttr_hours          double precision,
  -- MTBF：兩次故障之間撐多久（來自歷程）。**只有兩次以上故障才算得出來。**
  mtbf_hours          double precision,
  downtime_hours      double precision,
  repair_cost         double precision,
  -- 歷程的起點。早於 p_from 時這一列的 MTBF 才涵蓋完整區間 —— 見檔頭。
  history_since       timestamptz
)
LANGUAGE sql STABLE
AS $$
  WITH failures AS (
    -- 進入故障狀態的每一次。`from_status` 不是故障而 `to_status` 是 ——
    -- 只看 to_status 會把 DOWN→DEGRADED 也算成一次新故障。
    SELECT h.asset_id, h.changed_at,
           lead(h.changed_at) OVER (PARTITION BY h.asset_id ORDER BY h.changed_at)
             AS next_change
      FROM fms.asset_status_history h
     WHERE h.to_status IN ('DOWN', 'DEGRADED')
       AND (h.from_status IS NULL OR h.from_status NOT IN ('DOWN', 'DEGRADED'))
       AND h.changed_at >= p_from AND h.changed_at < (p_to + 1)
  ), fail_agg AS (
    SELECT asset_id,
           count(*) AS failure_count,
           -- 故障間隔的平均。`count(*) > 1` 才有間隔可算 ——
           -- 一次故障算不出 MTBF，回 NULL 而不是 0（0 會看起來像「一直在壞」）。
           CASE WHEN count(*) > 1 THEN
             extract(epoch FROM (max(changed_at) - min(changed_at)))
               / 3600.0 / (count(*) - 1)
           END AS mtbf_hours,
           sum(extract(epoch FROM (coalesce(next_change, now()) - changed_at)))
             / 3600.0 AS downtime_hours
      FROM failures GROUP BY asset_id
  ), repairs AS (
    SELECT w.asset_id,
           count(*) AS corrective_orders,
           avg(extract(epoch FROM (w.completed_at - w.created_at))) / 3600.0
             AS mttr_hours,
           sum(coalesce(w.labor_cost, 0) + coalesce(w.parts_cost, 0)
               + coalesce(w.other_cost, 0)) AS repair_cost
      FROM fms.work_orders w
     WHERE w.work_order_type = 'CORRECTIVE'
       AND w.deleted_at IS NULL
       AND w.completed_at IS NOT NULL
       AND w.created_at >= p_from AND w.created_at < (p_to + 1)
       AND w.asset_id IS NOT NULL
     GROUP BY w.asset_id
  )
  SELECT a.id, a.asset_code::text, a.name::text, a.facility_id, a.criticality,
         coalesce(f.failure_count, 0), coalesce(r.corrective_orders, 0),
         r.mttr_hours::float8, f.mtbf_hours::float8,
         coalesce(f.downtime_hours, 0)::float8,
         coalesce(r.repair_cost, 0)::float8,
         (SELECT min(h2.changed_at) FROM fms.asset_status_history h2
           WHERE h2.asset_id = a.id)
    FROM fms.assets a
    LEFT JOIN fail_agg f ON f.asset_id = a.id
    LEFT JOIN repairs r ON r.asset_id = a.id
   WHERE a.deleted_at IS NULL
     AND (p_facility_id IS NULL OR a.facility_id = p_facility_id)
     -- 只回有事發生過的設備。全部設備都回會讓 Top N 被一堆 0 淹沒。
     AND (f.failure_count IS NOT NULL OR r.corrective_orders IS NOT NULL)
   ORDER BY coalesce(f.failure_count, 0) DESC,
            coalesce(f.downtime_hours, 0) DESC,
            a.asset_code
   LIMIT greatest(p_limit, 1);
$$;

COMMENT ON FUNCTION fms.report_asset_reliability(date, date, uuid, int) IS
  'MTBF／MTTR／故障 Top N。MTBF 來自 asset_status_history（064 之後才有資料，'
  '所以回傳 history_since），MTTR 來自工單。一次故障算不出 MTBF → NULL 而非 0。';

-- -----------------------------------------------------------------------------
-- (3) space-utilization
-- -----------------------------------------------------------------------------
-- 先補一個 038 缺的工具：把一組時段加總成小時。
--
-- `'24:00'::time` 是合法的（PostgreSQL 的 time 上界），而 038 的形狀驗證
-- 明確允許它，所以 `'24:00' - '08:00'` = 16 小時這條路徑要走得通。
CREATE OR REPLACE FUNCTION fms.time_windows_hours(p jsonb)
RETURNS numeric
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT coalesce(sum(
           extract(epoch FROM ((w ->> 1)::time - (w ->> 0)::time)) / 3600.0
         ), 0)::numeric
    FROM jsonb_array_elements(
           CASE WHEN jsonb_typeof(p) = 'array' THEN p ELSE '[]'::jsonb END) w;
$$;

COMMENT ON FUNCTION fms.time_windows_hours(jsonb) IS
  '一組 [["08:00","21:00"], ...] 共幾小時。038 定了形狀與解析，'
  '但沒有人需要「總時數」—— 065 讓它成為使用率的分母。';

-- `bookable_resources.opening_hours` 從這支 migration 起決定一個報表分母，
-- 因此它的形狀開始有後果 —— 理由與 038 對 `facilities.operating_hours`
-- 加約束時一樣。壞掉的值該在寫入時擋，而不是在讀報表時讓 `::time` 炸掉。
-- 既有列的預設是 `{}`，通得過。
ALTER TABLE fms.bookable_resources
  DROP CONSTRAINT IF EXISTS ck_bookable_opening_hours;
ALTER TABLE fms.bookable_resources
  ADD CONSTRAINT ck_bookable_opening_hours
  CHECK (fms.operating_hours_are_valid(opening_hours));

CREATE OR REPLACE FUNCTION fms.report_space_utilization(
  p_from        date,
  p_to          date,
  p_facility_id uuid DEFAULT NULL
) RETURNS TABLE (
  resource_id        uuid,
  resource_name      text,
  resource_type      text,
  facility_id        uuid,
  capacity           int,
  reservations_total bigint,
  booked_hours       double precision,
  -- 分母：可預約時數（見檔頭），不是 24 小時。
  available_hours    double precision,
  utilization_rate   double precision,
  hours_basis        text,
  -- no-show 的分母**只含需要報到的預約**，而且讀的是預約自己的那一份旗標
  -- （與 no_show.rs 的掃描器同一欄）—— 見檔頭。
  checkin_required   bigint,
  no_shows           bigint,
  no_show_rate       double precision,
  cancelled          bigint
)
LANGUAGE sql STABLE
AS $$
  WITH res AS (
    SELECT r.bookable_resource_id AS rid,
           count(*) AS total,
           sum(extract(epoch FROM (r.end_at - r.start_at))) / 3600.0 AS booked_hours,
           -- **預約自己的旗標**，不是資源上的現值。與 no_show.rs 的掃描器
           -- 讀同一欄，否則分子與分母來自兩個地方 —— 見檔頭。
           count(*) FILTER (WHERE r.requires_check_in) AS checkin_required,
           count(*) FILTER (WHERE r.status = 'NO_SHOW') AS no_shows
      FROM fms.reservations r
     WHERE r.start_at >= p_from AND r.start_at < (p_to + 1)
       -- 取消的不佔用時段，所以不進 booked_hours；但要單獨計數。
       AND r.status NOT IN ('CANCELLED', 'REJECTED', 'EXPIRED')
     GROUP BY r.bookable_resource_id
  ), cancels AS (
    SELECT r.bookable_resource_id AS rid, count(*) AS n
      FROM fms.reservations r
     WHERE r.start_at >= p_from AND r.start_at < (p_to + 1)
       AND r.status IN ('CANCELLED', 'REJECTED')
     GROUP BY r.bookable_resource_id
  ), days AS (
    -- 逐日累加而不是「每日時數 × 天數」：星期不同時數不同，而場域的解析
    -- 還會把休館日算成 0 —— 乘法做不到這兩件事。
    SELECT d::date AS day,
           (ARRAY['mon','tue','wed','thu','fri','sat','sun'])[
             extract(isodow FROM d)::int] AS dow_key
      FROM generate_series(p_from::timestamp, p_to::timestamp, interval '1 day') d
  ), hours AS (
    SELECT br.id AS rid,
           sum(fms.time_windows_hours(br.opening_hours -> d.dow_key)) AS resource_hours,
           -- 場域那一層走 038 的解析器，所以假日行事曆一起生效。
           sum(fms.time_windows_hours(fms.business_windows(br.facility_id, d.day)))
             AS facility_hours,
           count(*) AS days
      FROM fms.bookable_resources br
      CROSS JOIN days d
     WHERE br.is_bookable
     GROUP BY br.id
  ), basis AS (
    -- 解析順序：資源 > 場域 > 假設 24 小時。用「> 0」而不是「有沒有設定」
    -- 判斷 —— 一個所有星期都空的 opening_hours 等於沒設定。
    SELECT h.rid,
           CASE WHEN h.resource_hours > 0 THEN h.resource_hours
                WHEN h.facility_hours > 0 THEN h.facility_hours
                ELSE 24 * h.days END AS available_hours,
           CASE WHEN h.resource_hours > 0 THEN 'resource.opening_hours'
                WHEN h.facility_hours > 0 THEN 'facility.operating_hours'
                ELSE 'assumed_24h' END AS basis
      FROM hours h
  )
  SELECT br.id, br.display_name::text, br.resource_type, br.facility_id,
         br.capacity,
         coalesce(res.total, 0),
         coalesce(res.booked_hours, 0)::float8,
         b.available_hours::float8,
         CASE WHEN b.available_hours > 0
              THEN (coalesce(res.booked_hours, 0) / b.available_hours)::float8 END,
         b.basis,
         coalesce(res.checkin_required, 0),
         coalesce(res.no_shows, 0),
         -- 分母是 checkin_required 而不是 total。0 時回 NULL 而不是 0 ——
         -- 「沒有任何需要報到的預約」與「都報到了」是不同的事。
         CASE WHEN coalesce(res.checkin_required, 0) > 0
              THEN (res.no_shows::numeric / res.checkin_required)::float8 END,
         coalesce(cancels.n, 0)
    FROM fms.bookable_resources br
    JOIN basis b ON b.rid = br.id
    LEFT JOIN res ON res.rid = br.id
    LEFT JOIN cancels ON cancels.rid = br.id
   WHERE br.is_bookable
     AND (p_facility_id IS NULL OR br.facility_id = p_facility_id)
   ORDER BY coalesce(res.booked_hours, 0) DESC, br.display_name;
$$;

COMMENT ON FUNCTION fms.report_space_utilization(date, date, uuid) IS
  '空間使用率與 No-show。使用率的分母是可預約時數（hours_basis 說出來源），'
  '不是 24 小時；no-show 的分母只含 requires_check_in 的預約 —— '
  '不需要報到的預約永遠不可能是 no-show，算進去會系統性壓低那個比率。';

-- -----------------------------------------------------------------------------
-- (4) service-volume
-- -----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.report_service_volume(
  p_from     date,
  p_to       date,
  p_group_by text DEFAULT 'service_item'  -- 'service_item' | 'facility' | 'org'
) RETURNS TABLE (
  group_key               text,
  group_label             text,
  requests                bigint,
  completed               bigint,
  labor_minutes           bigint,
  labor_cost              double precision,
  parts_cost              double precision,
  other_cost              double precision,
  chargeback_requests     bigint,
  chargeback_cost         double precision,
  -- **工時成本未知的張數。** 見檔頭：把 NULL 當 0 加總會讓帳單安靜地偏低。
  work_orders_without_rate bigint
)
LANGUAGE sql STABLE
AS $$
  WITH scoped AS (
    SELECT w.*,
           si.name::text AS service_item_name,
           si.code::text AS service_item_code,
           f.name::text AS facility_name,
           o.name::text AS org_name
      FROM fms.work_orders w
      LEFT JOIN fms.service_items si ON si.id = w.service_item_id
      LEFT JOIN fms.facilities f ON f.id = w.facility_id
      LEFT JOIN fms.organizations o ON o.id = w.chargeback_org_id
     WHERE w.deleted_at IS NULL
       AND w.work_order_type = 'SERVICE'
       AND w.created_at >= p_from AND w.created_at < (p_to + 1)
  ), keyed AS (
    SELECT s.*,
           CASE p_group_by
             WHEN 'facility' THEN s.facility_id::text
             WHEN 'org'      THEN s.chargeback_org_id::text
             ELSE s.service_item_id::text
           END AS grp_key,
           CASE p_group_by
             WHEN 'facility' THEN s.facility_name
             WHEN 'org'      THEN s.org_name
             ELSE coalesce(s.service_item_code || ' ' || s.service_item_name,
                           '(未指定服務項目)')
           END AS grp_label
      FROM scoped s
  )
  SELECT k.grp_key, max(k.grp_label),
         count(*)::bigint,
         count(*) FILTER (WHERE k.completed_at IS NOT NULL)::bigint,
         coalesce(sum(k.labor_minutes), 0)::bigint,
         coalesce(sum(k.labor_cost), 0)::float8,
         coalesce(sum(k.parts_cost), 0)::float8,
         coalesce(sum(k.other_cost), 0)::float8,
         count(*) FILTER (WHERE k.is_chargeback)::bigint,
         coalesce(sum(CASE WHEN k.is_chargeback
                           THEN coalesce(k.labor_cost, 0) + coalesce(k.parts_cost, 0)
                                + coalesce(k.other_cost, 0) END), 0)::float8,
         -- 有工時但沒有成本 → 費率未知。**不是免費。**
         count(*) FILTER (WHERE k.labor_minutes > 0
                            AND coalesce(k.labor_cost, 0) = 0)::bigint
    FROM keyed k
   GROUP BY k.grp_key
   ORDER BY 3 DESC NULLS LAST, 1;
$$;

COMMENT ON FUNCTION fms.report_service_volume(date, date, text) IS
  '軟性服務量與成本。三種成本分開回傳（爭議點不同，總和無法對帳）；'
  'work_orders_without_rate 是「有工時但工時成本為 0」的張數 —— '
  '那代表費率未知而不是免費，把 NULL 當 0 加總會讓帳單安靜地偏低。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構檢查；**數字正確性由 reporting_four_slice.rs 驗**（需要工單、預約、
-- 歷程資料，而這支 migration 跑在 seed 009 之前）。
--
-- 取原始碼時**先剝掉 `--` 註解**。理由是量出來的：把 MTBF 的守衛
-- `CASE WHEN count(*) > 1` 換成 `greatest(count(*) - 1, 1)` 之後，這裡的
-- 檢查仍然通過 —— 因為那個字串還留在守衛上方的註解裡。一個能被註解滿足的
-- 結構檢查等於沒有檢查（而它會安靜地一直是綠的）。
DO $$
DECLARE v_src text;
BEGIN
  -- (1) group-rollup 必須用 ltree 子樹，不是 parent_id 逐層。
  SELECT regexp_replace(prosrc, '--[^' || chr(10) || ']*', '', 'g') INTO v_src
    FROM pg_proc
   WHERE proname = 'report_group_rollup' AND pronamespace = 'fms'::regnamespace;
  IF v_src NOT LIKE '%org_path <@%' THEN
    RAISE EXCEPTION
      '065 FAILED: group_rollup 沒有用 ltree 子樹 —— '
      '逐層爬會讓中間層漏掉孫節點的設施，集團那一列會比底下的總和還小';
  END IF;

  -- (2) MTBF 一次故障要回 NULL 而不是 0。
  SELECT regexp_replace(prosrc, '--[^' || chr(10) || ']*', '', 'g') INTO v_src
    FROM pg_proc
   WHERE proname = 'report_asset_reliability' AND pronamespace = 'fms'::regnamespace;
  IF v_src NOT LIKE '%count(*) > 1%' THEN
    RAISE EXCEPTION
      '065 FAILED: MTBF 沒有「至少兩次故障」的守衛 —— '
      '一次故障算不出間隔，回 0 會看起來像設備一直在壞';
  END IF;
  -- `history_since` 在**簽章**裡而不是主體裡，所以要查
  -- `pg_get_function_result` —— `prosrc` 只含主體。
  -- （第一版查 prosrc，自我驗證因此誤報失敗。那個誤報是對的行為：
  --  它逼我發現檢查本身在看錯地方。）
  IF pg_get_function_result(
       (SELECT oid FROM pg_proc
         WHERE proname = 'report_asset_reliability'
           AND pronamespace = 'fms'::regnamespace)
     ) NOT LIKE '%history_since%' THEN
    RAISE EXCEPTION
      '065 FAILED: asset_reliability 沒有回傳 history_since —— '
      '歷程從 064 才開始，讀者必須知道這個 MTBF 涵蓋的是哪一段';
  END IF;

  -- (3) no-show 的分母只含需要報到的，而且讀預約自己的那一份。
  SELECT regexp_replace(prosrc, '--[^' || chr(10) || ']*', '', 'g') INTO v_src
    FROM pg_proc
   WHERE proname = 'report_space_utilization' AND pronamespace = 'fms'::regnamespace;
  IF v_src NOT LIKE '%r.requires_check_in%' THEN
    RAISE EXCEPTION
      '065 FAILED: no-show 的分母沒有讀 r.requires_check_in —— '
      '分子由 no_show.rs 依那一欄產生，分母改讀資源上的現值會讓兩者來自'
      '不同的地方，事後打開資源旗標就會把永遠不可能 NO_SHOW 的舊預約算進分母';
  END IF;

  -- 分母的形狀必須是 038 那一個。`hours_per_day` 是第一版的錯誤形狀，
  -- 它讓每一列都落到 assumed_24h，整個決定變成沒有效果的程式碼。
  IF v_src LIKE '%hours_per_day%' THEN
    RAISE EXCEPTION
      '065 FAILED: 使用率的分母在讀 hours_per_day —— '
      'opening_hours／operating_hours 的形狀是 038 定的星期鍵 → 時段陣列，'
      '沒有 hours_per_day 這個鍵，於是每一列都會落到 assumed_24h';
  END IF;
  IF v_src NOT LIKE '%time_windows_hours%' OR v_src NOT LIKE '%business_windows%' THEN
    RAISE EXCEPTION
      '065 FAILED: 使用率沒有走 038 的時段解析 —— '
      '場域那一層要用 business_windows()，休館日才不會算進分母';
  END IF;
  IF v_src NOT LIKE '%assumed_24h%' THEN
    RAISE EXCEPTION '065 FAILED: 使用率沒有說出時數基準';
  END IF;

  -- opening_hours 現在決定一個分母，形狀要在寫入時擋。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_bookable_opening_hours'
       AND conrelid = 'fms.bookable_resources'::regclass
  ) THEN
    RAISE EXCEPTION
      '065 FAILED: bookable_resources.opening_hours 沒有形狀約束 —— '
      '它從這裡起決定使用率的分母，壞掉的值該在寫入時擋而不是讀報表時炸';
  END IF;

  -- (4) service-volume 要把「費率未知」單獨計數。
  SELECT regexp_replace(prosrc, '--[^' || chr(10) || ']*', '', 'g') INTO v_src
    FROM pg_proc
   WHERE proname = 'report_service_volume' AND pronamespace = 'fms'::regnamespace;
  IF v_src NOT LIKE '%labor_minutes > 0%' THEN
    RAISE EXCEPTION
      '065 FAILED: service_volume 沒有計數「有工時但成本為 0」的工單 —— '
      '把費率未知當免費會讓 chargeback 帳單安靜地偏低';
  END IF;

  RAISE NOTICE '065 OK：四支報表函式建立，子樹用 ltree、MTBF 有兩次故障守衛、'
               'no-show 分母限定需報到、費率未知會計數（數字正確性在 '
               'reporting_four_slice.rs 驗）';
END;
$$;

COMMIT;
