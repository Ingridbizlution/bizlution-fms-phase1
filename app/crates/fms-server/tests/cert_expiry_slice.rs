//! 證照到期提醒（migration 059 + `cert_watchdog`）。
//!
//! # 這裡驗的是「該提醒的提醒了、不該重複的不重複」
//!
//! 059 的自我驗證只做結構檢查（述詞 sargable、幂等用 `IS NOT DISTINCT FROM`），
//! 因為那支 migration 跑在 seed 009 **之前**，沒有使用者也沒有證照可以驗；
//! 而在 migration 裡塞探測資料會留下業務資料。行為在這裡驗。
//!
//! # 四個核心
//!
//! **`a_`：前置期是每個技能自己的。** 電氣 60 天、預設 30 天。若 sweep 寫死
//! 一個數字，一張 45 天後到期的電氣執照就不會被提醒 —— 而換證要送審，
//! 那正是 60 天存在的理由。
//!
//! **`b_`／`c_`：幂等不能變成沉默。** 同一個到期日不重複寄；但**換證之後**
//! （`expires_at` 前進）必須重新提醒。存一個 bool 「已提醒」就會讓第二次
//! 到期永遠不提醒 —— 所以 059 存的是**那個到期日**。
//!
//! **`d_`：離職的人不提醒。** 他不會去換證，那封信只是噪音。
//!
//! **`e_`：沒有範本要被計數。** 那是「該通知而沒有人會收到」——
//! 這個 repo 反覆出現的缺陷類型，必須數得出來。

mod common;

use common::*;

const SKILL_ELECTRICAL: &str = "50000000-0000-4000-8000-000000000001";
/// HVAC —— `requires_certification = false`，用來驗證掃描不會碰它。
const SKILL_HVAC: &str = "50000000-0000-4000-8000-000000000006";
const USER_TECH: &str = "ffffffff-0000-4000-8000-000000000006";

/// 一輪掃描的結果。
#[derive(Debug, PartialEq)]
struct Sweep {
    reminded: i32,
    no_template: i32,
    already_reminded: i32,
}

async fn sweep(ctx: &TestContext) -> Sweep {
    let mut tx = ctx.owner_tx().await;
    let row: (i32, i32, i32) = sqlx::query_as("SELECT * FROM fms.sweep_certification_expiry()")
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

/// 給某人某項技能一個到期日（天數相對於**資料庫的今天**）。
///
/// 直接寫 `user_skills` 而不是走 `PUT /users/{id}/skills/{skillId}`：
/// 那支端點會把 `reminded_for_expiry` 留成 NULL，正好是我們要的起點，
/// 但它也需要登入與權限 —— 這裡驗的是掃描函式，不是那支端點。
async fn grant_skill(ctx: &TestContext, user_id: &str, skill_id: &str, days_until_expiry: i32) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.user_skills
           (user_id, skill_id, tenant_id, level, certified_at, expires_at, certificate_no)
         VALUES ($1::uuid, $2::uuid, $3::uuid, 3,
                 current_date - interval '2 years',
                 current_date + ($4::int || ' days')::interval, 'TEST-001')
         ON CONFLICT (user_id, skill_id) DO UPDATE
            SET expires_at = excluded.expires_at",
    )
    .bind(user_id)
    .bind(skill_id)
    .bind(TENANT_ID)
    .bind(days_until_expiry)
    .execute(&mut *tx)
    .await
    .expect("grant skill");
    tx.commit().await.expect("commit");
}

/// 本租戶這個人的 `CERT_EXPIRING` 通知數。
async fn cert_notifications(ctx: &TestContext, user_id: &str) -> i64 {
    let mut tx = ctx.owner_tx().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.notifications
          WHERE template_code = 'CERT_EXPIRING' AND recipient_user_id = $1::uuid",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    .expect("count");
    tx.commit().await.expect("commit");
    n
}

async fn set_user_status(ctx: &TestContext, user_id: &str, status: &str) {
    let mut tx = ctx.owner_tx().await;
    sqlx::query("UPDATE fms.users SET status = $2 WHERE id = $1::uuid")
        .bind(user_id)
        .bind(status)
        .execute(&mut *tx)
        .await
        .expect("set status");
    tx.commit().await.expect("commit");
}

/// 前置期是**每項技能自己的**：電氣 60 天，所以 45 天後到期的要提醒；
/// 而一張 90 天後到期的不該提醒。
///
/// 這條同時是「寫死 30」的突變測試：若 sweep 用固定 30 天，
/// 45 天那筆就不會被選中。
#[tokio::test]
async fn a_lead_time_comes_from_the_skill_not_a_hardcoded_number() {
    let ctx = TestContext::setup().await;

    grant_skill(&ctx, USER_TECH, SKILL_ELECTRICAL, 45).await;
    let s = sweep(&ctx).await;
    // 兩個管道（EMAIL + IN_APP）各一封。
    assert_eq!(
        s.reminded, 2,
        "45 天後到期的電氣執照該提醒（電氣的前置期是 60 天）"
    );
    assert_eq!(s.no_template, 0);

    // 90 天後到期：超出 60 天的前置期。
    grant_skill(&ctx, ADMIN_USER_ID, SKILL_ELECTRICAL, 90).await;
    let s = sweep(&ctx).await;
    assert_eq!(
        s.reminded, 0,
        "90 天後到期超出 60 天前置期，不該提醒；實際 {s:?}"
    );
    // 上一筆已提醒過，計入 already_reminded —— 這是幂等生效的證據。
    assert_eq!(s.already_reminded, 1);

    ctx.teardown().await;
}

/// 重跑不會重寄。**這是 059 的 `reminded_for_expiry` 唯一的理由**。
#[tokio::test]
async fn b_a_second_sweep_sends_nothing() {
    let ctx = TestContext::setup().await;

    grant_skill(&ctx, USER_TECH, SKILL_ELECTRICAL, 10).await;
    assert_eq!(sweep(&ctx).await.reminded, 2);
    let after_first = cert_notifications(&ctx, USER_TECH).await;

    let s = sweep(&ctx).await;
    assert_eq!(s.reminded, 0, "第二輪不該再寄；實際 {s:?}");
    assert_eq!(s.already_reminded, 1, "而且要數得出來是被跳過的");
    assert_eq!(
        cert_notifications(&ctx, USER_TECH).await,
        after_first,
        "通知數不該增加"
    );

    ctx.teardown().await;
}

/// 換證之後要**重新**提醒。
///
/// 若 059 存的是 bool 而不是那個到期日，這條會失敗 —— 而失敗的方式是
/// 沉默的：三年後那張證照再次到期，沒有人會被通知。
#[tokio::test]
async fn c_renewal_earns_a_fresh_reminder() {
    let ctx = TestContext::setup().await;

    grant_skill(&ctx, USER_TECH, SKILL_ELECTRICAL, 10).await;
    assert_eq!(sweep(&ctx).await.reminded, 2);
    assert_eq!(sweep(&ctx).await.reminded, 0);

    // 換證：到期日前進到 40 天後（仍在電氣的 60 天前置期內）。
    grant_skill(&ctx, USER_TECH, SKILL_ELECTRICAL, 40).await;
    let s = sweep(&ctx).await;
    assert_eq!(s.reminded, 2, "換證後的新到期日該重新提醒；實際 {s:?}");
    assert_eq!(s.already_reminded, 0);

    ctx.teardown().await;
}

/// 已離職／停權的人不提醒；`requires_certification = false` 的技能也不碰。
#[tokio::test]
async fn d_suspended_users_and_non_certified_skills_are_skipped() {
    let ctx = TestContext::setup().await;

    grant_skill(&ctx, USER_TECH, SKILL_ELECTRICAL, 10).await;
    set_user_status(&ctx, USER_TECH, "SUSPENDED").await;
    // HVAC 不需要證照 —— 即使有到期日也不提醒。
    grant_skill(&ctx, ADMIN_USER_ID, SKILL_HVAC, 5).await;

    let s = sweep(&ctx).await;
    assert_eq!(
        s,
        Sweep {
            reminded: 0,
            no_template: 0,
            already_reminded: 0
        },
        "停權者與非證照技能都不該被選中"
    );
    assert_eq!(cert_notifications(&ctx, USER_TECH).await, 0);

    // 復職之後就該提醒 —— 證明上面跳過的原因是狀態，不是別的東西。
    set_user_status(&ctx, USER_TECH, "ACTIVE").await;
    assert_eq!(sweep(&ctx).await.reminded, 2);

    ctx.teardown().await;
}

/// 缺範本要被**計數**，而不是安靜地當成「沒事要做」。
///
/// 而且 `reminded_for_expiry` 仍然要被寫上：否則每天都會重數一次同一筆，
/// warn 會變成永久噪音。這條把那個取捨釘住。
#[tokio::test]
async fn e_a_missing_template_is_counted_not_swallowed() {
    let ctx = TestContext::setup().await;

    {
        let mut tx = ctx.owner_tx().await;
        let n = sqlx::query(
            "UPDATE fms.notification_templates
                SET is_active = false
              WHERE code = 'CERT_EXPIRING'",
        )
        .execute(&mut *tx)
        .await
        .expect("停用範本")
        .rows_affected();
        assert_eq!(n, 2, "059 該建了 EMAIL 與 IN_APP 兩個範本");
        tx.commit().await.expect("commit");
    }

    grant_skill(&ctx, USER_TECH, SKILL_ELECTRICAL, 10).await;
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
    assert_eq!(cert_notifications(&ctx, USER_TECH).await, 0);

    ctx.teardown().await;
}

/// 已經過期的用 `HIGH`：那不是提醒，是違規狀態（他現在不該去做那件事）。
/// 而變數必須真的被代入 —— 一封寫著 `{{skill_name}}` 的信等於沒寄。
#[tokio::test]
async fn f_expired_certifications_are_high_priority_and_rendered() {
    let ctx = TestContext::setup().await;

    grant_skill(&ctx, USER_TECH, SKILL_ELECTRICAL, -3).await;
    assert_eq!(sweep(&ctx).await.reminded, 2);

    let mut tx = ctx.owner_tx().await;
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT channel::text, priority::text, subject::text, body::text
           FROM fms.notifications
          WHERE template_code = 'CERT_EXPIRING' AND recipient_user_id = $1::uuid
          ORDER BY channel",
    )
    .bind(USER_TECH)
    .fetch_all(&mut *tx)
    .await
    .expect("讀通知");
    tx.commit().await.expect("commit");

    assert_eq!(rows.len(), 2, "EMAIL + IN_APP");
    for (channel, priority, subject, body) in &rows {
        assert_eq!(priority, "HIGH", "{channel} 的已過期證照該是 HIGH");
        for text in [subject, body] {
            assert!(
                !text.contains("{{"),
                "{channel} 的內容有未代入的變數：{text}"
            );
        }
        assert!(
            body.contains("電氣"),
            "{channel} 的內容該包含技能名稱：{body}"
        );
    }

    ctx.teardown().await;
}
