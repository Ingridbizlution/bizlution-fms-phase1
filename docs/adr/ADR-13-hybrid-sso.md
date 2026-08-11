# ADR-13：混合式 SSO 的認證策略

| 項目 | 內容 |
|---|---|
| 日期 | 2026-08-04 |
| 狀態 | **部分定案** —— A／B／E 已定（內部架構，見各節）；**C／D／F 待客戶回答**，走 `docs/CUSTOMER-IDENTITY-QUESTIONNAIRE.md` |
| 觸發原因 | 「若企業客戶多為混合式 SSO 登入（Windows AD／Entra ID／Azure AD）與傳統帳號密碼登入，是否需做更完善規劃」 |
| 相關 | 002（identity_providers／user_identities）、014（resolve_tenant_by_code）、058（目錄同步）、073（sso_auth_requests）、074（scim_tokens）、ADR-11（角色是加法） |
| 訪談表 | `docs/CUSTOMER-IDENTITY-QUESTIONNAIRE.md` —— C／D／F 三個決定由客戶的 IT 回答，不由我們判斷 |

> **初版把六個決定全部標成「待決定」，那個框架是錯的。**
>
> 六個裡只有**三個真的需要客戶回答**（C 登入路由、D 本地密碼政策、F LDAPS），
> 而那三個問的是客戶的事實與政策 —— 不該由我們判斷，也不該由不熟悉企業身分
> 整合的人來猜。它們現在由 `docs/CUSTOMER-IDENTITY-QUESTIONNAIRE.md` 的十個
> 事實問題機械地決定，客戶的 IT 管理員十分鐘答得出來。
>
> 另外三個（A 密鑰解析器、B callback 完成、E 無群組的人）**本文件直接定案** ——
> A 是內部架構、客戶不在意；B 不是決定而是工作；E 有一個明確安全的預設值。
> 把它們也擺著等人決定只是把責任推給不該承擔的人。
>
> 每一節的「定案」都附一個**重新檢視的觸發條件** —— 預設值不是永久答案，
> 而是「在沒有相反資訊時的正確起點」。

---

## 0. 現況：一個很特別的形狀

以下每一項都是 2026-08-04 實測過的，不是從文件推的。

| 層面 | 狀態 | 證據 |
|---|---|---|
| **佈建**（誰存在、屬於哪些群組） | ✔ 完整 | SCIM 2.0 十支端點（#48），Entra ID 可推使用者與群組 |
| **授權**（能做什麼） | ✔ 完整 | `directory_role_mappings` 群組 → 角色（058），SCIM 填 `user_directory_groups` |
| **認證**（正在登入的是誰） | ✘ **只有本地密碼** | `/auth/sso/{code}/callback` 回 501 |
| LDAP 客戶端 | ✘ 不存在 | `ldap_host`／`ldap_port`／`ldap_base_dn`／`ldap_bind_dn`／`ldap_user_filter`／`ldap_group_filter` 六欄無任何讀者 |
| JIT 佈建 | ✘ 無消費者 | `jit_provisioning`／`jit_default_role_code`／`auto_deprovision` 三旗標無人讀（callback 沒完成） |
| `/auth/token` 的 grant | password + refresh_token 可用；`authorization_code` 與 `client_credentials` **明確回 501** | `handlers.rs:115` |

**這個形狀是：客戶可以從 Entra 把人與角色同步進來，但那些人登不進去。**

`identity_providers.provider_type` 的 CHECK 允許 `OIDC`／`SAML2`／`LDAP`／`LOCAL`
四種，也就是說 schema 早就為四條路留了位置，而只有 `LOCAL` 有實作。

---

## 1. 一個會改變規劃規模的事實

**Azure AD 就是 Entra ID**（Microsoft 2023 年改名），因此客戶說的三種其實是
兩個家族。而「地端 Windows AD」有四條路，成本差一個數量級：

| 路徑 | 需要什麼 | 使用者密碼會不會經過我們 |
|---|---|---|
| **A. Entra Connect 同步 + 雲端認證** | **零額外程式碼**（地端 AD 同步到 Entra，所有人對 Entra 認證） | 不會 |
| **B. ADFS 當 OIDC IdP** | 與 A 同一條 OIDC 路徑 | 不會 |
| **C. LDAPS bind** | 一個 LDAP 客戶端 | **會** —— API 收到明文密碼再去 bind |
| **D. Kerberos／SPNEGO 桌面單一登入** | API 要加入網域或持有 keytab | 不會 |

### 這裡的關鍵判斷

**多數「混合式 AD」的企業已經在跑 Entra Connect** —— 他們的地端帳號早就同步到
雲端了。因此把 **OIDC 那一條做完，就覆蓋了絕大多數客戶**，包含他們的地端使用者。

這比「同時支援四種」的直覺答案便宜得多，而且 **C 有一個政策問題**：許多企業的
資安規範現在禁止第三方系統接觸網域密碼，而 C 正是那樣做。把 C 當主線會在
資安審查時被退。

**建議：A + B 為主線，C 為備援（只在有客戶明確要求時做），D 不做。**

D 不做的理由不是難，是**它需要 API 伺服器加入 Windows 網域**，而那與這個系統
的部署模型（容器、可能在雲端）衝突。若真有需求，正確做法是在前面放一個
IIS／nginx 做 SPNEGO 然後轉成 header，而那是部署架構，不是應用程式。

---

## 2. 決定 A：密鑰管理服務的解析器 —— **卡住一切的那一個**

三個欄位都指向一個不存在的東西（**寫這段時的現況；解析器已於本文件定案後實作，
見下方定案框**）：

| 欄位 | 用途 | 沒有解析器的後果 |
|---|---|---|
| `identity_providers.client_secret_ref` | OIDC token 交換 | **`/callback` 回 501** |
| `identity_providers.ldap_bind_secret_ref` | LDAP bind | `test-connection` 驗不到 bind |
| `identity_providers.scim_token_ref` | （已繞過） | 無 —— 見下 |

### SCIM 那一條為什麼繞得過，而這兩條繞不過

SCIM token 是**入站**憑證：Entra 帶著它來，我們只需判斷「這個值對不對」。
因此 074 只存 SHA-256，明文在發放時回傳一次即不可還原。

`client_secret` 是**出站**的 —— 我們必須持有明文才能送去 token 端點。
LDAP bind 密碼同理。**方向差異決定了繞不過。**

### 選項

| 選項 | 成本 | 代價 |
|---|---|---|
| **A1. 每個 provider 一個環境變數**（`IDP_SECRET_<code>`） | ~1 天 | 新增／輪替 provider 要重啟服務；密鑰在容器的環境裡（`docker inspect` 看得到） |
| **A2. HashiCorp Vault** | 3–5 天 | 多一個要維運的元件；客戶不一定有 |
| **A3. 雲端的 Secrets Manager**（Azure Key Vault／AWS） | 2–4 天 | 綁雲；地端部署的客戶不能用 |
| **A4. 存在資料庫，欄位級加密**（`pgcrypto` + 一把主金鑰） | 2 天 | 主金鑰還是要放某處 —— 把問題推遲一層，但那一層小得多 |

### 定案：A1（每個 provider 一個環境變數），解析器做成 trait

> **已實作。** `fms-shared/src/secrets.rs` 的 `SecretResolver` trait +
> `EnvSecretResolver`（前綴 `IDP_SECRET_`）。組裝點在 `build_router` 的參數 ——
> 換成 Vault／Key Vault 只動那一行。部署說明見 `docker/README.md`。
>
> 目前唯一的消費者是 `test-connection` 的 `secret_reference_resolvable`：
> **它讓「參照設了、部署忘了給密鑰」第一次變成可觀察的**。
> 這件事在此之前要等到有人試著登入才會炸，而症狀出現在 IdP 那一側。
>
> 這**不會**讓 SSO 登入變成可用 —— `/callback` 仍然回 501，剩下的缺口是
> 決定 B 的第 2–4 步（token 交換與 id_token 的 JWKS 驗證）。
> 有了密鑰只是把兩個缺口減成一個。

**重新檢視的觸發條件**：客戶在訪談表 D2 答了 Key Vault／Vault／Secrets Manager，
**且**他們要求接自己現有的那一套。那時只是換 trait 的實作，不動呼叫端。

理由：Phase 1 的部署規模是單一租戶的示範與試點，provider 數量是個位數，
而「重啟才能輪替」在那個規模是可接受的。**但這個決定要寫進部署文件**，
否則第一次要輪替密鑰時沒有人知道要重啟。

不建議 A4：欄位級加密看起來像進步，實際上把主金鑰的問題留在原地，
而且會讓 `client_secret_ref` 這個「參照」的語意變成「密文」——
那是一個沉默的語意改變，下一個人會誤讀。

---

## 3. 決定 B：OIDC callback 要完成到哪裡

`/callback` 目前完成 state 的驗證與一次性消耗（CSRF 與重放防護），然後停在
token 交換之前。剩下的四步：

1. **用授權碼 + `client_secret` + PKCE verifier 換 token** ← 需要決定 A
2. **驗 `id_token` 的簽章**（抓 IdP 的 JWKS，比對 `kid`）
3. **驗 claims**：`iss`／`aud`／`exp`／`nonce`（`nonce` 已存在 073 的列裡）
4. **JIT 佈建或連結既有使用者** ← 需要決定 D 與 E

### 一件不能省的事

**第 2 步沒有做完就不能發我們自己的 token。** 一支核發身分卻沒有驗證
`id_token` 簽章的 callback，是這個系統裡最危險的程式碼 —— 任何人自己組一個
`id_token` 就能變成任何人。這也是為什麼現在是 501 而不是「先讓它動」。

JWKS 的抓取要過 `safe_http` 的 SSRF 閘門（與 discovery 同一道），並且**要快取**
——每次登入都抓一次 JWKS 會讓 IdP 對我們限流。快取要以 `kid` 失效
（IdP 輪替金鑰時 `kid` 會變），不是以時間。

### 定案：四步全做，不分階段交付

**B 不是一個決定，是一件工作。** 四個步驟沒有任何一步可以省 ——
省掉第 2 步（驗簽章）的話這支端點會變成整個系統最危險的程式碼，
而省掉第 1 步就沒有 token 可驗。因此它只有「做完」與「還是 501」兩個狀態。

**重新檢視的觸發條件**：無。

### 工作量

決定 A 完成之後 **2–3 天**，含測試。

**沒有辦法端到端測試** —— 沒有可對接的 IdP 就沒有真的 `id_token`。
可行的替代是在測試裡自建一個 JWKS 端點 + 自簽的 `id_token`，那能驗到我們的
驗證邏輯，但驗不到「與真實 Entra 的互通」。這個邊界要寫在 PR 裡。

---

## 4. 決定 C：多 IdP 的登入路由

`GET /auth/sso/{providerCode}/authorize` 目前**要求 `?tenant_code=`**
（002 的唯一鍵是 `(tenant_id, lower(code))`，provider code 只在租戶內唯一）。

問題是：**使用者在登入頁怎麼被導到正確的 IdP？**

| 選項 | 使用者體驗 | 代價 |
|---|---|---|
| **C1. 明確選擇器**（下拉選單） | 「請選擇你的公司」—— 醜，但零猜測 | 需要一支「列出可用 IdP」的公開端點（目前沒有，而且它會洩漏租戶清單） |
| **C2. Email 網域判別**（home realm discovery） | 輸入 email → 自動導到對應 IdP。**這是企業使用者預期的行為** | 需要「網域 → provider」的對應表（新 migration）；還要處理一個網域對多個租戶 |
| **C3. 租戶子網域**（`acme.fms.example.com`） | 最乾淨 —— 網址本身就帶著租戶 | 需要 wildcard DNS + 憑證；`PUBLIC_BASE_URL` 要變成 per-tenant |
| **C4. 前端設定檔寫死** | 零後端成本 | 每個客戶一份前端建置 |

**這個決定必須在做前端登入頁之前定**，否則登入頁要重做。

**建議 C2**，並保留 C1 當退路（同一個網域對到多個租戶時要人選）。
理由：企業使用者已經習慣「輸入公司 email 就跳到自家的登入畫面」，
而 C3 的 DNS 與憑證成本在 Phase 1 不划算。

C2 需要一個新表 `identity_provider_domains (tenant_id, provider_id, domain)`，
而 `domain` 要 case-insensitive 唯一 —— **跨租戶唯一**，否則兩個租戶宣稱同一個
網域時就回到猜測。

---

## 5. 決定 D：本地帳號與 SSO 帳號的共存規則

**目前結構上完全允許一個人同時有兩者**：`user_identities` 允許一個使用者掛多個
外部身分，而 `users.password_hash` 與它們獨立。

而 `fms.users` **沒有任何「僅限 SSO」的旗標** —— 實測欄位清單裡只有
`password_hash`／`password_updated_at`／`must_change_password`。

### 這是一個真的洞

一個由 SCIM 從 Entra 佈建進來的使用者，今天**可以**被設一個本地密碼
（`POST /users` 或直接 SQL），然後用 password grant 登入 ——
**完全繞過企業的 SSO 政策、MFA 與條件式存取**。

企業客戶的資安審查一定會問這件事。

### 選項

| 選項 | 後果 |
|---|---|
| **D1. 租戶級政策**（`tenants.settings.local_password_policy`：`ALLOWED` / `BREAK_GLASS_ONLY` / `FORBIDDEN`） | 客戶自己決定。`tenants.settings` 已經是 jsonb 且有 `tenant_settings_are_valid()` 驗證函式（070），加一個鍵的成本很低 |
| **D2. 使用者級旗標**（`users.local_login_disabled`） | 更細，但誰來設？若由 SCIM 設，那是 Entra 的 schema 沒有的欄位 |
| **D3. 由來源推導**：有 `user_identities` 列的人就不能用密碼 | 零設定，但**擋掉了緊急存取**——IdP 掛掉時沒有人進得去 |
| **D4. 什麼都不做** | 現況。洞留著 |

**建議 D1 + 一個明確的 break-glass 帳號概念。** `BREAK_GLASS_ONLY` 的意思是：
只有被明確標記的少數帳號能用密碼登入，而**那些帳號的每一次登入都寫一筆
高嚴重度的稽核**。理由：IdP 全域故障時必須有人能進系統改設定，而
「完全禁止」在那一刻會變成災難。

`tenant_settings_are_valid()` 目前認得 `password_min_length` 與
`satisfaction_editable_days` 兩個鍵，加第三個鍵是一個小 migration。

---

## 6. 決定 E：沒有對應到任何群組的人怎麼辦

`jit_default_role_code` 存在（002）但**沒有消費者**。SSO 登入成功而使用者不屬於
任何有對應的群組時，三種處置在稽核上完全不同：

| 選項 | 後果 |
|---|---|
| **E1. 給 `jit_default_role_code`（通常是 VIEWER）** | 人進得來但看不到什麼。**風險**：一個離職但還在 AD 裡的人仍然能登入 |
| **E2. 建帳號、零權限、要求管理員指派** | 最安全，但使用者第一次登入看到空白畫面 —— 需要一個「等待指派」的畫面 |
| **E3. 拒絕登入** | 最嚴格。**風險**：AD 群組設定錯誤時所有人都進不來，而錯誤訊息在我們這一側 |

### 定案：E2 為預設（建帳號、零權限），且是租戶設定

`jit_default_role_code` 預設留空 —— 空值的語意就是 E2。客戶若在訪談表 C2 選了
「給預設角色」，那是填一個值的設定變更，不是改程式碼。

**無論設成哪個，都寫一筆稽核。** 「某人以 SSO 登入但沒有匹配任何群組」是管理員
需要知道的事實，而不是一個靜默的空畫面 —— 沒有那筆稽核，管理員要等到那個人
來抱怨才會知道。

**重新檢視的觸發條件**：訪談表 C2 的答案（三個選項直接對應 E1／E2／E3）。

`auto_deprovision` 是另一半：AD 裡消失的人要不要自動停用？
SCIM 的 `DELETE /Users/{id}` 已經處理了「Entra 主動移除」的情況（#48），
因此 `auto_deprovision` 只在 **LDAP 輪詢同步**的路徑上才需要 —— 也就是決定 F。

---

## 7. 決定 F：要不要做 LDAPS bind

只在有客戶**沒有 Entra 租戶**時才需要。

| 需要做的 | 工作量 |
|---|---|
| LDAP 客戶端（`ldap3` crate）+ TLS | 2 天 |
| bind 驗證接進 `/auth/token`（新 grant 或 provider 判別） | 1 天 |
| 群組成員抓取（讓 `POST /identity-providers/{id}/sync` 真的有東西可抓） | 2 天 |
| `test-connection` 補上真的 bind 驗證 | 0.5 天 |

**合計 3–5 天，而且前提是決定 A 完成**（bind 密碼要能解析出來）。

### 一個必須先講清楚的政策問題

C 路徑會讓 **API 收到使用者的網域明文密碼**。那意味著：

* API 伺服器進入了網域密碼的信任邊界 —— 一次記憶體 dump 就是一批網域帳號
* 日誌絕對不能記請求體（目前 `TraceLayer` 不記，但那是預設值不是保證）
* 客戶的資安規範可能直接禁止

**建議：把這一項標成「客戶明確要求且書面確認風險」才做。**

---

## 8. 建議的順序

| # | 決定 | 誰決定 | 為什麼要先 | 工作量 |
|---|---|---|---|---|
| 1 | **A：密鑰解析器** | **已定**（A1） | 卡住 callback、LDAP bind 全部 | 約 1 天 |
| 2 | **C：登入路由** | **客戶**（訪談表 A3） | **會決定前端登入頁怎麼設計** | 半天（多網域 +1 個 migration） |
| 3 | **D：本地密碼政策** | **客戶**（訪談表 B1／B3） | 資安審查的必問題；影響 callback 要不要擋密碼登入 | 半天 + 一個 migration |
| 4 | **E：無群組的人** | **已定**（E2 預設，可設定） | 併入 callback 實作 | 併入下一項 |
| 5 | **B：callback 完成** | **不是決定** | 這是「SSO 能用」的定義 | 2–3 天 |
| 6 | **F：LDAPS** | **客戶**（訪談表 A1／A2／B1） | 只在客戶沒有 Entra 租戶且政策允許時 | 3–5 天 |

**待回答的只有三項（2、3、6），而三項都由訪談表機械地決定** ——
沒有一項需要我們或你判斷企業身分整合的最佳實務。

第 1 與第 4 已定案，第 5 是工作。因此**訪談表填完的那一刻，整個規劃就完整了**。

若只做 1、2、3、5（跳過 F），**約一週可以讓「Entra ID 登入」端到端可用**，
而那覆蓋了絕大多數混合式 AD 的客戶。

---

## 9. 這份文件沒有涵蓋的

* **SAML2** —— `provider_type` 的 CHECK 允許它，但沒有任何實作，而且它需要
  XML 簽章驗證（一個獨立的、歷史上出過很多漏洞的領域）。若客戶只有 ADFS
  且不能升級到 OIDC 才需要 —— 現代 ADFS 都支援 OIDC。
* **MFA** —— `users.mfa_enabled` 存在但無消費者。走 SSO 的話 MFA 是 IdP 的事
  （條件式存取），我們不該重做一份。只有本地密碼路徑才需要，而那與決定 D
  是同一個議題。
* **SCIM 之外的佈建**（LDAP 輪詢）—— 見決定 F。
* **登出的傳播**（OIDC front-channel／back-channel logout）—— 使用者在 IdP
  登出時，我們發出的 refresh token 應不應該失效？070 的撤銷機制已經就緒，
  缺的是 IdP 通知我們的那一段。這在決定 B 之後才有意義。
