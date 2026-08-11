# ADR-14：多租戶日曆聯邦（Microsoft 365／Google Workspace 資源同步）

| 項目 | 內容 |
|---|---|
| 日期 | 2026-08-05 |
| 狀態 | **部分定案** —— 方向與先後順序已由使用者拍板（見第 1 節）；G／H／I／J／K 五個內部架構決定本文件直接定案 |
| 觸發原因 | 「規劃如何在多租戶系統下，讓各租戶之 Microsoft Exchange、Outlook 365、Google Workspace 等系統能資源交換與同步預約」 |
| 相關 | 005（`reservations`／`bookable_resources`）、002／058（`identity_providers`／目錄同步，OAuth 與租戶輪詢的既有形狀）、077-080（`directory_sync_watchdog`，per-tenant 背景輪詢的範本）、080（`bim-worker`，比對＋待審佇列的範本）、001（`event_outbox`／`idempotency_keys`）、ADR-09（語言邊界）、ADR-13 決定 A（`SecretResolver`，已實作） |

> **這份文件的起點跟 ADR-13 不一樣**：ADR-13 有六個決定要拍板，這裡只有兩個
> 屬於「使用者的事」，而且已經拍板了——同步方向選 **C（雙向）**，先實作
> **Microsoft 365**。真正需要這份文件解決的是五個**內部架構**問題：租戶怎麼
> 認證兩家平台（G）、拉入的機制是輪詢還是即時推播（H）、雙向會不會自己咬自己
> 尾巴（I）、外部房間資源怎麼比對到內部空間（J）、程式碼放在哪個 crate（K）。
> 這五個由本文件直接定案，理由是它們不需要客戶回答，是我們該做的工程判斷。

---

## 0. 現況

* `fms.reservations` 已經有 `external_event_id varchar(200)`（欄位註解直接寫
  「Outlook/Google calendar correlation」）與 `created_via` enum 含
  `OUTLOOK`／`GOOGLE` —— **schema 早就留了位置，但沒有任何程式碼寫入過**。
  對 `app/`／`sql/` 全文 grep `exchange`／`outlook`／`google`／`ical`／`caldav`／
  `graph.microsoft`／`calendar` 沒有找到重複或衝突的既有工作。
* 衝突判定的真理來源是 GiST exclusion constraint
  （`excl_reservations_no_overlap`），不是應用層邏輯——只要外部事件也寫成
  一列 `reservations`，雙重預約就自動被擋，不需要重寫判斷邏輯。
* `SecretResolver` trait（ADR-13 決定 A，`fms-shared/src/secrets.rs`）已經
  實作，目前唯一消費者是 `test-connection` 的
  `secret_reference_resolvable`。這次會是**第二個消費者**。
* `fms-jobs` 目前跑 13 個背景迴圈（`tokio::join!`，`main.rs:279-292`），
  其中 `directory_sync_watchdog` 是本文件抄的範本：單一 `fms_owner` 連線，
  跨租戶找到期列，逐筆開 per-tenant tx 處理。
* `services/bim-worker` 是另一個範本：外部實體比對內部型錄，比不到就進
  待審佇列，不是猜測建立。

---

## 1. 已拍板的兩個決定（使用者，2026-08-05）

| 決定 | 選擇 |
|---|---|
| 同步方向 | **C：雙向**（FMS 推出 + 外部拉入，兩者皆做，含防迴圈標記） |
| 先做哪個平台 | **Microsoft 365 先行**；資料模型與 worker 用 `provider` 欄位／trait 抽象兩家，Google Workspace 的實作補在同一個介面之後 |

**一個必須先講清楚的釐清**：「雙向」是**方向**的決定，「即時」是**傳輸機制**
的決定，兩者正交。使用者選的是前者。本文件把傳輸機制定案為輪詢（見決定 H）
——雙向的價值在 Phase 1 就完整拿到，不需要先解決 webhook 的對外可達端點與
訂閱續約這組額外複雜度。若之後要把延遲從 5 分鐘壓到秒級，是加一個 Phase，
不是重做。

---

## 2. 決定 G：租戶怎麼認證兩家平台

兩家的 OAuth 形狀差異大到會影響「先做哪家比較划算」這個已經拍板的順序：

| | Microsoft 365 | Google Workspace |
|---|---|---|
| 認證模型 | 多租戶 Azure AD 應用（我們自己註冊**一個**），客戶 M365 系統管理員做一次 admin consent | OAuth 授權碼流程，客戶 Workspace 管理員同意後拿到 **per-tenant refresh_token** |
| 憑證流向 | client-credentials：`app_client_id`／`app_client_secret`（**全平台共用一份**）+ 客戶的 `ms_tenant_id`（不是密鑰，可明文存） | `google_refresh_token_ref`（**每個租戶各一份**，必須走 `SecretResolver`） |
| 對 `SecretResolver` 的依賴 | 幾乎沒有——只有我們自己那一份 app secret，甚至可以先用環境變數頂著 | **硬依賴**——沒有它就不能安全存放客戶的 refresh token |
| 客戶要給的權限 | Application permission `Calendars.ReadWrite`（app-only，不綁定個別使用者） | OAuth scope `https://www.googleapis.com/auth/calendar` + Admin SDK `admin.directory.resource.calendar.readonly`（資源列表用） |

### 定案：`CalendarProvider` trait 抽象，Microsoft 走 app-only 先落地

```rust
trait CalendarProvider {
    async fn list_resources(&self, integration: &CalendarIntegration) -> Result<Vec<ExternalResource>, Problem>;
    async fn fetch_events_delta(&self, mapping: &CalendarResourceMapping, since: Option<DeltaToken>) -> Result<DeltaPage, Problem>;
    async fn create_event(&self, mapping: &CalendarResourceMapping, reservation: &Reservation) -> Result<ExternalEventId, Problem>;
    async fn cancel_event(&self, mapping: &CalendarResourceMapping, external_event_id: &str) -> Result<(), Problem>;
}
```

Microsoft 實作只需要一份**全平台共用**的 `app_client_secret_ref`（跟客戶數量
無關），因此可以先跳過等 Google 那條路才真正卡住的 per-tenant 密鑰問題，
直接落地。Google 之後是「補一個 trait 實作 + 接上 `SecretResolver`」，不是
重新設計 `calendar_integrations`／`calendar_resource_mappings` 的形狀。

**重新檢視的觸發條件**：無——這是既定的技術事實，不會因為新資訊改變。

---

## 3. 決定 H：拉入機制——輪詢還是 webhook

| 選項 | 延遲 | 代價 |
|---|---|---|
| **H1. 輪詢**（跟 directory-sync 同一個量級，每 5 分鐘） | 分鐘級 | 沒有額外基礎設施；跟現有背景迴圈同一個形狀 |
| **H2. Webhook**（Graph subscriptions／Google watch channel） | 秒級 | 需要對外可達端點、訂閱續約 watchdog（Graph 訂閱約 3 天過期）、驗證握手（Graph 的 `validationToken`、Google 的 channel token） |

兩家平台的 delta 查詢都原生支援「給我上次同步後的變化，包含刪除」
（Graph delta query、Google `syncToken`），因此輪詢不需要每次全量比對，
增量成本很低——H2 省的只是延遲，不是正確性。

### 定案：H1（輪詢，5 分鐘）為 Phase 1，H2 留待之後評估

**重新檢視的觸發條件**：真的有客戶回報「5 分鐘的落差造成實際問題」
（例如高頻使用的會議室，訂了之後同事馬上想在 Outlook 確認），到那時候
再評估 H2 —— 屆時 `calendar_resource_mappings` 已經有真實資料可以量測
「輪詢間隔造成的實際衝突機率」，不用現在憑空猜。

---

## 4. 決定 I：防迴圈與衝突判定——雙向最容易出錯的地方

選了雙向（C）之後，兩個問題必須在寫程式前想清楚，不是寫著寫著才發現：

### 4.1 防迴圈：FMS 推出去的事件不能被自己拉回來當新預約

* **推出時**：在外部事件的自訂欄位上打標記——Graph 的
  `singleValueExtendedProperties`、Google 的 `extendedProperties.private`，
  寫入 `fms_reservation_id`。同時把 `external_event_id` 寫回
  `reservations`（欄位已存在，不需要新 migration 加這一格）。
* **拉入時**：delta 查詢回來的每個事件，先檢查這個自訂欄位——
  * 有 → 這是我們自己推出去的回音，不重新 import；若時間跟 FMS 這邊不一致
    （使用者直接在 Outlook 把 FMS 建立的會議挪了時間），進 4.2 的衝突判定，
    **不靜默覆蓋**。
  * 沒有 → 才是外部原生事件，upsert 成一列 `created_via='OUTLOOK'`／
    `'GOOGLE'` 的合成 `reservations`；delta 查詢回報「已刪除」時，對應的
    合成列要跟著標記取消，不留殭屍佔用。

### 4.2 去重鍵：不是沿用 `idempotency_keys`

`fms.idempotency_keys` 是 HTTP 請求重放用的表——24 小時就過期，綁定
`(tenant_id, idempotency_key, endpoint)`，**形狀對不上**「一個外部日曆事件
要跟內部預約永久關聯」這個需求。

**定案**：在 `reservations` 加一個 partial unique index：

```sql
CREATE UNIQUE INDEX uq_reservations_external_event
  ON fms.reservations (tenant_id, external_event_id)
  WHERE external_event_id IS NOT NULL;
```

拉入 worker 用這個索引直接查「這個外部事件是不是已經對應到一列預約」，
不重新發明一套去重機制。

### 4.3 衝突：兩邊都改了同一筆，不猜、不自動選邊

跟 `STALE_VERSION`（412）的哲學一致：偵測到分歧時不覆蓋，寫一筆
`fms.calendar_sync_conflicts` 記錄（形狀比照 BIM 的 `unresolved_elements`
待審佇列），讓管理者決定以哪邊為準。細部欄位留到實作時設計，這裡先定案
「不自動選邊」這個原則——自動選邊在雙向同步裡幾乎必然導致使用者的其中
一次修改無聲消失，那比「多一步人工確認」更難被信任。

**重新檢視的觸發條件**：無——這是雙向同步的根本正確性要求，不是一個
可以之後放寬的權宜之計。

---

## 5. 決定 J：外部房間資源怎麼比對到內部空間

跟 `services/bim-worker/bim_worker/matcher.py` 完全同一個形狀，不自創一套：

* 唯一鍵比對（Microsoft 房間信箱 UPN／Google 資源 email ↔
  `spatial_nodes` 上的對應欄位），比對成功才建立 `calendar_resource_mappings`
  的 `ACTIVE` 列。
* 比不到 → 狀態設 `UNRESOLVED`，等管理者透過
  `POST /calendar-integrations/{id}/resource-mappings` 手動掛上——不猜測
  建立，跟 BIM 的 `unresolved_elements` 待審佇列同一個判斷。

**重新檢視的觸發條件**：無。

---

## 6. 決定 K：程式碼放在哪個 crate

新的整合設定（`calendar_integrations`／`calendar_resource_mappings`／
`calendar_sync_conflicts`）與兩家平台的 API 客戶端，**不放進 `fms-reservation`
也不放進 `fms-identity`**，而是新建 `fms-calendar` crate。理由：

* 這不是 SSO／SCIM 那類「使用者身分」整合（`fms-identity` 的既有邊界），
  也不是預約業務規則本身（`fms-reservation` 的既有邊界）——是一個獨立的
  外部系統整合邊界，有自己的 OAuth 生命週期、自己的比對佇列、自己的
  provider 抽象。跟 BIM 的 Rust 端落在 `fms-tenancy`（因為 BIM 本質上是
  空間資料）不同，日曆整合本質上誰都不是，塞進既有 crate 只會讓邊界模糊。
* `fms-reservation` 只需要小幅改動：取消/改期的 handler 對
  `created_via IN ('OUTLOOK','GOOGLE')` 的預約直接拒絕並提示去原日曆處理
  ——這是讀 `fms-calendar` 定義的 enum 值，不是依賴它的邏輯。
* Worker（`CalendarSyncWatchdog` 拉入、outbox 消費者推出）掛進 `fms-jobs`，
  跟 `directory_sync_watchdog` 同一個組裝方式，成為第 14 個背景迴圈。

### 語言邊界：留在 Rust，不比照 BIM 去 Python（ADR-09 的判斷一致，結論不同）

BIM 去 Python 是因為 IFC 解析需要 IfcOpenShell，Rust 沒有對等生態——那是
一個**特定格式解析器**的缺口，不是「外部整合都該去 Python」的通則。
Microsoft Graph／Google Calendar 都是普通的 REST+JSON+OAuth2，`reqwest`+
`oauth2` crate 完全夠用，沒有離開 Rust 生態的理由。留在 Rust 可以直接複用
`begin_tenant_tx`／`TenantContext::background`／`SecretResolver`，不用像
Python 的 bim-worker 那樣透過 psycopg 重新兜一套租戶切換邏輯。

**重新檢視的觸發條件**：若未來要接的日曆系統本身需要一個只有其他語言有
成熟函式庫的協定（目前不存在這種情況——CalDAV／iCalendar 標準在 Rust 也有
可用的 crate），才需要重新評估。

---

## 7. 資料模型（migration 從 083 起）

```
fms.calendar_integrations
  id, tenant_id, facility_id (nullable=租戶全域),
  provider (MS365|GOOGLE), status (PENDING_CONSENT|ACTIVE|REVOKED|ERROR),
  ms_tenant_id (MS365 專用，明文), app_client_secret_ref (全平台共用一份，
    不是這張表裡每列各自的祕密——存放位置留到實作時決定，可能根本不是這張表的欄位),
  google_refresh_token_ref (GOOGLE 專用，per-tenant),
  sync_cron (預設每 5 分鐘，複用 fms_shared::cron), last_synced_at

fms.calendar_resource_mappings
  id, tenant_id, calendar_integration_id, spatial_node_id (FK，須 is_bookable),
  external_resource_id, external_resource_name,
  sync_direction (PULL_ONLY|PUSH_ONLY|BIDIRECTIONAL) — 決定 C 選了雙向，
    預設值是 BIDIRECTIONAL，但保留欄位讓個別資源可以退回單向（例如某個
    會議室政策上只允許在 Outlook 訂，FMS 這邊只顯示不受理),
  status (ACTIVE|UNRESOLVED|DISABLED)

fms.calendar_sync_conflicts
  id, tenant_id, reservation_id, calendar_resource_mapping_id,
  external_event_id, detected_at, fms_side_snapshot jsonb,
  external_side_snapshot jsonb, resolved_at, resolved_by, resolution

-- reservations 的新索引（不改既有欄位）
CREATE UNIQUE INDEX uq_reservations_external_event
  ON fms.reservations (tenant_id, external_event_id)
  WHERE external_event_id IS NOT NULL;
```

`app_client_secret_ref` 的確切存放位置（獨立的平台級設定表，還是環境變數）
留到實作時定案——它不像 `google_refresh_token_ref` 是 per-tenant 資料，
硬塞進一張以 `tenant_id` 為主鍵概念的表反而是誤導的形狀。

---

## 8. API（形狀比照 BIM 那組端點）

```
POST /facilities/{id}/calendar-integrations        # 註冊連線，回 admin consent URL
GET  /facilities/{id}/calendar-integrations         # 列出 + 同步健康狀態
GET  /calendar-integrations/{id}/unresolved-resources
POST /calendar-integrations/{id}/resource-mappings  # 手動掛未比對到的房間
```

---

## 9. 建議的實作順序

| # | 項目 | 內容 |
|---|---|---|
| 1 | Migration 083+ | 三張新表 + `reservations` 的 partial unique index |
| 2 | `fms-calendar` crate | `CalendarProvider` trait + Microsoft Graph 實作（client-credentials） |
| 3 | `fms-jobs` 第 14 個迴圈 | `CalendarSyncWatchdog`：拉入（delta fetch → upsert/取消合成預約，跳過自標記回音，寫入衝突佇列） |
| 4 | 新 outbox 消費者 | 篩 `reservation.created`／`confirmed`／`cancelled`，對有 mapping 的資源推出到 Graph，打標記，寫回 `external_event_id` |
| 5 | API | 四支端點 |
| 6 | 業務規則 | `fms-reservation` 的取消/改期 handler 對 `created_via IN ('OUTLOOK','GOOGLE')` 直接拒絕 |
| 7 | Google Workspace | 補 `CalendarProvider` 的第二個實作（前提：實務上要用到 `google_refresh_token_ref` 時才需要動 `SecretResolver` 的部署設定，trait 本身已經抽象好） |

---

## 10. 這份文件沒有涵蓋的

* **Webhook／即時推播**——決定 H 定案輪詢，見該節的重新檢視觸發條件。
* **Google Workspace 的具體實作細節**——先落地 Microsoft，trait 抽象好之後
  再另外規劃 Google 那條路的資源探索（Admin SDK Directory API）與同意流程。
* **使用者個人行事曆的雙向同步**——這份文件只做**房間／資源**日曆
  （Exchange 房間信箱、Google Calendar 資源），不做「把某人的 Outlook
  個人行事曆整個同步進 FMS」。範圍完全不同，牽涉的隱私與權限模型也不同，
  混在一起規劃會讓兩者都做不好。
* **遞迴週期會議的邊界情況**——先處理單次事件與已展開的週期例外，遞迴規則
  本身的雙向映射（RRULE ↔ Graph 的 `seriesMasterId` ↔ Google 的
  `recurringEventId`）留到有真實資料時再評估複雜度。
* **`app_client_secret_ref` 確切的存放位置**——見第 7 節，實作時再定案。
