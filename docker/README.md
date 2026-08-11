# Facility Management System — 本機開發環境（Docker Compose）

自建 PostgreSQL 取代託管服務。此環境同時用於**開發、CI 與 UAT 前驗證**，
目標是讓「在我機器上會過」與「在 pipeline 上會過」是同一件事。

## 快速開始

```bash
cd docker
cp .env.example .env          # 依需要調整密碼與埠號
make hooks                    # 啟用 git hooks（新 clone 做一次）
make up                       # 啟動 postgres / redis / minio / mailpit
make migrate                  # 套用 sql/001–008、011、013
make seed                     # 追加示範租戶（009）
make test                     # 執行煙霧測試 010（T1–T4）+ 012（T5–T10）
```

一次到底：`make fresh`（等同 reset → migrate → seed）。
`make help` 列出全部指令。

**前端開發要用 `MIGRATE_MODE=demo`**，不是 `make seed`：

```bash
docker compose run --rm -e MIGRATE_MODE=demo migrate
```

差別是 migration 075 的**示範活動資料**：32 張工單（16 種狀態全部有樣本）、
63 筆預約（兩個場館各有一筆正在進行）、20 台資產、6 筆告警，
**以及所有示範帳號的密碼**。

沒有 075 的話：`make seed` 的七個帳號 `password_hash` 全是 NULL，
於是 **`POST /auth/token` 對每一個帳號都失敗** —— 而那是前端做的第一件事。
（測試不受影響：`common/mod.rs` 自己設密碼，所以整個測試套件跑在一份
「有密碼」的資料上，而示範環境跑在一份「沒有密碼」的資料上。）

`all` 與 `demo` 的差別、以及為什麼測試模板刻意用 `all`，見
`docker/scripts/migrate.sh` 裡 `DEMO` 那一段。

### 為什麼 `make hooks` 要做一次

它把 `core.hooksPath` 指到進版控的 `.githooks/`，而那裡的 `pre-push`
**拒絕直接推送 main**。

理由是一件實際發生過的事：**`make test` 在 main 上壞了三個 commit，沒有人發現。**
CI 有跑、有紅，但 commit 已經在 main 上了，紅燈只是一個沒有人點開的叉。

`core.hooksPath` 是本地設定，**不會隨 clone 過來** —— 所以這一步必須做，
否則這個 hook 就是「宣告了但沒人讀」的另一個版本。

這只是替代方案：真正的防線是 GitHub 的 branch protection／ruleset（伺服器端，
繞不過），但這個 repo 是私有的，那個功能需要 GitHub Pro 或把 repo 轉公開。
hook 用 `git push --no-verify` 就繞過了 —— 它的作用是把「不小心忘了」
變成「刻意覆寫」。

## 服務與埠號

| 服務 | 用途 | 預設埠 |
|---|---|---|
| PostgreSQL 16 | 主資料庫 | **5433**（刻意避開 5432） |
| Redis 7 | 權限判定快取、租戶限流計數 | 6380 |
| MinIO | S3 相容儲存體（附件、BIM、匯出） | 9000 / 控制台 9001 |
| Mailpit | 攔截所有外寄信件，驗證通知範本 | SMTP 1025 / UI 8025 |
| Adminer | 資料庫瀏覽（`make tools`） | 8080 |
| Mosquitto | MQTT broker（`make iot`） | 1883 / WS 9002 |

MinIO 啟動時自動建立三個私有 bucket：`fms`（附件）、`fms-bim`（模型）、`fms-exports`（報表）。
附件一律透過預簽網址存取，bucket 不開放匿名讀取。

## API server 的環境變數

**compose 不跑 API server**，只跑基礎設施。server 自己起：

```bash
cd app && cargo run -p fms-server
```

**五個變數是必填的**，缺任何一個服務起不來（含 `S3_*` —— 物件儲存在啟動時就
檢查，不是延後到第一次上傳）。

| 變數 | 必填 | 說明 |
|---|---|---|
| `APP_DATABASE_URL` | ✔ | `fms_app` 的連線字串（不是 `fms_owner`，見下方角色一節） |
| `JWT_SECRET` | ✔ | 至少 32 字元，過短直接啟動失敗 |
| `S3_ENDPOINT` | ✔ | 例如 `http://localhost:9000`（compose 的 MinIO）。**沒設會啟動失敗**，不是延後到第一次上傳才失敗 |
| `S3_ACCESS_KEY` / `S3_SECRET_KEY` | ✔ | 同 compose 的 `MINIO_ROOT_USER` / `MINIO_ROOT_PASSWORD` |
| `BIND_ADDR` | | 預設 `0.0.0.0:8080` |
| `CORS_ALLOWED_ORIGINS` | **前端開發必填** | 見下 |
| `PUBLIC_BASE_URL` | SSO 必填 | `/auth/sso/*` 的 `redirect_uri` 由它組出；未設定時那兩支回 501 |
| `DB_MAX_CONNECTIONS` | | 預設見 `config.rs` |
| `OUTBOUND_ALLOW_PRIVATE_TARGETS` | | SSRF 閘門的例外清單。**生產環境不該有這個設定**，設了會記 warn |
| `IDP_SECRET_*` | SSO／LDAP 必填 | IdP 的出站密鑰，一個參照一個變數。見下 |

### `IDP_SECRET_*` —— IdP 密鑰，**輪替要重啟**

`identity_providers` 的 `client_secret_ref`、`ldap_bind_secret_ref`、
`metadata_xml_ref` 存的是**參照名稱**，不是密鑰本身。實際的值由環境變數提供
（ADR-13 決策 A）。名稱轉換規則：

```
參照 okta/prod/client-secret  →  環境變數 IDP_SECRET_OKTA_PROD_CLIENT_SECRET
```

非英數字一律換成 `_`、全部大寫、加上 `IDP_SECRET_` 前綴。**不必自己推導** ——
`POST /identity-providers/{id}/test-connection` 的 `secret_reference_resolvable`
那一格在解不開時會直接說出要設哪個環境變數。

三件要知道的事：

* **輪替密鑰要重啟服務。** 解析器讀的是行程的環境變數，沒有熱重載。
  這是 ADR-13 決策 A 明確接受的代價（Phase 1 的 provider 數量是個位數）。
  客戶若要求接自己的 Vault／Key Vault，換掉 `SecretResolver` 的實作即可，
  呼叫端不動。
* **設成空字串等於沒設。** `IDP_SECRET_X=` 會被當成未提供 —— 因為一個空密鑰
  送出去只會換到 IdP 那邊的 401，症狀離原因很遠。
* **密鑰在容器的環境裡**（`docker inspect` 看得到）。這也是決策 A 記錄的代價。

密鑰**不會**進 `Settings`：解析發生在用的那一刻，因此不會躺在任何被 clone
進 handler、而且實作了 `Debug` 的結構裡。見 `fms-shared/src/secrets.rs`。

### `CORS_ALLOWED_ORIGINS` —— 沒設的話瀏覽器一個請求都發不出去

逗號分隔的來源清單：

```bash
CORS_ALLOWED_ORIGINS=http://localhost:3000,http://localhost:5173
```

**未設定 = 不加 CORS 層**，而那對純伺服器對伺服器的部署是正確的預設。
但它同時意味著前端 dev server 的**每一個**跨來源請求都會在 preflight 就失敗 ——
而伺服器對伺服器的呼叫完全不受影響，所以測試全綠、`curl` 全通，
只有瀏覽器客戶端一行也跑不動。啟動日誌會在未設定時記一筆 warn。

**不支援 `*`。** 這個 API 用 `Authorization` 標頭，而「通配來源 + 帶憑證」在
CORS 規範裡是無效組合（瀏覽器會拒絕那個回應）。設了 `*` 會被忽略並記一筆
error，因為那個組合的症狀是「明明設了卻還是被擋」，很難查。

允許的標頭與 expose 的標頭見 `fms-server/src/lib.rs` 的 `cors_layer`；
行為驗證在 `fms-server/tests/cors_slice.rs`。

## 三個資料庫角色 — 這是隔離能否成立的關鍵

| 角色 | 用途 | 屬 `fms_platform` | RLS 是否生效 |
|---|---|---|---|
| `fms_owner` | 擁有 schema 與物件、執行 migration、支援排查 | 是 | 生效（FORCE RLS 對擁有者亦適用），但可切換平台情境 |
| `fms_app` | **API 服務連線** | 否 | 完整生效，且無法取得平台情境 |
| `fms_readonly` | 報表與稽核唯讀 | 否 | 完整生效 |
| `postgres` | 僅容器初始化 | — | **繞過（BYPASSRLS）— 絕不可用於應用或測試** |

```
API 連線字串（正確）
  postgresql+asyncpg://fms_app:<pw>@localhost:5433/fms

Migration 連線字串
  postgresql://fms_owner:<pw>@localhost:5433/fms
```

用 `postgres` 或 `fms_owner` 跑 API，多租戶隔離就形同不存在。
`make check-rls` 可驗證所有含 `tenant_id` 的表都已啟用並強制 RLS。

## 為什麼測試要用 `fms_owner` 而不是 `fms_app`

010 與 012 需要在前置階段建立跨租戶測試資料（需平台情境），
同時 T1 又必須驗證 RLS 真的生效。`FORCE ROW LEVEL SECURITY` 讓政策對擁有者
也適用，因此 `fms_owner` 同時滿足兩個條件：

- 前置資料建立：`SET app.is_platform='on'` → 因屬 `fms_platform` 而被接受
- T1 隔離斷言：平台情境關閉後，政策照樣過濾 → 斷言有意義

若以超級使用者執行，`smoke-test.sh` 會直接拒絕並提示，避免測試「靜默通過」。

## 平台情境的安全性（migration 013）

001 原本的 `fms.is_platform_context()` 只檢查 session 變數，任何連線都能
`SET LOCAL app.is_platform = 'on'` 關閉整套 RLS——一次 SQL injection 就足以
讀取所有租戶資料。013 將判定改為雙條件：

```sql
current_setting('app.is_platform') = 'on'
  AND pg_has_role(current_user, 'fms_platform', 'USAGE')
```

`fms_app` 不是 `fms_platform` 的成員，因此即使變數被設上也無效。
`fms.set_context()` 另外會在無權限時直接拋錯（`PLATFORM_CONTEXT_DENIED`），
而不是默默忽略——默默忽略會讓維運寫出「看似成功卻查不到資料」的腳本。

**013 必須最後執行**，且執行者需具 `CREATEROLE`（`fms_owner` 已具備）。

## Migration 順序

`migrate.sh` 固定以下順序，不可調換：

```
001 → 002 → 003 → 004 → 005 → 006 → 007 → 008 → 011 → 013   （+ 009 為選用示範資料）
```

- `005` 才補上 `work_orders → reservations` 外鍵；`006` 才補 `→ alarms`（循環依賴刻意分兩步）
- `008` 必須在任何工單資料寫入前執行（`work_orders.status` 有外鍵指向狀態字典）
- `011` 依賴 `005`（預約）與 `006`（裝置）
- `013` 改寫平台情境判定，須在種子資料之後

## 與正式環境的差異

| 項目 | 本機／dev | staging／production |
|---|---|---|
| PostgreSQL | 容器，單一實例 | 專用主機或 VM；1 主 + 1 讀複本 |
| 資料保存 | 具名 volume，可隨時 `make reset` | 獨立磁碟 + WAL 歸檔 + PITR |
| 密碼 | `.env`（開發用） | 密鑰管理服務注入並輪替 |
| 匿名 MQTT | 允許 | 禁止；IoT 閘道須用 `api_clients` 憑證與 CIDR 限制 |
| Adminer | 可用 | **不部署** |
| `log_min_duration_statement` | 300ms | 1000ms（或關閉，改用 `pg_stat_statements`） |
| 備份 | `make backup`（隨手備份） | 每日全量 + WAL 連續歸檔，每季還原演練 |

調校參數（`shared_buffers=512MB`、`work_mem=16MB`）是為了讓 8–16GB 的開發機
順利跑完效能測試的合成資料量；正式環境需依實際記憶體重新計算。

## 正式環境部署

跟本機開發最大的差別：**`docker-compose.prod.yml` 連 `fms-server`／
`fms-jobs` 都跑進 Docker**（本機開發是裸進程，`cargo run` 手動啟動）。
VM 開機需求見 `docs/DEPLOYMENT-VM-REQUIREMENTS.md`。

**TLS／憑證不在這台 VM 上處理。** 目前的正式部署（`api.fms.bizlution.ai`）
前面有機房自己管的反向代理（另一台主機），對外的 443 在那一層終止 TLS，
轉純 HTTP 進這台 VM 的 80 埠。所以這裡沒有 Caddy 或 nginx，`fms-server`
直接映射到主機的 80 埠（`ports: ["80:8080"]`）。**如果換一個沒有這層
機房反向代理的環境部署**（VM 直接對外），要自己在這台 VM 上加一層
反向代理處理 TLS——舊版本用過 Caddy，拿掉的原因記在
`docker-compose.prod.yml` 的檔頭註解。

### 檔案總覽

| 檔案 | 用途 |
|---|---|
| `app/Dockerfile` | 多階段建置 `fms-server`／`fms-jobs` 兩個 target。**build context 必須是 repo 根目錄**，不是 `app/`（原因見該檔檔頭） |
| `docker/docker-compose.prod.yml` | 正式環境的服務清單——沒有 mailpit／adminer／otel-collector，Postgres／MinIO／Mosquitto 不對外開埠，`fms-server` 直接映射主機 80 |
| `docker/.env.prod.example` | 正式環境 `.env` 範本（真正的 `.env` 只存在 VM 上，不進版控） |

### 第一次上線

```bash
cd /path/to/repo
cp docker/.env.prod.example docker/.env    # 把每個 change_me_* 換成真的值
cd docker
docker compose -f docker-compose.prod.yml build
docker compose -f docker-compose.prod.yml --profile jobs run --rm migrate
docker compose -f docker-compose.prod.yml up -d postgres minio minio-init mosquitto fms-server
curl http://localhost/api/v1/health          # VM 本機驗證，應回 ok
curl https://<機房反向代理設定的網域>/api/v1/health   # 對外驗證，應回 ok
```

`migrate` 這裡**不帶** `MIGRATE_MODE`——預設是 schema-only（`001`–`008`、
`011`、`013` 之後全部 CORE migration），不會種示範租戶（`009`）。正式環境
不該有示範資料；真實客戶租戶的建立是另一件事（目前沒有自動化流程）。

**`up -d` 這裡刻意明確列出服務名稱，不帶 `fms-jobs`。** 本機用這份
compose 檔實測過：`fms-jobs` 需要 `PM_GENERATOR_USER_ID`／
`DIRECTORY_SYNC_USER_ID` 才能啟動（見 `.env.prod.example` 的說明），而
兩者都要指向真實租戶的服務帳號——第一次上線還沒有真實租戶，沒有合法的
值可填，所以這個服務也用 `profiles: ["with-jobs"]` 標成不會被不帶
profile 的 `up -d` 意外啟動。等第一個真實租戶與其服務帳號都建好、
`.env` 填上對應的 id 之後：

```bash
docker compose -f docker-compose.prod.yml --profile with-jobs up -d fms-jobs
```

### 之後怎麼更新

CI/CD 上線後，push 到 `main` 且既有的 CI job（`data-layer`／`app`）都綠，
會自動 SSH 進 VM 跑：

```bash
git pull
docker compose -f docker-compose.prod.yml build
docker compose -f docker-compose.prod.yml --profile jobs run --rm migrate
docker compose -f docker-compose.prod.yml up -d postgres minio minio-init mosquitto fms-server
# fms-jobs 已經在跑的話（--profile with-jobs 啟動過），這一行讓它也套用新版本：
docker compose -f docker-compose.prod.yml --profile with-jobs up -d fms-jobs 2>/dev/null || true
```

見 `.github/workflows/data-layer.yml` 的 `deploy` job。

### 備份

沿用既有的 `make backup`／`fms_backup` 角色（見本檔前面「備份與還原都用
超級使用者」那一段的完整說明），只是要指到 prod compose 檔：

```bash
docker compose -f docker-compose.prod.yml exec -e PGPASSWORD=$FMS_BACKUP_PASSWORD postgres \
  pg_dump -U fms_backup -d fms -Fc > backups/fms-$(date +%Y%m%d-%H%M%S).dump
```

排進 VM 的 cron，每日一次。**這是第一次上線先做的最小可行備份**——完整
的 WAL 連續歸檔＋PITR（見下方「與正式環境的差異」表格）是流量大了之後
的事，還沒做。

### 為什麼不對外開 Postgres／MinIO／Mosquitto 的埠

這三個服務只有同一台 VM 上的其他容器（`fms-server`／`fms-jobs`／
`minio-init`）需要存取，透過 compose 內部網路的 service name
（`postgres`、`minio`）就解析得到，不需要對外映射埠。對外開放等於把
「資料庫沒有應用層驗證就能直接連」這件事暴露到公網——密碼再強都是
不必要的攻擊面。唯一對外開放的是 `fms-server` 的 80（前面接的是機房的
反向代理，見本節開頭的說明）。

**MinIO 是例外，但只開 loopback，不是對外開埠**——見下一節。

### MinIO 預簽網址的反向代理

`fms-shared::Storage::presign_get`／`presign_put` 產生的網址是給**瀏覽器**
直接連的（附件下載、BIM 模型上傳），但簽章當中的 host 用的是
`S3_ENDPOINT=http://minio:9000`——這個容器名稱只有 compose 內部網路解析
得到，瀏覽器連不到。修法**不是**把 MinIO 對外開埠（那會繞過
`mc anonymous set none` 設的私有 bucket 政策），而是：

1. `docker-compose.prod.yml` 把 MinIO 綁在 `127.0.0.1:9000`（只有這台 VM
   自己能連，跟 `fms-server` 的 80 埠映射一樣的收斂範圍）。
2. `fms-server`／`fms-jobs` 多一個 `S3_PUBLIC_ENDPOINT` 環境變數（自動由
   既有的 `PUBLIC_BASE_URL` 組出，`.env` 不用手動加新變數），
   `Storage` 拿它把回給前端的網址前綴換成公開位址，簽章本身不受影響
   （簽的是 path／query，不是 host）。
3. **這台 VM 上跑的 nginx（見 `/etc/nginx/sites-available/default`，跟
   `/api/v1/` 那條 proxy_pass 同一份設定）要手動加一條 `/storage/` 的
   路由**——這步不在 CI/CD 的自動部署腳本裡，換一台新 VM 或重建 nginx
   設定時要記得補：

   ```nginx
   location /storage/ {
       proxy_pass http://127.0.0.1:9000/;
       proxy_set_header Host minio:9000;
   }
   ```

   **`proxy_set_header Host minio:9000` 是關鍵，不能省。** MinIO 驗證
   SigV4 簽章時用的是「實際收到的 `Host` 標頭」去重算簽章，跟簽的時候
   用的 host（`minio:9000`）比對——如果 nginx 把公開請求的 `Host`
   （例如 `demo.fms.bizlution.ai`）原樣轉過去，簽章一定驗不過，
   回應是 `SignatureDoesNotMatch`，不是連不上。

   改完 `sudo nginx -t && sudo systemctl reload nginx`。

## CI 用法

已實作於 `.github/workflows/data-layer.yml`。CI **直接呼叫本目錄的 make target**，
不另寫一套 psql 步驟：

```yaml
defaults: { run: { working-directory: docker } }
steps:
  - uses: actions/checkout@v5
  - run: cp .env.example .env
  - run: make up
  - run: make migrate          # 001–008、011、013
  - run: make seed             # 009 示範租戶
  - run: make check-rls        # 全表 FORCE RLS
  - run: make check-isolation  # 以 fms_app 驗證隔離
  - run: make test             # T1–T10
  - run: make test             # 再跑一次，證明測試不留狀態
```

之所以走 compose 而不是 service container + 手寫步驟：本檔開頭的目標是「讓
『在我機器上會過』與『在 pipeline 上會過』是同一件事」。若 CI 自己拼一套初始化
順序，兩邊就會各自漂移——而順序正是這批 migration 最脆弱的地方
（005／006 補外鍵、008 先於工單、013 必須最後）。

`make check-isolation`（`scripts/check-isolation.sh`）就是原本這裡以文字要求的那條
機械化保險，以 `fms_app` 身分斷言三件事：

| 案例 | 斷言 |
|---|---|
| A 完全未設 context | 任一租戶表回 **0 列**（防「新增資料存取路徑時忘記注入 context」） |
| B 自行設 `app.is_platform='on'` | 仍回 **0 列**，且 `fms.is_platform_context()` 為 **false**（013 硬化生效） |
| C 設定正確 tenant context | 回 **> 0 列**（反向確認 RLS 沒有過度阻擋；少了這案，把政策寫成永遠 false 也會讓 A、B 通過） |

`make verify` 可一次跑完 check-rls + check-isolation + test。

## 常見問題

**埠號衝突**：改 `.env` 的 `POSTGRES_PORT` 等變數即可，不需改 compose。

**改了 initdb 腳本卻沒生效**：初始化只在資料卷為空時執行一次，需 `make reset`。

**`make test` 說找不到示範租戶**：先跑 `make seed`。

**擴充安裝失敗**：`ltree`／`btree_gist`／`pgcrypto`／`pg_trgm`／`citext` 都是
PostgreSQL 13+ 的 trusted extension，具備資料庫 `CREATE` 權限的 `fms_owner`
即可安裝，不需要超級使用者。若仍失敗，確認映像為官方 `postgres:16-alpine`
（含 contrib 模組）而非精簡自建映像。

**時區**：容器與角色都設為 `Asia/Taipei`。所有 API 時間欄位仍一律使用
帶時區的 RFC 3339，資料庫存 `timestamptz`——時區設定只影響日誌與 `to_char` 輸出。
