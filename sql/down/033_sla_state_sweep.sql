-- 回退 033：拆掉掃描函式。
--
-- **已經標記的 sla_state 不還原。** 那些標記是對的（逾期確實發生了），
-- 抹掉它們只是讓逾期看不見。回退的是機制，不是已經記錄下來的事實。
BEGIN;
SET search_path = fms, public;

DROP FUNCTION IF EXISTS fms.sweep_sla_states(numeric);

COMMIT;
