# ADR-09　應用層語言採 Rust，IFC 解析切為 Python worker

| 項目 | 內容 |
|---|---|
| 狀態 | 已決定（待第一個垂直切片量測後複核估算） |
| 日期 | 2026-07-31 |
| 取代 | 規格書 §4.1 L2「FastAPI + Pydantic v2 + SQLAlchemy 2.0（async）」 |
| 相關 | ADR-01（RLS 隔離）、ADR-04（排他約束為最終權威）、ADR-07（模組邊界對齊服務邊界）、第 16 章演進路線圖 |

## 背景

規格書 §4.1 指定應用層為 FastAPI 單一模組化服務，WBS 的 494 人日估算亦以此為前提。
本 ADR 記錄改用 Rust 的決定與理由，以及刻意保留的 Python 例外。

決策前提有一項與原規格書不同：開發團隊以 AI 輔助開發，語言熟練度不再是選型的限制條件。
這反而改變了取捨方向（見下）。

## 評估

| 維度 | FastAPI（原規格） | Rust（採用） |
|---|---|---|
| 165 個端點的樣板量 | 少 | 多 —— 但樣板正是 AI 輔助最能吸收的成本 |
| 防「漏注入 tenant context」 | 靠開發紀律與 code review | **型別系統**：extractor 不交出未設 context 的連線 |
| 防「應用層與 schema 漂移」 | 無（SQLAlchemy model 是 schema 的第二份副本） | **`sqlx::query_as!` 編譯期綁定真實 schema** |
| 第二階段「Rust 預約引擎」 | 長出跨語言邊界，需 gRPC 契約 | 抽出一個 crate |
| 第四階段「Rust IoT Broker」 | 同上 | 同上 |
| IFC／BIM 解析生態 | **IfcOpenShell（事實標準）** | 實質不存在 |
| SAML2 生態 | 成熟（pysaml2／python3-saml） | 弱 |
| 招募池 | 大 | 小 |
| WBS 494 人日估算 | 成立 | **失效，需重新 baseline** |

## 決定

**核心 API 以 Rust 實作**，理由依重要性排序：

1. **第 16 章路線圖第二／第四階段本來就要 Rust**（預約引擎、IoT Telemetry Broker）。
   規格書自稱「模組邊界即未來服務邊界」——這句話只在同語言時成立。若 Phase 1 用
   FastAPI，第二階段抽出預約引擎就從「搬一個 crate」變成「跨語言重寫 + 維護 gRPC 契約」。
2. **AI 輔助改變了取捨方向，而且與直覺相反。** AI 最擅長的是樣板，而樣板量是 FastAPI
   唯一的實質優勢，因此 AI 輔助**削弱 FastAPI 的優勢**。但 AI 輔助不會降低
   「產出看似自洽、實際偏離真實來源」的風險——反而提高。本專案已有實例：
   一份以 Rust 重寫的原型憑 prose 描述生出 15 張表與 13 個端點，自帶的測試全綠，
   但與 `sql/001–013` 的 93 張表結構性不符（`public` vs `fms.` schema、
   `organization_id` vs `tenant_id`、原生 ENUM vs `TEXT + CHECK`、無 ltree）。
   `query_as!` 會讓這類漂移在編譯期失敗，而非在測試期「以錯誤的理由通過」。
3. **編譯期保證的價值隨程式碼年齡與人員流動放大**，符合「選最適合長期發展」的判準。
4. ADR-01 自述共享 Schema 的最大風險是「應用層漏寫條件」。Rust 能把它從紀律問題
   降級為型別問題（詳見「實作紀律」）。

**同時刻意保留兩處 Python：**

| 範圍 | 理由 | 邊界 |
|---|---|---|
| IFC／BIM 模型解析（WBS 4.4／4.5） | IfcOpenShell 無 Rust 對等物，硬做等於自寫 IFC 解析器 | 背景作業，經 `bim_models` / `unresolved_elements` 與 outbox 解耦，無同步耦合 |
| SAML2（若客戶要求 ADFS） | Rust SAML2 生態弱 | 獨立小服務；Entra ID 走 OIDC 即可，SAML 非必然需求 |

多語言邊界**刻意放在既有的 DB outbox 縫上，不穿過 API 中間**。這與規格書
「第一階段以資料庫 outbox 發事件、worker 輪詢消化」的設計一致，因此不是新增耦合。

## 被放棄的選項

- **全 FastAPI（原規格）**：放棄的代價是第二／第四階段必然出現跨語言邊界，
  且失去對本專案已證實的兩種失敗模式（漏注入 context、層間漂移）的機械化防禦。
- **全 Rust（含 IFC）**：放棄的代價是為單一批次作業自寫 IFC 解析器，
  投入與風險與其價值不成比例。
- **Rust 核心 + Python 承擔所有整合**（Graph API、Google Calendar 等）：
  這些整合是單純的 REST 呼叫，Rust 生態足夠，沒有切出去的理由；
  切得愈多，多語言的維運成本愈難回收。

## 後果

**必須接受的：**
- **WBS 的 494 人日估算失效。** 第一個垂直切片（auth + 六項橫切關注點）
  完成後以「每端點行數 × 165」重新 baseline，並更新里程碑日期。
- 招募池縮小。此項以 AI 輔助為前提接受。
- 需維運兩套工具鏈（Rust + 一個 Python worker），CI 需分別建置。

**必須落實的實作紀律**（否則本 ADR 的理由不成立）：
1. **契約方向不可反轉。** `api/openapi.yaml` 是手寫的權威契約，前端由它產生 client。
   **不得**從程式碼產生 OpenAPI（原型曾用 utoipa 反向產生，導致與契約不相容）。
   CI 須加一條「實作是否符合 `openapi.yaml`」的檢查。
2. **權限判定一律呼叫 `fms.user_has_permission(user, permission, facility, org)`。**
   不得在 Rust 內重建權限模型——那會把 `permissions`／`role_permissions`／
   `user_role_assignments` 的邏輯抄成第二份副本，正是要避免的漂移。
3. **不得使用 Postgres 原生 ENUM 對應 Rust enum。** `sql/001` 刻意採 `TEXT + CHECK`
   以避免加值時的 exclusive lock；Rust 端須自 `TEXT` 映射。
4. **取得資料庫連線的唯一路徑須先經 `fms.set_context()`。** 以 extractor 型別強制，
   不提供繞過的建構子。
5. **`40P01`（死鎖）須重試或映射為 409。** T11 併發測試顯示 100 路競爭下
   落敗者的 SQLSTATE 會在 `23P01` 與 `40P01` 間隨機分佈；直接回 500 是錯的。
6. sqlx 採 offline 模式（`cargo sqlx prepare` + 版控 `.sqlx/`），
   並在 CI 檢查快取是否過期，避免編譯必須先建出 93 張表。

## 複核條件

第一個垂直切片完成後，若「每端點行數 × 165」外推出的工作量使里程碑不可接受，
本 ADR 應重新評估。此時損失僅一個切片，且六項橫切關注點的 HTTP 語意設計
（錯誤格式、分頁、樂觀鎖、冪等）與語言無關，可直接沿用於 FastAPI。
