-- =============================================================================
-- 017  Asset model catalogue seed (WBS 4.8)
-- =============================================================================
-- 為什麼需要這個檔案
--
-- `fms.asset_models` 在 008／009 完全沒有資料（平台 0 筆、租戶 0 筆），
-- 而契約有 `GET /asset-models`，且 003 的欄位註解說
-- `supported_protocols` 是「used by the compatibility checker」。
-- 空的型錄讓那支端點與相容性檢查都無從示範或測試。
--
-- 刻意放在新檔案而不是改 008／009：規格書交付的種子檔保持原樣，
-- 新增的示範資料就看得出是後補的、可單獨審閱與撤銷。
--
-- 內容對齊 009 已種下的設備：UPS、AHU、投影機各一個平台型號，
-- 外加一個**租戶自建**型號，讓契約的 `scope=all|platform|tenant`
-- 三種過濾都真的有資料可分辨 —— 只有平台資料時，
-- `scope=tenant` 回空陣列與「過濾沒實作」看起來一模一樣。
-- =============================================================================

BEGIN;

SET search_path = fms, public;

-- 平台層資料（`tenant_id IS NULL`）需要平台情境，否則 007 的
-- `tenant_isolation` 政策的 WITH CHECK 會拒絕寫入。與 011 同一個做法。
-- 交易結束即失效（`set_config` 第三參數為 true）。
SELECT set_config('app.is_platform', 'on', true);

-- 平台共用型號（tenant_id IS NULL，所有租戶可見）
INSERT INTO fms.asset_models
  (tenant_id, category_id, manufacturer, model_no, name, description,
   specifications, supported_protocols, power_rating_w, expected_life_months,
   mtbf_hours, spare_part_codes)
SELECT
  NULL,
  (SELECT id FROM fms.asset_categories WHERE code = m.category_code AND tenant_id IS NULL),
  m.manufacturer, m.model_no, m.name, m.description,
  m.specifications, m.protocols, m.power_w, m.life_months, m.mtbf, m.parts
FROM (VALUES
  ('UPS', 'Delta', 'DPH-100K', 'Delta DPH 100kVA 模組化 UPS',
   '三相模組化不斷電系統，支援熱插拔電池模組',
   '{"kva":100,"phases":3,"battery_type":"VRLA","runtime_min_at_full_load":10}'::jsonb,
   ARRAY['MODBUS_TCP','SNMP']::text[], 2500, 180, 250000,
   ARRAY['BATT-VRLA-12V100AH','FAN-DPH-01']::text[]),
  ('AHU', 'Trane', 'CSAA-020', 'Trane 組合式空調箱 20 噸',
   '含變頻風機與二段過濾的組合式空調箱',
   '{"cmh":12000,"cooling_tons":20,"filter_class":"MERV13","fan_type":"VFD"}'::jsonb,
   ARRAY['BACNET_IP','MODBUS_TCP']::text[], 15000, 240, 120000,
   ARRAY['FILT-MERV13-24X24','BELT-AHU-B54']::text[]),
  ('PROJECTOR', 'Barco', 'SP4K-15C', 'Barco 雷射投影機 SP4K-15C',
   '影廳用雷射光源投影機',
   '{"lumens":15000,"resolution":"4096x2160","light_source":"LASER","lamp_life_hours":30000}'::jsonb,
   ARRAY['HTTP','SNMP']::text[], 1800, 120, 30000,
   ARRAY['FILT-BARCO-SP4K','LENS-BARCO-1.2']::text[])
) AS m(category_code, manufacturer, model_no, name, description,
       specifications, protocols, power_w, life_months, mtbf, parts)
ON CONFLICT DO NOTHING;

-- 租戶自建型號（示範租戶自行登錄的機種）
INSERT INTO fms.asset_models
  (tenant_id, category_id, manufacturer, model_no, name, description,
   specifications, supported_protocols, power_rating_w, expected_life_months)
SELECT
  'aaaaaaaa-0000-4000-8000-000000000001',
  (SELECT id FROM fms.asset_categories WHERE code = 'FCU' AND tenant_id IS NULL),
  '協力空調', 'DEMO-FCU-450', '示範租戶自建：吊隱式風機盤管 450CFM',
  '客戶自行採購、不在平台型錄內的機種',
  '{"cfm":450,"coil_rows":3}'::jsonb,
  ARRAY['MODBUS_TCP']::text[], 120, 120
WHERE EXISTS (SELECT 1 FROM fms.tenants WHERE id = 'aaaaaaaa-0000-4000-8000-000000000001')
ON CONFLICT DO NOTHING;

-- 把 009 的設備接上對應型號，讓 `asset_model_id` 不再全為 NULL。
-- 只在該設備尚未指定型號時更新，因此重跑安全。
UPDATE fms.assets a
   SET asset_model_id = m.id
  FROM fms.asset_models m
 WHERE a.asset_model_id IS NULL
   AND m.tenant_id IS NULL
   AND m.category_id = a.category_id
   AND a.id IN ('20000000-0000-4000-8000-000000000001',
                '20000000-0000-4000-8000-000000000002',
                '20000000-0000-4000-8000-000000000003');

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
DO $$
DECLARE
  v_platform int;
  v_tenant   int;
  v_orphan   int;
BEGIN
  SELECT count(*) INTO v_platform FROM fms.asset_models WHERE tenant_id IS NULL;
  SELECT count(*) INTO v_tenant   FROM fms.asset_models WHERE tenant_id IS NOT NULL;

  -- category_id 是 NOT NULL，因此上面的子查詢若找不到分類會整批失敗；
  -- 這裡再確認一次沒有型號指向不存在的分類。
  SELECT count(*) INTO v_orphan
  FROM fms.asset_models m
  LEFT JOIN fms.asset_categories c ON c.id = m.category_id
  WHERE c.id IS NULL;

  IF v_platform < 3 THEN
    RAISE EXCEPTION '017 FAILED: 平台型錄應至少 3 筆，實際 %', v_platform;
  END IF;
  IF v_orphan > 0 THEN
    RAISE EXCEPTION '017 FAILED: % 筆型號指向不存在的分類', v_orphan;
  END IF;
  -- 租戶型號依賴 009，seed 未執行時可以是 0
  RAISE NOTICE '017 OK: 平台型號 % 筆、租戶型號 % 筆', v_platform, v_tenant;
END;
$$;

COMMIT;
