//! 預約提醒通知（migration 085 + `fms_reservation::reminder`）。
//!
//! 同 `cert_expiry_slice.rs` 的分工：085 的自我驗證只做結構檢查，行為在這裡驗。
//!
//! # 四個核心
//!
//! **`a_`：只有落在 15 分鐘窗內的才提醒**，窗外的（30 分鐘後）不提醒——
//! 若 sweep 沒有上界，會把整天的預約都掃進來，提醒失去「快開始了」的意義。
//!
//! **`b_`／`c_`：幂等不能變成沉默。** 同一個 `start_at` 不重複寄；但**改期**
//! （`start_at` 換了）必須重新提醒——理由跟 059 的 `reminded_for_expiry`
//! 完全對稱，只是這裡存的是 `start_at`。
//!
//! **`d_`：狀態不對的不提醒。** `CANCELLED` 的預約不會發生，提醒它是噪音。
//!
//! **`e_`：沒有範本要被計數。** 「該通知而沒有人會收到」必須數得出來。

mod common;

use common::*;

const ROOM_401: &str = "10000000-0000-4000-8000-000000000005";

/// 一輪掃描的結果。
#[derive(Debug, PartialEq)]
struct Sweep {
    reminded: i32,
    no_template: i32,
    already_reminded: i32,
}

async fn sweep(ctx: &TestContext) -> Sweep {
    let mut tx = ctx.owner_tx().await;
    let row: (i32, i32, i32) = sqlx::query_as("SELECT * FROM fms.sweep_reservation_reminders()")
        .fetch_one(&mut *tx)
        .await
        .expect("sweep");
    tx.commit().await.expect("commit");
    Sweep {
        reminded: row.0,
        no_template: row.1,
        already_reminded: row.2,
    }
}

/// 直接寫 `reservations`：驗的是掃描函式，不是 `POST /reservations`，
/// 而且要能自由控制 `start_at` 落在窗內／窗外與任意狀態。
async fn seed_reservation(ctx: &TestContext, minutes_from_now: i32, status: &str) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, title,
            start_at, end_at, status, requires_check_in)
         SELECT $1::uuid, br.facility_id, br.id,
                'RSV-RMD-' || substr(md5(random()::text), 1, 10),
                br.resource_type, $2::uuid, $3::uuid, '提醒測試會議',
                clock_timestamp() + make_interval(mins => $4),
                clock_timestamp() + make_interval(mins => $4) + interval '1 hour',
                $5, false
           FROM fms.bookable_resources br
          WHERE coalesce(br.spatial_node_id, br.asset_id) = $2::uuid
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(ROOM_401)
    .bind(ADMIN_USER_ID)
    .bind(minutes_from_now)
    .bind(status)
    .fetch_one(&mut *tx)
    .await
    .unwrap_or_else(|e| panic!("建 {status} 預約失敗：{e}"));
    tx.commit().await.expect("commit");
    id
}

async fn set_start_at_minutes_from_now(ctx: &TestContext, id: uuid::Uuid, minutes_from_now: i32) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "UPDATE fms.reservations
            SET start_at = clock_timestamp() + make_interval(mins => $2),
                end_at   = clock_timestamp() + make_interval(mins => $2) + interval '1 hour'
          WHERE id = $1::uuid",
    )
    .bind(id)
    .bind(minutes_from_now)
    .execute(&mut *tx)
    .await
    .expect("改期");
    tx.commit().await.expect("commit");
}

async fn reminder_notifications(ctx: &TestContext, reservation_id: uuid::Uuid) -> i64 {
    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.notifications
          WHERE template_code = 'RESERVATION_REMINDER' AND entity_id = $1::uuid",
    )
    .bind(reservation_id)
    .fetch_one(&mut *tx)
    .await
    .expect("count");
    tx.commit().await.expect("commit");
    n
}

/// 只有落在 15 分鐘窗內的才提醒；窗外（30 分鐘後）不提醒。
///
/// 這同時是「窗口沒有上界」的突變測試：若 sweep 忘了 `<=` 那個上界，
/// 30 分鐘後的那筆也會被選中。
#[tokio::test]
async fn a_only_reservations_inside_the_15_minute_window_are_reminded() {
    let ctx = TestContext::setup().await;

    let soon = seed_reservation(&ctx, 10, "CONFIRMED").await;
    // 90 分鐘後（1 小時時長）：跟上面那筆（10–70 分鐘）不重疊，
    // 避免撞上 `excl_reservations_no_overlap`——這裡驗的是提醒窗，不是排他約束。
    let later = seed_reservation(&ctx, 90, "CONFIRMED").await;

    let s = sweep(&ctx).await;
    assert_eq!(
        s.reminded, 2,
        "10 分鐘後的那筆該提醒，兩個管道各一封：{s:?}"
    );
    assert_eq!(s.no_template, 0);

    assert_eq!(reminder_notifications(&ctx, soon).await, 2);
    assert_eq!(
        reminder_notifications(&ctx, later).await,
        0,
        "90 分鐘後超出 15 分鐘窗，不該提醒"
    );

    ctx.teardown().await;
}

/// 重跑不會重寄。**這是 085 的 `reminded_for_start_at` 唯一的理由**。
#[tokio::test]
async fn b_a_second_sweep_sends_nothing() {
    let ctx = TestContext::setup().await;

    let id = seed_reservation(&ctx, 5, "CONFIRMED").await;
    assert_eq!(sweep(&ctx).await.reminded, 2);
    let after_first = reminder_notifications(&ctx, id).await;

    let s = sweep(&ctx).await;
    assert_eq!(s.reminded, 0, "第二輪不該再寄；實際 {s:?}");
    assert_eq!(s.already_reminded, 1, "而且要數得出來是被跳過的");
    assert_eq!(reminder_notifications(&ctx, id).await, after_first);

    ctx.teardown().await;
}

/// 改期之後要**重新**提醒。
///
/// 若 085 存的是 boolean 而不是 `start_at` 本身，這條會失敗——而失敗的
/// 方式是沉默的：改到新時段之後，沒有人會被通知。
#[tokio::test]
async fn c_rescheduling_earns_a_fresh_reminder() {
    let ctx = TestContext::setup().await;

    let id = seed_reservation(&ctx, 5, "CONFIRMED").await;
    assert_eq!(sweep(&ctx).await.reminded, 2);
    assert_eq!(sweep(&ctx).await.reminded, 0);

    // 改期：往後挪到 8 分鐘後（仍在 15 分鐘窗內）。
    set_start_at_minutes_from_now(&ctx, id, 8).await;
    let s = sweep(&ctx).await;
    assert_eq!(s.reminded, 2, "改期後的新時段該重新提醒；實際 {s:?}");
    assert_eq!(s.already_reminded, 0);

    ctx.teardown().await;
}

/// `CANCELLED`／`NO_SHOW` 的預約不會發生，不提醒。
#[tokio::test]
async fn d_cancelled_reservations_are_skipped() {
    let ctx = TestContext::setup().await;

    let id = seed_reservation(&ctx, 5, "CANCELLED").await;
    let s = sweep(&ctx).await;
    assert_eq!(
        s,
        Sweep {
            reminded: 0,
            no_template: 0,
            already_reminded: 0
        },
        "CANCELLED 不該被選中：{s:?}"
    );
    assert_eq!(reminder_notifications(&ctx, id).await, 0);

    ctx.teardown().await;
}

/// 缺範本要被**計數**，而不是安靜地當成「沒事要做」。
#[tokio::test]
async fn e_a_missing_template_is_counted_not_swallowed() {
    let ctx = TestContext::setup().await;

    {
        let mut tx = ctx.owner_tx().await;
        let n = sqlx::query(
            "UPDATE fms.notification_templates
                SET is_active = false
              WHERE code = 'RESERVATION_REMINDER'",
        )
        .execute(&mut *tx)
        .await
        .expect("停用範本")
        .rows_affected();
        assert_eq!(n, 2, "085 該建了 EMAIL 與 IN_APP 兩個範本");
        tx.commit().await.expect("commit");
    }

    let id = seed_reservation(&ctx, 5, "CONFIRMED").await;
    let s = sweep(&ctx).await;
    assert_eq!(
        s,
        Sweep {
            reminded: 0,
            no_template: 1,
            already_reminded: 0
        },
        "沒有範本 → 沒有人收到，但必須數得出來；實際 {s:?}"
    );
    assert_eq!(reminder_notifications(&ctx, id).await, 0);

    ctx.teardown().await;
}

/// 變數必須真的被代入——一封寫著 `{{title}}` 的信等於沒寄。
#[tokio::test]
async fn f_notification_content_has_variables_substituted() {
    let ctx = TestContext::setup().await;

    let id = seed_reservation(&ctx, 5, "PENDING_APPROVAL").await;
    assert_eq!(sweep(&ctx).await.reminded, 2, "PENDING_APPROVAL 也該提醒");

    let mut tx = ctx.owner_tx().await;
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT channel::text, priority::text, subject::text, body::text
           FROM fms.notifications
          WHERE template_code = 'RESERVATION_REMINDER' AND entity_id = $1::uuid
          ORDER BY channel",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .expect("讀通知");
    tx.commit().await.expect("commit");

    assert_eq!(rows.len(), 2, "EMAIL + IN_APP");
    for (channel, priority, subject, body) in &rows {
        assert_eq!(priority, "NORMAL");
        for text in [subject, body] {
            assert!(
                !text.contains("{{"),
                "{channel} 的內容有未代入的變數：{text}"
            );
        }
        assert!(
            body.contains("提醒測試會議") || subject.contains("提醒測試會議"),
            "{channel} 的內容該包含會議標題：subject={subject} body={body}"
        );
    }

    ctx.teardown().await;
}
