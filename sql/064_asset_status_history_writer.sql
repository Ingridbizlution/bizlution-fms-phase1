-- =============================================================================
-- 064：設備狀態歷程終於有寫入者
-- =============================================================================
-- 為什麼需要它
--
-- `fms.asset_status_history` 從 003 就存在，欄位齊全
-- （`from_status`、`to_status`、`reason`、`work_order_id`、`changed_by`）。
--
-- 而它**0 列、0 寫入者、0 讀者**。
--
-- 設備狀態改變發生在 `PATCH /assets/{id}`（以及 IoT 告警、工單完工等路徑），
-- 而那些路徑都只 UPDATE `assets.status`，沒有任何一條寫歷程。
--
-- 所以照契約做 `GET /assets/{id}/status-history` 會交付一支**永遠回空清單**
-- 的端點 —— 而它看起來會像「這台設備從來沒有故障過」。
-- 這與 063 的 PM 鏈、060 的遙測讀取面是同一個缺陷類型。
--
-- -----------------------------------------------------------------------------
-- 為什麼用觸發器而不是在 handler 裡寫
-- -----------------------------------------------------------------------------
-- `assets.status` 現在有多個寫入者，而且會再增加：
--
--   * `PATCH /assets/{id}`（人工改）
--   * `sql/030` 的計量規則（讀數越界 → DEGRADED）
--   * 未來的 IoT 告警自動降級、工單完工回復
--
-- 每一條路徑各自記歷程的話，漏掉一條的症狀是「那一類狀態變更查不到」——
-- 而歷程的用途正是回答「這台機器什麼時候開始出問題的」，
-- 少一類就足以讓那個問題答錯。
--
-- 觸發器讓它與寫入者的數量無關。這與 063 綁 `work_orders.completed_at`
-- 是同一個判斷。
--
-- -----------------------------------------------------------------------------
-- `changed_by` 可以是 NULL，而那是有意義的
-- -----------------------------------------------------------------------------
-- `fms.current_user_id()` 在背景工作（計量規則、PM 掃描）裡是 NULL。
-- 那不是缺漏 —— 「系統依規則自動降級」與「某個人手動改的」是不同的事實，
-- 而 NULL 正是前者的表達。硬塞一個假的使用者 id 會讓稽核說謊。
--
-- -----------------------------------------------------------------------------
-- 不記「沒有變化」的更新
-- -----------------------------------------------------------------------------
-- `AFTER UPDATE OF status` 在 `SET status = status` 時也會觸發。
-- 加 `WHEN (OLD.status IS DISTINCT FROM NEW.status)` 才不會在每次無關的
-- PATCH 上長出一列 —— 否則歷程會被雜訊淹沒，而那等於沒有歷程。
-- =============================================================================

BEGIN;

CREATE OR REPLACE FUNCTION fms.trg_record_asset_status_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  INSERT INTO fms.asset_status_history
    (tenant_id, asset_id, from_status, to_status, changed_by, changed_at)
  VALUES (
    NEW.tenant_id,
    NEW.id,
    OLD.status,
    NEW.status,
    -- 背景工作沒有使用者情境 → NULL。見檔頭：那是「系統改的」的表達，
    -- 不是缺漏。
    fms.current_user_id(),
    clock_timestamp());
  RETURN NEW;
END;
$$;

COMMENT ON FUNCTION fms.trg_record_asset_status_change() IS
  '設備狀態一變就記歷程。用觸發器而非 handler：status 有多個寫入者（人工 PATCH、'
  '030 的計量規則、未來的告警降級），每條路徑各自記會漏。';

DROP TRIGGER IF EXISTS trg_assets_status_history ON fms.assets;
CREATE TRIGGER trg_assets_status_history
  AFTER UPDATE OF status ON fms.assets
  FOR EACH ROW
  -- `SET status = status` 不該長出一列 —— 見檔頭。
  WHEN (OLD.status IS DISTINCT FROM NEW.status)
  EXECUTE FUNCTION fms.trg_record_asset_status_change();

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 純結構檢查；**行為（狀態一變真的長出一列、相同狀態不長）由
-- assets_completion_slice.rs 驗** —— 那需要一台設備與一次 UPDATE，
-- 而這支 migration 跑在 seed 009 之前。
DO $$
DECLARE v_when text;
BEGIN
  -- (1) 觸發器要綁在 `status` 這一欄上，不是整列更新。
  IF NOT EXISTS (
    SELECT 1 FROM pg_trigger t
      JOIN pg_attribute a ON a.attrelid = t.tgrelid
                         AND a.attnum = ANY (t.tgattr::smallint[])
     WHERE t.tgrelid = 'fms.assets'::regclass
       AND t.tgname = 'trg_assets_status_history'
       AND a.attname = 'status'
  ) THEN
    RAISE EXCEPTION '064 FAILED: trg_assets_status_history 沒有綁在 status 欄位上';
  END IF;

  -- (2) 必須有 WHEN 條件。少了它，每一次無關的 PATCH 都會長出一列，
  --     而歷程被雜訊淹沒等於沒有歷程。
  SELECT pg_get_triggerdef(oid) INTO v_when FROM pg_trigger
   WHERE tgrelid = 'fms.assets'::regclass AND tgname = 'trg_assets_status_history';
  IF v_when NOT LIKE '%IS DISTINCT FROM%' THEN
    RAISE EXCEPTION
      '064 FAILED: 觸發器缺 WHEN (OLD.status IS DISTINCT FROM NEW.status) —— '
      '沒有變化的更新也會長出一列，歷程會被雜訊淹沒';
  END IF;

  RAISE NOTICE '064 OK：狀態歷程有寫入者了，綁 status 欄位且只記真的變化'
               '（行為驗證在 assets_completion_slice.rs）';
END;
$$;

COMMIT;
