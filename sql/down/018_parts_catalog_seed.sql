-- 回退 018：移除料件目錄與庫存。
--
-- 若料件已被工單領用，`work_order_parts.part_id` 的 ON DELETE RESTRICT
-- 會讓刪除失敗。那是**正確的行為**：還在被引用的資料不該被靜默刪掉。
-- 遇到這個錯誤代表資料庫已經有依賴這批種子的業務資料，
-- 應該先處理那些工單，或根本不要回退這個 migration。
BEGIN;
SET search_path = fms, public;
SELECT set_config('app.is_platform', 'on', true);

DELETE FROM fms.part_stock
 WHERE part_id IN (
   SELECT id FROM fms.parts
    WHERE lower(part_code) IN ('filt-merv13-24x24','belt-ahu-b54','batt-vrla-12v100ah',
                               'lamp-barco-sp4k','filt-barco-sp4k'));

DELETE FROM fms.parts
 WHERE lower(part_code) IN ('filt-merv13-24x24','belt-ahu-b54','batt-vrla-12v100ah',
                            'lamp-barco-sp4k','filt-barco-sp4k');

COMMIT;
