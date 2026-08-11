-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 055: 平台技能目錄
-- =============================================================================
-- `fms.skills` 與 `fms.user_skills` 從 004 就存在，**兩張都是空的**，
-- 而且沒有任何端點會寫它們。做 `GET /skills` 時發現：照契約實作出來會是一支
-- 永遠回空清單的端點。
--
-- 這個 migration 補平台層的目錄（`tenant_id IS NULL`，所有租戶共用），
-- 與 008 種平台角色與權限是同一個模式。租戶自訂技能走 `POST /skills`。
--
-- -----------------------------------------------------------------------------
-- `requires_certification` 是這份目錄唯一有判斷的欄位
-- -----------------------------------------------------------------------------
-- 它標的不是「這件事很難」，而是**法規要求執業證照**。台灣的實務：
--
--   * 電氣、電梯、消防、鍋爐、高空作業 —— 需要主管機關核發的證照，
--     而證照**會到期**，過期執業是違規。
--   * 空調、水電、門禁、木作 —— 需要技術，但不需要證照。
--
-- 這個區分有實際後果：`user_skills.expires_at` 對前者是必填的業務事實，
-- 對後者通常是 NULL。004 為此建了 `idx_user_skills_expiring`
-- （部分索引，`WHERE expires_at IS NOT NULL`）。
--
-- **那個索引到現在仍然沒有讀者。** 到期提醒（掃描 + 通知）是獨立的一件事，
-- 這個 migration 沒有做它。誠實記在這裡，而不是讓索引看起來已經在服務誰。
--
-- 依賴：004（skills／user_skills）。
-- =============================================================================

-- 031 的規則：寫入受 RLS 保護的表要先宣告平台情境。這裡是**必要的**，
-- 不是形式 —— `skills` 的 `tenant_write` WITH CHECK 是
-- `is_platform_context() OR tenant_id = current_tenant_id()`，
-- 而平台目錄的 `tenant_id` 是 NULL，兩個分支都不成立。
-- （漏了它的症狀是 `new row violates row-level security policy`。）
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- 平台目錄用固定 uuid：測試與種子要引用得到，而 `gen_random_uuid()` 每次不同。
-- 前綴 `50000000` 與其他平台目錄（角色 ffffffff、設備型號 20000000…）區隔。
INSERT INTO fms.skills (id, tenant_id, code, name, domain, requires_certification) VALUES
  ('50000000-0000-4000-8000-000000000001', NULL, 'ELECTRICAL',  '電氣',     'MEP',      true),
  ('50000000-0000-4000-8000-000000000002', NULL, 'ELEVATOR',    '電梯',     'MEP',      true),
  ('50000000-0000-4000-8000-000000000003', NULL, 'FIRE_SAFETY', '消防安全', 'SAFETY',   true),
  ('50000000-0000-4000-8000-000000000004', NULL, 'BOILER',      '鍋爐',     'MEP',      true),
  ('50000000-0000-4000-8000-000000000005', NULL, 'WORK_AT_HEIGHT', '高空作業', 'SAFETY', true),
  ('50000000-0000-4000-8000-000000000006', NULL, 'HVAC',        '空調',     'MEP',      false),
  ('50000000-0000-4000-8000-000000000007', NULL, 'PLUMBING',    '給排水',   'MEP',      false),
  ('50000000-0000-4000-8000-000000000008', NULL, 'ACCESS_CONTROL', '門禁',  'SECURITY', false),
  ('50000000-0000-4000-8000-000000000009', NULL, 'CARPENTRY',   '木作',     'FABRIC',   false)
ON CONFLICT (id) DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE v_n int; v_cert int;
BEGIN
  SELECT count(*), count(*) FILTER (WHERE requires_certification)
    INTO v_n, v_cert
    FROM fms.skills WHERE tenant_id IS NULL;

  IF v_n < 9 THEN
    RAISE EXCEPTION '055 FAILED: 平台技能目錄只有 % 項', v_n;
  END IF;

  -- 兩邊都要有值。全部標成需要證照（或全部不需要）的話，那個欄位就沒有
  -- 判別力 —— 而 `GET /users/{id}/skills` 的到期判定完全建立在它上面。
  IF v_cert = 0 OR v_cert = v_n THEN
    RAISE EXCEPTION
      '055 FAILED: requires_certification 全部相同（% / %），該欄位失去判別力',
      v_cert, v_n;
  END IF;

  -- 平台目錄不屬於任何租戶。寫錯成某個租戶的話，其他租戶就看不到它 ——
  -- 而症狀是「這個技能怎麼不見了」，不是錯誤。
  IF EXISTS (SELECT 1 FROM fms.skills
              WHERE tenant_id IS NULL AND id::text LIKE '50000000%'
                AND code NOT IN ('ELECTRICAL','ELEVATOR','FIRE_SAFETY','BOILER',
                                 'WORK_AT_HEIGHT','HVAC','PLUMBING',
                                 'ACCESS_CONTROL','CARPENTRY')) THEN
    RAISE EXCEPTION '055 FAILED: 平台目錄有預期外的項目';
  END IF;

  RAISE NOTICE '055 OK：平台技能 % 項（其中 % 項需要證照）'
               '。到期提醒由 059 補上（前置期在 skills.reminder_days_before）',
               v_n, v_cert;
END;
$$;

COMMIT;
