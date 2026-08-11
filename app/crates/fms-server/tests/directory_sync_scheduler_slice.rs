//! `DirectorySyncWatchdog`（migration 078 的排程觸發，見該檔頭與
//! `fms_identity::directory_sync_watchdog` 的模組說明）。
//!
//! `directory_sync_slice.rs` 已經驗過對帳本身（收回、提權防護）——
//! 這裡驗的是**排程那一層**：哪些身分來源會被選中、哪些不會，以及
//! 一個壞掉的來源不會讓同一輪裡其他到期的來源也錯過。

mod common;

use common::*;

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const USER_REQUESTER: &str = "ffffffff-0000-4000-8000-000000000004";

/// 佈置一個身分來源 + 群組 + 成員 + 對應。`sync_cron`／`last_sync_at` 由呼叫端指定，
/// 藉此控制它在排程判斷裡是否到期。
async fn setup_provider(
    ctx: &TestContext,
    sync_enabled: bool,
    sync_cron: Option<&str>,
    last_sync_at: Option<chrono::DateTime<chrono::Utc>>,
    role_code: &str,
) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let code = format!("TEST_{}", uuid::Uuid::new_v4().simple());
    let provider: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.identity_providers
           (tenant_id, code, name, provider_type, ldap_host, ldap_base_dn,
            sync_enabled, sync_cron, last_sync_at)
         VALUES ($1::uuid, $2, '排程測試', 'LDAP',
                 'ad.example.com', 'dc=example,dc=com', $3, $4, $5)
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(&code)
    .bind(sync_enabled)
    .bind(sync_cron)
    .bind(last_sync_at)
    .fetch_one(&mut *tx)
    .await
    .expect("建 provider");

    let group: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.directory_groups
           (tenant_id, identity_provider_id, external_group_id, name)
         VALUES ($1::uuid, $2, 'ext-1', '排程測試群組')
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(provider)
    .fetch_one(&mut *tx)
    .await
    .expect("建群組");

    sqlx::query(
        "INSERT INTO fms.user_directory_groups (user_id, directory_group_id, tenant_id)
         VALUES ($1::uuid, $2, $3::uuid)",
    )
    .bind(USER_REQUESTER)
    .bind(group)
    .bind(TENANT_ID)
    .execute(&mut *tx)
    .await
    .expect("加入群組");

    sqlx::query(
        "INSERT INTO fms.directory_role_mappings
           (tenant_id, directory_group_id, role_id, scope_type, scope_id)
         SELECT $1::uuid, $2, r.id, 'FACILITY', $3::uuid
           FROM fms.roles r WHERE r.code = $4 AND r.tenant_id IS NULL",
    )
    .bind(TENANT_ID)
    .bind(group)
    .bind(FACILITY_HQ)
    .bind(role_code)
    .execute(&mut *tx)
    .await
    .expect("建對應");
    tx.commit().await.expect("commit");

    provider
}

async fn sync_runs_for(ctx: &TestContext, provider_id: uuid::Uuid) -> Vec<(String, String)> {
    let mut tx = ctx.owner_tx().await;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT run_type::text, status::text FROM fms.directory_sync_runs
          WHERE identity_provider_id = $1::uuid ORDER BY started_at",
    )
    .bind(provider_id)
    .fetch_all(&mut *tx)
    .await
    .expect("查作業列");
    tx.commit().await.expect("commit");
    rows
}

async fn last_sync_at(
    ctx: &TestContext,
    provider_id: uuid::Uuid,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let mut tx = ctx.owner_tx().await;
    let v: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_sync_at FROM fms.identity_providers WHERE id = $1::uuid")
            .bind(provider_id)
            .fetch_one(&mut *tx)
            .await
            .expect("查 last_sync_at");
    tx.commit().await.expect("commit");
    v
}

fn watchdog(
    pool: sqlx::PgPool,
) -> std::sync::Arc<fms_identity::directory_sync_watchdog::DirectorySyncWatchdog> {
    fms_identity::directory_sync_watchdog::DirectorySyncWatchdog::new(pool, admin_user_id())
}

/// 從未跑過（`last_sync_at IS NULL`）的來源一律視為到期，跑完之後留下
/// `run_type = SCHEDULED` 的作業列，且 `last_sync_at` 被寫入。
#[tokio::test]
async fn a_a_never_synced_provider_is_due_and_gets_a_scheduled_run() {
    let ctx = &TestContext::setup().await;
    let provider = setup_provider(ctx, true, Some("* * * * *"), None, "TECHNICIAN").await;

    // 這個 sweep 的計數是跨租戶的，示範租戶自己種的身分來源也會被算進去
    // （009 種下的來源 `sync_cron` 是 002 的預設值、`last_sync_at` 是 NULL，
    // 因此一律視為到期）—— 因此這裡不斷言絕對數字，只看新建的這個來源。
    watchdog(ctx.owner_pool().await)
        .run_once()
        .await
        .expect("run_once");

    let runs = sync_runs_for(ctx, provider).await;
    assert_eq!(runs.len(), 1, "應該恰好留下一筆作業列：{runs:?}");
    assert_eq!(
        runs[0].0, "SCHEDULED",
        "run_type 必須說出這是排程觸發的：{runs:?}"
    );
    assert_eq!(runs[0].1, "SUCCEEDED", "{runs:?}");
    assert!(
        last_sync_at(ctx, provider).await.is_some(),
        "同步過的來源必須更新 last_sync_at，否則下一輪判斷不出「已經跑過」"
    );

    ctx.teardown().await;
}

/// `sync_enabled = false` 或 `sync_cron IS NULL` 的來源，即使從未同步過，
/// 也不該被排程選中 —— 這正是 c_ 測試（HTTP 路徑）驗過的同一條規則，
/// 排程路徑必須遵守相同的開關。
#[tokio::test]
async fn b_disabled_or_unscheduled_providers_are_never_selected() {
    let ctx = &TestContext::setup().await;
    let disabled = setup_provider(ctx, false, Some("* * * * *"), None, "TECHNICIAN").await;
    let no_cron = setup_provider(ctx, true, None, None, "TECHNICIAN").await;

    watchdog(ctx.owner_pool().await)
        .run_once()
        .await
        .expect("run_once");

    assert!(sync_runs_for(ctx, disabled).await.is_empty());
    assert!(sync_runs_for(ctx, no_cron).await.is_empty());

    ctx.teardown().await;
}

/// 剛同步過、下一次排定時刻還沒到的來源，這一輪不該被選中。
#[tokio::test]
async fn c_a_provider_not_yet_due_is_skipped() {
    let ctx = &TestContext::setup().await;
    // 每 4 小時一次，一分鐘前才跑過 —— 下一次是快 4 小時以後。
    let provider = setup_provider(
        ctx,
        true,
        Some("0 */4 * * *"),
        Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
        "TECHNICIAN",
    )
    .await;

    watchdog(ctx.owner_pool().await)
        .run_once()
        .await
        .expect("run_once");
    assert!(sync_runs_for(ctx, provider).await.is_empty());

    ctx.teardown().await;
}

/// 一個 `sync_cron` 格式壞掉的來源不該讓整輪掃描失敗 —— 同一輪裡其他
/// 到期的來源必須照樣被處理。目前沒有寫入路徑能把壞值存進去
/// （`sync_cron` 不可 PATCH），但資料庫本身不保證這件事，這裡驗的是
/// 「萬一發生，不是致命的」。
#[tokio::test]
async fn d_a_malformed_cron_does_not_abort_the_rest_of_the_sweep() {
    let ctx = &TestContext::setup().await;
    let broken = setup_provider(ctx, true, Some("not a cron"), None, "TECHNICIAN").await;
    let healthy = setup_provider(ctx, true, Some("* * * * *"), None, "TECHNICIAN").await;

    watchdog(ctx.owner_pool().await)
        .run_once()
        .await
        .expect("run_once 不該因為一個壞掉的 cron 就整個失敗");

    assert!(
        sync_runs_for(ctx, broken).await.is_empty(),
        "格式壞掉的來源不該留下任何作業列"
    );
    assert_eq!(sync_runs_for(ctx, healthy).await.len(), 1);

    ctx.teardown().await;
}

/// 排程同步一樣受 052 的提權防護約束 —— 這裡驗的是**排程路徑**會把
/// 被擋下的對應記成 PARTIAL 並繼續（與 `directory_sync_slice.rs` 的
/// `c_sync_cannot_bypass_the_escalation_guard` 驗的是 HTTP 路徑同一條規則）。
/// migration 078 把服務帳號的權限限定在 `directory:sync`，因此正式部署下
/// 真正的排程觸發者比這裡用的測試帳號（`admin_user_id()`）權限更窄 ——
/// 這裡只驗證「被擋下時排程本身不會當機、且照樣留下歷程」。
#[tokio::test]
async fn e_a_blocked_mapping_is_recorded_as_partial_not_dropped_silently() {
    let ctx = &TestContext::setup().await;
    let provider = setup_provider(ctx, true, Some("* * * * *"), None, "PLATFORM_ADMIN").await;

    watchdog(ctx.owner_pool().await)
        .run_once()
        .await
        .expect("run_once");

    let runs = sync_runs_for(ctx, provider).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].0, "SCHEDULED");
    assert_eq!(runs[0].1, "PARTIAL", "{runs:?}");
    // 即使被擋，last_sync_at 仍要更新 —— 否則下一輪會誤判成「還沒跑過」
    // 而永遠重試同一個必然失敗的對應。
    assert!(last_sync_at(ctx, provider).await.is_some());

    ctx.teardown().await;
}
