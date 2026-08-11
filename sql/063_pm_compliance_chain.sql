-- =============================================================================
-- 063：把 PM 合規鏈接起來 —— occurrence 從來沒有終結狀態
-- =============================================================================
-- 為什麼需要它
--
-- `api/ENDPOINTS.md` 把 `GET /maintenance-occurrences` 註明為
-- 「排程執行紀錄（**PM 合規率來源**）」。而那張表的寫入者只有兩個：
--
--   fms-maintenance/repo.rs  claim_occurrence  → INSERT status = 'PLANNED'
--   fms-maintenance/repo.rs  mark_generated    → UPDATE status = 'GENERATED'
--
-- **沒有任何東西寫 COMPLETED／SKIPPED／MISSED，也沒有任何東西設 `completed_at`**
-- （全 repo 搜過，`completed_at` 在 occurrences 附近出現 0 次）。
--
-- 也就是說狀態永遠停在 GENERATED。一支讀 `status = 'COMPLETED'` 的
-- 「PM 準時完成率」會對每個租戶永遠回 **0%** —— 而它看起來會像一支正常的報表。
-- 這與遙測讀不回來、`devices.status` 永遠是 ONLINE 是同一個缺陷類型。
--
-- -----------------------------------------------------------------------------
-- COMPLETED 是**寫**的，MISSED 是**算**的
-- -----------------------------------------------------------------------------
-- 這兩者刻意用不同機制，理由是它們的性質不同：
--
--   * 「保養做完了」是一個**發生的事件** —— 有一個時刻、有一個行為者。
--     那種事實要寫下來。
--   * 「保養沒做」是一個**沒有發生的事** —— 沒有時刻、沒有行為者。
--     存它就需要有人定期去寫，而那個人不存在（就像 `DEVICE_OFFLINE` 掃描）。
--     所以 MISSED 由 `scheduled_for` 與現在比較算出來。
--
-- `maintenance_occurrences_status_check` 仍然允許 `'MISSED'`（004 定的），
-- 但**這支 migration 之後仍然沒有任何東西寫它**，那是刻意的。
-- 報表用算的；把它記在這裡，免得日後有人以為那是漏掉的功能。
--
-- -----------------------------------------------------------------------------
-- 觸發器綁 `completed_at` 的變化，不綁狀態名稱
-- -----------------------------------------------------------------------------
-- 工單的流程是 `… → COMPLETED → VERIFIED → CLOSED`，而只有 CLOSED 是終結。
-- 綁 CLOSED 是錯的：一張已完工但還沒歸檔的 PM 會被算成漏做，
-- 於是合規率變成在量文書作業的速度。
--
-- 綁 `work_orders.completed_at` 由 NULL 變成有值則是綁**事實本身**
-- （保養是什麼時候做完的），而它還順便處理了反方向：
-- `sql/044_reopen_clears_completion.sql` 在工單重開時會把 `completed_at`
-- 清成 NULL —— 於是這裡的 occurrence 自動退回 GENERATED。
-- 綁狀態名稱做不到那件事。
--
-- -----------------------------------------------------------------------------
-- 準時的容許窗由管理者定義
-- -----------------------------------------------------------------------------
-- `maintenance_plans` 原本沒有任何完工容許欄位（只有 `generate_lead_days`
-- ——「提前幾天產生工單」，語意不同，不能借用）。
--
-- 新增 `completion_grace_days`，預設 0。理由與 059 的 `reminder_days_before`
-- 相同：這是管理者該定義的條件。法定年檢給 0（一天都不能遲），
-- 月保養給 7（差幾天不影響設備）。一個全域數字會讓這兩者被同一把尺量。
--
-- -----------------------------------------------------------------------------
-- 被 skip 的不進主分母，但必須單獨看得見
-- -----------------------------------------------------------------------------
-- ADR-12 已經替 SLA 定過這個形狀（`excluded_*`／`substituted_*`）。這裡沿用。
--
-- 兩種極端都是謊：把 skip 算進分母，「全部跳過」得到 0%（看起來像糟糕的執行）；
-- 完全不計算，「全部跳過」得到 100%（看起來完美）。所以兩個數字並列，
-- 而 `excluded_skipped` 與 `on_time_rate` 一樣顯眼。
-- =============================================================================

BEGIN;

-- -----------------------------------------------------------------------------
-- (1) 每個計畫的完工容許窗
-- -----------------------------------------------------------------------------
ALTER TABLE fms.maintenance_plans
  ADD COLUMN IF NOT EXISTS completion_grace_days smallint NOT NULL DEFAULT 0;

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'ck_plans_completion_grace') THEN
    ALTER TABLE fms.maintenance_plans
      ADD CONSTRAINT ck_plans_completion_grace
      CHECK (completion_grace_days BETWEEN 0 AND 365);
  END IF;
END;
$$;

COMMENT ON COLUMN fms.maintenance_plans.completion_grace_days IS
  '完工容許窗（天）。0 = 排定日當天之後就算逾時（法定年檢）。'
  '由管理者定義 —— 不要在報表裡寫死一個全域數字。';

-- -----------------------------------------------------------------------------
-- (2) 把鏈接起來：工單完工 → occurrence 終結
-- -----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION fms.trg_sync_occurrence_completion()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  -- 只管掛著 occurrence 的工單。手開的維修工單與 PM 合規無關。
  IF NEW.maintenance_occurrence_id IS NULL THEN
    RETURN NEW;
  END IF;

  IF OLD.completed_at IS NULL AND NEW.completed_at IS NOT NULL THEN
    -- 保養做完了。`completed_at` 抄工單的，不是 `clock_timestamp()` ——
    -- 準時判定要用**保養實際完成的時刻**，不是資料庫寫這一列的時刻，
    -- 而 044 的重開流程會讓兩者差上幾天。
    UPDATE fms.maintenance_occurrences
       SET status = 'COMPLETED',
           completed_at = NEW.completed_at
     WHERE id = NEW.maintenance_occurrence_id;

  ELSIF OLD.completed_at IS NOT NULL AND NEW.completed_at IS NULL THEN
    -- 工單被重開（044）。退回 GENERATED —— 保養**還沒**做完，
    -- 而留著 COMPLETED 會讓合規率把一件重做中的事算成已完成。
    UPDATE fms.maintenance_occurrences
       SET status = 'GENERATED',
           completed_at = NULL
     WHERE id = NEW.maintenance_occurrence_id;
  END IF;

  RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS trg_work_orders_sync_occurrence ON fms.work_orders;
CREATE TRIGGER trg_work_orders_sync_occurrence
  AFTER UPDATE OF completed_at ON fms.work_orders
  FOR EACH ROW
  EXECUTE FUNCTION fms.trg_sync_occurrence_completion();

COMMENT ON FUNCTION fms.trg_sync_occurrence_completion() IS
  '工單的 completed_at 一有變化就同步 occurrence 的終結狀態（雙向）。'
  '綁欄位而不是綁狀態名稱：見 063 檔頭。';

-- -----------------------------------------------------------------------------
-- (3) 合規報表
-- -----------------------------------------------------------------------------
-- 形狀比照 034 的 `report_sla_compliance`：SQL 函式 + 薄 repo，
-- 分母分開、排除項具名。
CREATE OR REPLACE FUNCTION fms.report_pm_compliance(
  p_group_by text,                    -- 'facility' | 'plan' | 'none'
  p_from     date,
  p_to       date,
  p_grace_override int DEFAULT NULL   -- 覆寫每個計畫的容許窗（情境分析用）
) RETURNS TABLE (
  group_key            text,
  group_label          text,
  -- 主分母：期間內排定、且**已經有結果**的 occurrence。
  scheduled_total      bigint,
  completed_on_time    bigint,
  completed_late       bigint,
  -- 排定日已過而仍未完成 —— 這就是算出來的 MISSED。
  missed               bigint,
  -- 還在窗內、尚無結果。**不算分母** —— 它們還有機會。
  excluded_in_window   bigint,
  -- 被跳過的（含理由分佈）。不算分母，但必須看得見。
  excluded_skipped     bigint,
  skip_reasons         jsonb,
  -- 平均逾時天數（只算逾時的那些）。
  avg_days_late        double precision
)
LANGUAGE sql STABLE
AS $$
  WITH scoped AS (
    SELECT o.id, o.status, o.scheduled_for, o.completed_at,
           p.facility_id, p.id AS plan_id,
           p.code::text AS plan_code, p.name::text AS plan_name,
           f.name::text AS facility_name,
           o.skip_reason::text AS skip_reason,
           -- 容許窗：覆寫值優先，否則用計畫自己的。
           coalesce(p_grace_override, p.completion_grace_days) AS grace_days
      FROM fms.maintenance_occurrences o
      JOIN fms.maintenance_plans p ON p.id = o.plan_id
      LEFT JOIN fms.facilities f ON f.id = p.facility_id
     WHERE o.scheduled_for >= p_from
       AND o.scheduled_for < (p_to + 1)          -- p_to 含當天
  ), classified AS (
    SELECT s.*,
           s.scheduled_for + make_interval(days => s.grace_days) AS deadline,
           CASE
             WHEN s.status = 'SKIPPED' THEN 'skipped'
             WHEN s.completed_at IS NOT NULL
                  AND s.completed_at
                      <= s.scheduled_for + make_interval(days => s.grace_days)
               THEN 'on_time'
             WHEN s.completed_at IS NOT NULL THEN 'late'
             -- 尚未完成：過了容許窗就是漏做，還在窗內就先不判。
             WHEN now() > s.scheduled_for
                          + make_interval(days => s.grace_days) THEN 'missed'
             ELSE 'in_window'
           END AS verdict,
           CASE p_group_by
             WHEN 'facility' THEN s.facility_id::text
             WHEN 'plan'     THEN s.plan_id::text
             ELSE NULL
           END AS grp_key,
           CASE p_group_by
             WHEN 'facility' THEN s.facility_name
             WHEN 'plan'     THEN s.plan_code || ' ' || s.plan_name
             ELSE NULL
           END AS grp_label
      FROM scoped s
  ), reasons AS (
    -- 跳過的理由分佈，先按組算好再 join。
    -- 「全部跳過」時這是唯一能解釋 100%% 的東西。
    SELECT grp_key, jsonb_object_agg(r, n) AS skip_reasons
      FROM (SELECT grp_key, coalesce(skip_reason, '(未填)') AS r, count(*) AS n
              FROM classified
             WHERE verdict = 'skipped'
             GROUP BY 1, 2) x
     GROUP BY grp_key
  )
  SELECT c.grp_key AS group_key,
         max(c.grp_label) AS group_label,
         count(*) FILTER (WHERE c.verdict IN ('on_time','late','missed'))::bigint
           AS scheduled_total,
         count(*) FILTER (WHERE c.verdict = 'on_time')::bigint   AS completed_on_time,
         count(*) FILTER (WHERE c.verdict = 'late')::bigint      AS completed_late,
         count(*) FILTER (WHERE c.verdict = 'missed')::bigint    AS missed,
         count(*) FILTER (WHERE c.verdict = 'in_window')::bigint AS excluded_in_window,
         count(*) FILTER (WHERE c.verdict = 'skipped')::bigint   AS excluded_skipped,
         coalesce(r.skip_reasons, '{}'::jsonb) AS skip_reasons,
         avg(extract(epoch FROM (c.completed_at - c.deadline)) / 86400.0)
           FILTER (WHERE c.verdict = 'late') AS avg_days_late
    FROM classified c
    -- `IS NOT DISTINCT FROM`：group_by = 'none' 時 grp_key 兩邊都是 NULL，
    -- 用 `=` 會接不起來而讓 skip_reasons 永遠是空的。
    LEFT JOIN reasons r ON r.grp_key IS NOT DISTINCT FROM c.grp_key
   -- `r.skip_reasons` 一起 GROUP BY 而不是 max()：jsonb 沒有 max 聚合，
   -- 而 `reasons` 每組只有一列，所以放進分組鍵是等價且更直接的寫法。
   GROUP BY c.grp_key, r.skip_reasons
   ORDER BY 1 NULLS FIRST;
$$;

COMMENT ON FUNCTION fms.report_pm_compliance(text, date, date, int) IS
  'PM 準時完成率。分母分開（in_window 與 skipped 不進主分母，但各自具名回傳）；'
  '容許窗來自 maintenance_plans.completion_grace_days，可用參數覆寫做情境分析。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 純結構 + 純函式的檢查放這裡；**鏈真的接上了（工單完工 → occurrence 終結）
-- 由 pm_compliance_slice.rs 驗** —— 那需要工單與計畫，而這支 migration
-- 跑在 seed 009 之前。
DO $$
DECLARE v_src text;
BEGIN
  -- (1) 容許窗欄位與約束。
  IF NOT EXISTS (
    SELECT 1 FROM pg_attribute
     WHERE attrelid = 'fms.maintenance_plans'::regclass
       AND attname = 'completion_grace_days' AND attnotnull
  ) THEN
    RAISE EXCEPTION '063 FAILED: completion_grace_days 不存在或可為 NULL';
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'ck_plans_completion_grace') THEN
    RAISE EXCEPTION '063 FAILED: 缺 ck_plans_completion_grace';
  END IF;

  -- (2) 觸發器必須綁在 `completed_at` 這一欄上，不是綁整列更新。
  --     綁整列的話，任何 UPDATE 都會重跑一次同步；而綁狀態名稱則會讓
  --     044 的重開流程失效（見檔頭）。
  IF NOT EXISTS (
    SELECT 1 FROM pg_trigger t
      JOIN pg_attribute a ON a.attrelid = t.tgrelid
                         AND a.attnum = ANY (t.tgattr::smallint[])
     WHERE t.tgrelid = 'fms.work_orders'::regclass
       AND t.tgname = 'trg_work_orders_sync_occurrence'
       AND a.attname = 'completed_at'
  ) THEN
    RAISE EXCEPTION
      '063 FAILED: trg_work_orders_sync_occurrence 沒有綁在 completed_at 上 —— '
      '見檔頭：綁狀態名稱會讓 044 的重開流程失效';
  END IF;

  -- (3) 觸發器必須處理**兩個方向**。少了重開那一支，一件重做中的保養
  --     會一直被算成已完成。
  SELECT prosrc INTO v_src FROM pg_proc
   WHERE proname = 'trg_sync_occurrence_completion' AND pronamespace = 'fms'::regnamespace;
  IF v_src NOT LIKE '%OLD.completed_at IS NULL AND NEW.completed_at IS NOT NULL%'
     OR v_src NOT LIKE '%OLD.completed_at IS NOT NULL AND NEW.completed_at IS NULL%' THEN
    RAISE EXCEPTION '063 FAILED: 同步觸發器沒有同時處理完工與重開兩個方向';
  END IF;

  -- (4) 報表必須讀計畫的容許窗，而不是寫死天數。
  SELECT prosrc INTO v_src FROM pg_proc
   WHERE proname = 'report_pm_compliance' AND pronamespace = 'fms'::regnamespace;
  IF v_src NOT LIKE '%p.completion_grace_days%' THEN
    RAISE EXCEPTION
      '063 FAILED: report_pm_compliance 沒有讀 completion_grace_days —— '
      '寫死的容許窗會讓法定年檢與月保養被同一把尺量';
  END IF;
  -- (5) in_window 與 skipped 都不能進主分母。
  IF v_src NOT LIKE '%verdict IN (''on_time'',''late'',''missed'')%' THEN
    RAISE EXCEPTION
      '063 FAILED: 主分母的定義變了 —— in_window／skipped 進了分母的話，'
      '「全部跳過」會得到 0%% 或 100%%，兩者都是謊（見檔頭）';
  END IF;

  RAISE NOTICE '063 OK：容許窗由計畫定義、觸發器綁 completed_at 且雙向、'
               '分母排除 in_window 與 skipped（鏈的行為驗證在 pm_compliance_slice.rs）';
END;
$$;

COMMIT;
