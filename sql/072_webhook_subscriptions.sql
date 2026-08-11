-- =============================================================================
-- 072：客戶端 webhook 訂閱（`/webhooks` 的落地處）
-- =============================================================================
-- -----------------------------------------------------------------------------
-- 為什麼**不**自己造一套投遞機制
-- -----------------------------------------------------------------------------
-- `fms.notifications.channel` 的 CHECK 從 006 起就含 `'WEBHOOK'`，而 dispatcher
-- 的頻道分類（`DELIVERABLE` / `SELF_DELIVERING`）沒有它 —— 於是一筆
-- `channel = 'WEBHOOK'` 的通知會被標成
-- `SUPPRESSED, last_error = 'no transport configured for channel WEBHOOK'`。
--
-- 也就是說：這個系統早就宣告了 webhook 這個頻道，只是沒有傳輸層。
--
-- 因此 webhook 的投遞**重用 notifications 佇列**：
--
--   * 扇出（一個事件 → N 個訂閱）= 這支 migration 的 `fanout_webhooks()`，
--     形狀比照 041 的通知扇出；
--   * 重試、退避、停放（`attempt_count`、`last_error`、`SUPPRESSED`）
--     = dispatcher 既有的那一套，一行都不用重寫；
--   * 傳輸層 = dispatcher 新增的 WEBHOOK 分支（HMAC 簽章 + SSRF 閘門）。
--
-- 自己造一張 `webhook_deliveries` 加一套退避，等於把已經驗證過的機制複製一份，
-- 而複製品會在原件被修時悄悄過期。
--
-- `entity_type = 'WEBHOOK_SUBSCRIPTION'` / `entity_id = 訂閱 id` 是投遞時
-- 找回簽章密鑰的鍵 —— 那兩欄本來就是為這種關聯而存在的。
--
-- -----------------------------------------------------------------------------
-- 簽章密鑰是這個 codebase 唯一一處存放**明文可用**密鑰的地方
-- -----------------------------------------------------------------------------
-- 全專案的慣例是「只存密鑰管理服務的參照，不存密鑰」（見
-- identity_providers.rs 檔頭）。HMAC 簽章沒有這個選項：要簽就必須拿到金鑰，
-- 而 Phase 1 沒有 KMS。
--
-- 所以這裡的取捨是明說的：金鑰由伺服器產生、**只在建立時回傳一次**、存在
-- `signing_secret` 欄位裡。保護靠三件事：
--
--   1. **欄位級 REVOKE SELECT。** `fms_app`（API 的連線角色）讀不到這一欄 ——
--      因此任何端點都不可能把它放進回應，即使有人寫了 `SELECT *`。
--      投遞由 worker（`fms_owner`）做，它讀得到。
--   2. 租戶隔離的 RLS（與其他表相同）。
--   3. 上線前必須換成 KMS —— 記在 docs/security-review-open-items.md。
--
-- 欄位級權限是真的權限，不是慣例：`SELECT signing_secret` 以 fms_app 執行會回
-- `permission denied for table webhook_subscriptions`。自我驗證第 (3) 格驗它。
--
-- -----------------------------------------------------------------------------
-- 契約只有 GET 與 POST，沒有 PATCH／DELETE
-- -----------------------------------------------------------------------------
-- 一個關不掉的 webhook 是兩個問題：它會永遠敲一個死掉的端點，而且它是一條
-- 關不掉的資料外送通道。
--
-- 因此 `(tenant_id, url)` 是唯一鍵，而 `POST` 對既有的 url 是**更新** ——
-- 帶 `is_active: false` 就能停用。這讓「關掉」在契約宣告的兩個動作之內做得到，
-- 不需要偷偷加一支端點。
--
-- 另外有一個自動閥：連續失敗達 `ck_webhook_max_failures` 的門檻就自動停用
-- 並寫明原因。一個對方已經下線的訂閱不該無限期地製造出站流量。
--
-- 依賴：001（event_outbox、notifications 在 006）、006（notifications.channel
--       已含 WEBHOOK）、007（RLS 與授權）、016（permissions）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

CREATE TABLE IF NOT EXISTS fms.webhook_subscriptions (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id    uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  -- 只允許 https。應用層的 SSRF 閘門（fms_shared::safe_http）也擋，但那一層
  -- 是在**送出時**檢查；這裡讓一筆 http 的訂閱連存進來都不行。
  url          text NOT NULL
                 CONSTRAINT ck_webhook_url_https CHECK (url LIKE 'https://%'),
  -- 訂閱哪些事件。空陣列不允許 —— 那是一筆永遠不會觸發的訂閱，
  -- 而它在清單裡看起來是正常的。
  event_types  text[] NOT NULL
                 CONSTRAINT ck_webhook_event_types_present
                 CHECK (cardinality(event_types) > 0),
  -- HMAC-SHA256 的金鑰。**欄位級 REVOKE SELECT FROM fms_app**，見檔頭。
  signing_secret text NOT NULL,
  description  varchar(200),
  is_active    boolean NOT NULL DEFAULT true,
  -- 連續失敗計數。成功即歸零 —— 與登入節流同一個理由：
  -- 累計總失敗數會讓一個用了三年、偶爾抖一下的訂閱被停用。
  consecutive_failures int NOT NULL DEFAULT 0,
  disabled_at    timestamptz,
  disabled_reason text,
  last_success_at timestamptz,
  last_failure_at timestamptz,
  last_error     text,
  created_by   uuid REFERENCES fms.users(id) ON DELETE SET NULL,
  created_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  -- 自動停用時必須寫明原因。少了這條，一筆 is_active = false 的訂閱分不出
  -- 是「客戶自己關的」還是「系統因為連續失敗關的」——
  -- 而那兩件事的處置完全不同。
  CONSTRAINT ck_webhook_disabled_reason
    CHECK (disabled_at IS NULL OR disabled_reason IS NOT NULL)
);

-- 一個租戶一個 url 只有一筆訂閱。契約沒有 PATCH／DELETE，因此 POST 對既有的
-- url 是更新 —— 這個唯一鍵就是那個語意的依據（見檔頭）。
CREATE UNIQUE INDEX IF NOT EXISTS uq_webhook_subscriptions_url
  ON fms.webhook_subscriptions (tenant_id, url);

-- 扇出時的查詢：某個事件型別有哪些啟用中的訂閱。
CREATE INDEX IF NOT EXISTS idx_webhook_subscriptions_active
  ON fms.webhook_subscriptions USING gin (event_types)
  WHERE is_active;

-- -----------------------------------------------------------------------------
-- webhook 投遞的幂等鍵
-- -----------------------------------------------------------------------------
-- relay 是 **at-least-once**（它的檔頭要求 handler 自行幂等），因此同一個事件
-- 會被重新取用。041 的 `uq_notifications_event_recipient` 對 webhook **無效**：
-- 它的鍵是 `(source_event_id, recipient_user_id, channel)`，而 webhook 的
-- `recipient_user_id` 是 NULL —— 唯一索引裡的 NULL 互不衝突，所以那個索引
-- 一筆都擋不住。
--
-- 症狀會是：relay 每次重放都對客戶端再送一次同樣的事件，而客戶端沒有辦法
-- 分辨那是新事件還是重放。
--
-- 因此 webhook 用 `entity_id`（訂閱 id）當第二個維度：一個事件對一個訂閱
-- 只會有一筆通知。
CREATE UNIQUE INDEX IF NOT EXISTS uq_notifications_webhook_event
  ON fms.notifications (source_event_id, entity_id)
  WHERE channel = 'WEBHOOK' AND source_event_id IS NOT NULL;

COMMENT ON TABLE fms.webhook_subscriptions IS
  '客戶端 webhook 訂閱。投遞重用 notifications 佇列（channel = WEBHOOK，'
  ' entity_id = 本表的 id）—— dispatcher 的重試與停放機制因此不必重寫。';

COMMENT ON COLUMN fms.webhook_subscriptions.signing_secret IS
  'HMAC-SHA256 金鑰。**fms_app 沒有這一欄的 SELECT 權限**（欄位級 REVOKE）—— '
  ' 因此任何端點都不可能把它放進回應。只在建立時回傳一次。'
  ' 這是本 codebase 唯一存放明文可用密鑰的地方，上線前須換成 KMS。';

COMMENT ON COLUMN fms.webhook_subscriptions.consecutive_failures IS
  '連續失敗數，成功即歸零。累計總數會讓一個偶爾抖一下的長期訂閱被停用。';

-- -----------------------------------------------------------------------------
-- RLS
-- -----------------------------------------------------------------------------
ALTER TABLE fms.webhook_subscriptions ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.webhook_subscriptions FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS tenant_isolation ON fms.webhook_subscriptions;
CREATE POLICY tenant_isolation ON fms.webhook_subscriptions
FOR ALL
USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

-- **REVOKE 那一行不是多餘的**：007 的 `ALTER DEFAULT PRIVILEGES` 在 CREATE 的
-- 那一刻就把全部權限給了 fms_app（070 的自我驗證抓到過同一件事）。
-- 先全收回，再逐欄給 —— 這是「fms_app 讀不到 signing_secret」的實現方式。
REVOKE ALL ON fms.webhook_subscriptions FROM fms_app;
GRANT SELECT (id, tenant_id, url, event_types, description, is_active,
              consecutive_failures, disabled_at, disabled_reason,
              last_success_at, last_failure_at, last_error,
              created_by, created_at, updated_at)
  ON fms.webhook_subscriptions TO fms_app;
-- INSERT 需要包含 signing_secret：金鑰由 API 產生並回傳一次。
-- 欄位級的 INSERT 與 SELECT 是**分開的**權限，所以「寫得進去、讀不出來」成立。
GRANT INSERT (tenant_id, url, event_types, signing_secret, description,
              is_active, created_by)
  ON fms.webhook_subscriptions TO fms_app;
-- UPDATE 不含 signing_secret：換金鑰目前沒有端點，而讓 API 改得動它就等於
-- 它可以先寫入一個已知值、再用那個值驗證別人的簽章。
GRANT UPDATE (url, event_types, description, is_active, updated_at,
              disabled_at, disabled_reason)
  ON fms.webhook_subscriptions TO fms_app;

-- -----------------------------------------------------------------------------
-- 扇出：事件 → notifications（channel = WEBHOOK）
-- -----------------------------------------------------------------------------
-- 回傳建立的筆數。0 有三個原因，而呼叫端要能分辨（handler 會回報）：
-- 沒有訂閱這個事件型別、訂閱都停用了、或這個租戶根本沒有訂閱。
--
-- `body` 放整個 payload 的 JSON 文字：dispatcher 送出的就是它，
-- 而簽章也是對它算的。**不在這裡組簽章** —— 金鑰在 worker 那一側，
-- 而且簽章要包含時間戳（防重放），那是送出時才知道的東西。
CREATE OR REPLACE FUNCTION fms.fanout_webhooks(
  p_event_id     bigint,
  p_tenant_id    uuid,
  p_event_type   text,
  p_aggregate_type text,
  p_aggregate_id uuid,
  p_payload      jsonb
) RETURNS integer
LANGUAGE plpgsql
AS $$
DECLARE
  v_count integer := 0;
BEGIN
  INSERT INTO fms.notifications
         (tenant_id, recipient_address, channel, subject, body,
          entity_type, entity_id, priority, status, source_event_id)
  SELECT s.tenant_id,
         s.url,
         'WEBHOOK',
         p_event_type,
         jsonb_build_object(
           'event_type', p_event_type,
           'aggregate_type', p_aggregate_type,
           'aggregate_id', p_aggregate_id,
           'tenant_id', p_tenant_id,
           'payload', p_payload
         )::text,
         'WEBHOOK_SUBSCRIPTION',
         s.id,
         'NORMAL',
         'QUEUED',
         p_event_id
    FROM fms.webhook_subscriptions s
   WHERE s.tenant_id = p_tenant_id
     AND s.is_active
     AND p_event_type = ANY(s.event_types)
  -- 幂等：見上面 uq_notifications_webhook_event 的說明。重放時這裡會是 0 筆，
  -- 而那是正確的答案 —— 這一輪沒有新的投遞要建立。
  ON CONFLICT DO NOTHING;

  GET DIAGNOSTICS v_count = ROW_COUNT;
  RETURN v_count;
END;
$$;

COMMENT ON FUNCTION fms.fanout_webhooks(bigint, uuid, text, text, uuid, jsonb) IS
  '事件 → notifications（channel = WEBHOOK）。投遞、重試與停放沿用 dispatcher '
  ' 既有的機制；entity_id 指回訂閱，讓 worker 找得到簽章金鑰。';

-- -----------------------------------------------------------------------------
-- 投遞結果回寫
-- -----------------------------------------------------------------------------
-- 由 worker 呼叫。放在 SQL 而不是 Rust，因為「成功歸零、失敗累加、達門檻自動
-- 停用」是一組必須原子完成的更新 —— 分成三個語句會在並發投遞下算錯計數。
--
-- `p_max_failures` 由呼叫端傳：那是一個維運參數（多少次失敗算對方下線了），
-- 不是資料庫的事實。
CREATE OR REPLACE FUNCTION fms.record_webhook_result(
  p_subscription_id uuid,
  p_success      boolean,
  p_error        text,
  p_max_failures int
) RETURNS boolean
LANGUAGE plpgsql
AS $$
DECLARE
  v_disabled boolean := false;
BEGIN
  IF p_success THEN
    UPDATE fms.webhook_subscriptions
       SET consecutive_failures = 0,
           last_success_at = clock_timestamp(),
           last_error = NULL,
           updated_at = clock_timestamp()
     WHERE id = p_subscription_id;
  ELSE
    UPDATE fms.webhook_subscriptions
       SET consecutive_failures = consecutive_failures + 1,
           last_failure_at = clock_timestamp(),
           last_error = left(coalesce(p_error, 'unknown'), 2000),
           -- 達門檻就自動停用。一個對方已經下線的訂閱不該無限期製造出站流量，
           -- 而且那些請求會一直佔著 dispatcher 的每一輪。
           is_active = CASE WHEN consecutive_failures + 1 >= p_max_failures
                            THEN false ELSE is_active END,
           disabled_at = CASE WHEN consecutive_failures + 1 >= p_max_failures
                                   AND disabled_at IS NULL
                              THEN clock_timestamp() ELSE disabled_at END,
           disabled_reason = CASE WHEN consecutive_failures + 1 >= p_max_failures
                                       AND disabled_at IS NULL
                                  THEN format('連續失敗 %s 次後自動停用；最後的錯誤：%s',
                                              consecutive_failures + 1,
                                              left(coalesce(p_error, 'unknown'), 500))
                                  ELSE disabled_reason END,
           updated_at = clock_timestamp()
     WHERE id = p_subscription_id
    RETURNING NOT is_active INTO v_disabled;
  END IF;
  RETURN coalesce(v_disabled, false);
END;
$$;

COMMENT ON FUNCTION fms.record_webhook_result(uuid, boolean, text, int) IS
  '回寫投遞結果。三個更新（歸零／累加／達門檻停用）必須原子完成，'
  ' 否則並發投遞會算錯連續失敗數。回傳 true 表示這一次讓訂閱被自動停用。';

REVOKE ALL ON FUNCTION fms.record_webhook_result(uuid, boolean, text, int)
  FROM PUBLIC, fms_app;
GRANT EXECUTE ON FUNCTION fms.record_webhook_result(uuid, boolean, text, int)
  TO fms_owner;

-- -----------------------------------------------------------------------------
-- 權限碼
-- -----------------------------------------------------------------------------
-- 契約寫 `tenant:update`。**沿用它，不新增。** 理由與 alarm:suppress 相反：
-- 那裡是「acknowledge 的持有者太廣」，而這裡 `tenant:update` 的持有者
-- （租戶管理員層級）正好就是該決定「本租戶的資料往哪裡送」的人。
-- webhook 訂閱是一條資料外送通道，它屬於租戶設定，不屬於任何場域。
--
-- 這裡不 INSERT 任何權限 —— 只在自我驗證裡確認 `tenant:update` 存在且是
-- TENANT 範圍，因為那是端點的授權前提。

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_src text;
BEGIN
  -- (1) FORCE RLS。
  IF NOT EXISTS (SELECT 1 FROM pg_class
                  WHERE oid = 'fms.webhook_subscriptions'::regclass
                    AND relrowsecurity AND relforcerowsecurity) THEN
    RAISE EXCEPTION '072 FAILED: webhook_subscriptions 沒有 FORCE ROW LEVEL SECURITY';
  END IF;

  -- (2) 只允許 https。一筆 http 的訂閱是一條明文的資料外送通道。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_webhook_url_https'
       AND conrelid = 'fms.webhook_subscriptions'::regclass
  ) THEN
    RAISE EXCEPTION '072 FAILED: ck_webhook_url_https 不存在';
  END IF;

  -- (3) **fms_app 讀不到 signing_secret，但讀得到 url。**
  --     這是整支 migration 最重要的一格：少了它，一次 `SELECT *` 就會把
  --     HMAC 金鑰放進 API 回應，而那讓簽章完全失去意義
  --     （任何人都能偽造我們的 webhook）。
  IF has_column_privilege('fms_app', 'fms.webhook_subscriptions',
                          'signing_secret', 'SELECT') THEN
    RAISE EXCEPTION
      '072 FAILED: fms_app 讀得到 signing_secret —— 一次 SELECT * 就會把 '
      'HMAC 金鑰放進 API 回應';
  END IF;
  IF NOT has_column_privilege('fms_app', 'fms.webhook_subscriptions',
                              'url', 'SELECT') THEN
    RAISE EXCEPTION '072 FAILED: fms_app 讀不到 url —— 清單端點會壞';
  END IF;
  -- 寫得進去（建立時要能存金鑰）。
  IF NOT has_column_privilege('fms_app', 'fms.webhook_subscriptions',
                              'signing_secret', 'INSERT') THEN
    RAISE EXCEPTION '072 FAILED: fms_app 寫不了 signing_secret —— 無法建立訂閱';
  END IF;
  -- **改不動**（換金鑰沒有端點；能改就能先寫入已知值再偽造簽章）。
  IF has_column_privilege('fms_app', 'fms.webhook_subscriptions',
                          'signing_secret', 'UPDATE') THEN
    RAISE EXCEPTION
      '072 FAILED: fms_app 改得動 signing_secret —— 它可以先寫入一個已知的'
      '金鑰，之後用那個金鑰偽造簽章';
  END IF;

  -- (4) 扇出函式只挑啟用中、且訂閱了該事件型別的列。
  --     少了 `is_active`，停用的訂閱還會收到事件 —— 那讓「停用」變成假的。
  SELECT regexp_replace(prosrc, '--[^\n]*', '', 'g') INTO v_src
    FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
   WHERE n.nspname = 'fms' AND p.proname = 'fanout_webhooks';
  IF v_src IS NULL THEN
    RAISE EXCEPTION '072 FAILED: 找不到 fms.fanout_webhooks';
  END IF;
  IF v_src NOT LIKE '%s.is_active%' THEN
    RAISE EXCEPTION
      '072 FAILED: fanout_webhooks 沒有過濾 is_active —— 停用的訂閱還會收到事件';
  END IF;
  IF v_src NOT LIKE '%ANY(s.event_types)%' THEN
    RAISE EXCEPTION
      '072 FAILED: fanout_webhooks 沒有比對 event_types —— 訂閱一個事件會收到全部';
  END IF;
  -- entity_id 必須指回訂閱：worker 靠它找簽章金鑰。
  IF v_src NOT LIKE '%WEBHOOK_SUBSCRIPTION%' THEN
    RAISE EXCEPTION
      '072 FAILED: fanout_webhooks 沒有寫 entity_type —— worker 找不到金鑰';
  END IF;

  -- (4b) **幂等索引必須存在。** relay 是 at-least-once，少了它每次重放都會
  --      對客戶端再送一次同樣的事件，而客戶端分不出那是新事件還是重放。
  --      041 的索引擋不住（webhook 的 recipient_user_id 是 NULL）。
  IF NOT EXISTS (
    SELECT 1 FROM pg_indexes
     WHERE schemaname = 'fms' AND indexname = 'uq_notifications_webhook_event'
       AND indexdef LIKE '%entity_id%'
       AND indexdef LIKE '%WEBHOOK%'
  ) THEN
    RAISE EXCEPTION
      '072 FAILED: uq_notifications_webhook_event 不存在或沒有以 entity_id 為維度 '
      '—— relay 重放會對客戶端雙送';
  END IF;
  IF v_src NOT LIKE '%ON CONFLICT DO NOTHING%' THEN
    RAISE EXCEPTION
      '072 FAILED: fanout_webhooks 沒有 ON CONFLICT DO NOTHING —— 重放會拋錯而'
      '不是安靜地跳過，於是事件會被無限重試';
  END IF;
  IF v_src NOT LIKE '%source_event_id%' THEN
    RAISE EXCEPTION
      '072 FAILED: fanout_webhooks 沒有寫 source_event_id —— 幂等索引形同不存在';
  END IF;

  -- (5) 自動停用必須寫明原因。分不出「客戶關的」與「系統關的」時，
  --     處置完全不同的兩件事會被當成一件。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_webhook_disabled_reason'
       AND conrelid = 'fms.webhook_subscriptions'::regclass
  ) THEN
    RAISE EXCEPTION '072 FAILED: ck_webhook_disabled_reason 不存在';
  END IF;

  -- (6) 端點的授權前提：tenant:update 存在且是 TENANT 範圍。
  IF NOT EXISTS (
    SELECT 1 FROM fms.permissions
     WHERE code = 'tenant:update' AND min_scope_level = 'TENANT'
  ) THEN
    RAISE EXCEPTION
      '072 FAILED: tenant:update 不存在或不是 TENANT 範圍 —— '
      'webhook 訂閱是租戶級的資料外送設定，不該落到場域層級';
  END IF;

  -- (7) 空的 event_types 要擋掉：那是一筆永遠不會觸發、但在清單裡看起來
  --     正常的訂閱。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_webhook_event_types_present'
       AND conrelid = 'fms.webhook_subscriptions'::regclass
  ) THEN
    RAISE EXCEPTION '072 FAILED: ck_webhook_event_types_present 不存在';
  END IF;

  RAISE NOTICE '072 OK：webhook_subscriptions（FORCE RLS、https、'
               'fms_app 讀不到也改不動 signing_secret）、fanout_webhooks 過濾'
               ' is_active 與 event_types、自動停用要寫原因'
               '（簽章與投遞的行為驗證在 webhooks_slice.rs）';
END;
$$;

COMMIT;
