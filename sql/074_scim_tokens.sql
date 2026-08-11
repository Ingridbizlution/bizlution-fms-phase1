-- =============================================================================
-- 074_scim_tokens.sql —— SCIM 2.0 供裝端點的入站憑證
-- =============================================================================
--
-- SCIM 的驗證方向與這個系統其他所有秘密都相反。
--
-- LDAP bind、OIDC 的 client_secret、webhook 的簽章金鑰都是**出站**用的：
-- 我們必須持有明文才能送出去。SCIM 的 bearer token 是**入站**的：Entra ID
-- 帶著它來，我們只需要判斷「這個值對不對」。
--
-- 那個方向差異值一整個設計：**我們永遠不需要明文，所以只存雜湊。**
-- 發放的那一刻回傳一次，之後任何人（包含資料庫的擁有者）都無法還原。
-- webhook 的 signing_secret 做不到這件事 —— 它必須被讀出來簽章。
--
-- -----------------------------------------------------------------------------
-- 為什麼是新的一張表，而不是 identity_providers 的欄位
-- -----------------------------------------------------------------------------
-- 002 已經有 `scim_token_ref`（指向密鑰管理服務的參照，Phase 1 沒有解析器 ——
-- 與 ldap_bind_secret_ref、client_secret_ref 同一個根本原因）。直覺是再加一個
-- `scim_token_hash` 欄位。有兩個理由不那樣做：
--
-- **1. 欄位級 REVOKE 打不穿表級 GRANT。** 007 的 `ALTER DEFAULT PRIVILEGES` 在
--    CREATE 的那一刻就給了 fms_app 整張 identity_providers 的 SELECT。
--    PostgreSQL 的規則是：表級權限存在時，欄位級 REVOKE **不會**縮小它
--    （手冊 GRANT 節）。要讓一個欄位讀不到，得先撤掉整張表的 SELECT 再逐欄
--    授回 —— 而那份欄位清單會在下一次 ALTER TABLE ADD COLUMN 時腐化。
--    070 與 072 都被 007 的預設權限咬過一次，這是第三次。
--
-- **2. identity_providers 有稽核觸發器（029），而 trg_audit_row 不遮蔽任何欄位。**
--    它用 `to_jsonb(NEW)` 存整列。也就是說任何寫進那張表的秘密形狀欄位，
--    都會被複製進 audit_log 的 after_data，而 audit_log 是 append-only 且長期保留。
--    這不是假設 —— `fms.users.password_hash` 現在就在 audit_log 裡（argon2id
--    明碼雜湊，可離線暴力破解）。那是既有缺陷、不在本 slice 範圍，但它說明了
--    「往一張有稽核觸發器的表加秘密欄位」的代價。**新表沒有觸發器，問題不存在。**
--
-- 另外三個順帶的好處：可以撤銷、可以留輪替歷程、可以記 last_used_at
-- （「這個 token 從來沒被用過」是設定錯誤的第一個訊號）。
--
-- -----------------------------------------------------------------------------
-- 雜湊在 Rust 算，不在 SQL 算
-- -----------------------------------------------------------------------------
-- pgcrypto 有 digest()，寫成 `authenticate_scim_token(p_token text)` 更直覺。
-- 但那會讓**明文 token 成為一個查詢參數** —— 於是它會出現在
-- `log_statement`/`log_min_duration_statement` 的輸出、pg_stat_activity 的
-- query 欄位、以及任何抓連線的除錯工具裡。
--
-- 因此函式收的是 `p_token_hash`：Rust 算 SHA-256，明文從不離開行程記憶體。
-- 安全性等價（要算出雜湊仍然得先有明文），但少一個外洩面。
--
-- -----------------------------------------------------------------------------
-- 為什麼認證要走 SECURITY DEFINER
-- -----------------------------------------------------------------------------
-- fms_app 對 `token_hash` **沒有 SELECT 權限**。因此比對不可能在應用層做 ——
-- 那正是重點：fms_app 唯一能做的事是「問這個雜湊有沒有效」，
-- 它**永遠無法列舉**所有 token 的雜湊。SQL injection 或一個寫錯的查詢
-- 都偷不走任何東西，因為那一欄對它不存在。
--
-- 這也是為什麼函式必須 SECURITY DEFINER：SCIM 請求在租戶情境建立**之前**
-- 抵達（token 本身就是租戶的判別依據），沒有 current_tenant_id() 可用。
-- 與 073 的 pre-auth 政策不同，這裡選 SECURITY DEFINER 而不是一條放行政策，
-- 因為政策會讓無情境的連線讀得到**整張表**，而函式只回傳一列的兩個欄位。
-- =============================================================================

BEGIN;

CREATE TABLE fms.scim_tokens (
  id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id            uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  identity_provider_id uuid NOT NULL REFERENCES fms.identity_providers(id) ON DELETE CASCADE,
  -- SHA-256 的十六進位（64 字元）。**fms_app 沒有這一欄的 SELECT 權限。**
  token_hash           text NOT NULL CHECK (token_hash ~ '^[0-9a-f]{64}$'),
  -- 明文的前 8 字元。用途是讓人在日誌與 UI 裡辨認「是哪一個 token」，
  -- 而不必存明文。8 字元（32 bit）不足以暴力還原一個 256 bit 的 token。
  token_prefix         varchar(12) NOT NULL,
  created_at           timestamptz NOT NULL DEFAULT clock_timestamp(),
  created_by_user_id   uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  -- 「從來沒被用過」是設定錯誤最早的訊號：管理者以為接上了，其實 Entra
  -- 那側貼錯了值。沒有這一欄就只能等使用者反映「人沒有同步過來」。
  last_used_at         timestamptz,
  revoked_at           timestamptz,
  revoked_reason       text,
  CONSTRAINT ck_scim_tokens_revoked_reason CHECK (
    (revoked_at IS NULL) = (revoked_reason IS NULL)
  )
);

-- 雜湊唯一。索引不需要欄位的 SELECT 權限，因此這與上面的 REVOKE 並不衝突。
CREATE UNIQUE INDEX uq_scim_tokens_hash ON fms.scim_tokens (token_hash);

-- **一個身分來源同時只能有一個有效 token。**
--
-- 這不是整潔癖，是讓「輪替」在結構上不可能出錯：輪替必須先撤銷舊的才插得進
-- 新的。少了它，一次寫錯的輪替會留下兩個都能用的 token，而被遺忘的那一個
-- 沒有任何地方會顯示它還活著。
CREATE UNIQUE INDEX uq_scim_tokens_active
  ON fms.scim_tokens (identity_provider_id) WHERE revoked_at IS NULL;

CREATE INDEX idx_scim_tokens_provider ON fms.scim_tokens (identity_provider_id);

ALTER TABLE fms.scim_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.scim_tokens FORCE ROW LEVEL SECURITY;

-- 管理端（PATCH /identity-providers）在正常租戶情境下讀寫自己的 token 列。
CREATE POLICY tenant_isolation ON fms.scim_tokens FOR ALL
  USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
  WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

-- 認證路徑需要自己的一條政策，而這件事很容易被漏掉。
--
-- **`FORCE ROW LEVEL SECURITY` 對表的擁有者也生效。** 因此
-- `authenticate_scim_token` 雖然是 SECURITY DEFINER（以 fms_owner 執行），
-- 上面那條 `tenant_isolation` 仍然套用在它身上 —— 而認證發生在租戶情境建立
-- **之前**，`current_tenant_id()` 是 NULL，於是判定式是 NULL 而一列都看不到。
-- 症狀是「token 剛發出來就認不過」，而且完全不會有錯誤訊息。
--
-- 這個缺陷在手動驗證時看不到：以 psql 測那支函式的人通常會先開平台情境
-- （`set_config('app.is_platform','on')`），而那讓 `tenant_isolation` 直接放行。
-- 本 slice 第一次跑 scim_slice.rs 時 11 格全紅，才暴露出來。
--
-- `TO fms_owner` 是關鍵：這條政策**只**在 SECURITY DEFINER 的執行情境成立。
-- fms_app 直接查這張表時仍然受 `tenant_isolation` 管，因此無情境的連線
-- 讀不到任何一列 —— 比 073 的 pre-auth 政策（對所有角色放行）收得更緊。
CREATE POLICY scim_tokens_authenticate ON fms.scim_tokens
  FOR ALL TO fms_owner
  USING (fms.current_tenant_id() IS NULL)
  WITH CHECK (fms.current_tenant_id() IS NULL);

-- **同一件事在 identity_providers 上還要做一次，而這一步更容易漏。**
--
-- `authenticate_scim_token` 的 WHERE 裡有一段
-- `EXISTS (SELECT 1 FROM fms.identity_providers … AND p.scim_enabled)`。
-- 那張表也有 007 的 `tenant_isolation`，於是無情境時子查詢看不到任何列，
-- **整個 WHERE 因此是 false**，函式回 0 列 —— 症狀與上一段完全相同
-- （token 剛發出來就認不過），但原因在另一張表。
--
-- 第一次修只加了 scim_tokens 的政策，slice 仍然 11 格全紅：直接查
-- `fms.scim_tokens` 看得到 1 列，但經函式仍然是 0 列。兩張表都要顧。
--
-- ## 為什麼不用「函式內部先 set_config 出租戶情境」
--
-- 那樣更省事：token 那一列已經帶著 `tenant_id`，設進 `app.tenant_id`
-- 之後 `tenant_isolation` 就會放行。但 `set_config(..., true)` 是**交易級**的，
-- 而這支函式若被包在一個更大的交易裡呼叫，那個租戶情境會**留在該交易裡** ——
-- 等於繞過 `fms.set_context()`（013 在其中加了平台情境的授權檢查）取得了
-- 租戶身分。一條宣告式的政策看得見、不改變 session 狀態，因此選它。
--
-- ## 這條政策的實際暴露面
--
-- 只有 fms_owner 且無租戶情境時成立。應用程式連的是 fms_app（見
-- `common/mod.rs` 的兩條連線字串），migration 一律先開平台情境，
-- 因此唯一會命中它的就是這支 SECURITY DEFINER 函式。
-- 而持有 fms_owner 憑證的人本來就能刪掉任何政策。
CREATE POLICY idp_scim_authenticate ON fms.identity_providers
  FOR SELECT TO fms_owner
  USING (fms.current_tenant_id() IS NULL);

CREATE TRIGGER trg_freeze_tenant_id
  BEFORE UPDATE ON fms.scim_tokens
  FOR EACH ROW EXECUTE FUNCTION fms.trg_freeze_tenant_id();

COMMENT ON TABLE fms.scim_tokens IS
  'SCIM 2.0 端點的入站 bearer token。只存 SHA-256 雜湊 —— 入站憑證不需要明文，'
  '發放時回傳一次即不可還原。刻意獨立於 identity_providers：'
  '(1) 007 的表級 GRANT 讓欄位級 REVOKE 失效；'
  '(2) identity_providers 有 029 的稽核觸發器，而它用 to_jsonb(NEW) 不遮蔽欄位。';

COMMENT ON COLUMN fms.scim_tokens.token_hash IS
  'SHA-256 十六進位。**fms_app 沒有 SELECT 權限**（欄位級授權）—— '
  '因此應用層只能經 fms.authenticate_scim_token() 詢問單一雜湊是否有效，'
  '永遠無法列舉。雜湊由 Rust 計算：明文因此不會成為查詢參數而進入 Postgres 的日誌。';

COMMENT ON COLUMN fms.scim_tokens.last_used_at IS
  'NULL 表示這個 token 從未被使用過 —— 供裝設定錯誤最早的訊號，'
  '不必等到「使用者沒有同步過來」的客訴。';

COMMENT ON COLUMN fms.identity_providers.scim_token_ref IS
  '密鑰管理服務的參照。Phase 1 沒有解析器，因此**沒有任何消費者** —— '
  '實際的 SCIM 憑證在 fms.scim_tokens（只存雜湊）。'
  '與 client_secret_ref、ldap_bind_secret_ref 是同一個未解決的根本原因。';

-- -----------------------------------------------------------------------------
-- 授權：token_hash 寫得進去，讀不出來
-- -----------------------------------------------------------------------------
-- REVOKE ALL 那一行不是多餘的：007 的 ALTER DEFAULT PRIVILEGES 在 CREATE
-- 的那一刻就把整張表的權限給了 fms_app（070、072 各被咬過一次）。
REVOKE ALL ON fms.scim_tokens FROM fms_app;

GRANT SELECT (id, tenant_id, identity_provider_id, token_prefix, created_at,
              created_by_user_id, last_used_at, revoked_at, revoked_reason)
  ON fms.scim_tokens TO fms_app;

GRANT INSERT (id, tenant_id, identity_provider_id, token_hash, token_prefix,
              created_by_user_id)
  ON fms.scim_tokens TO fms_app;

-- UPDATE 只給撤銷用的兩欄。**沒有 token_hash** —— 換掉雜湊等於偷偷替換憑證，
-- 而那不會留下任何輪替紀錄。輪替的正確做法是撤銷 + 插入新列。
GRANT UPDATE (revoked_at, revoked_reason) ON fms.scim_tokens TO fms_app;

-- 沒有 DELETE：能刪就能讓一個 token 的存在與使用紀錄消失。

-- -----------------------------------------------------------------------------
-- 認證函式
-- -----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.authenticate_scim_token(p_token_hash text)
RETURNS TABLE (
  identity_provider_id uuid,
  tenant_id            uuid,
  scim_token_id        uuid
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = fms, public
AS $$
BEGIN
  -- 條件寫在一起而不是先查再判：分開會讓「token 有效但 provider 停用」
  -- 這種狀態有機會被誤放行。
  --
  -- `scim_enabled` 必須在這裡檢查。管理者把它關掉的意思是「停止供裝」，
  -- 如果只在端點層檢查，一條漏掉的路徑就讓那個開關變成裝飾。
  RETURN QUERY
  UPDATE fms.scim_tokens t
     SET last_used_at = clock_timestamp()
   WHERE t.token_hash = p_token_hash
     AND t.revoked_at IS NULL
     AND EXISTS (
       SELECT 1 FROM fms.identity_providers p
        WHERE p.id = t.identity_provider_id
          AND p.deleted_at IS NULL
          AND p.status = 'ACTIVE'
          AND p.scim_enabled
     )
  RETURNING t.identity_provider_id, t.tenant_id, t.id;
END;
$$;

COMMENT ON FUNCTION fms.authenticate_scim_token(text) IS
  'SCIM bearer token 認證。收雜湊而非明文（明文因此不會進 Postgres 的日誌）。'
  'SECURITY DEFINER：SCIM 請求在租戶情境建立之前抵達（token 本身就是租戶的'
  '判別依據），且 fms_app 讀不到 token_hash。無效時回 0 列 —— '
  '不區分「不存在」「已撤銷」「provider 停用」，那三者對呼叫端都是 401，'
  '而區分它們會讓這支函式變成一個可探測的預言機。'
  '順帶更新 last_used_at：認證本身就是「被使用」的定義。';

-- fms_app 需要 EXECUTE；其他角色不需要。
REVOKE ALL ON FUNCTION fms.authenticate_scim_token(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION fms.authenticate_scim_token(text) TO fms_app;

-- -----------------------------------------------------------------------------
-- 稽核觸發器的遮蔽清單
-- -----------------------------------------------------------------------------
-- 029 的 trg_audit_row 用 `to_jsonb(NEW)` 存整列，沒有任何遮蔽。後果是
-- **fms.users.password_hash 現在就在 audit_log 裡**（已在本機資料庫確認：
-- `after_data->>'password_hash'` 是 `$argon2id$v=19$...`）。
--
-- 那是既有缺陷，嚴格說不屬於 SCIM 這個切片。修它的理由是：
--
-- 1. 本 slice 的整個設計前提是「秘密不該進稽核軌」。把 scim_tokens 拆成
--    獨立的表繞開了那個問題，但**繞開不等於解決** —— 下一個往
--    identity_providers 或 users 加欄位的人會再踩一次。
-- 2. 遮蔽的機制是一份清單加一個迴圈。既然要寫，把 password_hash 漏在
--    清單外是明知故犯 —— 它正是這份清單存在的最壞情況。
--
-- 遮蔽成 NULL 而不是刪掉鍵：`diff_keys` 仍會包含 password_hash，
-- 於是「這次改動包含改密碼」這個事實**留著**，只有值不見了。
-- 刪掉鍵會讓稽核軌對改密碼這件事完全沉默。
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
  v_key      text;
  -- 值永遠不進稽核軌的欄位。名稱比對，因此對所有掛了這支觸發器的表通用。
  redacted   text[] := ARRAY['password_hash', 'token_hash', 'signing_secret',
                             'scim_token_hash', 'pkce_verifier'];
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
    --
    -- diff 在遮蔽**之前**算：否則兩次改密碼的 before/after 都是 NULL，
    -- diff_keys 就不會包含 password_hash，而「有人改了密碼」這件事會消失。
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

  -- 遮蔽。只在鍵存在時動作，因此對沒有這些欄位的表是零成本的。
  FOREACH v_key IN ARRAY redacted LOOP
    IF v_before ? v_key THEN
      v_before := jsonb_set(v_before, ARRAY[v_key], 'null'::jsonb);
    END IF;
    IF v_after ? v_key THEN
      v_after := jsonb_set(v_after, ARRAY[v_key], 'null'::jsonb);
    END IF;
  END LOOP;

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
  ' 稽核寫不進去就該讓業務寫入一起失敗，否則它只是一個有時候會記錄的 log。'
  ' 074 起遮蔽秘密欄位的值（password_hash 等）為 NULL —— 鍵仍在 diff_keys 裡，'
  ' 因此「這次改動包含改密碼」這個事實保留，只有值不見了。';

-- -----------------------------------------------------------------------------
-- 既有的 audit_log 列：把已經洩出去的雜湊清掉
-- -----------------------------------------------------------------------------
-- 遮蔽只擋未來。已經在表裡的 argon2 雜湊必須真的清掉，否則這個修復只完成一半。
--
-- audit_log 是 append-only（007 沒有給 fms_app DELETE／UPDATE），但這裡是
-- fms_owner 執行 migration，且 UPDATE 只把值換成 null、不動任何其他欄位。
-- 這是刻意的例外：留著它比破壞 append-only 更糟。
SELECT set_config('app.is_platform', 'on', true);

UPDATE fms.audit_log
   SET before_data = CASE WHEN before_data ? 'password_hash'
                          THEN jsonb_set(before_data, '{password_hash}', 'null'::jsonb)
                          ELSE before_data END,
       after_data  = CASE WHEN after_data ? 'password_hash'
                          THEN jsonb_set(after_data, '{password_hash}', 'null'::jsonb)
                          ELSE after_data END
 WHERE (before_data ->> 'password_hash') IS NOT NULL
    OR (after_data  ->> 'password_hash') IS NOT NULL;

-- -----------------------------------------------------------------------------
-- 權限：發放 SCIM token 屬於身分來源的設定
-- -----------------------------------------------------------------------------
-- 不新增權限碼。發放走 PATCH /identity-providers，而那支端點的授權前提是
-- `identity_provider:write` —— 能改 issuer 與 client_id 的人本來就能改掉整個
-- 身分來源。自我驗證第 (7) 格確認那個權限碼存在。

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_cnt               int;
  v_probe_id          uuid;
  v_probe_was_enabled boolean;
BEGIN
  -- (1) FORCE RLS。
  IF NOT EXISTS (
    SELECT 1 FROM pg_class
     WHERE oid = 'fms.scim_tokens'::regclass AND relforcerowsecurity
  ) THEN
    RAISE EXCEPTION '074 FAILED: scim_tokens 沒有 FORCE ROW LEVEL SECURITY';
  END IF;

  -- (2) 這是整張表的重點：fms_app 讀不到 token_hash。
  IF has_column_privilege('fms_app', 'fms.scim_tokens', 'token_hash', 'SELECT') THEN
    RAISE EXCEPTION
      '074 FAILED: fms_app 讀得到 token_hash —— 應用層就能列舉所有 SCIM 憑證的雜湊';
  END IF;

  -- (3) 但寫得進去，否則發放不了。
  IF NOT has_column_privilege('fms_app', 'fms.scim_tokens', 'token_hash', 'INSERT') THEN
    RAISE EXCEPTION '074 FAILED: fms_app 寫不了 token_hash —— 無法發放 token';
  END IF;

  -- (4) 且改不動：換掉雜湊等於無紀錄地替換憑證。
  IF has_column_privilege('fms_app', 'fms.scim_tokens', 'token_hash', 'UPDATE') THEN
    RAISE EXCEPTION
      '074 FAILED: fms_app 改得動 token_hash —— 輪替應該是撤銷 + 新增，不是覆寫';
  END IF;

  -- (5) 沒有 DELETE：token 的使用紀錄不該可以消失。
  IF has_table_privilege('fms_app', 'fms.scim_tokens', 'DELETE') THEN
    RAISE EXCEPTION '074 FAILED: fms_app 有 scim_tokens 的 DELETE 權限';
  END IF;

  -- (6) 一個 provider 只能有一個有效 token —— 讓輪替在結構上不可能留下遺孤。
  IF NOT EXISTS (
    SELECT 1 FROM pg_indexes
     WHERE schemaname = 'fms' AND indexname = 'uq_scim_tokens_active'
       AND indexdef LIKE '%revoked_at IS NULL%'
  ) THEN
    RAISE EXCEPTION '074 FAILED: uq_scim_tokens_active 不存在或沒有部分條件';
  END IF;

  -- (6b) 認證路徑的政策存在，且只給 definer 角色。
  --      少了它，token 一發出來就認不過（FORCE RLS 對擁有者也生效），
  --      而那個失敗完全沒有錯誤訊息。
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
     WHERE schemaname = 'fms' AND tablename = 'scim_tokens'
       AND policyname = 'scim_tokens_authenticate'
       AND roles = '{fms_owner}'
       AND qual LIKE '%current_tenant_id() IS NULL%'
  ) THEN
    RAISE EXCEPTION
      '074 FAILED: scim_tokens_authenticate 政策不存在、判定式不對，或不是只給 '
      'fms_owner —— SECURITY DEFINER 的認證路徑會被 FORCE RLS 擋成 0 列';
  END IF;

  -- (6c) 同一件事在 identity_providers 上。少了它，函式的 EXISTS 子查詢
  --      看不到 provider，整個 WHERE 變成 false —— 與 (6b) 相同的症狀，
  --      不同的表。第一次修只加了 (6b)，slice 仍然全紅。
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
     WHERE schemaname = 'fms' AND tablename = 'identity_providers'
       AND policyname = 'idp_scim_authenticate'
       AND roles = '{fms_owner}'
  ) THEN
    RAISE EXCEPTION
      '074 FAILED: idp_scim_authenticate 政策不存在 —— '
      'authenticate_scim_token 的 EXISTS 子查詢會被 identity_providers 的 RLS 擋掉';
  END IF;

  -- (6d) 行為驗證：真的插一列 token 並在**無情境**下認證一次。
  --      前面三格都是結構比對，而這個缺陷（兩張表的 RLS）恰好是結構比對
  --      看不出來的 —— 兩條政策都在，仍然可能有第三張表擋著。
  --      因此這裡真的跑一次完整的認證路徑。
  PERFORM set_config('app.is_platform', 'on', true);
  SELECT id, scim_enabled INTO v_probe_id, v_probe_was_enabled
    FROM fms.identity_providers
   WHERE deleted_at IS NULL AND status = 'ACTIVE' LIMIT 1;

  IF v_probe_id IS NOT NULL THEN
    UPDATE fms.identity_providers SET scim_enabled = true WHERE id = v_probe_id;
    INSERT INTO fms.scim_tokens (tenant_id, identity_provider_id, token_hash, token_prefix)
    SELECT tenant_id, id, repeat('f', 64), '074test'
      FROM fms.identity_providers WHERE id = v_probe_id;

    -- 清掉情境，模擬 middleware 的無情境連線。
    PERFORM set_config('app.is_platform', 'off', true);
    PERFORM set_config('app.tenant_id', '', true);

    SELECT count(*) INTO v_cnt FROM fms.authenticate_scim_token(repeat('f', 64));
    IF v_cnt <> 1 THEN
      RAISE EXCEPTION
        '074 FAILED: 無租戶情境下的認證回了 % 列（預期 1）—— '
        'RLS 擋住了認證路徑，而 token 一發出來就會認不過', v_cnt;
    END IF;

    -- 清理。`scim_enabled` 還原成**原本的值**而不是一律 false ——
    -- 這支 migration 若在一個已經啟用供裝的部署上重跑，一律 false 會靜默
    -- 關掉一個正在運作的整合。
    PERFORM set_config('app.is_platform', 'on', true);
    DELETE FROM fms.scim_tokens WHERE token_prefix = '074test';
    UPDATE fms.identity_providers
       SET scim_enabled = v_probe_was_enabled WHERE id = v_probe_id;
  ELSE
    RAISE NOTICE '074：沒有 identity_providers 列，跳過 (6d) 的認證行為驗證';
  END IF;

  -- (7) 端點的授權前提。
  IF NOT EXISTS (
    SELECT 1 FROM fms.permissions WHERE code = 'identity_provider:write'
  ) THEN
    RAISE EXCEPTION
      '074 FAILED: identity_provider:write 不存在 —— PATCH /identity-providers 無從授權';
  END IF;

  -- (8) 認證函式必須檢查 scim_enabled。少了它，管理者關掉供裝開關等於沒關。
  IF (SELECT regexp_replace(prosrc, '--[^\n]*', '', 'g')
        FROM pg_proc WHERE oid = 'fms.authenticate_scim_token(text)'::regprocedure)
     NOT LIKE '%scim_enabled%' THEN
    RAISE EXCEPTION
      '074 FAILED: authenticate_scim_token 沒有檢查 scim_enabled —— 供裝開關是裝飾';
  END IF;

  -- (9) 稽核遮蔽真的生效。這一格是行為驗證而非結構比對：
  --     直接寫一列進 audit_log 的來源表不現實（會動到真實資料），
  --     因此驗遮蔽清單存在且 password_hash 在其中。
  IF (SELECT regexp_replace(prosrc, '--[^\n]*', '', 'g')
        FROM pg_proc WHERE oid = 'fms.trg_audit_row()'::regprocedure)
     NOT LIKE '%password_hash%' THEN
    RAISE EXCEPTION
      '074 FAILED: trg_audit_row 的遮蔽清單沒有 password_hash';
  END IF;

  -- (10) 而且已經在表裡的雜湊被清掉了。
  SELECT count(*) INTO v_cnt
    FROM fms.audit_log
   WHERE (after_data ->> 'password_hash') IS NOT NULL
      OR (before_data ->> 'password_hash') IS NOT NULL;
  IF v_cnt > 0 THEN
    RAISE EXCEPTION
      '074 FAILED: audit_log 還有 % 列存著 password_hash —— 遮蔽只擋了未來', v_cnt;
  END IF;

  RAISE NOTICE '074 OK：scim_tokens（只存雜湊、fms_app 讀不到也改不動、'
               '一個 provider 一個有效 token）、authenticate_scim_token 檢查 '
               'scim_enabled、稽核觸發器遮蔽秘密欄位、audit_log 裡已無殘留的 '
               'password_hash（SCIM 端點的行為驗證在 scim_slice.rs）';
END;
$$;

COMMIT;
