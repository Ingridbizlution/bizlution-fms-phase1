#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    // `_guard` 不能寫成 `_`：後者會立刻 drop，匯出器在第一個請求之前就關掉，
    // 而症狀是「collector 什麼都沒收到」，看起來像設定錯誤。
    let _guard = fms_shared::init_telemetry("fms-server");

    let settings = fms_shared::Settings::from_env().map_err(|e| anyhow::anyhow!(e))?;
    let addr = settings.bind_addr.clone();
    let state = fms_server::build_state(settings).await?;
    // 啟動時就建立：設定缺漏該讓程序起不來，而不是第一個上傳才 500。
    let storage = fms_server::build_storage()?;
    let router = fms_server::build_router(
        state,
        storage,
        // 正式部署讀環境變數（`IDP_SECRET_*`）。**不在啟動時把值讀進 Settings** ——
        // 解析發生在用的那一刻，密鑰因此不會躺在任何被 clone 進 handler、
        // 而且有 `Debug` 的結構裡。見 `fms_shared::secrets`。
        std::sync::Arc::new(fms_shared::EnvSecretResolver),
        // ADR-14 決定 G：全平台共用一份，不是密鑰參照——沒設定時日曆整合的
        // 端點仍然能列出/註冊/手動對應，只有真的呼叫 Graph（列外部資源）
        // 那一步會失敗，錯誤訊息會說缺什麼。
        std::env::var("MS365_APP_CLIENT_ID").unwrap_or_default(),
        std::env::var("MS365_APP_CLIENT_SECRET").unwrap_or_default(),
    );

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "fms-server listening");
    axum::serve(listener, router).await?;
    Ok(())
}
