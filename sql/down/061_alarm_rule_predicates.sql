-- 回退 061：把 057 的評估器還原成自己內含 CASE 的版本，然後丟掉兩支述詞。
--
-- 順序有關係：先還原評估器（它依賴那兩支），再 DROP。
-- 反過來做會因為 `depends on function` 而失敗 —— 而失敗的時機是回退中途，
-- 那是最不想要修東西的時候。

BEGIN;

CREATE OR REPLACE FUNCTION fms.evaluate_telemetry_rules(
  p_telemetry_point_id uuid,
  p_value              numeric,
  p_observed_at        timestamptz DEFAULT NULL
) RETURNS TABLE (raised int, skipped_sustained int, bad_rule_codes text[])
LANGUAGE plpgsql
AS $$
DECLARE
  v_point_code text;
  v_rule       fms.alarm_rules;
  v_op         text;
  v_threshold  numeric;
  v_fired      boolean;
BEGIN
  raised := 0;
  skipped_sustained := 0;
  bad_rule_codes := ARRAY[]::text[];

  IF p_value IS NULL THEN
    RETURN NEXT;
    RETURN;
  END IF;

  SELECT point_code INTO v_point_code
    FROM fms.telemetry_points WHERE id = p_telemetry_point_id;

  FOR v_rule IN
    SELECT * FROM fms.alarm_rules
     WHERE is_active
       AND (telemetry_point_id = p_telemetry_point_id
            OR (telemetry_point_id IS NULL
                AND point_code IS NOT NULL
                AND point_code = v_point_code))
  LOOP
    IF v_rule.rule_type <> 'THRESHOLD' THEN
      skipped_sustained := skipped_sustained + 1;
      CONTINUE;
    END IF;

    IF v_rule.condition ? 'for_seconds' THEN
      skipped_sustained := skipped_sustained + 1;
      CONTINUE;
    END IF;

    v_op := v_rule.condition->>'op';
    v_threshold := (v_rule.condition->>'value')::numeric;

    v_fired := CASE v_op
                 WHEN '>'  THEN p_value >  v_threshold
                 WHEN '>=' THEN p_value >= v_threshold
                 WHEN '<'  THEN p_value <  v_threshold
                 WHEN '<=' THEN p_value <= v_threshold
                 WHEN '='  THEN p_value =  v_threshold
                 WHEN '!=' THEN p_value <> v_threshold
               END;

    IF v_fired IS NULL THEN
      bad_rule_codes := bad_rule_codes || v_rule.code::text;
      CONTINUE;
    END IF;

    IF v_fired THEN
      PERFORM fms.raise_alarm(
        v_rule.id,
        p_telemetry_point_id,
        p_value,
        format('%s：%s %s %s（實測 %s）',
               v_rule.name, coalesce(v_point_code, '讀數'), v_op, v_threshold, p_value),
        coalesce(p_observed_at, clock_timestamp()));
      raised := raised + 1;
    END IF;
  END LOOP;

  RETURN NEXT;
END;
$$;

DROP FUNCTION IF EXISTS fms.alarm_rule_covers_point(uuid, text, uuid, text);
DROP FUNCTION IF EXISTS fms.telemetry_rule_fires(jsonb, numeric);

COMMIT;
