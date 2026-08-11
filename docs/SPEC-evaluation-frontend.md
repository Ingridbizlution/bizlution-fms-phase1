# 需求規格：客戶採購前評估用的四畫面示範前端

> 這份規格是給**另一個 session 冷啟動**用的。假設讀者沒有本專案的對話脈絡，
> 因此除了「要做什麼」也寫了「這個 repo 會怎麼絆倒你」。
>
> 格式比照 `SPEC-api-explorer-and-e2e.md`。
>
> **執行順序：畫面 1 → 2 → 3 → 4。** 前三個各自獨立；畫面 4 依賴一件
> 目前不存在的東西（第二個租戶），所以放在最後，而且它有一個**必須先做的
> 前置決定**（見 §4.0）。

---

## 0. 這份規格與 `SPEC-api-explorer-and-e2e.md` 的關係

那一份的 §0 明確寫了「**為什麼不是直接做前端**」：契約還在收斂，
在動的契約上蓋前端等於要重蓋一次。

**那個理由現在不成立了，而且是量出來的：**

| 事實 | 當時（2026-08-02） | 現在（2026-08-04） |
|---|---|---|
| 已實作端點 | 84 | **契約 174 列全部 ✔✔** |
| 測試總數 | 240 | **528** |
| `openapi.yaml` 改動頻率 | 一天六次 | 契約完成後未再改形狀 |

同一份規格也寫了退出條件：

> 目的若變成「給利害關係人看／真實使用者 UAT」，那前端是對的，
> 而且該在契約穩定後照正規做，不是當測試工具。

**這份規格就是那個情況。** 目的是**採購前評估**：讓客戶的決策者看到系統
真的在動，而不是看 API 文件。

### 這不是產品前端

寫下來，因為它會影響每一個取捨：

* **四個畫面，不是一套完整 UI。** 沒有導航殼、沒有設定頁、沒有 i18n 框架。
* **選畫面的標準是「只有這個系統做得到」**，不是「客戶最常用」。
  工單列表誰都做得出來；**SLA 逾期狀態由資料庫的營業時間函式算出來**
  這件事，示範才有意義。
* **不追求好看，追求可信。** 一個顯示「達成率 87.3%」的畫面若沒有旁邊的
  「口徑：strict、分母 N=23、排除 2 筆」，決策者沒有理由信它。
  **每個數字都要能點開看到它的來源。**

---

## 1. 硬性約束（四個畫面都適用）

### 1.1 只能透過 HTTP API 取資料

不可以連資料庫、不可以在前端 repo 裡放 SQL。理由不只是分層：這四個畫面的
價值之一就是**證明 API 足夠支撐真實 UI**。若某個畫面需要繞過 API 才做得出來，
那是一個要回報的缺口，不是一個要繞過的障礙。

**發現缺口就記下來並回報，不要自己補。** 補在前端等於把缺口藏起來。

### 1.2 每個請求都要帶 `X-Request-ID`

一個 uuid。它會出現在錯誤回應、伺服器日誌與稽核軌裡。
回應的 `X-Request-ID` 有被 CORS expose，因此 JS 讀得到 ——
**在畫面上顯示它**（例如錯誤 toast 的角落）。示範時「這個錯誤的追蹤碼是 X」
比「發生錯誤」有說服力得多。

### 1.3 錯誤一律當 problem+json 處理，並顯示 `code`

不要只顯示 HTTP 狀態碼。這個 API 的錯誤有型別化的 `code`，而畫面要用它分流。
**錯誤碼一定要從 `app/crates/fms-shared/src/problem.rs` 逐條抄**，不要憑印象 ——
這件事已經在 `FRONTEND-GETTING-STARTED.md` 上出過一次錯（三個錯誤碼寫錯，
其中一個把 412 寫成 409）。已知會遇到的：

| code | HTTP | 什麼時候 |
|---|---|---|
| `VALIDATION_ERROR` | 422 | 欄位錯 |
| `PERMISSION_DENIED` | 403 | 權限不足 |
| `TENANT_MISMATCH` | 403 | `X-Tenant-ID` 與 token 的 `tid` 不符 |
| `STALE_VERSION` | **412** | 樂觀鎖：`If-Match` 的版本過期 |
| `PRECONDITION_REQUIRED` | 428 | 該給 `If-Match` 卻沒給 |
| `WORK_ORDER_ILLEGAL_TRANSITION` | 409 | 當前狀態不允許這個動作 |
| `RESERVATION_CONFLICT` | 409 | 時段重疊 |
| `TOO_MANY_REQUESTS` | 429 | 登入節流 |

### 1.4 不可以依賴外部 CDN

地端產品，客戶環境可能沒有外網。**示範當天沒有網路是會發生的事。**
所有資產（字型、圖示、JS 框架）都要 bundle 進去。

### 1.5 環境

```bash
cd docker && make up && MIGRATE_MODE=demo make migrate && make test-template
cd ../app && cargo run -p fms-server
```

`MIGRATE_MODE=demo` **是必須的**（不是選項）：它跑 `sql/075`，那是
示範帳號密碼與活動資料的來源。用預設的 `schema` 模式跑完之後
**每一個示範帳號的 `password_hash` 都是 NULL，登不進去** ——
這個坑已經踩過一次。

API server 的必填環境變數（缺任何一個起不來，含三個 `S3_*`）見
`docker/README.md`。**`CORS_ALLOWED_ORIGINS` 一定要設**，否則瀏覽器一個請求
都發不出去（preflight 就死）：

```bash
CORS_ALLOWED_ORIGINS=http://localhost:5173
```

不支援 `*` —— 這個 API 用 `Authorization` 標頭，而「通配來源 + 帶憑證」
在 CORS 規範裡是無效組合。設了會被忽略並記一筆 error。

### 1.6 示範帳號

密碼全部 `Demo1234!`，租戶代碼 `DEMO_GROUP`，
`X-Tenant-ID: aaaaaaaa-0000-4000-8000-000000000001`。

| 帳號 | 角色 | 這四個畫面裡用來示範什麼 |
|---|---|---|
| `admin.chen` | TENANT_ADMIN | 預設帳號，四個畫面都看得到 |
| `fm.lin` | FACILITY_ADMIN（只在台北總部） | **場域級隔離** —— 他看不到影城的資料 |
| `user.huang` | REQUESTER | **權限不足的畫面**與**私人預約的遮罩** |
| `tech.liu` | TECHNICIAN | 執行者視角：能執行工單，不能派工 |

---

## 畫面 1：工單看板 + SLA

### 1.1 要交付什麼

一個依狀態分欄的看板，加上每張卡片的 SLA 狀態，以及一個把達成率**拆解到
可查核**的側欄。

### 1.2 用哪些端點

| 用途 | 端點 |
|---|---|
| 卡片資料 | `GET /api/v1/work-orders`（`status`、`facility_id`、`sla_state`、`assignee_id`、`mine`、`cursor`、`limit`） |
| 單張詳情 | `GET /api/v1/work-orders/{id}` |
| **哪些按鈕該亮** | `GET /api/v1/work-orders/{id}/available-actions` |
| 執行動作 | `POST /api/v1/work-orders/{id}/transitions` |
| 達成率 | `GET /api/v1/reports/sla-compliance?from=&to=&group_by=&strictness=` |
| 首頁彙總 | `GET /api/v1/reports/facility-dashboard?facility_id=&period=` |

### 1.3 這個畫面真正要示範的三件事

**（a）按鈕該不該亮由伺服器決定，不是前端猜。**

`available-actions` 回的每個動作帶 `permitted`。**前端不可以自己用狀態機
推導哪些按鈕能按** —— 那會變成同一套規則的第二份實作，而兩份一定會漂移。

系統驅動的動作（如 `BREACH_SLA`）一律 `permitted=false`：列出來但按不動。
**畫面要把它們顯示成 disabled 而不是隱藏** —— 那是在示範「系統會自己做這件事」。

**（b）`transitions` 回 409 是正常流程，不是錯誤。**

`WORK_ORDER_ILLEGAL_TRANSITION` 代表資源存在、格式也對，是**當前狀態**
不允許。正確的處理是**重新拉 `available-actions` 並更新按鈕**，不是彈一個
紅色錯誤框。

> **示範腳本裡要刻意觸發一次。** 開兩個瀏覽器視窗，一邊完成工單，
> 另一邊按舊按鈕 → 看板自己刷新成正確狀態。那一幕證明的是**併發正確性**，
> 而那是這個系統少數難被競品做對的地方。

**（c）SLA 的數字要能拆解。**

`/reports/sla-compliance` 的 `strictness` 有 `strict` 與 `operational` 兩種
口徑，而 `facility-dashboard` 固定用 `strict`（`dashboard.rs` 的
`SLA_STRICTNESS`）。**兩個畫面顯示同一個場域時數字必須一致** ——
這件事後端已經有測試釘住，前端不要各自傳不同的 `strictness` 把它弄壞。

側欄要顯示：口徑、`from`/`to`、分母、被排除的筆數。
`group_by` 只接受 `facility` / `org` / `team` / `service_item` / `priority`
五個值（`dto.rs` 的 `GROUP_BY`）—— 傳別的會 422。

> **為什麼要擋在前端**：那五個值是 SQL 裡一個 `CASE` 的分支，而那個 `CASE`
> **沒有 ELSE**。未知值不報錯，只會讓 `group_key` 整欄變成 NULL ——
> 也就是「一個叫做『全部』的分組」。後端已經擋掉了；前端的下拉選單也不要
> 讓使用者有機會送出別的值。

### 1.4 已知的絆腳石

* **`status` 是逗號分隔多值**（契約如此定義），不是重複的 query 參數。
* 工單狀態有 **16 個**，示範資料每一個都有（`sql/075` 用 `(n-1) % 16` 分配）。
  16 欄的看板不能看 —— **用 `status_category` 分欄**，把 `status` 放在卡片上。
* 分頁是 cursor，不是 offset。**沒有總頁數**，不要做「第 N 頁」的頁碼列。
* 改工單要 `If-Match`（樂觀鎖）。少了會 **428**，版本過期會 **412**。
  `ETag` 有被 CORS expose，所以 JS 讀得到 —— 從 GET 的回應標頭拿。

---

## 畫面 2：預約行事曆 + 佔用地圖

### 2.1 要交付什麼

一個週檢視的資源行事曆，加上一個**牆面板模式**的即時佔用地圖
（大字、無互動、自動刷新）。

### 2.2 用哪些端點

| 用途 | 端點 |
|---|---|
| 可訂資源 | `GET /api/v1/facilities/{id}/bookable-resources` |
| 行事曆 | `GET /api/v1/reservations` |
| 可用時段 | `GET /api/v1/facilities/{id}/availability` |
| **佔用地圖** | `GET /api/v1/facilities/{id}/occupancy` |
| 佔位 | `POST /api/v1/reservations/holds` |
| 建立 | `POST /api/v1/reservations` |
| 報到／離場 | `POST /api/v1/reservations/{id}/check-in` `/check-out` |
| 停用時段 | `GET|POST /api/v1/resource-blackouts` |

### 2.3 這個畫面真正要示範的三件事

**（a）私人預約的遮罩 —— 用 `user.huang` 看一次。**

`OccupancyDto` 有 `is_private`。私人預約的 `title` 與 `organizer_name`
會被遮罩成 `null`，而 `state` 與時段照舊。**畫面要顯示成「已預約」加時段**，
不要顯示成空白或「未知」。

> 這是 `sql/011` 的欄位註解明文指定的行為，而它在 API 層曾經**完全沒有實作**
> —— 任何拿得到 `reservation:read` 的人都看得到標題與主辦人姓名，
> 牆面板也一起洩漏。示範時用 `admin.chen` 與 `user.huang` 對比同一個時段，
> 一眼看得出差別。

**（b）佔位（hold）是兩階段。**

`POST /reservations/holds` 拿到 `holdToken`，再帶著它建立。這是在示範
「兩個人同時點同一個時段不會雙重預訂」。`RESERVATION_CONFLICT`（409）
要顯示成「這個時段剛剛被訂走了」並自動刷新，不是一個技術性錯誤。

**（c）未到場自動釋放。**

`auto_release_at` 到了之後 worker 會把預約轉成 `NO_SHOW` 並釋放時段。
畫面上要看得到那個轉換 —— 這是「系統自己在管」的證據。

> 後端的行為驗證在 `journeys_slice.rs` 的鏈 A（PR #56），它的最後一步是
> **同一個時段可以重新預約成功**（「釋放了」唯一可觀察的定義）。

### 2.4 已知的絆腳石

* **預約不能建在過去。** `sql/011` 第 1036 行檢查
  `p_start_at < clock_timestamp() + min_notice_minutes` → 422 `TOO_LATE`，
  訊息會說「需提前 N 分鐘預約」（示範資料 N=0，所以是「不能訂過去」）。
  **示範腳本要用未來時段**，而測試裡的做法是建好之後用 SQL 平移 ——
  前端沒有那個手段。
* 佔用狀態有四個：`FREE` / `OCCUPIED`（已報到）/ `RESERVED`（已訂未報到）/
  `HELD`（佔位中）。**`RESERVED` 與 `OCCUPIED` 不同**，牆面板要分得出來
  （「已預約」與「使用中」是不同的資訊）。
* 示範資料保證**每一個場域**都有一個跨越現在的 `CHECKED_IN` 預約
  （`sql/075` 的自我檢查逐場域驗過）。
  若某個場域的佔用地圖全 `FREE`，那是 bug，不是資料不足。

  > 這一條原本寫成「至少有一個」，結果那一筆落在錯的場域，
  > 佔用端點全回 FREE 而 migration 的自我檢查照樣通過。

---

## 畫面 3：稽核軌

### 3.1 要交付什麼

一個可篩選的稽核軌檢視，重點是**串起一次請求的完整因果鏈**。

### 3.2 用哪些端點

| 用途 | 端點 |
|---|---|
| 列表 | `GET /api/v1/audit-log`（`entity_type`、`entity_id`、`actor_user_id`、`action`、`from`、`to`、`cursor`、`limit`） |
| 匯出 | `POST /api/v1/audit-log:export` |
| 取檔 | `GET /api/v1/audit-log/exports/{id}` |

### 3.3 這個畫面真正要示範的三件事

**（a）`request_id` 串得起一次操作的全部後果。**

每一列有 `request_id`。點一個 `request_id` → 顯示同一次請求造成的**所有**
變更。示範「改一個欄位，系統連帶動了什麼」時這是最有力的一頁 ——
稽核人員問的正是這個問題。

**（b）只有 `diff_keys`，沒有前後值。這是刻意的，要說出來。**

契約的 `AuditEntry` 只有 `diff_keys`（哪些欄位改了），沒有 before/after。
**畫面上要寫明這件事**，不要讓人以為是還沒做。

理由：稽核軌保留期長、而且是 append-only。存前後值等於把每一份敏感資料
複製一份到一個更難刪除的地方。

> 這不是空談。這個系統的稽核軌曾經存了 **71 個 argon2id 密碼雜湊** ——
> 可離線破解、append-only、長期保留。`sql/074` 加了一份紅字清單
> （`password_hash`、`token_hash`、`signing_secret`、`scim_token_hash`、
> `pkce_verifier`）並清掉了既有那 71 列。**遮罩在算出 `diff_keys` 之後才做**，
> 因此「哪些欄位改了」仍然看得到，值不會留下。

**（c）匯出是非同步的。**

`POST :export` 回一個 job，`GET .../exports/{id}` 輪詢。**畫面要顯示
job 狀態**，不要假裝是同步下載。這在示範大量資料匯出時是加分項。

### 3.4 已知的絆腳石

* `audit:read` 是 **TENANT 範圍**的權限。`fm.lin`（FACILITY_ADMIN）**看不到
  稽核軌**，會拿到 403。這是對的 —— 稽核軌會洩漏其他場域的活動。
  **畫面不要對所有人顯示這個頁籤。**
* `from > to` 會被擋（422）。日期選擇器自己也要擋。
* 分頁同樣是 cursor。

---

## 畫面 4：多租戶隔離

### 4.0 **先做這個決定** —— 目前只有一個示範租戶

盤點過的事實：

* `sql/009` 只建**一個**示範租戶（`DEMO_GROUP`）。
* `sql/010_smoke_tests.sql` 確實建了第二個（`TEST_OTHER`，
  `aaaaaaaa-0000-4000-8000-0000000000ff`），**但**：
  * 它只在 `make smoke` 的路徑跑（`docker/scripts/smoke-test.sh` 第 37 行），
    不在任何 `MIGRATE_MODE` 裡；
  * 而且**它沒有任何使用者** —— 也就是說**登不進去**。

所以「用兩個租戶的帳號各自登入、看到不同資料」這個最直觀的示範
**現在做不到**。三條路，選一條並在 PR 裡寫下理由：

| 選項 | 成本 | 代價 |
|---|---|---|
| **4A. 只示範 `TENANT_MISMATCH`** | 0 —— 現在就能做 | 只證明「標頭偽造被擋」，不證明資料真的分開 |
| **4B. 新增 `sql/076` 建第二個示範租戶（含使用者與少量資料）** | 半天到一天 | 多一份要維護的種子；`demo` 模式的資料量變兩倍 |
| **4C. 用平台管理端點跨租戶示範** | — | **這條路不存在**：這個系統沒有任何 `/api/v1/platform/*` 端點 |

**建議 4A + 4B，而且 4A 先做**：它不需要新資料、而且它示範的東西
（偽造標頭被伺服器擋下）是決策者真正在問的問題。4B 讓「資料真的分開」
看得到，但那是額外的一天。

> **4A 單獨交付時必須誠實標註。** 一個只示範標頭檢查的畫面若標題寫
> 「多租戶隔離」，那是在宣稱一件沒有被示範的事。標題該是
> 「租戶邊界檢查」，並在頁面上寫明「資料層的隔離由資料庫的 RLS 強制，
> 這個畫面示範的是 API 層的標頭比對」。

### 4.1 要交付什麼（4A）

一個「攻擊面示範」頁：讓觀看者**自己改 `X-Tenant-ID`**，看伺服器怎麼回。

### 4.2 這個畫面真正要示範的三件事

**（a）`X-Tenant-ID` 是必填，而且會與 token 交叉比對。**

`verify_tenant_header`（`fms-shared/src/context.rs` 第 80 行）的三種回應：

| 情形 | 回應 |
|---|---|
| 沒帶標頭 | 400，`X-Tenant-ID header is required` |
| 不是合法 UUID | 400 |
| 是合法 UUID 但與 token 的 `tid` 不符 | **403 `TENANT_MISMATCH`** |

**第三種是重點**：不是靜默忽略、不是回空清單，是明確拒絕。
畫面上要三種都按得到。

**（b）場域級隔離（這個現在就示範得到，而且很有說服力）。**

用 `fm.lin` 登入 —— 他是 FACILITY_ADMIN，**只在台北總部**。
同一支 `GET /work-orders` 在他與 `admin.chen` 之下回不同的資料。
**不需要第二個租戶**，而且它示範的是同一套機制（RLS + 場域收斂）。

> 這比 4B 划算得多：零新資料，而且它示範的是客戶真正會遇到的情形
> （同一家公司裡不同分公司的人不該互看）。

**（c）權限不足的畫面。**

用 `user.huang`（REQUESTER）打一支他沒有權限的端點 → 403 + problem+json。
**這個畫面很容易被忘記做，直到真實使用者遇到。**

### 4.3 已知的絆腳石

* `TENANT_MISMATCH` 是 **403 不是 401**。不要在攔截器裡把它當成
  「token 過期」去觸發 refresh —— 那會變成一個無窮迴圈。
* 場域收斂靠的是連線情境裡的 `app.facility_ids`。從 HTTP 走一定是對的；
  **這也是為什麼 §1.1 說不要繞過 API** —— 直連資料庫時少注入那個變數，
  隔離會「看起來有做、實際沒有」。

---

## 5. 這個 repo 的工作慣例（照著做，否則 review 會退）

前端在獨立的 repo 或子目錄都可以，但下列幾條照舊：

* **契約是權威**（ADR-09 紀律 1）。前端發現契約與實作不符時，
  **回報，不要在前端繞過**。
* **每個決定留下理由。** 這個 codebase 的註解寫的是「為什麼這樣而不是那樣」、
  「這個判斷曾經是錯的」。前端的取捨（為什麼用 `status_category` 分欄、
  為什麼不自己推導按鈕）照樣要寫。
* **不要憑印象抄錯誤碼／欄位名。** 逐條從原始碼抄。
  `FRONTEND-GETTING-STARTED.md` 的錯誤碼表第一版有三個是錯的。
* **分支與 PR**：從最新的 `main` 開分支，squash merge。
  `main` 有分支保護（CI 綠燈 + 與 main 同步）。squash 之後所有舊分支要
  rebase —— 否則 PR 是 `CONFLICTING`，而**衝突的 PR 不會跑 CI**
  （症狀是 `gh pr checks` 回「no checks reported」，看起來像 workflow 壞了）。

### 5.1 一條方法論上的事實，值得先讀

這個後端有 528 個測試、0 失敗，而它同時存在**兩個讓 API 對瀏覽器完全不可用
的阻礙**，兩個都不是任何測試看得見的：

1. **整個樹沒有 CORS 層。** 測試用 `oneshot` 直接打 router，`curl` 不做
   preflight —— 套件全綠、`curl` 全通、瀏覽器一個請求都發不出去。
2. **每個示範帳號的 `password_hash` 都是 NULL。** 測試自己設密碼，
   所以整套跑在一個「有密碼」的資料庫上，而剛種好的環境是「沒密碼」的。
   **沒有任何東西檢查這兩者一致。**

**這對前端工作的意義**：你會是第一個從真正的瀏覽器完整用過這個 API 的人。
遇到「文件說可以但實際不行」時，**那很可能真的是後端的缺口，不是你的錯**。
記下來、回報，附上 `X-Request-ID`。

---

## 6. 完成的定義

**畫面 1（工單看板 + SLA）**

- [ ] 依 `status_category` 分欄，卡片顯示 `status` 與 `sla_state`
- [ ] 按鈕的 enable/disable **完全來自 `available-actions`**，前端沒有第二份狀態機
- [ ] 系統驅動的動作顯示為 disabled，不是隱藏
- [ ] 409 `WORK_ORDER_ILLEGAL_TRANSITION` 觸發重新拉 `available-actions`，不是紅色錯誤框
- [ ] 兩個視窗的併發示範跑得通（一邊完成、另一邊按舊按鈕 → 自動更正）
- [ ] SLA 側欄顯示口徑、期間、分母、排除筆數
- [ ] 看板與 `facility-dashboard` 對同一個場域顯示**同一個**達成率
- [ ] cursor 分頁（**沒有**頁碼列）
- [ ] 改工單帶 `If-Match`；412 與 428 各有一個可重現的畫面

**畫面 2（預約行事曆 + 佔用地圖）**

- [ ] 週檢視 + 牆面板模式（大字、自動刷新）
- [ ] 四種佔用狀態視覺上分得出來，`RESERVED` ≠ `OCCUPIED`
- [ ] `admin.chen` 與 `user.huang` 看同一個私人預約，差別一眼看得出
- [ ] 私人預約顯示為「已預約 + 時段」，不是空白
- [ ] hold → create 兩階段；409 顯示為「剛被訂走」並自動刷新
- [ ] 每一個場域的佔用地圖都至少有一格非 `FREE`（若全 FREE，回報為 bug）

**畫面 3（稽核軌）**

- [ ] 五個篩選都能用；`from > to` 在前端就擋掉
- [ ] 點 `request_id` 顯示同一次請求的所有變更
- [ ] 頁面上寫明「只有 `diff_keys`，沒有前後值」並說出理由
- [ ] `fm.lin` 看不到這個頁籤（不是點進去才 403）
- [ ] 匯出顯示 job 狀態，不假裝同步

**畫面 4（租戶邊界）**

- [ ] §4.0 的選項已選定，理由寫在 PR 裡
- [ ] 三種 `X-Tenant-ID` 情形都按得到，回應原樣顯示（含 `code`）
- [ ] `TENANT_MISMATCH` **不會**觸發 token refresh
- [ ] `fm.lin` 的場域級隔離示範跑得通
- [ ] `user.huang` 的 403 畫面存在
- [ ] 若只做 4A，標題**不是**「多租戶隔離」，且頁面說明了示範邊界

**全部**

- [ ] 離網可用（斷掉外網後重新載入，畫面照樣完整）
- [ ] 每個請求帶 `X-Request-ID`，錯誤畫面顯示它
- [ ] 錯誤碼表逐條核對過 `problem.rs`
- [ ] 一份「後端缺口回報」清單（可以是空的，但要明確說是空的）

---

## 7. 附錄：這四個畫面背後的後端行為在哪裡驗

看不懂某個行為為什麼是那樣時，先讀對應的測試 —— 它們寫了「為什麼」：

| 畫面 | 測試檔 |
|---|---|
| 工單狀態機、`available-actions` | `work_order_slice.rs`、`workorder_tail_slice.rs` |
| SLA 口徑一致性 | `facility_dashboard_slice.rs` 的 `b_`、`sla_report_slice.rs` |
| 私人預約遮罩 | `private_reservation_slice.rs` |
| hold／衝突／未到場釋放 | `reservation_slice.rs`、`journeys_slice.rs` 鏈 A（PR #56） |
| 樂觀鎖與冪等的併發行為 | `concurrency_correctness_slice.rs` |
| 工單執行面（tasks／labor／parts） | `wo_execution_slice.rs` |
| 稽核軌與紅字遮罩 | `audit_log_slice.rs`、`sql/074` 的自我檢查 6d |
| CORS | `cors_slice.rs` |
| 租戶邊界（`TENANT_MISMATCH`） | `auth_slice.rs`（第 267 行斷言那個 code）、`tenant_slice.rs` |
