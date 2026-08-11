-- 回退 071。
--
-- **回退之後抑制會反過來造成噪音。** 006 版的 `raise_alarm` 不認 SUPPRESSED，
-- 因此任何仍處於抑制中的告警在下一次觸發時會產生一筆新告警加一封通知
-- （完整說明在 071 檔頭）。回退前應先把抑制中的告警解除：
--
--   UPDATE fms.alarms SET status='ACTIVE', suppressed_until=NULL
--    WHERE status='SUPPRESSED';
--
-- 這裡不自動做那件事：那會改動使用者刻意設定的狀態，而回退是維運決定，
-- 不該順手推翻業務資料。寫在這裡，因為執行回退的人不會去讀 071 的檔頭。
--
-- `alarm:suppress` 一併移除。留著它會變成一個沒有任何端點檢查的權限碼，
-- 而那比缺少它更難察覺。
BEGIN;

-- `role_permissions` 與 `alarms` 都掛了 029 的稽核觸發器，而那個觸發器寫
-- `audit_log`（有 RLS）。回退腳本沒有租戶情境，因此需要平台情境才寫得進去。
-- 少了這一行，回退會以
-- 「new row violates row-level security policy for table "audit_log"」失敗 ——
-- 而那個訊息完全指不到真正的原因。（migrate-roundtrip 第一次跑就是這樣失敗的。）
SELECT set_config('app.is_platform', 'on', true);

ALTER TABLE fms.alarms DROP CONSTRAINT IF EXISTS ck_alarms_suppression_bounded;

-- 還原 006 的去重索引（只涵蓋 ACTIVE／ACKNOWLEDGED）。
DROP INDEX IF EXISTS fms.uq_alarms_open_per_point;
CREATE UNIQUE INDEX uq_alarms_open_per_point
  ON fms.alarms (alarm_rule_id,
                 coalesce(telemetry_point_id, '00000000-0000-0000-0000-000000000000'::uuid))
  WHERE status IN ('ACTIVE', 'ACKNOWLEDGED');

DELETE FROM fms.role_permissions WHERE permission_code = 'alarm:suppress';
DELETE FROM fms.permissions WHERE code = 'alarm:suppress';

-- `tenant_settings_are_valid` 還原成 070 的版本（satisfaction_editable_days +
-- password_min_length，不含 alarm_max_suppress_minutes）。不是 DROP：
-- 067 的 `ck_tenants_settings` 依賴它。還原之後 alarm_max_suppress_minutes
-- 變成未知的鍵，因此既有設定值不會擋在約束上。
CREATE OR REPLACE FUNCTION fms.tenant_settings_are_valid(p jsonb)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT CASE
    WHEN p IS NULL THEN true
    WHEN jsonb_typeof(p) <> 'object' THEN false
    ELSE
      (CASE
        WHEN NOT (p ? 'satisfaction_editable_days') THEN true
        WHEN jsonb_typeof(p -> 'satisfaction_editable_days') <> 'number' THEN false
        WHEN (p ->> 'satisfaction_editable_days')::numeric
               <> trunc((p ->> 'satisfaction_editable_days')::numeric) THEN false
        WHEN (p ->> 'satisfaction_editable_days')::numeric NOT BETWEEN 0 AND 365
          THEN false
        ELSE true
      END)
      AND
      (CASE
        WHEN NOT (p ? 'password_min_length') THEN true
        WHEN jsonb_typeof(p -> 'password_min_length') <> 'number' THEN false
        WHEN (p ->> 'password_min_length')::numeric
               <> trunc((p ->> 'password_min_length')::numeric) THEN false
        WHEN (p ->> 'password_min_length')::numeric NOT BETWEEN 8 AND 128
          THEN false
        ELSE true
      END)
  END;
$$;

-- `raise_alarm` 不在這裡還原成 006 的版本。
--
-- 理由：071 的版本對「沒有任何告警處於抑制中」的資料庫**行為完全相同** ——
-- 多出來的三段都以 `status = 'SUPPRESSED'` 為前提。而把它改回去需要把 006
-- 那 90 行整份複製到這裡，那份複製會在 006 日後被修改時悄悄過期，
-- 於是回退會把一個更舊的實作寫回去。
--
-- 這是刻意的不對稱：down migration 的目的是讓 schema 可逆，
-- 而一個在相關資料不存在時行為相同的函式不構成不可逆。
COMMIT;
