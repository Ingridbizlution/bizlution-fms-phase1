-- =============================================================================
-- Down migration 055：移除平台技能目錄
-- =============================================================================
-- `user_skills.skill_id` 是 ON DELETE CASCADE，因此**引用這些技能的使用者
-- 技能紀錄會一併消失**（含證照號碼與到期日）。那不是可以重建的資料。
-- 回退前請先自行備份 fms.user_skills。
-- =============================================================================

-- **需要平台情境，與 up 一樣。** 我修了 up 卻漏了這裡，而失效方式很安靜：
-- `skills` 的 `tenant_read` 允許無情境**讀取**平台列（述詞裡有
-- `tenant_id IS NULL` 那個分支），但 `tenant_write` **不允許刪除**它們。
--
-- 於是 DELETE 影響 0 列而不報錯，接著自我驗證讀得到 9 列 ——
-- CI 的 migrate-roundtrip 就是這樣抓到的（`down 055 FAILED: 還有 9 項`）。
-- 若這個 down 沒有自我驗證，它會「成功」而什麼都沒做。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

DELETE FROM fms.skills WHERE tenant_id IS NULL AND id::text LIKE '50000000%';

DO $$
DECLARE v_n int;
BEGIN
  SELECT count(*) INTO v_n FROM fms.skills WHERE tenant_id IS NULL;
  IF v_n <> 0 THEN
    RAISE EXCEPTION 'down 055 FAILED: 還有 % 項平台技能', v_n;
  END IF;
  RAISE NOTICE 'down 055 OK（提醒：連帶刪除的 user_skills 無法復原）';
END;
$$;

COMMIT;
