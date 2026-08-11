-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 032: SLA 量測鏈的前三段（ADR-12）
-- =============================================================================
-- SLA 達成率報表卡在資料層，不是查詢層。現況：
--
--   * `work_orders.response_due_at` / `resolution_due_at` —— 沒有任何東西會寫
--   * `sla_policy_id` —— 只有 `raise_alarm` 會設（而它是從告警規則抄過來的，
--     不是解析出來的）；手動開單、PM 產單、預約衍生單全都是 NULL
--   * `sla_policies.applies_to_priority` —— 宣告了，零個讀取點
--   * 004 的完成判定是 `resolution_due_at IS NULL OR now() <= resolution_due_at`
--     → 左邊恆為真 → **每一張完成的工單都是 MET**
--
-- 最後那一項是重點：報表不是「還沒做」，是**做出來會全部 100%**。
-- 假數字比 null 糟得多 —— 拿去談合約的人不會知道它是假的。
--
-- -----------------------------------------------------------------------------
-- 實作時發現 ADR-12 初版有一處事實錯誤
-- -----------------------------------------------------------------------------
-- 那份文件寫「實際回應時刻沒有這個欄位」。錯的：`first_responded_at` 存在，
-- 而且狀態機用 `side_effects.set_responded` 在維護它 —— 宣告在四個進入
-- `ASSIGNED` 的動作上（ASSIGN×2、ACCEPT、AUTO_ASSIGN）。
--
-- 也就是說「回應 = 有人接下工單」這個語意，目錄裡早就定好了而且定得對。
-- 要修的是**寫入條件**：`AUTO_ASSIGN` 的 `side_effects` 帶
-- `"actor": "SYSTEM"`，但 `transition_work_order` 從來不讀它 ——
-- 於是自動派工也會填 `first_responded_at`，而被派到的人可能還沒看過那張單。
--
-- 這正是 ADR-12 決定 B 要防的失效模式（量到的是系統反應快、不是人反應快），
-- 而它已經上線了。
--
-- -----------------------------------------------------------------------------
-- 這個 migration 修四件事
-- -----------------------------------------------------------------------------
-- (1) `fms.resolve_sla_policy()` —— 讓 `applies_to_priority` 有讀取點
-- (2) `work_orders` 的 BEFORE INSERT 觸發器 —— 開單時解析 policy 並算 due
-- (3) `transition_work_order` —— actor_type、first_responded_at 的條件、
--     SUBMIT 起算時鐘、REOPEN 重設、以及那個恆真的 MET 判定
-- (4) 回填既有工單（升級路徑用；本 repo 的 DB 上是 no-op）
--
-- 依賴：004（work_orders 與 sla_policies）、022（transition 權限檢查）、
--       015（動作目錄與 side_effects）。
-- =============================================================================

-- `work_orders`／`sla_policies`／`tenants` 都是 FORCE RLS，而 migration 的連線
-- 沒有租戶情境 → `current_tenant_id()` 是 NULL → `tenant_isolation` 判定為 NULL
-- → 一列都看不到。回填會**靜默地什麼都沒做**，自我驗證才會炸。
--
-- 031 的檔頭把這條規則寫成「改動被稽核的那六張表要宣告平台情境」，
-- 但真正的規則更寬：**任何要讀寫租戶資料的 migration 都要。**
-- 031 只是第一個撞到的。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- 1. Policy 解析
-- -----------------------------------------------------------------------------
-- `sla_policies` 有 `facility_id`（可為 NULL＝租戶通用）與
-- `applies_to_priority`（可為 NULL＝適用所有優先度）。兩個維度各有
-- 「精確」與「通用」，因此優先序是四格：
--
--        facility 精確 + priority 精確   ← 最specific
--        facility 精確 + priority 通用
--        facility 通用 + priority 精確
--        facility 通用 + priority 通用   ← 最後備
--
-- 這個順序不是隨意選的：**場域先於優先度**。理由是 SLA 通常寫在
-- 「這棟樓的合約」裡，而優先度是那份合約內部的分級 ——
-- 反過來排會讓某棟樓的合約被另一棟樓的通用規則蓋掉。
--
-- 解析不到就回 NULL，呼叫端負責處理（→ NOT_APPLICABLE）。
-- **刻意不設「最後有個 default policy」的後備**：那會製造一個沒有人同意過
-- 的達成率。沒有 policy 就是沒有 policy，報表要說得出來。
CREATE OR REPLACE FUNCTION fms.resolve_sla_policy(
  p_tenant_id   uuid,
  p_facility_id uuid,
  p_priority    text
) RETURNS fms.sla_policies
LANGUAGE sql
STABLE
AS $$
  SELECT *
    FROM fms.sla_policies sp
   WHERE sp.tenant_id = p_tenant_id
     AND sp.is_active
     AND (sp.facility_id IS NULL OR sp.facility_id = p_facility_id)
     AND (sp.applies_to_priority IS NULL OR sp.applies_to_priority = p_priority)
   ORDER BY (sp.facility_id IS NOT NULL) DESC,
            (sp.applies_to_priority IS NOT NULL) DESC,
            sp.code
   LIMIT 1;
$$;

COMMENT ON FUNCTION fms.resolve_sla_policy(uuid, uuid, text) IS
  'ADR-12 決定 F：依 (facility, priority) 解析 SLA policy。'
  '場域先於優先度。解析不到回 NULL —— 刻意沒有 default 後備。';

-- -----------------------------------------------------------------------------
-- 2. 開單時算出 due
-- -----------------------------------------------------------------------------
-- **做成觸發器而不是改 `POST /work-orders`。** `source` 的 CHECK 列了七種
-- 開單來源（MANUAL／PM_PLAN／IOT_ALARM／RESERVATION／API／IMPORT／
-- INSPECTION_FINDING），逐一去改就是逐一有機會漏 —— 而漏掉的那條路徑
-- 產出的工單會永遠是 100% 達成，沒有任何訊號。
--
-- ADR-09 紀律 2：判斷交給資料庫。
--
-- 決定 A：時鐘從進入 `SUBMITTED` 起算。因此 `DRAFT` 開單時不算 due
-- （由 SUBMIT 轉移補上，見下一段）—— 使用者慢慢填表不該扣自己的分。
--
-- 決定 F：`response_due_at` / `resolution_due_at` 是絕對時刻，本身就是快照。
-- 之後有人調 policy 的分鐘數，已開的單不受影響。
CREATE OR REPLACE FUNCTION fms.trg_work_order_sla_targets()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_policy fms.sla_policies;
  v_from   timestamptz;
BEGIN
  -- DRAFT 還沒送出 → 不起算。
  IF NEW.status = 'DRAFT' THEN
    RETURN NEW;
  END IF;

  -- `raise_alarm` 會從告警規則帶 `wo_sla_policy_id` 進來。那是個明示的選擇，
  -- 不該被解析結果蓋掉 —— 但它同樣需要算出 due（006 只設了 id）。
  IF NEW.sla_policy_id IS NOT NULL THEN
    SELECT * INTO v_policy FROM fms.sla_policies WHERE id = NEW.sla_policy_id;
  ELSE
    v_policy := fms.resolve_sla_policy(NEW.tenant_id, NEW.facility_id, NEW.priority);
    NEW.sla_policy_id := v_policy.id;
  END IF;

  IF v_policy.id IS NULL THEN
    -- 解析不到 policy。**標 NOT_APPLICABLE 而不是留 ON_TRACK**：
    -- ON_TRACK 是「有目標且還沒逾期」，這裡是「沒有目標」。
    -- 兩者混在一起，報表就分不出「達成」與「沒在量」。
    NEW.sla_state := 'NOT_APPLICABLE';
    RETURN NEW;
  END IF;

  v_from := coalesce(NEW.created_at, clock_timestamp());
  NEW.response_due_at   := v_from + make_interval(mins => v_policy.response_minutes);
  NEW.resolution_due_at := v_from + make_interval(mins => v_policy.resolution_minutes);
  RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS trg_work_order_sla_targets ON fms.work_orders;
CREATE TRIGGER trg_work_order_sla_targets
  BEFORE INSERT ON fms.work_orders
  FOR EACH ROW EXECUTE FUNCTION fms.trg_work_order_sla_targets();

-- -----------------------------------------------------------------------------
-- 3. 狀態機
-- -----------------------------------------------------------------------------
-- 以 022 的版本為基準，改五處，其餘逐字保留。改動處都標了 `-- 032:`。
CREATE OR REPLACE FUNCTION fms.transition_work_order(
  p_work_order_id uuid,
  p_action        varchar,
  p_actor_user_id uuid    DEFAULT NULL,
  p_reason        varchar DEFAULT NULL,
  p_metadata      jsonb   DEFAULT '{}'::jsonb
) RETURNS fms.work_orders
LANGUAGE plpgsql
AS $$
DECLARE
  v_wo         fms.work_orders;
  v_rule       fms.work_order_transitions_allowed;
  v_actor      uuid := coalesce(p_actor_user_id, fms.current_user_id());
  -- 032: 系統動作與人為動作必須分得開。`side_effects.actor` 已經宣告了
  -- （`AUTO_ASSIGN` 是 `SYSTEM`），但過去沒有任何地方讀它。
  v_actor_type text;
  v_by_system  boolean;
  v_policy     fms.sla_policies;
  v_meta       jsonb := coalesce(p_metadata, '{}'::jsonb);
BEGIN
  SELECT * INTO v_wo FROM fms.work_orders WHERE id = p_work_order_id FOR UPDATE;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'work order % not found', p_work_order_id USING ERRCODE = 'P0002';
  END IF;

  SELECT * INTO v_rule
  FROM fms.work_order_transitions_allowed t
  WHERE t.is_active
    AND (t.tenant_id IS NULL OR t.tenant_id = v_wo.tenant_id)
    AND (t.work_order_type IS NULL OR t.work_order_type = v_wo.work_order_type)
    AND t.from_status = v_wo.status
    AND t.action = p_action
  ORDER BY t.tenant_id NULLS LAST, t.work_order_type NULLS LAST
  LIMIT 1;

  IF v_rule.id IS NULL THEN
    RAISE EXCEPTION 'action % is not allowed from status % (wo %)',
      p_action, v_wo.status, v_wo.wo_no USING ERRCODE = '23514';
  END IF;

  -- 權限檢查（022 新增）。以 42501 拋出，讓應用層的既有 SQLSTATE 映射
  -- 轉成 403 —— 與 set_context 的 PLATFORM_CONTEXT_DENIED 同一個碼，
  -- 語意也相同：這個身分不被允許做這件事。
  IF v_rule.required_permission IS NOT NULL THEN
    IF v_actor IS NULL THEN
      RAISE EXCEPTION
        'transition % requires permission % but no actor was supplied',
        p_action, v_rule.required_permission USING ERRCODE = '42501';
    END IF;
    IF NOT fms.user_has_permission(v_actor, v_rule.required_permission, v_wo.facility_id) THEN
      RAISE EXCEPTION
        'actor % lacks permission % required for action % on work order %',
        v_actor, v_rule.required_permission, p_action, v_wo.wo_no
        USING ERRCODE = '42501';
    END IF;
  END IF;

  -- 032（改動 1）：actor_type 從目錄讀，不再吃 DEFAULT 'USER'。
  --
  -- 過去每一筆 transition 都是 'USER'，包含 AUTO_ASSIGN 與 BREACH_SLA
  -- 這種沒有人參與的轉移。稽核軌跡上「誰做的」那一欄有一部分是假的，
  -- 而且看不出來 —— 這比缺值糟。
  v_actor_type := coalesce(v_rule.side_effects ->> 'actor', 'USER');
  IF v_actor_type NOT IN ('USER', 'SYSTEM', 'SERVICE_ACCOUNT') THEN
    RAISE EXCEPTION
      'side_effects.actor = % is not a valid actor_type (action %)',
      v_actor_type, p_action USING ERRCODE = '22023';
  END IF;
  v_by_system := v_actor_type <> 'USER';

  -- 032（改動 2）：SUBMIT 時起算時鐘（決定 A）。
  --
  -- DRAFT 開單時觸發器刻意不算 due，因此草稿送出是唯一的補算點。
  -- 用轉移後的優先度：使用者可能在草稿階段改過它。
  IF v_wo.status = 'DRAFT' AND v_rule.to_status = 'SUBMITTED'
     AND v_wo.resolution_due_at IS NULL THEN
    IF v_wo.sla_policy_id IS NOT NULL THEN
      SELECT * INTO v_policy FROM fms.sla_policies WHERE id = v_wo.sla_policy_id;
    ELSE
      v_policy := fms.resolve_sla_policy(v_wo.tenant_id, v_wo.facility_id, v_wo.priority);
    END IF;
  END IF;

  -- 032（改動 3）：REOPEN 是新的量測（決定 E）。
  --
  -- 「第一次有沒有準時修好」與「重開後有沒有準時修好」是兩個事實，
  -- 合併會讓兩者都看不見。因此把前一輪的結果**快照進這筆轉移的 metadata**
  -- 再重設 —— 那個事實被保留下來，只是不再是工單的當前狀態。
  --
  -- 為什麼放 metadata 而不是新開一張表：transitions 本來就是狀態機的
  -- 歷史軌跡，而「重開時前一輪的結果是什麼」正是這筆轉移的性質。
  -- 為它另立一張表是在既有答案旁邊再造一個。
  IF p_action = 'REOPEN' THEN
    v_meta := v_meta || jsonb_build_object(
      'sla_cycle_closed', jsonb_build_object(
        'sla_state',          v_wo.sla_state,
        'response_due_at',    v_wo.response_due_at,
        'resolution_due_at',  v_wo.resolution_due_at,
        'first_responded_at', v_wo.first_responded_at,
        'completed_at',       v_wo.completed_at,
        'reopened_count',     v_wo.reopened_count));

    IF v_wo.sla_policy_id IS NOT NULL THEN
      SELECT * INTO v_policy FROM fms.sla_policies WHERE id = v_wo.sla_policy_id;
    END IF;
  END IF;

  UPDATE fms.work_orders
     SET status = v_rule.to_status,
         -- 032（改動 4）：系統動作不算「有人回應」（決定 B）。
         --
         -- AUTO_ASSIGN 把工單塞給某個人；那個人還沒看過它。
         -- 舊條件會把那一刻記成回應時刻，於是回應時間量到的是
         -- 「系統派工多快」而不是「人多快接手」。
         first_responded_at = CASE
           WHEN first_responded_at IS NULL
                AND (v_rule.side_effects ->> 'set_responded') = 'true'
                AND NOT v_by_system
           THEN clock_timestamp() ELSE first_responded_at END,
         actual_start_at = CASE
           WHEN (v_rule.side_effects ->> 'set_actual_start') = 'true'
           THEN coalesce(actual_start_at, clock_timestamp()) ELSE actual_start_at END,
         actual_end_at = CASE
           WHEN (v_rule.side_effects ->> 'set_actual_end') = 'true'
           THEN clock_timestamp() ELSE actual_end_at END,
         completed_at = CASE
           WHEN v_rule.to_status = 'COMPLETED' THEN clock_timestamp() ELSE completed_at END,
         closed_at = CASE
           WHEN v_rule.to_status = 'CLOSED' THEN clock_timestamp() ELSE closed_at END,
         cancelled_reason = CASE
           WHEN v_rule.to_status IN ('CANCELLED','REJECTED') THEN p_reason ELSE cancelled_reason END,
         -- SUBMIT 補算（改動 2）／REOPEN 重設解決時鐘（改動 3）。
         sla_policy_id = coalesce(sla_policy_id, v_policy.id),
         response_due_at = CASE
           WHEN p_action = 'REOPEN' THEN response_due_at   -- 回應已經發生過，不重設
           WHEN v_policy.id IS NOT NULL AND response_due_at IS NULL
           THEN clock_timestamp() + make_interval(mins => v_policy.response_minutes)
           ELSE response_due_at END,
         resolution_due_at = CASE
           WHEN p_action = 'REOPEN' AND v_policy.id IS NOT NULL
           THEN clock_timestamp() + make_interval(mins => v_policy.resolution_minutes)
           WHEN v_policy.id IS NOT NULL AND resolution_due_at IS NULL
           THEN clock_timestamp() + make_interval(mins => v_policy.resolution_minutes)
           ELSE resolution_due_at END,
         -- `reopened_count` **刻意不在這裡加**。`fms-workorder` 的 repo 已經
         -- 有一個 `reopened_count = reopened_count + 1`（repo.rs:614），
         -- 在這裡再加一次就是同一條規則的第二份實作 —— 測試實測到 2 而不是 1。
         --
         -- 留下的不對稱：直接呼叫本函式的路徑（排程器）不會遞增。
         -- 那是既有狀況，不在 032 的授權範圍內，記在這裡而不是順手改掉。
         --
         -- 032（改動 5）：那個恆真的 MET 判定。
         --
         -- 舊條件是 `resolution_due_at IS NULL OR now() <= resolution_due_at`，
         -- 而 resolution_due_at 從來沒有東西會寫 → 恆為 NULL → 恆為真
         -- → 每一張完成的工單都是 MET。這是報表現在會回 100% 的直接原因。
         --
         -- 沒有 due 就是**沒在量**，不是達成。
         --
         -- 同時讓 `side_effects.compute_sla` 有意義（過去四個動作宣告、
         -- 零個讀取點）：進入 ASSIGNED 時比對回應時刻與 response_due_at。
         sla_state = CASE
           WHEN p_action = 'REOPEN' THEN
             CASE WHEN v_policy.id IS NULL THEN 'NOT_APPLICABLE' ELSE 'ON_TRACK' END

           WHEN (v_rule.side_effects ->> 'compute_sla') = 'true'
                AND response_due_at IS NOT NULL
                AND NOT v_by_system
                AND first_responded_at IS NULL          -- 這一刻才回應
                AND clock_timestamp() > response_due_at
             THEN 'RESPONSE_BREACHED'

           WHEN v_rule.to_status IN ('COMPLETED','CLOSED') THEN
             CASE
               WHEN sla_state IN ('RESPONSE_BREACHED','RESOLUTION_BREACHED')
                 THEN sla_state
               WHEN resolution_due_at IS NULL THEN 'NOT_APPLICABLE'
               WHEN clock_timestamp() <= resolution_due_at THEN 'MET'
               ELSE 'RESOLUTION_BREACHED'
             END

           ELSE sla_state
         END
   WHERE id = p_work_order_id
   RETURNING * INTO v_wo;

  INSERT INTO fms.work_order_transitions
    (tenant_id, work_order_id, from_status, action, to_status,
     actor_user_id, actor_type, reason, metadata)
  VALUES
    (v_wo.tenant_id, v_wo.id, v_rule.from_status, p_action, v_rule.to_status,
     v_actor, v_actor_type, p_reason, v_meta);

  PERFORM fms.emit_event(
    v_wo.tenant_id,
    coalesce(v_rule.side_effects ->> 'emit', 'work_order.status_changed'),
    'WORK_ORDER', v_wo.id,
    jsonb_build_object(
      'wo_no', v_wo.wo_no, 'from', v_rule.from_status, 'to', v_rule.to_status,
      'action', p_action, 'actor_user_id', v_actor,
      'facility_id', v_wo.facility_id, 'assignee_id', v_wo.assignee_id));

  RETURN v_wo;
END;
$$;

-- -----------------------------------------------------------------------------
-- 4. 回填
-- -----------------------------------------------------------------------------
-- 觸發器只影響新的 INSERT，因此既有工單仍然沒有 due。
--
-- **在這個 repo 的 DB 上這一段是 no-op**：009 只建組織／場域／設備／使用者，
-- 不建工單（實測 `work_orders` 是 0 筆）。留著是為了升級路徑 ——
-- 任何已經在跑的環境昇到 032 時，既有工單都需要這一段，
-- 否則它們會永遠停在「沒有 due」而被舊判定當成達成。
--
-- 以 `created_at` 為起點、用今天的 policy 回推。這是**追溯適用一條規則**，
-- 因此只做兩件事上安全的限縮：
--   * 只碰 `resolution_due_at IS NULL` 的（不覆寫任何既有值）
--   * 不碰 DRAFT（決定 A）
--
-- 回填不猜「當時的 policy 是什麼」—— 那個資訊不存在。示範資料上這無妨；
-- 若日後在有真實歷史的環境跑，這一段的結果只能當估算。
WITH resolved AS (
  SELECT wo.id,
         p.id  AS policy_id,
         p.response_minutes,
         p.resolution_minutes
    FROM fms.work_orders wo
    CROSS JOIN LATERAL fms.resolve_sla_policy(wo.tenant_id, wo.facility_id, wo.priority) p
   WHERE wo.resolution_due_at IS NULL
     AND wo.status <> 'DRAFT'
)
UPDATE fms.work_orders wo
   SET sla_policy_id     = coalesce(wo.sla_policy_id, r.policy_id),
       response_due_at   = wo.created_at + make_interval(mins => r.response_minutes),
       resolution_due_at = wo.created_at + make_interval(mins => r.resolution_minutes)
  FROM resolved r
 WHERE wo.id = r.id
   AND r.policy_id IS NOT NULL;

-- 解析不到 policy 的既有工單：標 NOT_APPLICABLE。
-- 它們現在多半是 ON_TRACK 或 MET，而兩者都是假的 —— 沒有 due 就沒在量。
UPDATE fms.work_orders
   SET sla_state = 'NOT_APPLICABLE'
 WHERE resolution_due_at IS NULL
   AND status <> 'DRAFT'
   AND sla_state <> 'NOT_APPLICABLE';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- **這個 migration 在 CORE 裡執行，位置早於 009**，因此這裡看不到任何
-- `sla_policies`（那是 009 種的）。026／027 的檔頭記過同一件事，
-- 而我第一版還是寫了「CRITICAL 應解析到 SLA_CRITICAL」——
-- 在已經套過 009 的開發資料庫上它通過了，只有從零建 template 時才炸。
--
-- 教訓與 migrate.sh 檔頭那條「CORE 一律按編號遞增」同源：**開發資料庫
-- 是逐個手動套用的，狀態比 template 寬鬆，因此它不能當驗證環境。**
--
-- 因此解析順序、觸發器行為、狀態機的五處改動，全部在
-- `sla_measurement_slice.rs` 斷言 —— 那裡有種子資料也有租戶情境。
-- 這裡只驗**不依賴租戶資料**的東西。
DO $$
DECLARE
  v_n      bigint;
  v_policy fms.sla_policies;
BEGIN
  -- (1) 空租戶必須回 NULL 而不是報錯。
  --     這一格看似瑣碎，但它是「解析不到就回 NULL」這個契約的下界 ——
  --     觸發器完全依賴它（回 NULL → NOT_APPLICABLE）。
  v_policy := fms.resolve_sla_policy(
    '00000000-0000-4000-8000-000000000000'::uuid, NULL, 'CRITICAL');
  IF v_policy.id IS NOT NULL THEN
    RAISE EXCEPTION '032 FAILED: 不存在的租戶竟解析到 %', v_policy.code;
  END IF;

  -- (2) 觸發器存在。
  IF NOT EXISTS (
    SELECT 1 FROM pg_trigger
     WHERE tgrelid = 'fms.work_orders'::regclass
       AND tgname = 'trg_work_order_sla_targets'
       AND NOT tgisinternal
  ) THEN
    RAISE EXCEPTION '032 FAILED: 缺少 trg_work_order_sla_targets';
  END IF;

  -- (3) 回填之後，不該再有「非草稿、非 NOT_APPLICABLE、卻沒有 due」的工單。
  --     那個組合正是舊 MET 判定恆真的來源。
  SELECT count(*) INTO v_n
    FROM fms.work_orders
   WHERE status <> 'DRAFT'
     AND resolution_due_at IS NULL
     AND sla_state <> 'NOT_APPLICABLE';
  IF v_n > 0 THEN
    RAISE EXCEPTION '032 FAILED: 仍有 % 張工單沒有 due 卻不是 NOT_APPLICABLE', v_n;
  END IF;

  -- (4) `compute_sla` 與 `actor` 現在都有讀取點了。斷言目錄裡的
  --     `actor` 值全都是 actor_type 的合法值 —— 否則狀態機會在執行時
  --     才拋 22023，而那時已經是使用者的請求失敗了。
  SELECT count(*) INTO v_n
    FROM fms.work_order_transitions_allowed
   WHERE side_effects ? 'actor'
     AND side_effects ->> 'actor' NOT IN ('USER','SYSTEM','SERVICE_ACCOUNT');
  IF v_n > 0 THEN
    RAISE EXCEPTION '032 FAILED: % 筆動作的 side_effects.actor 不是合法 actor_type', v_n;
  END IF;

  SELECT count(*) INTO v_n
    FROM fms.work_orders
   WHERE resolution_due_at IS NOT NULL;
  RAISE NOTICE '032 OK: % 張工單有 SLA 目標時刻', v_n;

  SELECT count(*) INTO v_n
    FROM fms.work_orders WHERE sla_state = 'NOT_APPLICABLE';
  RAISE NOTICE '032 OK: % 張工單標為 NOT_APPLICABLE（解析不到 policy）', v_n;
END;
$$;

COMMIT;
