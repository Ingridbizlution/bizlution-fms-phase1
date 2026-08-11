#!/bin/sh
# =============================================================================
# 套用 migration（以 fms_owner 身分，確保物件擁有者正確）
# =============================================================================
# 為什麼一定要用 fms_owner 而不是 postgres：
#   * 以超級使用者建立的表，其擁有者是 postgres；超級使用者具 BYPASSRLS，
#     日後任何以該身分執行的腳本都會看穿所有租戶。
#   * FORCE ROW LEVEL SECURITY 對擁有者亦生效，因此 fms_owner 執行測試時
#     RLS 斷言仍然有意義。
#
# 用法（由 compose 呼叫）：
#   docker compose run --rm migrate                           # CORE：001–008 + 011 + 013
#   MIGRATE_MODE=seed-only docker compose run --rm migrate    # 只跑 009（在 CORE 之後追加）
#   MIGRATE_MODE=all       docker compose run --rm migrate    # CORE + 009，一次到底
#   MIGRATE_MODE=demo      docker compose run --rm migrate    # all + 075 的示範活動資料
#   MIGRATE_MODE=scale     docker compose run --rm migrate    # demo + 076 的壓測規模夾具
#
# `demo` 與 `all` 的差別只有 075（32 張工單、63 筆預約、20 台資產、6 筆告警）。
# **測試模板用 `all`，不是 `demo`** —— 見下方 DEMO 那一段的說明。
#
# 注意 seed-only 與 all 的差別：CORE 裡的 001 使用 CREATE TABLE（無 IF NOT EXISTS），
# 因此不可重跑。已經跑過 CORE 之後要補示範資料，必須用 seed-only；用 all 會在
# 001 的 "relation \"tenants\" already exists" 失敗。
# （舊的 MIGRATE_MODE=seed 等同 all，保留為別名以維持相容。）
#
# CORE 會用 public.schema_migrations 記錄哪些檔案已經套用過，重跑時自動略過
# 已套用的項目（見 run_core()）——這樣 CI 的 deploy job 才能每次 push 到 main
# 都直接呼叫 migrate，不需要分辨這是第一次還是後續部署。SEED／POST_SEED／
# DEMO／SCALE 沒有這層追蹤，語意跟原來一樣（各自的說明見下方對應區塊）。
# =============================================================================
set -eu

HOST="${PGHOST:-postgres}"
PORT="${PGPORT:-5432}"
DB="${POSTGRES_DB:-fms}"
USER="fms_owner"
MODE="${MIGRATE_MODE:-schema}"

CORE="001_foundation.sql
002_identity_directory_rbac.sql
003_spatial_assets.sql
004_work_orders_maintenance_service.sql
005_reservations.sql
006_iot_alarms_notifications.sql
007_rls_policies.sql
008_seed_platform.sql
011_offision_parity.sql
013_platform_context_hardening.sql
014_preauth_tenant_resolution.sql
015_work_order_action_catalog.sql
016_permission_codes.sql
020_facility_scope_write_check.sql
021_accessible_facilities_definer.sql
022_transition_enforces_permission.sql
023_security_review_fixes.sql
024_auth_event_trail.sql
025_idempotency_keys_per_user.sql
026_enforce_min_scope_level.sql
027_split_facility_write.sql
028_time_partition_maintenance.sql
029_audit_trail.sql
030_meter_value_rule.sql
031_role_catalogue_decisions.sql
032_sla_measurement_chain.sql
033_sla_state_sweep.sql
034_sla_compliance_report.sql
035_sla_escalation.sql
036_sla_thresholds_from_catalogue.sql
037_sla_policy_administration.sql
038_business_hours.sql
039_report_reads_sla_basis.sql
040_holiday_administration.sql
041_notification_fanout.sql
042_notification_template_administration.sql
043_inapp_is_delivered_on_insert.sql
044_reopen_clears_completion.sql
045_role_catalogue_cleanup.sql
046_facility_scope_must_be_restrictive.sql
047_notify_templates_and_perm_tokens.sql
048_org_manager_writes_sla_policies.sql
049_audit_work_orders_and_assets.sql
050_tenant_wide_rows_need_tenant_scope.sql
051_backup_role_grants.sql
052_role_assignment_escalation_guard.sql
053_audit_log_tenant_wide_rows_are_readable.sql
054_audit_exports.sql
055_platform_skill_catalogue.sql
056_work_order_from_alarm.sql
057_evaluate_telemetry_rules.sql
058_directory_sync_reconcile.sql
059_certification_expiry_reminder.sql
060_telemetry_facility_scope.sql
061_alarm_rule_predicates.sql
062_child_table_facility_scope.sql
063_pm_compliance_chain.sql
064_asset_status_history_writer.sql
065_reporting_four.sql
066_report_exports.sql
067_satisfaction.sql
068_service_item_availability.sql
069_tree_move_cycle_guard.sql
070_refresh_token_revocation.sql
071_alarm_suppression.sql
072_webhook_subscriptions.sql
073_sso_auth_requests.sql
074_scim_tokens.sql
077_directory_mappings_need_a_group.sql
078_directory_sync_service_account.sql
081_device_connectivity_function.sql
082_reservation_availability_permission.sql
083_calendar_federation.sql
085_reservation_reminder.sql
086_floor_plan_markers.sql"

SEED="009_seed_demo_tenant.sql"

# 種子之後才能執行的檔案：017 的租戶自建型號與「把示範設備接上型號」
# 都要等 009 種完設備。平台型號部分不依賴 009，但同一個檔案不值得拆開。
POST_SEED="017_asset_model_catalog_seed.sql
018_parts_catalog_seed.sql
019_pm_generator_service_account.sql
079_directory_sync_demo_service_account.sql
080_bim_ingest_worker_service_account.sql
084_calendar_sync_worker_service_account.sql"

echo "==> 目標：$USER@$HOST:$PORT/$DB（模式 $MODE）"

run() {
  f="$1"
  echo "--> $f"
  psql -v ON_ERROR_STOP=1 -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -q -f "/sql/$f"
}

# 放 public schema 不是 fms，因為第一次跑的時候 fms schema 還不存在（001 才會建）。
# 建完要補一句 GRANT——02-backup-role.sh 只給 fms_backup public schema 的
# USAGE，沒有把裡面的表也授權出去（那邊的註解說明了為什麼：public 平常沒有
# 表，因此沒有預設授權），backup-restore-drill 的 pg_dump 因此會在這張表卡
# permission denied。
psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -q -c \
  "CREATE TABLE IF NOT EXISTS public.schema_migrations (
     filename text PRIMARY KEY,
     applied_at timestamptz NOT NULL DEFAULT now()
   );
   GRANT SELECT ON public.schema_migrations TO fms_backup;"

run_core() {
  f="$1"
  applied=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -Atc \
    "SELECT 1 FROM public.schema_migrations WHERE filename = '$f'")
  if [ "$applied" = "1" ]; then
    echo "--> $f（已套用，略過）"
    return
  fi
  run "$f"
  psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -q -c \
    "INSERT INTO public.schema_migrations (filename) VALUES ('$f')"
}

# 順序不可調換：005 才補上 work_orders→reservations 外鍵，006 才補 →alarms，
# 008 必須在任何工單資料寫入前執行（work_orders.status 有外鍵），
# 013 必須在種子之後（會改寫 is_platform_context 的判定條件），
# **CORE 一律按編號遞增**。這裡曾經因為「020 屬 RLS 硬化，主題上接在 013 之後」
# 而把 020–023 插到 014–016 前面，結果 023 參照 015 建立的表時失敗
# （`relation "fms.work_order_actions" does not exist`）。
# 開發資料庫沒有暴露它 —— 那裡是逐個手動套用的，順序剛好是對的；
# 只有從零建立 template 時才會踩到。編號序是唯一不需要每次重新推理的規則。
#
# 014 依賴 013 已完成的硬化，因此排在其後，
# 015 的自我驗證要讀 008 種下的狀態機規則，因此必須在 008 之後。
if [ "$MODE" != "seed-only" ]; then
  for f in $CORE; do run_core "$f"; done
fi

if [ "$MODE" = "seed-only" ] || [ "$MODE" = "seed" ] || [ "$MODE" = "all" ] \
   || [ "$MODE" = "demo" ] || [ "$MODE" = "scale" ]; then
  run "$SEED"
  for f in $POST_SEED; do run "$f"; done
fi

# 示範**活動**資料（工單／預約／告警／資產），讓前端每個畫面都不是空的。
#
# **自己一個模式，刻意不含在 `all` 裡。** 兩個理由，第二個是硬性的：
#
# 1. 生產環境不該有 32 張假工單與 63 筆假預約 —— 混進真實資料之後沒有可靠的
#    方法分辨。
# 2. **`make-test-template.sh` 用的就是 `all`。** 放進 `all` 會讓那 63 筆預約
#    出現在每一個測試資料庫裡，於是任何斷言列數的測試（「清單應該回 2 筆」）
#    都會壞掉 —— 而那個失敗看起來像測試本身的問題。
#
# 因此：`all` = CORE + 種子骨架（測試模板要的）；`demo` = `all` + 活動資料。
DEMO="075_seed_demo_activity.sql"

if [ "$MODE" = "demo" ] || [ "$MODE" = "scale" ]; then
  for f in $DEMO; do run "$f"; done
fi

# 壓力測試的規模夾具：250 帳號／100 教室／1000 裝置／200 工單。
#
# **獨立一個模式，既不在 `all` 也不在 `demo`。** 它把 users 從 7 變成 257、
# bookable_resources 從 4 變成 104 —— 進了 `all` 就會讓每一個斷言列數的
# 整合測試同時紅掉（與 075 不在 `all` 是同一個理由，見上）。
#
# **重跑它就是把負載夾具重設**：它會刪掉負載帳號名下的預約。
# 兩次量測之間一定要重跑，否則第二次會撞到第一次留下的時段
# （實測 11.3% 的建立變成 409），而那一格就變成在量衝突處理。
SCALE="076_seed_scale_load.sql"

if [ "$MODE" = "scale" ]; then
  for f in $SCALE; do run "$f"; done
fi

echo "==> Migration 完成。物件擁有者：$USER"
psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -Atc \
  "SELECT '資料表數：' || count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname='fms' AND c.relkind IN ('r','p');"
psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -Atc \
  "SELECT 'RLS 已啟用的表：' || count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname='fms' AND c.relrowsecurity;"
psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB" -Atc \
  "SELECT '權限項數：' || count(*) FROM fms.permissions;"
