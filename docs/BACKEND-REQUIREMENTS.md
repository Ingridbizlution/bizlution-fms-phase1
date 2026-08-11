# 後端 API 需求 — 空間預約（Reservations / Bookable Resources）模組

2026-08-07，前端稽核 `frontend/src/routes/reservations/`、`frontend/src/routes/facilities/`
與 `api/openapi.yaml` 逐項比對後整理。前端能靠既有 API 補齊的功能（可預約資源規則
管理、封鎖時段管理、預約編輯、狀態篩選、取消/駁回原因輸入）已經一併完成，見文末
附錄。以下是**後端 API 本身也還沒有**的缺口。

`participants` 寫入面、`PATCH` 的 `apply_scope`（改整個週期系列）已由 #77 解決
——那兩項原本也在這份清單草稿裡，合併時發現已經修好，故不重複列出。

## 1. 現有回應缺漏欄位（小修）

### 1.1 `Reservation` 沒有回傳 `rejection_reason` / `approved_by` / `approved_at`

`POST /reservations/{id}/reject` 的說明寫「原因必填 —— 被駁回的人需要知道理由，
`rejection_reason` 會保存它」，代表欄位已經存在於資料庫；但 `Reservation`／
`ReservationDetail` schema 沒有把這三個欄位包進回應。被駁回的申請人打開自己的
預約詳情看不到「為什麼」，前端也沒有地方可以顯示，因為 API 沒給。

建議：`GET /reservations/{id}`、`GET /reservations` 的回應把這三個欄位加進
`Reservation` schema；`approved_by`/`approved_at` 可仿照 `organizer` 的遮罩邏輯
處理隱私。

### 1.2 文件矛盾：`Reservation.is_private` 說「無法從 API 設定」，但 `ReservationCreate.is_private` 已經有完整定義

`api/openapi.yaml` 裡 `Reservation.is_private` 的說明結尾寫「**目前無法從 API
設定這個旗標** —— `ReservationCreate` 沒有這個屬性」，但緊接著的 `ReservationCreate.
is_private`（目前在 `ReservationCreate` schema 裡）已經有完整的欄位定義與說明，
寫著「不需要額外權限」。這句舊說明應該是欄位補上寫入面之後沒有一併更新，需要刪掉
或改寫，否則前端/串接方看文件會誤判此欄位無效。

## 2. 完全沒有的 API（依優先度排序）

### 2.1 新增可預約資源（房間/資源）的建立 API — 高

`BookableResource` 沒有對應的 `POST` endpoint。目前唯一的產生方式是把既有的空間
節點（spatial node）或設備（asset）標記 `is_bookable`，規則（容量、時長限制、審核
流程等）之後才能用 `PATCH /bookable-resources/{id}` 補上——「新增一個房間」這個最
基本的管理動作在 API 層完全不存在。

需求：`POST /bookable-resources`，body 至少要有 `facility_id`、`resource_type`
（`SPATIAL_NODE`/`ASSET`）、`resource_id`（指向的節點/設備），規則欄位可全部給
預設值。

### 2.2 房間結構化中繼資料（設備/位置） — 中

`BookableResource` 只有一個通用的 `attributes: object`（無結構），沒有「樓層」
「投影機」「白板」等結構化欄位，房間照片也沒有欄位。房間搜尋/篩選（依設備、容量
找房間）目前完全做不到，只能先選日期再看某一個房間有沒有空檔。

需求：評估是否新增結構化 amenities/equipment 欄位，或至少在文件裡定義
`attributes` 的建議 schema。

### 2.3 週期性封鎖規則（Recurring Blackout） — 中

`POST /resource-blackouts` 只能建立單一起訖區間的封鎖，沒有 RRULE 或任何週期規則
支援。「每週日不開放」「國定假日全部不開放」這類常見情境目前只能靠管理員手動一筆
一筆建立。

需求：支援類似 `recurrence_rule`（RFC 5545 RRULE，與 `ReservationCreate.
recurrence_rule` 一致的設計），或至少支援「依星期幾套用」的簡化規則。

### 2.4 等候名單 / 候補通知 — 低

資源被訂滿時，使用者只能等對方取消後自己回來手動查詢，沒有登記候補、資源釋出時
收到通知的機制。

### 2.5 資源群組/資源池的管理介面 — 低

`BookableResource.capacity > 1` 目前語意是「共用資源池，排除約束停用」，但沒有
任何 API 讓管理員定義、檢視或調整池子的實際成員或分配邏輯，完全靠這一個數字欄位
隱含表達。

### 2.6 更細緻的空間使用率報表 — 低

`GET /reports/space-utilization` 只回總體使用率、no-show 率等彙總指標，沒有時段
熱力圖、依部門/申請人的使用量排行、免出席排行榜等細項。純報表需求，不影響核心
預約流程。

---

## 附錄：本次已完成的純前端補工（不需要新 API）

以下項目後端 API 都已支援，純粹是前端沒有接上，本次已完成並更新到
`demo.fms.bizlution.ai`：

- 可預約資源規則管理後台（容量、時長限制、緩衝時間、審核路由、開放時段、逾時
  自動釋出、每人上限）— `Facilities · Spatial · BIM → Bookable resources`
- 封鎖時段（維修/公休/包場）管理後台，含既有預約衝突偵測與二次確認 —
  `Facilities · Spatial · BIM → Blackouts`
- 預約編輯功能（標題/用途/人數/時間），取代原本只能建立/取消
- 預約列表新增狀態、日期區間篩選
- 取消/駁回動作補上原因輸入欄位（駁回原因為後端必填，先前送出會直接 422 失敗，
  已修正 `rejectReservation()` 沒帶 body 的問題）
- 取消整個週期系列後顯示實際取消/略過筆數，而不是靜默導回列表
- Check-out 後顯示實際使用時長 vs. 預約時長
- 預約詳情頁新增「與會者」清單顯示
