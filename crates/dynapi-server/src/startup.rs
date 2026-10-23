use dynapi_server::{
    App,
    telemetry::{StdoutSink, Stream, Telemetry},
};
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

/// Observe the first flight without applying the request cold-wait limit,
/// starting retries, or cancelling the flight when this wait expires.
pub async fn wait_for_catalog(app: &App) -> &'static str {
    app.warm_catalog();
    let deadline = Instant::now() + app.config.catalog_wait;
    loop {
        let status = app.catalog_status();
        if status.loaded_at.is_some() {
            return "ready";
        }
        if !status.refreshing {
            return "failed";
        }
        if Instant::now() >= deadline {
            return "timeout";
        }
        tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(10))).await;
    }
}

pub async fn report_catalog_ready(app: Arc<App>, stdout: Arc<StdoutSink>, bound: Instant) {
    loop {
        if app.catalog_status().loaded_at.is_some() {
            stdout.emit(
                Stream::Terminal,
                serde_json::json!({
                    "event": "api_catalog_ready", "catalogReady": true,
                    "bindToCatalogReadyMs": bound.elapsed().as_secs_f64() * 1000.,
                }),
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
