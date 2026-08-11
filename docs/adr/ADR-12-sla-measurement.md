# ADR-12：SLA 的量測規則

| 項目 | 內容 |
|---|---|
| 日期 | 2026-08-01 |
| 狀態 | **已核准並實作**（A–H 全部定案；migration 032–039 + `fms-worker` + `fms-report`） |
| 觸發原因 | `GET /reports/sla-compliance`（契約已定義，尚未實作） |
| 相關 | 004（work_orders 的 SLA 欄位）、022（狀態機）、ADR-09 紀律 2 |

> **本文件初版有一處事實錯誤**（見「修正」一節）：`first_responded_at`
> 這個欄位是存在的，而且狀態機已經在維護它。決定 B 不變，但實作方式因此
> 從「由 transitions 導出」改成「修正既有欄位的寫入條件」——
> 而修正的內容正是決定 B 要防的那個失效模式，它已經上線了。

## 為什麼需要這份文件

報表不是難在 SQL，難在**要量的資料現在不存在**，而一旦開始產生那些資料，
規則就寫進了歷史 —— 之後要改就得回填。因此規則先定案，再寫程式。

## 現況：報表現在做出來會是 100%

| 欄位 | 誰寫它 |
|---|---|
| `work_orders.response_due_at` | **沒有任何東西**（004 宣告、應用層只讀） |
| `work_orders.resolution_due_at` | **沒有任何東西** |
| `work_orders.sla_policy_id` | 只有 `raise_alarm`（告警自動開單）。手動開單與 PM 產單都沒有 |
| `work_orders.first_responded_at` | 狀態機（4 個動作宣告 `set_responded`）—— **但寫入條件是錯的**，見「修正」 |

而 004 第 525 行在完成時的判定是：

```sql
sla_state = CASE WHEN to_status IN ('COMPLETED','CLOSED')
                 AND (resolution_due_at IS NULL OR clock_timestamp() <= resolution_due_at)
                 AND sla_state NOT IN ('RESPONSE_BREACHED','RESOLUTION_BREACHED')
            THEN 'MET' ELSE sla_state END
```

`resolution_due_at` 恆為 NULL → 條件恆為真 → **每一張完成的工單都是 `MET`**。

這正是 WBS 4.1 警告過的形態：「假數字比 null 糟得多 —— 財務報表會拿它去算，
而沒有人知道它是假的」。而它已經在 schema 裡，只是還沒有人去讀它。

## 決定

### A. 時鐘從進入 `SUBMITTED` 開始（已定案）

不用 `created_at`：`DRAFT` 是還沒送出的草稿，把它計入等於因為使用者慢慢填表
而扣自己的分。`work_order_transitions` 有完整歷史，因此這個時刻永遠算得出來，
不需要新欄位。

### B. 「回應」= 首次由**人**接下工單（已定案）

`first_responded_at` 已經存在，而狀態機用 `side_effects.set_responded` 決定
何時寫它。宣告在四個動作上：

| 動作 | 路徑 | `side_effects.actor` |
|---|---|---|
| `ASSIGN` | `SUBMITTED`／`APPROVED` → `ASSIGNED` | （無，即人為） |
| `ACCEPT` | `SUBMITTED` → `ASSIGNED` | （無，即人為） |
| `AUTO_ASSIGN` | `SUBMITTED` → `ASSIGNED` | **`SYSTEM`** |

也就是說「回應 = 有人接下工單」這個語意，目錄裡早就定好了，**而且定得對**。
不需要改語意，也不需要新欄位，更不需要從 transitions 導出。

要改的是寫入條件：**`AUTO_ASSIGN` 目前也會設 `first_responded_at`。**
自動派工把工單塞給某個人，`first_responded_at` 就被填上，
而那個人可能還沒看過它。這正是本決定要防的失效模式 ——
它決定指標量的是「系統反應快」還是「人反應快」—— 而它已經上線。

因此 032 做兩件事：

1. `first_responded_at` 只在 `side_effects->>'actor'` 不是 `SYSTEM` 時寫入。
2. `transition_work_order` 依 `side_effects->>'actor'` 設
   `work_order_transitions.actor_type`。**它目前從不設**，因此每一筆
   transition 都吃了 `DEFAULT 'USER'` —— 包含系統動作。
   稽核軌跡上「誰做的」這一欄，現在有一部分是假的。

### C. 營業時間（已定案，038／039 之後改為真的計算）

初版的決定是「一律以自然時間計」，理由是**沒有任何函式能算營業時間內的
經過時間** —— 需要 `facilities.operating_hours` 加上一張假日表，而假日表
不存在。（`operating_hours` 本身是第十個「宣告了沒人讀」：001 建了欄位、
009 種了內容，零個評估點。）

那個決定有一個當時沒有寫下來的代價：`strict` 報表必須整批排除宣告
`business_hours_only` 的政策，而種子的 `SLA_STANDARD`（MEDIUM，多數工單）
就宣告了它。**也就是一份把大多數工單靜默排除掉的合約報表。**

**038 改成真的計算，關鍵是計算的位置**：不是在報表算經過時間，而是在
**算 due 的時候**就把營業時間算進去 ——

    resolution_due_at = 起算時刻 + N 個「營業分鐘」

算出來仍然是一個絕對時刻。於是掃描（033）與報表（034）的比較都不用改
一個字而且是對的，決定 F 的快照語意也保留：之後改班表或補假日，
已開的單不受影響。把它放在報表裡則相反 —— 每個讀取點都要重算一次，
而且班表一改，上個月的報表就變了。

`holiday_calendars` 是新的行事曆表（場域專屬優先於租戶通用），
帶 `is_working_day` 表達補班日。**補班日必須自帶時段**：台灣的補班日是
週六而多數辦公場域只排週一至五，若沿用「那個星期的班表」就會得到空的
時段 —— 那個欄位是驗算時逼出來的，不是選配。

**剩下的缺口**：政策要求營業時間但場域沒有定義班表。那時期限退回自然時間
（有目標比沒目標好）並在 `work_orders.sla_basis` 標記 `NATURAL_FALLBACK`。
`strict` 排除它、`operational` 納入並計數 —— 因此 `strictness` 保留它的
意義與那唯一一個行為差異，只是適用範圍從「所有 business_hours_only 的
政策」縮到「設定不完整的場域」。

**仍是牆鐘時間，但已標示**：報表的 `avg_response_minutes` 與
`avg_resolution_minutes` 不是營業分鐘。達成率（那個要拿去談合約的數字）
已經正確，平均值則會把夜間與週末算進去 —— 一張週五晚上開、週一上午修好的
工單「達成，平均 2296 分鐘」。

因此回應帶 `meta.minutes_basis = "WALLCLOCK"`。**沒有算營業分鐘**，理由是
那需要 `add_business_minutes` 的反函式（規模與 038 那支相當），而達成率
已經對了。真的要算的時候，那個欄位會需要下沉到每一列 ——
一個分組可能同時有 24/7 與營業時間的政策，混在一起平均是蘋果加橘子。

標籤住在 Rust、計算住在 SQL，因此
`sla_report_slice::the_averages_are_wallclock_and_labelled_as_such`
斷言平均值**精確等於**牆鐘差 —— 把兩個檔案釘在一起。少了它，日後改了單位
而忘了改標籤，回應會自稱牆鐘而其實不是。

### D. 等待狀態不停錶（已定案）

`WAITING` 類別有四個狀態（`ON_HOLD`／`PENDING_APPROVAL`／`WAITING_PARTS`／
`WAITING_VENDOR`）。停錶比較「公平」，但需要逐段累加，而且會引出新的問題
（等客戶回覆算誰的？等料是採購的責任還是維修的？）。

第一階段不停錶，報表中一併輸出各工單處於 `WAITING` 的總時長，
讓看報表的人自己判斷。**先給事實，不先給判斷。**

### E. 重開的工單視為新的量測（已定案）

`work_orders.reopened_count` 存在。重開之後：解決時鐘從重開那一刻重新起算，
而**原本那一次的達成與否保留**。理由是「第一次有沒有準時修好」與
「重開後有沒有準時修好」是兩個不同的事實，合併會讓兩者都看不見。

### F. 開單時解析並**快照** `sla_policy_id`（已定案）

`sla_policies.applies_to_priority` 已經存在（種子：`CRITICAL`→15/120、
`HIGH`→15/60、`MEDIUM`→60/480），但沒有任何程式碼用它。

建議：建立工單時依 `(facility_id, priority)` 解析出 policy，
**把 id 與當下的分鐘數一起寫進工單**（`response_due_at`／`resolution_due_at`
是絕對時刻，本身就是快照）。

**快照而非即時查表**，理由是合約報表不能回溯改變：今天調整 policy 的分鐘數，
不該讓上個月的達成率跟著變。

解析不到 policy 的工單 → `sla_state = 'NOT_APPLICABLE'`，且**不進分母**。
種子只覆蓋三種 priority，`LOW`／`URGENT` 目前解析不到 —— 那是目錄的缺口，
報表要能說出「有多少工單因此被排除」，而不是假裝它們達成了。

### G. 分母：排除終止狀態與草稿；PM 工單只計解決不計回應（已定案）

* 排除 `CANCELLED`／`REJECTED`（`TERMINAL` 類別）與 `DRAFT`：
  沒有做完不代表逾期。
* 排除 `sla_state = 'NOT_APPLICABLE'`（見 F）。
* **`source = 'PM_PLAN'` 的工單不計入回應指標**：它們是排程產生的，
  「多久有人回應」對一張三個月前就排好的保養單沒有意義。但**仍計入解決指標**
  —— 準不準時做完是有意義的。

回應與解決因此有**不同的分母**，報表必須各自輸出分母，
否則兩個百分比看起來可比、實際不可比。

## 嚴格度是參數，不是第二份實作

需求是「合約用」與「內部監控用」兩種，但**不做成兩支計算** ——
那會變成兩份真實來源，最後出現「兩個數字都對但不一樣」。

一份計算加一個 `strictness` 參數：

| | `strict`（合約） | `operational`（內部） |
|---|---|---|
| `business_hours_only` 的 policy | 排除，計入 `excluded_business_hours` | 以自然時間計，計入 `substituted_business_hours` |
| 解析不到 policy | 排除 | 排除（兩者相同） |
| 尚不可判定 | 排除 | 排除（兩者相同） |

兩種模式都**必須**在回應中輸出 `excluded_*` 與 `substituted_*` 的數量與原因。
一個沒有附上排除數的達成率，無法判斷它是不是被挑選過的。

> **本表原本有第三列**：「缺回應時刻的工單 → strict 排除、operational 以首次
> 任何 transition 代替」。那一列是在「`first_responded_at` 不存在」這個錯誤
> 前提下寫的（見「修正」）。欄位存在且維護正確之後，「沒有回應時刻」只有
> 兩種意思：**還沒到期限**（兩種模式都排除，計入 `excluded_in_flight`）或
> **過期未回應**（兩種模式都算逾期）。
>
> 把後者排除會是實打實地美化數字 —— 一張沒有人看過而且已經逾時的工單，
> 正是這份報表最該抓到的東西。因此那一列刪除，`strictness` 只剩一個
> 行為差異。

### H. 逾期自動升級（後續決定，2026-08-01）

033 原本刻意只標 `sla_state`，把「要不要自動改工單狀態」留作產品決定。
決定是**要**，由 migration 035 實作。

但目錄的形狀限制了覆蓋範圍。`BREACH_SLA` 只有兩條規則
（`ASSIGNED`／`IN_PROGRESS` → `SLA_BREACHED`），而 035 不擴充它：

| 未覆蓋的狀態 | 為什麼不補規則 |
|---|---|
| `SUBMITTED`（沒有人接手） | `SLA_BREACHED` 出去只有 `CANCEL`／`COMPLETE`／`RESUME`，**沒有 `ASSIGN`** —— 補了會把工單困死。要覆蓋得同時補 `ASSIGN: SLA_BREACHED → ASSIGNED`，那是工作流程改動 |
| `WAITING` 的四個狀態 | 改成 `SLA_BREACHED` 會抹掉「為什麼卡住」，而那是決定 D 要讓人看見的資訊 |

**第一列是這個決定最不舒服的地方**：最該升級的一類（沒有人看過的逾期工單）
是唯一升不了的。它仍然被標記、仍然進報表分母，也仍然出現在 worker 的
`warn` log 與回應的 `not_escalatable` 裡 —— 但不會有狀態變更。

**036 讓那件事變成設定而不是改程式**：在目錄補
`SUBMITTED → SLA_BREACHED`（以及配套的 `ASSIGN: SLA_BREACHED → ASSIGNED`）
就會生效。035 原本把可升級的狀態寫死在迴圈裡，還加了一條自我驗證去擋
目錄新增規則 —— 那條驗證等於**禁止管理者設定**，036 把它刪掉了。

同一輪也修掉了預警門檻：033 用一個全域的 `0.8` 蓋掉了三個 policy 各自
宣告的 `escalation_rules.at_pct`（SLA_CRITICAL 是 75、SLA_CLEANING 明確
表示不要預警）。那是第九個「宣告了沒人讀」，而且是最不該由程式代為決定的
一類 —— **可以讓管理者定義的條件不要寫死。**

順帶記錄第八個「宣告了沒人讀」：那兩條規則的 `notify` 欄位沒有任何消費者
（全 repo 零個 `INSERT INTO fms.notifications`），因此升級目前**不會通知
任何人**。事件進 `event_outbox` 後被 relay 標成 `SKIPPED`。

## 要建的四段鏈（報表是最後一步）

1. **開單時解析 policy 並算出 due**（F、A）—— migration 032
2. **修正回應時刻的寫入條件**（B）—— migration 032，不新增欄位
3. **逾期標記**（`sla_state`）—— migration 033 + `fms-worker`，因為逾期是
   「時間到了而某事沒發生」，沒有觸發點（與 no-show 掃描同一個形狀）
4. **報表**（`GET /reports/sla-compliance`）

前三段會**寫進歷史資料**，因此本文件核准後才動工。第 4 段是純讀取，
規則改了重算即可。

第 1 段做成 `work_orders` 的 **BEFORE INSERT 觸發器**而不是改
`POST /work-orders`：開單路徑有七種（`source` 的 CHECK 列了
`MANUAL`／`PM_PLAN`／`IOT_ALARM`／`RESERVATION`／`API`／`IMPORT`／
`INSPECTION_FINDING`），逐一去改就是逐一有機會漏。這也是 ADR-09 紀律 2
（判斷交給資料庫）的直接應用。

## 已知會被這份決定影響的既有缺陷

**(1) `MET` 判定恆真。** 004 的判定在 `resolution_due_at` 為 NULL 時
無條件成立。第 1 段完成後大部分工單會有 due，但**解析不到 policy 的仍然是
NULL**，於是它們會繼續被標成 `MET`。032 一併修掉：`resolution_due_at IS NULL`
導向 `NOT_APPLICABLE`，不是 `MET`。

**(2) `AUTO_ASSIGN` 被當成人為回應。** 見決定 B。

**(3) `transitions.actor_type` 全部是 `USER`。** 見決定 B 第 2 點。
這一項的影響超出 SLA —— 它是稽核軌跡的正確性問題。

**(4) `side_effects.compute_sla` 宣告了但沒有人讀。** 四個動作
（`ASSIGN`×2／`ACCEPT`／`AUTO_ASSIGN`）都帶 `"compute_sla": true`，
零個讀取點。它們全都是進入 `ASSIGNED` 的轉移，因此原意應該是
「此刻評估回應 SLA」。032 讓它有意義：在這些轉移上比對
`first_responded_at` 與 `response_due_at`，逾時則標 `RESPONSE_BREACHED`。

## 修正（實作時發現）

本文件初版寫「『實際回應時刻』沒有這個欄位」。**那是錯的** ——
`work_orders.first_responded_at` 存在，且狀態機已經在維護它。
我當時查 `work_orders` 的欄位時列舉了關鍵字，而 `first_responded_at`
不在我列舉的關鍵字裡。

決定 B 的**語意不變**（回應 = 有人接下工單），但實作方式反轉了：
不是「新建一套導出邏輯」，而是「修正既有欄位的寫入條件」。
而那個要修的條件，正是決定 B 當初被寫下來要防的失效模式。

這是同一個形態的第七次：**宣告了、有人讀、但寫的人搞錯了條件**，
以及**宣告了、沒有人讀**（`compute_sla`、`actor`）。
前六次記在 `docs/security-review-open-items.md` 與 028／030 的檔頭。
