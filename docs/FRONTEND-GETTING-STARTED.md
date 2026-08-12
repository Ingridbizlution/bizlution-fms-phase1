# 前端上手指南

> 這份文件的目標是**第一天就能發出第一個成功的請求**，以及知道哪幾件事今天
> 做不到（而不是花半天以為是自己設定錯了）。
>
> 契約的權威是 `api/openapi.yaml`，服務位址 `/api/v1/openapi.yaml`。
> 互動式瀏覽器在 **`/docs`**，可以直接在裡面試打。這份文件不重複契約的內容 ——
> 它寫的是契約讀不出來的東西。

---

## 0. 三件先知道的事

| 事實 | 影響 |
|---|---|
| **SSO 登入無法完成** | `/auth/sso/{code}/authorize` 可用（會回授權網址），但 `/callback` 回 **501** —— 缺密鑰管理服務的解析器。**登入請用本地帳號的 password grant**，功能完整 |
| **有些畫面沒有後端** | visitor／announcement／quota／signage／integration／amenity／desk assignment 在資料庫裡有表，但**不在 Phase 1 契約**。若你的畫面清單來自「功能對照矩陣」，這一段要先跟我們對齊 |
| **`services[].work_order` 恆為 null** | 預約的附加服務不會產生工單（fan-out worker 未實作）。契約把它標為可空，因此這是符合契約的狀態 |

---

## 1. 起環境

```bash
cd docker
cp .env.example .env
make up                                        # postgres / redis / minio / mailpit
docker compose run --rm -e MIGRATE_MODE=demo migrate
```

**必須用 `MIGRATE_MODE=demo`，不是 `make seed`。** `make seed` 只建組織骨架 ——
七個帳號的密碼是空的，工單與預約是 0 筆。`demo` 會多跑 migration 075，
補上密碼與示範活動資料。

然後起 API server（**compose 不跑它**）：

```bash
cd app
APP_DATABASE_URL=postgres://fms_app:change_me_app@localhost:5433/fms \
JWT_SECRET=please-change-me-at-least-32-characters \
S3_ENDPOINT=http://localhost:9000 \
S3_ACCESS_KEY=fmsminio \
S3_SECRET_KEY=change_me_minio \
CORS_ALLOWED_ORIGINS=http://localhost:3000 \
cargo run -p fms-server
```

### Base URL 與版本

本機開發是 `http://localhost:8080/api/v1`。目前沒有已發布的 staging／
production URL —— 有的話會補在這裡。

版本前綴固定是 `/api/v1`，目前只有這一個版本並存，**還沒有制定版本淘汰
或並存策略**——這裡誠實說「還沒有」，等真的要推第二個版本再補這段，
不先預留一個目前不存在的政策。

### `CORS_ALLOWED_ORIGINS` 沒設的話，你一個請求都發不出去

未設定時伺服器**完全不加 CORS 層**，於是你的 dev server 對 API 的每一個請求
都會在 preflight 失敗。啟動日誌會記一筆 warn 說這件事。

逗號分隔多個來源。**不支援 `*`** —— 這個 API 用 `Authorization` 標頭，而
「通配來源 + 帶憑證」在 CORS 規範裡是無效組合（瀏覽器會拒絕那個回應）；
設了 `*` 會被忽略並記一筆 error。

---

## 2. 拿 token

```bash
curl -X POST http://localhost:8080/api/v1/auth/token \
  -H 'Content-Type: application/json' \
  -d '{"grant_type":"password","tenant_code":"DEMO_GROUP",
       "username":"admin.chen@demo.bizlution.com","password":"Demo1234!"}'
```

回應是 `{ access_token, refresh_token, expires_in, token_type, tenant_id,
user_id, must_change_password }`。**`expires_in` 目前是 900 秒**（契約的
example 寫 3600，別照那個算 refresh 時機）。

### `username` 這個欄位收 email，也收 username

契約的欄位名是 `username`，但它的語意是**識別碼**：`users.email` 或
`users.username` 皆可，兩者都不分大小寫。登入畫面收 email 就直接放進這個
欄位，不必先查出 username。

email 不是必填欄位（`users.email` 可為 NULL —— 示範租戶的 `clean.vendor01`
與四個服務帳號就是），那些帳號仍然只能以 username 登入。撞名時 email 優先，
理由見 `openapi.yaml` 的欄位說明。

### 示範帳號

密碼全部是 **`Demo1234!`**，租戶代碼 `DEMO_GROUP`，email 一律是
`<username>@demo.bizlution.com`。

| 帳號 | 角色 | 適合驗什麼 |
|---|---|---|
| `admin.chen` | TENANT_ADMIN | 什麼都看得到。**開發時預設用這個** |
| `fm.lin` | FACILITY_ADMIN（只在台北總部） | 場域級隔離：他看不到影城的資料 |
| `tech.liu` / `tech.wang` | TECHNICIAN | 執行者視角：能執行工單，不能派工 |
| `user.huang` | REQUESTER | 最小權限：只能報修與看自己的東西。**驗 403 的畫面用他** |

> **用 `user.huang` 驗一次權限不足的畫面。** 這個 API 對權限不足回 403 +
> problem+json，而那個畫面很容易被忘記做，直到真實使用者遇到。

### 換租戶

`X-Tenant-ID` 是**每一個需認證請求的必填標頭**，而且伺服器會與 token 裡的
`tid` 交叉比對 —— 不符回 403（不是靜默忽略）。示範租戶是
`aaaaaaaa-0000-4000-8000-000000000001`。

---

## 3. 每個請求的標頭

```
Authorization: Bearer <access_token>
X-Tenant-ID:   aaaaaaaa-0000-4000-8000-000000000001
X-Request-ID:  <uuid>          # 選填但強烈建議，見下
```

**`X-Request-ID` 請每個請求都帶一個 uuid。** 它會出現在錯誤回應、伺服器日誌
與稽核軌裡 —— 回報問題時附上它，我們能直接定位到那一次請求。不帶的話伺服器
自己產生一個，但你手上就沒有那個值。

回應的 `X-Request-ID` 有被 CORS expose，因此 JS 讀得到。

---

## 4. 四個會影響你怎麼寫程式的機制

### 4.1 分頁是游標，不是頁碼

```
GET /api/v1/work-orders?limit=50
→ { "data": [...], "page": { "next_cursor": "...", "has_more": true } }
GET /api/v1/work-orders?limit=50&cursor=<next_cursor>
```

`limit` 上限 200、預設 50。**沒有 `total`** —— 游標分頁不提供總數（那需要一次
額外的全表計數）。UI 請用「載入更多」而不是頁碼。

例外：SCIM 的 `/scim/v2/*` 用 `startIndex`／`count` 並提供 `totalResults`，
因為 RFC 7644 這樣規定。那組端點你大概不會碰。

### 4.2 更新要帶 `If-Match`（樂觀鎖）

```
GET   /api/v1/reservations/{id}        → ETag: "3"，body 的 version 也是 3
PATCH /api/v1/reservations/{id}        If-Match: "3"
```

**不帶 `If-Match` 是 428**（`PRECONDITION_REQUIRED`），版本不符是
**412**（`STALE_VERSION`）。這是刻意的：兩個人同時改同一筆時，後寫的那個
必須知道自己覆蓋了什麼。

`ETag` 容忍帶引號（`"3"`）與裸值（`3`）兩種寫法。

`ETag` 已經 CORS expose，因此 JS 讀得到。你也可以直接用 body 裡的 `version`。

412 的處理建議：重新 GET、把差異呈現給使用者、讓他決定，**不要自動重試** ——
自動重試就是「後寫的贏」，那正是樂觀鎖要防的。

### 4.3 建立要帶 `Idempotency-Key`

適用於以下 13 支端點（全部是有副作用的 `POST`；`openapi.yaml` 裡
`parameters` 帶 `IdempotencyKey` 的操作即是權威清單，改版時請以那裡為準，
不要沿用這份清單）：

- `POST /identity-providers`
- `POST /organizations`
- `POST /facilities`
- `POST /facilities/{facilityId}/spatial-nodes`
- `POST /assets`
- `POST /assets/{assetId}/meters/{meterCode}/readings`
- `POST /maintenance-plans`
- `POST /work-orders`
- `POST /work-orders/{workOrderId}/transitions`
- `POST /reservations/holds`
- `POST /reservations`
- `POST /telemetry:batch-ingest`
- `POST /alarms/{alarmId}/work-order`

帶同一個鍵重送會拿到**第一次的回應**，不會建立第二筆／不會重複執行第二次
狀態轉換。**沒帶鍵的端點，網路超時重試就是真的重試** —— 例如
`DELETE /reservations/{id}`（取消）本身是可重複執行的軟操作，因此不需要它；
但上面列的每一支都是「重送＝可能產生第二個真實副作用」，帶鍵是必須，不是選用。

```
POST /api/v1/reservations
Idempotency-Key: <uuid，由前端產生並在重試時沿用>
```

鍵的有效期 24 小時，而且**綁使用者** —— 別人用同一個字串不會撞到你。

網路超時的正確處理是「用同一個鍵重送」，而不是「先查有沒有建立成功」。

### 4.4 錯誤是 RFC 7807

```json
{
  "type": "https://api.fms.bizlution.com/problems/validation-error",
  "title": "Validation error",
  "status": 422,
  "code": "VALIDATION_ERROR",
  "detail": "end_at must be after start_at",
  "request_id": "...",
  "errors": [{ "pointer": "/end_at", "code": "…", "message": "…" }]
}
```

`type` 的形式固定是 `<base>/problems/<kebab-case 的 code>`，因此它不帶額外資訊 ——
**請對 `code` 分支**。`detail` 是給人看的訊息，會隨版本調整；`code` 是穩定的
機器可讀值。

`errors[]` 有值時是欄位級錯誤，`pointer` 是 JSON Pointer，可以直接對應到表單欄位。

### 完整的 `code` 清單（取自 `fms-shared/src/problem.rs`）

| code | 狀態 | 意思 |
|---|---|---|
| `UNAUTHENTICATED` | 401 | token 無效或過期 → 走 refresh，失敗就回登入頁 |
| `PERMISSION_DENIED` | 403 | 權限不足。**這個畫面要做** |
| `TENANT_MISMATCH` | 403 | `X-Tenant-ID` 與 token 的 `tid` 不符 |
| `NOT_FOUND` | 404 | 也可能是「不是你的」（刻意不區分，避免變成存在性探測） |
| `CONFLICT` | 409 | 一般性衝突（重複、唯一約束） |
| `RESERVATION_CONFLICT` | 409 | 時段被佔用。**與 `CONFLICT` 分開**，因為 UI 要顯示「哪個時段」 |
| `WORK_ORDER_ILLEGAL_TRANSITION` | 409 | 狀態機不允許這個動作。按鈕該先變灰，這是後盾 |
| `IDEMPOTENCY_IN_PROGRESS` | 409 | 同一個鍵的前一次請求還在處理 → 稍後用**同一個鍵**重試 |
| `IDEMPOTENCY_KEY_REUSED` | 409 | 同一個鍵用在不同的請求內容上 |
| `STALE_VERSION` | **412** | `If-Match` 的版本已過期。**不是 409** |
| `VALIDATION_ERROR` | 422 | 欄位級錯誤，看 `errors[]` |
| `QUOTA_EXCEEDED` | 422 | 超出配額 |
| `PRECONDITION_REQUIRED` | 428 | 忘了帶 `If-Match` |
| `TOO_MANY_REQUESTS` | 429 | 有 `Retry-After` 標頭（秒） |
| `BAD_REQUEST` | 400 | 請求格式問題（缺標頭、非法 uuid） |
| `INTERNAL_ERROR` | 500 | 我們壞了 → 附 `X-Request-ID` 回報 |
| `NOT_IMPLEMENTED` | 501 | 這件事在**這個部署**裡做不到，`detail` 會說前提是什麼 |

> **`STALE_VERSION` 是 412，不是 409。** 這一條特別容易寫錯 —— 樂觀鎖的衝突
> 處理如果掛在 409 上，版本衝突會掉進「一般性衝突」的分支而顯示錯誤的訊息。

> **`TOO_MANY_REQUESTS` 目前只會出現在 `POST /auth/token`。** 這是登入失敗
> 節流（依 `tenant_code`＋`username`），不是全 API 的流量限制 —— 其他端點
> 目前沒有任何 rate limit。不要因為表裡列了 429 就在每一支 API 呼叫都做
> 429 重試邏輯；只有登入頁需要處理它。

---

## 5. 示範資料裡有什麼

`MIGRATE_MODE=demo` 之後（migration 075）：

| 資源 | 量 | 刻意放進去的邊界 |
|---|---|---|
| 工單 | 32 | **16 種狀態全部至少一筆**；有逾期未回應（`sla_state=RESPONSE_BREACHED` 且 `first_responded_at` 為 null）；有沒有受理人的（待派工）；有沒有描述的（驗 null 處理） |
| 預約 | 63 | 前後兩週；**兩個場館各有一筆正在進行**（佔用地圖因此不是空的）；一筆私人；一個三次的週期系列；PENDING_APPROVAL 與 NO_SHOW 各有樣本 |
| 資產 | 20 | 五種狀態；三筆保固 60 天內到期；三筆健康分數偏低 |
| 告警 | 6 | 兩筆 ACTIVE + CRITICAL；一筆已開工單（驗「告警 → 工單」的跳轉） |

**時間是相對今天計算的**，因此重跑 075 會把資料「移到今天」。不要把列數或
時間寫進前端的測試斷言。

### 私人預約值得特別試一次

示範資料裡有一筆 `is_private: true` 的預約。用 `admin.chen` 看得到標題，
用一個沒有 `reservation:view_private` 的帳號看，`title`／`purpose`／`organizer`
全部是 `null` 而 `is_private` 仍然是 `true`。

UI 的處理：`is_private && title == null` → 顯示「已預約」；
`is_private && title != null` → 顯示標題加一個鎖的圖示。

佔用地圖（牆面板）同樣遮罩。

---

## 6. 值得先做的幾支端點

| 端點 | 為什麼先做 |
|---|---|
| `GET /reports/facility-dashboard?facility_id=…` | **一個請求回傳首頁需要的全部彙總**（告警、資產健康、PM 合規、工單、預約）。不要用五個請求自己拼 |
| `GET /facilities/{id}/occupancy` | 牆面板／樓層圖的即時狀態。回的是**此刻**，不是未來可訂時段 |
| `GET /facilities/{id}/availability?from=&to=` | 未來可訂時段。**已經扣掉封鎖時段與既有預約**，不需要自己算 |
| `GET /facilities/{id}/floor-view` | 樓層圖儀表板專用：**一個請求**同時回幾何、告警、即時佔用、設備連線，見第 7 節 |

---

## 7. BIM 匯入與樓層圖儀表板

### 匯入流程是非同步的

```
POST /uploads/presign                      → 拿預簽網址，直傳檔案到儲存體（不經過 API）
POST /facilities/{id}/bim-models           → 用 storage_key 註冊模型，回 202，status=UPLOADED
（輪詢）GET /bim-models/{id}                → 等 status 變成 PARSED 或 PARSE_FAILED
GET /facilities/{id}/floor-view            → PARSED 之後，樓層/空間/幾何才看得到
```

`202` 只代表「已登記排隊」，**不代表解析完成**。解析由獨立服務
`bim-worker` 每 30 秒輪詢一次處理，**沒有推播通道** —— 前端要自己輪詢
`GET /bim-models/{id}` 直到 `status` 變成終態。失敗時看 `parse_report`
說明原因。目前只有 `source_format: IFC` 有解析器，其他格式一律
`PARSE_FAILED`。

### 樓層圖儀表板用一支端點：`GET /facilities/{id}/floor-view`

每個節點已經帶好幾何、告警、即時佔用、設備連線，**不需要**拼接
`GET /occupancy` 或 `GET /devices` 的結果。

* **`geometry`** 只有一種形狀：`{"type":"bbox","min":[x,y],"max":[x,y]}`，
  單位是公尺，座標是這個 BIM 模型自己的世界座標系（同場域內的節點共用
  同一個原點，可以直接拿來換算相對位置畫圖）。`{}` 代表沒有人匯入過
  幾何 —— 模型還在排隊解析，或這個節點本來就是手動建立。
* **沒有推播通道**。這支端點回的是查詢當下的快照，樓層圖要自己輪詢
  （輪詢間隔自行拿捏，沒有伺服器端建議值）。
* `worst_alarm_severity` 是**字典序**不是嚴重度序 —— 排序或做顏色漸層
  請用 `worst_alarm_rank`。
* `occupancy_state` 為 `null` 代表這個節點不是可預約資源，不是
  「目前沒有資料」。
* **`device_count - devices_offline_count` 不是在線設備數**——
  `device_count` 也含 `NEVER_SEEN`／`DISABLED`／`MAINTENANCE` 狀態的
  設備，那些既不算離線也不算在線。要拿真正的在線數，另外呼叫
  `GET /devices` 用 `connectivity` 欄位過濾。

---

## 8. 已知的粗糙處

* **`/docs` 不需認證**（刻意的，理由在 `fms-server/src/docs.rs` 檔頭）。
  生產部署請在反向代理層擋掉。
* **通知只進資料庫與 Mailpit**，不會真的寄出。Mailpit UI 在 `:8025`。
* **附件走預簽網址**：`POST /…/attachments` 回一個 MinIO 的網址，
  上傳是前端直接 PUT 到那個網址，不經過 API。
* **`otel-collector` 目前起不來**（bind-mount 的 config 有問題）。不影響 API。
* **如果你為了追一個 500 而去跑後端的 `cargo test`**：那批整合測試預設的併發
  會把 PostgreSQL 的連線吃光（每個測試各建一個資料庫），而耗盡的症狀是
  **登入回 500 `INTERNAL_ERROR`** —— 看起來正是 auth 壞了。停掉
  `fms-server`、加 `--test-threads=2`、跑完清掉殘留的 `fms_test_*`；
  完整說明在 `docker/README.md` 的「跑 Rust 整合測試」一節。

---

## 9. 回報問題時

附上 `X-Request-ID`、完整的 problem+json、以及請求的 method + path。
有那三樣我們能直接定位；只說「工單列表壞了」要來回三次。
