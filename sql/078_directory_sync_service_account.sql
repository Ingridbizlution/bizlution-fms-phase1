-- =============================================================================
-- 078  Directory sync 排程觸發：schema 與平台角色
-- =============================================================================
-- 058 的檔頭記過一個刻意的已知限制：
--
--   排程觸發（identity_providers.sync_cron）沒有人類觸發者。那需要另一個
--   答案（指定一個服務帳號，其權限即為同步的上限），而 Phase 1 沒有排程
--   同步。刻意記在這裡而不是留一個空白。
--
-- 本檔與 079 合起來就是那個答案，與 019（PM 產生器）同一個手法：
-- `reconcile_directory_roles` 需要一個 `p_actor_id`（052 的提權防護判定
-- 對象是「觸發同步的人」），排程沒有人，因此需要一個服務帳號頂替。
--
-- # 為什麼分成兩個檔（078／079）
--
-- 平台角色（`tenant_id IS NULL`）與 `directory_sync_runs` 的 schema 變更
-- 不依賴任何租戶存在，屬於 CORE，任何部署模式都該套用。**示範租戶的
-- 服務帳號**（`fms.users` 一列）依賴 009 的示範租戶先存在，因此是 POST_SEED
-- （見 079，與 017／018／019 同一組）。混在一支檔案裡的代價是實際踩到的：
-- 從零建立 template（`MIGRATE_MODE=all`）時，078 在 009 之前執行，
-- 插入服務帳號會撞外鍵——`aaaaaaaa-...` 那個租戶還不存在。
--
-- # 為什麼服務帳號只給 `directory:sync`，不多給
--
-- 這正是 058 檔頭說的「其權限即為同步的上限」：052 的規則是「你不能授出
-- 一項你自己沒有的危險權限」，且該函式的註解已經明確預期非人類授權者
-- （見 `role_grant_blocked_by` 的 COMMENT：「DIRECTORY_SYNC／SCIM／SYSTEM
-- 沒有人類授權者」）。服務帳號只有 `directory:sync`（`is_dangerous = false`），
-- 因此排程同步能授出的角色，被限制在**不含任何危險權限**的角色 ——
-- 示範資料的兩筆對應（FACILITY_ADMIN、TECHNICIAN）恰好都在這個範圍內。
--
-- 若某條對應指向的角色帶危險權限（例如不小心把 AD 群組對到
-- TENANT_ADMIN），排程那一輪會把它計入 `blocked_mappings`、狀態降成
-- PARTIAL —— 與人類觸發、但自己沒有那項危險權限時完全相同的行為。
-- 這是刻意的安全預設值，不是縮水版功能：**要讓排程也能授出危險角色，
-- 必須先明確地把對應權限加給這個服務帳號**，那本身就是一個可稽核、
-- 可覆核的決定，而不是背景作業預設就有的能力。
--
-- # `directory_sync_runs.run_type` 新增 'SCHEDULED'
--
-- 現有四種（FULL／DELTA／SCIM_PUSH／MANUAL）都假設有一個發起管道。
-- 排程觸發的是背景迴圈，不是任何管道，也不是「手動」——歸進 MANUAL
-- 會讓稽核記錄說謊。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

SELECT set_config('app.is_platform', 'on', true);

ALTER TABLE fms.directory_sync_runs
  DROP CONSTRAINT directory_sync_runs_run_type_check;

ALTER TABLE fms.directory_sync_runs
  ADD CONSTRAINT directory_sync_runs_run_type_check
  CHECK (run_type IN ('FULL', 'DELTA', 'SCIM_PUSH', 'MANUAL', 'SCHEDULED'));

-- 平台層角色（tenant_id IS NULL，所有租戶共用）
-- `scope_level` 必須明確設成 TENANT：預設是 FACILITY（002），而
-- `directory:sync` 的 `min_scope_level` 是 TENANT（008）。026 的
-- `no_role_holds_a_permission_wider_than_its_own_scope` 會擋下「角色範圍
-- 比自己持有的權限要求更窄」——026 是後來才加的通用不變量，019 的
-- PM_GENERATOR 沒踩到只是因為它那三個權限剛好都是 FACILITY 範圍，
-- 不代表新角色可以省略這一欄。
INSERT INTO fms.roles (tenant_id, code, name, description, is_system, scope_level)
VALUES (NULL, 'DIRECTORY_SYNC', '目錄同步（服務帳號）',
        '排程觸發目錄同步的背景作業使用。僅具觸發同步的權限 —— 能授出的角色
         上限見本檔檔頭。',
        true, 'TENANT')
ON CONFLICT DO NOTHING;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'directory:sync'
FROM fms.roles r
WHERE r.code = 'DIRECTORY_SYNC' AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM fms.roles WHERE code = 'DIRECTORY_SYNC' AND tenant_id IS NULL
  ) THEN
    RAISE EXCEPTION '078 FAILED: DIRECTORY_SYNC 角色沒有建起來';
  END IF;
  RAISE NOTICE '078 OK: DIRECTORY_SYNC 角色與 directory:sync 權限就緒，run_type 接受 SCHEDULED';
END;
$$;

COMMIT;
