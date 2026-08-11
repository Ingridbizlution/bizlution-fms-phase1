# WBS 重新 baseline（ADR-09 要求）

| 項目 | 內容 |
|---|---|
| 日期 | 2026-07-31 |
| 原基準 | 137 任務／494 人日／S0–S12／2026-08-03 – 2027-01-29 |
| 觸發原因 | ADR-09 將應用層由 FastAPI 改為 Rust，原估算失效；另有 7 個交付物缺陷影響前期假設 |
| 狀態 | 待 PM 覆核後併入 xlsx |

## 0. 方法與誠實的限制

**我不會把量到的行數換算成人日。** 這兩個切片是 AI 輔助下由單一開發者完成的，
吞吐量不代表你的團隊。任何「行數 × 係數 = 人日」的推導都會是假精確。

本文件提供三種可驗證的東西，以及一個明確標示為假設的區間：

1. **已完成工作與原 WBS 任務的逐項對帳**（事實）
2. **實際量到的產出規模**（事實）
3. **範圍與風險的變動**（事實）
4. **重估區間**（假設，附敏感度分析）

第 4 項需要團隊自行校準 —— 建議做法見第 6 節。

## 1. 已完成的工作對帳

### 完全完成（依原 WBS 估算計 53.5 人日）

| WBS | 任務 | 人日 | 備註 |
|---|---|---|---|
| 1.1 | 建立 Git repo 與分支策略 | 1 | |
| 1.2 | 建立 CI 流程 | 3 | 工具鏈改為 fmt／clippy／cargo test，非 ruff／mypy／pytest |
| 1.3 | 自建 PostgreSQL 環境 | 3 | 交付物已存在；修掉 4 個 blocker（見第 3 節） |
| 1.4 | Migration 執行機制 | 2 | 新增 `MIGRATE_MODE=seed-only`（原本 `make fresh` 不可能成功） |
| 1.5 | 開發環境文件與 CI 整合 | 2.5 | README 的 CI 章節原本記載跑不起來的做法 |
| 1.6 | 專案骨架與模組目錄 | 3 | Rust workspace，5 crate |
| 2.1 | 套用 001 並驗證 | 2 | |
| 2.2 | RLS 中介層（連線層強制） | 3 | `TenantTx` 無公開建構子，型別強制 |
| 2.3 | 套用 007 並驗證 | 2.5 | `make check-rls` 納入 CI |
| 2.7 | outbox 與 emit_event 機制 | 4 | relay + 4 項保證測試 |
| 2.8 | 冪等中介層 | 2.5 | |
| 2.10 | 租戶隔離測試套件 | 3 | 含 mutation test |
| 3.1 | 套用 002 並驗證 | 2 | |
| 3.2 | 本地帳號認證 | 3 | argon2id |
| 3.3 | JWT 發放與驗證 | 4 | 含 `X-Tenant-ID` 與 `tid` 交叉驗證 |
| 7.1 | 套用 005 並驗證 | 2.5 | |
| 7.10 | 預約併發壓力測試 | 5 | T11，100 客戶端，含 mutation test |
| 10.1 | 套用 011 並驗證 | 2 | 修掉缺少 `set_config` 的 blocker |
| 10.15 | 平台情境權限硬化（013） | 2 | |
| 10.14 | 012 煙霧測試納入 CI | 1.5 | |
| 4.8 | 型錄與相容性資料 | 3（暫估） | `GET /asset-models` 含 `scope` 過濾；平台／租戶型錄種子（017）—— 原本兩者皆為 0 筆 |
| 2.x | 組織／場域／空間節點 API | 6（暫估） | 八支端點；**首次可只用 API 開通租戶**；兩棵 ltree 樹的寫入與子樹搬移首次被執行 |
| 5.x-b | PM 產生器常駐化 | 3（暫估） | `fms-jobs` 執行檔（relay 分片 + 每小時掃描 + 優雅關機）＋服務帳號（019）；relay → 產生器整條鏈路首次驗證 |
| S4c | 工時與料件明細 | 4（暫估） | 料件目錄與庫存種子（018）＋領料原子扣帳＋成本 rollup；`include=labor,parts` |
| S4b | 工單子資源（檢查表、留言） | 4（暫估） | `PATCH .../tasks/{taskId}`（契約）＋ `POST .../comments`（後補）；檢查表由保養範本展開 |
| S5 | 物件儲存與附件 | 5（暫估） | 四支端點（**契約原本一支都沒有**）＋ 私有 bucket 預簽下載；實測真的抓得下來 |
| 5.x | PM 計畫 API 與產生器 | 8（暫估） | 三支端點 + 日曆／計量兩條觸發路徑；RRULE 展開在場域當地時區；冪等由 `uq_maintenance_occurrences` 仲裁 |
| 4.9 | 計量讀數 | 3（暫估） | `POST .../meters/{meterCode}/readings`；門檻觸發規則定案並 mutation test |
| 4.7 | 子設備與依賴圖 API | 4 | `include=children,relations,meters,maintenance_plans` + `GET /assets/{assetId}/dependency-graph`；方向語意與深度上界皆有 mutation test |
| S4 | 工單狀態機與 API | 12（暫估） | 六支端點；三關檢查（權限→必填欄位→狀態機）；`side_effects` 補上兩個惰性 key；`payload` 以 `service_items.form_schema` 驗證。**人日數是暫估**：WBS 對應的列尚未核對，且本次額外做掉了 4.1c／4.1d 兩項原本沒有任務的工作 |

### 部分完成（加權約 8.6 人日）

| WBS | 任務 | 人日 | 完成度 | 缺什麼 |
|---|---|---|---|---|
| 1.7 | 可觀測性基礎 | 3 | **100%** | tracing + request-id + **每筆請求一筆帶 `tenant_id`／`user_id`／延遲的存取記錄**（有測試與 mutation test）。OTLP 匯出**已接上並驗證**（`init_telemetry`；compose 有 collector；`make otel-smoke` 在 CI 與本地都四格通過），見下 |
| 2.9 | API 共用層 | 4 | ~95% | cursor 分頁、ETag/If-Match、稀疏欄位集合（`fields`）、單欄排序（`sort`，游標會記下排序欄位）、`include` 關聯展開白名單皆已完成。唯一未做的是多欄排序，且那是刻意的（見 4.1b） |
| 3.9 | RBAC 判定與快取 | 4 | ~90% | 判定委派資料庫；`_own` 列級範圍、場域級 RLS 啟用、請求層級記憶皆已完成。**刻意不做跨請求 Redis 快取**，理由見 4.1f |
| 7.5 | 預約建立與變更 API | 5 | ~60% | 有 CRUD + 樂觀鎖；缺 hold 消耗、附加服務、週期展開 |

**已銀行入帳約 114 人日（原估算的 23.1%）**，剩餘約 **380 人日**
（其中 S4 的 12 人日與 4.8／4.9 的 6 人日是暫估，見上表註記）
（以原 FastAPI 費率計）。

**注意兩個指標已經分歧**：契約覆蓋率是 37/58（63.8%），人日進度 23.1%。
（分母從 53 變 57 是 S5 新增了四支端點，見 4.1j —— 與先前的數字不可直接比較。）
初稿時兩者吻合（6/53 對 12.6%），現在覆蓋率跑在前面。這**不代表超前**，
更可能的解釋是先做的端點偏容易 —— auth、CRUD 列表、單筆詳情都屬於
契約中結構最單純的一群，而剩下的 41 支包含工單狀態機、BIM 匯入、
IoT 遙測寫入、報表聚合。因此**不要用覆蓋率外推完工日期**；
兩個指標分歧本身才是要盯的訊號。

## 2. 實際量到的產出規模

| 部分 | 程式行數（不含註解） | 每端點 |
|---|---|---|
| 橫切共用層 `fms-shared` | 629 | 一次性，**已完成** |
| identity 模組（3 端點） | 528 | ~176 |
| reservation 模組（4 端點） | 517 | **~129** |
| **asset 模組（6 端點，WBS 4.6＋4.7）** | **1148** | **~191** |
| **work_order 模組（8 端點，S4＋S4b＋S4c）** | **1938** | **~242** |
| **asset 模組（8 端點，4.6＋4.7＋4.8＋4.9）** | **1569** | **~196** |
| **maintenance 模組（3 端點 + 產生器，5.x）** | **984** | **~328**（見下） |
| **attachment 模組 + storage（4 端點，S5）** | **601** | **~150** |
| **tenancy 模組（8 端點）** | **981** | **~123** |
| outbox relay worker | ~250 | — |
| 伺服器組裝 | 85 | — |
| 整合測試 | 950 + 400（relay） | ~90 |

**校準結果（2026-07-31 更新）**：asset 模組是第 6 節建議的校準對象。
WBS 4.6 的五支 CRUD 端點實測 **133 行／端點**，與 reservation 的 129 幾乎一致
—— 兩個彼此獨立的 CRUD 型模組收斂到同一個數量級，這個錨點因此比單一資料點
可信得多。

接著做完 4.7 後，同一模組升到 **191 行／端點**（666 → 1148 行，只多一支端點）。
這個上升本身就是重要資訊：新增的 482 行裡，依賴圖端點只佔約 130 行，
其餘 350 行是 `include` 的四個關聯展開 —— **端點數沒變，工作量卻加了一半**。

因此「行數／端點」只在 CRUD 型端點上是穩定錨點。契約裡凡是帶
`include`、圖走訪、或多型 `target` 的端點，都必須單獨估，
不能用端點數乘以 130 推算。原 WBS 把 4.7 估為 4 人日、與 4.6 的 5 人日同級，
從實作量看是低估了關聯展開的部分。

maintenance 模組的 328 行／端點是這張表裡**最不該被直接使用**的一格：
那 984 行裡有排程展開、產生器、兩條觸發路徑，都不是「端點」——
它們是三支端點順帶帶出來的基礎設施。這正好示範了為什麼
「行數／端點」只能用在 CRUD 型端點上：把它套在帶背景作業的模組上
會系統性高估。要估 5.x 這類工作，該數的是「有幾條需要自己定義規則的鏈路」。

工單模組（209 行／端點）進一步印證：它的端點數與資產模組相同，
行數卻更多，因為狀態機的三關檢查、副作用執行、`form_schema` 驗證
都不是「多一支端點」而是「多一層邏輯」。三個模組的實測值 129／191／209
說明**曲線是往上的**，愈晚做的端點愈重 —— 這與第 1 節「兩個進度指標分歧」
是同一件事的兩種量法。

值得注意的是 asset 模組**功能更重**（8 個查詢過濾條件、ltree 子樹查詢、
category code↔id 換算、刪除前的參照檢查、稀疏欄位集合），卻仍落在同一區間。
這支持「**約 130 行／端點**」作為 CRUD 型端點的規劃基準。

`identity` 的 176 偏高（`/auth/me` 聚合 5 支查詢 + 5 個 DTO，
且 password grant 有一次性的租戶解析），屬於聚合型端點，不宜當通用錨。

外推 152 個契約列 ≈ **2 萬行**應用碼，加上約 1.4 萬行測試碼。

## 3. 範圍變動（事實）

### 3.1 已從交付物中修掉的 7 個缺陷

修這些的成本已含在上表，但它們揭示了 **S0/S1 的前期假設過於樂觀**：
原 WBS 假設 `sql/`、`docker/`、`api/` 是可直接使用的既有資產，
實際上 documented happy path（`make fresh && make test`）**完全無法執行**。

1. `01-set-passwords.sh` 缺 execute bit → 角色無密碼
2. `001` 的 `CREATE EXTENSION` 未指定 `SCHEMA public` → `002` 起全部失敗
3. `011` 缺 `set_config('app.is_platform')` → 自己的 RLS 政策拒絕自己的種子
4. `MIGRATE_MODE=seed` 重跑非 idempotent 的 `001` → `make fresh` 不可能成功
5. `012` T9 寫死絕對日期，超出 `advance_booking_days` 窗口 → 從撰寫當日就不可能通過
6. **登入依契約不可實作**（架構級）→ 新增 migration `014`
7. 契約 `CurrentUser.roles[].scope_id` 未標可空，與 `ck_ura_scope` 矛盾

**對排程的含意**：類似「套用 0xx 並驗證」的任務（S3 的 4.1、S4 的 5.1、
S7 的 8.1）估的是 2–2.5 人日，但這些 migration 同樣從未被執行過。
建議各加 1 人日的缺陷處理緩衝，合計 **+3 人日**。

### 3.2 語言變更帶來的範圍調整

| WBS | 原任務 | 人日 | 變動 |
|---|---|---|---|
| 3.5 | SAML2 整合 | 4 | **延後／改 Python**（ADR-09）。Entra ID 走 OIDC 即可；SAML 僅 ADFS 客戶需要 |
| 4.4 | BIM 模型註冊與解析作業 | 6 | **改 Python worker**（IfcOpenShell）。Rust 無 IFC 生態，硬做等於自寫解析器 |
| 4.5 | 未識別元件補正 API | 3 | API 留在 Rust，解析在 Python |
| 3.6 | 地端 AD（LDAPS） | 6 | Rust `ldap3` 可用但成熟度較低，**風險項** |

### 3.3 原 WBS 沒有、但必須做的新任務

| 新任務 | 人日 | 狀態 |
|---|---|---|
| 契約符合性檢查納入 CI（ADR-09 紀律 1） | 2 | **已完成**。原 WBS 只有 9.6「OpenAPI 契約定稿」，沒有「驗證實作符合契約」 |
| ADR-09 撰寫與評審 | 0.5 | 已完成 |
| Python worker 工具鏈與 CI（IFC 用） | 2 | 未開始 |
| sqlx offline 模式（`cargo sqlx prepare` + CI 快取檢查） | 1 | 未開始。端點數變多後 CI 每次都要先建 schema 才能編譯 |
| `40P01` 死鎖重試強化（見 4.1） | 2 | 未開始 |

## 4. 已知風險（不再是估算，是實測結果）

**4.1 高競爭下的預約失敗模式**（T11 實測）

100 路併發搶同一時段時，落敗者的 SQLSTATE 在 `23P01` 與 `40P01`（死鎖）之間
隨機分佈，實測 0–58 個死鎖／輪。正確性不受影響（永遠恰好一筆成功），但：

- API 必須把 `40P01` 視同「時段被搶走」重試或回 409，不可回 500
- 測試需把 `statement_timeout` 放寬到 120s 才能讓 100 個客戶端跑完；
  **沿用 `fms_app` 的 role 預設 30s，此競爭程度下會有可觀比例被 57014 取消**

**這使 ADR-04 把 Redis 短期鎖列為第二階段「減少無效往返」的定位低估了它。**
若任何客戶有真實的高競爭場景（開放報名、整點搶會議室），
兩階段 `reservation_holds`（WBS 7.4，3 人日）就不是選配而是前提；
若仍不足，Redis 鎖層需前移進第一階段（估 +5 人日）。

**4.1b 多欄排序刻意不支援**

`sort` 已支援單欄（`-` 前綴降冪，白名單外回 422）。多欄一律回 422 並說明原因：
多欄 keyset 需要游標承載 N 個鍵、SQL 比較子展開成 N 層字典序，
複雜度與實際需求不成比例（UI 上的排序幾乎都是點單一欄位標頭）。
若日後有真實需求，這是 WBS 2.9 的延伸工作，不是既有實作的缺陷。

實作代價值得記下：為了保住 `query_as!` 的編譯期驗證，動態排序是以
「每個欄位 × 方向各一個 ORDER BY CASE 分支」實作的 —— 未選中的分支對所有列
回 NULL 因而不影響排序。這段 SQL 隨可排序欄位數線性變長（資產 3 個欄位
約 16 行 ORDER BY）。**這是 sqlx 編譯期驗證與動態查詢之間的真實張力**，
規劃後續列表端點時應把這段冗長算進去。

**4.1c 狀態機的宣告式欄位大半沒有執行者（S4 實作時發現）**

`work_order_transitions_allowed` 一列裡有三個宣告式欄位。實測
`fms.transition_work_order()` 只落實了其中一部分：

| 欄位／key | 由誰執行 | 狀態 |
|---|---|---|
| `to_status` + `from_status` + `action` | 資料庫函式 | ✅ 且有觸發器擋直接 UPDATE |
| `side_effects.emit` | 資料庫函式（`emit_event`） | ✅ |
| `side_effects.set_responded` / `set_actual_start` / `set_actual_end` | 資料庫函式 | ✅ |
| **`required_permission`** | **無人** | ❌ 函式查出規則列卻沒讀這一欄 |
| **`required_fields`** | **無人** | ❌ 同上 |
| `side_effects.increment_reopen` | 無人 | ⚠️ 本次由應用層補上 |
| `side_effects.release_assignee` | 無人 | ⚠️ 本次由應用層補上 |
| `side_effects.notify` | 無人 | ❌ 通知模組（006 有表、無派送器） |
| `side_effects.compute_sla` | 無人 | ❌ **全 schema 沒有任何 SLA 計算函式** |
| `side_effects.update_asset_status` | 無人 | ❌ 「結案後設備改回什麼狀態」沒有規則可循 |
| `side_effects.request_satisfaction` | 無人 | ❌ 缺通知模組 |
| `side_effects.release_reservation_step` | 無人 | ❌ 缺預約服務步驟 |
| `side_effects.actor: "SYSTEM"` | 無人 | ❌ 稽核列的 `actor_type` 一律寫 `USER` |

也就是 12 個宣告中有 7 個在本次之前是完全惰性的。這**不是** schema 的錯 ——
004 的欄位註解寫得很清楚（「Fields the API must supply」、
「executed by the service layer」），設計上本來就把它們交給服務層。
問題在於**原 WBS 沒有任何任務對應這些欄位**，很容易被讀成「資料庫已經做完了」。

已補上的兩個選擇標準是「語意明確且不補就靜默錯誤」：
`increment_reopen` 不做會讓 `reopened_count` 永遠是 0（影響 PM 品質報表），
`release_assignee` 不做會讓「我的工單」與 `idx_wo_assignee_open`
對「未結」的定義不一致。其餘五個需要尚不存在的模組，**刻意不假裝執行**。

**必須記在風險欄的一件事**：`required_permission` 由應用層執行，代表任何
**不經 REST API** 的呼叫者（日後的 PM 產單器、SLA 逾期排程器）若直接呼叫
`fms.transition_work_order()`，都會繞過權限與必填欄位檢查。要讓那條路徑也安全，
正解是把檢查下移進 SQL 函式，而不是在每個呼叫端重複實作。
建議在 WBS 加一個明確任務。

**4.1d `available-actions` 的 `label_zh` 原本沒有資料來源**

契約要求回傳動作的中文標籤，而 `work_order_statuses` 只有**狀態**名稱
（`START_WORK` 的按鈕該寫「開始作業」，不是目標狀態的「執行中」）。
整個 schema 沒有動作標籤。已新增 migration `015_work_order_action_catalog.sql`
（與 `work_order_statuses` 同一個 catalog 模式，且含自我驗證：
狀態機用到的每個動作都必須有標籤）。

不選「在 Rust 裡寫死 24 個中文字串」的理由是這個端點的存在目的 ——
契約寫「避免把狀態機邏輯複製到各前端」，把標籤埋進後端程式碼只是換個地方複製。

**4.1e `work_order:read_own` 尚未實作**

權限目錄有 `work_order:read_own`（REQUESTER／TECHNICIAN／SERVICE_STAFF 都只有這個），
但列表與詳情目前只檢查 `work_order:read`。後果是這三個角色**看不到自己的工單**。
實作需要在 `list` 加「僅本人相關」的過濾、在 `get` 加申請人／負責人比對，
屬 WBS 3.9（RBAC 判定）的延伸，不是本次工單切片的範圍。已列為已知缺口。

**4.1f RBAC：三個彼此相關的缺口（WBS 3.9 實作時發現）**

原本標記「缺 Redis 快取」，實際查下去發現缺的不是快取，是三件功能：

**(a) `_own` 權限完全沒有實作。** 權限目錄有 `work_order:read_own` 與
`reservation:read_own`，而 `REQUESTER`／`TECHNICIAN`／`SERVICE_STAFF`
**只有** `_own`。列表與詳情先前只檢查完整的 `read`，因此這三個角色
連自己報修的工單都看不到 —— 也就是系統對絕大多數實際使用者不可用。
已實作為列級範圍（`ReadScope`），單筆讀取回 404 而非 403：
工單編號是租戶內連號，可區分的 403／404 會讓人逐號試探出租戶的工單量。

**(b) 沒有 `facility_id` 過濾條件的列表端點只有 TENANT 角色能用。**
`user_permission_codes` 的 FACILITY 分支比對 `scope_id = p_facility_id`，
傳 NULL 永遠不成立。於是 `GET /work-orders`（不帶 facility_id）對
`FACILITY_ADMIN` 一律 403。這不是保守的預設值，是功能壞掉。
已把「無範圍」的語意改成**在任一範圍持有**（新增
`fms.user_permission_codes_anywhere`）。

**(c) 007 的場域級 RLS 從未生效。** 007 為 15 張表建了 RESTRICTIVE 政策
`facility_scope`，判定式讀 `app.facility_ids`；該 GUC 為空時
`current_facility_ids()` 回 NULL，政策**全部放行**。007 的註解本來就寫
「The API sets app.facility_ids」，只是應用層一直沒設。
已在 `begin_tenant_tx` 內以 `fms.user_accessible_facilities()` 填入。

(b) 與 (c) **必須成對**：只放寬授權會讓場域角色看到整個租戶（權限擴大），
只收斂可見性則端點根本進不去。三者都有 mutation test 守住。

**為什麼不加跨請求的 Redis 快取**

先量再決定。示範資料、Docker Desktop 上的 PG16：

| 情境 | 時間 |
|---|---|
| 冷啟第一次判定 | 4.5ms（含 2.4ms planning、913 shared buffer hits） |
| 暖機後單次判定 | **約 0.16ms**（55 次不同參數共 8.68ms） |

0.16ms 與一次 Redis 往返同級，換不到什麼；而要付的代價有三種失效，
其中一種特別容易被忽略：

1. 角色指派變更 —— 有寫入事件，可主動失效
2. `role_permissions` 變更 —— 同上
3. **`user_role_assignments.valid_until` 時間到期** —— **沒有任何寫入事件**。
   沒有東西會去失效它，於是「已到期而被撤銷的權限」會在快取裡繼續有效。
   只能靠 TTL 兜底，而 TTL 必須短於最小有意義的到期粒度，
   那時快取命中率也就沒剩多少了。

因此改為**請求層級記憶**：同一個請求內對同一組範圍只查一次。
沒有失效問題（請求是毫秒級，交易結束即消失），而且解掉了真正的浪費 ——
`available-actions` 原本對六個動作各問一次權限，是實作 S4 時自己造出來的
N+1，現在是一次查詢加記憶體比對。

`user_has_permission` 也已改成以集合版實作（migration 016），
讓 scope 判定（TENANT／FACILITY／ORG ltree）只有一份 SQL。
012 的 **T12** 逐一比對整個（使用者 × 權限 × 場域）交叉乘積
（示範資料 820 組），並持有 002 原始判定式的參考複本 ——
日後任何人「順手優化」其中一支函式，CI 會立刻紅。

**若日後真的需要跨請求快取**，前提是先有量測支持，且必須連同
`valid_until` 的時間性失效一起設計，不能只靠寫入事件。

**4.1g 順帶修掉的一個 bug：`reservation:create` 檢查時沒有帶場域**

`POST /reservations` 以 `facility_id = None` 檢查 `reservation:create`，
因此只有 TENANT 範圍的角色能建立預約 ——`REQUESTER` 與 `FACILITY_ADMIN`
全部 403。已改為先解析資源所屬場域再檢查。這個 bug 在只用租戶管理員
測試時完全看不到，是「測試帳號權限太大」的典型後果；
測試腳手架因此加了 `login_as`，讓不同角色的差異真的被執行到。

**4.1h 計量讀數：兩個既有落差與一個必須定案的規則（WBS 4.9）**

**(a) `reading_type` 只有一種被實作。** schema 宣告三種
（`CUMULATIVE`／`GAUGE`／`DELTA`），但唯一的既有寫入路徑
`fms.ingest_telemetry`（006）一律 `last_value = value`。
對 `DELTA` 型讀表那是錯的 —— 會把增量寫成總量。
本次的人工登錄端點按型別處理（DELTA 累加、CUMULATIVE 取代並檢查倒退、
GAUGE 取代）。**`ingest_telemetry` 未修**：那是 IoT 路徑，
改它要連同遙測測試一起做，且 Phase 4 會用 Rust broker 換掉內部實作。
已列為已知落差 —— 若有 DELTA 型讀表接上 IoT 點位，資料會是錯的。

**(b) `rollover_at` 全系統未使用。** 會歸零的計數器（例如四位數電表）
在既有程式裡沒有任何處理。本次在人工登錄路徑實作：累計型讀數變小時，
若 `rollover_at` 有值則視為繞回，否則回 422 並在訊息裡指出該欄位。

**(c) 門檻觸發規則原本不存在，本次定案。**
`maintenance_plans` 有 `meter_threshold` 與 `meter_tolerance_pct`，
但**全 schema 沒有任何函式、觸發器或註解說明如何判定**，
`next_due_at` 對計量型計畫也從來沒有人寫入。契約的
`triggered_maintenance_plan_ids` 因此沒有可依循的定義。

定案的規則依讀表型別分兩種，因為「門檻」對兩者的意思不同：

| 讀表型別 | 門檻的意思 | 判定式 |
|---|---|---|
| `CUMULATIVE`／`DELTA` | **週期** | `floor(new/th) > floor(old/th)` |
| `GAUGE` | **界線** | `old < th <= new`（僅向上跨越） |

對兩者套同一條規則必然有一種是錯的：燈泡 5000 小時的計畫要在
5000、10000、15000… 各觸發一次；而壓差門檻若用倍數判定，
在界線附近震盪會每筆讀數都觸發。兩條規則都有 mutation test。

**本端點刻意不產生工單。** 契約的欄位叫 `triggered_maintenance_plan_ids`
而非 `created_work_order_ids`；產單是 PM 產生器的職責（尚未實作）。
因此改為寫入 outbox 事件 `maintenance.meter_threshold_reached`，
與讀數在同一個交易 —— 不會出現「讀數存了但通知遺失」。
PM 產生器實作時應消費這個事件，而不是重新掃全部讀表。

**遲到的讀數不觸發保養。** 補登三個月前的讀數會寫入歷史
（歷史是歷史），但不推進 `last_value`，也不判門檻 ——
否則資料匯入會產生一批「今天到期」的假工單。這一條也有 mutation test。

**4.1i PM 產生器：schema 已經準備好了，只是沒有人接上（WBS 5.x）**

004 把該有的東西都放好了 —— `maintenance_occurrences` 表、
`uq_maintenance_occurrences (plan_id, coalesce(asset_id, 零), scheduled_for)`
唯一索引、`work_orders.maintenance_plan_id` 與 `maintenance_occurrence_id`
外鍵、`source = 'PM_PLAN'` 的列舉值。缺的只是驅動它的程式。

**冪等完全來自那個唯一索引**，應用層沒有自己去重：產生器以
`INSERT ... ON CONFLICT DO NOTHING RETURNING id` 搶占位，搶不到就跳過。
因此產生器重跑、outbox 事件重放（at-least-once）、兩個 worker 同時跑，
都不會產生第二張工單，也不需要應用層加鎖 ——
先查再寫在併發下就是 check-then-act 競態。

**兩條觸發路徑共用一份產生邏輯**：日曆型由掃描驅動、計量型由 4.9 發出的
`maintenance.meter_threshold_reached` 事件驅動，但產出完全相同
（占位 + 工單 + 回寫），因此只寫一次。計量型刻意**不**自己掃讀表：
門檻規則有型別分支（見 4.1h），複製必然漂移。

三個實作時的決策值得記下：

1. **RRULE 用 crate 不自己寫。** RRULE 是規格不是領域規則，
   月底夾值、DST、`BYDAY` 與 `INTERVAL` 的交互是 bug 經典來源。
   這是 ADR-09「不要製造第二份真實來源」的同一條原則，
   只是這次的真實來源是 RFC 5545。
2. **展開在場域當地時區**（`facilities.timezone`，`maintenance_plans`
   自己沒有時區欄位）。「每月 5 號上午 9 點」是當地時間的敘述；
   在 UTC 展開會讓早於當地 08:00 的排程整批落到前一天。
3. **建立計畫時就算出 `next_due_at`。** 產生器完全以它驅動，
   留空的計畫會靜靜地永遠不產生任何工單，而使用者看不出哪裡不對。

**一個 worker 的部署前提必須寫進運維文件**：產生器的跨租戶掃描需要
`fms_owner`（`fms_platform` 成員）連線。以 `fms_app` 連線時
013 的硬化條件不成立，`tenant_isolation` 政策會濾掉全部列 ——
症狀是**產生器安靜地什麼都不做**，沒有錯誤也沒有 log。
實作時真的踩到了，因此註解寫在型別旁邊而非只在函式說明裡。

**尚未做的部分**：`maintenance_occurrences` 的 `SKIPPED`／`MISSED` 狀態
（需要「逾期未產生」的判定規則）、`meter_tolerance_pct`（容差目前未使用）、
以及把產生器接上 `fms-worker` 的 relay 迴圈與排程器
（目前是可呼叫的函式 + 測試，尚未有常駐進程的 main）。

**4.1j 附件：契約有 schema、有引用，卻沒有任何端點（WBS S5）**

`openapi.yaml` 定義了 `Attachment`，並在 `WorkOrderDetail.attachments` 與
`WorkOrderCreate.attachment_ids` 引用它 —— 但**沒有任何端點能產生附件**。
也就是契約要求客戶端提供 `attachment_ids`，卻沒給它取得 id 的方法。
那兩個既有欄位在契約自身的範圍內是不可用的。

本次補上四支端點（`GET/POST /attachments`、
`GET/DELETE /attachments/{attachmentId}`）。**這是新增契約面**，
與先前幾次「修正契約內部矛盾」不同層級，因此單獨記在這裡供審閱。
副作用是覆蓋率的分母從 53 變成 57 —— 27/57（47.4%）與舊分母不可直接比較。

實作決策：

1. **bucket 保持私有、下載一律預簽。** 這不是選擇：`minio-init` 已用
   `mc anonymous set none` 設定三個 bucket，註解寫「附件一律走預簽網址」。
   附件裡有設備照片、簽名、廠商報價 —— 公開可讀的 bucket 等於把租戶隔離
   拆掉一半，資料庫再怎麼 RLS，物件儲存那一側照樣能直接下載。
   測試會抓掉簽章參數的裸網址，斷言它被拒絕。
2. **上傳直接經 API，不用預簽 PUT**（Phase 1）。預簽 PUT 的好處是位元組
   不經應用層，代價是資料列必須在物件存在前先建立，多出一種半完成狀態，
   需要完成回呼或清掃工作。照片與說明書不值得付這個代價。
   **BIM 模型（4.4／4.5，數百 MB）應改用預簽 PUT**，界線寫在
   `storage` 模組的說明裡以免日後誤用。
3. **寫入順序：先上傳物件、後寫資料列。** 反過來的話，上傳失敗會留下指向
   不存在物件的紀錄，而**預簽不檢查物件是否存在** —— 使用者要到點下載
   才看到 404。反向的失敗（物件成功、交易回滾）只留下孤立物件，
   那可用生命週期規則清掃；孤立資料列是壞資料。
4. **刪除是「資料列軟刪除、物件硬刪除」。** 稽核需要知道曾有這個檔案、
   誰上傳的；但軟刪除的意思是「紀錄還在」，不是「檔案還能下載」。
5. **`entity_id` 是多型的、沒有外鍵**，因此存在性只能由應用層檢查。
   這一點有 mutation test：拿掉檢查就能把附件掛到不存在的設備上。
6. **權限沿用所屬實體的權限**（掛在工單上要 `work_order:update`），
   不新造 `attachment:write` —— 新增權限碼要動 008 的種子與所有角色，
   代價遠大於收益。

**一個實作時踩到的 bug 值得單獨記：非 ASCII 檔名。**
`Content-Disposition` 的 `filename=` 限定 ISO-8859-1，直接放 UTF-8 的中文
檔名是**不合法的標頭**，MinIO 的處理方式是整個丟掉。症狀是使用者下載到
一個以物件鍵（uuid）命名的檔案，而且沒有任何錯誤訊息。
**對繁體中文的部署這是常態不是邊緣案例。** 已改為 RFC 6266 的雙寫法：
`filename=` 放 ASCII 退化版、`filename*=UTF-8''<百分比編碼>` 放真檔名。
mutation test 守住它。

**另一個值得記的是測試自己的錯誤假設。** 原本斷言「兩次預簽的網址必須不同」，
理由是「簽章含時間戳」。但 SigV4 的 `X-Amz-Date` 只到秒，同一秒內簽兩次
本來就完全相同 —— 那個斷言編碼了一個錯的假設，而不是抓到 bug。
已改為斷言「是預簽網址且當下可下載」。

**尚未做**：`purpose` 沒有白名單（schema 也沒有 CHECK，因此是自由字串）、
`checksum_sha256` 只寫入未驗證（沒有回頭比對物件的機制）、
BIM 與 exports 兩個 bucket 尚未使用。

**4.1k 工單子資源：檢查表的驗證層沒有替代品（S4b）**

契約把 `WorkOrderTask.result_value` 宣告成無型別（`{}`），而
`work_order_tasks.result_value` 是 jsonb —— **沒有任何資料庫約束能保證
結果值符合該項目的 `input_type`**。資料庫會欣然把字串寫進 NUMBER 項目。
沒有應用層這一關，範本裡的 `min_value`／`max_value`／`options`
就只是裝飾欄位。已按 `input_type` 分派驗證，並有 mutation test。

界線值得寫清楚：**超出範本範圍是 422，不是 `is_pass = false`**。
「進風溫度 55°C」是超標（技師該回報的事實），
「進風溫度 = '熱'」或「= 550」是打錯字。範本設 min／max 是為了界定
**合理讀值**，落在界外的多半是輸入錯誤，靜默收下會污染後續趨勢分析。

檢查項目**只由保養範本的 `checklist` 展開而來**，因此沒有產生器就沒有
檢查表可回填 —— 兩者無法分開測，測試也就放在同一個切片裡。
展開用 `jsonb_to_recordset` 一次寫入，靠 `uq_wo_tasks_seq` 做冪等，
與產生器同一個手法。009 的範本 JSON 欄位名刻意與
`work_order_tasks` 的欄位同名，所以展開是純形狀轉換，沒有應用層決策。

`POST .../comments` 是後補的契約面：`WorkOrderDetail.comments` 已宣告
留言陣列，卻沒有任何端點能新增 —— 與附件同一類「有讀無寫」的缺口。

**尚未做**：`include=labor`／`include=parts`（維持 422 並指名原因）。
兩者都要先有料件目錄種子（`fms.parts` 目前 0 筆），
而 `WorkOrderTransitionRequest.parts_used` 與 `labor_minutes` 目前只寫進
`work_orders.labor_minutes`，沒有產生 `work_order_parts` 與
`work_order_labor` 的明細列，成本 rollup 也還沒接。這是下一個明確的工作項。

**4.1l 一個被時間掩蓋的冪等 bug（值得單獨記）**

計量觸發的排程占位原本用「處理事件時的時鐘」當 `scheduled_for`。
唯一索引是 `(plan_id, asset_id, scheduled_for)`，所以同一筆 outbox 事件
在**不同的秒**重放就是不同的鍵 —— 會產生第二個占位與第二張工單。
outbox 是 at-least-once，重放不是理論風險。

測試原本會通過，因為兩次呼叫剛好落在同一秒。加進檢查表展開讓流程變慢之後
才暴露出來。修法是讓事件自己攜帶 `reading_at`，消費端以它為 `scheduled_for`
—— 冪等鍵必須來自**事件內容**，不能來自處理時的時鐘。
測試現在刻意在兩次呼叫之間睡 1.1 秒，確保它不再靠時間巧合通過。

教訓可以一般化：**任何以「現在」為冪等鍵組成部分的設計，
在重放下都是錯的**。凡是消費 at-least-once 事件的地方都該檢查這一點。

**4.1m 工時成本無法計算：全 schema 沒有費率來源（S4c）**

`hourly_rate` 只出現在 `work_order_labor` 自己身上 —— **沒有使用者費率表、
沒有技能費率表、沒有團隊費率表**，契約的 `WorkOrderTransitionRequest`
也沒有 rate 欄位。因此：

* 工時的**分鐘數**可以記（明細列 + rollup 到 `labor_minutes`）
* 工時的**成本**無法計算，`labor_cost` 恆為 0、明細的 `cost` 恆為 null

刻意不填一個預設費率：那會產生看起來精確而實際憑空的成本數字，
比留 null 糟得多 —— 財務報表會拿它去算，而沒有人知道它是假的。
**若要工時成本，前置條件是先有費率模型** —— 而規格書明確把
「費用結算與計費（Chargeback）」列在**未納入**項目，並兩次說明欄位已預留
（`is_chargeback`、`chargeback_org_id`）而出帳流程未實作。
工時成本不是獨立議題：單價 × 工時就是出帳的計算基礎。

費率的形狀（依使用者／技能／班別、外包是否同一套、跨場域是否不同、
快照 vs 版本化）是**業務決策**，不是本切片能順手決定的。
五個必須先回答的問題、可沿用的既有模式，以及「費率進來後只需改兩處」的
說明，已整理在 **[ADR-10](adr/ADR-10-labour-cost-prerequisites.md)**。

料件成本則完整可算（`parts.unit_cost` 存在），且在**領用時快照**：
日後調價不改寫已完成工單的成本。

**4.1n 領料的併發正確性來自條件式 UPDATE，不是 CHECK 約束**

`ck_part_stock_nonneg` 存在，但**不該讓它成為錯誤路徑**：
它拋 `23514`，而那個 SQLSTATE 在本專案已有兩種語意（配額、狀態機），
再加一種只會讓錯誤映射更難維護。實測驗證了這個判斷 ——
mutation test 把足量條件拿掉後，庫存不足的請求回的是 **500**，不是 409。

正確做法是把條件寫進 `WHERE quantity_on_hand >= $qty` 並看影響列數：
先查再扣是 check-then-act，兩張工單同時領最後一片濾網時兩者都會讀到「還有 1」。

**「該場域沒有這個料件的庫存」不視為錯誤**：照樣記錄用量、不連結庫存列。
廠商當場帶料、緊急採購都是真實情境，拒絕它會讓系統無法記錄真正發生的事。
018 刻意讓 UPS 電池只在總部有庫存，讓這條路徑有資料可測。

**契約修正**：`getWorkOrder` 的 `include` 說明列出 `labor`，
但 `WorkOrderDetail` 沒有對應欄位（與 `open_work_orders` 同一類不一致）。
已補上，並在 schema 註明 `cost` 目前恆為 null 及其原因。

**4.1o 服務帳號沒有角色指派就會被 RLS 完全擋住（5.x-b）**

背景作業需要一個寫入身分（`work_orders.created_by` 有外鍵、
`set_context` 需要 user_id）。002 早就支援 `user_type='SERVICE_ACCOUNT'`，
但**只建使用者列是不夠的**：

`begin_tenant_tx` 會以 `user_accessible_facilities()` 填 `app.facility_ids`，
而 007 的 `facility_scope` RESTRICTIVE 政策讀它（3.9 才啟用，見 4.1f(c)）。
沒有任何角色指派的使用者，可見場域清單是空的 → 應用層填入全零 uuid 哨兵 →
**RLS 濾掉每一列**。症狀是連線正常、查詢成功、永遠回 0 筆，沒有錯誤訊息。

因此 019 建立的是「服務帳號 + **TENANT 範圍**的角色指派」，
並在 migration 內自我驗證「該帳號至少看得到一個場域」——
這個斷言會在佈建出錯時立刻失敗，而不是等到某天發現保養工單沒產生。

角色是專屬的 `PM_GENERATOR`（只有 `maintenance_plan:read`、`asset:read`、
`work_order:create`），不借用 `MAINTENANCE_SUPERVISOR`：借用人類角色會讓
「產生器能做什麼」變成要讀程式才知道的事，而且日後調整那個人類角色
會意外改變背景作業的能力。

**生產佈建注意**：`users.tenant_id` 是 NOT NULL，所以**每個租戶都需要
自己的服務帳號**。租戶佈建流程必須包含這一步，否則新租戶的 PM 產生器
會安靜地不動。已寫在 019 的檔頭。

**4.1p 套件循環揭露的架構問題（值得記）**

把執行檔放在 `fms-worker` 裡會產生
`fms-worker → fms-maintenance → fms-worker` 的循環，Cargo 直接拒絕。
那個循環不是煩人的限制，是**正確的訊號**：`fms-worker` 是機制
（outbox claim、退避、停放），`fms-maintenance` 是政策，
而組裝根（composition root）不該住在機制函式庫裡面。

已拆出 `fms-jobs` 只負責組裝。`fms-worker` 回歸純函式庫、不再有執行檔，
因此「機制不認識任何領域概念」這件事現在由**編譯器**保證，
而不是靠紀律。

**4.1q relay → 產生器這條連結先前從未被驗證**

之前的測試直接呼叫 `on_meter_threshold`，等於繞過 relay。
「relay 認得這個事件型別、取用它、handler 成功後標記 PUBLISHED」
這條連結斷掉的症狀是**事件永遠是 PENDING、工單永遠不出現**，
而且兩邊各自的單元測試都會是綠的。

現在有端到端測試涵蓋，並經 mutation test 驗證三種斷法：
`handles()` 回 false（事件被標 SKIPPED）、handler 假裝成功而不呼叫產生器、
以及產生器借用真人 id 而非服務帳號。

**4.1r 建立場域曾經是不可能的：三層疊起來的死結**

這是本專案目前最有價值的單一發現，而它只有在**真的去建立一個場域**時才會出現。
`POST /facilities` 一開始必然失敗，錯誤是誤導人的
「new row violates row-level security policy」。拆開來是三個獨立問題：

**(1) `FOR ALL ... USING` 會被當成 `WITH CHECK`。**
007 建的政策是 `facility_scope ... FOR ALL USING (platform OR facility_in_scope(id))`，
沒寫 `WITH CHECK`。PostgreSQL 此時把 `USING` 同時用於寫入檢查 ——
於是新增場域必須滿足 `facility_in_scope(新的 id)`，而一個還不存在的 id
不可能在允許清單裡。**同一個政策套在其他 15 張表上是正確的**
（新增資產時要求 `facility_id` 在範圍內正是我們要的），
只有 `facilities` 自己會撞上自舉，因為它的 scope 鍵就是自己的主鍵。
→ migration **020** 只針對 `facilities` 補上獨立的 `WITH CHECK`。

**(2) `INSERT ... RETURNING` 會對回傳列套用 SELECT 側政策。**
即使寫入通過，`RETURNING id` 仍然失敗 —— 剛建立的列不在允許清單裡。
這條規則在 PostgreSQL 文件裡有，但錯誤訊息與 (1) 一模一樣，
極容易誤判為同一個問題。
→ 改為應用層產生 uuid、INSERT 不帶 RETURNING。

**(3) 授權的權威來源被它所授權的可見性過濾。**
`user_accessible_facilities()` 讀 `fms.facilities`，而該表有
`facility_scope` 政策。也就是說**用來算出允許清單的函式，本身被那份清單過濾**：
新場域不在舊清單裡 → 函式看不到它 → 重算的清單還是沒有它 → 永遠看不到。
這是結構性循環，不是快照過期。
→ migration **021** 改為 `SECURITY DEFINER`。

**而 (3) 的修法本身還有一層**：所有租戶表都是 `FORCE ROW LEVEL SECURITY`，
**連表的擁有者都受政策約束**，所以 DEFINER 只是換了 `current_user`，
循環依然存在。必須在函式內暫時取得平台情境（`fms_owner` 是
`fms_platform` 成員）並在離開前還原 —— 與 014 的
`resolve_tenant_by_code` 完全相同的手法與相同的教訓。
**「DEFINER 在 FORCE RLS 下不足」這件事在本專案已經出現兩次**，
值得寫進 code review checklist。

應用層對應的修正是 `refresh_facility_scope()`：`app.facility_ids` 是交易
開始時取的**快照**，任何改變「使用者能看到哪些場域」的寫入都會讓它過期。
目前唯一觸發點是建立場域；**日後新增「指派角色」端點時必須一併呼叫**。

若重算後建立者仍看不到那個場域（例如只有 FACILITY 範圍的角色），
handler **不提交**並回 403 附可行動訊息 ——
「建立了一個自己看不到的東西」比失敗更糟。

四個層次都有 mutation test，其中一個是**直接改資料庫觸發器**：
把 `trg_spatial_node_path` 的子樹重算拿掉後，測試精準地抓到
「`FL01` 搬到了 `TB.FL02.FL01`，但 `R101` 還留在 `TB.FL01.R101`」。

**4.1s `facilities` 沒有 `version` 欄位，但契約宣告了它**

`assets` 與 `work_orders` 都有 `version` 與 `trg_bump_version`，
`facilities` 沒有。契約的 `Facility.version` 因此沒有來源，
而 `PATCH /facilities/{id}` 在契約裡也**沒有** `If-Match` 參數 ——
兩者是一致的：這支端點本來就沒有樂觀鎖。

目前以 `updated_at` 的秒級 epoch 充當版本標記，並**刻意不接到 `If-Match`**：
接上去等於宣稱有樂觀鎖而實際沒有（同一秒內的兩次更新會拿到相同版本號，
衝突偵測不到），那比沒有更危險。
若要真正的樂觀鎖，正解是給 `facilities` 加 `version` 與 `trg_bump_version`
（一次 migration），並在契約補上 `If-Match`。已列為已知缺口。

**4.1t `ltree` 標籤的字元限制必須在應用層擋**

兩個觸發器都用 `regexp_replace(code, '[^A-Za-z0-9_]', '_', 'g')` 產生路徑標籤。
因此 `R-401` 與 `R_401` 會產生**相同的路徑**，撞上
`uq_spatial_nodes_path (facility_id, node_path)`，
而錯誤訊息會指向一個使用者從來沒打過的字串。
已在應用層先擋並說明原因。mutation test 的輸出正好示範了這個陷阱：
`code: "BAD-CODE"` 卻 `org_path: "BAD_CODE"`。

**4.1u 可觀測性：已完成的與刻意未完成的**

**已完成且可驗證**：每筆請求發出一筆 `request completed` 記錄，
帶 `tenant_id`、`user_id`、method、path、status、`latency_ms`。
租戶取自 `require_auth` 放進 extensions 的 `Caller`（JWT `tid` 與
`X-Tenant-ID` 已交叉驗證），**不是直接讀標頭** —— 讀標頭會讓 log 可以被偽造。
有測試攔截 JSON log 斷言欄位真的輸出，並經 mutation test。

一個實作時的判斷：**只建立 span 是不夠的**。span 欄位要靠 subscriber 的
`with_span_events` 設定才會輸出，而那是部署方的設定 ——
「租戶標籤有沒有進 log」不該取決於它。因此明確發出一筆事件。

**已接上，但端到端未跑過一次**：OTLP 匯出。

`init_telemetry`（fms-shared）掛上 `tracing-opentelemetry`，兩個執行檔都呼叫它；
compose 有 `otel-collector`（profile `otel`）；`make otel-smoke` 是驗證機制。

**接上的過程抓到兩個真缺陷**，兩個都不是理論問題：

1. **開機即 panic。** `reqwest` 是以 `rustls-no-provider` 編進來的，建立 Client
   時 panic（"No rustls crypto provider is configured"）。而 panic 不是 Err，
   所以「匯出器建不起來也不讓服務起不來」那段完全接不到 —— 症狀是**第一個設
   `OTEL_EXPORTER_OTLP_ENDPOINT` 的人發現伺服器開不起來**。CI 抓不到，
   因為 CI 不設那個變數。
2. **每個 span 都靜默遺失。** `with_batch_exporter` 的處理器跑在自己的專屬
   執行緒上，而那個執行緒沒有 tokio reactor，所以非同步的 reqwest client 在
   上面 panic —— 而那個 panic 發生在背景執行緒，主流程看不到。
   改用 `reqwest-blocking-client`（上游預設就是它，正是為了配這個執行緒）。

抓到第 2 個的是 `TelemetryGuard` 把 `flush` 與 `shutdown` **分開回報**那一步：
flush 失敗代表真的丟資料，而合在一起看時它被 shutdown 的抱怨蓋掉了。

**已驗證**：`make otel-smoke` 在 CI 與本地都四格通過（collector 就緒 HTTP 200
→ 應用端送出 → collector 收到唯一 marker → `service.name` 正確），
並已接進 CI 的 app job。

-----------------------------------------------------------------------------
一段值得留著的過程紀錄：我一度誤判「這裡驗不了」
-----------------------------------------------------------------------------
撰寫期間我斷定「環境同時封住 Docker registry 與 loopback HTTP，所以端到端
驗不了」，並據此做了三次決策 —— 包括猶豫要不要把一個「從未跑綠」的檢查
接進 CI。

**其中 registry 那半是錯的。** 映像（486 MB）最後有下載完，只是極慢；
我把「慢」誤判成「不可能」。loopback 那半是真的，但不重要 —— 真正的
collector 走 Docker 發布的埠，而那條路一直是通的。

代價是具體的：接進 CI 之後的前兩次紅燈（就緒檢查用字串比對而假通過、
collector 的 `telemetry.logs.level: warn` 把要看的 span 輸出關掉）
**都是本地就抓得到的**。

教訓不是「要更有耐心」，而是：**「驗不了」是一個結論，不是一個觀察。**
下結論之前要能說出「我試了什麼、失敗長什麼樣」——
「等了 20 分鐘還沒好」與「連不上」是兩件事，而它們指向完全不同的下一步。

**4.2 `TEXT + CHECK` 的殘餘漂移風險**

`query!` 驗證欄位與型別，但**不驗證 CHECK 約束的字串值**。
實際踩到過：寫 `state='IN_PROGRESS'` 而約束只允許 `IN_FLIGHT`。
schema 刻意選 `TEXT + CHECK`（避免加值時 exclusive lock）的代價就是這類值
必須靠讀約束定義來確保，編譯器抓不到。

**已採取的緩解**：資產切片踩到第三次（`ACTIVE` vs `OPERATIONAL`）時發現，
真正的問題不只是我方筆誤 —— 客戶端傳一個看似合理但不在清單內的值會得到
**500 而非 422**。因此在 handler 層對這類欄位再驗一次並列出合法值
（見 `fms-asset` 的 `validate_enums`）。後續每個帶 `TEXT + CHECK` 欄位的
端點都應比照，並列入 code review checklist。

**4.3 尚未驗證的架構層**

- 9 個 worker 只完成 1 個（`outbox-relay`）。其餘 8 個共用已驗證的骨架
  （owner 連線 + 平台情境 + 交易邊界 + 退避），單支成本應低於原估
- 13 個 domain event 只有 reservation 家族有 trigger 在發；
  `work_order.*`／`alarm.*`／`asset.*`／`maintenance.*`／`directory.*`
  的 trigger 在 004/006 內，但沒有 handler 消費

## 5. 重估區間（假設，需校準）

**假設**：Rust 相對 FastAPI 的係數只施加於「API／服務層實作」任務
（Backend 角色），不施加於 DBA 的 migration 驗證、QA 的測試設計、
資安、UAT、文件、專案管理。粗分後受影響的實作任務約 **300 人日**。

| 情境 | 係數 | 剩餘人日 | 剩餘工作日 | vs 原定 130 工作日 | 專案總計 | vs 原 494 |
|---|---|---|---|---|---|---|
| 樂觀 | 1.2× | 491 | 129 | −1（持平） | 553 | +12% |
| 中性 | 1.4× | 551 | 145 | **+15 日（約 3 週）** | 613 | **+24%** |
| 保守 | 1.6× | 611 | 161 | +31 日（約 6 週） | 673 | +36% |

上表已含：剩餘 432 人日、+3（migration 缺陷緩衝）、+5（新任務）、
−9（SAML2 延後）、BIM 改 Python 視為平手（省下 Rust 死路，付出工具鏈成本）。

**兩個數字要分開看，不要混用：**

- **「剩餘工作日」是排程要看的數字。** 今天是 2026-07-31，S0 原定 2026-08-03 開始，
  日曆尚未消耗，因此剩餘工作日可直接與原定的 130 個工作日比較。
  中性情境 145 日 → **S12 結束由 2027-01-29 順延約 3 週**。
- **「專案總計」是預算要看的數字**（已完成的 62 人日 + Rust 調整後的剩餘）。
  中性情境 613 人日，較原 494 高 24%。

已銀行入帳的 62 人日**不能**再從剩餘工作日中扣除 —— 它已經不在剩餘裡了。
（本文件初稿曾這樣重複扣除，導致中性情境誤判為「與原排程幾乎相同」。）

## 6. 建議

1. **不要直接採用第 5 節的數字。** 行數校準已完成（見第 2 節，約 130 行／端點，
   兩個模組互相印證），但**行數不等於人日**。仍需你們團隊實際計時一次：
   **4.6–4.9 與 5.x 的核心均已由本次實作完成**，不再是可用的計時對象。
   建議改以 **S3 的 BIM 匯入（4.4／4.5）** 計時：它是唯一跨語言的邊界
   （ADR-09 已定案用 Python worker），範圍清楚，而且 S5 已經把物件儲存
   與預簽上傳的界線畫好了（見 4.1j）。若要工時成本，
   前置條件是先定案費率模型 —— 見
   **[ADR-10](adr/ADR-10-labour-cost-prerequisites.md)**，那應獨立估算
   （規格書本就把 chargeback 排除在 Phase 1 之外，因此不在原 494 人日內）。
2. **先確認高競爭場景是否真實存在**（4.1）。這決定 `reservation_holds`
   是否從選配變成前提，影響 S6 的關鍵路徑。
3. **S3 的 BIM 任務（4.4／4.5）需要 Python 決策落地**：
   是獨立 worker、獨立服務，還是外包。這是唯一跨語言的邊界，宜及早定案。
4. **把 migration 缺陷緩衝寫進 WBS**（3.1），而不是等踩到再吸收。
5. 契約覆蓋率（目前 37/58）已由 CI 每次執行輸出，建議作為 sprint 進度指標，
   比「任務完成數」更難造假。
