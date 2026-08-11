//! 資料庫存取。核心不變量：**取得可查詢的 handle 的唯一路徑，會先呼叫
//! `fms.set_context()`**（ADR-09 實作紀律 4）。
//!
//! ADR-01 自述共享 Schema 的最大風險是「應用層漏寫條件」。本模組把它從
//! 「每支查詢都要記得寫 tenant_id」降級為型別問題：`TenantTx` 沒有公開的
//! 建構子，只能經 [`begin_tenant_tx`] 取得，而該函式必定先設好 context。
//! RLS 因此成為預設行為，不是紀律。

use std::collections::{HashMap, HashSet};

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::problem::Problem;

/// 請求層級的租戶情境。由 middleware 自 JWT `tid` 與 `X-Tenant-ID` 交叉驗證後產生，
/// 因此拿到這個型別本身就代表兩者一致。
#[derive(Debug, Clone, Copy)]
pub struct TenantContext {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// 供 029 的稽核觸發器關聯 log。`None` 表示不是經由 API 進來的
    /// （背景作業、migration、手動 SQL）—— 那時稽核列的 `request_id` 為空，
    /// 而**空本身就是訊號**：它代表這個變更沒有對應的 HTTP 請求。
    pub request_id: Option<Uuid>,
    pub actor_type: crate::context::ActorType,
}

impl TenantContext {
    /// 背景作業的情境。沒有請求可以關聯，因此 `request_id` 必然是 `None`。
    ///
    /// 提供這個建構子而不是讓呼叫端寫結構實字，是為了讓「這是背景作業」
    /// 成為一個明確的選擇 —— 而不是某人複製了一段 handler 的程式碼、
    /// 順手把 `actor_type` 留成 `User`，於是稽核軌把系統動作記成使用者動作。
    /// WBS 4.1 已經記過一次那個缺陷（`side_effects.actor: "SYSTEM"` 無人實作）。
    pub fn background(
        tenant_id: Uuid,
        user_id: Uuid,
        actor_type: crate::context::ActorType,
    ) -> Self {
        Self {
            tenant_id,
            user_id,
            request_id: None,
            actor_type,
        }
    }
}

/// 已注入 `fms.set_context()` 的交易。
///
/// 內部欄位刻意私有：外部無法用既有的 `Transaction` 直接建構一個
/// 「未設 context」的 `TenantTx`。要取得它只能走 [`begin_tenant_tx`]。
pub struct TenantTx {
    tx: Transaction<'static, Postgres>,
    ctx: TenantContext,
    /// 本次請求已解析過的權限集合，以 `(facility_id, org_id)` 為鍵。
    ///
    /// 只在**單一請求**內有效，因此不存在快取失效問題：一個請求是毫秒級，
    /// 期間權限不會被改掉，而交易結束時整份 map 就消失了。這是刻意選擇的
    /// 快取層級 —— 見 `permission_codes` 的說明。
    permissions: HashMap<(Option<Uuid>, Option<Uuid>), HashSet<String>>,
}

impl TenantTx {
    /// 交易的連線借用，供 `query_as!` 等使用（`Executor` 實作在
    /// `&mut PgConnection` 上，因此這裡解參考到連線層）。
    ///
    /// 刻意不叫 `as_mut`：那會與 `std::convert::AsMut` 撞名。
    pub fn conn(&mut self) -> &mut sqlx::PgConnection {
        &mut self.tx
    }

    pub fn context(&self) -> TenantContext {
        self.ctx
    }

    /// 提交。刻意要求顯式呼叫：未提交而 drop 即回滾，
    /// 這對寫入路徑而言是正確的預設值。
    pub async fn commit(self) -> Result<(), Problem> {
        self.tx.commit().await.map_err(Problem::from)
    }
}

/// 開啟交易並注入租戶情境。
///
/// 以 `fms.set_context()` 而非直接 `set_config`：該函式是規格書定義的單一入口，
/// 且 013 在其中加了平台情境的授權檢查（無權時拋 `PLATFORM_CONTEXT_DENIED`
/// 而非靜默忽略）。繞過它就繞過了那道檢查。
///
/// 第三參數固定為 `false`：一般請求路徑永遠不取得平台情境。
/// 平台級操作走另外的、明確標示的路徑。
pub async fn begin_tenant_tx(pool: &PgPool, ctx: TenantContext) -> Result<TenantTx, Problem> {
    let mut tx = pool.begin().await.map_err(Problem::from)?;

    // 兩支函式在**同一個 statement** 裡呼叫：稽核上下文與租戶情境是同一件
    // 「這個請求是誰、從哪來」，分兩次往返只是多一次 RTT。
    //
    // `set_request_context` 由 029 引入，餵的是稽核觸發器 —— 沒有它，
    // audit_log 的 request_id 與 actor_type 會永遠是預設值。
    sqlx::query("SELECT fms.set_context($1, $2, false), fms.set_request_context($3, $4)")
        .bind(ctx.tenant_id)
        .bind(ctx.user_id)
        .bind(ctx.request_id.map(|id| id.to_string()))
        .bind(ctx.actor_type.as_str())
        .execute(&mut *tx)
        .await
        .map_err(Problem::from)?;

    // 啟用 007 的場域級 RLS。
    //
    // 007 為 15 張表建了 RESTRICTIVE 政策 `facility_scope`，判定式是
    // `facility_in_scope()`，而後者讀 `app.facility_ids`。該 GUC 若為空，
    // `current_facility_ids()` 回 NULL，政策就**全部放行** ——
    // 也就是說在應用層設定它之前，那 15 個政策是完全惰性的。
    // 007 的註解本來就寫「The API sets app.facility_ids」，只是先前沒有人設。
    //
    // 這件事與權限判定是一組的：端點層的授權放寬成「在任一範圍持有權限」
    // （見 `permission_codes`），列可見性就必須由這裡收斂，
    // 否則場域範圍的角色會看到整個租戶。兩者少做一邊都是錯的。
    set_facility_scope(&mut tx, ctx.user_id).await?;

    Ok(TenantTx {
        tx,
        ctx,
        permissions: HashMap::new(),
    })
}

/// 由 `user_accessible_facilities()` 推導並寫入 `app.facility_ids`。
///
/// 空清單不能寫成空字串：那會被 `current_facility_ids()` 讀成 NULL，
/// 也就是「不限制」。用不可能存在的全零 uuid 當哨兵，
/// 讓沒有任何角色的使用者看不到任何列。
async fn set_facility_scope(
    tx: &mut Transaction<'static, Postgres>,
    user_id: Uuid,
) -> Result<(), Problem> {
    let facilities: Vec<Uuid> =
        sqlx::query_scalar("SELECT facility_id FROM fms.user_accessible_facilities($1)")
            .bind(user_id)
            .fetch_all(&mut **tx)
            .await
            .map_err(Problem::from)?;

    let ids = if facilities.is_empty() {
        "00000000-0000-0000-0000-000000000000".to_string()
    } else {
        facilities
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    sqlx::query("SELECT set_config('app.facility_ids', $1, true)")
        .bind(&ids)
        .execute(&mut **tx)
        .await
        .map_err(Problem::from)?;
    Ok(())
}

/// 在交易中重算 `app.facility_ids`。
///
/// # 什麼時候必須呼叫
///
/// `app.facility_ids` 是交易開始時對 `user_accessible_facilities()` 取的
/// **快照**。任何改變「這個使用者能看到哪些場域」的寫入都會讓它變**過期**，
/// 而過期的後果是 RLS 擋掉使用者其實有權存取的列。
///
/// 目前唯一的觸發點是**建立場域**：新場域的 id 不可能出現在交易開始時的
/// 快照裡，因此連 `INSERT ... RETURNING` 都會失敗
/// （PostgreSQL 會對 RETURNING 的列套用 SELECT 側政策）。
/// 這不是政策的問題，是快照過期。
///
/// 日後若新增「指派角色」的端點，那裡也必須呼叫這一支。
pub async fn refresh_facility_scope(tx: &mut TenantTx) -> Result<(), Problem> {
    let user_id = tx.ctx.user_id;
    set_facility_scope(&mut tx.tx, user_id).await
}

/// 取得使用者在指定範圍內持有的**全部**權限碼，並在本次請求內記憶。
///
/// 一律委派給 `fms.user_permission_codes()`（ADR-09 實作紀律 2）。
/// 刻意不在 Rust 內建權限模型：四張表的解析邏輯（ORG 範圍沿 ltree 涵蓋子樹、
/// 授權有效期間、來源區分）已在資料庫的函式與 view 裡；在應用層抄一份
/// 就是製造第二份真實來源。
///
/// # 為什麼快取層級是「請求」而不是 Redis
///
/// 實測（示範資料）：單次判定暖機後約 0.16ms。跨請求快取要換來的節省
/// 與一次 Redis 往返同級，卻要付出三種失效成本 ——
/// 角色指派變更、`role_permissions` 變更，以及最容易被忽略的
/// `user_role_assignments.valid_until` **時間到期**：那不是任何寫入事件，
/// 沒有東西會去主動失效它，於是被撤銷的權限會在快取裡繼續有效。
///
/// 請求層級的記憶沒有這些問題，卻解掉真正的浪費：同一個請求裡對同一組
/// 範圍反覆判定（`available-actions` 一次要問六個動作）。
/// 詳細取捨見 `docs/WBS-rebaseline.md` 4.1f。
///
/// # `facility_id = None` 的語意是「在任一範圍持有」
///
/// **不是**「在 TENANT 範圍持有」。這個區別不是細節：
/// `user_permission_codes` 的 FACILITY 分支比對 `scope_id = p_facility_id`，
/// 傳 NULL 就永遠不成立。若把 None 解讀成一次帶 NULL 的判定，
/// 所有沒有 `facility_id` 過濾條件的列表端點就只有 TENANT 範圍的角色能用，
/// `FACILITY_ADMIN`／`TECHNICIAN`／`REQUESTER` 一律 403。
///
/// 授權與可見性是兩件事：這裡回答「能不能用這個端點」，
/// 「看得到哪些列」由 [`begin_tenant_tx`] 設定的 `app.facility_ids`
/// 搭配 007 的 `facility_scope` 政策回答。少做任何一邊都是錯的 ——
/// 只放寬授權會讓場域角色看到整個租戶，只收斂可見性則端點根本進不去。
pub async fn permission_codes(
    tx: &mut TenantTx,
    facility_id: Option<Uuid>,
    org_id: Option<Uuid>,
) -> Result<&HashSet<String>, Problem> {
    let key = (facility_id, org_id);
    if !tx.permissions.contains_key(&key) {
        let ctx = tx.ctx;
        let codes: Vec<String> = if facility_id.is_none() && org_id.is_none() {
            sqlx::query_scalar("SELECT fms.user_permission_codes_anywhere($1)")
                .bind(ctx.user_id)
                .fetch_all(&mut *tx.tx)
                .await
                .map_err(Problem::from)?
        } else {
            sqlx::query_scalar("SELECT fms.user_permission_codes($1, $2, $3)")
                .bind(ctx.user_id)
                .bind(facility_id)
                .bind(org_id)
                .fetch_all(&mut *tx.tx)
                .await
                .map_err(Problem::from)?
        };
        tx.permissions.insert(key, codes.into_iter().collect());
    }
    // 上面確保了鍵存在。
    tx.permissions
        .get(&key)
        .ok_or_else(|| Problem::internal(std::io::Error::other("permission cache lost its key")))
}

/// 權限判定。
///
/// 底層走集合版：一次請求裡問第二個權限碼時不會再往返資料庫。
/// 016 讓 `user_has_permission` 也以 `user_permission_codes` 實作，
/// 因此兩者的 scope 判定是同一份 SQL，不會漂移
/// （012 的 T12 逐一比對整個交叉乘積來守住這件事）。
pub async fn has_permission(
    tx: &mut TenantTx,
    permission: &str,
    facility_id: Option<Uuid>,
    org_id: Option<Uuid>,
) -> Result<bool, Problem> {
    Ok(permission_codes(tx, facility_id, org_id)
        .await?
        .contains(permission))
}

/// 「這次請求已經通過一次授權判定」的憑證。
///
/// 零大小、欄位私有，因此**只有** [`require_permission`] 能產生它。
/// 需要「某個動作只能在授權之後發生」時，讓那個動作要求一個 `Authorized`
/// 參數，就把順序變成編譯期的事而不是慣例
/// （目前的用途：`concurrency::PendingReplay::release`）。
///
/// # 它證明什麼、不證明什麼
///
/// 證明：本次請求在此之前跑過一次 [`require_permission`] 且通過了。
/// **不**證明檢查的是哪一個權限碼或哪個範圍 —— 要那樣就得把權限碼帶進型別，
/// 而權限碼是資料庫裡的資料（016 的目錄），不是 Rust 的列舉。
/// 對現有用途而言這個強度已經足夠：先前的問題是**一次檢查都沒有**。
#[derive(Debug, Clone, Copy)]
pub struct Authorized(());

/// 要求「在 **TENANT 範圍**持有」這個權限。
///
/// # 為什麼 `require_permission(.., None, None)` 不等於這件事
///
/// 那個組合會走 `user_permission_codes_anywhere`，語意是「在**任一**範圍持有」
/// —— 那是刻意的（見 [`permission_codes`]：否則所有沒有 facility 過濾條件的
/// 列表端點只有 TENANT 角色能用）。但有少數動作的正確語意確實是
/// 「非 TENANT 範圍不得執行」，例如建立**根組織**：它不落在任何 ORG 子樹內，
/// 因此「你的 ORG 範圍涵蓋它嗎」這個問題沒有肯定答案。
///
/// 實作直接呼叫 `fms.user_permission_codes(user, NULL, NULL)`。那支函式的
/// 述詞在兩個參數都是 NULL 時只有 `scope_type = 'TENANT'` 分支可能成立，
/// 因此「在 TENANT 範圍持有」這個判定**早就存在於 016**，只是 Rust 這一層
/// 從來沒有用那個組合呼叫它。這裡不新增任何 SQL。
///
/// 刻意不進 [`permission_codes`] 的請求層記憶：它的鍵是
/// `(facility_id, org_id)`，而 `(None, None)` 已經被 `_anywhere` 的結果佔用。
/// 用同一個鍵存兩種語意的結果是最容易寫出來、也最難查的那種 bug。
/// 這支判定一次請求最多用一次，省下那次往返沒有意義。
pub async fn require_tenant_scoped_permission(
    tx: &mut TenantTx,
    permission: &str,
) -> Result<Authorized, Problem> {
    let user_id = tx.ctx.user_id;
    let held: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM fms.user_permission_codes($1, NULL, NULL) AS c
                         WHERE c = $2)",
    )
    .bind(user_id)
    .bind(permission)
    .fetch_one(&mut *tx.tx)
    .await
    .map_err(Problem::from)?;

    if held {
        Ok(Authorized(()))
    } else {
        Err(Problem::permission_denied(format!(
            "missing permission: {permission} (requires a TENANT-scoped grant)"
        )))
    }
}

/// 權限不足時直接回 `PERMISSION_DENIED`，供 handler 以 `?` 使用。
///
/// 回傳 [`Authorized`] 而非 `()`：絕大多數呼叫端在敘述位置直接丟掉它
/// （`require_permission(..).await?;`），只有需要證明順序的地方會接住。
/// 刻意不加 `#[must_use]` —— 那會讓所有既有呼叫端都跳警告，
/// 而它們丟掉憑證是完全正確的。
pub async fn require_permission(
    tx: &mut TenantTx,
    permission: &str,
    facility_id: Option<Uuid>,
    org_id: Option<Uuid>,
) -> Result<Authorized, Problem> {
    if has_permission(tx, permission, facility_id, org_id).await? {
        Ok(Authorized(()))
    } else {
        Err(Problem::permission_denied(format!(
            "missing permission: {permission}"
        )))
    }
}
