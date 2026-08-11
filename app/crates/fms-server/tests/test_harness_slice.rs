//! 測試腳手架自己的測試。
//!
//! 這裡只有一格，守的是 `common::TestContext::teardown` 的逾時保護 ——
//! 也就是把「漏掉一條連線」從**無限卡住**變成**會失敗的測試**的那一段。
//!
//! 為什麼值得一格：那個保護的失效方式是**靜默**的。sqlx 之後若改掉
//! `Pool::close()` 的號誌邏輯、或有人把 `assert!` 拿掉，保護就沒了，
//! 而 CI 一路綠燈，直到下一次有人漏掉一個 `drop` 才會再花掉 30 分鐘。
//! 沒有被執行過的保護等於沒有保護。

mod common;

use common::TestContext;

/// 漏掉一條連線時，teardown 必須**失敗**而不是卡住。
///
/// # 為什麼這樣佈置就一定重現
///
/// `Pool::close()` 會等到 `max_connections` 個號誌全部拿得到。被握著的連線
/// 永遠不還，缺口只能靠「close() 關掉 idle 連線時釋出的號誌」補 ——
/// 因此**當下 idle 佇列是空的**就補不上，close() 永久等待。
///
/// `setup()` 之後連線池裡剛好只有一條連線（`connect()` 建的那條，已歸還為
/// idle）。這裡立刻 `tenant_tx()` 把它取走，idle 歸零、size 為 1 ——
/// 正是那個條件。不打任何 HTTP 請求是刻意的：只要打過一個請求就會有第二條
/// 連線躺在 idle 裡，缺口就補得上，這一格也就測不到東西了。
/// （`state_machine` 當初的間歇性正是來自這裡：卡不卡取決於上一個請求的
///  連線有沒有在 close() 之前被背景 task 歸還。）
#[tokio::test]
#[should_panic(expected = "pool.close() 逾時")]
async fn a_leaked_connection_fails_teardown_instead_of_hanging() {
    let ctx = TestContext::setup().await;
    let _leaked = ctx.tenant_tx().await;
    ctx.teardown().await;
}
