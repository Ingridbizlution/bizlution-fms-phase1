-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 049: 把稽核擴大到有場域維度的表（work_orders／assets）
-- =============================================================================
-- 029 掛了六張表，全部是身分與租戶治理的表：`users`、`user_role_assignments`、
-- `roles`、`role_permissions`、`identity_providers`、`tenants`。它們**都沒有
-- 場域維度**，因此每一列稽核的 `facility_id` 都是 NULL。
--
-- 046 為此收緊了讀取端（場域受限的讀者只看得到自己場域的稽核列），並在檔頭
-- 寫了這一句：「日後把稽核擴大到有場域維度的表（work_orders／assets）時，
-- 這個欄位會自己填上，而 (2) 的收斂會立刻開始生效。」這個 migration 就是那件事。
--
-- 029 的觸發器函式是通用的，`facility_id` 也早就寫好了
-- （`(v_rec ->> 'facility_id')::uuid`），所以這裡真的只需要兩行 CREATE TRIGGER。
-- 那是 029 那個設計的回報：擴大範圍不用改函式。
--
-- -----------------------------------------------------------------------------
-- 與 work_order_transitions 的重疊 —— 刻意不去除
-- -----------------------------------------------------------------------------
-- `work_orders` 已經有一份專用軌跡：`work_order_transitions`。所以每次狀態
-- 轉移會同時產生**兩列**紀錄。那看起來像重複，但兩者記的不是同一件事：
--
--     work_order_transitions   這是什麼**業務動作**（ASSIGN／REJECT…）、
--                              誰做的、reason、metadata
--     audit_log                **哪些欄位**從什麼變成什麼（整列前後 + diff_keys）、
--                              request_id／ip／user_agent
--
-- 而且轉移軌跡有一個明確的空洞：**`PATCH /work-orders/{id}` 完全不在裡面。**
-- 改標題、改優先度、改負責人、改排程時間、改成本 —— 那些都不是狀態轉移，
-- 因此 047 之前**沒有任何地方**記得住它們。對一個要拿去對帳與稽核的系統來說，
-- 「誰把這張工單的優先度從 LOW 改成 CRITICAL」是必須答得出來的問題。
--
-- 因此不做去重。去重的做法會是「狀態轉移就不寫稽核」，而那等於在稽核軌跡上
-- 挖掉**最重要的那些事件**，只為了省下與另一張表的重疊。
--
-- -----------------------------------------------------------------------------
-- 成本要說清楚
-- -----------------------------------------------------------------------------
-- `work_orders` 有 56 個欄位、`assets` 有 35 個。稽核存的是 `before_data` 與
-- `after_data` **整列**，所以每一次 UPDATE 大約寫進兩份完整的列。
-- `work_orders` 是這個系統最熱的表，因此這是一筆真實的寫入放大。
--
-- 三件讓它可控的事，都已經在：
--   * `audit_log` 是**按月分割**的（001），舊分割可以直接卸載或歸檔；
--   * 029 的「沒有任何欄位變動的 UPDATE 不記一列」在這裡不太會觸發
--     （`updated_at`／`version` 每次都會動），所以不要指望它省什麼；
--   * `sweep_sla_states()` 的機器更新**不是每分鐘每列** —— 它的守衛只讓
--     `ON_TRACK → AT_RISK → SLA_BREACHED` 各發生一次，因此一張工單一生
--     最多多出兩列機器來源的稽核（`actor_type = 'SYSTEM'`，分得出來）。
--
-- 若日後真的太多，該做的是分割輪替策略，不是讓稽核選擇性地漏記。
--
-- -----------------------------------------------------------------------------
-- 這件事解鎖了什麼
-- -----------------------------------------------------------------------------
-- 046 的檔頭記過：`audit:read` 目前是 TENANT 範圍，而當時**不能**把它降成
-- FACILITY —— 因為那時每一列的 `facility_id` 都是 NULL，降級會「看起來像
-- 收斂了範圍，實際上給的是全租戶的存取」。
--
-- 049 之後那個前提變了：工單與設備的稽核列都帶著真實的 `facility_id`，
-- 046 的 RESTRICTIVE FOR SELECT 政策開始真的過濾。降級因此變成一個安全的
-- 動作 —— 但**這個 migration 不做**，因為降級會同時改變那六張身分表的
-- 可見性（它們的列仍然是 `facility_id IS NULL`，降級後場域受限的管理員
-- 就看不到自己場域使用者的角色變更了）。那是一個獨立的決定。
--
-- 依賴：029（通用觸發器）、046（稽核的場域可見性）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DO $$
DECLARE
  t text;
  -- 有場域維度的表。兩者的 `facility_id` 都是 NOT NULL，
  -- 因此每一列稽核都會帶著真實的場域 —— 那正是 046 的收斂要能生效的前提。
  audited text[] := ARRAY['work_orders', 'assets'];
BEGIN
  FOREACH t IN ARRAY audited LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS trg_audit ON fms.%I', t);
    EXECUTE format(
      'CREATE TRIGGER trg_audit AFTER INSERT OR UPDATE OR DELETE ON fms.%I
         FOR EACH ROW EXECUTE FUNCTION fms.trg_audit_row()', t);
  END LOOP;
END;
$$;

COMMENT ON TABLE fms.audit_log IS
  '稽核軌跡。facility_scope 政策只管 SELECT（見 migration 046）：'
  '寫入不能被場域收斂，否則 ORG 範圍的使用者做被稽核的動作時，'
  '稽核列寫不進去會讓他的整個動作失敗（029 的設計）。'
  '049 之後涵蓋 work_orders 與 assets，因此不再是每一列的 facility_id 都是 NULL '
  '—— 046 的收斂從那時起真的會過濾。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- CORE 位置：**跑在 009 之前**，所以不能建工單或設備來驗行為
-- （那需要租戶、場域、空間節點）。這裡只驗結構；行為由整合測試
-- `audit_trail_slice.rs` 驗，那裡有種子資料。
DO $$
DECLARE
  v_missing text;
  v_n       bigint;
BEGIN
  -- (1) 兩張表都掛上了，而且是 AFTER 的三種操作、FOR EACH ROW。
  --     只檢查「觸發器存在」不夠：少了 DELETE 或寫成 FOR EACH STATEMENT
  --     都會讓它安靜地少記東西。
  SELECT string_agg(x.t, '、' ORDER BY x.t) INTO v_missing
    FROM unnest(ARRAY['work_orders', 'assets']) AS x(t)
   WHERE NOT EXISTS (
     SELECT 1 FROM pg_trigger g
      WHERE g.tgrelid = ('fms.' || x.t)::regclass
        AND g.tgname = 'trg_audit'
        AND NOT g.tgisinternal
        AND g.tgtype & 1 = 1          -- FOR EACH ROW
        AND g.tgtype & 4 = 4          -- INSERT
        AND g.tgtype & 8 = 8          -- DELETE
        AND g.tgtype & 16 = 16        -- UPDATE
        AND g.tgtype & 2 = 0);        -- AFTER（BEFORE 這一位會是 1）
  IF v_missing IS NOT NULL THEN
    RAISE EXCEPTION
      '049 FAILED: 這些表沒有正確的 trg_audit（需 AFTER INSERT/UPDATE/DELETE '
      'FOR EACH ROW）：%', v_missing;
  END IF;

  -- (2) 029 的六張沒有被弄掉。DO 迴圈裡的 DROP TRIGGER 打錯表名的話，
  --     症狀是「某張表從此不再稽核」—— 沒有任何錯誤。
  SELECT count(*) INTO v_n
    FROM pg_trigger g
    JOIN pg_class c ON c.oid = g.tgrelid
   WHERE g.tgname = 'trg_audit'
     AND NOT g.tgisinternal
     AND c.relnamespace = 'fms'::regnamespace;
  IF v_n <> 8 THEN
    RAISE EXCEPTION '049 FAILED: 掛了稽核的表應為 8 張（029 的 6 + 049 的 2），實際 %', v_n;
  END IF;

  -- (3) **每一張掛了稽核、且有 facility_id 欄位的表，那個欄位都不可為 NULL。**
  --
  --     這一格保的是 046 的收斂不會被悄悄繞過。若日後有人把稽核掛到一張
  --     `facility_id` 可為 NULL 的表上，那些列會落進「不屬於任何場域」，
  --     而 046 的政策對它們的處理是「場域受限的讀者看不到」——
  --     也就是那些稽核列對場域管理員**隱形**。那可能是對的，也可能是
  --     疏忽，但它必須是一個有人想過的決定，而不是一個預設。
  SELECT string_agg(c.relname, '、' ORDER BY c.relname) INTO v_missing
    FROM pg_trigger g
    JOIN pg_class c ON c.oid = g.tgrelid
    JOIN pg_attribute a ON a.attrelid = c.oid AND a.attname = 'facility_id'
   WHERE g.tgname = 'trg_audit'
     AND NOT g.tgisinternal
     AND c.relnamespace = 'fms'::regnamespace
     AND NOT a.attnotnull;
  IF v_missing IS NOT NULL THEN
    RAISE EXCEPTION
      '049 FAILED: 這些表掛了稽核但 facility_id 可為 NULL，那些稽核列會對'
      '場域受限的讀者隱形（046）。若那是刻意的，請在這裡明確排除：%', v_missing;
  END IF;

  RAISE NOTICE '049 OK: work_orders 與 assets 已納入稽核，稽核列開始帶場域';
END;
$$;

COMMIT;
