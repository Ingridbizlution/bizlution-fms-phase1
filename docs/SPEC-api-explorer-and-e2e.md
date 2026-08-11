# 需求規格：API 瀏覽器 + 端到端情境測試

> 這份規格是給**另一個 session 冷啟動**用的。假設讀者沒有本專案的對話脈絡，
> 因此除了「要做什麼」也寫了「這個 repo 會怎麼絆倒你」。
>
> 執行順序：**先做項目 1，再做項目 2。** 兩者互相獨立，但項目 1 會讓
> 項目 2 的除錯容易得多（可以在瀏覽器裡試打，不用一直改測試）。

---

## 0. 為什麼是這兩件事（量出來的，不是猜的）

2026-08-02 的盤點：

| 事實 | 數字 |
|---|---|
| 已實作端點 | **84** |
| `openapi.yaml` 端點列（含未實作） | 168 |
| 整合測試檔 | 39 |
| 測試總數 | 240 |

而三個缺口是實的：

1. **`api/openapi.yaml` 沒有被任何東西服務出來。** 沒有人能在瀏覽器裡看
   這 84 支端點，也沒有辦法試打。
2. **`openapi.yaml` 的結構只被驗到一半。**
   `endpoints_doc.rs` 確實用 `serde_yaml` 解析它（第 50 行），所以**YAML
   語法錯誤會被抓到**（解析失敗會讓 `contract_ops()` panic，三格測試一起紅）。
   但它只比對「路徑 × 方法」的集合一致性，**不檢查**：
   * `$ref` 解不解得開 —— 指向不存在的 schema 目前不會有人發現
   * `operationId` 是否存在、是否重複
   這兩項是項目 1 要補的。

   > 先讀 `endpoints_doc.rs` 再動手，不要重複實作它已經做的事。
3. **39 個測試檔各驗一段，沒有人驗接縫。** 每一格都自己佈置資料，
   所以「A 的輸出能不能當 B 的輸入」從來沒被驗過。

> **為什麼不是直接做前端**：契約還在收斂。2026-08-02 這一天就改了
> `openapi.yaml` 六次，每次都是因為契約定義的那組端點單獨無法使用
> （指派得了角色卻看不到、匯出了拿不回檔案、兩支 GET 對著空表）。
> 在還在動的契約上蓋前端，等於要重蓋一次。
>
> 目的若變成「給利害關係人看／真實使用者 UAT」，那前端是對的，
> 而且該在契約穩定後照正規做，不是當測試工具。

---

## 項目 1：把 OpenAPI 規格服務出來 + 互動式瀏覽器

### 1.1 要交付什麼

* `GET /api/v1/openapi.yaml`（或 `.json`）—— 回傳 `api/openapi.yaml` 的內容
* `GET /docs` —— 一個互動式 API 瀏覽器，讀上面那份規格
* 一個**會解析 YAML** 的測試，讓語法／`$ref` 錯誤第一次有守衛

### 1.2 硬性約束

**（a）規格必須是同一份檔案，不可以複製。**
ADR-09 紀律 1 是「契約是權威」。若把 openapi 內容複製進 Rust 字串或另一份
檔案，兩份會漂移 —— 而這個 repo 已經有多次「同一條規則兩份手抄本，
其中一份漏了東西」的實例（見 `sql/053` 的檔頭）。

做法建議：`include_str!("../../../../api/openapi.yaml")` 編進 binary。
好處是不依賴執行時的工作目錄（容器裡的 CWD 與開發機不同），
而且**規格改了但忘記重編**會被 `cargo` 自己抓到。

> 若選擇執行時讀檔，必須處理「檔案不存在」—— 而且要讓它**啟動就失敗**，
> 不是第一個請求才 500。這個 repo 的既有做法是這樣
> （`fms-server/src/main.rs` 對 storage 的處理與註解）。

**（b）瀏覽器頁面不可以依賴外部 CDN。**
這是地端產品，客戶環境可能沒有外網。Scalar／Redoc 都有單檔 standalone
的發行版，把它放進 repo（`api/vendor/` 或類似）並用 `include_str!` 一起編進去。

**（c）`/docs` 與 `/openapi.yaml` 的授權要明確決定並寫下理由。**
兩個選項都合理，但**必須選一個並說明**：

* **不需認證**：規格本身不含資料，而地端部署的網路邊界已經是一層防護。
  好處是新人與整合廠商不用先拿 token。
* **需要認證**：規格洩漏了完整的攻擊面清單（84 支端點、參數、權限碼）。

建議前者，但要在程式碼註解裡寫明「這是刻意的，理由是 X」——
這個 repo 的慣例是決定都留下理由。

**（d）不要把 `/docs` 加進 `IMPLEMENTED_OPERATIONS`。**
那份清單是 `endpoints_doc` 與 `contract_conformance` 的輸入，
`implemented_column_matches_the_router` 會要求 ENDPOINTS.md 有對應列。
`/docs` 是基礎設施端點，不屬於契約表格。

**注意：這個 repo 目前一個基礎設施端點都沒有**（沒有 `/healthz`、
沒有 `/readyz`），所以**沒有現成的排除模式可以照抄** —— 你會是第一個。

兩條路，選一條並寫下理由：
* 掛在 `/api/v1` 前綴**之外**（例如 `/docs`、`/openapi.yaml`），
  並確認 `endpoints_doc` 的比對只看 `/api/v1` 底下的路由。
  **先去讀 `implemented_column_matches_the_router` 怎麼取路由清單**，
  確認它不會把它們掃進來。
* 或者掛在 `/api/v1/docs` 並在測試的比對裡明確排除。
  這樣要改測試，而改測試就要說明為什麼那不是在放寬守衛。

第一條比較乾淨。

### 1.3 新增的測試（這是項目 1 真正的價值之一）

**現有的 `endpoints_doc.rs` 已經解析 YAML 並比對路徑集合。** 不要重做那些。
要補的是它沒做的三件事，加上服務端點本身：

1. `paths` 底下每一個 operation 都有 `operationId`，且 `operationId` **不重複**
   —— 重複的 operationId 會讓產生出來的 client SDK 有兩個同名函式
2. **所有 `$ref` 都解得開** —— 指向 `#/components/schemas/X` 而 X 不存在時要失敗。
   目前這種錯誤不會被任何測試抓到，而它在瀏覽器裡的症狀是某一段 schema
   顯示不出來（不是報錯）
3. `GET /docs` 回 200 且 body 含瀏覽器的掛載點
4. `GET /api/v1/openapi.yaml` 回 200，且**內容解析後與磁碟上那份相同**
   —— 這一格守的是「服務出去的與契約是同一份」

> **反面斷言不可省。** 「解析得開」若只驗 `是不是 Ok`，一個回傳空文件的
> 實作也會通過。要同時斷言 `paths` 的數量下界（目前 168 列端點，
> 取一個保守值如 `>= 100`）—— 否則規格被截斷不會有人發現。

### 1.4 已知的絆腳石

* **YAML 函式庫已經有了**：`serde_yaml = "0.9"` 是 `fms-server` 的
  **dev-dependency**（給 `endpoints_doc.rs` 用）。新的測試直接用它，
  不需要加依賴。
* 而且**不要**把它變成正式 dependency —— 服務規格只需要 `include_str!`，
  生產 binary 不必背一個 YAML 解析器。
* `serde_yaml` 0.9 已進入維護停止狀態。既有測試已經在用它，
  **這次不要順手換掉** —— 那是獨立的一件事，混進來會讓這個 PR 的
  變更範圍失焦。
* Scalar 的 standalone JS 約 1–2 MB。那會進 binary。若在意，
  Redoc 的 standalone 也是同量級。**不要為了省容量改用 CDN**（見約束 b）。

---

## 項目 2：端到端情境測試

### 2.1 要交付什麼

一個新的測試檔（建議 `app/crates/fms-server/tests/e2e_journey_slice.rs`），
**一格測試**走完一條連續的業務路徑，每一步都用**前一步的輸出**當輸入。

### 2.2 路徑（每一步的斷言要求）

```
1. 建立使用者          POST /users                → 201，status = INVITED
2. 指派角色            POST /users/{id}/role-assignments → 201
3. 那個人登入          POST /auth/token           → **必須失敗**（INVITED 無密碼）
4. 建工單              POST /work-orders          → 201
5. 指派給步驟 1 的人    POST /work-orders/{id}/transitions (ASSIGN)
6. 執行                 同上 (START_WORK)
7. 完工                 同上 (COMPLETE)
8. SLA 有量到          GET /reports/sla-compliance → 這張工單要在分母裡
9. 稽核查得到          GET /audit-log?entity_id=… → 步驟 1、2 的動作都要在
10. 匯出拿得到檔案      POST /audit-log:export → 202
                       （worker handler 直接呼叫，見 2.4）
                       GET /audit-log/exports/{id} → COMPLETED + download_url
                       下載下來，內容要含步驟 1、2 的動作
```

**步驟 3 是刻意插在中間的反面斷言。** 它證明步驟 1 建立的帳號真的還不能用
（`POST /users` 刻意不設密碼）。少了它，「建立使用者」這一步可能其實建出了
一個可登入的帳號而沒有人發現。

**步驟 5 的執行者必須是場域級的人。** 用 `tech.liu`
（`USERNAME_TECHNICIAN_HQ`，總部的技師）或步驟 1 建立的人並指派場域級角色。
**不要用 `admin.chen`（租戶管理員）**：租戶級授權涵蓋所有場域，它通過
**不代表**場域級的會通過 —— 那正是 `sql/010` 的 T3 曾經掩蓋過的問題
（見 `fix(seed): 補上總部的技師` 那個 commit）。

### 2.3 這一格要抓的是什麼

**接縫，不是功能。** 每個端點本身已經有切片測試。這一格唯一的價值是
「A 的輸出能不能當 B 的輸入」，例如：

* 步驟 1 回的 `id` 能不能直接給步驟 2 當 path 參數
* 步驟 2 建立的角色指派，能不能讓步驟 5 的 ASSIGN 通過權限檢查
* 步驟 7 完工之後，SLA 報表的分母**當下**就包含它，還是要等 worker
* 步驟 9 的 `entity_id` 過濾能不能命中步驟 1 建立的那個人

失敗訊息要說出**是哪一個接縫斷了**，不是只說 assert failed。

### 2.4 harness 的事實（不知道會浪費很多時間）

以下都在 `app/crates/fms-server/tests/common/mod.rs`：

* `TestContext::setup()` —— 從 `fms_template` 複製一個**獨立資料庫**，
  所以測試之間不互相汙染。`ctx.teardown()` 會 DROP 它。
* **`TenantTx` 不可以跨 `teardown()` 存活。** 持有一個開著的交易時
  `DROP DATABASE` 會**無限等待**（不是失敗）。把 tx 包在 block 裡讓它先 drop。
  這個陷阱踩過一次，症狀是測試「卡住」而不是失敗。
* `ctx.login_as(USERNAME)` / `USERNAME_FACILITY_ADMIN` / `USERNAME_REQUESTER`
  / `USERNAME_TECHNICIAN_HQ` —— 只有 `TEST_USERS` 清單裡的帳號設了密碼。
  要用別的示範帳號登入，得先把它加進那個清單。
* `ctx.owner_tx()` —— `fms_owner` + **平台情境**的交易，供佈置資料用。
  一般讀寫要用 HTTP 路徑，不要用它繞過端點。
* `ctx.pool` 是 `fms_app`。用它開交易時**必須自己注入情境**
  （`fms.set_context` + `app.facility_ids`）—— 少了第二步，
  `current_facility_ids()` 是 NULL，而那會讓場域收斂**看起來**有做、實際沒做。
* worker 的 handler 可以在測試裡直接呼叫（不必等 relay 的 idle_interval）。
  範例：`audit_export_slice.rs` 的 `run_export()`、
  `notification_template_slice.rs` 的 `fms_worker::run_once(...)`。
* 物件儲存用 `test_storage()`，**不要用 `fms_server::build_storage()`** ——
  後者走 `StorageSettings::from_env()`，而測試刻意要能在沒有 `.env` 時跑。

### 2.5 環境

需要 compose 全部起來（postgres／redis／minio／mailpit）：

```bash
cd docker && make up && make migrate && make seed && make test-template
```

**改了 `sql/` 之後一定要重跑 `make test-template`** —— 測試從 template
複製資料庫，不重建的話新的 migration 不會生效，而症狀會是一個看起來
完全無關的失敗（踩過：三格測試失敗，原因是 template 建在 migration 之前）。

### 2.6 已知的本機抖動（不要誤判成自己的錯）

macOS 上跑完整套件時，偶爾會看到某個測試 binary 冒出：

```
could not load platform certs: failed to load user trust settings
Os(Error { code: -36, message: "I/O error." })
```

接著**同一個 binary 裡其餘用到它的測試**全部以
`LazyLock instance has previously been poisoned` 失敗 ——
7 格裡只有 1 格顯示真正的成因。

成因是 macOS Security framework 在十幾個測試程序並發讀鑰匙圈時回 `-36`。
**已診斷、刻意不修**（修法會讓 S3 client 忽略系統信任存放區，
而地端客戶的 MinIO 可能用私有 CA）。CI（Linux）不受影響。
完整說明在 `tests/common/mod.rs` 的 `test_storage()` 註解。

**看到整組莫名其妙一起掛時，先找那一格不一樣的。** 重跑通常就過。

---

## 3. 這個 repo 的工作慣例（照著做，否則 review 會退）

* **契約是權威**（ADR-09 紀律 1）。要改行為就先改 `api/openapi.yaml` 與
  `api/ENDPOINTS.md`，不要在實作裡繞過契約。三者一致由
  `endpoints_doc` 測試強制。
* **每個決定留下理由。** 這個 codebase 的註解密度很高，而且寫的是
  「為什麼這樣而不是那樣」、「這個判斷曾經是錯的」。照著寫。
* **反面斷言不可省。** 只驗「該通過的通過了」，一個「一律通過」的實作
  也會過。每一個守衛都要有一格反向的。
* **突變測試是標準。** 交付前把你加的守衛**故意弄壞**，確認有測試失敗，
  而且失敗的是你預期的那一格。並確認突變真的套用了
  （踩過：突變沒生效而誤以為測試有效）。
* **推之前跑完整檢查**：
  ```bash
  cd app && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
  cd ../docker && make migrate-roundtrip   # 只在改了 sql/ 時
  ```
  `migrate-roundtrip` 曾經在 CI 抓到一個 down migration「成功但什麼都沒做」
  （RLS 擋掉 DELETE，影響 0 列而不報錯）。**改了 `sql/` 就在本機先跑它。**
* **分支與 PR**：從最新的 `main` 開分支，squash merge。
  `main` 有分支保護（CI 綠燈 + 與 main 同步才能合併）。
  **squash merge 之後，所有從舊分支長出來的東西都要 rebase 到新的 main**
  —— 否則 PR 會是 `CONFLICTING`，而**衝突的 PR 不會跑 CI**
  （症狀是 `gh pr checks` 回「no checks reported」，看起來像 workflow 壞了）。

---

## 4. 完成的定義

**項目 1**

- [ ] `GET /api/v1/openapi.yaml` 回 200 且內容與 `api/openapi.yaml` 相同
- [ ] `GET /docs` 回 200，在瀏覽器裡打得開、看得到 84 支已實作端點
- [ ] 頁面不發出任何外部網路請求（離網環境可用）
- [ ] 新增的 YAML 解析測試通過，且**含反面斷言**（`paths` 數量下界）
- [ ] `/docs` 的授權決定寫在程式碼註解裡，含理由
- [ ] `cargo test --workspace` 全過、`fmt` 0、`clippy -D warnings` 0

**項目 2**

- [ ] 一格測試走完 §2.2 的 10 個步驟，每一步用前一步的輸出
- [ ] 步驟 3（INVITED 帳號登不進來）與步驟 5（場域級執行者）都成立
- [ ] 每個斷言的失敗訊息說得出是哪一個接縫斷了
- [ ] 這一格**確實抓得到接縫問題**：故意把步驟 2 拿掉，步驟 5 必須失敗
      （若拿掉角色指派之後 ASSIGN 還會過，這一格就沒有在驗接縫）
- [ ] `cargo test --workspace` 全過

---

## 5. 附錄：目前的端點盤點（2026-08-02）

已實作 **84** 支。契約已定義、實作缺的還剩 **7** 支：

| 群組 | 支數 | 備註 |
|---|---|---|
| `/identity-providers` ×3（含 `:sync`） | 3 | `:sync` 會用到 `/directory-role-mappings` |
| BIM 模型 ×3 | 3 | 含 `unresolved-elements` |
| `/reports/facility-dashboard` | 1 | 「單一請求回傳前端首頁所需的全部彙總」 |

**明確標記為未做**的功能（不是遺漏，是有意識的範圍決定，
各自的理由寫在對應的檔頭）：

* ~~**證照到期提醒**（掃描 + 通知）~~ —— 寫這份規格時未做，
  之後由 `sql/059` + `fms-worker/src/cert_watchdog.rs` 補上，
  `idx_user_skills_expiring` 因此有了第一個讀者
* **持續型與掃描型告警規則**（`for_seconds`、`DEVICE_OFFLINE`）——
  見 `sql/057` 檔頭；形狀與 SLA watchdog 相同
* **`alarm_rules.debounce_seconds`** 刻意不讀 ——
  在現行設定下被 `dedupe_window_minutes` 完全涵蓋，
  實作它不會改變任何可觀察的行為
