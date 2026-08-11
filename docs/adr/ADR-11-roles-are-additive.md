# ADR-11：角色是純加法，不引入 deny

| 項目 | 內容 |
|---|---|
| 日期 | 2026-08-01 |
| 狀態 | 已決定 |
| 觸發原因 | `docs/security-review-open-items.md` 第 3 項：混合 `read` 與 `read_own` 時範圍取聯集，結果是 All |
| 相關 | ADR-09（實作紀律 2：判定委派資料庫）、016（權限判定的唯一權威）、026（`min_scope_level` 的執行） |

## 決定

**多個角色的權限取聯集。系統不提供任何「拒絕」語意。**

一個使用者的有效權限 = 他所有有效角色指派所帶的權限的聯集，再由
`min_scope_level`（026）依授權範圍過濾。沒有任何機制能「扣掉」已由其他角色
授予的權限。

## 背景：第 3 項的原始描述

> 使用者同時具備 `x.read_own` 與某個含 `x.read` 的角色時，範圍計算取聯集，
> 結果是 All。多數情況下這是對的（明確授予的較寬權限應該生效），但
> `read_own` 若是刻意的降級授權，聯集就違反意圖。
>
> 修法取決於一個語意決定：**角色是加法還是可以減法**。目前全系統假設加法。

`fms-shared/src/scope.rs` 的 `read_scope` 先看完整權限，有就回 `All`：

```rust
if codes.contains(full) { return Ok(ReadScope::All); }
if codes.contains(own)  { return Ok(ReadScope::Own(user_id)); }
```

## 查證：那個「刻意的降級授權」在現行目錄裡不存在

權限目錄裡只有兩個 `_own`：`work_order:read_own` 與 `reservation:read_own`。
實際分布（008／011 的角色對應）：

| 角色 | 持有 |
|---|---|
| REQUESTER、TECHNICIAN、SERVICE_STAFF | **只有** `_own` |
| PLATFORM_ADMIN、TENANT_ADMIN、VIEWER | `read` **與** `_own` 兩者 |
| DISPATCHER、FACILITY_ADMIN、ORG_MANAGER、MAINTENANCE_SUPERVISOR | 只有 `read` |

**單一角色內同時持有兩者的都是管理／檢視角色**，對它們取聯集得到 `All`
正是意圖。第 3 項描述的情境需要跨角色組合，且要求管理員的意圖是
「窄的壓制寬的」—— 現行目錄裡沒有任何一個這樣的組合。

也就是說：這不是一個正在發生的問題，而是一個假設性的需求。

## 為什麼選加法

**1. 這是使用者對 RBAC 的預設心智模型。**
Kubernetes RBAC 沒有 deny；PostgreSQL 自己的 `GRANT` 只有授予（`REVOKE`
是撤銷一筆既有授予，不是一條優先於其他授予的規則）。「多給一個角色反而
看得更少」會製造難以除錯的行為：使用者回報「我看不到工單」時，
要跨他所有角色推理才能回答為什麼。

**2. deny 不是一個小改動，是一整套語意。**
引入它就必須同時回答：deny 是否一律優先？deny 的範圍怎麼算（在場域 A
deny 是否影響租戶級查詢）？更高層級的授予能不能 override 較低層級的 deny？
兩個角色一個 deny 一個 allow 誰贏？每一個答案都會變成新的邊界情況，
而它們全部落在授權路徑上 —— 這個系統最不該有隱晦行為的地方。

**3. 真實需求有更簡單的表達方式。**
「某人調查期間只能看自己的工單」不需要 deny：把那個較寬的角色指派
以 `user_role_assignments.valid_until` 收掉即可。撤銷授予比疊加一條
拒絕規則容易理解，也留下可稽核的時間軸。

**4. 加法讓判定維持單一來源。**
016 的整個用意是「範圍述詞只存在一份」，026 又把層級判定加在同一個視圖上。
deny 必須在聯集**之後**再做一次減法，那是第二個判定階段，
而兩階段判定遲早會與只看第一階段的呼叫端不一致。

## 後果

* `read_scope` 的行為維持不變：`read` 勝過 `read_own`。
* 要收窄一個人的可見範圍，正確操作是**不要給他那個較寬的角色**，
  而不是額外給他一個較窄的。
* 若日後真的出現需要降級的業務需求，正確做法是**拆角色**
  （建立一個不含 `read` 的變體），而不是引入 deny。
* 第 3 項因此關閉，不留待辦。

## 一併修正的文件錯誤

`scope.rs` 的 `read_scope` 註解原本舉 `DISPATCHER` 當「同時擁有 read 與
read_own」的例子。實測 `DISPATCHER` 只有 `work_order:read`。
結論對，例子錯，已改為 `TENANT_ADMIN`／`VIEWER`。
