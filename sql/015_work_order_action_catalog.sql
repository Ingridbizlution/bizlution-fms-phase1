-- =============================================================================
-- 015  Work order action catalogue
-- =============================================================================
-- 為什麼需要這張表
--
-- 契約的 `GET /work-orders/{id}/available-actions` 回傳
-- `{action, to_status, label_zh, required_fields, permitted}`，
-- 而該端點存在的理由是「前端據此決定按鈕的顯示與 enable，
-- 避免把狀態機邏輯複製到各前端」。
--
-- `work_order_statuses` 有 `name_zh`／`name_en`，但那是**狀態**的名稱，
-- 不是**動作**的名稱：`START_WORK` 的按鈕該寫「開始作業」，
-- 而不是目標狀態的「執行中」。整個 schema 裡沒有任何地方放動作標籤，
-- 因此 `label_zh` 沒有資料來源。
--
-- 三個候選解法與取捨：
--   1. 回傳 `to_status` 的 name_zh —— 錯的字，且會誤導前端直接顯示
--   2. 在應用層寫死 24 個中文標籤 —— 把 UI 文案埋進後端，
--      改一個字要重新部署，且違背這個端點的設計目的
--   3. 建一張 catalog 表（本檔）—— 與 `work_order_statuses` 同一個模式，
--      標籤與狀態機規則放在一起，租戶日後要覆寫也有地方掛
--
-- 選 3。這張表刻意只放「顯示用」欄位：動作的合法性仍然完全由
-- `work_order_transitions_allowed` 決定，本表不參與判定，
-- 缺一列只會讓 label 是 NULL，不會讓動作變得不可執行。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

CREATE TABLE IF NOT EXISTS fms.work_order_actions (
  code     varchar(40) PRIMARY KEY,
  label_zh varchar(60) NOT NULL,
  label_en varchar(60) NOT NULL,
  -- 破壞性動作（取消、駁回）前端通常要二次確認並用警示色。
  is_destructive boolean NOT NULL DEFAULT false
);

COMMENT ON TABLE fms.work_order_actions IS
  'Display metadata for state machine actions. Purely presentational: legality lives in work_order_transitions_allowed, so a missing row yields a null label, never a blocked action.';

INSERT INTO fms.work_order_actions (code, label_zh, label_en, is_destructive) VALUES
  ('SUBMIT',           '送出',       'Submit',           false),
  ('REQUEST_APPROVAL', '送審',       'Request Approval', false),
  ('APPROVE',          '核准',       'Approve',          false),
  ('REJECT',           '駁回',       'Reject',           true),
  ('ASSIGN',           '派工',       'Assign',           false),
  ('AUTO_ASSIGN',      '自動派工',   'Auto Assign',      false),
  ('REASSIGN',         '改派',       'Reassign',         false),
  ('ACCEPT',           '接單',       'Accept',           false),
  ('SCHEDULE',         '排程',       'Schedule',         false),
  ('START_WORK',       '開始作業',   'Start Work',       false),
  ('HOLD',             '暫停',       'Hold',             false),
  ('WAIT_PARTS',       '待料',       'Wait for Parts',   false),
  ('WAIT_VENDOR',      '待廠商',     'Wait on Vendor',   false),
  ('RESUME',           '繼續作業',   'Resume',           false),
  ('COMPLETE',         '完成',       'Complete',         false),
  ('VERIFY',           '驗收',       'Verify',           false),
  ('REOPEN',           '重啟',       'Reopen',           false),
  ('CLOSE',            '結案',       'Close',            false),
  ('CANCEL',           '取消',       'Cancel',           true),
  ('BREACH_SLA',       'SLA 逾期',   'SLA Breached',     false)
ON CONFLICT (code) DO UPDATE
  SET label_zh = EXCLUDED.label_zh,
      label_en = EXCLUDED.label_en,
      is_destructive = EXCLUDED.is_destructive;

-- catalog 表，全租戶共用，唯讀即可。與 008 的其他平台 catalog 一致，
-- 不開 RLS —— 表內沒有任何租戶資料。
GRANT SELECT ON fms.work_order_actions TO fms_app, fms_readonly;

-- -----------------------------------------------------------------------------
-- 自我驗證：狀態機用到的每個動作都要有標籤，否則前端會拿到 null
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_missing text[];
BEGIN
  SELECT array_agg(DISTINCT t.action)
    INTO v_missing
  FROM fms.work_order_transitions_allowed t
  LEFT JOIN fms.work_order_actions a ON a.code = t.action
  WHERE a.code IS NULL;

  IF v_missing IS NOT NULL THEN
    RAISE EXCEPTION '015 FAILED: transitions reference actions with no label: %', v_missing;
  END IF;

  RAISE NOTICE '015 OK: every action in work_order_transitions_allowed has a label';
END;
$$;

COMMIT;
