-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 024: 讓 auth_events 在「還沒有租戶情境」時可寫入
-- =============================================================================
-- 補的是 docs/security-review-open-items.md 第 4 項的資料層前提。
--
-- 002 建了 fms.auth_events（schema 完整、兩個索引都在），但沒有任何程式碼
-- 寫入它。要開始寫入時撞到一個結構問題：
--
--   auth_events 有 tenant_id 欄位，因此 007 的迴圈為它建了
--     tenant_isolation FOR ALL
--       USING      (is_platform_context() OR tenant_id = current_tenant_id())
--       WITH CHECK (is_platform_context() OR tenant_id = current_tenant_id())
--   並且 ENABLE + FORCE ROW LEVEL SECURITY。
--
--   而「登入失敗」這件事**發生在租戶情境存在之前**：
--     * tenant_code 不存在時，根本沒有 tenant_id 可寫 → WITH CHECK 為 false
--     * 就算 tenant_code 解析成功，事件也必須在**認證失敗的交易之外**寫入，
--       否則它會跟著那個交易一起回滾 —— 一筆會被自己抹掉的稽核記錄
--       等於沒有記錄。而交易外的連線沒有情境。
--
--   結果是 fms_app 在登入路徑上對 auth_events 的每一次 INSERT 都被 RLS 擋掉。
--   這不是「忘記寫程式」，是資料層先擋住了。
--
-- 修法：新增一條**只允許附加登入事件、且只在無租戶情境下成立**的 INSERT 政策。
--
-- 為什麼條件是 `current_tenant_id() IS NULL` 而不是 `tenant_id IS NULL`：
--   我們想記下的不只是「無租戶的失敗」。tenant_code 解析成功、密碼錯誤的
--   那一筆**必須帶 tenant_id**，否則租戶的管理員永遠看不到自己帳號被試密碼
--   （tenant_isolation 的 USING 讓 tenant_id 為 NULL 的列只有平台情境讀得到）。
--   因此判定的是「呼叫端有沒有情境」，而不是「這一列有沒有 tenant_id」。
--
-- 這條政策放寬了什麼，說清楚：
--   一個**尚未認證**的 fms_app session 可以附加一筆 event_type 為
--   LOGIN_SUCCESS／LOGIN_FAILED 的列，且該列的 tenant_id 未經驗證。
--   也就是說能執行任意 SQL 的攻擊者可以往任一租戶的登入軌塞假記錄。
--
--   兩個理由讓這個代價可接受：
--     1. 已認證的連線**不受**這條政策影響（它有情境，落回 tenant_isolation，
--        只能寫自己租戶）。放寬只存在於登入這一條路徑上。
--     2. 能以 fms_app 執行任意 SQL 的攻擊者，在此之前就已經可以對自己租戶
--        偽造登入軌（007 一直有 INSERT 授權）。真正守住的是**不可篡改**：
--        023 讓 UPDATE／DELETE 保持撤銷，因此既有的列刪不掉也改不了。
--        稽核軌的價值在「下手的人抹不掉」，而那個性質沒有被動搖。
--
--   刻意收斂的部分：
--     * event_type 白名單只有兩個值。LOGOUT／MFA_CHALLENGE／TOKEN_REFRESH
--       都發生在已認證之後，會有情境，不需要這條政策 —— 把它們排除掉，
--       這條洞就只有登入這一個用途。
--     * 只有 FOR INSERT。讀取仍完全由 tenant_isolation 決定：
--       租戶讀得到自己的列，tenant_id 為 NULL 的列只有平台情境看得到。
--
-- 為什麼不用 SECURITY DEFINER 函式（014／021 的做法）：
--   那會多一個能繞過 RLS 的函式，而它要達成的效果與這條政策相同。
--   013 之後每新增一個 DEFINER 函式都是一個要獨立審查的授權面
--   （021 正是在那裡寫錯了守衛，見 023）。一條 FOR INSERT、
--   帶 event_type 白名單的政策，判定式攤在 pg_policies 裡看得見，
--   比一段 plpgsql 好審。
--
-- 依賴：002（auth_events）、007（tenant_isolation 與授權）、023（append-only）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

DROP POLICY IF EXISTS auth_events_preauth_append ON fms.auth_events;
CREATE POLICY auth_events_preauth_append ON fms.auth_events
  FOR INSERT
  WITH CHECK (
    fms.current_tenant_id() IS NULL
    AND event_type IN ('LOGIN_SUCCESS', 'LOGIN_FAILED')
  );

COMMENT ON TABLE fms.auth_events IS
  '認證事件軌。append-only（023 撤銷 fms_app 的 UPDATE／DELETE）。'
  ' 登入事件由應用層在認證交易之外、無租戶情境的連線上寫入，'
  ' 依 024 的 auth_events_preauth_append 政策放行；'
  ' tenant_id 為 NULL 的列代表 tenant_code 無法解析，僅平台情境可讀。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 只斷言政策存在會漏掉最重要的那件事：它到底放行了什麼。因此實際試四種寫入，
-- 並要求恰好前兩種成功。
--
-- 為什麼以 fms_owner（migration 執行者）測試就足夠，不必切到 fms_app：
--   007 對 auth_events 下了 FORCE ROW LEVEL SECURITY，而 FORCE 的語意就是
--   「政策對表的擁有者也適用」。因此只要 app.is_platform 是 off，
--   fms_owner 在這張表上受到的政策判定與 fms_app 完全相同 ——
--   兩者唯一的差異在 is_platform_context() 的角色條件，而那一半在
--   GUC 為 off 時根本不會被問到。
--   （順帶說明為何不用 SET LOCAL ROLE fms_app：fms_owner 不是 fms_app 的
--    成員，SET ROLE 會直接被拒。這是刻意的角色隔離，不該為了測試放寬。）
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
     WHERE schemaname='fms' AND tablename='auth_events'
       AND policyname='auth_events_preauth_append'
  ) THEN
    RAISE EXCEPTION '024 FAILED: auth_events_preauth_append 未建立';
  END IF;

  PERFORM set_config('app.tenant_id', '', true);
  PERFORM set_config('app.user_id', '', true);
  PERFORM set_config('app.is_platform', 'off', true);

  -- (1) 無情境 + 無 tenant_id 的失敗記錄：應成功
  INSERT INTO fms.auth_events (event_type, result, failure_reason)
  VALUES ('LOGIN_FAILED', 'FAILURE', '024 self-test: unknown tenant');

  -- (2) 無情境 + 帶 tenant_id：應成功（這一筆租戶自己才看得到）
  INSERT INTO fms.auth_events (tenant_id, event_type, result, failure_reason)
  VALUES ('00000000-0000-4000-8000-0000000024aa', 'LOGIN_FAILED', 'FAILURE',
          '024 self-test: bad password');

  -- (3) 白名單外的 event_type：應被擋
  BEGIN
    INSERT INTO fms.auth_events (event_type) VALUES ('LOGOUT');
    RAISE EXCEPTION '024 FAILED: 無情境下竟能寫入 LOGOUT（白名單失效）';
  EXCEPTION WHEN insufficient_privilege THEN
    NULL;  -- 預期
  END;

  -- (4) 有租戶情境時，這條政策不得成為跨租戶寫入的後門
  PERFORM set_config('app.tenant_id', '00000000-0000-4000-8000-0000000024bb', true);
  BEGIN
    INSERT INTO fms.auth_events (tenant_id, event_type)
    VALUES ('00000000-0000-4000-8000-0000000024cc', 'LOGIN_SUCCESS');
    RAISE EXCEPTION '024 FAILED: 已設情境的連線竟能寫入別的 tenant_id';
  EXCEPTION WHEN insufficient_privilege THEN
    NULL;  -- 預期：落回 tenant_isolation
  END;

  -- 清掉自我測試的兩列。需要平台情境：FORCE RLS 之下 fms_owner 也被
  -- tenant_isolation 過濾，沒有情境的 DELETE 會靜默影響 0 列。
  PERFORM set_config('app.tenant_id', '', true);
  PERFORM set_config('app.is_platform', 'on', true);
  DELETE FROM fms.auth_events WHERE failure_reason LIKE '024 self-test:%';
  IF EXISTS (SELECT 1 FROM fms.auth_events WHERE failure_reason LIKE '024 self-test:%') THEN
    RAISE EXCEPTION '024 FAILED: 自我測試的列沒有清掉';
  END IF;
  PERFORM set_config('app.is_platform', 'off', true);

  RAISE NOTICE '024 OK: 無情境可附加登入事件（含帶 tenant_id 的），白名單與跨租戶寫入皆被擋';
END;
$$;

COMMIT;
