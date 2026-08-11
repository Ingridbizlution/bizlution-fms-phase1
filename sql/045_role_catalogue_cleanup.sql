-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 045: 清掉五個永遠用不到的角色授權
-- =============================================================================
-- 026 讓 `min_scope_level` 從宣告變成執行之後，留下五個「角色宣告的範圍比
-- 權限要求的窄」的組合。031 量了它們、把門檻定在 5（只擋惡化），
-- 並註明「VIEWER 到底該不該是場域級角色是產品決定，不是那個 migration
-- 該替人做的」。
--
--     MAINTENANCE_SUPERVISOR（FACILITY）持有 maintenance_template:write（TENANT）
--     VIEWER（FACILITY）持有 audit:read（TENANT）
--     VIEWER（FACILITY）持有 identity_provider:read（TENANT）
--     VIEWER（FACILITY）持有 role:read（TENANT）
--     VIEWER（FACILITY）持有 tenant:read（TENANT）
--
-- 這五筆**今天沒有任何效果**（026 之後場域範圍的授權展不開它們，而這兩個
-- 角色在種子裡一次都沒有被指派過）。要清的是它們對讀目錄的人說的謊：
-- 「場域級的 VIEWER 看得到稽核日誌」。
--
-- -----------------------------------------------------------------------------
-- 為什麼是移除授權，而不是降低宣告
-- -----------------------------------------------------------------------------
-- 兩種做法都能讓目錄變誠實：
--
--   (a) 移除授權   → 目錄說「VIEWER 沒有這些」
--   (b) 降低宣告   → 目錄說「這些可以場域級讀，而 VIEWER 有」
--
-- 哪一個為真取決於產品對 VIEWER 的定義。而**這五個權限目前沒有任何已實作的
-- 端點在使用**（實測：`GET /roles`、`GET /permissions`、`GET /tenant`、
-- `GET /audit-log`、`GET /identity-providers`、`POST /maintenance-templates`
-- 的「實作」欄全是 `—`）。
--
-- 因此 (b) 會在**不知道那些端點的 payload 長什麼樣**的情況下，先替未來的
-- 作者決定範圍 —— 而 026 會強制執行那個決定。他不會發現自己繼承了一個
-- 沒有人選過的預設值。
--
-- (a) 反過來：日後建 `GET /roles` 的人會撞到「VIEWER 讀不到角色」，
-- 然後在**知道 payload 的時候**做決定。那個摩擦是有價值的。
--
-- 移除授權可逆，降低宣告不可逆（改回去要再想一次它為什麼曾經是 FACILITY）。
--
-- -----------------------------------------------------------------------------
-- audit:read 有一個額外的理由
-- -----------------------------------------------------------------------------
-- 我原本考慮把它降成 FACILITY —— `audit_log` 有 `facility_id` 欄位，也有
-- `facility_scope` RLS 政策，看起來降級是安全的。
--
-- **但那 92 筆列的 `facility_id` 全是 NULL**，而 `facility_in_scope(NULL)`
-- 回 true。也就是說那條政策目前什麼都不過濾。029 的稽核觸發器從不填那個
-- 欄位，而它稽核的六張表（users／roles／role_permissions／
-- identity_providers／user_role_assignments／tenants）**本來就沒有場域維度**。
--
-- 因此降級會讓存取**看起來**被場域收斂而實際上是全租戶可見 ——
-- 比現狀糟。這一項在 `audit_log.facility_id` 真的被填之前不該動宣告。
--
-- 依賴：026（min_scope_level 生效）、031（量測與只擋惡化的門檻）。
-- =============================================================================

-- 動 role_permissions（029 的稽核觸發器掛在上面）→ 需要平台情境，
-- 理由與 031 相同。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DELETE FROM fms.role_permissions rp
 USING fms.roles r
 WHERE rp.role_id = r.id
   AND (
     (r.code = 'MAINTENANCE_SUPERVISOR' AND rp.permission_code = 'maintenance_template:write')
     OR (r.code = 'VIEWER' AND rp.permission_code IN (
           'audit:read', 'identity_provider:read', 'role:read', 'tenant:read'))
   );

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE v_bad text;
BEGIN
  -- 031 的門檻是「不超過 5」（只擋惡化）。這裡收到 0 —— 那是新的地板。
  --
  -- 031 那條斷言仍然會通過（0 不大於 5），而它留在那裡是歷史紀錄：
  -- 「那一輪清乾淨了兩筆，剩下五筆是產品決定」。不去改它 ——
  -- 已套用的 migration 是歷史。
  SELECT string_agg(r.code || '（' || r.scope_level || '）持有 ' || p.code
                    || '（要求 ' || p.min_scope_level || '）', E'\n  '
                    ORDER BY r.code, p.code)
    INTO v_bad
  FROM fms.roles r
  JOIN fms.role_permissions rp ON rp.role_id = r.id
  JOIN fms.permissions p ON p.code = rp.permission_code
  WHERE fms.scope_width(r.scope_level) < fms.scope_width(p.min_scope_level);

  IF v_bad IS NOT NULL THEN
    RAISE EXCEPTION E'045 FAILED: 仍有「角色範圍窄於權限要求」的組合：\n  %', v_bad;
  END IF;

  -- 反面：不要順手把別的授權也刪掉。VIEWER 仍該是一個能讀東西的角色。
  IF NOT EXISTS (
    SELECT 1 FROM fms.roles r
    JOIN fms.role_permissions rp ON rp.role_id = r.id
    WHERE r.code = 'VIEWER'
  ) THEN
    RAISE EXCEPTION '045 FAILED: VIEWER 一個授權都不剩了 —— 刪過頭';
  END IF;

  RAISE NOTICE '045 OK: 「角色範圍窄於權限要求」的組合歸零';
END;
$$;

COMMIT;
