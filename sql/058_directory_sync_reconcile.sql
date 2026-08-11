-- =============================================================================
-- Bizlution FMS — Phase 1 Backend
-- Migration 058: 目錄同步的對帳 —— directory_role_mappings 的第一個消費者
-- =============================================================================
-- `POST /identity-providers/{id}/sync` 的落地處。
--
-- -----------------------------------------------------------------------------
-- 在這之前，群組→角色對應是一份沒有人讀的規則
-- -----------------------------------------------------------------------------
-- `fms.directory_role_mappings` 有資料（009 種了 2 筆，而
-- `POST /directory-role-mappings` 也能建）。但除了表定義、種子與那組 CRUD，
-- **整個 codebase 沒有任何程式碼讀它** —— 對應建得出來、列得到、刪得掉，
-- 卻永遠不會產生任何角色指派。
--
-- 這支函式是那一半。
--
-- -----------------------------------------------------------------------------
-- 同步**必須會收回**，不是只會發放
-- -----------------------------------------------------------------------------
-- `directory_sync_runs` 有 `roles_revoked` 這個計數器 —— schema 本身就在說
-- 這件事。而「只加不減」是這種對帳最容易只做一半的地方：
--
--   * 有人離開了 AD 群組 → 他的授權必須消失
--   * 有人把對應停用或刪除 → 由它產生的授權必須消失
--
-- 而 `DELETE /role-assignments/{id}` 拒絕撤銷 `source = DIRECTORY_SYNC` 的授權，
-- 訊息寫「下一輪同步會再建回來 —— 要移除請改群組對應」。
-- **若同步不會收回，那句話就是假的**，而使用者會發現改了對應卻沒有效果。
--
-- 收回的範圍嚴格限定在 `source = 'DIRECTORY_SYNC'` 且
-- `origin_directory_group_id` 屬於這個 provider 的群組。人工指派
-- （`source = 'MANUAL'`）永遠不動 —— 同步不該吃掉管理員手動給的東西。
--
-- -----------------------------------------------------------------------------
-- 提權防護：判定對象是**觸發同步的人**
-- -----------------------------------------------------------------------------
-- 052 的規則是「你不能授出一項你自己沒有的危險權限」。但同步沒有「你」，
-- 而 `directory_role_mappings` **沒有 `created_by` 欄位**，無法回溯到
-- 建立對應的人。
--
-- 因此判定對象是 `p_actor_id`（觸發同步的人）。理由：
--
--   * 「觸發一個會發出 PLATFORM_ADMIN 的同步」與「直接發出它」是同一個行為。
--   * 這讓 `directory:sync` 這個權限有牙齒 —— 你只同步得出你授得出的東西。
--   * 對應可以被種子或手寫 SQL 建立（繞過 handler 的 052 檢查），
--     所以不能假設「建立時已經檢查過了」。
--
-- 被擋下的對應**計入 `blocked_mappings` 並回傳**，不是靜默跳過 ——
-- 「這條對應設定了但永遠不會生效」必須看得見。
--
-- **已解決（078）：排程觸發（`identity_providers.sync_cron`）原本沒有
-- 人類觸發者。** 那需要另一個答案（指定一個服務帳號，其權限即為同步的
-- 上限）—— 078 建了那個服務帳號（只有 `directory:sync`），
-- `fms-identity::directory_sync_watchdog` 是驅動它的背景迴圈。
--
-- -----------------------------------------------------------------------------
-- 這支函式**不連 AD／Entra**
-- -----------------------------------------------------------------------------
-- 它從 `user_directory_groups`（成員關係）與 `directory_role_mappings`（規則）
-- 對帳出 `user_role_assignments`。**「去外部目錄抓成員關係」是另一半**，
-- 需要 LDAP／Graph 客戶端，Phase 1 沒有。
--
-- 也就是說：這支函式跑完之後，成員關係仍然是別人放進去的
-- （SCIM、手動、或未來的 connector）。函式名稱用 `reconcile` 而不是 `sync`
-- 就是為了不誤導。
--
-- -----------------------------------------------------------------------------
-- 後記：這裡的 JOIN 是內連接，而 077 讓那件事變成明確的契約
-- -----------------------------------------------------------------------------
-- 發放與收回都以 `m.directory_group_id` 為軸內連接 `directory_groups`。
-- 002 的 `directory_role_mappings` 原本允許改填 `claim_value`（不填群組），
-- 而那種列在第一個 JOIN 就被丟掉 —— 沒有錯誤、不進 `blocked_mappings`。
--
-- **077 把「必須有群組」寫成資料庫約束**，理由是那個欄位缺三件現在不存在的
-- 東西（claim 來源、以 claim 為鍵的成員關係、以 claim 為鍵的收回身分），
-- 其中最後一件正是這支函式的收回段落只認得 `origin_directory_group_id`。
-- 完整量測在 077 檔頭。這支函式**不需要改**：它一直是對的那一半。
--
-- 依賴：002（identity_providers／directory_groups／directory_role_mappings／
--       user_directory_groups／user_role_assignments）、
--       052（role_grant_blocked_by）。
-- =============================================================================

BEGIN;
SET search_path = fms, public;

-- 回傳這一輪的計數。呼叫端（handler）把它寫進 `directory_sync_runs`。
--
-- 不是 SECURITY DEFINER：呼叫端已注入租戶情境，RLS 照常生效。
-- 看不到的 provider／群組／對應就是不存在。
CREATE OR REPLACE FUNCTION fms.reconcile_directory_roles(
  p_identity_provider_id uuid,
  p_actor_id             uuid
) RETURNS TABLE (
  roles_granted    int,
  roles_revoked    int,
  groups_synced    int,
  blocked_mappings text[]
)
LANGUAGE plpgsql
AS $$
DECLARE
  v_m       record;
  v_blocked text;
  v_ins     int;
BEGIN
  roles_granted := 0;
  roles_revoked := 0;
  blocked_mappings := ARRAY[]::text[];

  SELECT count(*) INTO groups_synced
    FROM fms.directory_groups
   WHERE identity_provider_id = p_identity_provider_id;

  -- --- 發放 ---------------------------------------------------------------
  -- 一個 (使用者, 對應) 組合一列。`priority` 只影響套用順序，不影響結果
  -- （角色指派是加法的，見 ADR-11），但照它排序讓行為可預測。
  FOR v_m IN
    SELECT udg.user_id, m.id AS mapping_id, m.role_id, m.scope_type, m.scope_id,
           m.directory_group_id, r.code AS role_code
      FROM fms.directory_role_mappings m
      JOIN fms.directory_groups g ON g.id = m.directory_group_id
      JOIN fms.user_directory_groups udg ON udg.directory_group_id = g.id
      JOIN fms.roles r ON r.id = m.role_id
     WHERE m.is_active
       AND g.identity_provider_id = p_identity_provider_id
     ORDER BY m.priority, m.id, udg.user_id
  LOOP
    -- **提權防護。** 見檔頭：判定對象是觸發同步的人。
    -- 對應可以被種子或手寫 SQL 建立，因此不能假設建立時檢查過了。
    SELECT string_agg(c, ',' ORDER BY c) INTO v_blocked
      FROM fms.role_grant_blocked_by(
             p_actor_id, v_m.role_id, v_m.scope_type, v_m.scope_id) c;

    IF v_blocked IS NOT NULL THEN
      -- 計數並具名，不是靜默跳過。同一條對應可能命中多個使用者，
      -- 因此去重 —— 回報的是「哪條對應被擋」而不是「被擋幾次」。
      IF NOT (v_m.role_code::text = ANY (blocked_mappings)) THEN
        blocked_mappings := blocked_mappings || v_m.role_code::text;
      END IF;
      CONTINUE;
    END IF;

    INSERT INTO fms.user_role_assignments
      (tenant_id, user_id, role_id, scope_type, scope_id, source,
       origin_directory_group_id)
    SELECT fms.current_tenant_id(), v_m.user_id, v_m.role_id,
           v_m.scope_type, v_m.scope_id, 'DIRECTORY_SYNC', v_m.directory_group_id
     WHERE NOT EXISTS (
       SELECT 1 FROM fms.user_role_assignments ura
        WHERE ura.user_id = v_m.user_id
          AND ura.role_id = v_m.role_id
          AND ura.scope_type = v_m.scope_type
          AND coalesce(ura.scope_id, '00000000-0000-0000-0000-000000000000'::uuid)
              = coalesce(v_m.scope_id, '00000000-0000-0000-0000-000000000000'::uuid));

    -- `GET DIAGNOSTICS` 而不是猜：上面的 INSERT 帶 `WHERE NOT EXISTS`，
    -- 所以它可能影響 0 列（那個人已經有這個角色了）。把 0 當成 1 會讓
    -- `roles_granted` 變成「命中的對應數」而不是「真的新增的授權數」，
    -- 而使用者看那個數字是為了知道這一輪改變了什麼。
    GET DIAGNOSTICS v_ins = ROW_COUNT;
    roles_granted := roles_granted + v_ins;
  END LOOP;

  -- --- 收回 ---------------------------------------------------------------
  -- 只動 `source = 'DIRECTORY_SYNC'` 且來源群組屬於這個 provider 的授權。
  -- **人工指派永遠不動** —— 同步不該吃掉管理員手動給的東西。
  --
  -- 收回的條件：這筆授權對不上任何「還在生效的對應 × 還在群組裡的成員」。
  -- 涵蓋三種情況：人離開群組、對應被停用、對應被刪除。
  WITH gone AS (
    DELETE FROM fms.user_role_assignments ura
     USING fms.directory_groups g
     WHERE ura.source = 'DIRECTORY_SYNC'
       AND ura.origin_directory_group_id = g.id
       AND g.identity_provider_id = p_identity_provider_id
       AND NOT EXISTS (
         SELECT 1
           FROM fms.directory_role_mappings m
           JOIN fms.user_directory_groups udg
                ON udg.directory_group_id = m.directory_group_id
          WHERE m.is_active
            AND m.directory_group_id = ura.origin_directory_group_id
            AND m.role_id = ura.role_id
            AND m.scope_type = ura.scope_type
            AND coalesce(m.scope_id, '00000000-0000-0000-0000-000000000000'::uuid)
                = coalesce(ura.scope_id, '00000000-0000-0000-0000-000000000000'::uuid)
            AND udg.user_id = ura.user_id)
     RETURNING ura.id)
  SELECT count(*)::int INTO roles_revoked FROM gone;

  RETURN NEXT;
END;
$$;

COMMENT ON FUNCTION fms.reconcile_directory_roles(uuid, uuid) IS
  '目錄同步的對帳：directory_role_mappings 的第一個消費者。'
  ' 會發放**也會收回**（roles_revoked）—— 只加不減會讓「改對應」沒有效果，'
  ' 而 DELETE /role-assignments 的錯誤訊息正是要求使用者去改對應。'
  ' 提權防護的判定對象是觸發同步的人（對應表沒有 created_by）；'
  ' 被擋的對應計入 blocked_mappings 而不是靜默跳過。'
  ' **不連 AD／Entra** —— 成員關係由別人放進 user_directory_groups。';

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構的：行為驗證需要 provider、群組、成員關係與對應四層資料，
-- 而 CORE 階段沒有（053／054／056／057 記過同一個層次問題）。
-- 行為在 `directory_sync_slice.rs`。
DO $$
DECLARE v_src text;
BEGIN
  IF to_regprocedure('fms.reconcile_directory_roles(uuid,uuid)') IS NULL THEN
    RAISE EXCEPTION '058 FAILED: 函式不存在';
  END IF;
  v_src := pg_get_functiondef('fms.reconcile_directory_roles(uuid,uuid)'::regprocedure);

  -- (1) **一定要有收回。** 只發不收會讓 DELETE /role-assignments 的錯誤訊息
  --     （「要移除請改群組對應」）變成一句假話。
  IF v_src NOT LIKE '%DELETE FROM fms.user_role_assignments%' THEN
    RAISE EXCEPTION
      '058 FAILED: 沒有收回的路徑 —— 只加不減會讓「改對應」沒有效果，'
      '而 DELETE /role-assignments 正是要求使用者去改對應';
  END IF;

  -- (2) 收回只能動 DIRECTORY_SYNC。少了這個條件，同步會吃掉人工指派。
  IF v_src NOT LIKE '%source = ''DIRECTORY_SYNC''%' THEN
    RAISE EXCEPTION '058 FAILED: 收回沒有限定 source —— 會吃掉人工指派';
  END IF;

  -- (3) 提權防護要在。少了它，一條既有的對應就能把 PLATFORM_ADMIN
  --     發給整個群組 —— 而那是繞過 052 的第三條路徑。
  IF v_src NOT LIKE '%role_grant_blocked_by%' THEN
    RAISE EXCEPTION
      '058 FAILED: 沒有提權防護 —— 同步是第三條授權路徑，'
      '不檢查就能用一條既有對應把 PLATFORM_ADMIN 發給整個群組';
  END IF;

  -- (4) 被擋的對應要被具名回報，不是靜默跳過。
  IF v_src NOT LIKE '%blocked_mappings := blocked_mappings%' THEN
    RAISE EXCEPTION '058 FAILED: 被擋的對應沒有回報 —— 「設定了但不生效」會看不見';
  END IF;

  -- (5) 不可以是 SECURITY DEFINER：那會讓它成為跨租戶發放角色的後門。
  IF EXISTS (SELECT 1 FROM pg_proc
              WHERE oid = 'fms.reconcile_directory_roles(uuid,uuid)'::regprocedure
                AND prosecdef) THEN
    RAISE EXCEPTION '058 FAILED: 函式是 SECURITY DEFINER';
  END IF;

  RAISE NOTICE '058 OK：會收回、只動 DIRECTORY_SYNC、有提權防護、被擋會具名'
               '（行為驗證在 directory_sync_slice.rs）';
END;
$$;

COMMIT;
