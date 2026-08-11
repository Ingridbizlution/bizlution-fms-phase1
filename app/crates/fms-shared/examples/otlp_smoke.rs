//! 送一個帶標記的 span 出去，用來驗證 OTLP 匯出真的接通了。
//!
//! 這支存在的理由是**驗證必須走我們自己的程式碼**。直接用 curl 對 collector
//! 送一份 OTLP payload 也會在 log 裡看到東西 —— 但那證明的是 collector 會收，
//! 不是 `init_telemetry` 會送。兩者之間正好是 WBS 1.7 記的那個缺口
//! （「exporter 已備妥開關，但未接上」）。
//!
//! 用 example 而不是 `#[test]`：tracing 的 subscriber 是**全域、只能設一次**
//! 的，而 `cargo test` 會把同一個 binary 裡的測試跑在同一個 process 裡。
//! 一個獨立的 process 讓「初始化 → 送出 → flush → 結束」這條路徑與正式
//! 執行檔完全一樣，包含 `TelemetryGuard` 在 drop 時 flush 那一段 ——
//! 而那一段正是最容易漏掉、且漏掉時症狀是「collector 什麼都沒收到」的地方。
//!
//! 用法（由 docker/scripts/otel-smoke.sh 呼叫）：
//!
//!     OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
//!     OTLP_SMOKE_MARKER=<唯一字串> cargo run -p fms-shared --example otlp_smoke
//!
//! **必須是 `#[tokio::main]`。** 批次匯出器的背景任務需要 runtime；同步 main
//! 的話 span 會攢在 channel 裡誰也沒送出去，而關閉時只會看到
//! 「channel is empty and sending half is closed」。第一版就是這樣，
//! 接收端一個 POST 都沒收到 —— 而那是**安靜的**失敗，
//! 所以 `init_telemetry` 現在會在 runtime 外被呼叫時明確警告。

#[tokio::main]
async fn main() {
    let marker = std::env::var("OTLP_SMOKE_MARKER").unwrap_or_else(|_| "no-marker".into());

    if fms_shared::otlp_endpoint().is_none() {
        eprintln!(
            "OTEL_EXPORTER_OTLP_ENDPOINT 未設定 —— 這支程式沒有意義。\n\
             init_telemetry 在沒有 endpoint 時刻意只裝 JSON log。"
        );
        std::process::exit(2);
    }

    // guard 必須活到 span 送出去為止。這裡刻意示範正確的寫法。
    let guard = fms_shared::init_telemetry("fms-otlp-smoke");

    {
        let span = tracing::info_span!("otlp_smoke", smoke_marker = %marker);
        let _enter = span.enter();
        tracing::info!(smoke_marker = %marker, "smoke span emitted");
    }

    // 明確 drop 而不是等 main 結束：讓「flush 發生在這裡」這件事看得見。
    drop(guard);
    println!("已送出 span，marker={marker}");
}
