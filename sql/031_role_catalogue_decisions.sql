-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 031: 兩個角色目錄的決定
-- =============================================================================
-- 026 執行 min_scope_level 之後留下兩個待決事項，兩者都是**產品決定**而非
-- 技術問題，因此當時刻意沒有順手改。現在兩題都有答案了。
--
-- -----------------------------------------------------------------------------
-- 決定 #4：role:assign 維持 ORG 級
-- -----------------------------------------------------------------------------
-- `role:assign` 在 008 就宣告為 ORG，因此**宣告本身不用改**。要處理的是它
-- 帶出來的不一致：008 同時把它給了 `FACILITY_ADMIN`，而那個角色的
-- `scope_level` 是 FACILITY。
--
-- 026 之後，FACILITY 範圍的 FACILITY_ADMIN 已經拿不到 role:assign
-- （寬度 1 < ORG 的 2）—— 也就是說那筆授權在角色宣告的範圍內**永遠用不到**。
-- 留著它有兩個壞處：
--   * 讀目錄的人會以為場域管理員能指派角色
--   * 若有人把 FACILITY_ADMIN 指派在 ORG 範圍（與 roles.scope_level 矛盾的
--     設定），它會突然生效 —— 一個沒有人預期的權限
--
-- 因此移除。**今天沒有任何功能改變**：那筆授權目前不可能生效。
-- 「指派角色是組織級的事」這句話，現在目錄本身說得出來。
--
-- -----------------------------------------------------------------------------
-- 決定 #5：ORG_MANAGER 取得 organization:write
-- -----------------------------------------------------------------------------
-- 026 把 `organization:write` 從 TENANT 改成 ORG，理由是組織是 ltree 階層
-- 物件、組織經理該能在自己的子樹內建子組織。但當時只改了宣告 ——
-- 008 從未把這個權限給過 ORG_MANAGER，因此那條路徑實際上沒有人走得到
-- （`rbac_scope_slice` 的測試必須改用「TENANT_ADMIN 指派在 ORG 範圍」
--  才驗得到述詞，註解裡寫明了那是權宜）。
--
-- 這次補上，讓宣告與目錄一致。
--
-- 範圍仍然受限：`create_org` 以 parent 所在的組織子樹判定授權，
-- 而建立**根組織**需要 TENANT 範圍（見 fms-tenancy 的說明）。
-- 也就是說 ORG_MANAGER 能在自己那棵樹底下長出子組織，不能長出新的樹。
--
-- 依賴：008（角色與授權）、026（min_scope_level 生效）。
-- =============================================================================

-- -----------------------------------------------------------------------------
-- 029 引入的尖角：改動被稽核的表需要平台情境
-- -----------------------------------------------------------------------------
-- 這個 migration 第一次執行時失敗了：
--     ERROR: new row violates row-level security policy for table "audit_log"
--
-- 原因鏈：029 的觸發器掛在 role_permissions 上；那張表沒有 tenant_id，
-- 因此稽核列的 tenant_id 退回 `current_tenant_id()`，而 migration 的連線
-- 沒有租戶情境 → NULL。audit_log 的 tenant_isolation 判定是
-- `is_platform_context() OR tenant_id = current_tenant_id()`，
-- 兩邊都是 NULL 時結果是 NULL 而不是 true，於是 INSERT 被擋。
--
-- **這不是缺陷，是 029 的設計後果**：稽核寫不進去就該讓業務寫入一起失敗。
-- 但它意味著一條新規則：**任何改動那六張表（users／user_role_assignments／
-- roles／role_permissions／identity_providers／tenants）的 migration 或
-- 手動 SQL，都必須先宣告平台情境。** 應用層不受影響 ——
-- `begin_tenant_tx` 一定設了租戶情境。
--
-- 錯誤訊息本身看不出這條規則（它只說 RLS 擋了），因此記在這裡。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

-- 決定 #4
DELETE FROM fms.role_permissions rp
 USING fms.roles r
 WHERE rp.role_id = r.id
   AND r.code = 'FACILITY_ADMIN'
   AND rp.permission_code = 'role:assign';

-- 決定 #5
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'organization:write'
FROM fms.roles r
WHERE r.code = 'ORG_MANAGER'
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 除了兩筆授權，也順帶斷言一個**目錄層的不變量**：
-- 任何角色都不該持有「比自己宣告的 scope_level 更寬」的權限 ——
-- 那種組合在該角色的正常指派範圍內永遠用不到，只會誤導讀目錄的人。
--
-- 這個斷言目前為真，但它不是 CHECK 約束：日後刻意加入這種組合是可能的
-- （例如某個角色被設計成只在較寬的範圍使用）。放在這裡是為了讓
-- 「這一輪清乾淨了」成為一個被記錄的事實，而不是一句宣稱。
DO $$
DECLARE v_bad text;
BEGIN
  IF EXISTS (
    SELECT 1 FROM fms.roles r
    JOIN fms.role_permissions rp ON rp.role_id = r.id
    WHERE r.code = 'FACILITY_ADMIN' AND rp.permission_code = 'role:assign'
  ) THEN
    RAISE EXCEPTION '031 FAILED: FACILITY_ADMIN 仍持有 role:assign';
  END IF;

  IF NOT EXISTS (
    SELECT 1 FROM fms.roles r
    JOIN fms.role_permissions rp ON rp.role_id = r.id
    WHERE r.code = 'ORG_MANAGER' AND rp.permission_code = 'organization:write'
  ) THEN
    RAISE EXCEPTION '031 FAILED: ORG_MANAGER 沒有 organization:write';
  END IF;

  -- 順帶量一個目錄層的不變量：有多少「角色宣告的範圍比權限要求的更窄」
  -- 的組合。那種組合在該角色的正常指派範圍內永遠用不到，只會誤導讀目錄的人。
  --
  -- **刻意只擋惡化，不擋現況。** 移除 FACILITY_ADMIN 的 role:assign 之後
  -- 還剩 5 個既有組合（MAINTENANCE_SUPERVISOR／VIEWER 的四項），
  -- 而「VIEWER 到底該不該是場域級角色」是產品決定，不是這個 migration
  -- 該替人做的 —— 它只被授權處理上面那兩筆。
  --
  -- 因此門檻是 5：修好會過，新增一個就失敗。
  SELECT count(*)::text || E' 個：\n  ' ||
         string_agg(r.code || '（' || r.scope_level || '）持有 ' || p.code
                    || '（要求 ' || p.min_scope_level || '）', E'\n  ' ORDER BY r.code, p.code)
    INTO v_bad
  FROM fms.roles r
  JOIN fms.role_permissions rp ON rp.role_id = r.id
  JOIN fms.permissions p ON p.code = rp.permission_code
  WHERE fms.scope_width(r.scope_level) < fms.scope_width(p.min_scope_level);

  IF (SELECT count(*)
        FROM fms.roles r
        JOIN fms.role_permissions rp ON rp.role_id = r.id
        JOIN fms.permissions p ON p.code = rp.permission_code
       WHERE fms.scope_width(r.scope_level) < fms.scope_width(p.min_scope_level)) > 5 THEN
    RAISE EXCEPTION E'031 FAILED: 「角色範圍窄於權限要求」的組合變多了 —— %', v_bad;
  END IF;

  RAISE NOTICE '031 OK: role:assign 已自 FACILITY_ADMIN 移除、ORG_MANAGER 取得 organization:write';
  RAISE NOTICE '031 待決（不在本次授權範圍內）：仍有 %', v_bad;
END;
$$;

COMMIT;
