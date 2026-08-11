//! outbox relay 的保證測試。
//!
//! 驗的是四件事，全部是目前完全未被驗證過的架構層：
//!   1. **交易性**（ADR-05 的核心主張）：事件與業務資料同生共死。
//!      回滾業務寫入，事件必須一併消失 —— 否則就會出現「工單建好了但事件掉了」
//!      的反面：「事件發出去了但工單沒建立」。
//!   2. **不重複投遞**：兩個 relay 實例並行時，同一事件只被處理一次
//!      （`FOR UPDATE SKIP LOCKED`）。
//!   3. **退避重試**：handler 失敗後事件退回 FAILED、attempt_count 遞增、
//!      available_at 被推遲，因此不會立刻被同一輪重取。
//!   4. **毒事件停放**：超過重試上限後標為 SKIPPED，不再無限重試。

use fms_worker::{run_once, EventHandler, OutboxEvent, RelayConfig};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const TENANT_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

/// 所有測試事件共用的前綴，供 handler 以 `starts_with` 比對。
const EVENT_PREFIX: &str = "relay.test.";

/// 每個測試自己的 event_type。
///
/// `cargo test` 預設並行執行同一檔案內的測試，若共用同一個 event_type，
/// 各測試的前置清理會互相刪掉對方正在處理的事件（實測就是這樣讓
/// 併發測試只看到 13/20 筆）。以 UUID 隔離命名空間，測試才能安全並行。
fn unique_event_type() -> String {
    format!("{EVENT_PREFIX}{}", Uuid::new_v4())
}

/// 限縮到本測試自己的 event_type，避免並行的測試互相搶事件。
fn cfg_for(event_type: &str) -> RelayConfig {
    RelayConfig {
        event_types: Some(vec![event_type.to_string()]),
        ..Default::default()
    }
}

fn owner_url() -> String {
    std::env::var("OWNER_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://fms_owner:change_me_owner@localhost:5433/fms".into())
}
fn app_url() -> String {
    std::env::var("APP_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://fms_app:change_me_app@localhost:5433/fms".into())
}

async fn owner_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(6)
        .connect(&owner_url())
        .await
        .expect("connect as fms_owner")
}

/// 清掉本測試的事件，讓測試可重複執行（前置清理，不只後置）。
async fn cleanup(pool: &PgPool, event_type: &str) {
    let mut conn = pool.acquire().await.expect("acquire");
    sqlx::query("SELECT set_config('app.is_platform','on',false)")
        .execute(&mut *conn)
        .await
        .expect("platform context");
    sqlx::query("DELETE FROM fms.event_outbox WHERE event_type = $1")
        .bind(event_type)
        .execute(&mut *conn)
        .await
        .expect("cleanup");
}

/// 直接以 `fms.emit_event()` 插入測試事件 —— 走的是生產端真正用的那支函式。
async fn emit(pool: &PgPool, event_type: &str, n: usize) -> Vec<i64> {
    let mut conn = pool.acquire().await.expect("acquire");
    sqlx::query("SELECT set_config('app.is_platform','on',false)")
        .execute(&mut *conn)
        .await
        .expect("platform context");

    let mut ids = Vec::new();
    for i in 0..n {
        let id: i64 = sqlx::query_scalar(
            "SELECT fms.emit_event($1::uuid, $2, 'TEST', gen_random_uuid(), $3::jsonb)",
        )
        .bind(TENANT_ID)
        .bind(event_type)
        .bind(serde_json::json!({ "seq": i }))
        .fetch_one(&mut *conn)
        .await
        .expect("emit_event");
        ids.push(id);
    }
    ids
}

async fn status_of(pool: &PgPool, id: i64) -> (String, i16, Option<String>) {
    let mut conn = pool.acquire().await.expect("acquire");
    sqlx::query("SELECT set_config('app.is_platform','on',false)")
        .execute(&mut *conn)
        .await
        .expect("platform context");
    sqlx::query_as("SELECT status, attempt_count, last_error FROM fms.event_outbox WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .expect("read status")
}

/// 永遠成功。
struct Ok_;
impl EventHandler for Ok_ {
    fn handles(&self, t: &str) -> bool {
        t.starts_with(EVENT_PREFIX)
    }
    async fn handle(&self, _e: &OutboxEvent) -> Result<(), String> {
        Ok(())
    }
}

/// 永遠失敗，並計數被呼叫幾次。
struct AlwaysFail(Arc<AtomicUsize>);
impl EventHandler for AlwaysFail {
    fn handles(&self, t: &str) -> bool {
        t.starts_with(EVENT_PREFIX)
    }
    async fn handle(&self, _e: &OutboxEvent) -> Result<(), String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("simulated downstream failure".into())
    }
}

/// 什麼都不處理。
struct HandlesNothing;
impl EventHandler for HandlesNothing {
    fn handles(&self, _t: &str) -> bool {
        false
    }
    async fn handle(&self, _e: &OutboxEvent) -> Result<(), String> {
        unreachable!("handles() 回 false 就不該被呼叫")
    }
}

/// 記錄處理過的事件 id，用來檢查有無重複投遞。
struct Recording(Arc<std::sync::Mutex<Vec<i64>>>);
impl EventHandler for Recording {
    fn handles(&self, t: &str) -> bool {
        t.starts_with(EVENT_PREFIX)
    }
    async fn handle(&self, e: &OutboxEvent) -> Result<(), String> {
        // 讓兩個 relay 的取用窗口重疊，才測得到 SKIP LOCKED
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.0.lock().unwrap().push(e.id);
        Ok(())
    }
}

/// 1. 交易性：ADR-05 的核心主張。
///
/// 以 `fms_app` 在一個交易內建立預約（觸發 `trg_reservations_events`
/// 寫出 outbox 事件），然後**回滾**。事件必須跟著消失。
#[tokio::test]
async fn events_are_written_in_the_same_transaction_as_business_data() {
    let owner = owner_pool().await;

    let app = PgPoolOptions::new()
        .max_connections(2)
        .connect(&app_url())
        .await
        .expect("connect as fms_app");

    let before: i64 = {
        let mut c = owner.acquire().await.unwrap();
        sqlx::query("SELECT set_config('app.is_platform','on',false)")
            .execute(&mut *c)
            .await
            .unwrap();
        sqlx::query_scalar(
            "SELECT count(*) FROM fms.event_outbox WHERE aggregate_type='RESERVATION'",
        )
        .fetch_one(&mut *c)
        .await
        .unwrap()
    };

    // --- 在交易內建立預約，確認事件已寫入，然後回滾 ---
    let mut tx = app.begin().await.unwrap();
    sqlx::query("SELECT fms.set_context($1::uuid, $2::uuid, false)")
        .bind(TENANT_ID)
        .bind("ffffffff-0000-4000-8000-000000000001")
        .execute(&mut *tx)
        .await
        .unwrap();

    let start = chrono::Utc::now() + chrono::Duration::days(9);
    sqlx::query(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no, resource_type,
            resource_id, organizer_id, title, start_at, end_at, status)
         VALUES ($1::uuid, 'cccccccc-0000-4000-8000-000000000001',
                 '70000000-0000-4000-8000-000000000001',
                 fms.next_document_no($1::uuid,'RESERVATION','RSV'),
                 'SPATIAL_NODE', '10000000-0000-4000-8000-000000000005',
                 'ffffffff-0000-4000-8000-000000000001', 'outbox 交易性測試',
                 $2, $3, 'CONFIRMED')",
    )
    .bind(TENANT_ID)
    .bind(start)
    .bind(start + chrono::Duration::hours(1))
    .execute(&mut *tx)
    .await
    .expect("insert reservation");

    let inside: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.event_outbox WHERE aggregate_type='RESERVATION'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        inside,
        before + 1,
        "交易內應看得到 trg_reservations_events 寫出的事件"
    );

    tx.rollback().await.unwrap();

    // --- 回滾後事件必須也不見了 ---
    let after: i64 = {
        let mut c = owner.acquire().await.unwrap();
        sqlx::query("SELECT set_config('app.is_platform','on',false)")
            .execute(&mut *c)
            .await
            .unwrap();
        sqlx::query_scalar(
            "SELECT count(*) FROM fms.event_outbox WHERE aggregate_type='RESERVATION'",
        )
        .fetch_one(&mut *c)
        .await
        .unwrap()
    };
    assert_eq!(
        after, before,
        "業務寫入回滾後事件仍留在 outbox —— transactional outbox 的保證不成立"
    );
}

/// 2. 成功路徑 + 3. 退避重試 + 4. 毒事件停放
#[tokio::test]
async fn relay_publishes_retries_with_backoff_then_parks_poison_events() {
    let pool = owner_pool().await;
    let event_type = unique_event_type();
    cleanup(&pool, &event_type).await;

    // --- 成功 ---
    let ok_ids = emit(&pool, &event_type, 3).await;
    let cfg_ok = cfg_for(&event_type);
    let batch = run_once(&pool, &Ok_, &cfg_ok).await.expect("relay run");
    assert!(batch.published >= 3, "應發佈 3 筆，實際 {batch:?}");
    for id in &ok_ids {
        let (status, _, _) = status_of(&pool, *id).await;
        assert_eq!(status, "PUBLISHED", "事件 {id} 應為 PUBLISHED");
    }

    // --- 無 handler → SKIPPED（不是假裝成功）---
    let none_ids = emit(&pool, &event_type, 1).await;
    let batch = run_once(&pool, &HandlesNothing, &cfg_ok)
        .await
        .expect("relay run");
    assert_eq!(batch.skipped, 1, "{batch:?}");
    let (status, _, err) = status_of(&pool, none_ids[0]).await;
    assert_eq!(status, "SKIPPED");
    assert!(
        err.unwrap_or_default().contains("no handler"),
        "應記錄跳過的原因"
    );

    // --- 失敗 → FAILED、attempt_count 遞增、available_at 被推遲 ---
    let fail_ids = emit(&pool, &event_type, 1).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = AlwaysFail(calls.clone());
    // backoff_base 設 2 秒：第一次失敗後延後 4 秒，因此同一輪不會被重取
    let cfg = RelayConfig {
        max_attempts: 3,
        backoff_base: Duration::from_secs(2),
        ..cfg_for(&event_type)
    };

    let batch = run_once(&pool, &handler, &cfg).await.expect("relay run");
    assert_eq!(batch.retried, 1, "{batch:?}");
    let (status, attempts, err) = status_of(&pool, fail_ids[0]).await;
    assert_eq!(status, "FAILED");
    assert_eq!(attempts, 1, "attempt_count 應遞增");
    assert!(err.unwrap_or_default().contains("simulated"));

    // 立刻再跑一輪：因為 available_at 被推遲，這一輪不該再取到它
    let batch = run_once(&pool, &handler, &cfg).await.expect("relay run");
    assert!(
        batch.is_empty(),
        "退避未生效：事件在 available_at 之前又被取用了（{batch:?}）"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "handler 在退避期間不應被再次呼叫"
    );

    // --- 毒事件：把 available_at 拉回並跑到上限，應停放為 SKIPPED ---
    for _ in 0..cfg.max_attempts {
        let mut c = pool.acquire().await.unwrap();
        sqlx::query("SELECT set_config('app.is_platform','on',false)")
            .execute(&mut *c)
            .await
            .unwrap();
        sqlx::query("UPDATE fms.event_outbox SET available_at = clock_timestamp() WHERE id = $1")
            .bind(fail_ids[0])
            .execute(&mut *c)
            .await
            .unwrap();
        run_once(&pool, &handler, &cfg).await.expect("relay run");
    }
    let (status, attempts, err) = status_of(&pool, fail_ids[0]).await;
    assert_eq!(
        status, "SKIPPED",
        "超過 max_attempts 應停放為 SKIPPED，而不是無限重試"
    );
    assert!(attempts <= cfg.max_attempts, "attempt_count 不應超過上限");
    assert!(err.unwrap_or_default().contains("giving up"));

    cleanup(&pool, &event_type).await;
}

/// 2. 不重複投遞：兩個 relay 並行時同一事件只被處理一次。
#[tokio::test]
async fn concurrent_relays_do_not_deliver_the_same_event_twice() {
    let pool = owner_pool().await;
    let event_type = unique_event_type();
    cleanup(&pool, &event_type).await;

    let ids = emit(&pool, &event_type, 20).await;

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cfg = RelayConfig {
        batch_size: 20,
        ..cfg_for(&event_type)
    };

    // 兩個 relay 同時跑。Recording 的 handler 會 sleep，
    // 確保兩者的取用窗口重疊 —— 否則測不到 SKIP LOCKED。
    let handler_a = Recording(seen.clone());
    let handler_b = Recording(seen.clone());
    let (a, b) = tokio::join!(
        run_once(&pool, &handler_a, &cfg),
        run_once(&pool, &handler_b, &cfg),
    );
    let a = a.expect("relay A");
    let b = b.expect("relay B");

    let processed = seen.lock().unwrap().clone();
    let unique: std::collections::BTreeSet<i64> = processed.iter().copied().collect();

    assert_eq!(
        processed.len(),
        unique.len(),
        "同一事件被處理了多次：{processed:?} —— FOR UPDATE SKIP LOCKED 未生效"
    );
    assert_eq!(
        a.published + b.published,
        ids.len(),
        "兩個 relay 合計應恰好處理完全部事件（A={a:?} B={b:?}）"
    );

    for id in &ids {
        let (status, _, _) = status_of(&pool, *id).await;
        assert_eq!(status, "PUBLISHED", "事件 {id} 未被處理");
    }

    cleanup(&pool, &event_type).await;
}

/// relay 必須以 fms_owner 連線才能跨租戶排空；以 fms_app 連線看不到事件。
/// 這條同時是「別把 relay 接成 fms_app」的回歸保護。
#[tokio::test]
async fn fms_app_cannot_drain_the_outbox() {
    let owner = owner_pool().await;
    let event_type = unique_event_type();
    cleanup(&owner, &event_type).await;
    emit(&owner, &event_type, 3).await;

    let app = PgPoolOptions::new()
        .max_connections(2)
        .connect(&app_url())
        .await
        .expect("connect as fms_app");

    // 未設情境的 fms_app 看不到任何事件
    let visible: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fms.event_outbox WHERE event_type = $1")
            .bind(&event_type)
            .fetch_one(&app)
            .await
            .expect("query as fms_app");
    assert_eq!(
        visible, 0,
        "fms_app 不該看到 outbox 事件（若看得到，表示 RLS 或角色設定有誤）"
    );

    // 即使自行宣稱平台情境也一樣（013 雙條件）
    let mut conn = app.acquire().await.unwrap();
    sqlx::query("SET app.is_platform = 'on'")
        .execute(&mut *conn)
        .await
        .unwrap();
    let visible: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fms.event_outbox WHERE event_type = $1")
            .bind(&event_type)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(visible, 0, "fms_app 宣稱平台情境仍不該看到事件（013 硬化）");

    cleanup(&owner, &event_type).await;
}
