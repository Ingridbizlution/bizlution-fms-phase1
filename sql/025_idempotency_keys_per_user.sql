-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 025: 冪等鍵綁定使用者
-- =============================================================================
-- 補的是 docs/security-review-open-items.md 第 1 項。
--
-- 001 的主鍵是 (tenant_id, idempotency_key, endpoint) —— **沒有 user_id**。
-- 後果是同一租戶內任何一個使用者只要取得（或猜到）別人的 Idempotency-Key，
-- 送同樣的 body 就會拿到對方那次請求的完整回應，即使他本來無權執行該操作
-- （應用層的回放發生在授權檢查之前）。
--
-- 鍵通常是客戶端產生的 uuid，猜不到；但它會出現在客戶端 log、
-- 行動端當機報告、以及任何攔截到請求的中間層。冪等鍵不是機密，
-- 所以不該具備「憑鍵取回應」的能力。
--
-- 修法：把 user_id 納入主鍵。
--
-- 為什麼是主鍵而不是「多存一欄再比對」：
--   若 user_id 只是一個附加欄位、主鍵不變，那麼兩個使用者在同一端點用了
--   同一個鍵字串時，第二個人會撞到既有列，而應用層只能回 422
--   IDEMPOTENCY_KEY_REUSED。那有兩個問題：
--     * 它是一個弱預言機 —— 「這個鍵字串已經有人用過」本身是資訊
--     * 它會拒絕一個完全無辜的使用者（鍵字串相同不是他的錯）
--   納入主鍵之後兩列並存，兩個人各自獨立，沒有任何交互。
--
-- -----------------------------------------------------------------------------
-- 部署注意：既有列會被刪除
-- -----------------------------------------------------------------------------
-- 既有列無法歸屬到任何使用者（那個資訊從未被記下），因此不可能回填。
-- 這張表本來就是 24 小時的暫存（`expires_at`，`idx_idempotency_keys_expiry`），
-- 內容依設計是可丟棄的，所以刪除是唯一誠實的選項。
--
-- **代價要說清楚**：套用本 migration 之後的 24 小時內，若客戶端重送一個
-- 部署前發出的鍵，那次重送會被視為全新請求而**真的再執行一次** ——
-- 也就是可能產生重複的預約／工單／資產。
--
-- 兩件事讓這個窗可控：
--   * 預約有排他約束、資產與工單有各自的唯一碼，重複建立多半會直接撞約束
--   * 目前尚未上線（見 docs/WBS-rebaseline.md），實務上這張表是空的
-- 若日後在生產環境重跑類似變更，該做的是選在流量低點部署，而不是
-- 試圖回填一個從未存在的欄位。
--
-- 依賴：001（idempotency_keys）、007（tenant_isolation 與授權）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- -----------------------------------------------------------------------------
-- (1) 清空無法歸屬的既有列
-- -----------------------------------------------------------------------------
-- 需要平台情境：FORCE RLS 之下連 fms_owner 都被 tenant_isolation 過濾，
-- 沒有情境的 DELETE 會靜默影響 0 列，然後 (2) 的 SET NOT NULL 才會失敗 ——
-- 而那時的錯誤訊息完全看不出真正的原因。
DO $$
DECLARE v_removed bigint;
BEGIN
  PERFORM set_config('app.is_platform', 'on', true);
  WITH gone AS (DELETE FROM fms.idempotency_keys RETURNING 1)
  SELECT count(*) INTO v_removed FROM gone;
  PERFORM set_config('app.is_platform', 'off', true);

  IF v_removed > 0 THEN
    RAISE WARNING '025：已刪除 % 筆無法歸屬使用者的冪等鍵。'
                  '接下來 24 小時內若有客戶端重送部署前的鍵，該請求會被重新執行'
                  '（見本檔的部署注意）', v_removed;
  ELSE
    RAISE NOTICE '025：沒有既有的冪等鍵需要處理';
  END IF;
END;
$$;

-- -----------------------------------------------------------------------------
-- (2) 加入 user_id 並改主鍵
-- -----------------------------------------------------------------------------
-- 刻意不加 users 的外鍵：001 對這張表的 tenant_id 也沒有加，維持一致。
-- 更實際的理由是外鍵會讓刪除使用者被一張暫存表擋住，
-- 而這些列 24 小時後就該消失。
ALTER TABLE fms.idempotency_keys
  ADD COLUMN user_id uuid NOT NULL;

ALTER TABLE fms.idempotency_keys
  DROP CONSTRAINT idempotency_keys_pkey;

ALTER TABLE fms.idempotency_keys
  ADD CONSTRAINT idempotency_keys_pkey
  PRIMARY KEY (tenant_id, user_id, idempotency_key, endpoint);

COMMENT ON COLUMN fms.idempotency_keys.user_id IS
  '發出這次請求的使用者。屬於主鍵：冪等鍵不是機密，不得讓同租戶的其他人憑鍵取回應。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 除了斷言主鍵欄位，也實際驗證「兩個使用者可以用同一個鍵字串」——
-- 那是把 user_id 放進主鍵（而非只存一欄）真正要換來的性質，
-- 只檢查 pg_constraint 看不出來。
DO $$
DECLARE
  v_cols text;
  v_tenant uuid := '00000000-0000-4000-8000-0000000025aa';
BEGIN
  SELECT string_agg(a.attname, ',' ORDER BY k.ord)
    INTO v_cols
    FROM pg_constraint c
    CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord)
    JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum
   WHERE c.conrelid = 'fms.idempotency_keys'::regclass AND c.contype = 'p';

  IF v_cols <> 'tenant_id,user_id,idempotency_key,endpoint' THEN
    RAISE EXCEPTION '025 FAILED: 主鍵欄位是 %，預期 tenant_id,user_id,idempotency_key,endpoint', v_cols;
  END IF;

  PERFORM fms.set_context(v_tenant, '00000000-0000-4000-8000-0000000025b1');
  INSERT INTO fms.idempotency_keys
         (tenant_id, user_id, idempotency_key, endpoint, request_hash)
  VALUES (v_tenant, '00000000-0000-4000-8000-0000000025b1', 'shared-key',
          'POST /self-test', repeat('0', 64));

  -- 同租戶、同鍵字串、同端點，但不同使用者 —— 必須成功
  INSERT INTO fms.idempotency_keys
         (tenant_id, user_id, idempotency_key, endpoint, request_hash)
  VALUES (v_tenant, '00000000-0000-4000-8000-0000000025b2', 'shared-key',
          'POST /self-test', repeat('0', 64));

  -- 同使用者重複 —— 必須被主鍵擋下
  BEGIN
    INSERT INTO fms.idempotency_keys
           (tenant_id, user_id, idempotency_key, endpoint, request_hash)
    VALUES (v_tenant, '00000000-0000-4000-8000-0000000025b1', 'shared-key',
            'POST /self-test', repeat('0', 64));
    RAISE EXCEPTION '025 FAILED: 同一使用者的重複鍵竟然寫入成功';
  EXCEPTION WHEN unique_violation THEN
    NULL;  -- 預期
  END;

  DELETE FROM fms.idempotency_keys WHERE tenant_id = v_tenant;
  PERFORM set_config('app.tenant_id', '', true);
  PERFORM set_config('app.user_id', '', true);

  RAISE NOTICE '025 OK: user_id 已納入主鍵，同鍵字串在不同使用者間互不干擾';
END;
$$;

COMMIT;
