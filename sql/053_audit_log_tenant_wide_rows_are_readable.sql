-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 053: 租戶級稽核事件連租戶管理員都讀不到
-- =============================================================================
-- 這個缺陷是在做 `GET /audit-log` 時量出來的，而它比那支端點嚴重。
--
-- -----------------------------------------------------------------------------
-- 症狀：租戶管理員看得到 34 列裡的 7 列
-- -----------------------------------------------------------------------------
-- 照 `begin_tenant_tx` 的真實路徑（`set_context` + `set_facility_scope`），
-- 以示範租戶的 TENANT_ADMIN 查 `fms.audit_log`：
--
--     總列數（該租戶）      34
--     看得到                 7
--     facility_id IS NULL   27  ← 一列都看不到
--
-- 那 27 列的來源：`USERS`、`USER_ROLE_ASSIGNMENTS`、`ROLES`、
-- `ROLE_PERMISSIONS`、`IDENTITY_PROVIDERS`、`TENANTS`。
-- **整個身分與授權的稽核軌跡，對租戶管理員是看不見的** ——
-- 而「誰把誰變成管理員」正是稽核最該回答的問題。
--
-- -----------------------------------------------------------------------------
-- 我一開始把成因判斷錯了，這裡記下來
-- -----------------------------------------------------------------------------
-- 現行述詞（046）是：
--
--     USING (is_platform_context()
--            OR current_facility_ids() IS NULL
--            OR facility_id = ANY (current_facility_ids()))
--
-- 我的第一版 053 認定這是「046 手抄了一份 `facility_in_scope()` 而漏掉
-- `p_facility_id IS NULL`」，於是直接換回那支函式。
--
-- **那是錯的。** `audit_trail_slice.rs` 有一格測試釘著相反的意圖：
--
--     「租戶層的稽核列（facility_id IS NULL）不屬於任何場域，
--       場域受限的讀者不該看到 —— 046 之前 facility_in_scope(NULL) 會放行」
--
-- 也就是說 046 **刻意**不用 `facility_in_scope()`，因為那支函式對 NULL 放行，
-- 而那會讓一個只管一個場域的管理員讀到整個租戶的身分變更紀錄。
-- 那個判斷是對的。換回去會讓那一格測試失敗 —— 它確實失敗了，
-- 而那正是它存在的理由。
--
-- -----------------------------------------------------------------------------
-- 真正的成因：`app.facility_ids` 分不出 TENANT_ADMIN 與 FACILITY_ADMIN
-- -----------------------------------------------------------------------------
-- 046 的規則對，實作卻連租戶管理員一起擋掉了：
--
--   * `set_facility_scope` 一律寫入一份**具體清單**（連「沒有任何角色」都寫成
--     全零 uuid 哨兵），所以中間那條 `IS NULL` 逃生口在 `begin_tenant_tx`
--     底下永遠不成立。
--   * TENANT_ADMIN 拿到的是**全部場域**的清單，而 `NULL = ANY('{…}')`
--     的結果是 **NULL**，不是 true。RESTRICTIVE 政策判 NULL 就是不通過。
--
-- **050 的檔頭已經把這件事寫下來了**，一字不差：
--
--     「更根本的問題：`app.facility_ids` 分不出 TENANT_ADMIN 與
--       FACILITY_ADMIN。兩者都是非 NULL 清單，只差長度。」
--
-- 050 為此建了 `fms.tenant_wide_write_allowed()` —— 去讀
-- `user_role_assignments.scope_type`，那是資料不是 GUC（013 的教訓：
-- 自行宣稱的 GUC 不構成安全邊界）。它當時只用在寫入端的 8 張表，
-- 沒有回頭看 `audit_log` 的讀取端有同一個問題。
--
-- -----------------------------------------------------------------------------
-- 修法：加一條分支，不動既有的三條
-- -----------------------------------------------------------------------------
--     OR (facility_id IS NULL AND fms.tenant_wide_write_allowed())
--
-- 兩邊的意圖同時成立：
--   * 場域受限的讀者**仍然**看不到租戶級列（046 的規則，測試繼續守著）
--   * 租戶範圍的讀者看得到（新增的，`audit_log_slice.rs` 守著）
--
-- 函式名字寫的是 `write`，這裡用在讀取端。**刻意共用同一支**而不是複製一份
-- ——「這個人的角色指派裡有沒有 TENANT 範圍」是同一個問題，
-- 而我這個 migration 的第一版就是「以為某處手抄了判定」而差點自己再抄一份。
--
-- 成本不是問題：它 `STABLE` 且**無參數**，因此在一次查詢裡只求值一次，
-- 不是每列一次。（第一版的檔頭以「每列一次子查詢」為由否決了它，那也是錯的。）
--
-- 依賴：029（audit_log）、046（現行政策）、050（tenant_wide_write_allowed）。
-- =============================================================================

-- 不需要 `SET app.is_platform = 'on'`（031 對改動身分相關表的要求）：
-- 這個檔案只改政策、只讀 `pg_policy`，不碰任何受 RLS 管的業務資料。
BEGIN;
SET search_path = fms, public;

-- 維持 RESTRICTIVE 與 cmd = SELECT，只多一條 OR 分支。
-- （`check-isolation.sh` 的 A2 格會檢查所有 `facility_scope%` 政策都是
--  RESTRICTIVE —— 046 那次的教訓是漏寫 `AS RESTRICTIVE` 會被 OR 進
--  tenant_isolation，於是整張表在無情境時讀得到。）
DROP POLICY IF EXISTS facility_scope ON fms.audit_log;

CREATE POLICY facility_scope ON fms.audit_log
AS RESTRICTIVE FOR SELECT
USING (fms.is_platform_context()
       OR fms.current_facility_ids() IS NULL
       OR facility_id = ANY (fms.current_facility_ids())
       OR (facility_id IS NULL AND fms.tenant_wide_write_allowed()));

-- -----------------------------------------------------------------------------
-- 自我驗證：**結構的，不是行為的**
-- -----------------------------------------------------------------------------
-- 第一版在這裡跑行為驗證（插三列探針、切成單場域情境、數看得到幾列）。
-- 它在 `make test-template` 掛掉了：CORE 階段跑在 009 之前，那時一個場域都
-- 沒有，而我把「找不到場域」寫成 EXCEPTION —— 於是整條 migration 鏈斷在這裡。
--
-- 那個 EXCEPTION 是對的（靜默跳過看起來跟通過一樣），錯的是**層次**：
-- 政策改動必須在 CORE（沒有 seed 的正式資料庫也要正確），
-- 而行為驗證需要資料。硬把兩者塞在一起，只能二選一地犧牲。
--
-- 所以拆開，而且**兩個方向各有守衛**：
--   * 這裡驗結構 —— 政策存在、RESTRICTIVE、僅 SELECT、
--     而且述詞真的引用了 `tenant_wide_write_allowed`。不依賴任何資料。
--   * 「租戶範圍讀得到」→ `audit_log_slice.rs` 的
--     `a_tenant_wide_audit_rows_are_visible`（走 HTTP 的真實路徑）
--   * 「場域範圍**仍然**讀不到」→ `audit_trail_slice.rs` 的
--     `a_facility_scoped_reader_cannot_see_tenant_level_audit_rows`
--     （046 建立的那一格，這次沒有動它 —— 它是我判斷錯誤時的煞車）
DO $$
DECLARE
  v_qual text;
  v_cmd  "char";
  v_perm boolean;
BEGIN
  SELECT pg_get_expr(polqual, polrelid), polcmd, polpermissive
    INTO v_qual, v_cmd, v_perm
    FROM pg_policy
   WHERE polrelid = 'fms.audit_log'::regclass AND polname = 'facility_scope';

  IF v_qual IS NULL THEN
    RAISE EXCEPTION '053 FAILED: audit_log 上找不到 facility_scope 政策';
  END IF;

  -- (1) 述詞必須問「這個人有沒有 TENANT 範圍」，而不是靠場域清單推測。
  --     `app.facility_ids` 分不出 TENANT_ADMIN 與 FACILITY_ADMIN（050 的結論），
  --     任何想用清單長度來判斷的啟發式在單場域租戶都會誤判。
  IF v_qual NOT LIKE '%tenant_wide_write_allowed%' THEN
    RAISE EXCEPTION
      '053 FAILED: facility_scope 沒有使用 fms.tenant_wide_write_allowed() —— '
      '述詞是「%」。少了它，租戶級稽核列連租戶管理員都讀不到', v_qual;
  END IF;

  -- (2) 場域比對必須還在。只留下 tenant-wide 那條分支的話，
  --     場域受限的讀者會看到別的場域的列。
  IF v_qual NOT LIKE '%current_facility_ids%' THEN
    RAISE EXCEPTION '053 FAILED: facility_scope 不再比對場域清單，場域隔離消失';
  END IF;

  -- (3) 必須仍是 RESTRICTIVE。PERMISSIVE 會被 OR 進 tenant_isolation，
  --     整條場域限制就等於不存在（046 的教訓，check-isolation.sh 的 A2 也在守）。
  IF v_perm THEN
    RAISE EXCEPTION '053 FAILED: facility_scope 變成 PERMISSIVE，場域限制等於不存在';
  END IF;

  -- (4) 必須仍只管 SELECT。若不小心放大成 ALL，這個 migration 就從
  --     「修讀取」變成「放寬寫入」—— 而檔頭明說它不碰寫入端。
  IF v_cmd <> 'r' THEN
    RAISE EXCEPTION
      '053 FAILED: facility_scope 的 cmd 是「%」而不是 SELECT —— '
      '這個 migration 不該影響寫入端', v_cmd;
  END IF;

  RAISE NOTICE '053 OK：facility_scope 仍比對場域清單，並額外放行'
               '「租戶範圍讀者 × 租戶級列」（行為驗證見 audit_log_slice.rs '
               '與 audit_trail_slice.rs）';
END;
$$;

COMMIT;
