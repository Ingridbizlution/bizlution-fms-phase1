-- =============================================================================
-- 066：報表匯出作業
-- =============================================================================
-- `POST /reports/{reportCode}:export` 的落地處。形狀刻意比照 054 的
-- `audit_exports`：outbox 觸發、這張表記狀態與結果、`requested_by` 是
-- **授權判定的輸入**而不是稽核用的裝飾。
--
-- -----------------------------------------------------------------------------
-- 為什麼是新的一張表，而不是加一欄到 audit_exports
-- -----------------------------------------------------------------------------
-- 兩者的**權限閘門不同**：稽核匯出要 `audit:export`（`is_dangerous`，
-- TENANT 範圍），報表匯出要 `report:export`（FACILITY 範圍）。共用一張表
-- 就必須在每一次讀寫時依 `kind` 分流成兩種權限判斷 —— 那是一個很容易
-- 漏掉一處的地方，而漏掉的後果是「拿得到報表權限的人讀得到稽核匯出」。
--
-- 型別分開讓權限判斷留在各自的端點裡，不需要任何分流。
--
-- 另外兩者的欄位也不一樣：報表匯出需要 `report_code` 與 `format`，
-- 稽核匯出不需要；而稽核匯出的 `filters` 與 `GET /audit-log` 的過濾條件
-- 對應，報表匯出的與各支報表的查詢參數對應。
--
-- -----------------------------------------------------------------------------
-- `report_code` 沒有做成外鍵指向一張目錄表
-- -----------------------------------------------------------------------------
-- 因為「有哪些報表」不是管理者定義的條件，而是程式碼的事實：每一支報表是
-- 一個獨立的 SQL 函式，有自己的參數與欄位。把它放進資料表只會讓「表裡有一
-- 列但沒有對應的函式」變成可能，而那一列會安靜地產出一個永遠 FAILED 的作業。
--
-- 白名單因此在應用層（`fms-report::export::REPORTS`），而
-- `report_export_slice.rs` 有一格斷言那份清單與**實際掛上路由的**
-- `GET /reports/*` 對得上 —— 兩邊分歧時是「可匯出但沒實作」或反之。
--
-- 這張表只用 CHECK 擋掉明顯不合的字串（長度與字元集），因為它的作用是
-- 防止髒資料，不是定義有哪些報表。
--
-- -----------------------------------------------------------------------------
-- `format` 的 CHECK 是 csv 與 xlsx
-- -----------------------------------------------------------------------------
-- 契約（ENDPOINTS.md）寫的是「匯出 xlsx/csv」，兩種都做。差別只在產檔那
-- 一步，查詢與收斂完全共用。
--
-- 依賴：054（同形的作業表，這裡刻意不繼承它）、065（四支報表函式）、
--       034（sla-compliance）、063（pm-compliance）、002（users／tenants）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

CREATE TABLE IF NOT EXISTS fms.report_exports (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id    uuid NOT NULL REFERENCES fms.tenants(id) ON DELETE CASCADE,
  -- 授權判定的輸入。worker 必須以這個人的身分重新注入情境再查報表函式 ——
  -- 那些函式是 SECURITY INVOKER，收斂靠的是呼叫者的情境。
  requested_by uuid NOT NULL REFERENCES fms.users(id) ON DELETE RESTRICT,
  -- 白名單在應用層，見檔頭。這裡只擋髒字串。
  report_code  text NOT NULL
                 CONSTRAINT ck_report_exports_code
                 CHECK (report_code ~ '^[a-z][a-z0-9-]{2,39}$'),
  format       text NOT NULL DEFAULT 'csv'
                 CONSTRAINT ck_report_exports_format
                 CHECK (format IN ('csv', 'xlsx')),
  -- 那支報表的查詢參數，原樣存下來。與 054 同一個理由：這組參數會隨報表
  -- 演進，每加一個就改一次表結構沒有意義。
  params       jsonb NOT NULL DEFAULT '{}'::jsonb,
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
  -- （054 的同一條約束，同一個理由。）
  CONSTRAINT ck_report_exports_result CHECK (
    (status <> 'COMPLETED' OR (object_key IS NOT NULL AND row_count IS NOT NULL))
    AND (status <> 'FAILED' OR error IS NOT NULL)
  )
);

CREATE INDEX IF NOT EXISTS idx_report_exports_tenant_time
  ON fms.report_exports (tenant_id, created_at DESC);

COMMENT ON TABLE fms.report_exports IS
  '報表匯出作業。outbox 觸發、這張表記狀態與結果。與 audit_exports 分開是'
  ' 因為權限閘門不同（report:export vs audit:export）—— 共用一張表就必須'
  ' 依 kind 分流權限判斷，而漏掉一處的後果是跨型別的越權讀取。';

COMMENT ON COLUMN fms.report_exports.requested_by IS
  '授權判定的輸入。報表函式是 SECURITY INVOKER，worker 必須以這個人的情境'
  ' 查詢（含 app.facility_ids），否則匯出的檔案會含他看不到的場域。';

-- -----------------------------------------------------------------------------
-- RLS
-- -----------------------------------------------------------------------------
-- 只有租戶隔離，沒有場域維度 —— 與 054 同一個判斷：這張表沒有 facility_id，
-- **內容**的場域收斂發生在產檔的時候（worker 以 requested_by 的情境查）。
-- 在這裡再加一層場域政策只會讓人以為收斂在這裡發生。
ALTER TABLE fms.report_exports ENABLE ROW LEVEL SECURITY;
ALTER TABLE fms.report_exports FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS tenant_isolation ON fms.report_exports;
CREATE POLICY tenant_isolation ON fms.report_exports
FOR ALL
USING (fms.is_platform_context() OR tenant_id = fms.current_tenant_id())
WITH CHECK (fms.is_platform_context() OR tenant_id = fms.current_tenant_id());

GRANT SELECT, INSERT, UPDATE ON fms.report_exports TO fms_app;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構的（這支跑在 CORE 階段，seed 009 還沒進來）。
-- **數字與收斂的行為驗證在 report_export_slice.rs。**
DO $$
BEGIN
  -- (1) FORCE RLS。少了它，fms_owner（表的擁有者）讀得到所有租戶的匯出。
  IF NOT EXISTS (SELECT 1 FROM pg_class
                  WHERE oid = 'fms.report_exports'::regclass
                    AND relrowsecurity AND relforcerowsecurity) THEN
    RAISE EXCEPTION '066 FAILED: report_exports 沒有 FORCE ROW LEVEL SECURITY';
  END IF;

  -- (2) 結果約束存在，而且指名了 object_key 與 row_count。
  --     少了它，「COMPLETED 但沒有檔案」會是一個合法的列。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_report_exports_result'
       AND conrelid = 'fms.report_exports'::regclass
       AND pg_get_constraintdef(oid) LIKE '%object_key%'
       AND pg_get_constraintdef(oid) LIKE '%row_count%'
  ) THEN
    RAISE EXCEPTION
      '066 FAILED: ck_report_exports_result 不存在或沒有指名 object_key／row_count';
  END IF;

  -- (3) 兩種格式都要在 CHECK 裡。契約寫的是 xlsx/csv，只做一種是把
  --     契約悄悄縮小 —— 而縮小之後的差別只有讀契約的人會發現。
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'ck_report_exports_format'
       AND conrelid = 'fms.report_exports'::regclass
       AND pg_get_constraintdef(oid) LIKE '%xlsx%'
       AND pg_get_constraintdef(oid) LIKE '%csv%'
  ) THEN
    RAISE EXCEPTION '066 FAILED: format 的 CHECK 沒有同時含 csv 與 xlsx';
  END IF;

  -- (4) `requested_by` 必須 NOT NULL。它是授權判定的輸入 ——
  --     可為 NULL 的話，一筆 NULL 的作業會讓 worker 無從收斂，
  --     而最自然的實作（跳過情境切換）會產出整個租戶的資料。
  IF EXISTS (
    SELECT 1 FROM pg_attribute
     WHERE attrelid = 'fms.report_exports'::regclass
       AND attname = 'requested_by' AND NOT attnotnull
  ) THEN
    RAISE EXCEPTION
      '066 FAILED: requested_by 可為 NULL —— 它是授權判定的輸入，不是裝飾';
  END IF;

  RAISE NOTICE '066 OK：report_exports 建立（FORCE RLS、結果約束、csv/xlsx、'
               'requested_by NOT NULL）；收斂與產檔的行為驗證在 '
               'report_export_slice.rs';
END;
$$;

COMMIT;
