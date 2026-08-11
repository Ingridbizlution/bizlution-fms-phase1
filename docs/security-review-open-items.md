# 安全審查發現項與處置紀錄

2026-08-01，由一個未參與實作的獨立 agent 執行。三個已修（見 commit 29067ad），
四項**確認存在但當時刻意未動**，因為修法會牽動既有語意，值得單獨決定而不是順手改掉。

**四項全部已於 2026-08-01 處理完畢。** 這份文件從「待辦清單」變成「決定紀錄」——
保留每一項的原始描述與最後做了什麼決定，因為那些決定的**理由**不會出現在
程式碼的 diff 裡。

租戶隔離本身在攻擊測試下成立：沒有找到任何跨租戶讀寫路徑。四項都是租戶**內部**
的權限邊界問題。

| 項 | 主題 | 待決定的問題 | 決定 |
|---|---|---|---|
| 1 | 冪等重放跳過授權、不綁使用者 | 舊鍵的相容處理 | 刪除，不回填 |
| 2 | 場域範圍的管理員可建立租戶級物件 | 租戶級物件該由誰建立 | **決定早就做完了**，缺的是執行 |
| 3 | 混合 `read` / `read_own` 放寬成 All | 角色是加法還是可減法 | 純加法（ADR-11） |
| 4 | 登入無節流、`auth_events` 從未寫入 | 節流放應用層或反向代理 | 兩者都要，本次做應用層 |

---

# 已處理

## 1. 冪等重放跳過授權，且不綁使用者（2026-08-01 修畢）

原始描述：`Idempotency-Key` 命中時直接回放存下的回應，不重跑授權檢查，
鍵也不含 user_id。同租戶內另一個使用者若取得（或猜到）別人的鍵，
會拿到不屬於他的回應內容，即使他本來無權執行該操作。

當時待決定的是**舊鍵的相容處理**。**決定：刪除，不回填。**
既有列從未記下 user_id，因此不可能歸屬到任何人；而這張表本來就是
24 小時的暫存（`expires_at` + `idx_idempotency_keys_expiry`），
內容依設計可丟棄。代價（部署後 24 小時內，重送部署前的鍵會被當成新請求
而真的再執行一次）寫在 migration 檔頭，因為那是部署時要知道的事。

### 做了什麼

| 項目 | 位置 |
|---|---|
| migration 025：`user_id` 納入主鍵 | `sql/025_idempotency_keys_per_user.sql` |
| 鍵的查詢／登記／完成三處都帶 `user_id`（取自 `TenantTx` 情境） | `fms-shared/src/concurrency.rs` |
| `PendingReplay`：回放內容必須交出 `Authorized` 才能取出 | 同上 |
| `Authorized`：只有 `require_permission` 產得出來的零大小憑證 | `fms-shared/src/db.rs` |
| 五個冪等端點改為「登記 → 授權 → 回放」 | reservation（2）、asset（2）、work_order、maintenance |
| 端到端測試（兩項都做過突變驗證） | `fms-server/tests/idempotency_slice.rs` |

三個判斷值得記下來：

**`user_id` 進主鍵，不是只多存一欄。** 若主鍵不變、只多一欄比對，兩個使用者
在同一端點用同一個鍵字串時，第二個人會撞到既有列，只能回 422
`IDEMPOTENCY_KEY_REUSED`。那既是一個弱預言機（「這個鍵有人用過」本身是資訊），
也會拒絕一個完全無辜的使用者。納入主鍵之後兩列並存，彼此無關。

**呼叫端不傳 `user_id`。** 它從 `TenantTx` 的情境取，也就是 `require_auth`
交叉驗證 JWT 之後的值。這讓「綁錯使用者」在結構上不可能發生，
也讓五個呼叫端的簽章一行都不用改。

**回放的授權由型別強制，不靠註解。** 光是 025 就已經擋掉「別人的鍵」
（查不到列就沒有回放）。剩下的是同一個使用者在 24 小時窗內權限被撤銷後重送 ——
洩漏的是他本來就看過的內容，嚴重度低，但成本也低：`permission_codes`
有請求層級的記憶，handler 已做過的判定不會再往返資料庫。
`PendingReplay::release` 要求一個 `Authorized`，而 `Authorized` 的欄位是私有的，
因此**在 safe Rust 裡寫不出「先回放再授權」**（驗證這一點時，
突變測試必須用 `mem::transmute` 偽造憑證才能重現舊行為）。

`Authorized` 證明「本次請求跑過一次授權判定」，但不證明檢查的是哪一個權限碼。
要那樣就得把權限碼帶進型別，而權限碼是資料庫裡的資料（016 的目錄）不是
Rust 的列舉。對這個用途夠了 —— 先前的問題是**一次檢查都沒有**。

### 順帶修掉的兩件事

1. `complete()` 的 `UPDATE` 影響 0 列時原本靜默通過。鍵條件一旦與 `begin()`
   漂移（這次加 `user_id` 就是一次），症狀會是「冪等鍵看起來沒作用」而毫無錯誤。
   現在回 500 —— 那是我們的 bug，不該由客戶端以重複執行的形式承擔。
2. `make migrate-roundtrip` 的快照原本只比對表、函式、政策，**看不到欄位與約束**
   —— 也就是說 025 的 down 無論寫得對不對，roundtrip 都會通過。
   已加入欄位（名稱／型別／可空性）與約束定義。

## 2. 場域範圍的管理員可建立租戶級物件（2026-08-01 修畢）

原始描述：`facility_scope` 限制的是「能碰哪些場域的資料」，但有些物件（分類、
範本、政策）沒有 `facility_id`，對它們而言場域限制不存在 —— 限制目前靠 handler
慣例維持，不是靠 schema。當時的判斷是「這是設計缺口，需要先決定租戶級物件
該由誰建立」。

**查證後的結論不同：那個決定早就做完了，缺的是執行。**

002 給 `fms.permissions` 加了 `min_scope_level`（CHECK 限定
TENANT／ORG／FACILITY／SPATIAL_NODE），008 與 011 為每一項權限都填了值 ——
20 項宣告 TENANT。同時 `fms.roles` 有 `scope_level`、`fms.permissions` 有
`is_dangerous`。三個欄位在全 `sql/` 與全 `app/crates/` 的引用**只有「定義它」
與「填它」，沒有任何一處讀它**。

這與 022 是同一型缺陷：`work_order_transitions_allowed.required_permission`
宣告在表上、`transition_work_order` 完全忽略它。

### 實測的可達性

以 `fm.lin`（FACILITY_ADMIN，範圍只有總部一個場域）：

| 動作 | 結果 |
|---|---|
| `POST /facilities` | 403 |
| 加派 `TENANT_ADMIN` 但範圍仍限單一場域 → `POST /organizations` | **201 成功** |
| 同上身分 → `POST /facilities` | 403 |

兩個 403 **不是權限檢查給的** —— 是 007 的 `facility_scope` RESTRICTIVE 政策：
新場域的 id 不在交易開始時取的可見快照裡，`create_facility` 重算後仍讀不回來
就回滾。行為對，理由不對。

201 那條之所以成功，是因為 `organizations` 沒有 `facility_id`，也就沒有那條政策。
**目前唯一擋住租戶級建立的東西，是「那張表剛好有 facility_scope 政策」這個巧合。**
57 張無 `facility_id` 的表裡，`organizations` 只是第一個有 POST 端點的
（原始描述說 24 張，實際 57 張；差額多半是「透過父表的 FK 繼承場域範圍」的子表，
那些不是問題）。

### 決定與做法

**判定從嚴，例外寫在宣告資料裡。**

| 項目 | 位置 |
|---|---|
| migration 026：`scope_width()` + 視圖層級述詞 + 修正四格宣告 | `sql/026_enforce_min_scope_level.sql` |
| migration 027：`facility:write` 拆成 `facility:create`（ORG）／`facility:update`（FACILITY） | `sql/027_split_facility_write.sql` |
| `create_org` / `create_facility` 的範圍判定 | `fms-tenancy/src/handlers.rs` |
| `require_tenant_scoped_permission` | `fms-shared/src/db.rs` |
| 端到端測試（含突變驗證） | `fms-server/tests/rbac_scope_slice.rs` |

**述詞加在 `v_user_effective_permissions` 裡**，因為它已經 JOIN 了
`fms.permissions`。一條 `scope_width(ura.scope_type) >= scope_width(p.min_scope_level)`
就自動傳播到全部四個消費者：`user_permission_codes`、`..._anywhere`、
`user_has_permission`、以及 `/auth/me` 的權限清單。最後一項不是附帶效果而是
必要條件 —— 只收斂函式不收斂視圖，`/auth/me` 會宣告一組實際上用不了的權限，
而 012 的 T12 正是在交叉比對這兩者。

`scope_width()` 對未知層級回 NULL，使比較失敗而非通過：日後有人加了第五個
層級卻忘記加進來，症狀是「權限失效」而不是「權限一律通過」。

### 開始執行之後才看得出來：四格宣告是錯的

008 是按「這個**資源**住在哪一層」填 `min_scope_level`（organization／user／role／
tenant 是租戶級資源），而不是按「這個**動作**的影響範圍」。在沒有任何東西執行它
的情況下這個區別沒有成本，所以沒人需要小心。

正確語意是後者：**讀一個租戶級資源不是租戶級特權，寫它才是。** 依這條規則檢查
全部 20 項，`:write` 類全部宣告正確，被過度宣告的是 `:read` 類：

| 權限 | 原宣告 | 改為 | 理由 |
|---|---|---|---|
| `organization:write` | TENANT | ORG | 組織是 ltree 階層物件，ORG 經理該能在自己子樹內建子組織 |
| `organization:read` | TENANT | FACILITY | 場域管理員要知道自己屬於哪個組織 |
| `asset_model:read` | TENANT | FACILITY | 共用型錄查詢；007 已把 `asset_models` 列入 `catalog_tables` |
| `user:read` | TENANT | FACILITY | 派工要選人 |

不改這四格會讓 `GET /asset-models` 與 `GET /organizations` 對 FACILITY_ADMIN
回歸成 403 —— 也就是把安全修正做成功能回歸。`work_order_slice.rs` 的
`facility_scoped_roles_can_list_and_only_see_their_facility` 正是在守這個方向。

`role:read`、`tenant:read`、`identity_provider:read`、`integration:read` 目前沒有
端點使用，實際需要多寬還不知道，刻意留 TENANT。

### 為什麼拆 `facility:write`

一個權限碼同時管兩種層級的動作：**建立**場域是租戶／組織級（新場域沒有父場域，
「在哪個場域裡建立一個場域」不成立），**修改**場域是場域級。一個碼只能填一個
`min_scope_level`，無論填什麼都必然對其中一邊是錯的。

拆開後 `FACILITY_ADMIN` 只拿 `facility:update`，那個 403 由權限判定給出，
RLS 退回它該扮演的第二道防線。不是契約變更：`openapi.yaml` 完全沒有提到權限碼。

一個 migration 陷阱：008 給 `PLATFORM_ADMIN`／`TENANT_ADMIN` 的是萬用授權
（「全部」／「除 `user:impersonate` 以外的全部」），**那不會因為後來新增權限碼
而重跑**。027 必須自己補 `role_permissions` 的列，否則新碼會有名字而沒有人持有。

### ORG 範圍不能逃出自己的子樹

`organization:write` 一旦可由 ORG 範圍取得，就必須限制在**哪一棵**子樹，否則
組織經理能建立自己範圍外的組織 —— 那比原本的缺口更難察覺。

子樹內的情況不需要新程式碼：016 的 ORG 分支比對的是
`o_target.org_path <@ o_scope.org_path`，把 parent 當成 `org_id` 傳進
`require_permission` 就正好回答「你的授權範圍涵蓋這個位置嗎」。

**但根組織（`parent_id` 為 None）需要另一支判定。** 原本以為傳 `None` 就會
落到「ORG 分支必然不成立」，實測不是：`require_permission(.., None, None)`
會走 `user_permission_codes_anywhere`（那是 3.9 為了讓場域級角色能用列表端點
刻意做的），完全跳過範圍述詞。因此新增 `require_tenant_scoped_permission`，
它直接呼叫 `fms.user_permission_codes(user, NULL, NULL)` —— 那個組合的述詞
只有 `scope_type = 'TENANT'` 分支可能成立，也就是說「在 TENANT 範圍持有」
這個判定**早就存在於 016**，只是 Rust 這一層從來沒用那個組合呼叫它。
沒有新增任何 SQL。

### 突變驗證揭露的一件事

第一次跑突變（移除視圖述詞、保留宣告）時測試**全部通過**。那是有意義的資訊：
`create_org`／`create_facility` 現在都走 `user_permission_codes` 的範圍述詞，
那本身就足以擋住那兩條路 —— 也就是說**原始漏洞是 handler 的修正關掉的，
不是 026**。

026 守的是不同的東西：所有仍用 `require_permission(.., None, None)` 慣例的端點，
以及 `/auth/me` 宣告的清單。補上一個對 `/auth/me` 的斷言之後，突變就被抓到了，
而失敗輸出本身就是說明 —— 沒有述詞時，一個「範圍限單一場域」的授權會取得
`role:write`、`tenant:update`、`identity_provider:write`、`user:write`、
`role:assign`、`facility:create`，整組 TENANT_ADMIN。

`role:write` + `role:assign` 合起來是提權鏈：鑄造一個含任意權限的角色再指派給
自己。這是這一項最該擋住的組合，也是為什麼不採「TENANT 宣告一律接受 ORG」
的字面做法。

### 留給後續的兩件事

1. ~~**`role:assign` 宣告 ORG，但 008 給了 `FACILITY_ADMIN`。**~~
   **（2026-08-02 已判斷，migration 052）** 那支端點做出來了
   （`POST /users/{id}/role-assignments`），重新判斷的結果是**維持 031 的移除**：
   FACILITY_ADMIN 不拿回 `role:assign`。理由不是「場域管理員不該指派人」，
   而是那個判斷已經有更精確的依據 —— 兩道閘（範圍 + 提權）都是資料驅動的，
   要讓某個角色能指派人，管理員給它 `role:assign` 就好，不必改程式。

   同時發現一個**照契約做就會不一致**的地方並改了契約：
   `GET /users/{id}/role-assignments` 原本只要 `role:read`（宣告 TENANT），
   而 ORG_MANAGER 只有 `role:assign`（宣告 ORG）—— 結果是**指派得了角色卻
   看不到自己指派了什麼**，連撤銷都做不到（要 id，而 id 只能從那支清單拿）。
   改成 `role:read` 或 `role:assign`；今天沒有任何角色因此失去存取。
2. **`ORG_MANAGER` 沒有 `organization:write`。** 008 第 129 行的清單裡只有
   `organization:read`。026 讓 ORG 範圍**有可能**取得 write，但現行目錄裡沒有
   任何 ORG 級角色持有它 —— 因此 ORG 那條路目前只能靠「刻意把租戶級角色指派
   在 ORG 範圍」達成。「ORG_MANAGER 是否該拿到 `organization:write`」是獨立的
   產品決定，刻意不在一個安全收斂的 migration 裡順手擴權。
3. **`roles.scope_level` 仍然無人讀取；`permissions.is_dangerous` 已有讀者。**

   **`is_dangerous`（2026-08-02，migration 052）** 成為角色指派提權防護的依據：
   *你不能授出一項你自己沒有的危險權限*。量出來的判別力 ——
   ORG_MANAGER 可指派 8 個角色，擋掉 TENANT_ADMIN 與 PLATFORM_ADMIN。
   若沒有這道閘，ORG_MANAGER 把 TENANT_ADMIN 指派給自己會多拿 **14 項**權限
   （026 收斂掉了其餘的），含 `asset:delete` 與 `reservation:override`。

   這也關掉了本節上面記的那條鏈（`role:write` + `role:assign` 鑄造角色再指派
   給自己）：鑄出來的角色若含你沒有的危險權限，一樣過不了。

   **`scope_level` 刻意仍然不讀。** 052 評估過拿它當上限，否決的理由是量到的：
   * `IOT_INGEST` 宣告 `TENANT`，四項權限卻**全是** `FACILITY`-min。
     用它當上限會擋掉「只在單一場域收資料的 ingest 帳號」。
   * 現存指派 `PM_GENERATOR[FACILITY] @ TENANT` 已經違反那個上限（019 刻意的）。

   一個語意不一致、現存資料又已經違反的欄位，不適合拿來當授權判定。

## 3. 混合 read / read_own 會放寬成 All（2026-08-01 決定不改）

原始描述：使用者同時具備 `x.read_own` 與某個含 `x.read` 的角色時，範圍計算
取聯集，結果是 All。`read_own` 若是刻意的降級授權，聯集就違反意圖。
待決定的是**角色是加法還是可以減法**。

**決定：純加法，不引入 deny。完整理由見 `docs/adr/ADR-11-roles-are-additive.md`。**

查證的關鍵事實是：**那個「刻意的降級授權」在現行目錄裡不存在。** 只有兩個
`_own` 權限，而單一角色內同時持有 `read` 與 `read_own` 的都是管理／檢視角色
（PLATFORM_ADMIN、TENANT_ADMIN、VIEWER），對它們取聯集正是意圖。
第 3 項描述的情境需要跨角色組合且意圖是「窄的壓制寬的」，現行目錄裡沒有
任何一個這樣的組合 —— 這不是正在發生的問題，是一個假設性需求。

三個不做的理由（摘要）：加法是使用者對 RBAC 的預設心智模型（Kubernetes RBAC、
PostgreSQL 的 `GRANT` 都是）；deny 不是小改動而是一整套語意（優先順序、範圍、
能否被上層 override），每個答案都會變成授權路徑上的新邊界情況；真實需求
（臨時降權）用 `user_role_assignments.valid_until` 撤掉寬角色即可。

順帶修正一個文件錯誤：`scope.rs` 的註解原本舉 `DISPATCHER` 當「同時擁有 read
與 read_own」的例子，實測 `DISPATCHER` 只有 `work_order:read`。結論對，例子錯。

## 4. 登入無節流，且 `auth_events` 從未寫入（2026-08-01 修畢）

原始描述：`/auth/token` 沒有速率限制，暴力嘗試不受阻；`auth_events` 表存在、
schema 完整、但沒有任何程式碼寫入它——失敗登入不留痕跡。另外存在時間側通道：
使用者不存在時比密碼錯誤時回得快，可用來列舉帳號。

當時待決定的是「節流放在應用層或反向代理」。**決定：兩者都要，但這次只做
應用層做得到、而代理做不到的那一半。**

### 做了什麼

| 項目 | 位置 |
|---|---|
| migration 024：讓 `auth_events` 在無租戶情境時可寫入 | `sql/024_auth_event_trail.sql` |
| 登入事件寫入（成功／四種失敗，帶 tenant／user／user_agent） | `fms-identity/src/repo.rs` 的 `record_login_event` |
| 以 `(tenant_code, username)` 為鍵的失敗節流，429 + `Retry-After` | `fms-identity/src/throttle.rs` |
| 四條失敗路徑等時化（假 argon2 驗證） | `fms-identity/src/password.rs` 的 `verify_dummy` |
| 端到端測試（含時間側通道的行為驗證） | `fms-server/tests/auth_hardening_slice.rs` |

三個判斷值得記下來，因為它們都不是唯一解：

**節流的鍵是帳號，不是 IP。** 要防的是「對某個帳號猜密碼」，以帳號為鍵直接
限制的就是這件事，而且換 IP 繞不過。以 IP 為鍵才能擋「同一來源掃描大量帳號」，
但那需要可信的對端位址，而應用層目前拿不到（見下）。兩個維度不是替代關係，
IP 那一半刻意留給反向代理。

**計數失敗、成功歸零，不做帳號鎖定。** 「N 次失敗鎖 M 分鐘」會把暴力破解的
防護變成針對已知帳號的阻斷服務：任何人只要知道 username 就能定時送錯密碼
把該帳號鎖住。窗式計數沒有這個性質。代價是門檻用盡的那段時間裡正確密碼也
會被擋（測試裡明確斷言了這個行為，以免日後被當成 bug）。

**節流狀態在記憶體裡，每個行程一份。** 水平擴充到 N 個實例時實際容許的嘗試
次數是 N 倍。刻意接受：換成 Redis 會讓登入依賴一個新的外部元件，而 Redis
不可用時要在「拒絕所有登入」與「完全不限流」之間選一個，兩個答案都不好。
真的需要全域精確時，該做的是在代理層限流，而不是讓應用層變脆。

### 還沒做的：`ip_address` 一直是 NULL

`auth_events.ip_address` 有欄位，但登入事件寫入時固定留空。

能拿到的只有 `X-Forwarded-For`，而在沒有「可信代理清單」的設定之前，那是
客戶端可任意偽造的字串。**在安全軌裡放一個看起來權威、實際上由攻擊者填寫的
位址比留空更糟**：事後調查會據此追錯對象。`axum::serve` 目前也沒有帶
`ConnectInfo`，即使帶了，在反向代理後面拿到的也是代理的位址。

要補齊需要兩件事，而兩件都是**部署決策**，與這次的修法同一個性質：

1. 確定生產環境的代理層數與位址，據此設定可信代理清單（信任幾層 XFF）
2. `axum::serve` 改用 `into_make_service_with_connect_info`，並讓
   `ConnectInfo` 在測試環境缺席時可降級（`oneshot` 不會帶它）

在那之前，「哪個來源在掃帳號」這個問題要靠代理層的 log 回答，
而不是靠 `auth_events`。

### 順帶發現：`make migrate-roundtrip` 其實不在 CI 裡

`sql/down/README.md` 寫「CI 會執行它 —— 沒有被執行過的 down migration 等於
沒有 down migration」，但 `.github/workflows/data-layer.yml` 沒有這個步驟。
024 的 down 已在本機驗過（up → down → up 後 schema 完全相同），但那個保證
目前不會在 PR 上自動重跑。與本項無關，獨立記錄。
