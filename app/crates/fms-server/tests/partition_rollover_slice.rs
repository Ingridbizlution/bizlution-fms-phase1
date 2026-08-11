//! 分割輪替走進 DEFAULT 之後會發生什麼（028 + 049）。
//!
//! `partition_maintenance_slice.rs` 驗的是**正常狀態**：未來月份有分割、沒有縫、
//! 幂等、涵蓋所有分割表。它提到 DEFAULT 六次，但每一次都只是把它從計數裡
//! **排除** —— 從來沒有真的走進「列已經掉進 DEFAULT」那個狀態。
//!
//! 本檔補的就是那條路徑，而 **049 讓它從「以後再說」變成「該有測試」**：
//! `work_orders` 與 `assets` 現在每次變更都寫一列稽核（整列前後，56／35 欄），
//! 所以 DEFAULT 填得比以前快得多。
//!
//! 三件事，其中第一件與我原本的假設相反：
//!
//!   1. **DEFAULT 分割是可用性的承重牆。** 缺一個月份分割**不會**擋住業務寫入
//!      —— 稽核列落進 DEFAULT，工單照樣改得動。實測確認（我原本假設 029 的
//!      「稽核寫不進去就讓業務寫入一起失敗」會讓工單改不動，那是錯的）。
//!   2. **但那個狀態會自我鎖死**，而 028 已經預期到並給了有用的錯誤訊息。
//!   3. **028 檔頭寫的補救方式真的有效**（把列搬出 DEFAULT 再建分割）。
//!
//! 第 1 格的價值在於：**沒有任何地方說 DEFAULT 分割是承重牆**。
//! 有人「清理」掉它的時候，症狀會是跨月那一刻所有寫入開始失敗。

mod common;

use common::*;

/// 當月分割的名字。028 的命名慣例是 `<parent>_YYYYmMM`，
/// 邊界時區由 `partition_boundary_timezone()` 固定（不是伺服器的 TimeZone）。
async fn current_month_partition(ctx: &TestContext, parent: &str) -> String {
    let mut tx = ctx.owner_tx().await;
    let name: String = sqlx::query_scalar(
        "SELECT $1 || '_' || to_char(clock_timestamp() AT TIME ZONE
                  fms.partition_boundary_timezone(), 'YYYY\"m\"MM')",
    )
    .bind(parent)
    .fetch_one(&mut *tx)
    .await
    .expect("推導分割名");
    tx.commit().await.expect("commit");
    name
}

/// 卸除當月的 `audit_log` 分割，模擬「維護作業沒跑」。
async fn drop_current_audit_partition(ctx: &TestContext) -> String {
    let part = current_month_partition(ctx, "audit_log").await;
    let mut tx = ctx.owner_tx().await;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class
                         WHERE relname = $1 AND relnamespace = 'fms'::regnamespace)",
    )
    .bind(&part)
    .fetch_one(&mut *tx)
    .await
    .expect("查分割");
    assert!(exists, "前提：當月分割 {part} 應該存在（028 建的）");

    sqlx::query(&format!("DROP TABLE fms.{part}"))
        .execute(&mut *tx)
        .await
        .expect("卸除分割");
    tx.commit().await.expect("commit");
    part
}

/// 改一台設備 —— 049 之後這會寫一列稽核。回傳設備 id。
async fn touch_an_asset(ctx: &TestContext) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "UPDATE fms.assets SET name = name || '（分割測試）'
          WHERE id = (SELECT id FROM fms.assets ORDER BY id LIMIT 1)
        RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("改設備");
    tx.commit().await.expect("commit");
    id
}

/// **DEFAULT 分割是可用性的承重牆 —— 這件事沒有任何地方寫著。**
///
/// 缺一個月份分割時，稽核列落進 DEFAULT，而業務寫入**照樣成功**。
/// 這與 029 的「稽核寫不進去就該讓業務寫入一起失敗」不衝突：稽核並沒有
/// 寫不進去，只是寫到了 DEFAULT。
///
/// 這一格存在的理由是防止有人把 DEFAULT 分割當成多餘的東西移除 ——
/// 那樣做的症狀是**跨月那一刻所有被稽核的寫入開始失敗**，而 049 之後
/// 那包含每一次工單與設備的變更。
#[tokio::test]
async fn the_default_partition_is_what_keeps_writes_working() {
    let ctx = &TestContext::setup().await;

    // 前提：DEFAULT 分割存在。
    {
        let mut tx = ctx.owner_tx().await;
        let has_default: bool = sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT 1 FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
                WHERE i.inhparent = 'fms.audit_log'::regclass
                  AND pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT')",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("查 DEFAULT 分割");
        tx.commit().await.expect("commit");
        assert!(has_default, "前提：audit_log 應該有 DEFAULT 分割");
    }

    let part = drop_current_audit_partition(ctx).await;
    let asset = touch_an_asset(ctx).await;

    let mut tx = ctx.owner_tx().await;
    // 業務寫入真的生效了（不是「沒報錯但也沒改到」）。
    let name: String = sqlx::query_scalar("SELECT name FROM fms.assets WHERE id = $1")
        .bind(asset)
        .fetch_one(&mut *tx)
        .await
        .expect("讀設備");
    // 稽核列在 DEFAULT 裡，不是不見了。
    let in_default: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.audit_log_default
          WHERE entity_type = 'ASSETS' AND entity_id = $1",
    )
    .bind(asset)
    .fetch_one(&mut *tx)
    .await
    .expect("查 DEFAULT 裡的稽核列");
    tx.commit().await.expect("commit");

    assert!(
        name.contains("（分割測試）"),
        "少了 {part} 不該擋住業務寫入 —— DEFAULT 分割就是為此存在：{name}"
    );
    assert_eq!(in_default, 1, "稽核列該落進 DEFAULT 分割，而不是消失或報錯");

    ctx.teardown().await;
}

/// **但那個狀態會自我鎖死，而失敗訊息必須說得出怎麼修。**
///
/// 一旦某個月的列進了 DEFAULT，PostgreSQL 就拒絕為那個月建立 range 分割
/// （新分割的約束會被 DEFAULT 裡的既有列違反）。
///
/// 028 已經預期到這件事，把原始錯誤包成一句說得出補救方式的訊息 ——
/// 那正是這一格要守的東西。原始的 `updated partition constraint ... would be
/// violated` 看不出要怎麼辦；值班的人在半夜看到它只會卡住。
#[tokio::test]
async fn a_row_in_the_default_partition_locks_the_month_and_says_how_to_fix_it() {
    let ctx = &TestContext::setup().await;

    let part = drop_current_audit_partition(ctx).await;
    touch_an_asset(ctx).await; // 這一列進了 DEFAULT

    let mut tx = ctx.owner_tx().await;
    let err = sqlx::query("SELECT * FROM fms.ensure_time_partitions(3)")
        .fetch_all(&mut *tx)
        .await
        .expect_err("DEFAULT 裡有該月的列時，維護作業必須失敗");
    let msg = err.to_string();

    assert!(msg.contains(&part), "訊息要指出是哪個分割建不起來：{msg}");
    assert!(
        msg.contains("DEFAULT"),
        "訊息要指出原因是 DEFAULT 分割裡有列 —— \
         原始的 Postgres 錯誤看不出這件事：{msg}"
    );
    assert!(
        msg.contains("搬出") || msg.contains("ACCESS EXCLUSIVE"),
        "訊息要說得出補救方式（把列搬出 DEFAULT，需 ACCESS EXCLUSIVE 鎖）：{msg}"
    );

    ctx.teardown().await;
}

/// **028 檔頭寫的補救方式真的有效。**
///
/// 一份說明修法的文件如果沒有人走過那條路，它就只是一段看起來合理的文字。
/// 這一格把那段文字變成可執行的東西：搬出 → 建分割 → 搬回 → 資料仍然查得到。
#[tokio::test]
async fn the_documented_remedy_actually_unlocks_the_month() {
    let ctx = &TestContext::setup().await;

    let part = drop_current_audit_partition(ctx).await;
    let asset = touch_an_asset(ctx).await;

    // ---- 補救：把該月的列搬出 DEFAULT ----
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "CREATE TEMP TABLE rescued AS
               SELECT * FROM fms.audit_log_default
                WHERE occurred_at >= date_trunc('month',
                        clock_timestamp() AT TIME ZONE fms.partition_boundary_timezone())
                      AT TIME ZONE fms.partition_boundary_timezone()",
        )
        .execute(&mut *tx)
        .await
        .expect("搬出");

        let moved: i64 = sqlx::query_scalar("SELECT count(*) FROM rescued")
            .fetch_one(&mut *tx)
            .await
            .expect("數搬出的列");
        assert!(moved >= 1, "前提：DEFAULT 裡應該有該月的列");

        sqlx::query(
            "DELETE FROM fms.audit_log_default
              WHERE occurred_at >= date_trunc('month',
                      clock_timestamp() AT TIME ZONE fms.partition_boundary_timezone())
                    AT TIME ZONE fms.partition_boundary_timezone()",
        )
        .execute(&mut *tx)
        .await
        .expect("清空 DEFAULT 裡該月的列");

        // ---- 現在分割建得起來了 ----
        let created: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT parent_table, partition_name, action FROM fms.ensure_time_partitions(3)",
        )
        .fetch_all(&mut *tx)
        .await
        .expect("搬出之後維護作業該成功");
        assert!(
            created
                .iter()
                .any(|(_, name, action)| name == &part && action == "created"),
            "應該建出 {part}：{created:?}"
        );

        // ---- 搬回去 ----
        sqlx::query("INSERT INTO fms.audit_log SELECT * FROM rescued")
            .execute(&mut *tx)
            .await
            .expect("搬回");
        tx.commit().await.expect("commit");
    }

    // 資料落在正確的分割裡，而且從父表查得到。
    let mut tx = ctx.owner_tx().await;
    let in_month: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM fms.{part} WHERE entity_type = 'ASSETS' AND entity_id = $1"
    ))
    .bind(asset)
    .fetch_one(&mut *tx)
    .await
    .expect("查月份分割");
    let in_default: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.audit_log_default
          WHERE entity_type = 'ASSETS' AND entity_id = $1",
    )
    .bind(asset)
    .fetch_one(&mut *tx)
    .await
    .expect("查 DEFAULT");
    let via_parent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.audit_log
          WHERE entity_type = 'ASSETS' AND entity_id = $1",
    )
    .bind(asset)
    .fetch_one(&mut *tx)
    .await
    .expect("從父表查");
    tx.commit().await.expect("commit");

    assert_eq!(in_month, 1, "搬回去之後該落在 {part}");
    assert_eq!(in_default, 0, "DEFAULT 裡不該再有它");
    assert_eq!(via_parent, 1, "從父表查得到，而且只有一筆（沒有搬成兩份）");

    ctx.teardown().await;
}
