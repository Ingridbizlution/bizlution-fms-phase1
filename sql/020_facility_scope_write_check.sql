-- =============================================================================
-- 020  facility_scope 的寫入側檢查修正
-- =============================================================================
-- # 症狀
--
-- 啟用 `app.facility_ids` 之後（見 3.9／WBS-rebaseline 4.1f(c)），
-- **`POST /facilities` 永遠失敗**，錯誤是 42501
-- 「new row violates row-level security policy」。
--
-- # 原因
--
-- 007 建立的政策是：
--
--     CREATE POLICY facility_scope ON fms.facilities AS RESTRICTIVE FOR ALL
--     USING (is_platform_context() OR facility_in_scope(id))
--
-- `FOR ALL` 且只寫 `USING` 時，PostgreSQL 會把該表達式**同時當作
-- INSERT／UPDATE 的 WITH CHECK**。於是新增場域必須滿足
-- `facility_in_scope(new.id)` —— 而一個還不存在的場域的 id
-- 不可能出現在 `app.facility_ids` 裡。這是無解的自舉問題。
--
-- 同樣的政策套在其他 15 張表上是**正確的**：
-- 新增資產時要求 `facility_id` 在可見範圍內，正是我們要的約束
-- （不該能在看不到的場域裡建東西）。只有 `facilities` 自己會撞上自舉，
-- 因為它的 scope 鍵是自己的主鍵。
--
-- # 修法
--
-- 只針對 `fms.facilities` 重建政策，明確給出 `WITH CHECK`，
-- 讓場域範圍只作用於**讀取與「可以動哪些列」**，不作用於寫入的結果列：
--
--   * `USING` 保持不變 —— 仍然不能讀取、更新、刪除範圍外的場域
--   * `WITH CHECK` 只留平台情境判斷 —— 跨租戶寫入仍由
--     `tenant_isolation` 的 WITH CHECK 阻止（那條沒有被動到）
--
-- 也就是說：場域級隔離本質上是**可見性**問題（規格書的例子是
-- 「廠長只看自己的廠」），把它套在寫入結果列上並不增加安全性，
-- 卻讓建立場域變成不可能。
--
-- # 保留的限制
--
-- 建立場域者在建立**之後**仍然看不到它（除非他有 TENANT 範圍的角色，
-- 或有人指派了該場域的角色給他）。這是刻意的：
-- 修這一點屬於佈建流程（建立場域後要指派角色），不是 RLS 政策的責任。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

DROP POLICY IF EXISTS facility_scope ON fms.facilities;

CREATE POLICY facility_scope ON fms.facilities AS RESTRICTIVE FOR ALL
  USING (fms.is_platform_context() OR fms.facility_in_scope(id))
  WITH CHECK (fms.is_platform_context() OR true);

-- -----------------------------------------------------------------------------
-- 自我驗證：政策必須有獨立的 with_check，否則就是又退回自舉死結
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_qual  text;
  v_check text;
BEGIN
  SELECT qual, with_check INTO v_qual, v_check
  FROM pg_policies
  WHERE schemaname = 'fms' AND tablename = 'facilities' AND policyname = 'facility_scope';

  IF v_check IS NULL THEN
    RAISE EXCEPTION
      '020 FAILED: facility_scope 沒有獨立的 WITH CHECK，USING 會被當成寫入檢查，建立場域仍會失敗';
  END IF;
  IF v_check LIKE '%facility_in_scope%' THEN
    RAISE EXCEPTION '020 FAILED: WITH CHECK 仍含 facility_in_scope，自舉問題未解';
  END IF;
  IF v_qual IS NULL OR v_qual NOT LIKE '%facility_in_scope%' THEN
    RAISE EXCEPTION '020 FAILED: USING 不再限制場域範圍，讀取側的隔離被弄壞了';
  END IF;

  -- tenant_isolation 必須完好無損：跨租戶寫入靠它阻止
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
     WHERE schemaname = 'fms' AND tablename = 'facilities'
       AND policyname = 'tenant_isolation' AND with_check IS NOT NULL
  ) THEN
    RAISE EXCEPTION '020 FAILED: facilities 的 tenant_isolation WITH CHECK 不見了';
  END IF;

  RAISE NOTICE '020 OK: facility_scope 讀取側仍受限、寫入側解除自舉，tenant_isolation 完好';
END;
$$;

COMMIT;
