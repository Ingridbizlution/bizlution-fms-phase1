# FMS Platform API — 第一階段端點清單

本清單是**完整端點盤點**，兩個欄位分別回答兩個正交的問題：

| 欄位 | 意思 |
|---|---|
| **契約** | 已在 `api/openapi.yaml` 定義（含 request/response schema）。`—` 表示同一階段內需補進契約，schema 沿用既有模型 |
| **實作** | 已在 `fms_server::IMPLEMENTED_OPERATIONS` 且可路由。`—` 表示尚未實作 |

前端團隊只需依 `openapi.yaml` 產生 client；本表用於進度追蹤與範圍確認。

> **這兩欄由測試維護，不要手改。**
> `app/crates/fms-server/tests/endpoints_doc.rs` 會逐列比對本表與
> `openapi.yaml`、`IMPLEMENTED_OPERATIONS`，不一致就失敗。
>
> 原本只有一個「狀態」欄，把上面兩件事混在一起 —— 在只有 6 支端點時沒差，
> 44 支之後人們真正要問的是「實作了嗎」，而表格答不出來。盤點當時實測：
> 3 列的狀態填錯（已進契約卻標「待補」），另有 5 支**已實作**的 operation
> 整列不存在（4 支 attachments 與 occupancy）。一份會說謊的地圖比沒有地圖危險，
> 因此改成由來源推導、由測試守住。

---

## 通用規範速查

| 項目 | 規範 |
|---|---|
| Base path | `/api/v1` |
| 必要標頭 | `Authorization: Bearer <jwt>`、`X-Tenant-ID` |
| 選用標頭 | `X-Org-ID`、`X-Facility-ID`、`X-Request-ID`、`Idempotency-Key`、`If-Match` |
| 分頁 | `?limit=50&cursor=...` → `{ data: [], page: { next_cursor, limit, total_estimate } }` |
| 排序 | `?sort=-created_at,name` |
| 過濾 | 具名查詢參數；多值以逗號表示 OR（`?status=ASSIGNED,IN_PROGRESS`） |
| 稀疏欄位 | `?fields=id,wo_no,status` |
| 關聯展開 | `?include=tasks,comments`（白名單，避免 N+1 爆炸） |
| 錯誤 | `application/problem+json`（RFC 9457）+ 穩定 `code` |
| 樂觀鎖 | 具 `version` 的資源：`ETag` / `If-Match`，衝突回 `412` |
| 冪等 | 建立類 POST 支援 `Idempotency-Key`，24h TTL |
| 軟刪除 | `DELETE` 為軟刪除，回 `204`；被引用時回 `409` |

---

## 1. Auth & Identity

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| POST | `/auth/token` | 發放權杖（password / authorization_code / client_credentials） | — | ✔ | ✔ |
| POST | `/auth/token/refresh` | 更新權杖 | — | ✔ | ✔ |
| POST | `/auth/logout` | 撤銷 body 裡那一個 refresh token（access token 不受影響，仍有效至過期） | 已登入 | ✔ | ✔ |
| GET | `/auth/me` | 使用者 + 可用場館 + 有效權限 | 已登入 | ✔ | ✔ |
| GET | `/auth/sso/{providerCode}/authorize` | 回傳 `authorize_url`（不回 302）＋產生 state／nonce／PKCE。**需要 `?tenant_code=`** —— provider code 只在租戶內唯一 | — | ✔ | ✔ |
| GET | `/auth/sso/{providerCode}/callback` | **只完成 state 驗證與一次性消耗**（CSRF／重放防護）；token 交換回 501 —— 缺密鑰解析器與可對接的 IdP，同 LDAP bind 的根本原因 | — | ✔ | ✔ |
| POST | `/auth/password/change` | 變更本地帳號密碼（**不會**登出其他裝置，見回應的 `other_sessions_remain_valid`） | 已登入 | ✔ | ✔ |
| GET | `/identity-providers` | 列出身分來源 | `identity_provider:read` | ✔ | ✔ |
| POST | `/identity-providers` | 新增身分來源 | `identity_provider:write` | ✔ | ✔ |
| PATCH | `/identity-providers/{id}` | 修改身分來源（`code`／`provider_type` 不可改；回應列出目前還沒有消費者的欄位） | `identity_provider:write` | ✔ | ✔ |
| POST | `/identity-providers/{id}/test-connection` | OIDC discovery 完整驗（含 issuer 相符）；**LDAP 只驗 TCP 可達，bind 驗不到**（無客戶端、無密鑰解析器）—— 未驗項目列在 `checks_not_performed` | `identity_provider:write` | ✔ | ✔ |
| POST | `/identity-providers/{id}/sync` | 觸發目錄同步 | `directory:sync` | ✔ | ✔ |
| GET | `/identity-providers/{id}/sync-runs` | 同步歷程與統計 | `identity_provider:read` | ✔ | ✔ |
| GET | `/directory-groups` | 已同步的 AD/Entra 群組（列數由外部寫入；回報從未同步與沒有對應的計數） | `identity_provider:read` | ✔ | ✔ |
| GET | `/directory-role-mappings` | 群組→角色對應清單 | `role:read` | ✔ | ✔ |
| POST | `/directory-role-mappings` | 建立群組→角色對應（同 052 的提權防護） | `role:write` | ✔ | ✔ |
| DELETE | `/directory-role-mappings/{id}` | 刪除對應（回報仍掛著的授權筆數） | `role:write` | ✔ | ✔ |
| GET/POST | `/scim/v2/Users`, `/scim/v2/Groups` | SCIM 2.0 集合（清單 + 建立；filter 只支援 `attr eq "值"`） | SCIM token | ✔ | ✔ |
| GET/PATCH/DELETE | `/scim/v2/Users/{id}`, `/scim/v2/Groups/{id}` | SCIM 2.0 單筆（讀、改、刪；使用者的刪除是改成 DEPROVISIONED） | SCIM token | ✔ | ✔ |

> **SCIM 那兩列原本是一列** —— `GET/POST/PATCH/DELETE | /scim/v2/Users, /scim/v2/Groups`。
> 本表的兩欄是「方法 × 路徑」的交叉展開，因此那一列會展開出
> `PATCH /scim/v2/Users` 與 `DELETE /scim/v2/Groups` 這種 SCIM 根本沒有定義的
> 操作（RFC 7644 §3.5／3.6：PATCH 與 DELETE 只作用在單一資源上）。
> 拆成集合與單筆兩列之後，10 個 operation 與 `IMPLEMENTED_OPERATIONS` 一一對應。
>
> **SCIM token 不是 JWT，也不需要 `X-Tenant-ID`。** 它是 256 bit 的不透明字串，
> 由 `PATCH /identity-providers/{id}` 帶 `rotate_scim_token: true` 產生並**只回傳
> 一次**（migration 074 只存 SHA-256 雜湊）。token 本身就是租戶與身分來源的判別
> 依據 —— Entra ID 只能設一個靜態 token，送不出 `X-Tenant-ID`。
>
> **讀寫範圍限定在發出請求的那個身分來源**：`GET /scim/v2/Users` 只回有該
> provider 的 `user_identities` 列的使用者。回整個租戶會讓 Entra 得以接管
> 它沒有佈建的帳號（包含租戶管理員）。代價寫在 openapi 的 409 說明裡。
>
> **未實作**（刻意的範圍，不是遺漏）：`PUT`、`/ServiceProviderConfig`、
> `/Schemas`、`/ResourceTypes`、完整 filter 文法、`sortBy`、`Bulk`、ETag、
> 巢狀群組成員。理由逐項寫在 `openapi.yaml` 的 SCIM 區塊註解與
> `fms-identity/src/scim.rs` 的模組檔頭。

> **混合式 AD 的設計要點**：認證與授權分離。認證可能來自 Entra ID、地端 AD（LDAPS 或 ADFS）
> 或本地帳號；授權一律落在 `user_role_assignments` 上，並以 `source` 區分是人工指派或目錄同步
> 產生。目錄同步只增刪 `source='DIRECTORY_SYNC'` 的授權，永不動人工授權——這是避免
> 「同步一次把管理員權限清掉」的關鍵約束。

## 2. Tenancy

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/tenant` | 目前租戶設定與功能開關 | `tenant:read` | ✔ | ✔ |
| PATCH | `/tenant` | 更新租戶設定 | `tenant:update` | ✔ | ✔ |
| GET | `/organizations` | 組織清單（支援 `subtree_of`） | `organization:read` | ✔ | ✔ |
| POST | `/organizations` | 建立組織 | `organization:write` | ✔ | ✔ |
| GET | `/organizations/{id}` | 組織詳情 | `organization:read` | ✔ | ✔ |
| PATCH | `/organizations/{id}` | 更新組織（含移動父層，會重算 ltree 子樹） | `organization:write` | ✔ | ✔ |
| DELETE | `/organizations/{id}` | 刪除組織（有下層或設施時回 409） | `organization:write` | ✔ | ✔ |
| GET | `/facilities` | 設施清單（自動限縮於可存取範圍） | `facility:read` | ✔ | ✔ |
| POST | `/facilities` | 建立設施 | `facility:create` | ✔ | ✔ |
| GET | `/facilities/{id}` | 設施詳情 | `facility:read` | ✔ | ✔ |
| PATCH | `/facilities/{id}` | 更新設施 | `facility:update` | ✔ | ✔ |

## 3. Spatial & BIM

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/facilities/{facilityId}/spatial-nodes` | 空間節點（`view=flat\|tree`） | `spatial_node:read` | ✔ | ✔ |
| POST | `/facilities/{facilityId}/spatial-nodes` | 建立節點 | `spatial_node:write` | ✔ | ✔ |
| GET | `/spatial-nodes/{id}` | 節點詳情（含設備摘要） | `spatial_node:read` | ✔ | ✔ |
| PATCH | `/spatial-nodes/{id}` | 更新節點（含 re-parent） | `spatial_node:write` | ✔ | ✔ |
| DELETE | `/spatial-nodes/{id}` | 刪除節點 | `spatial_node:write` | ✔ | ✔ |
| POST | `/facilities/{facilityId}/spatial-nodes:bulk-import` | 批次匯入樓層/房間（CSV/JSON） | `spatial_node:write` | ✔ | ✔ |
| GET | `/spatial-node-types` | 節點型別（平台 + 租戶自訂） | `spatial_node:read` | ✔ | ✔ |
| GET | `/facilities/{facilityId}/bim-models` | BIM 模型清單 | `bim_model:read` | ✔ | ✔ |
| POST | `/facilities/{facilityId}/bim-models` | 註冊模型，非同步排入解析（`bim-worker` 輪詢處理） | `bim_model:write` | ✔ | ✔ |
| GET | `/bim-models/{id}` | 模型詳情與解析報告 | `bim_model:read` | ✔ | ✔ |
| DELETE | `/bim-models/{id}` | 刪除模型（連同它匯入的樓層/空間/設備） | `bim_model:write` | ✔ | ✔ |
| POST | `/bim-models/{id}/reset` | 清掉上次匯入的資料，重新排入解析佇列（不必重新上傳檔案） | `bim_model:write` | ✔ | ✔ |
| GET | `/bim-models/{id}/unresolved-elements` | 未對應元件（人工補正） | `bim_model:read` | ✔ | ✔ |
| POST | `/bim-models/{id}/mappings` | 手動對應元件↔節點/設備 | `bim_model:write` | ✔ | ✔ |
| GET | `/facilities/{facilityId}/floor-view` | 逐層檢視資料（節點 + 設備 + 告警 + 幾何 + 即時佔用 + 設備連線） | `spatial_node:read` | ✔ | ✔ |
| GET | `/spatial-nodes/{floorNodeId}/floor-plan-markers` | 2.5D 平面圖上的設備標記清單 | `spatial_node:read` | ✔ | ✔ |
| POST | `/spatial-nodes/{floorNodeId}/floor-plan-markers` | 在平面圖上新增設備標記 | `spatial_node:write` | ✔ | ✔ |
| DELETE | `/floor-plan-markers/{id}` | 刪除平面圖標記 | `spatial_node:write` | ✔ | ✔ |
| GET | `/facilities/{facilityId}/calendar-integrations` | 日曆整合清單與同步狀態（ADR-14） | `calendar_integration:read` | ✔ | ✔ |
| POST | `/facilities/{facilityId}/calendar-integrations` | 註冊 Microsoft 365／Google Workspace 日曆整合 | `calendar_integration:write` | ✔ | ✔ |
| GET | `/calendar-integrations/{id}/unresolved-resources` | 還沒對應空間節點的外部房間資源（即時算，非儲存狀態） | `calendar_integration:read` | ✔ | ✔ |
| POST | `/calendar-integrations/{id}/resource-mappings` | 手動對應外部房間資源↔空間節點 | `calendar_integration:write` | ✔ | ✔ |

## 4. Assets

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/assets` | 查詢設備 | `asset:read` | ✔ | ✔ |
| POST | `/assets` | 建立設備 | `asset:write` | ✔ | ✔ |
| GET | `/assets/{id}` | 詳情（`include=children,relations,meters,...`） | `asset:read` | ✔ | ✔ |
| PATCH | `/assets/{id}` | 更新設備 | `asset:write` | ✔ | ✔ |
| DELETE | `/assets/{id}` | 報廢／刪除 | `asset:delete` | ✔ | ✔ |
| GET | `/assets/{id}/dependency-graph` | 跨系統依賴圖 | `asset:read` | ✔ | ✔ |
| POST | `/assets/{id}/relations` | 建立依賴關係 | `asset:write` | ✔ | ✔ |
| DELETE | `/asset-relations/{id}` | 移除依賴關係 | `asset:write` | ✔ | ✔ |
| GET | `/assets/{id}/work-orders` | 設備維修履歷 | `work_order:read` | ✔ | ✔ |
| GET | `/assets/{id}/status-history` | 狀態變更歷程 | `asset:read` | ✔ | ✔ |
| POST | `/assets:bulk-import` | 批次匯入設備（含試跑模式） | `asset:write` | ✔ | ✔ |
| POST | `/assets/{id}/meters/{meterCode}/readings` | 登錄計量讀數 | `meter:write` | ✔ | ✔ |
| GET | `/assets/{id}/meters/{meterCode}/readings` | 讀數時序 | `meter:read` | ✔ | ✔ |
| GET | `/asset-categories` | 設備分類樹 | `asset:read` | ✔ | ✔ |
| GET | `/asset-models` | 設備型錄 | `asset_model:read` | ✔ | ✔ |
| POST | `/asset-models` | 新增租戶型錄 | `asset_model:write` | ✔ | ✔ |
| GET | `/asset-models/{id}/compatibility` | 相容性檢查結果 | `asset_model:read` | ✔ | ✔ |
| GET | `/attribute-definitions` | 動態欄位定義（前端動態表單用） | `asset:read` | ✔ | ✔ |
| POST | `/attribute-definitions` | 新增動態欄位 | `tenant:update` | ✔ | ✔ |

## 5. Maintenance (PM)

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/maintenance-templates` | 保養範本 | `maintenance_plan:read` | ✔ | ✔ |
| POST | `/maintenance-templates` | 建立範本 | `maintenance_template:write` | ✔ | ✔ |
| GET | `/maintenance-plans` | PM 計畫清單 | `maintenance_plan:read` | ✔ | ✔ |
| POST | `/maintenance-plans` | 建立 PM 計畫 | `maintenance_plan:write` | ✔ | ✔ |
| PATCH | `/maintenance-plans/{id}` | 更新計畫 | `maintenance_plan:write` | ✔ | ✔ |
| GET | `/maintenance-plans/{id}/preview-schedule` | 試算未來排程（不寫入） | `maintenance_plan:read` | ✔ | ✔ |
| POST | `/maintenance-plans/{id}/generate-now` | 立即產生下一張工單 | `maintenance_plan:write` | ✔ | ✔ |
| GET | `/maintenance-occurrences` | 排程執行紀錄（PM 合規率來源） | `maintenance_plan:read` | ✔ | ✔ |
| POST | `/maintenance-occurrences/{id}/skip` | 跳過本次（需理由） | `maintenance_plan:write` | ✔ | ✔ |

## 6. Service Catalogue (Soft FM)

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/facilities/{facilityId}/service-items` | 可申請服務（含 `form_schema`）；附加服務與獨立申請的前置條件 | `service_item:read` | ✔ | ✔ |
| POST | `/facilities/{facilityId}/service-items` | 建立服務項目 | `service_item:write` | ✔ | ✔ |
| PATCH | `/service-items/{id}` | 更新服務項目 | `service_item:write` | ✔ | ✔ |
| DELETE | `/service-items/{id}` | 停用服務項目 | `service_item:write` | ✔ | ✔ |
| GET | `/service-items/{id}/availability` | 服務可用時段（依 availability 設定） | `service_item:read` | ✔ | ✔ |

## 7. Work Orders（統一工單）

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/work-orders` | 查詢工單（15 種過濾維度） | `work_order:read` / `read_own` | ✔ | ✔ |
| POST | `/work-orders` | 建立工單／服務請求 | `work_order:create` | ✔ | ✔ |
| GET | `/work-orders/{id}` | 詳情（`include=...`） | `work_order:read` | ✔ | ✔ |
| PATCH | `/work-orders/{id}` | 更新欄位（不含狀態） | `work_order:update` | ✔ | ✔ |
| POST | `/work-orders/{id}/transitions` | 執行狀態機動作 | 依動作而定 | ✔ | ✔ |
| GET | `/work-orders/{id}/available-actions` | 目前可執行動作（前端按鈕來源） | `work_order:read` | ✔ | ✔ |
| GET | `/work-orders/{id}/tasks` | 檢查項目 | `work_order:read` | ✔ | ✔ |
| PATCH | `/work-orders/{id}/tasks/{taskId}` | 回填檢查結果 | `work_order:execute` | ✔ | ✔ |
| POST | `/work-orders/{id}/comments` | 新增留言 | `work_order:update` | ✔ | ✔ |
| POST | `/work-orders/{id}/labor` | 登錄工時 | `work_order:execute` | ✔ | ✔ |
| POST | `/work-orders/{id}/parts` | 領用備品 | `work_order:execute` | ✔ | ✔ |
| POST | `/work-orders/{id}/attachments` | **由 `/attachments` 提供**（`target_type=WORK_ORDER` + `target_id`，含預簽上傳） | 依附著對象 | 不適用 | 不適用 |
| POST | `/work-orders/{id}/satisfaction` | 申請人評價 | 申請人本人 | ✔ | ✔ |
| POST | `/work-orders:bulk-transition` | 批次派工／批次結案 | `work_order:assign` | ✔ | ✔ |
| GET | `/work-order-statuses` | 狀態字典（含中英文與分類） | 已登入 | ✔ | ✔ |
| GET | `/work-order-state-machine` | 狀態機定義（供前端繪流程圖） | `work_order:read` | ✔ | ✔ |
| GET | `/teams` | 團隊與成員 | `team:read` | ✔ | ✔ |
| GET | `/teams/{id}/workload` | 團隊負載（派工決策用） | `team:read` | ✔ | ✔ |
| GET | `/teams/{id}/shifts` | 班表 | `team:read` | ✔ | ✔ |
| POST | `/teams/{id}/shifts` | 建立班表 | `team:write` | ✔ | ✔ |
| GET | `/parts` | 備品目錄 | `part:read` | ✔ | ✔ |
| GET | `/part-stock` | 備品庫存（依場域） | `part:read` | ✔ | ✔ |
| GET | `/sla-policies` | SLA 政策清單（含租戶通用） | `sla_policy:read` | ✔ | ✔ |
| POST | `/sla-policies` | 建立政策（租戶通用需 TENANT 範圍） | `sla_policy:write` | ✔ | ✔ |
| PATCH | `/sla-policies/{id}` | 更新政策（不影響已開立的工單） | `sla_policy:write` | ✔ | ✔ |
| GET | `/holiday-calendars` | 假日與補班日 | `holiday:read` | ✔ | ✔ |
| POST | `/holiday-calendars` | 建立（租戶通用需 TENANT 範圍） | `holiday:write` | ✔ | ✔ |
| PATCH | `/holiday-calendars/{id}` | 更新（不影響已開立的工單） | `holiday:write` | ✔ | ✔ |
| DELETE | `/holiday-calendars/{id}` | 刪除（真刪除） | `holiday:write` | ✔ | ✔ |

SLA 政策是**合約條款**，因此有自己的權限碼而不是沿用 `tenant:update`
（「能改公司名稱」不該等於「能改 SLA 承諾」）。兩個範圍規則：

* `sla_policy:write` 宣告 `FACILITY` —— 場域專屬的政策勝過租戶通用的
  （ADR-12 決定 F：SLA 通常寫在「這棟樓的合約」裡），若要求 TENANT
  那條設計就沒有人走得到。
* **租戶通用的政策（`facility_id: null`）額外要求 TENANT 範圍**，因為它
  套用到每一個場域。搬移政策的場域則要求對新舊兩端都有權限 ——
  否則一次 PATCH（把 `facility_id` 設成 `null`）就是權限放大。

假日行事曆決定 SLA 期限（`business_hours_only` 的政策），因此它有自己的
權限碼並標為 `is_dangerous`。**補班日通常必須指定 `windows`** ——
台灣的補班日是週六而多數辦公場域只排週一至五，沿用常規班表會讓那一天
可用 0 分鐘，整筆設定沉默失效；伺服端會擋掉那個組合。

行事曆可以真的刪除，SLA 政策只能停用。差別在於已開立的工單快照了
`sla_policy_id`，但沒有任何東西參照行事曆 —— 期限在開單時就算成絕對時刻了。

種子只覆蓋 `CRITICAL`／`HIGH`／`MEDIUM`。`LOW` 與 `URGENT` 目前**沒有政策**，
因此那兩種優先度的工單一律 `NOT_APPLICABLE`：不進報表分母、不被掃描、
不會升級。補上它們是管理者的決定（分鐘數是合約數字），這三支端點就是為此存在。

## 8. Reservations

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/facilities/{facilityId}/availability` | 多資源可用時段（時間軸資料） | `reservation:read` / `read_availability` | ✔ | ✔ |
| GET | `/facilities/{facilityId}/bookable-resources` | 可預約資源與規則 | `reservation:read` | ✔ | ✔ |
| POST | `/reservations/holds` | 建立短期佔位（兩階段第一步） | `reservation:create` | ✔ | ✔ |
| DELETE | `/reservations/holds/{token}` | 主動釋放佔位 | `reservation:create` | ✔ | ✔ |
| GET | `/reservations` | 查詢預約 | `reservation:read` / `read_own` | ✔ | ✔ |
| POST | `/reservations` | 建立預約（`hold_token` 消耗、附加服務、RRULE 展開、`participants` 皆已實作） | `reservation:create` | ✔ | ✔ |
| GET | `/reservations/{id}` | 詳情（含附加服務、其工單狀態與與會者） | `reservation:read` | ✔ | ✔ |
| PATCH | `/reservations/{id}` | 更改預約 | `reservation:update` | ✔ | ✔ |
| DELETE | `/reservations/{id}` | 取消預約 | `reservation:update` / `cancel_any` | ✔ | ✔ |
| POST | `/reservations/{id}/check-in` | 報到 | 使用者本人／管理員 | ✔ | ✔ |
| POST | `/reservations/{id}/check-out` | 提前離場並釋放時段 | 使用者本人 | ✔ | ✔ |
| POST | `/reservations/{id}/approve` | 審核通過 | `reservation:approve` | ✔ | ✔ |
| POST | `/reservations/{id}/reject` | 審核駁回 | `reservation:approve` | ✔ | ✔ |
| GET | `/facilities/{facilityId}/occupancy` | 即時佔用（現在誰在用哪個資源） | `reservation:read` | ✔ | ✔ |
| DELETE | `/reservation-series/{recurrenceGroupId}` | 取消整個週期系列 | `reservation:update` | ✔ | ✔ |
| PATCH | `/bookable-resources/{id}` | 設定預約規則 | `bookable_resource:write` | ✔ | ✔ |
| GET | `/amenities` | 附屬設備目錄（平台預設＋租戶自訂） | `reservation:read`／`read_own`／`create` 任一 | ✔ | ✔ |
| GET | `/bookable-resources/{id}/amenities` | 資源目前的附屬設備 | `reservation:read`／`read_own`／`create` 任一（該資源所屬場域） | ✔ | ✔ |
| PUT | `/bookable-resources/{id}/amenities` | 設定資源的附屬設備（完整覆寫） | `bookable_resource:write` | ✔ | ✔ |
| GET | `/resource-blackouts` | 封鎖時段清單（預設只回現在起；`bookable_resource_id` 為 null＝全場域） | **`reservation:read`**（刻意不用 `blackout:write` —— 封鎖視窗已從可用性查詢洩漏） | ✔ | ✔ |
| POST | `/resource-blackouts` | 建立封鎖時段（視窗內有既有預約時回 409，需明確 acknowledge；**不會取消既有預約**） | `blackout:write` | ✔ | ✔ |

## 9. IoT

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| POST | `/telemetry:batch-ingest` | 批次寫入遙測（機器帳號） | `telemetry:ingest` | ✔ | ✔ |
| GET | `/devices` | 裝置清單與連線狀態 | `device:read` | ✔ | ✔ |
| POST | `/devices` | 註冊裝置 | `device:write` | ✔ | ✔ |
| PATCH | `/devices/{id}` | 更新裝置（含綁定設備／空間） | `device:write` | ✔ | ✔ |
| GET | `/devices/{id}/points` | 通訊點清單 | `device:read` | ✔ | ✔ |
| GET | `/telemetry/latest` | 多點最新值（儀表板用，單次批量） | `telemetry:read` | ✔ | ✔ |
| GET | `/telemetry/series` | 時序查詢（含降採樣 `interval=5m`） | `telemetry:read` | ✔ | ✔ |
| GET | `/alarms` | 查詢告警（含 `unlinked_only`） | `alarm:read` | ✔ | ✔ |
| POST | `/alarms/{id}/acknowledge` | 確認告警 | `alarm:acknowledge` | ✔ | ✔ |
| POST | `/alarms/{id}/work-order` | 由告警補建工單 | `work_order:create` | ✔ | ✔ |
| POST | `/alarms/{id}/suppress` | 抑制告警（維修期間；一定有期限，上限由租戶設定） | **`alarm:suppress`**（071 新增，刻意不用 `alarm:acknowledge` —— 後者現場人員也有） | ✔ | ✔ |
| GET/POST | `/alarm-rules` | 告警規則（含自動建單設定） | `alarm_rule:read`／`alarm_rule:write` | ✔ | ✔ |
| POST | `/alarm-rules/{id}/test` | 以歷史資料試跑規則 | `alarm_rule:write` | ✔ | ✔ |
| POST | `/alarms:reconcile-work-orders` | 批次補串歷史未關聯告警（逐場域；回應區分四種「沒有補」） | `work_order:create` | ✔ | ✔ |

## 10. Admin

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/users` | 查詢使用者 | `user:read` | ✔ | ✔ |
| POST | `/users` | 建立使用者（本地帳號／外包；建立為 INVITED 且無密碼） | `user:write` | ✔ | ✔ |
| PATCH | `/users/{id}` | 更新使用者（不含 username／status） | `user:write` | ✔ | ✔ |
| POST | `/users/{id}/suspend` | 停用或註銷（不能停用自己） | `user:write` | ✔ | ✔ |
| GET | `/users/{id}/role-assignments` | 授權清單 | `role:read` 或 `role:assign` | ✔ | ✔ |
| POST | `/users/{id}/role-assignments` | 指派角色（含 scope） | `role:assign` | ✔ | ✔ |
| DELETE | `/role-assignments/{id}` | 撤銷授權 | `role:assign` | ✔ | ✔ |
| GET | `/roles` | 角色（平台 + 租戶自訂） | `role:read` 或 `role:assign` | ✔ | ✔ |
| POST | `/roles` | 建立自訂角色（不得含自己沒有的危險權限） | `role:write` | ✔ | ✔ |
| GET | `/permissions` | 權限字典（權限矩陣 UI 用） | `role:read` | ✔ | ✔ |
| GET | `/audit-log` | 稽核日誌 | `audit:read` | ✔ | ✔ |
| POST | `/audit-log:export` | 匯出稽核（非同步產檔） | `audit:export` | ✔ | ✔ |
| GET | `/audit-log/exports/{id}` | 匯出作業狀態與下載網址 | `audit:export` | ✔ | ✔ |
| GET | `/skills` | 技能目錄（平台 + 租戶自訂） | `team:read` | ✔ | ✔ |
| POST | `/skills` | 建立租戶自訂技能 | `team:write` | ✔ | ✔ |
| GET | `/users/{id}/skills` | 使用者技能與證照（到期狀態為算出來的） | `team:read` | ✔ | ✔ |
| PUT | `/users/{id}/skills/{skillId}` | 指派／更新技能與證照（upsert） | `team:write` | ✔ | ✔ |
| — | 到期提醒（掃描＋通知） | 不是端點：`sql/059` 的 `sweep_certification_expiry()` + `cert_watchdog`，前置期在 `skills.reminder_days_before` | — | 不適用 | 不適用 |
| POST | `/uploads/presign` | 取得檔案直傳預簽網址 | 已登入 | ✔ | ✔ |
| GET | `/notifications` | 站內通知收件匣（僅自己的 `IN_APP`） | 已登入 | ✔ | ✔ |
| POST | `/notifications/{id}/read` | 標記已讀（幂等） | 已登入 | ✔ | ✔ |
| GET | `/notification-templates` | 通知範本（含缺文案的轉移清單） | `notification_template:read` | ✔ | ✔ |
| POST | `/notification-templates` | 建立租戶範本（覆寫平台版） | `notification_template:write` | ✔ | ✔ |
| PATCH | `/notification-templates/{id}` | 更新租戶範本 | `notification_template:write` | ✔ | ✔ |
| DELETE | `/notification-templates/{id}` | 刪除覆寫（平台版重新生效） | `notification_template:write` | ✔ | ✔ |
| GET | `/webhooks` | webhook 訂閱清單（含可訂閱事件、簽章規格、at-least-once 語意） | `tenant:update` | ✔ | ✔ |
| POST | `/webhooks` | 建立或更新訂閱（同 url 為更新；`signing_secret` 只回一次；帶 `is_active:false` 即停用 —— 契約無 DELETE） | `tenant:update` | ✔ | ✔ |

## 11. Reporting

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/reports/facility-dashboard` | 設施儀表板彙總（單次請求） | `report:read` | ✔ | ✔ |
| GET | `/reports/sla-compliance` | SLA 達成率（多維彙總） | `report:read` | ✔ | ✔ |
| GET | `/reports/group-rollup` | 集團跨組織彙總（依 org 子樹） | `report:read` | ✔ | ✔ |
| GET | `/reports/asset-reliability` | MTBF / MTTR / 故障 Top N | `report:read` | ✔ | ✔ |
| GET | `/reports/pm-compliance` | PM 準時完成率 | `report:read` | ✔ | ✔ |
| GET | `/reports/space-utilization` | 空間使用率與 No-show | `report:read` | ✔ | ✔ |
| GET | `/reports/service-volume` | 軟性服務量與成本（可 chargeback） | `report:read` | ✔ | ✔ |
| POST | `/reports/{reportCode}:export` | 匯出 xlsx/csv（非同步） | `report:export` | ✔ | ✔ |
| GET | `/reports/exports/{id}` | 查詢匯出作業狀態 | `report:export` | ✔ | ✔ |

`sla-compliance` 的量測規則見 [ADR-12](../docs/adr/ADR-12-sla-measurement.md)
（migration 032–034）。三件事與直覺不同，都是刻意的：

* **回應與解決有不同的分母**（PM 工單不計回應），因此**沒有**單一的
  `compliance_pct` —— 提供那個欄位會讓兩個不可比的百分比看起來可比。
* `excluded_*` / `substituted_*` 是回應的一部分：一個沒有附上排除數的
  達成率，無法判斷它是不是被挑選過的。
* 分母為 0 時 `compliance_pct` 是 `null`，不是 0 也不是 100。

`strictness=strict`（合約用）排除宣告 `business_hours_only` 的 policy；
`operational`（內部監控）以自然時間代算並計數。兩者是同一支計算的參數。

## 12. Attachments

附件是多型的：`target_type` + `target_id` 指向工單、預約、設備或 BIM 模型。
上傳走預簽 PUT，下載回預簽 GET，物件本身不經過 API。

| Method | Path | 說明 | 權限 | 契約 | 實作 |
|---|---|---|---|---|---|
| GET | `/attachments` | 依 `target_type` + `target_id` 列出附件 | 依附著對象 | ✔ | ✔ |
| POST | `/attachments` | 登記附件並取得預簽上傳網址 | 依附著對象 | ✔ | ✔ |
| GET | `/attachments/{id}` | 附件詳情（含預簽下載網址） | 依附著對象 | ✔ | ✔ |
| DELETE | `/attachments/{id}` | 刪除附件 | 依附著對象 | ✔ | ✔ |

---

## 領域事件（Outbox → 訂閱者）

第一階段以資料庫 outbox + worker 輪詢實作；第二階段同一批事件原封不動轉送 Kafka，
生產端與訂閱端契約不變。

| 事件 | 觸發時機 | 主要訂閱者 |
|---|---|---|
| `work_order.created` | 工單建立（含 IoT 自動建單） | 通知、報表 |
| `work_order.assigned` | 派工 | 推播給負責人、SLA 計時 |
| `work_order.status_changed` | 任何狀態變更 | 稽核、報表 |
| `work_order.completed` | 完成 | 通知申請人、滿意度邀請、資產狀態回寫 |
| `work_order.sla_breached` | SLA 逾期 | 升級通知 |
| `reservation.confirmed` | 預約成立 | **附加服務工單 fan-out**、行事曆同步 |
| `reservation.cancelled` | 預約取消 | 取消對應服務工單、釋放班表 |
| `reservation.no_show` | 未報到自動釋放 | 空間使用率報表 |
| `alarm.raised` | 告警產生 | 自動建單、通知 |
| `alarm.cleared` | 告警解除 | 關聯工單提示 |
| `asset.status_changed` | 設備狀態變更 | BIM 檢視、儀表板 |
| `maintenance.occurrence_generated` | PM 產單 | 派工排程 |
| `directory.sync_completed` | 目錄同步完成 | 管理員摘要 |

## 背景作業（Workers）

| 作業 | 頻率 | 職責 |
|---|---|---|
| `outbox-relay` | 每 2 秒 | 撈取 outbox（`FOR UPDATE SKIP LOCKED`）→ 執行 side effects／發通知 |
| `pm-generator` | 每小時 | 依 RRULE / 計量門檻產生 `maintenance_occurrences` 與工單 |
| `sla-watchdog` | 每分鐘 | 掃描 `response_due_at` / `resolution_due_at`，標記 `AT_RISK` / `RESPONSE_BREACHED` / `RESOLUTION_BREACHED`，並對 `ASSIGNED`／`IN_PROGRESS` 的逾期工單觸發 `BREACH_SLA`（**已實作**，migration 033＋035 + `fms_worker::sla_watchdog`） |
| `reservation-janitor` | 每分鐘 | 過期 hold 失效、未報到轉 `NO_SHOW`、釋放時段 |
| `device-heartbeat` | 每 5 分鐘 | 偵測離線裝置並依規則產生 `DEVICE_OFFLINE` 告警 |
| `directory-sync` | 依 IdP cron | AD/Entra 使用者與群組同步、角色自動增撤 |
| `partition-maintainer` | 每日 | 預建下 3 個月的 audit / telemetry 分區，歸檔逾期分區 |
| `rollup-refresher` | 每 15 分鐘 | 更新節點健康度、使用率、設備 health_score 快取 |
| `notification-dispatcher` | 每 10 秒 | 送出 EMAIL（SMTP，指數退避重試）；`IN_APP` 存在即送達；其餘頻道停放為 `SUPPRESSED`（**已實作**，migration 043 + `fms_worker::dispatcher`） |
| `bim-ingest` | 每 30 秒 | 輪詢 `bim_models.status='UPLOADED'`，用 IfcOpenShell 拆出樓層／空間／設備寫入 `spatial_nodes`／`assets`，寫回 `status`（`PARSING`→`PARSED`／`PARSE_FAILED`）。**唯一的 Python 服務**（`services/bim-worker`），獨立於 `fms-jobs` 之外裸進程部署（migration 080 + `services/bim-worker`） |

### `sla-watchdog` 的兩個門檻都由管理者定義，不寫在程式裡

* **預警時點** 來自各 policy 的 `sla_policies.escalation_rules`：
  `at_pct < 100` 的最小值。沒有那樣的規則 → **這個 policy 不預警**
  （種子的 `SLA_CLEANING` 就是這樣宣告的）。
* **哪些狀態可以自動升級** 來自 `work_order_transitions_allowed` 的
  `BREACH_SLA` 規則，含租戶專屬規則。

目前目錄只允許 `ASSIGNED` 與 `IN_PROGRESS`，因此以下兩類逾期
**只標記、不改狀態**（計入回應中的 `not_escalatable`）：

* **還停在 `SUBMITTED`（沒有人接手）** —— 而這是最該升級的一類。
  要覆蓋它，在目錄補 `SUBMITTED → SLA_BREACHED` 就會生效，
  **不需要改程式**；但請一併補 `ASSIGN: SLA_BREACHED → ASSIGNED`，
  否則工單會被困死（`SLA_BREACHED` 出去只有 `CANCEL`／`COMPLETE`／`RESUME`）。
* **`WAITING` 類別的四個狀態** —— 改成 `SLA_BREACHED` 會抹掉「為什麼卡住」
  （等料／等廠商／等核准），而那正是 ADR-12 決定 D 要讓人看見的資訊。
  建議維持不升級。

兩類都仍然進報表分母、仍然被標 `sla_state`。

### 通知的三段：扇出 → 投遞 → 收件匣

migration 041／042 + `fms_worker::notifier` 把事件變成 `fms.notifications` 列：
收件人由 `side_effects.notify` 解析、內容由 `notification_templates` 渲染
（租戶的覆寫版本優先於平台範本）。

migration 043 + `fms_worker::dispatcher` 把它們送出去。狀態的意思是：

| 狀態 | 意思 |
|---|---|
| `QUEUED` | **在等傳輸層**（只有 `EMAIL` 會停在這裡，而且只在 dispatcher 沒跑時） |
| `FAILED` | 暫時性失敗，退避後重試（與 `event_outbox` 同一個語意） |
| `SENT` | 已送達。`IN_APP` 在扇出時就是這個狀態 —— 它存在即送達 |
| `SUPPRESSED` | 終態：該頻道沒有傳輸層、收件人沒有 email、或達重試上限 |
| `READ` | 收件人讀過（只有 `IN_APP` 會到這個狀態） |

**`SMS`／`PUSH`／`WEBHOOK`／`LINE` 沒有傳輸層**，因此它們會被停放為
`SUPPRESSED` 並寫明原因 —— 刻意不留在 `QUEUED`：一個持續成長的 `QUEUED`
堆看起來像「還沒送」，實際是「永遠不會送」。

dispatcher 需要 `SMTP_URL` 與 `MAIL_FROM`；**沒設就不啟動**（啟動時記一筆
`warn`），那樣 `QUEUED` 才誠實地代表「沒有傳輸層」而不是「dispatcher 壞了」。
開發環境用 compose 裡的 mailpit（`smtp://localhost:1025`，UI 在 8025）。

監控：`status = 'QUEUED'` 且 `created_at` 超過門檻的筆數 —— 043 之後那個
查詢才有意義。

### 缺的文案由管理者自己補

十條有 `notify` 的轉移沒有對應範本（009 種的 13 個範本裡只有三個對得上）。
那是**內容工作**，因此 `/notification-templates` 讓管理者自己寫，而
`GET` 的 `meta.transitions_without_template` 會直接列出還缺哪些 ——
不必等事件發生才從 log 發現。

平台範本（`tenant_id IS NULL`）讀得到但改不了（007 的 RLS 就是這樣定的）。
客製的方式是以相同的 `(code, channel, locale)` 建一個租戶版本，
而 migration 042 讓它**確定地**勝出 —— 041 沒有那個優先序，
於是覆寫會有時候生效、有時候不生效。

仍然沒有讀取點的一件事：`APPROVER` 這個 notify 代號既不是角色碼也不是
工單欄位，解析不到任何人（計入 `unresolved` 並記 `warn`）。
要嘛加一個 APPROVER 角色，要嘛把那條規則的 notify 改成既有的角色碼 ——
產品決定。
