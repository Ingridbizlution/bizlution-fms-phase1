//! 時間分區的預先建立（migration 028 + `fms_worker::partitions`）。
//!
//! 這件事的失敗方式全是無聲的，因此測試要斷言的是**狀態**而不是「函式回了 Ok」：
//!   * 未來月份真的有分區（不是靠 DEFAULT 收）
//!   * 分區之間沒有縫（縫裡的列會掉進 DEFAULT）
//!   * 幂等 —— 第二次執行不重建
//!   * 涵蓋所有分區表，包含我一開始沒發現的 `asset_meter_readings`

mod common;

use common::*;

/// 這三張是目前 `fms` 裡以 timestamptz 做 RANGE 分區的表。
///
/// 寫在測試裡而不是讓測試自己探索，是刻意的：函式用探索（避免漏掉新表），
/// 測試用明確清單（避免「探索不到任何表」時測試空轉通過）。
/// 兩邊互為對照 —— 日後新增分區表時這個清單會過期，而過期的表現是
/// 下面的「函式涵蓋的表 ⊇ 這個清單」斷言失敗，那正是提醒。
const PARTITIONED: &[&str] = &["asset_meter_readings", "audit_log", "telemetry_readings"];

async fn month_partition_count(ctx: &TestContext, parent: &str) -> i64 {
    let mut tx = ctx.owner_tx().await;
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c
           JOIN pg_inherits i ON i.inhrelid = c.oid
           JOIN pg_class p ON p.oid = i.inhparent
           JOIN pg_namespace n ON n.oid = p.relnamespace
          WHERE n.nspname = 'fms' AND p.relname = $1
            AND pg_get_expr(c.relpartbound, c.oid) NOT LIKE 'DEFAULT%'",
    )
    .bind(parent)
    .fetch_one(&mut *tx)
    .await
    .expect("count month partitions")
}

#[tokio::test]
async fn every_partitioned_table_is_covered_and_has_no_gaps() {
    let ctx = &TestContext::setup().await;

    // ---- 每張表都被涵蓋，且分區數一致 ----
    // 不一致代表 028 的探索條件漏掉了某一張表。
    let mut counts = Vec::new();
    for parent in PARTITIONED {
        let n = month_partition_count(ctx, parent).await;
        assert!(
            n >= 4,
            "{parent} 只有 {n} 個月分區 —— 028 應涵蓋當月 +3（共 4 個）"
        );
        counts.push(n);
    }
    assert!(
        counts.windows(2).all(|w| w[0] == w[1]),
        "各分區表的月分區數應一致（探索若漏掉某張表就會不一致）：{counts:?}"
    );

    // ---- 沒有縫 ----
    // 縫是最危險的失敗：縫裡的列會掉進 DEFAULT，而那個狀態會自我鎖死。
    {
        let mut tx = ctx.owner_tx().await;
        let gaps: i64 = sqlx::query_scalar(
            r#"
            WITH bounds AS (
              SELECT p.relname AS parent,
                     split_part(split_part(pg_get_expr(c.relpartbound, c.oid), '''', 2), '''', 1) AS lo,
                     split_part(split_part(pg_get_expr(c.relpartbound, c.oid), '''', 4), '''', 1) AS hi
              FROM pg_class c
              JOIN pg_inherits i ON i.inhrelid = c.oid
              JOIN pg_class p ON p.oid = i.inhparent
              JOIN pg_namespace n ON n.oid = p.relnamespace
              WHERE n.nspname = 'fms'
                AND pg_get_expr(c.relpartbound, c.oid) NOT LIKE 'DEFAULT%'
            )
            SELECT count(*) FROM (
              SELECT parent, hi, lead(lo) OVER (PARTITION BY parent ORDER BY lo) AS next_lo
              FROM bounds
            ) t WHERE next_lo IS NOT NULL AND next_lo <> hi
            "#,
        )
        .fetch_one(&mut *tx)
        .await
        .expect("check gaps");
        assert_eq!(gaps, 0, "分區之間不得有縫");
    }

    ctx.teardown().await;
}

#[tokio::test]
async fn ensure_time_partitions_is_idempotent() {
    let ctx = &TestContext::setup().await;

    let before = month_partition_count(ctx, "telemetry_readings").await;

    // 直接呼叫兩次。第二次必須一個都不建 —— 這支函式會被背景作業每天叫一次，
    // 不幂等就會在第 32 天炸掉（分區已存在）。
    let second_run_created: i64 = {
        let mut tx = ctx.owner_tx().await;
        sqlx::query_scalar(
            "SELECT count(*) FROM fms.ensure_time_partitions(3) WHERE action = 'created'",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("second run")
    };
    assert_eq!(
        second_run_created, 0,
        "028 在 migration 裡已經跑過一次，再跑不該建立任何分區"
    );
    assert_eq!(
        month_partition_count(ctx, "telemetry_readings").await,
        before,
        "幂等呼叫不該改變分區數"
    );

    ctx.teardown().await;
}

/// 背景作業的 `run_once` 與 SQL 函式回報一致，且能真的補上缺口。
///
/// 刻意**先製造缺口**再驗證補上：只呼叫一次「已經補齊」的函式，
/// 無法分辨「有效」與「什麼都沒做」。
#[tokio::test]
async fn the_maintainer_closes_a_real_gap() {
    let ctx = &TestContext::setup().await;
    let pool = ctx.owner_pool().await;

    // 刪掉最後一個月分區，製造一個真實的缺口。
    // 它是空的（測試資料庫剛從 template 複製），因此刪除是安全的。
    let dropped: String = {
        let mut tx = ctx.owner_tx().await;
        let name: String = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_class c
               JOIN pg_inherits i ON i.inhrelid = c.oid
               JOIN pg_class p ON p.oid = i.inhparent
               JOIN pg_namespace n ON n.oid = p.relnamespace
              WHERE n.nspname = 'fms' AND p.relname = 'telemetry_readings'
                AND pg_get_expr(c.relpartbound, c.oid) NOT LIKE 'DEFAULT%'
              ORDER BY c.relname DESC LIMIT 1",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("find last partition");
        tx.commit().await.expect("commit");
        name
    };
    sqlx::query(&format!("DROP TABLE fms.{dropped}"))
        .execute(&pool)
        .await
        .expect("drop last partition");

    let before = month_partition_count(ctx, "telemetry_readings").await;

    let maintainer = fms_worker::partitions::PartitionMaintainer::new(pool.clone());
    let outcome = maintainer.run_once(3).await.expect("run_once");
    assert_eq!(
        outcome.created, 1,
        "剛好刪掉一個分區，因此應恰好補回一個：{outcome:?}"
    );
    assert!(
        outcome.already_present > 0,
        "其餘分區應被回報為 exists，而不是重建：{outcome:?}"
    );
    assert_eq!(
        month_partition_count(ctx, "telemetry_readings").await,
        before + 1,
        "缺口應已補上"
    );

    pool.close().await;
    ctx.teardown().await;
}
