-- 回退 077。
--
-- 把約束還原成 002 的二選一版本，也就是**把那個缺陷放回去**：只填
-- `claim_value` 的對應會再次建得起來，而 058 的對帳仍然不讀它 ——
-- 建得起來、回 201、永遠不授予任何角色、沒有症狀（見 077 檔頭）。
--
-- 所以這支 down 只是為了讓 migrate-roundtrip 能驗 schema 可逆，
-- 實務上不該執行它。
BEGIN;
SET search_path = fms, public;

ALTER TABLE fms.directory_role_mappings
  DROP CONSTRAINT IF EXISTS ck_drm_group_required;

-- 與 002 的定義逐字相同：roundtrip 比對的是 pg_get_constraintdef 全文，
-- 換一個等價但不同寫法的述詞會被判成沒有還原。
ALTER TABLE fms.directory_role_mappings
  ADD CONSTRAINT ck_drm_source
  CHECK (directory_group_id IS NOT NULL OR claim_value IS NOT NULL);

-- 002 沒有 COMMENT ON COLUMN，因此還原成沒有。
COMMENT ON COLUMN fms.directory_role_mappings.claim_value IS NULL;

COMMIT;
