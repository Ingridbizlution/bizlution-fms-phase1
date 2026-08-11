-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 082: REQUESTER 查得到教室空檔（權限資料，不是新邏輯）
-- =============================================================================
-- 負載測試量到的（docs/perf-baseline.md 發現二）：`GET
-- /facilities/{id}/availability` 要 `reservation:read`
-- （fms-reservation/src/handlers.rs），而 REQUESTER 只有
-- `reservation:read_own`。結果是一個老師可以 `reservation:create`，
-- 卻查不到任何教室的空檔，只能盲訂再吃 409 RESERVATION_CONFLICT ——
-- 403 是正確的授權行為，錯的是權限資料。
--
-- -----------------------------------------------------------------------------
-- 為什麼是新權限碼，不是把 `reservation:read` 直接發給 REQUESTER
-- -----------------------------------------------------------------------------
-- `reservation:read` 同時打開 `GET /reservations`（清單／單筆，含標題、備註、
-- 主辦人姓名）與 `GET /facilities/{id}/occupancy`（牆面板）。老師需要的只是
-- 「這個時段這間教室忙不忙」，不是「租戶裡每一筆預約的完整內容」——
-- 那是完全不同的曝光面，沿用既有碼會讓授權變成全有全無。與 037／040
-- 為 sla_policy／holiday 各開一支新碼、不沿用更寬的既有碼，是同一個理由。
--
-- `reservation:read_availability` 只打開 availability 端點（handlers.rs 的
-- `availability()` 改成接受 `reservation:read` **或**這個新碼 —— 016 之後
-- 權限碼是純字串集合比對，兩者互不隱含，因此兩個碼都要各自出現在
-- `role_permissions` 才算持有），不影響 `GET /reservations` 或 `/occupancy`
-- 的既有授權面。
--
-- -----------------------------------------------------------------------------
-- 這個決定同時揭出、也一併堵上的洞：availability 從來沒有遮罩私人預約
-- -----------------------------------------------------------------------------
-- `busy_blocks()` 把 `reservations.title` 原樣塞進 `busy[].reason`，
-- 完全沒有讀 011 的 `is_private`。011 私人預約遮罩那次
-- （private_reservation_slice.rs）修的是 `GET /reservations`、
-- `GET /reservations/{id}` 與 `GET /facilities/{id}/occupancy` 三條洩漏
-- 路徑，availability 不在那張表裡 —— 不是漏改，是因為在此之前 availability
-- 只有 ORG_MANAGER／FACILITY_ADMIN／TENANT_ADMIN／PLATFORM_ADMIN 能叫，
-- 而這四個角色都持有 `reservation:view_private`，沒有人踩得到那個洞。
--
-- 把 availability 開給 REQUESTER 之後，那個洞就是活的：一個沒有
-- `view_private` 的老師會在 `busy[].reason` 看到別人私人會議的完整標題。
-- 因此這一刀連同 Rust 一起修（repo.rs 的 `busy_blocks` 補回
-- `is_private`／`organizer_id`／`on_behalf_of_id`，handlers.rs 的
-- `availability()` 套用與清單／單筆同一套遮罩）；這裡的純資料變更只是
-- 那一刀能安全上線的前提。
--
-- 依賴：016（permission_codes）、026（min_scope_level 生效）。
-- =============================================================================

SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous)
VALUES
  ('reservation:read_availability', 'reservation', 'read_availability', 'RESERVATION',
   '查詢資源的忙碌／可用時段（僅時段與種類，不含預約標題／備註／主辦人）',
   'FACILITY', false)
ON CONFLICT (code) DO UPDATE
  SET resource = EXCLUDED.resource,
      action = EXCLUDED.action,
      module = EXCLUDED.module,
      description = EXCLUDED.description,
      min_scope_level = EXCLUDED.min_scope_level,
      is_dangerous = EXCLUDED.is_dangerous;

-- 只給 REQUESTER —— 已經持有 `reservation:read` 的角色（ORG_MANAGER／
-- FACILITY_ADMIN／TENANT_ADMIN／PLATFORM_ADMIN）不需要它：handlers.rs 的
-- availability() 檢查 `reservation:read` 或這個新碼，任一即可通過。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'reservation:read_availability'
FROM fms.roles r
WHERE r.code = 'REQUESTER'
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE v_n bigint;
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM fms.permissions
     WHERE code = 'reservation:read_availability' AND min_scope_level = 'FACILITY'
       AND NOT is_dangerous
  ) THEN
    RAISE EXCEPTION '082 FAILED: reservation:read_availability 應宣告 FACILITY 範圍且非 dangerous';
  END IF;

  SELECT count(*) INTO v_n
    FROM fms.roles r
    JOIN fms.role_permissions rp ON rp.role_id = r.id
   WHERE rp.permission_code = 'reservation:read_availability'
     AND r.code = 'REQUESTER';
  IF v_n <> 1 THEN
    RAISE EXCEPTION '082 FAILED: REQUESTER 應該持有 reservation:read_availability，實際 %', v_n;
  END IF;

  RAISE NOTICE '082 OK: reservation:read_availability 就緒，REQUESTER 持有';
END;
$$;

COMMIT;
