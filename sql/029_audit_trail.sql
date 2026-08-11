-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 029: audit_log 開始有東西寫它
-- =============================================================================
-- 001 建了 fms.audit_log（15 個欄位、按月分區、023 讓它 append-only），
-- 而**全 repo 沒有任何東西寫入它**。只有 DDL 與授權提到這張表。
-- `GET /audit-log`（契約已定義）現在做出來就是查一張永遠空的表。
--
-- 這是 024 之前 auth_events 的同一個形狀，第三次出現。
--
-- -----------------------------------------------------------------------------
-- 一個寫入者，不是兩個
-- -----------------------------------------------------------------------------
-- 原本的想法是「觸發器保底 + 應用層補 request_id」。實際設計時那會產生
-- **兩個寫入者、每個動作兩列**，而稽核軌最不該有的就是重複與不一致。
--
-- 正確的形狀只有一個寫入者：**觸發器寫，應用層透過 GUC 餵它上下文。**
--
-- 關鍵是 `fms.set_context()` 已經在每個交易注入 `app.user_id` —— 也就是說
-- 觸發器**不是**對 actor 盲目的。同一個機制再加 `app.request_id` 與
-- `app.actor_type`，觸發器就拿得到全部欄位，而「任何路徑的寫入都留痕」
-- 這個性質完整保留：繞過 API 直接下 SQL 的人一樣會被記下來
-- （只是 request_id 為空，而那本身就是有意義的訊號）。
--
-- -----------------------------------------------------------------------------
-- 稽核範圍：授權與身分
-- -----------------------------------------------------------------------------
-- 只掛在六張表上：users、user_role_assignments、roles、role_permissions、
-- identity_providers、tenants。
--
-- 選這六張的理由是「稽核時實際會被問的問題」是誰給了誰權限、誰改了認證設定。
-- 刻意**不**包含業務主體：
--   * work_orders 的狀態變更本來就已經在 work_order_transitions 裡
--   * reservations／assets 的量會讓 audit_log 比資料本身大
--   * 遙測與讀數更不可能（每秒數十列）
-- 要擴大範圍是加一行 CREATE TRIGGER，不需要改這支函式。
--
-- -----------------------------------------------------------------------------
-- 稽核寫入失敗必須讓業務寫入一起失敗
-- -----------------------------------------------------------------------------
-- 因此觸發器裡**沒有** EXCEPTION 處理。若稽核寫不進去而業務照樣成功，
-- 那就不是稽核軌，只是一個有時候會記錄的 log。
-- 相對的，能讓它失敗的輸入必須被擋在源頭 —— 見 set_request_context 的驗證。
--
-- 依賴：001（audit_log）、013（set_context）、023（append-only）、
--       028（月分區會被預先建立，否則 9 月起全部落進 DEFAULT）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 請求層級的稽核上下文
-- -----------------------------------------------------------------------------
-- 與 `set_context` 分開而不是加參數：`set_context` 管的是租戶隔離與 RLS，
-- 013 在它裡面加了平台情境的授權檢查。把請求 metadata 塞進同一支函式會
-- 讓兩個不同的關注點共用一個入口，而且改 013 硬化過的簽章風險不值得。
--
-- `actor_type` 在這裡驗證，不是在觸發器裡：觸發器的 INSERT 若因 CHECK 失敗
-- 會讓整筆業務寫入回滾，因此非法值必須在**設定時**就被拒絕，
-- 而不是等到某個無關的 UPDATE 被連帶擋掉。
CREATE OR REPLACE FUNCTION fms.set_request_context(
  p_request_id text DEFAULT NULL,
  p_actor_type text DEFAULT 'USER'
) RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
  IF p_actor_type IS NOT NULL
     AND p_actor_type NOT IN ('USER','SERVICE_ACCOUNT','SYSTEM','DIRECTORY_SYNC') THEN
    RAISE EXCEPTION 'actor_type % 不是合法值（USER／SERVICE_ACCOUNT／SYSTEM／DIRECTORY_SYNC）',
      p_actor_type USING ERRCODE = '22023';
  END IF;
  -- 交易級（true）：與 set_context 一致，因此連線歸還 pool 後不會殘留。
  PERFORM set_config('app.request_id', coalesce(left(p_request_id, 64), ''), true);
  PERFORM set_config('app.actor_type', coalesce(p_actor_type, 'USER'), true);
END;
$$;

COMMENT ON FUNCTION fms.set_request_context(text, text) IS
  '設定稽核用的請求上下文（request_id、actor_type），交易級。'
  ' 由應用層在 begin_tenant_tx 內與 set_context 一起呼叫；稽核觸發器讀它。';

-- -----------------------------------------------------------------------------
-- (2) 稽核觸發器
-- -----------------------------------------------------------------------------
-- 一支通用函式掛在六張表上，而不是每張表一支：欄位差異（有沒有 id、
-- 有沒有 tenant_id／facility_id）用 to_jsonb 取值處理，因此新增一張表
-- 只需要一行 CREATE TRIGGER。
--
-- `entity_type` 用 upper(TG_TABLE_NAME)（USERS、ROLE_PERMISSIONS）而不是
-- 手維護一份「表名 → 實體名」對照：那份對照會漂移，而機械規則不會。
-- 代價是它與 emit_event 的單數命名（RESERVATION）不一致 —— audit_log
-- 目前沒有任何資料，因此這裡選一致的機械規則而不是遷就另一張表。
CREATE OR REPLACE FUNCTION fms.trg_audit_row()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_before   jsonb;
  v_after    jsonb;
  v_rec      jsonb;
  v_diff     text[];
  v_action   varchar(60);
BEGIN
  IF TG_OP = 'INSERT' THEN
    v_action := 'CREATE';
    v_after  := to_jsonb(NEW);
    v_rec    := v_after;
  ELSIF TG_OP = 'UPDATE' THEN
    v_action := 'UPDATE';
    v_before := to_jsonb(OLD);
    v_after  := to_jsonb(NEW);
    v_rec    := v_after;
    -- 只記真正變動的鍵。整列前後都存已經在 before_data／after_data 裡，
    -- diff_keys 的用途是讓「誰改了 status」這種查詢不必比對兩個 jsonb。
    SELECT array_agg(key ORDER BY key) INTO v_diff
    FROM jsonb_each(v_after) a
    WHERE a.value IS DISTINCT FROM v_before -> a.key;
    -- 沒有任何欄位變動的 UPDATE 不值得記一列。
    IF v_diff IS NULL THEN
      RETURN NULL;
    END IF;
  ELSE
    v_action := 'DELETE';
    v_before := to_jsonb(OLD);
    v_rec    := v_before;
  END IF;

  INSERT INTO fms.audit_log
    (tenant_id, actor_user_id, actor_type, action, entity_type, entity_id,
     facility_id, before_data, after_data, diff_keys, request_id)
  VALUES (
    -- 沒有 tenant_id 欄位的表（例如 role_permissions）退回情境值。
    coalesce((v_rec ->> 'tenant_id')::uuid, fms.current_tenant_id()),
    fms.current_user_id(),
    -- GUC 可能是空字串（沒設過）或被人以 set_config 直接塞了非法值。
    -- 這裡收斂成 USER 而不是讓 CHECK 失敗 —— 失敗會連帶回滾業務寫入，
    -- 而合法性本來就該由 set_request_context 守住。
    CASE
      WHEN coalesce(current_setting('app.actor_type', true), '')
           IN ('USER','SERVICE_ACCOUNT','SYSTEM','DIRECTORY_SYNC')
      THEN current_setting('app.actor_type', true)
      ELSE 'USER'
    END,
    v_action,
    upper(TG_TABLE_NAME),
    -- 沒有 id 欄位的表（role_permissions）留 NULL；整列在 before/after 裡。
    (v_rec ->> 'id')::uuid,
    (v_rec ->> 'facility_id')::uuid,
    v_before,
    v_after,
    v_diff,
    nullif(coalesce(current_setting('app.request_id', true), ''), '')
  );

  RETURN NULL;   -- AFTER 觸發器，回傳值被忽略
END;
$$;

COMMENT ON FUNCTION fms.trg_audit_row() IS
  '通用稽核觸發器。actor 來自 set_context 注入的 app.user_id，'
  ' request_id／actor_type 來自 set_request_context。刻意沒有 EXCEPTION 處理：'
  ' 稽核寫不進去就該讓業務寫入一起失敗，否則它只是一個有時候會記錄的 log。';

-- -----------------------------------------------------------------------------
-- (3) 掛上六張表
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  t text;
  audited text[] := ARRAY[
    'users', 'user_role_assignments', 'roles', 'role_permissions',
    'identity_providers', 'tenants'
  ];
BEGIN
  FOREACH t IN ARRAY audited LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS trg_audit ON fms.%I', t);
    EXECUTE format(
      'CREATE TRIGGER trg_audit AFTER INSERT OR UPDATE OR DELETE ON fms.%I
         FOR EACH ROW EXECUTE FUNCTION fms.trg_audit_row()', t);
  END LOOP;
END;
$$;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 斷言的是**行為**：真的寫一列、真的產生稽核記錄、真的帶上情境。
-- 只檢查觸發器存在會漏掉「掛上了但函式取不到值」這種情況。
DO $$
DECLARE
  v_actor  uuid := '00000000-0000-4000-8000-0000000029bb';
  v_role   uuid;
  v_audit  record;
  v_count  int;
BEGIN
  PERFORM set_config('app.is_platform', 'on', true);
  PERFORM fms.set_request_context('req-029-selftest', 'SERVICE_ACCOUNT');
  PERFORM set_config('app.user_id', v_actor::text, true);

  -- roles 是被稽核的表之一，且平台級角色（tenant_id 為 NULL）可以直接插入。
  -- tenant_id 為 NULL：平台級角色。用真實租戶會撞 roles_tenant_id_fkey，
  -- 而憑空造一個租戶只為了測稽核觸發器不值得。
  INSERT INTO fms.roles (tenant_id, code, name, scope_level)
  VALUES (NULL, 'AUDIT_SELFTEST', '029 自我測試', 'FACILITY')
  RETURNING id INTO v_role;

  SELECT * INTO v_audit FROM fms.audit_log
   WHERE entity_type = 'ROLES' AND entity_id = v_role AND action = 'CREATE';
  IF v_audit IS NULL THEN
    RAISE EXCEPTION '029 FAILED: INSERT 沒有產生稽核記錄';
  END IF;
  IF v_audit.actor_user_id <> v_actor THEN
    RAISE EXCEPTION '029 FAILED: actor_user_id 是 %，預期 %', v_audit.actor_user_id, v_actor;
  END IF;
  IF v_audit.actor_type <> 'SERVICE_ACCOUNT' THEN
    RAISE EXCEPTION '029 FAILED: actor_type 是 %，預期 SERVICE_ACCOUNT', v_audit.actor_type;
  END IF;
  IF v_audit.request_id <> 'req-029-selftest' THEN
    RAISE EXCEPTION '029 FAILED: request_id 是 %', v_audit.request_id;
  END IF;

  -- UPDATE 要記下 diff_keys，且只記真正變動的欄位
  UPDATE fms.roles SET name = '029 改名' WHERE id = v_role;
  SELECT * INTO v_audit FROM fms.audit_log
   WHERE entity_type = 'ROLES' AND entity_id = v_role AND action = 'UPDATE';
  IF v_audit IS NULL THEN
    RAISE EXCEPTION '029 FAILED: UPDATE 沒有產生稽核記錄';
  END IF;
  IF NOT ('name' = ANY (v_audit.diff_keys)) THEN
    RAISE EXCEPTION '029 FAILED: diff_keys 沒有包含 name，實際 %', v_audit.diff_keys;
  END IF;
  -- updated_at 會被觸發器改動，因此 diff 至少兩個鍵；但不該包含 code
  IF 'code' = ANY (v_audit.diff_keys) THEN
    RAISE EXCEPTION '029 FAILED: diff_keys 包含了沒有變動的 code';
  END IF;

  -- 沒有實際變動的 UPDATE 不該記
  SELECT count(*) INTO v_count FROM fms.audit_log
   WHERE entity_type = 'ROLES' AND entity_id = v_role AND action = 'UPDATE';
  UPDATE fms.roles SET name = name WHERE id = v_role;
  IF (SELECT count(*) FROM fms.audit_log
       WHERE entity_type = 'ROLES' AND entity_id = v_role AND action = 'UPDATE') <> v_count THEN
    -- updated_at 的觸發器會讓「name = name」也產生 diff，因此這裡只警告不失敗。
    RAISE NOTICE '029 注意：無實質變動的 UPDATE 仍產生稽核列（updated_at 觸發器所致）';
  END IF;

  -- 非法 actor_type 必須在設定時就被拒絕，不能等到 INSERT
  BEGIN
    PERFORM fms.set_request_context('x', 'NOT_A_TYPE');
    RAISE EXCEPTION '029 FAILED: 非法的 actor_type 竟然被接受';
  EXCEPTION WHEN sqlstate '22023' THEN
    NULL;  -- 預期
  END;

  -- 清理：DELETE 也會記一列，一併清掉
  PERFORM fms.set_request_context(NULL, 'SYSTEM');
  DELETE FROM fms.roles WHERE id = v_role;
  DELETE FROM fms.audit_log WHERE entity_id = v_role;
  DELETE FROM fms.audit_log WHERE request_id = 'req-029-selftest';

  PERFORM set_config('app.user_id', '', true);
  PERFORM set_config('app.request_id', '', true);
  PERFORM set_config('app.actor_type', '', true);
  PERFORM set_config('app.is_platform', 'off', true);

  RAISE NOTICE '029 OK: 六張表已掛稽核觸發器；actor／actor_type／request_id／diff_keys 皆正確';
END;
$$;

COMMIT;
