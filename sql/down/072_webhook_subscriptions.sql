-- 回退 072。
--
-- **DROP TABLE 會讓所有客戶的 webhook 訂閱消失，包含簽章金鑰。**
-- 那些金鑰只在建立時回傳過一次，客戶端存的是同一份 —— 重新套用這支 migration
-- 之後訂閱要全部重建，而每一個客戶都得換掉他們那一側的驗簽金鑰。
-- 寫在這裡，因為執行回退的人不會去讀 072 的檔頭。
--
-- 已經扇出到 notifications 的 WEBHOOK 列**留著**：它們是「這件事發生過」的
-- 紀錄。回退之後 dispatcher 沒有 WEBHOOK 傳輸層，那些列會被標成
-- `SUPPRESSED, last_error = 'no transport configured for channel WEBHOOK'` ——
-- 那是誠實的結果，不是缺陷。
BEGIN;

-- role_permissions／notifications 都掛了 029 的稽核觸發器，而 audit_log 有 RLS。
-- 回退腳本沒有租戶情境，因此需要平台情境（071 的 down 踩過同一件事）。
SELECT set_config('app.is_platform', 'on', true);

DROP FUNCTION IF EXISTS fms.record_webhook_result(uuid, boolean, text, int);

-- **簽章必須與 072 完全一致。**
--
-- 第一版寫的是 `(uuid, text, text, uuid, jsonb)`（5 個參數），而 072 後來在最前
-- 面加了 `p_event_id bigint`（幂等鍵需要它）。`IF EXISTS` 於是**靜默成功而什麼
-- 都沒刪** —— 手動 down → up 驗證時發現函式還在。
--
-- 這是 `DROP ... IF EXISTS` 帶簽章時的一個陷阱：簽章改了，那一行就變成 no-op，
-- 而回退看起來完全成功。
DROP FUNCTION IF EXISTS fms.fanout_webhooks(bigint, uuid, text, text, uuid, jsonb);

-- 072 把這個索引加在 **notifications** 上（不是 webhook_subscriptions），
-- 因此 DROP TABLE 帶不走它。少了這一行，roundtrip 的 schema 比對會看到一個
-- 多出來的索引。
DROP INDEX IF EXISTS fms.uq_notifications_webhook_event;

DROP TABLE IF EXISTS fms.webhook_subscriptions;

COMMIT;
