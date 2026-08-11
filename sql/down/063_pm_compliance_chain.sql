-- 回退 063。**這會讓 PM 合規鏈重新斷開** —— occurrence 又不會有終結狀態，
-- 那是 063 之前的狀態，所以回退就該回到那裡。

BEGIN;

DROP TRIGGER IF EXISTS trg_work_orders_sync_occurrence ON fms.work_orders;
DROP FUNCTION IF EXISTS fms.trg_sync_occurrence_completion();
DROP FUNCTION IF EXISTS fms.report_pm_compliance(text, date, date, int);

-- 欄位與約束一起移除。約束要先 DROP —— 反過來會因為欄位還被它引用而失敗。
ALTER TABLE fms.maintenance_plans DROP CONSTRAINT IF EXISTS ck_plans_completion_grace;
ALTER TABLE fms.maintenance_plans DROP COLUMN IF EXISTS completion_grace_days;

-- 063 寫進去的終結狀態要回捲，否則 up→down→up 之後資料裡會殘留
-- 一批「沒有任何寫入者能產生」的 COMPLETED —— 那正是 063 要修的那個矛盾。
UPDATE fms.maintenance_occurrences
   SET status = 'GENERATED', completed_at = NULL
 WHERE status = 'COMPLETED';

COMMIT;
