#![cfg(feature = "harness")]
mod support;
use dynapi_catalog::{
    native::{FetchContext, FetchError, Fetcher, NativeHooks, StacCatalog},
    refresh::CatalogConfig,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::*;
use tokio::sync::Semaphore;

struct Fetches {
    paths: Mutex<Vec<String>>,
    fail: AtomicBool,
    hold: AtomicBool,
    release: Semaphore,
    generation: AtomicUsize,
    cancelled: AtomicUsize,
}
impl Fetches {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            paths: Mutex::new(vec![]),
            fail: AtomicBool::new(false),
            hold: AtomicBool::new(false),
            release: Semaphore::new(0),
            generation: AtomicUsize::new(1),
            cancelled: AtomicUsize::new(0),
        })
    }
    fn count(&self) -> usize {
        self.paths.lock().unwrap().len()
    }
    fn catalog(self: &Arc<Self>, clock: Arc<TestClock>) -> StacCatalog {
        StacCatalog::with_dependencies(
            "https://catalog.example.test/catalog.json",
            CatalogConfig::default(),
            clock,
            self.clone(),
            Arc::new(NativeHooks),
        )
        .unwrap()
    }
}
impl Fetcher for Fetches {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        context: FetchContext,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            self.paths.lock().unwrap().push(url.to_owned());
            if self.hold.load(Ordering::SeqCst) {
                tokio::select! {
                    permit = self.release.acquire() => { permit.unwrap().forget(); }
                    _ = context.cancelled() => {
                        self.cancelled.fetch_add(1, Ordering::SeqCst);
                        return Err(FetchError::Transport("Cancelled"));
                    }
                }
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(FetchError::Status(404));
            }
            let body = if url.ends_with("/catalog.json") {
                json!({"links":[{"rel":"child","href":"forecast.json"}]})
            } else {
                let mut collection = input("forecast")["collection"].clone();
                collection["version"] = json!(format!(
                    "generation-{}",
                    self.generation.load(Ordering::SeqCst)
                ));
                collection
            };
            Ok(serde_json::to_vec(&body).unwrap())
        })
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
async fn versions(server: &Server, expected: &str) {
    let listing = server
        .request("GET", "/v1/data-products", &Value::Null)
        .await;
    assert_eq!(listing.status, 200);
    assert_eq!(listing.json()["dataProducts"][0]["version"], expected);
    let forecast = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(
        forecast.status,
        200,
        "{}",
        String::from_utf8_lossy(&forecast.body)
    );
    assert_eq!(
        forecast.json()["results"][0]["dataProduct"]["version"],
        expected
    );
}

#[tokio::test]
async fn startup_catalog_and_forecast_share_one_root_child_flight_and_warm_snapshot() {
    let fetches = Fetches::new();
    fetches.hold.store(true, Ordering::SeqCst);
    let catalog = fetches.catalog(Arc::new(TestClock::default()));
    let server = Server::start_with_catalog(&[], Some(catalog.clone())).await;
    server.app.warm_catalog();
    until(|| fetches.count() == 1).await;
    let listing = server.request("GET", "/v1/data-products", &Value::Null);
    let body = payload();
    let forecast = server.request("POST", "/v1/forecasts", &body);
    let release = async {
        // Both requests encounter the startup flight rather than creating one.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(fetches.count(), 1);
        fetches.release.add_permits(1);
        until(|| fetches.count() == 2).await;
        assert!(catalog.status().refreshing);
        fetches.release.add_permits(1);
    };
    let (listing, forecast, _) = tokio::join!(listing, forecast, release);
    assert_eq!(listing.status, 200);
    assert_eq!(
        forecast.status,
        200,
        "{}",
        String::from_utf8_lossy(&forecast.body)
    );
    assert_eq!(listing.json()["dataProducts"][0]["version"], "generation-1");
    assert_eq!(
        forecast.json()["results"][0]["dataProduct"]["version"],
        "generation-1"
    );
    versions(&server, "generation-1").await;
    assert_eq!(fetches.count(), 2);
    assert!(fetches.paths.lock().unwrap()[1].ends_with("/forecast.json"));
    assert_eq!(catalog.status().consecutive_failures, 0);
}

#[tokio::test]
async fn startup_failure_backoff_and_refresh_publication_are_common_to_both_routes() {
    let fetches = Fetches::new();
    fetches.fail.store(true, Ordering::SeqCst);
    let clock = Arc::new(TestClock::default());
    let catalog = fetches.catalog(clock.clone());
    let server = Server::start_with_catalog(&[], Some(catalog.clone())).await;
    server.app.warm_catalog();
    until(|| catalog.status().consecutive_failures == 1).await;
    let listing = server
        .request("GET", "/v1/data-products", &Value::Null)
        .await;
    let forecast = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!((listing.status, forecast.status), (503, 503));
    assert_eq!(listing.body, forecast.body);
    assert!(!listing.headers.contains_key("retry-after"));
    assert!(!forecast.headers.contains_key("retry-after"));
    assert_eq!(fetches.count(), 1);
    fetches.fail.store(false, Ordering::SeqCst);
    clock.0.store(4, Ordering::SeqCst);
    assert_eq!(
        server
            .request("GET", "/v1/data-products", &Value::Null)
            .await
            .status,
        503
    );
    assert_eq!(fetches.count(), 1);
    clock.0.store(5, Ordering::SeqCst);
    versions(&server, "generation-1").await;
    assert_eq!(fetches.count(), 3);
    assert_eq!(catalog.status().consecutive_failures, 0);
    let first = catalog.snapshot().await.unwrap();
    clock.0.store(305, Ordering::SeqCst);
    fetches.generation.store(2, Ordering::SeqCst);
    fetches.hold.store(true, Ordering::SeqCst);
    // Stale serving and the background refresh are shared, too.
    versions(&server, "generation-1").await;
    until(|| fetches.count() == 4).await;
    fetches.hold.store(false, Ordering::SeqCst);
    fetches.release.add_permits(1);
    until(|| !catalog.status().refreshing).await;
    let second = catalog.snapshot().await.unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    versions(&server, "generation-2").await;
    assert_eq!(fetches.count(), 5);
}

#[tokio::test]
async fn forecast_batch_retains_one_catalog_generation_during_refresh() {
    let fetches = Fetches::new();
    let clock = Arc::new(TestClock::default());
    let catalog = fetches.catalog(clock.clone());
    let server = Server::start_with_catalog(&[], Some(catalog.clone())).await;
    server.app.warm_catalog();
    until(|| catalog.status().loaded_at.is_some()).await;
    server
        .host
        .reads
        .hold_snapshots
        .store(true, Ordering::SeqCst);
    let mut body = payload();
    let query = body["queries"][0].clone();
    body["queries"].as_array_mut().unwrap().push(query);
    let request = server.request("POST", "/v1/forecasts", &body);
    let refresh = async {
        let started = tokio::time::timeout(
            Duration::from_secs(10),
            server.host.reads.snapshot_started.notified(),
        )
        .await;
        if started.is_err() {
            server
                .host
                .reads
                .hold_snapshots
                .store(false, Ordering::SeqCst);
            server.host.reads.release.add_permits(100);
        }
        started.unwrap();
        clock.0.store(300, Ordering::SeqCst);
        fetches.generation.store(2, Ordering::SeqCst);
        server.app.warm_catalog();
        until(|| !catalog.status().refreshing).await;
        assert_eq!(
            catalog.snapshot().await.unwrap().collections["fixture-forecast"].version,
            "generation-2"
        );
        server
            .host
            .reads
            .hold_snapshots
            .store(false, Ordering::SeqCst);
        server.host.reads.release.add_permits(32);
    };
    let (reply, _) = tokio::join!(request, refresh);
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    for result in reply.json()["results"].as_array().unwrap() {
        assert_eq!(result["dataProduct"]["version"], "generation-1");
    }
    versions(&server, "generation-2").await;
    assert_eq!(fetches.count(), 4);
}

#[tokio::test]
async fn abandoned_request_and_temporary_handle_do_not_close_but_final_app_drop_cancels() {
    let fetches = Fetches::new();
    fetches.hold.store(true, Ordering::SeqCst);
    let catalog = fetches.catalog(Arc::new(TestClock::default()));
    let server = Server::start_with_catalog(&[], Some(catalog.clone())).await;
    server.app.warm_catalog();
    until(|| fetches.count() == 1).await;
    drop(catalog.clone());
    let request = server
        .socket("GET", "/v1/data-products", &Value::Null)
        .await;
    drop(request);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(catalog.status().refreshing);
    assert_eq!(fetches.cancelled.load(Ordering::SeqCst), 0);
    fetches.release.add_permits(1);
    until(|| fetches.count() == 2).await;
    // Final owner closes even with an externally retained catalog handle.
    let owner = Arc::downgrade(&server.app);
    drop(server);
    until(|| owner.upgrade().is_none()).await;
    until(|| {
        fetches.cancelled.load(Ordering::SeqCst) == 1 && catalog.status().outstanding_workers == 0
    })
    .await;
    assert!(!catalog.status().refreshing);
    assert_eq!(
        catalog.snapshot().await.unwrap_err().to_string(),
        "STAC catalog is closed"
    );
}

struct FixedHooks;
impl dynapi_catalog::native::RuntimeHooks for FixedHooks {
    fn jitter(&self, low: f64, high: f64) -> f64 {
        assert!(low <= high);
        high
    }
    fn utc_now(&self) -> String {
        "2026-01-01T00:00:00Z".into()
    }
    fn sleep(&self, seconds: f64) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(Duration::from_secs_f64(seconds)))
    }
}
fn detail_catalog(
    fetches: Arc<Fetches>,
    clock: Arc<TestClock>,
    cold_wait_seconds: f64,
) -> StacCatalog {
    StacCatalog::with_dependencies(
        "https://catalog.example.test/catalog.json",
        CatalogConfig {
            cold_wait_seconds,
            ..CatalogConfig::default()
        },
        clock,
        fetches,
        Arc::new(FixedHooks),
    )
    .unwrap()
}
fn assert_detail(reply: &Reply, version: &str) {
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.json()["id"], "fixture-forecast");
    assert_eq!(reply.json()["version"], version);
    assert_eq!(
        reply.headers["cache-control"],
        "public, max-age=300, stale-while-revalidate=600"
    );
    assert!(!reply.headers.contains_key("etag"));
    assert!(!reply.headers.contains_key("retry-after"));
}

#[tokio::test]
async fn detail_startup_timeout_joins_one_flight_through_root_and_child_publication() {
    let fetches = Fetches::new();
    fetches.hold.store(true, Ordering::SeqCst);
    let catalog = detail_catalog(fetches.clone(), Arc::new(TestClock::default()), 0.01);
    let server = Server::start_with_catalog(&[], Some(catalog.clone())).await;
    let _held: Vec<_> = (0..server.app.config.compute_slots)
        .map(|_| server.app.gate.try_acquire().unwrap())
        .collect();
    server.app.warm_catalog();
    until(|| fetches.count() == 1).await;
    let paths = [
        "/v1/data-products/fixture-forecast",
        "/v1/data-products/missing",
        "/v1/data-products/fixture%2Dforecast",
    ];
    for path in paths {
        let reply = server.request("GET", path, &Value::Null).await;
        assert_eq!(reply.status, 503);
        assert_eq!(
            reply.json(),
            json!({"detail":"Timed out waiting for STAC catalog"})
        );
        for header in ["retry-after", "etag", "cache-control"] {
            assert!(!reply.headers.contains_key(header));
        }
        assert_eq!(fetches.count(), 1);
        assert!(catalog.status().refreshing);
    }
    fetches.release.add_permits(1);
    until(|| fetches.count() == 2).await;
    let waiting = server.request("GET", paths[0], &Value::Null).await;
    assert_eq!(waiting.status, 503);
    assert_eq!(fetches.count(), 2);
    assert!(catalog.status().loaded_at.is_none());
    fetches.release.add_permits(1);
    until(|| catalog.status().loaded_at.is_some()).await;
    assert_detail(
        &server.request("GET", paths[0], &Value::Null).await,
        "generation-1",
    );
    assert_detail(
        &server.request("GET", paths[2], &Value::Null).await,
        "generation-1",
    );
    let missing = server.request("GET", paths[1], &Value::Null).await;
    assert_eq!(missing.status, 404);
    assert_eq!(
        missing.json(),
        json!({"detail":"Unknown data product 'missing'"})
    );
    assert_eq!(fetches.count(), 2);
    assert_eq!(catalog.status().consecutive_failures, 0);
}

#[tokio::test]
async fn detail_backoff_stale_failure_and_held_refresh_publish_complete_generations() {
    let fetches = Fetches::new();
    fetches.fail.store(true, Ordering::SeqCst);
    let clock = Arc::new(TestClock::default());
    let catalog = detail_catalog(fetches.clone(), clock.clone(), 10.);
    let server = Server::start_with_catalog(&[], Some(catalog.clone())).await;
    server.app.warm_catalog();
    until(|| catalog.status().consecutive_failures == 1).await;
    for second in [0, 4] {
        clock.0.store(second, Ordering::SeqCst);
        let reply = server
            .request("GET", "/v1/data-products/fixture-forecast", &Value::Null)
            .await;
        assert_eq!(reply.status, 503);
        assert_eq!(
            reply.json(),
            json!({"detail":"STAC catalog refresh failed (HTTPStatusError)"})
        );
        assert!(!reply.headers.contains_key("retry-after"));
        assert_eq!(fetches.count(), 1);
    }
    fetches.fail.store(false, Ordering::SeqCst);
    clock.0.store(5, Ordering::SeqCst);
    assert_detail(
        &server
            .request("GET", "/v1/data-products/fixture-forecast", &Value::Null)
            .await,
        "generation-1",
    );
    assert_eq!(fetches.count(), 3);
    let original = catalog.snapshot().await.unwrap();
    clock.0.store(305, Ordering::SeqCst);
    fetches.fail.store(true, Ordering::SeqCst);
    fetches.hold.store(true, Ordering::SeqCst);
    assert_detail(
        &server
            .request("GET", "/v1/data-products/fixture-forecast", &Value::Null)
            .await,
        "generation-1",
    );
    until(|| fetches.count() == 4).await;
    for _ in 0..3 {
        assert_detail(
            &server
                .request("GET", "/v1/data-products/fixture-forecast", &Value::Null)
                .await,
            "generation-1",
        );
    }
    assert_eq!(fetches.count(), 4);
    fetches.release.add_permits(1);
    until(|| catalog.status().consecutive_failures == 1 && !catalog.status().refreshing).await;
    assert!(Arc::ptr_eq(&original, &catalog.snapshot().await.unwrap()));
    clock.0.store(334, Ordering::SeqCst);
    assert_detail(
        &server
            .request("GET", "/v1/data-products/fixture-forecast", &Value::Null)
            .await,
        "generation-1",
    );
    assert_eq!(fetches.count(), 4);
    fetches.fail.store(false, Ordering::SeqCst);
    fetches.generation.store(2, Ordering::SeqCst);
    clock.0.store(335, Ordering::SeqCst);
    assert_detail(
        &server
            .request("GET", "/v1/data-products/fixture-forecast", &Value::Null)
            .await,
        "generation-1",
    );
    until(|| fetches.count() == 5).await;
    fetches.release.add_permits(1);
    until(|| fetches.count() == 6).await;
    assert_detail(
        &server
            .request("GET", "/v1/data-products/fixture-forecast", &Value::Null)
            .await,
        "generation-1",
    );
    assert!(Arc::ptr_eq(&original, &catalog.snapshot().await.unwrap()));
    fetches.release.add_permits(1);
    until(|| !catalog.status().refreshing).await;
    assert_detail(
        &server
            .request("GET", "/v1/data-products/fixture-forecast", &Value::Null)
            .await,
        "generation-2",
    );
    assert!(!Arc::ptr_eq(&original, &catalog.snapshot().await.unwrap()));
    assert_eq!(catalog.status().consecutive_failures, 0);
    assert_eq!(fetches.count(), 6);
}
