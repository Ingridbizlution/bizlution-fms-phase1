-- 回退 038。
--
-- 觸發器還原成 032 的版本（一律自然時間），拆掉行事曆表、營業時間函式與
-- 形狀約束。
--
-- **`sla_basis` 的欄位與值不還原**：那些是「期限當初是怎麼算的」的紀錄，
-- 而回退機制不該抹掉紀錄。留著一個沒有人寫的欄位比留一段假歷史好。
-- 已經用營業時間算出來的 due 也不重算 —— 那會回溯改變報表。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- 032 的觸發器函式，逐字還原。
CREATE OR REPLACE FUNCTION fms.trg_work_order_sla_targets()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_policy fms.sla_policies;
  v_from   timestamptz;
BEGIN
  -- DRAFT 還沒送出 → 不起算。
  IF NEW.status = 'DRAFT' THEN
    RETURN NEW;
  END IF;

  -- `raise_alarm` 會從告警規則帶 `wo_sla_policy_id` 進來。那是個明示的選擇，
  -- 不該被解析結果蓋掉 —— 但它同樣需要算出 due（006 只設了 id）。
  IF NEW.sla_policy_id IS NOT NULL THEN
    SELECT * INTO v_policy FROM fms.sla_policies WHERE id = NEW.sla_policy_id;
  ELSE
    v_policy := fms.resolve_sla_policy(NEW.tenant_id, NEW.facility_id, NEW.priority);
    NEW.sla_policy_id := v_policy.id;
  END IF;

  IF v_policy.id IS NULL THEN
    -- 解析不到 policy。**標 NOT_APPLICABLE 而不是留 ON_TRACK**：
    -- ON_TRACK 是「有目標且還沒逾期」，這裡是「沒有目標」。
    -- 兩者混在一起，報表就分不出「達成」與「沒在量」。
    NEW.sla_state := 'NOT_APPLICABLE';
    RETURN NEW;
  END IF;

  v_from := coalesce(NEW.created_at, clock_timestamp());
  NEW.response_due_at   := v_from + make_interval(mins => v_policy.response_minutes);
  NEW.resolution_due_at := v_from + make_interval(mins => v_policy.resolution_minutes);
  RETURN NEW;
END;
$$;


DROP FUNCTION IF EXISTS fms.sla_target_at(uuid, timestamptz, int, boolean);
DROP FUNCTION IF EXISTS fms.add_business_minutes(uuid, timestamptz, int);
DROP FUNCTION IF EXISTS fms.business_windows(uuid, date);

DROP TABLE IF EXISTS fms.holiday_calendars;

ALTER TABLE fms.facilities DROP CONSTRAINT IF EXISTS ck_facilities_operating_hours;
DROP FUNCTION IF EXISTS fms.operating_hours_are_valid(jsonb);
DROP FUNCTION IF EXISTS fms.time_windows_are_valid(jsonb);

COMMIT;
