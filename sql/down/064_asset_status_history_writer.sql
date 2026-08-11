-- 回退 064。**這會讓狀態歷程重新沒有寫入者**（064 之前的狀態）。
--
-- 已經記下的列不刪：它們是真實發生過的事實。與 down/063 不同 ——
-- 那支要回捲 COMPLETED，因為回退之後沒有任何寫入者能產生那個值，
-- 留著會造成矛盾。歷程列沒有這個問題，它只是不再增加。

BEGIN;

DROP TRIGGER IF EXISTS trg_assets_status_history ON fms.assets;
DROP FUNCTION IF EXISTS fms.trg_record_asset_status_change();

COMMIT;
