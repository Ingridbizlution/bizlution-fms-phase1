//! 可觀測性：每個請求一個帶租戶標籤的 span。
//!
//! # 為什麼 `tenant_id` 必須在 span 上，而不只是在錯誤訊息裡
//!
//! 這是多租戶系統，而生產環境最常見的問題形態是「**某一個租戶**的某個功能
//! 壞了」。沒有 `tenant_id` 標籤，log 只能整批看，無法回答
//! 「這個客戶這一小時的錯誤率是多少」——而那正是支援工單進來時的第一個問題。
//!
//! span 欄位會被 `tracing-subscriber` 的 JSON 格式器寫進**每一筆**該請求
//! 產生的 log，包含底層 crate 發出的。這是它比「在每個 log! 手動帶 tenant_id」
//! 更可靠的原因：後者只要漏一處就在需要時斷掉。
//!
//! # 與 OpenTelemetry 的關係
//!
//! 這一層產生的是**結構化的 span**，本身不依賴 OTLP。要匯出到
//! collector 時由 `init_telemetry` 掛上 `tracing-opentelemetry`，
//! 同一組 span 就同時成為 trace。兩者刻意分開：即使沒有 collector，
//! 租戶標籤仍然在 log 裡 —— 可觀測性不該全有或全無。

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tracing::field::Empty;

/// 每個請求建立一個 span，並掛上租戶與使用者。
///
/// 放在 `require_auth` **之後**：此時 `Caller` 已經過 JWT `tid` 與
/// `X-Tenant-ID` 的交叉驗證，因此記下的租戶是**已驗證的**，
/// 而不是客戶端自稱的。直接讀標頭會讓 log 可以被偽造。
pub async fn tenant_span(request: Request, next: Next) -> Response {
    // Caller 由 require_auth 放進 extensions。取不到就留空 ——
    // 未認證的路徑（health、token）本來就沒有租戶。
    let caller = request
        .extensions()
        .get::<crate::context::Caller>()
        .copied();

    let span = tracing::info_span!(
        "http_request",
        method = %request.method(),
        path = %request.uri().path(),
        tenant_id = Empty,
        user_id = Empty,
        status = Empty,
    );
    if let Some(c) = caller {
        span.record("tenant_id", tracing::field::display(c.tenant_id));
        span.record("user_id", tracing::field::display(c.user_id));
    }

    let started = std::time::Instant::now();
    let response = {
        use tracing::Instrument;
        next.run(request).instrument(span.clone()).await
    };
    // 狀態碼在回應之後才知道，因此事後記錄到同一個 span 上。
    span.record("status", response.status().as_u16());

    // 明確發出一筆存取記錄，而不是只依賴 span 的存在。
    //
    // 只建立 span 不發事件時，log 裡**什麼都不會出現** —— span 欄位要靠
    // subscriber 設定（`with_span_events`）才會被輸出，而那是部署方的設定，
    // 不該是「租戶標籤有沒有進 log」的決定因素。一筆明確的請求完成事件
    // 讓這件事與 subscriber 設定無關。
    //
    // 延遲一併記下：它與 tenant_id 在同一筆記錄裡，才能回答
    // 「這個客戶的 p99 是多少」。
    let _enter = span.enter();
    tracing::info!(
        latency_ms = started.elapsed().as_millis() as u64,
        status = response.status().as_u16(),
        "request completed"
    );
    drop(_enter);

    response
}

/// 由環境變數決定是否匯出到 OpenTelemetry collector。
///
/// **未設 `OTEL_EXPORTER_OTLP_ENDPOINT` 時完全不啟用匯出**，只保留 JSON log。
/// 這是刻意的預設：開發與 CI 不需要 collector，而一個連不上 collector 的
/// exporter 會在每次請求後印出連線錯誤，把真正的 log 淹掉。
pub fn otlp_endpoint() -> Option<String> {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

// =============================================================================
// OpenTelemetry 匯出
// =============================================================================

/// 持有 tracer provider；drop 時 flush 並關閉。
///
/// **必須留在 `main` 的作用域裡**（`let _guard = init_telemetry(...)`）。
/// 批次匯出器把 span 攢在記憶體裡背景送出，程序直接結束會丟掉最後一批 ——
/// 而那一批往往正是你在追的那次當機前的 span。
///
/// 命名成 `_guard` 而不是 `_`：後者會**立刻** drop，於是匯出器在第一個
/// 請求之前就關掉了，而症狀是「collector 什麼都沒收到」，看起來像設定錯誤。
#[must_use = "drop 掉這個 guard 會立刻關閉匯出器，最後一批 span 會遺失"]
pub struct TelemetryGuard {
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(p) = self.provider.take() {
            // **flush 與 shutdown 分開回報。** 這兩件事的失敗意義完全不同：
            //   flush 失敗    → span 沒送出去（真的丟資料）
            //   shutdown 失敗 → 送出去了但收尾不乾淨（通常無害）
            //
            // 合在一起看的話，「shutdown 抱怨 channel 已關」會被誤讀成
            // 「span 遺失」，而那兩者要採取的行動不一樣。實測遇過只有後者
            // 出現的情形，當時分不出是哪一種。
            //
            // 這裡用 eprintln 而不是 tracing：subscriber 可能已經在拆了。
            if let Err(e) = p.force_flush() {
                eprintln!("OTLP flush 失敗 —— 最後一批 span 確實遺失了：{e}");
            }
            if let Err(e) = p.shutdown() {
                eprintln!("OTLP 匯出器關閉不乾淨（flush 若成功則 span 已送出）：{e}");
            }
        }
    }
}

/// 初始化 log 與（若有設定 endpoint）trace 匯出。
///
/// 這支函式是這個模組檔頭承諾了很久的東西 —— 檔頭寫著「要匯出到 collector
/// 時由 `init_telemetry` 掛上 `tracing-opentelemetry`」，而它一直不存在。
/// WBS 1.7 把這件事記成「exporter 已備妥開關，但未接上，也未經驗證」。
///
/// **沒設 `OTEL_EXPORTER_OTLP_ENDPOINT` 時只裝 JSON log。** 這是刻意的預設：
/// 開發與 CI 不需要 collector，而一個連不上 collector 的匯出器會在背景不斷
/// 重試並印出連線錯誤，把真正的 log 淹掉。
pub fn init_telemetry(service_name: &'static str) -> TelemetryGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer().json();

    // **批次匯出器需要 tokio runtime。** 在 runtime 外呼叫的話，背景匯出任務
    // 跑不起來，於是 span 攢在 channel 裡誰也沒送出去 —— 而關閉時只會看到
    // 「channel is empty and sending half is closed」，看不出真正的原因。
    //
    // 實測踩到：第一版的 otlp_smoke example 是同步 main，結果接收端一個 POST
    // 都沒收到。兩個正式執行檔都是 `#[tokio::main]` 所以沒事，但這個前提
    // 原本完全隱含 —— 而它失敗的方式是**安靜的**。
    if otlp_endpoint().is_some() && tokio::runtime::Handle::try_current().is_err() {
        eprintln!(
            "init_telemetry：設了 OTEL_EXPORTER_OTLP_ENDPOINT 但不在 tokio runtime 內。\n\
             批次匯出器需要 runtime，否則 span 不會送出去。請在 #[tokio::main] 之內呼叫。"
        );
    }

    let Some(endpoint) = otlp_endpoint() else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
        return TelemetryGuard { provider: None };
    };

    // `catch_unwind` 而不只是 match Err：**Err 路徑不夠**。實測 reqwest 的
    // Client 在缺 rustls crypto provider 時是 **panic** 而不是回 Err，
    // 於是下面那段「只保留 JSON log」完全接不到，服務直接死在啟動。
    //
    // 上游的 feature 已經修好那個特定原因（見 Cargo.toml），但這裡要守的是
    // **承諾本身**：可觀測性是輔助，不是相依。下一個在初始化路徑上 panic 的
    // 依賴不會事先通知我們。
    let built = std::panic::catch_unwind(|| build_provider(&endpoint, service_name))
        .unwrap_or_else(|_| Err("匯出器初始化時 panic（已攔下）".into()));

    match built {
        Ok(provider) => {
            let tracer = opentelemetry::trace::TracerProvider::tracer(&provider, service_name);
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .init();
            tracing::info!(%endpoint, %service_name, "OTLP trace 匯出已啟用");
            TelemetryGuard {
                provider: Some(provider),
            }
        }
        // **匯出器建不起來不該讓服務起不來。** 可觀測性是輔助，不是相依。
        // 但也不能安靜 —— 沒有這一行，症狀會是「trace 就是沒出現」而沒有人
        // 知道為什麼。
        Err(e) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .init();
            tracing::error!(%endpoint, error = %e, "OTLP 匯出器建立失敗 —— 只保留 JSON log");
            TelemetryGuard { provider: None }
        }
    }
}

fn build_provider(
    endpoint: &str,
    service_name: &'static str,
) -> Result<opentelemetry_sdk::trace::SdkTracerProvider, Box<dyn std::error::Error>> {
    use opentelemetry_otlp::WithExportConfig;

    // reqwest 是以 `rustls-no-provider` 編進來的，因此**必須先裝一個 crypto
    // provider**，否則建立 Client 時 panic。`install_default` 在已經有 provider
    // 時回 Err —— 那不是錯誤（可能是別的 crate 先裝了），忽略它。
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{}/v1/traces", endpoint.trim_end_matches('/')))
        .build()?;

    Ok(opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(service_name)
                .build(),
        )
        .build())
}
