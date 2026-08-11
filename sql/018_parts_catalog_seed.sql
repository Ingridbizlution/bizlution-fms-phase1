-- =============================================================================
-- 018  Parts catalogue and stock seed
-- =============================================================================
-- `fms.parts` 與 `fms.part_stock` 在 008／009 完全沒有資料（0 筆），
-- 而 `WorkOrderTransitionRequest.parts_used` 需要真實的 `part_id`
-- （`work_order_parts.part_id` 是 `ON DELETE RESTRICT` 的外鍵）。
-- 沒有料件目錄，契約的那個欄位就無法使用。
--
-- 料號取自 017 的型號 `spare_part_codes` 與 009 範本的耗材，
-- 讓「型號 → 備品 → 庫存」這條線在示範資料裡是連貫的。
--
-- 與 017 同樣放在新檔案：規格書交付的種子檔保持原樣。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

-- 需要平台情境才讀寫得到租戶資料：以 fms_owner 執行時 FORCE RLS 仍然生效，
-- 未設情境時 `current_tenant_id()` 為 NULL，政策會濾掉全部列 ——
-- 症狀是自我驗證誤報「租戶不存在」。與 011／017 同一個做法。
SELECT set_config('app.is_platform', 'on', true);

INSERT INTO fms.parts
  (tenant_id, part_code, name, unit, unit_cost, currency, manufacturer,
   manufacturer_part_no, is_consumable)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001',
       p.code, p.name, p.unit, p.cost, 'TWD', p.maker, p.maker_no, p.consumable
FROM (VALUES
  ('FILT-MERV13-24X24', 'MERV13 初級濾網 24x24', 'PCS', 850.00,  'Camfil', '30/30-24X24', true),
  ('BELT-AHU-B54',      '空調箱傳動皮帶 B54',     'PCS', 420.00,  'Gates',  'B54', true),
  ('BATT-VRLA-12V100AH','UPS 電池 12V100Ah',      'PCS', 3200.00, 'CSB',    'GPL121000', false),
  ('LAMP-BARCO-SP4K',   'Barco SP4K 光源模組',    'PCS', 68000.00,'Barco',  'R9801276', false),
  ('FILT-BARCO-SP4K',   'Barco SP4K 空氣濾網',    'PCS', 1500.00, 'Barco',  'R9801277', true)
) AS p(code, name, unit, cost, maker, maker_no, consumable)
WHERE EXISTS (SELECT 1 FROM fms.tenants WHERE id = 'aaaaaaaa-0000-4000-8000-000000000001')
ON CONFLICT DO NOTHING;

-- 庫存：總部備空調耗材，影廳備投影機耗材。
-- 刻意讓 UPS 電池在總部有庫存但影廳沒有 —— 「這個場域沒有這個料件的庫存」
-- 是真實情境，而 API 對它的行為（照樣記錄用量、不連結庫存）需要測試涵蓋。
INSERT INTO fms.part_stock (tenant_id, part_id, facility_id, quantity_on_hand, reorder_point)
SELECT 'aaaaaaaa-0000-4000-8000-000000000001', pt.id, s.facility_id, s.qty, s.reorder
FROM (VALUES
  ('FILT-MERV13-24X24', 'cccccccc-0000-4000-8000-000000000001'::uuid, 24.000, 8.000),
  ('BELT-AHU-B54',      'cccccccc-0000-4000-8000-000000000001'::uuid,  6.000, 2.000),
  ('BATT-VRLA-12V100AH','cccccccc-0000-4000-8000-000000000001'::uuid, 16.000, 4.000),
  ('LAMP-BARCO-SP4K',   'cccccccc-0000-4000-8000-000000000002'::uuid,  2.000, 1.000),
  ('FILT-BARCO-SP4K',   'cccccccc-0000-4000-8000-000000000002'::uuid,  8.000, 2.000)
) AS s(part_code, facility_id, qty, reorder)
JOIN fms.parts pt
  ON pt.tenant_id = 'aaaaaaaa-0000-4000-8000-000000000001'
 AND lower(pt.part_code) = lower(s.part_code)
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_parts int;
  v_stock int;
  v_nocost int;
BEGIN
  SELECT count(*) INTO v_parts FROM fms.parts;
  SELECT count(*) INTO v_stock FROM fms.part_stock;
  -- unit_cost 為 NULL 的料件無法計算工單成本，種子不該留這種洞
  SELECT count(*) INTO v_nocost FROM fms.parts WHERE unit_cost IS NULL;

  IF v_parts = 0 THEN
    RAISE NOTICE '018 SKIPPED: 示範租戶不存在（尚未執行 009）';
    RETURN;
  END IF;
  IF v_nocost > 0 THEN
    RAISE EXCEPTION '018 FAILED: % 筆料件沒有 unit_cost，成本無法計算', v_nocost;
  END IF;
  RAISE NOTICE '018 OK: 料件 % 筆、庫存 % 筆', v_parts, v_stock;
END;
$$;

COMMIT;
