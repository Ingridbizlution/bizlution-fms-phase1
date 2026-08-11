-- =============================================================================
-- 073：SSO 授權請求（`/auth/sso/{providerCode}/authorize` 與 `/callback`）
-- =============================================================================
-- OIDC 的授權碼流程需要伺服器在兩次請求之間記住三個一次性的秘密：
--
--   | 值 | 防的是什麼 | 少了它會怎樣 |
--   |---|---|---|
--   | `state` | CSRF | 攻擊者可以把**他自己的**授權碼塞進受害者的瀏覽器，讓受害者登入攻擊者的帳號 |
--   | `nonce` | id_token 重放 | 一張在別處取得的 id_token 可以被拿來當成這一次登入的結果 |
--   | PKCE `code_verifier` | 授權碼攔截 | 攔到授權碼的人可以自己去換 token |
--
-- 三者都必須是**伺服器端**的狀態：放在 cookie 或 URL 裡就等於交給攻擊者。
-- 因此這張表。
--
-- -----------------------------------------------------------------------------
-- 為什麼 `state` 是唯一鍵，而且是一次性的
-- -----------------------------------------------------------------------------
-- callback 只有 `state` 可以用來認出「這是哪一次登入嘗試」——
-- 那時還沒有任何使用者情境。所以 `state` 同時是查詢鍵與能力憑證
-- （256 bit 的隨機值，知道它就等於證明自己發起過那次跳轉）。
--
-- `consumed_at` 讓它一次性：`UPDATE ... WHERE consumed_at IS NULL RETURNING` 是
-- 原子的，因此同一個 state 被重放時第二次拿不到列。少了它，一個被攔截的
-- callback URL 可以被重複使用。
--
-- -----------------------------------------------------------------------------
-- RLS：pre-auth 的讀寫
-- -----------------------------------------------------------------------------
-- `/authorize` 與 `/callback` 都在**使用者登入之前**，因此沒有租戶情境。
-- 做法比照 024 的 `auth_events_preauth_append`：一條 `current_tenant_id() IS NULL`
-- 的政策，判定式攤在 `pg_policies` 裡看得見，比再多一個 SECURITY DEFINER 函式
-- 好審（013 之後每個 DEFINER 函式都是一個要獨立審查的授權面）。
--
-- 那條政策讓無情境的連線讀得到**所有**列。這是刻意的取捨，而它成立的前提是
-- `state` 的不可猜測性：唯一會執行 SQL 的是我們自己的程式碼，而它只以 state
-- 查詢。這與「auth_events 的 pre-auth 寫入可以寫任何一列」是同一種取捨。
--
-- -----------------------------------------------------------------------------
-- 過期的列
-- -----------------------------------------------------------------------------
-- `expires_at` 預設 10 分鐘：使用者在 IdP 上輸入密碼與 MFA 需要時間，
-- 但一個放了一小時的授權請求沒有正當用途。
--
-- 清理沿用 070 建好的形狀（fms-worker 的 token_purge 迴圈），因此這裡只提供
-- 一個函式，由那個迴圈一起呼叫 —— 不另開一個一天跑一次的迴圈去掃一張小表。
--
-- 依賴：002（identity_providers、user_identities）、007（RLS 與授權）、
--       014（resolve_tenant_by_code —— `/authorize` 用它在無情境下定位租戶）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

CREATE TABLE IF NOT EXISTS fms.sso_auth_requests (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id    uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  identity_provider_id uuid NOT NULL
                 REFERENCES fms.identity_providers(id) ON DELETE CASCADE,
  -- CSRF。callback 唯一能用的查詢鍵，同時是能力憑證。
  state        text NOT NULL,
  -- id_token 的重放防護。換到 token 之後要比對 id_token 裡的 nonce。
  nonce        text NOT NULL,
  -- PKCE。**存 verifier 不存 challenge**：challenge 是從 verifier 算出來的
  -- （S256），而 token 交換要送的是 verifier。存 challenge 就換不了 token。
  pkce_verifier text NOT NULL,
  -- 送給 IdP 的 redirect_uri。**必須原樣回傳給 token 端點**（OIDC 要求兩次
  -- 一致），因此要存下來 —— 從設定重算會在設定改過之後對不上。
  redirect_uri text NOT NULL,
  consumed_at  timestamptz,
  expires_at   timestamptz NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- `state` 全域唯一：它是 callback 的查詢鍵，而 callback 沒有租戶情境可以縮小
-- 範圍。跨租戶撞號不是需要處理的情況（256 bit 隨機值），但唯一鍵讓
-- 「同一個 state 對應兩列」變成不可能，而那會讓一次性保證失效。
CREATE UNIQUE INDEX IF NOT EXISTS uq_sso_auth_requests_state
  ON fms.sso_auth_requests (state);

-- 清理用。
CREATE INDEX IF NOT EXISTS idx_sso_auth_requests_expiry
  ON fms.sso_auth_requests (expires_at);

COMMENT ON TABLE fms.sso_auth_requests IS
  'OIDC 授權碼流程的伺服器端狀態（state／nonce／PKCE verifier）。'
  ' 三者都不能放在 cookie 或 URL 裡 —— 那等於交給攻擊者。'
  ' consumed_at 讓 state 一次性；沒有它，被攔截的 callback URL 可以重複使用。';

COMMENT ON COLUMN fms.sso_auth_requests.pkce_verifier IS
  '存 verifier 不存 challenge：challenge 是從 verifier 算出來的（S256），'
  ' 而 token 交換要送的是 verifier。存 challenge 就換不了 token。';

COMMENT ON COLUMN fms.sso_auth_requests.redirect_uri IS
  '原樣存下來。OIDC 要求 token 端點收到的 redirect_uri 與 authorize 時一致，'
  ' 而從設定重算會在設定改過之後對不上 —— 症狀是換 token 時被 IdP 拒絕。';

-- -----------------------------------------------------------------------------
-- RLS
-- -----------------------------------------------------------------------------
ALTER TABLE fms.sso_auth_requests ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.sso_auth_requests FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS tenant_isolation ON fms.sso_auth_requests;
CREATE POLICY tenant_isolation ON fms.sso_auth_requests
FOR ALL
USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

-- pre-auth：`/authorize` 與 `/callback` 都在使用者登入之前，沒有租戶情境。
-- 比照 024 的做法（見檔頭）。
DROP POLICY IF EXISTS sso_requests_preauth ON fms.sso_auth_requests;
CREATE POLICY sso_requests_preauth ON fms.sso_auth_requests
FOR ALL
USING (fms.current_tenant_id() IS NULL)
WITH CHECK (fms.current_tenant_id() IS NULL);

REVOKE ALL ON fms.sso_auth_requests FROM fms_app;
-- 沒有 DELETE：清理走 fms_owner。少給它，讓「一次性」不是應用層可以繞過的
-- （能刪就能把 consumed_at 的紀錄清掉再重放一次）。
GRANT SELECT, INSERT, UPDATE ON fms.sso_auth_requests TO fms_app;

-- -----------------------------------------------------------------------------
-- 清理
-- -----------------------------------------------------------------------------
-- 與 070 的 `purge_expired_refresh_revocations` 同一個形狀與理由：
-- 過期的授權請求再也不可能被 callback 使用（consume 的條件含 `expires_at >
-- clock_timestamp()`），因此刪掉不會讓任何東西復活。
CREATE OR REPLACE FUNCTION fms.purge_expired_sso_requests()
RETURNS bigint
LANGUAGE plpgsql
AS $$
DECLARE
  v_deleted bigint;
BEGIN
  DELETE FROM fms.sso_auth_requests WHERE expires_at < clock_timestamp();
  GET DIAGNOSTICS v_deleted = ROW_COUNT;
  RETURN v_deleted;
END;
$$;

COMMENT ON FUNCTION fms.purge_expired_sso_requests() IS
  '刪掉過期的授權請求。安全的理由與 070 相同：consume 的條件含 expires_at，'
  ' 所以過期的列再也不可能被使用。需要平台情境。';

REVOKE ALL ON FUNCTION fms.purge_expired_sso_requests() FROM PUBLIC, fms_app;
GRANT EXECUTE ON FUNCTION fms.purge_expired_sso_requests() TO fms_owner;

-- -----------------------------------------------------------------------------
-- 一次性消耗
-- -----------------------------------------------------------------------------
-- 條件式 UPDATE，因此「查了沒被用過就標記已用」是原子的。
-- 分成三個語句會讓兩個並發的 callback 都通過。
--
-- 回傳值刻意區分四種結果 —— callback 要能對呼叫端說出**哪一種**問題：
-- 不存在（state 亂填或已被清理）、已使用（重放）、已過期（放太久）、成功。
CREATE OR REPLACE FUNCTION fms.consume_sso_state(p_state text)
RETURNS TABLE (
  outcome              text,
  request_id           uuid,
  tenant_id            uuid,
  identity_provider_id uuid,
  nonce                text,
  pkce_verifier        text,
  redirect_uri         text
)
LANGUAGE plpgsql
AS $$
DECLARE
  v_row fms.sso_auth_requests;
BEGIN
  UPDATE fms.sso_auth_requests s
     SET consumed_at = clock_timestamp()
   WHERE s.state = p_state
     AND s.consumed_at IS NULL
     AND s.expires_at > clock_timestamp()
  RETURNING s.* INTO v_row;

  IF v_row.id IS NOT NULL THEN
    RETURN QUERY SELECT 'CONSUMED'::text, v_row.id, v_row.tenant_id,
                        v_row.identity_provider_id, v_row.nonce,
                        v_row.pkce_verifier, v_row.redirect_uri;
    RETURN;
  END IF;

  -- UPDATE 沒有命中：分辨是哪一種。**不合併成一個「無效」** ——
  -- 「已使用」是可能的攻擊訊號（重放），「已過期」只是使用者在 IdP 上待太久，
  -- 而「不存在」通常是設定錯誤。三者的處置完全不同。
  SELECT * INTO v_row FROM fms.sso_auth_requests s WHERE s.state = p_state;
  IF v_row.id IS NULL THEN
    RETURN QUERY SELECT 'NOT_FOUND'::text, NULL::uuid, NULL::uuid, NULL::uuid,
                        NULL::text, NULL::text, NULL::text;
  ELSIF v_row.consumed_at IS NOT NULL THEN
    RETURN QUERY SELECT 'ALREADY_USED'::text, v_row.id, v_row.tenant_id,
                        v_row.identity_provider_id, NULL::text, NULL::text, NULL::text;
  ELSE
    RETURN QUERY SELECT 'EXPIRED'::text, v_row.id, v_row.tenant_id,
                        v_row.identity_provider_id, NULL::text, NULL::text, NULL::text;
  END IF;
END;
$$;

COMMENT ON FUNCTION fms.consume_sso_state(text) IS
  '一次性消耗 state。條件式 UPDATE 讓「查了沒用過就標記已用」是原子的 ——'
  ' 分成三個語句會讓兩個並發的 callback 都通過。回傳值區分 NOT_FOUND／'
  ' ALREADY_USED（可能的重放）／EXPIRED（使用者待太久）/ CONSUMED。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_src text;
BEGIN
  -- (1) FORCE RLS。
  IF NOT EXISTS (SELECT 1 FROM pg_class
                  WHERE oid = 'fms.sso_auth_requests'::regclass
                    AND relrowsecurity AND relforcerowsecurity) THEN
    RAISE EXCEPTION '073 FAILED: sso_auth_requests 沒有 FORCE ROW LEVEL SECURITY';
  END IF;

  -- (2) `state` 唯一。少了它，同一個 state 可以對應兩列，而一次性保證就失效
  --     （消耗一列，另一列還在）。
  IF NOT EXISTS (
    SELECT 1 FROM pg_indexes
     WHERE schemaname = 'fms' AND indexname = 'uq_sso_auth_requests_state'
       AND indexdef LIKE '%UNIQUE%'
  ) THEN
    RAISE EXCEPTION '073 FAILED: state 沒有唯一索引 —— 一次性保證會失效';
  END IF;

  -- (3) pre-auth 政策存在。`/authorize` 與 `/callback` 沒有租戶情境，
  --     少了它兩支端點在 FORCE RLS 下必定 0 筆／寫不進去。
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
     WHERE schemaname = 'fms' AND tablename = 'sso_auth_requests'
       AND policyname = 'sso_requests_preauth'
  ) THEN
    RAISE EXCEPTION '073 FAILED: sso_requests_preauth 政策不存在';
  END IF;

  -- (4) **消耗必須是條件式 UPDATE。**
  --     `SELECT` 之後再 `UPDATE` 會讓兩個並發的 callback 都通過，
  --     而那正是重放要利用的窗口。
  SELECT regexp_replace(prosrc, '--[^\n]*', '', 'g') INTO v_src
    FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
   WHERE n.nspname = 'fms' AND p.proname = 'consume_sso_state';
  IF v_src IS NULL THEN
    RAISE EXCEPTION '073 FAILED: 找不到 fms.consume_sso_state';
  END IF;
  IF v_src NOT LIKE '%consumed_at IS NULL%' THEN
    RAISE EXCEPTION
      '073 FAILED: consume_sso_state 的 UPDATE 沒有 `consumed_at IS NULL` 條件 —— '
      '同一個 state 可以被重放';
  END IF;
  IF v_src NOT LIKE '%expires_at > clock_timestamp()%' THEN
    RAISE EXCEPTION
      '073 FAILED: consume_sso_state 沒有檢查 expires_at —— 過期的授權請求還能用';
  END IF;
  -- 四種結果都要能分辨。合併成一個「無效」會讓重放與「使用者待太久」
  -- 在日誌裡長得一樣。
  IF v_src NOT LIKE '%ALREADY_USED%' OR v_src NOT LIKE '%EXPIRED%'
     OR v_src NOT LIKE '%NOT_FOUND%' THEN
    RAISE EXCEPTION
      '073 FAILED: consume_sso_state 沒有區分 NOT_FOUND／ALREADY_USED／EXPIRED';
  END IF;

  -- (5) fms_app 不能 DELETE：能刪就能把 consumed_at 的紀錄清掉再重放一次。
  IF has_table_privilege('fms_app', 'fms.sso_auth_requests', 'DELETE') THEN
    RAISE EXCEPTION
      '073 FAILED: fms_app 可以 DELETE sso_auth_requests —— 一次性保證可被繞過';
  END IF;

  -- (6) 存的是 verifier 不是 challenge。存錯了 token 交換一定失敗，
  --     而那個錯誤要到接上真實 IdP 才會出現。
  IF NOT EXISTS (
    SELECT 1 FROM pg_attribute
     WHERE attrelid = 'fms.sso_auth_requests'::regclass
       AND attname = 'pkce_verifier' AND NOT attisdropped
  ) THEN
    RAISE EXCEPTION '073 FAILED: 沒有 pkce_verifier 欄位';
  END IF;
  IF EXISTS (
    SELECT 1 FROM pg_attribute
     WHERE attrelid = 'fms.sso_auth_requests'::regclass
       AND attname = 'pkce_challenge' AND NOT attisdropped
  ) THEN
    RAISE EXCEPTION
      '073 FAILED: 存了 pkce_challenge —— token 交換要送的是 verifier，'
      '存 challenge 就換不了 token';
  END IF;

  RAISE NOTICE '073 OK：sso_auth_requests（FORCE RLS、state 唯一、pre-auth 政策、'
               'fms_app 無 DELETE）、consume_sso_state 是條件式 UPDATE 且區分四種'
               '結果（行為驗證在 sso_slice.rs）';
END;
$$;

COMMIT;
