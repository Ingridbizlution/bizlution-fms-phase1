-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 054: 稽核匯出作業
-- =============================================================================
-- `POST /audit-log:export` 的落地處。ENDPOINTS.md 對它的描述是
-- 「匯出稽核（非同步產檔）」，而非同步這件事在這裡是必要的而不是裝飾：
-- `audit_log` 是全庫唯一按月分割的表，一次合規匯出可能跨數百萬列。
--
-- -----------------------------------------------------------------------------
-- 為什麼要一張表，而不是只丟一筆 event_outbox
-- -----------------------------------------------------------------------------
-- `event_outbox` 已經有重試、退避與 `EventHandler` 分派，佇列的部分完全複用
-- （不新造第二套）。但它少了兩樣東西：
--
--   * **可輪詢的資源。** 客戶端要問「好了沒」，而 outbox 列在成功之後會被標為
--     PUBLISHED 並最終清掉 —— 那不是一個可以拿 id 回來查的東西。
--   * **結果。** 產出的物件鍵、列數、失敗原因都要留下來。
--
-- 所以是「outbox 負責觸發，audit_exports 負責狀態與結果」。
--
-- -----------------------------------------------------------------------------
-- `requested_by` 是這張表最重要的欄位
-- -----------------------------------------------------------------------------
-- worker 跑在平台情境下（它要跨租戶取用 outbox）。若它就這樣執行匯出查詢，
-- 產出的檔案會包含**發起者本來看不到的列** —— 一次匯出就繞過了 053 剛修好的
-- `audit_log.facility_scope`。
--
-- 因此 handler 必須以 `requested_by` 的身分重新注入情境再查。這個欄位不是
-- 稽核用的裝飾，它是**授權判定的輸入**。
-- `audit_export_slice.rs` 有一格專門驗這件事：場域受限的發起者匯出的檔案裡
-- 不能有別的場域的列。
--
-- -----------------------------------------------------------------------------
-- 為什麼沒有 `expires_at` 與自動清理
-- -----------------------------------------------------------------------------
-- 匯出檔會累積，而合規匯出通常有保存期限的要求 —— 但那個期限是**管理者
-- 該定義的條件**，不是這裡該寫死的數字。Phase 1 先不猜：檔案留著，
-- 清理由未來的保存政策決定。刻意記在這裡，而不是留一個空白。
--
-- 依賴：029（audit_log）、030 或更早（event_outbox）、002（users／tenants）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

CREATE TABLE IF NOT EXISTS fms.audit_exports (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id    uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  -- 授權判定的輸入，見檔頭。ON DELETE RESTRICT：帳號刪掉不該讓一份已產出的
  -- 匯出失去它的歸屬（而 users 目前也只有停用、沒有真刪）。
  requested_by uuid NOT NULL REFERENCES fms.users(id) ON DELETE RESTRICT,
  -- 與 GET /audit-log 相同的過濾條件，原樣存下來。
  -- 存 jsonb 而不是拆成欄位：這組條件會隨那支端點演進，
  -- 而每加一個過濾條件就改一次表結構沒有意義。
  filters      jsonb NOT NULL DEFAULT '{}'::jsonb,
  status       text NOT NULL DEFAULT 'PENDING'
                 CHECK (status IN ('PENDING','RUNNING','COMPLETED','FAILED')),
  object_key   text,
  row_count    bigint,
  error        text,
  created_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  started_at   timestamptz,
  completed_at timestamptz,
  -- 三個狀態各自該有什麼，讓資料庫說了算 —— 否則「COMPLETED 但沒有檔案」
  -- 這種列會安靜地存在，而客戶端拿到的是一個 200 加一個 null 連結。
  CONSTRAINT ck_audit_exports_result CHECK (
    (status <> 'COMPLETED' OR (object_key IS NOT NULL AND row_count IS NOT NULL))
    AND (status <> 'FAILED' OR error IS NOT NULL)
  )
);

CREATE INDEX IF NOT EXISTS idx_audit_exports_tenant_time
  ON fms.audit_exports (tenant_id, created_at DESC);

COMMENT ON TABLE fms.audit_exports IS
  '稽核匯出作業。outbox 觸發、這張表記狀態與結果。'
  ' requested_by 是授權判定的輸入 —— worker 必須以他的身分注入情境再查，'
  ' 否則匯出會繞過 audit_log 的 facility_scope（見 053）。';

-- -----------------------------------------------------------------------------
-- RLS
-- -----------------------------------------------------------------------------
-- 只有租戶隔離，沒有場域維度：這張表沒有 facility_id，而**內容**的場域收斂
-- 發生在產檔的時候（handler 以 requested_by 的情境查 audit_log）。
-- 在這裡再加一層場域政策只會讓人以為收斂在這裡發生。
ALTER TABLE fms.audit_exports ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.audit_exports FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS tenant_isolation ON fms.audit_exports;
CREATE POLICY tenant_isolation ON fms.audit_exports
FOR ALL
USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

GRANT SELECT, INSERT, UPDATE ON fms.audit_exports TO fms_app;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構的，不依賴 seed（053 的教訓：CORE 階段跑在 009 之前）。
DO $$
DECLARE v_n int;
BEGIN
  -- (1) FORCE RLS。少了它，fms_owner（表的擁有者）讀得到所有租戶的匯出。
  IF NOT EXISTS (SELECT 1 FROM pg_class
                  WHERE oid = 'fms.audit_exports'::regclass
                    AND relrowsecurity AND relforcerowsecurity) THEN
    RAISE EXCEPTION '054 FAILED: audit_exports 沒有 FORCE ROW LEVEL SECURITY';
  END IF;

  -- (2) 結果約束存在，而且**指名了正確的欄位**。
  --
  --     第一版在這裡真的 INSERT 一列來驗它擋得住。那被 RLS 擋下了 ——
  --     migration 沒有平台情境，`current_tenant_id()` 是 NULL，
  --     tenant_isolation 的 WITH CHECK 先一步拒絕。就算繞過 RLS，
  --     外鍵也會先於 CHECK 觸發（tenants／users 在 CORE 階段是空的）。
  --
  --     這是 053 記過的同一個層次問題：政策／約束必須在 CORE，
  --     行為驗證需要資料。所以這裡只驗結構，
  --     行為驗證在 `audit_export_slice.rs`（有真實租戶可用）。
  SELECT count(*) INTO v_n
    FROM pg_constraint
   WHERE conrelid = 'fms.audit_exports'::regclass
     AND conname = 'ck_audit_exports_result'
     AND pg_get_constraintdef(oid) LIKE '%object_key%'
     AND pg_get_constraintdef(oid) LIKE '%row_count%'
     AND pg_get_constraintdef(oid) LIKE '%error%';
  IF v_n <> 1 THEN
    RAISE EXCEPTION
      '054 FAILED: ck_audit_exports_result 不存在或沒有涵蓋 object_key／row_count／error'
      ' —— 少了它，「COMPLETED 但沒有檔案」的列會安靜地存在，'
      '而客戶端拿到一個 200 加一個 null 連結';
  END IF;

  -- (3) 四個狀態值都在。少一個的話 handler 會在寫入時才發現。
  SELECT count(*) INTO v_n
    FROM unnest(ARRAY['PENDING','RUNNING','COMPLETED','FAILED']) s
   WHERE pg_get_constraintdef(
           (SELECT oid FROM pg_constraint
             WHERE conrelid = 'fms.audit_exports'::regclass
               AND conname LIKE '%status%')) LIKE '%' || s || '%';
  IF v_n <> 4 THEN
    RAISE EXCEPTION '054 FAILED: status 的 CHECK 少了狀態值（只找到 %）', v_n;
  END IF;

  RAISE NOTICE '054 OK：audit_exports 建立，FORCE RLS 生效，結果約束已宣告'
               '（它擋不擋得住由 audit_export_slice.rs 驗）';
END;
$$;

COMMIT;
