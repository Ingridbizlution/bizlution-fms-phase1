-- =============================================================================
-- 060：遙測三張表補上場域 RLS，並補一個 alarm_rule:read
-- =============================================================================
-- 為什麼需要它：一個宣告為場域級的權限，實際上是租戶級
--
-- `telemetry:read` 的 `min_scope_level` 是 `FACILITY`，而 006 建的三張表
-- （`telemetry_points`、`telemetry_readings`、`telemetry_latest`）只有
-- tenant-only 的 PERMISSIVE 政策。`devices` 與 `alarm_rules` 有 RESTRICTIVE 的
-- `facility_in_scope`，那三張沒有。
--
-- 量過（在 fms_template 上，`app.facility_ids` 只含台北總部大樓，
-- 也就是 `begin_tenant_tx` 給 fm.lin 的情境）：
--
--   表                    別場域的資料看得到幾列
--   devices                 0   ← 場域 RLS 有效
--   telemetry_points        1   ← 洩漏
--   telemetry_readings      1   ← 洩漏
--   telemetry_latest        1   ← 洩漏
--
-- 也就是說：場域管理員看不到別場域的**裝置**，卻看得到那台裝置的**點位、
-- 讀數與最新值**。在 `/telemetry/latest` 與 `/telemetry/series` 之前沒有人
-- 讀那三張表，所以這個洞一直沒有出口 —— 那兩支端點就是出口。
--
-- -----------------------------------------------------------------------------
-- 述詞為什麼走 EXISTS 而不是把 facility_id 反正規化下來
-- -----------------------------------------------------------------------------
-- `telemetry_readings` 是按月分區的時序表，加一欄要回填並改寫每個分區，
-- 而 `ingest_telemetry` 也得跟著改。
--
-- 量過（fms_template，單一點位灌 100k 列、ANALYZE 之後，
-- 以 fm.lin 的場域情境跑 `WHERE point = ? AND observed_at >= now() - 24h
-- ORDER BY observed_at DESC LIMIT 500`）：
--
--   Index Scan using telemetry_readings_2026m08_telemetry_point_id_observed_at_idx
--     (actual time=0.065..0.300 rows=500)
--   Filter: ... OR (hashed SubPlan 9)
--   Execution Time: 0.35 ms
--
-- 兩件事是關鍵：
--
--   1. **`hashed SubPlan`** —— 規劃器把 EXISTS 變成一次性的雜湊，
--      不是每列重跑一次子查詢。
--   2. **索引條件保留** —— `(telemetry_point_id, observed_at)` 仍然走索引，
--      RLS 沒有把它變成 Seq Scan。
--
-- 所以反正規化 `facility_id` 現在沒有理由做。若哪天資料量證明它不夠快，
-- 那仍然是可行的第二步 —— 但不該先做。
--
-- -----------------------------------------------------------------------------
-- 順便補 alarm_rule:read
-- -----------------------------------------------------------------------------
-- 008 只有 `alarm_rule:write`（三個角色持有）。於是 `GET /alarm-rules` 只有
-- 改得動規則的人讀得到 —— 而 `alarm:read` 十個角色都有。
--
-- 技師看得到「冷氣過熱」這個告警，卻看不到「超過 28 度就響」這個門檻，
-- 那是把「為什麼會響」藏起來。規則是：**看得到告警的人就看得到產生它的門檻**，
-- 所以授予的對象直接由 `alarm:read` 的持有者推導，不是另寫一份名單。
-- =============================================================================

-- 平台情境必須在 BEGIN 之前設：`role_permissions` 沒有 `tenant_id` 欄位，
-- 於是 `trg_audit_row` 的稽核列退回 `current_tenant_id()`（migration 裡是 NULL），
-- 而 `audit_log` 的政策會擋掉那一列。症狀是
-- `new row violates row-level security policy for table "audit_log"`，
-- 看起來像稽核表壞了，其實是缺情境。與 055／down/055 同一課。
SET app.is_platform = 'on';

BEGIN;

-- -----------------------------------------------------------------------------
-- (1) 三張遙測表的場域 RESTRICTIVE 政策
-- -----------------------------------------------------------------------------
-- RESTRICTIVE 而不是 PERMISSIVE：PERMISSIVE 之間是 OR，補一條寬鬆的政策
-- 只會讓可見範圍變大。場域收斂一定要 AND 進去。
--
-- 政策裡的子查詢本身也受 `devices` 的 RLS 管，所以這裡的 `facility_in_scope`
-- 是**第二道**而不是唯一一道。明寫它的理由是不想依賴 `devices` 的政策
-- 保持現狀 —— 兩處都改才會鬆綁，比一處就鬆綁安全。

CREATE POLICY facility_scope ON fms.telemetry_points
AS RESTRICTIVE FOR ALL
USING (
  fms.is_platform_context()
  OR EXISTS (
       SELECT 1 FROM fms.devices d
        WHERE d.id = telemetry_points.device_id
          AND fms.facility_in_scope(d.facility_id))
);

-- `telemetry_latest` 自己就有 `device_id`，不必繞 points。
CREATE POLICY facility_scope ON fms.telemetry_latest
AS RESTRICTIVE FOR ALL
USING (
  fms.is_platform_context()
  OR EXISTS (
       SELECT 1 FROM fms.devices d
        WHERE d.id = telemetry_latest.device_id
          AND fms.facility_in_scope(d.facility_id))
);

-- `telemetry_readings` 只有 `telemetry_point_id`，要兩層。
CREATE POLICY facility_scope ON fms.telemetry_readings
AS RESTRICTIVE FOR ALL
USING (
  fms.is_platform_context()
  OR EXISTS (
       SELECT 1 FROM fms.telemetry_points p
         JOIN fms.devices d ON d.id = p.device_id
        WHERE p.id = telemetry_readings.telemetry_point_id
          AND fms.facility_in_scope(d.facility_id))
);

-- -----------------------------------------------------------------------------
-- (2) alarm_rule:read
-- -----------------------------------------------------------------------------
INSERT INTO fms.permissions (code, resource, action, module, description, min_scope_level, is_dangerous)
VALUES ('alarm_rule:read', 'alarm_rule', 'read', 'IOT',
        '查看告警規則與門檻（看得到告警的人就看得到產生它的門檻）', 'FACILITY', false)
ON CONFLICT (code) DO UPDATE
  SET resource = EXCLUDED.resource,
      action = EXCLUDED.action,
      module = EXCLUDED.module,
      description = EXCLUDED.description,
      min_scope_level = EXCLUDED.min_scope_level;

-- 授予對象由 `alarm:read` 的持有者推導 —— 不是另寫一份會漂移的名單。
-- 只動平台角色（`tenant_id IS NULL`）：租戶自訂角色的權限是租戶自己的事。
INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT DISTINCT rp.role_id, 'alarm_rule:read'
  FROM fms.role_permissions rp
  JOIN fms.roles r ON r.id = rp.role_id
 WHERE rp.permission_code = 'alarm:read'
   AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;

-- -----------------------------------------------------------------------------
-- 自我驗證
-- -----------------------------------------------------------------------------
-- 結構檢查放這裡，**行為驗證（別場域真的看不到）放 telemetry_read_slice.rs**。
-- 理由與 059 相同：這支 migration 沒有場域級使用者的情境可用，
-- 而在 migration 裡佈置探測資料會留下業務資料。
DO $$
DECLARE
  v_missing text[];
  v_roles   int;
  v_expect  int;
BEGIN
  -- (1) 三張表都要有 RESTRICTIVE 的 facility_scope。
  --     漏一張的症狀是「另外兩張擋住了，看起來像修好了」。
  SELECT array_agg(t) INTO v_missing
    FROM unnest(ARRAY['telemetry_points','telemetry_readings','telemetry_latest']) AS t
   WHERE NOT EXISTS (
     SELECT 1 FROM pg_policies p
      WHERE p.schemaname = 'fms' AND p.tablename = t
        AND p.policyname = 'facility_scope' AND p.permissive = 'RESTRICTIVE');
  IF v_missing IS NOT NULL THEN
    RAISE EXCEPTION '060 FAILED: 這些表沒有 RESTRICTIVE 的 facility_scope：%', v_missing;
  END IF;

  -- (2) 授予對象必須與 alarm:read 的持有者一致。數量對不上表示推導壞了
  --     （例如 JOIN 條件寫錯而只授到一個角色）。
  SELECT count(*) FILTER (WHERE rp.permission_code = 'alarm_rule:read'),
         count(*) FILTER (WHERE rp.permission_code = 'alarm:read')
    INTO v_roles, v_expect
    FROM fms.role_permissions rp
    JOIN fms.roles r ON r.id = rp.role_id
   WHERE r.tenant_id IS NULL;
  IF v_roles <> v_expect THEN
    RAISE EXCEPTION
      '060 FAILED: alarm_rule:read 授予 % 個平台角色，alarm:read 有 % 個 —— 推導壞了',
      v_roles, v_expect;
  END IF;

  RAISE NOTICE '060 OK：三張遙測表有 RESTRICTIVE facility_scope、'
               'alarm_rule:read 授予 % 個平台角色（行為驗證在 telemetry_read_slice.rs）',
               v_roles;
END;
$$;

COMMIT;
