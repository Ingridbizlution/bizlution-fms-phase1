-- =============================================================================
-- 070：refresh token 撤銷（`POST /auth/logout` 的落地處）
-- =============================================================================
-- 契約把 `POST /auth/logout` 寫成「撤銷 refresh token」。在這支 migration
-- 之前，refresh token 是**純無狀態 JWT**：`refresh_grant` 只驗簽章與帳號狀態，
-- 伺服器端沒有任何一列與某個 token 對應。因此那時能做出來的 logout 只有兩種：
-- 一種是回 204 但什麼都沒撤銷，另一種是不做。
--
-- 前者更糟 —— 端點名稱與契約敘述都宣稱撤銷，客戶端會據此認為「登出後那個
-- token 就死了」，而它其實還能再用 7 天（`refresh_ttl`）。這正是本專案反覆
-- 出現的那一類缺陷：宣告了但沒有人執行。
--
-- 這張表讓撤銷真的發生。
--
-- -----------------------------------------------------------------------------
-- 為什麼是黑名單，而不是 session 表
-- -----------------------------------------------------------------------------
-- session 表（發出去的每個 token 都有一列，refresh 時查「這一列還在嗎」）是
-- 另一種可行設計，但它把驗證從「驗簽章」變成「必須查表」：資料庫掛掉時
-- 連換發都停擺，而且每一次登入都要寫一列。
--
-- 黑名單反過來：預設有效，被撤銷的才記一列。代價是「有效」這件事變成
-- **開放的**（沒有記錄就是有效），因此列一旦該存在就必須存在 —— 所以
-- logout 的寫入是交易的一部分，寫不進去就回錯，不像 `auth_events` 那樣
-- 容許只記 log（見 repo.rs 的 `record_login_event`）。兩者的區別是：
-- auth_events 寫不進去只是軌跡有缺口，這張表寫不進去等於**沒有撤銷**。
--
-- -----------------------------------------------------------------------------
-- 撤銷的粒度是「這一個 token」，不是「這個使用者全部的 token」
-- -----------------------------------------------------------------------------
-- 決策由需求方拍板。另一個選項是在 users 加一欄 `tokens_valid_from`，logout
-- 設成 now()，refresh 拒絕 iat 更早的 token —— 更省（不需要表也不需要清理），
-- 但一次在自助機登出會把同一個人的手機與筆電一起登出。
--
-- 選 per-token 的後果必須寫在這裡，因為它不是可以事後補的：
--   **改密碼無法撤銷其他裝置上的 refresh token。** 改密碼的請求帶的是
--   access token，手上沒有其他裝置的 jti，而 per-token 的機制沒有「這個人
--   全部的 token」這個概念。`POST /auth/password/change` 因此在回應裡明說
--   `other_sessions_remain_valid: true`，不假裝做到了。要做到得補上面那一欄，
--   那是一次獨立的決策。
--
-- -----------------------------------------------------------------------------
-- 為什麼連「換發時被消耗的 token」也記一列（ROTATED）
-- -----------------------------------------------------------------------------
-- 少了這一步，logout 是**可證明不完整的**：`refresh_grant` 每次都簽一個新的
-- refresh token，而舊的並沒有失效。客戶端在 T1 換發拿到 B（A 仍有效），
-- 之後拿 B 登出 —— A 還能再用到它自己過期為止。使用者做了登出，攻擊者手上
-- 那一份卻還活著，正是登出要防的情況。
--
-- 記下來之後，一條換發鏈在任一時刻只有最後一個 token 是活的，logout 撤銷它
-- 就等於撤銷整條鏈。附帶得到一個標準的偵測訊號：已經被換掉的 token 又被拿來
-- 用，代表它被複製過（RFC 6819 §5.2.2.3）—— `refresh_grant` 因此在這種情況
-- 回 401 並寫一筆 `TOKEN_REUSE` 到 auth_events，而不是靜默地當成過期。
--
-- 代價是列數：一個每 15 分鐘換發一次的客戶端一天產生約 96 列。所以需要清理，
-- 見下面的 `purge_expired_refresh_revocations()`。
--
-- -----------------------------------------------------------------------------
-- access token 不在撤銷範圍內
-- -----------------------------------------------------------------------------
-- 刻意的，不是遺漏。access token 的 TTL 是 15 分鐘，而要撤銷它就得在
-- `require_auth`（每一個請求都會過）裡加一次資料庫查詢 —— 把整個 API 的
-- 認證從驗簽章變成查表，換來的是把 15 分鐘的窗縮短。
--
-- 所以 logout 之後那張 access token 仍然有效到它自己過期。這件事寫在 logout
-- 的回應裡（`access_token_remains_valid_for_seconds`），因為客戶端如果以為
-- 登出是立即的，就不會去清掉本機那一份。
--
-- -----------------------------------------------------------------------------
-- 為什麼過期的列可以直接刪
-- -----------------------------------------------------------------------------
-- `jwt::verify` 在檢查黑名單**之前**就已經擋掉過期的 token。因此一列的
-- `expires_at` 過了之後，它守的那個 token 再也不可能走到黑名單檢查 ——
-- 刪掉不會讓任何東西復活。這是清理可以無條件刪的唯一理由，
-- 也是 `expires_at` 必須存 token 自己的 exp（而不是撤銷時間加一個猜的長度）
-- 的原因。
--
-- -----------------------------------------------------------------------------
-- 順帶：password_min_length 進 tenants.settings
-- -----------------------------------------------------------------------------
-- `POST /auth/password/change` 需要一個最短長度。密碼政策是管理者定義的條件，
-- 不是程式碼的事實，所以走 067 建好的 `tenants.settings` 與
-- `fms.tenant_setting_int()`，不寫死在 Rust 裡。這裡只把它加進形狀約束的
-- 已知鍵 —— 型別錯了要在設定的人面前失敗，不是在三層之外改密碼的人面前。
--
-- 依賴：002（users／tenants／auth_events）、007（is_platform_context、
--       current_tenant_id）、067（tenant_settings_are_valid、tenant_setting_int）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

CREATE TABLE IF NOT EXISTS fms.revoked_refresh_tokens (
  -- token 自己的 jti。主鍵而不是另開一個 id：撤銷同一個 token 兩次
  -- （重複登出、客戶端重試）該是幂等的，而不是兩列。
  jti        uuid PRIMARY KEY,
  tenant_id  uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  user_id    uuid NOT NULL REFERENCES fms.users(id) ON DELETE CASCADE,
  -- token 自己的 exp，不是「撤銷時間 + 猜一個長度」。見檔頭最後一節：
  -- 清理的正確性完全建立在這一欄真的是 token 的過期時刻上。
  expires_at timestamptz NOT NULL,
  reason     text NOT NULL
               CONSTRAINT ck_revoked_refresh_tokens_reason
               CHECK (reason IN ('LOGOUT', 'ROTATED')),
  revoked_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- 清理用。掃的是「已過期」，因此以 expires_at 為鍵。
CREATE INDEX IF NOT EXISTS idx_revoked_refresh_tokens_expiry
  ON fms.revoked_refresh_tokens (expires_at);

COMMENT ON TABLE fms.revoked_refresh_tokens IS
  'refresh token 的撤銷黑名單。預設有效、被撤銷的才有列 —— 因此這張表的'
  ' 寫入失敗等於「沒有撤銷」，不可以像 auth_events 那樣只記 log。'
  ' 粒度是單一 token（見檔頭：改密碼因此撤銷不了其他裝置）。';

COMMENT ON COLUMN fms.revoked_refresh_tokens.expires_at IS
  'token 自己的 exp。清理可以無條件刪過期列，靠的就是這一欄的語意 ——'
  ' jwt 驗證會先擋掉過期 token，所以過期列守的東西再也走不到黑名單檢查。';

COMMENT ON COLUMN fms.revoked_refresh_tokens.reason IS
  'LOGOUT＝使用者登出；ROTATED＝換發時被消耗。少了 ROTATED，logout 只能'
  ' 撤銷客戶端手上最後那一個，換發鏈上先前的 token 仍然有效。';

-- -----------------------------------------------------------------------------
-- RLS
-- -----------------------------------------------------------------------------
-- 只有租戶隔離。沒有場域維度 —— token 不屬於某個場域。
--
-- 注意這張表的讀取發生在 refresh 路徑上，而那條路徑的情境是
-- `TenantContext::background(claims.tid, claims.sub, System)`：tid 來自已驗簽
-- 的 claims，所以「查得到自己租戶的撤銷紀錄」這件事成立。跨租戶查不到，
-- 而 jti 是 uuid v4，不同租戶撞號不是需要處理的情況。
ALTER TABLE fms.revoked_refresh_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.revoked_refresh_tokens FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS tenant_isolation ON fms.revoked_refresh_tokens;
CREATE POLICY tenant_isolation ON fms.revoked_refresh_tokens
FOR ALL
USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

-- 沒有 UPDATE 與 DELETE：一列撤銷紀錄沒有「改」的語意，而刪除只有清理會做，
-- 清理走 fms_owner。少給這兩個權限，讓「撤銷被撤回」不是應用層做得到的事。
--
-- **REVOKE 那一行不是多餘的。** 007 有
-- `ALTER DEFAULT PRIVILEGES IN SCHEMA fms GRANT ... ON TABLES TO fms_app`，
-- 所以這張表在 CREATE 的那一刻 fms_app 就已經有 DELETE 了 —— 下面的
-- `GRANT SELECT, INSERT` 只是加，不會把多的收回去。這個 migration 的自我驗證
-- 第 (4) 格正是在第一次執行時抓到這件事的。
REVOKE ALL ON fms.revoked_refresh_tokens FROM fms_app;
GRANT SELECT, INSERT ON fms.revoked_refresh_tokens TO fms_app;

-- -----------------------------------------------------------------------------
-- 清理
-- -----------------------------------------------------------------------------
-- 跨租戶刪除，因此需要平台情境（`fms_worker::begin_platform_tx`）。
-- EXECUTE 不給 fms_app：這是維運動作，不是任何端點該碰的東西。
CREATE OR REPLACE FUNCTION fms.purge_expired_refresh_revocations()
RETURNS bigint
LANGUAGE plpgsql
AS $$
DECLARE
  v_deleted bigint;
BEGIN
  DELETE FROM fms.revoked_refresh_tokens
   WHERE expires_at < clock_timestamp();
  GET DIAGNOSTICS v_deleted = ROW_COUNT;
  RETURN v_deleted;
END;
$$;

COMMENT ON FUNCTION fms.purge_expired_refresh_revocations() IS
  '刪掉守著已過期 token 的撤銷紀錄。安全的原因見 070 檔頭：jwt 驗證先擋'
  ' 過期，所以這些列守的 token 再也走不到黑名單檢查。需要平台情境。';

-- 057 之後 fms_app 有 schema 層級的 EXECUTE 預設權限（不是 PUBLIC），
-- 因此 REVOKE ... FROM PUBLIC 碰不到它，必須指名 —— 與 033 同一個理由。
REVOKE ALL ON FUNCTION fms.purge_expired_refresh_revocations() FROM PUBLIC, fms_app;
GRANT EXECUTE ON FUNCTION fms.purge_expired_refresh_revocations() TO fms_owner;

-- -----------------------------------------------------------------------------
-- password_min_length 進 tenants.settings 的已知鍵
-- -----------------------------------------------------------------------------
-- 067 的原函式只認 satisfaction_editable_days。整支重寫（不是加一層包裝），
-- 因為未知的鍵放行這件事必須留在最外層，否則新增一個鍵就會擋掉舊資料。
CREATE OR REPLACE FUNCTION fms.tenant_settings_are_valid(p jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE
    WHEN p IS NULL THEN true
    WHEN jsonb_typeof(p) <> 'object' THEN false
    ELSE
      -- satisfaction_editable_days：整數 0–365。
      (CASE
        WHEN NOT (p ? 'satisfaction_editable_days') THEN true
        WHEN jsonb_typeof(p -> 'satisfaction_editable_days') <> 'number' THEN false
        WHEN (p ->> 'satisfaction_editable_days')::numeric
               <> trunc((p ->> 'satisfaction_editable_days')::numeric) THEN false
        WHEN (p ->> 'satisfaction_editable_days')::numeric NOT BETWEEN 0 AND 365
          THEN false
        ELSE true
      END)
      AND
      -- password_min_length：整數 8–128。
      --
      -- 下界 8 不是「建議值」而是約束：租戶把它設成 1 之後，這個系統對密碼
      -- 就沒有任何要求了，而那個設定畫面離受影響的人很遠。上界 128 是為了
      -- 讓「設成 100000 導致沒有人能改密碼」不是一個合法的設定。
      (CASE
        WHEN NOT (p ? 'password_min_length') THEN true
        WHEN jsonb_typeof(p -> 'password_min_length') <> 'number' THEN false
        WHEN (p ->> 'password_min_length')::numeric
               <> trunc((p ->> 'password_min_length')::numeric) THEN false
        WHEN (p ->> 'password_min_length')::numeric NOT BETWEEN 8 AND 128
          THEN false
        ELSE true
      END)
  END;
$$;
COMMENT ON FUNCTION fms.tenant_settings_are_valid(jsonb) IS
  'tenants.settings 的形狀。只驗已知的鍵（未知的放行，這個欄位會長大）；'
  '已知的鍵型別錯了會在讀設定的地方炸，而那離設定它的人三層之外。';

-- 067 建的約束不會自動重驗，但函式換掉之後既有資料仍需符合新規則：
-- 重建約束讓 password_min_length 已經被設成 1 的租戶在**這裡**就失敗，
-- 而不是等到有人改密碼。
ALTER TABLE fms.tenants DROP CONSTRAINT IF EXISTS ck_tenants_settings;
ALTER TABLE fms.tenants
  ADD CONSTRAINT ck_tenants_settings
  CHECK (fms.tenant_settings_are_valid(settings));

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構的（這支跑在 CORE 階段，seed 還沒進來）。
-- **撤銷真的讓 refresh 失效、換發鏈的完整性、重用偵測 —— 全部在
--   auth_tail_slice.rs。** 這裡驗的是「少了它們一定壞」的那些結構事實。
DO $$
BEGIN
  -- (1) FORCE RLS。少了它，fms_owner 讀得到所有租戶的撤銷紀錄；更要緊的是
  --     這張表的擁有者就是 fms_owner，沒有 FORCE 時政策對它完全不生效。
  IF NOT EXISTS (SELECT 1 FROM pg_class
                  WHERE oid = 'fms.revoked_refresh_tokens'::regclass
                    AND relrowsecurity AND relforcerowsecurity) THEN
    RAISE EXCEPTION
      '070 FAILED: revoked_refresh_tokens 沒有 FORCE ROW LEVEL SECURITY';
  END IF;

  -- (2) jti 是主鍵。少了唯一性，重複登出會寫出兩列 —— 不會壞掉，但
  --     「撤銷是幂等的」這件事就不再由資料庫保證，而是靠每個呼叫端記得。
  IF NOT EXISTS (
    SELECT 1 FROM pg_index i
     WHERE i.indrelid = 'fms.revoked_refresh_tokens'::regclass
       AND i.indisprimary
       AND (SELECT array_agg(a.attname::text ORDER BY a.attname)
              FROM pg_attribute a
             WHERE a.attrelid = i.indrelid
               AND a.attnum = ANY (i.indkey)) = ARRAY['jti']
  ) THEN
    RAISE EXCEPTION '070 FAILED: jti 不是主鍵 —— 撤銷就不是幂等的';
  END IF;

  -- (3) ROTATED 必須在 reason 的 CHECK 裡。
  --     少了它，換發時消耗掉的 token 就寫不進來，而那不是少一筆紀錄的問題：
  --     見檔頭，那會讓 logout 只撤銷換發鏈上最後一個 token。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_revoked_refresh_tokens_reason'
       AND conrelid = 'fms.revoked_refresh_tokens'::regclass
       AND pg_get_constraintdef(oid) LIKE '%ROTATED%'
       AND pg_get_constraintdef(oid) LIKE '%LOGOUT%'
  ) THEN
    RAISE EXCEPTION
      '070 FAILED: reason 的 CHECK 沒有同時含 LOGOUT 與 ROTATED';
  END IF;

  -- (4) fms_app 不能 DELETE。有 DELETE 的話，「撤銷可以被撤回」變成應用層
  --     做得到的事 —— 而一個能自己刪掉黑名單列的漏洞，效果等於沒有 logout。
  IF has_table_privilege('fms_app', 'fms.revoked_refresh_tokens', 'DELETE') THEN
    RAISE EXCEPTION
      '070 FAILED: fms_app 可以 DELETE revoked_refresh_tokens —— 撤銷可被撤回';
  END IF;

  -- (5) fms_app 不能執行清理。它會跨租戶刪，端點不該碰得到。
  IF has_function_privilege(
       'fms_app', 'fms.purge_expired_refresh_revocations()', 'EXECUTE') THEN
    RAISE EXCEPTION
      '070 FAILED: fms_app 可以執行 purge_expired_refresh_revocations()';
  END IF;

  -- (6) expires_at 必須 NOT NULL。可為 NULL 時 `expires_at < now()` 對那一列
  --     是 NULL（不刪），所以不會清掉不該清的 —— 壞的是另一頭：一列永遠留著，
  --     而寫入它的程式碼會被當成正確的。
  IF EXISTS (
    SELECT 1 FROM pg_attribute
     WHERE attrelid = 'fms.revoked_refresh_tokens'::regclass
       AND attname = 'expires_at' AND NOT attnotnull
  ) THEN
    RAISE EXCEPTION '070 FAILED: expires_at 可為 NULL';
  END IF;

  -- (7) password_min_length 的形狀約束真的在守。8 以下要擋掉 ——
  --     這是密碼政策的下界，不是預設值。
  IF fms.tenant_settings_are_valid('{"password_min_length": 1}'::jsonb) THEN
    RAISE EXCEPTION '070 FAILED: settings 放行了 password_min_length = 1';
  END IF;
  IF fms.tenant_settings_are_valid('{"password_min_length": "十二"}'::jsonb) THEN
    RAISE EXCEPTION '070 FAILED: settings 放行了字串型別的 password_min_length';
  END IF;
  IF fms.tenant_settings_are_valid('{"password_min_length": 200}'::jsonb) THEN
    RAISE EXCEPTION '070 FAILED: settings 放行了 password_min_length = 200 ——'
                    ' 沒有人能改密碼會是一個合法的設定';
  END IF;
  IF NOT fms.tenant_settings_are_valid('{"password_min_length": 12}'::jsonb) THEN
    RAISE EXCEPTION '070 FAILED: settings 擋掉了合法的 password_min_length = 12';
  END IF;
  -- 067 的鍵不能因為這次重寫而失效（兩個鍵是 AND，寫錯很容易變成只驗新的）。
  IF fms.tenant_settings_are_valid('{"satisfaction_editable_days": 400}'::jsonb) THEN
    RAISE EXCEPTION '070 FAILED: 重寫 tenant_settings_are_valid 之後'
                    ' satisfaction_editable_days 不再被驗證';
  END IF;
  -- 兩個鍵同時給，且其中一個錯 —— AND 短路寫錯時這格會過。
  IF fms.tenant_settings_are_valid(
       '{"satisfaction_editable_days": 14, "password_min_length": 2}'::jsonb) THEN
    RAISE EXCEPTION '070 FAILED: 兩鍵並存時 password_min_length 沒有被驗證';
  END IF;
  -- 未知的鍵仍然放行（這個欄位會長大，見 067）。
  IF NOT fms.tenant_settings_are_valid('{"future_knob": {"a": 1}}'::jsonb) THEN
    RAISE EXCEPTION '070 FAILED: settings 擋掉了未知的鍵';
  END IF;

  RAISE NOTICE '070 OK：revoked_refresh_tokens（FORCE RLS、jti 主鍵、'
               'LOGOUT/ROTATED、fms_app 無 DELETE）、清理函式、'
               'password_min_length 進 settings；撤銷的行為驗證在 '
               'auth_tail_slice.rs';
END;
$$;

COMMIT;
