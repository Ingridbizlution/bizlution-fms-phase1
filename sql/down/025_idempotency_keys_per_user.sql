-- 回退 025。**這會重新引入「憑鍵取回應」的缺陷**，只為 roundtrip 驗證的
-- 完整性而存在。
--
-- 同樣必須先清空：025 之後可能存在兩列 (tenant_id, key, endpoint) 相同、
-- 只有 user_id 不同的資料，那在舊主鍵下無法並存。
-- 這裡不試圖「挑一列留下」—— 任何挑法都是隨意的，而這張表是 24 小時暫存。
BEGIN;
SET search_path = fms, public;

DO $$
BEGIN
  PERFORM set_config('app.is_platform', 'on', true);
  DELETE FROM fms.idempotency_keys;
  PERFORM set_config('app.is_platform', 'off', true);
END;
$$;

ALTER TABLE fms.idempotency_keys DROP CONSTRAINT idempotency_keys_pkey;
ALTER TABLE fms.idempotency_keys DROP COLUMN user_id;
ALTER TABLE fms.idempotency_keys
  ADD CONSTRAINT idempotency_keys_pkey
  PRIMARY KEY (tenant_id, idempotency_key, endpoint);

COMMIT;
