# =============================================================================
# Facility Management System — 本機開發環境操作入口
# =============================================================================
# make help 列出所有指令。所有指令都在 docker/ 目錄下執行。
# =============================================================================

SHELL := /bin/sh
COMPOSE := docker compose
PSQL_OWNER := $(COMPOSE) exec -e PGPASSWORD=$$FMS_OWNER_PASSWORD postgres psql -U fms_owner -d fms
PSQL_APP := $(COMPOSE) exec -e PGPASSWORD=$$FMS_APP_PASSWORD postgres psql -U fms_app -d fms

.DEFAULT_GOAL := help
.PHONY: help up down stop logs ps migrate seed fresh test psql psql-app redis minio mail tools iot reset check-rls explain-plan backup restore hooks backup-restore-drill audit-overhead-bench backup-role otel otel-smoke load-fixture

help: ## 顯示可用指令
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	 | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

# ---------------------------------------------------------------- 生命週期 ----
up: ## 啟動基礎服務（postgres、redis、minio、mailpit）
	$(COMPOSE) up -d postgres redis minio mailpit
	$(COMPOSE) up minio-init
	@echo ""
	@echo "PostgreSQL  localhost:$${POSTGRES_PORT:-5433}"
	@echo "Redis       localhost:$${REDIS_PORT:-6380}"
	@echo "MinIO API   http://localhost:$${MINIO_PORT:-9000}"
	@echo "MinIO 控制台 http://localhost:$${MINIO_CONSOLE_PORT:-9001}"
	@echo "Mailpit UI  http://localhost:$${MAILPIT_UI_PORT:-8025}"
	@echo ""
	@echo "下一步： make migrate && make seed && make test"

hooks: ## 啟用 git hooks（拒絕直接推送 main）
	@git -C .. config core.hooksPath .githooks
	@chmod +x ../.githooks/*
	@echo "core.hooksPath = .githooks（拒絕直接推送 main；緊急時用 git push --no-verify）"
	@echo ""
	@echo "這只是替代方案。真正的防線是 GitHub branch protection／ruleset，"
	@echo "但私有 repo 需要 GitHub Pro 或轉公開 —— 那是商業決定。"

tools: ## 額外啟動 Adminer（資料庫瀏覽）
	$(COMPOSE) --profile tools up -d adminer
	@echo "Adminer http://localhost:$${ADMINER_PORT:-8080}（伺服器填 postgres，使用者 fms_owner）"

otel: ## 額外啟動 OpenTelemetry collector（trace 匯出的接收端）
	$(COMPOSE) --profile otel up -d otel-collector
	@echo "OTLP HTTP  http://localhost:$${OTEL_HTTP_PORT:-4318}"
	@echo ""
	@echo "應用端要設 OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:$${OTEL_HTTP_PORT:-4318}"
	@echo "沒設就只有 JSON log，不會匯出（見 fms-shared 的 init_telemetry）。"
	@echo "看收到的 span： docker compose logs -f otel-collector"

otel-smoke: ## 驗證 span 真的從應用程式抵達 collector
	$(COMPOSE) --profile otel up -d otel-collector
	sh scripts/otel-smoke.sh

iot: ## 額外啟動 MQTT broker
	$(COMPOSE) --profile iot up -d mosquitto

stop: ## 停止所有容器（保留資料）
	$(COMPOSE) stop

down: ## 移除容器（保留資料卷）
	$(COMPOSE) down

reset: ## 完全重建：刪除容器與資料卷後重新初始化
	$(COMPOSE) down -v
	$(COMPOSE) up -d postgres redis minio mailpit
	$(COMPOSE) up minio-init
	@echo "已重建乾淨環境。請執行 make migrate"

ps: ## 檢視服務狀態
	$(COMPOSE) ps

logs: ## 追蹤 PostgreSQL 日誌（含慢查詢 >300ms）
	$(COMPOSE) logs -f postgres

# ------------------------------------------------------------------ 資料庫 ----
migrate: ## 套用 001–008、011、013（不含示範資料）
	$(COMPOSE) --profile jobs run --rm migrate

seed: ## 追加示範租戶 009（開發／前端 fixtures 用；不重跑 001–013）
	$(COMPOSE) --profile jobs run --rm -e MIGRATE_MODE=seed-only migrate

fresh: ## reset + migrate + seed 一次到底
	$(MAKE) reset
	@sleep 3
	$(MAKE) migrate
	$(MAKE) seed
	@echo "環境已就緒。執行 make test 驗證。"

migrate-down: ## 回退 migration 至 TO=<版本>（僅 014 以後，見 sql/down/README.md）
	$(COMPOSE) --profile jobs run --rm -e TO=$(TO) --entrypoint /bin/sh migrate /scripts/migrate-down.sh

migrate-roundtrip: ## 驗證 down migration 可逆（up → down → up，schema 必須相同）
	$(COMPOSE) --profile jobs run --rm --entrypoint /bin/sh migrate /scripts/migrate-roundtrip.sh

test-template: ## 建立測試 template 資料庫（整合測試由它複製出各自的 DB）
	$(COMPOSE) --profile jobs run --rm --entrypoint /bin/sh migrate /scripts/make-test-template.sh

test: ## 執行煙霧測試 010（T1–T4）與 012（T5–T10）
	$(COMPOSE) --profile jobs run --rm smoke-test

check-isolation: ## 以 fms_app 驗證租戶隔離（未設 context／宣稱平台情境／正確 context）
	$(COMPOSE) --profile jobs run --rm check-isolation

concurrency-test: ## T11：100 個客戶端同時搶同一時段，須恰好一筆成功
	$(COMPOSE) --profile jobs run --rm concurrency-test

# 不用 `-e FMS_BACKUP_PASSWORD=$$...`：make 不會讀 .env，那個變數在 make 的
# shell 裡是空的，於是腳本會退回內建預設值，而演練走 TCP 時帶的是 .env 的值
# —— 兩邊不一致，症狀是認證失敗。密碼改由 compose 從 .env 注入 postgres 服務
# 的環境（見 docker-compose.yml），腳本直接讀它。
#
# 既有容器是在加這個環境變數之前建的，所以要先 `make up` 讓它重建
# （資料在具名卷裡，不會掉）。
backup-role: ## 建立／更新 fms_backup 角色（BYPASSRLS、唯讀）—— 既有資料卷用
	$(COMPOSE) up -d postgres
	$(COMPOSE) exec postgres sh /docker-entrypoint-initdb.d/02-backup-role.sh

backup-restore-drill: ## 驗證備份真的還原得回來（政策／FORCE RLS／觸發器／分割／列數）
	$(COMPOSE) --profile jobs run --rm backup-restore-drill

audit-overhead-bench: ## 量 049 稽核寫入放大的倍數（只記錄，不判成敗）
	$(COMPOSE) --profile jobs run --rm audit-overhead-bench

load-fixture: ## 建（或重設）壓力測試的規模夾具：250 帳號/100 教室/1000 裝置/200 工單
	MIGRATE_MODE=scale $(MAKE) migrate

verify: ## 完整驗證：check-rls + check-isolation + test + concurrency-test（CI 同一組指令）
	$(MAKE) check-rls
	$(MAKE) check-isolation
	$(MAKE) test
	$(MAKE) concurrency-test

psql: ## 以 fms_owner 進入 psql（可跑 migration 與平台情境）
	$(PSQL_OWNER)

psql-app: ## 以 fms_app 進入 psql（RLS 完整生效，用來驗證隔離）
	$(PSQL_APP)

check-rls: ## 檢查所有租戶表是否都已啟用並強制 RLS
	@$(PSQL_OWNER) -Atc "\
	SELECT CASE WHEN count(*) = 0 THEN 'OK：所有含 tenant_id 的表都已啟用 FORCE RLS' \
	            ELSE '警告：以下表未強制 RLS → ' || string_agg(relname, ', ') END \
	  FROM (SELECT c.relname, c.relrowsecurity, c.relforcerowsecurity \
	          FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
	          JOIN pg_attribute a ON a.attrelid = c.oid \
	         WHERE n.nspname='fms' AND c.relkind IN ('r','p') \
	           AND a.attname='tenant_id' AND NOT c.relispartition) t \
	 WHERE NOT (relrowsecurity AND relforcerowsecurity);"

explain-plan: ## 列出最慢的 10 支查詢（需先跑過壓測）
	@$(PSQL_OWNER) -c "\
	SELECT round(mean_exec_time::numeric,1) AS avg_ms, calls, \
	       left(query, 110) AS query \
	  FROM pg_stat_statements \
	 WHERE query NOT LIKE '%pg_stat_statements%' \
	 ORDER BY mean_exec_time DESC LIMIT 10;"

# 備份與還原都用**超級使用者**，不是 fms_owner。
#
# 這是 backup-restore-drill 找出來的：以 fms_owner 跑 pg_dump 會失敗 ——
#
#   ERROR: query would be affected by row-level security policy for table "alarm_rules"
#
# 007 的 FORCE ROW LEVEL SECURITY **對擁有者也生效**，而 pg_dump 沒有租戶
# 情境，所以政策把每一列都濾掉。pg_dump 拒絕產出一份被過濾過的備份（報錯而
# 不是靜默備份空的 —— 那是對的行為，否則你會有一份看起來成功的空備份）。
#
# 也就是說**這兩個指令從第一天就沒有真的可用過**，而沒有人發現，因為沒有人
# 還原過。演練現在會攔住這件事。
#
# 還原同理：INSERT 會被政策的 WITH CHECK 擋下。
#
# **修法分兩邊，而那個不對稱是刻意的：**
#
#   backup   → `fms_backup`（LOGIN BYPASSRLS，唯讀）。備份是排程執行、
#              無人值守的，憑證放在備份系統裡；它需要讀到所有列，
#              但不需要建資料庫、改角色或寫入任何東西。
#              見 postgres/initdb/02-backup-role.sh。
#   restore  → **DBA 身分（超級使用者）**。還原會覆蓋整個資料庫，是有人在場
#              的動作。給備份那個常駐憑證寫入權，等於讓它有能力覆蓋一切。
#
# BYPASSRLS 而不是把它加進 fms_platform：後者是**應用語意層**的繞過鑰匙
# （013 的平台情境），而備份只需要**儲存層**讀得到所有列。
backup: ## 匯出資料庫至 backups/fms-<timestamp>.dump（以 fms_backup，最小權限）
	@mkdir -p backups
	$(COMPOSE) exec -e PGPASSWORD=$$FMS_BACKUP_PASSWORD postgres \
	  pg_dump -U fms_backup -d fms -Fc > backups/fms-$$(date +%Y%m%d-%H%M%S).dump
	@ls -lh backups | tail -1

restore: ## 從 DUMP=<path> 還原（會覆寫現有資料）
	@test -n "$(DUMP)" || (echo "用法： make restore DUMP=backups/fms-xxx.dump" && exit 1)
	$(COMPOSE) exec -T -e PGPASSWORD=$$POSTGRES_SUPERUSER_PASSWORD postgres \
	  pg_restore -U $${POSTGRES_SUPERUSER:-postgres} -d fms --clean --if-exists < $(DUMP)
