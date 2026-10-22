#![cfg(feature = "native")]
mod support;
use dynapi_catalog::native::{FetchContext, FetchError, Fetcher, RuntimeHooks, StacCatalog};
use dynapi_catalog::refresh::CatalogConfig;
use futures::future::BoxFuture;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use support::FakeClock;
use tokio::sync::Semaphore;
const ROOT: &[u8] = br#"{"links":[{"rel":"child","href":"child.json"}]}"#;
const CHILD:&[u8]=br#"{"id":"example","version":"1","assets":{"icechunk":{"href":"https://data.dynamical.org/example/v1/"}}}"#;
struct Hooks {
    clock: Arc<FakeClock>,
    sleeps: Mutex<Vec<f64>>,
}
impl RuntimeHooks for Hooks {
    fn jitter(&self, _: f64, b: f64) -> f64 {
        b
    }
    fn utc_now(&self) -> String {
        "2026-10-21T00:00:00+00:00".into()
    }
    fn sleep(&self, seconds: f64) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.sleeps.lock().unwrap().push(seconds);
            self.clock.set(self.clock.now() + seconds);
        })
    }
}
use dynapi_catalog::Clock;
struct Mock {
    calls: AtomicUsize,
    fail: AtomicBool,
    block: AtomicBool,
    gate: Semaphore,
    root: Vec<u8>,
    child: Mutex<Vec<u8>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}
impl Mock {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            block: AtomicBool::new(false),
            gate: Semaphore::new(0),
            root: ROOT.into(),
            child: Mutex::new(CHILD.into()),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }
}
impl Fetcher for Mock {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        _context: FetchContext,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(FetchError::Status(404));
            }
            if self.block.load(Ordering::SeqCst) {
                let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(n, Ordering::SeqCst);
                self.gate.acquire().await.unwrap().forget();
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(if url.ends_with("root.json") {
                self.root.clone()
            } else {
                self.child.lock().unwrap().clone()
            })
        })
    }
}
fn service(fetch: Arc<Mock>, clock: Arc<FakeClock>, config: CatalogConfig) -> StacCatalog {
    StacCatalog::with_dependencies(
        "https://stac.example.test/root.json",
        config,
        clock.clone(),
        fetch,
        Arc::new(Hooks {
            clock,
            sleeps: Mutex::new(vec![]),
        }),
    )
    .unwrap()
}
async fn eventually(predicate: impl Fn() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn cold_and_stale_readers_share_one_flight_and_keep_snapshot() {
    let fetch = Arc::new(Mock::new());
    let clock = Arc::new(FakeClock::default());
    let catalog = service(fetch.clone(), clock.clone(), CatalogConfig::default());
    fetch.block.store(true, Ordering::SeqCst);
    catalog.warm();
    eventually(|| fetch.calls.load(Ordering::SeqCst) == 1).await;
    let mut jobs = vec![];
    for _ in 0..12 {
        let c = catalog.clone();
        jobs.push(tokio::spawn(
            async move { c.list_collections().await.unwrap() },
        ));
    }
    tokio::task::yield_now().await;
    assert_eq!(fetch.calls.load(Ordering::SeqCst), 1);
    fetch.block.store(false, Ordering::SeqCst);
    fetch.gate.add_permits(1);
    for job in jobs {
        assert_eq!(job.await.unwrap()[0].id, "example");
    }
    assert_eq!(fetch.calls.load(Ordering::SeqCst), 2);
    clock.set(300.);
    fetch.block.store(true, Ordering::SeqCst);
    for _ in 0..32 {
        assert_eq!(catalog.list_collections().await.unwrap()[0].version, "1");
    }
    eventually(|| fetch.calls.load(Ordering::SeqCst) == 3).await;
    assert!(catalog.status().refreshing);
    catalog.warm();
    assert_eq!(fetch.calls.load(Ordering::SeqCst), 3);
    fetch.block.store(false, Ordering::SeqCst);
    fetch.gate.add_permits(1);
    eventually(|| !catalog.status().refreshing).await;
    catalog.close();
}
#[tokio::test]
async fn failure_backoff_stale_reads_and_recovery() {
    let fetch = Arc::new(Mock::new());
    let clock = Arc::new(FakeClock::default());
    let catalog = service(fetch.clone(), clock.clone(), CatalogConfig::default());
    assert_eq!(catalog.list_collections().await.unwrap().len(), 1);
    fetch.fail.store(true, Ordering::SeqCst);
    clock.set(301.);
    assert_eq!(catalog.list_collections().await.unwrap().len(), 1);
    eventually(|| !catalog.status().refreshing).await;
    assert_eq!(
        catalog.status().last_error.as_deref(),
        Some("STAC catalog refresh failed (HTTPStatusError)")
    );
    let count = fetch.calls.load(Ordering::SeqCst);
    clock.set(330.99);
    catalog.warm();
    assert_eq!(fetch.calls.load(Ordering::SeqCst), count);
    clock.set(331.);
    fetch.fail.store(false, Ordering::SeqCst);
    catalog.warm();
    eventually(|| !catalog.status().refreshing).await;
    assert_eq!(catalog.status().consecutive_failures, 0);
    catalog.close();
}
#[tokio::test]
async fn cold_wait_timeout_late_result_and_close() {
    let fetch = Arc::new(Mock::new());
    let clock = Arc::new(FakeClock::default());
    let config = CatalogConfig {
        cold_wait_seconds: 0.01,
        load_deadline_seconds: 0.05,
        ..CatalogConfig::default()
    };
    let catalog = service(fetch.clone(), clock.clone(), config);
    fetch.block.store(true, Ordering::SeqCst);
    assert_eq!(
        catalog.list_collections().await.unwrap_err().to_string(),
        "Timed out waiting for STAC catalog"
    );
    assert!(catalog.status().refreshing);
    eventually(|| !catalog.status().refreshing).await;
    assert_eq!(catalog.status().outstanding_workers, 1);
    assert!(catalog.status().loaded_at.is_none());
    clock.set(5.);
    fetch.block.store(false, Ordering::SeqCst);
    assert_eq!(catalog.list_collections().await.unwrap()[0].version, "1");
    fetch.gate.add_permits(1);
    eventually(|| catalog.status().outstanding_workers == 0).await;
    catalog.close();
    catalog.close();
    catalog.warm();
    assert_eq!(
        catalog.list_collections().await.unwrap_err().to_string(),
        "STAC catalog is closed"
    );
}
#[tokio::test]
async fn invalid_child_skipped_all_invalid_and_bad_json_fail() {
    for (body, expected) in [
        (
            b"{}".as_slice(),
            "STAC catalog refresh failed (CatalogError)",
        ),
        (
            b"{".as_slice(),
            "STAC catalog refresh failed (JSONDecodeError)",
        ),
    ] {
        let fetch = Arc::new(Mock::new());
        *fetch.child.lock().unwrap() = body.to_vec();
        let catalog = service(
            fetch,
            Arc::new(FakeClock::default()),
            CatalogConfig::default(),
        );
        assert_eq!(
            catalog.list_collections().await.unwrap_err().to_string(),
            expected
        );
        catalog.close();
    }
    struct Mixed;
    impl Fetcher for Mixed {
        fn fetch<'a>(
            &'a self,
            url: &'a str,
            _: FetchContext,
        ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
            Box::pin(async move {
                Ok(if url.ends_with("root.json") {
                    br#"{"links":[{"rel":"child","href":"a.json"},{"rel":"child","href":"b.json"}]}"#.to_vec()
                } else if url.ends_with("a.json") {
                    b"{}".to_vec()
                } else {
                    CHILD.to_vec()
                })
            })
        }
    }
    let clock = Arc::new(FakeClock::default());
    let catalog = StacCatalog::with_dependencies(
        "https://stac.example.test/root.json",
        CatalogConfig::default(),
        clock.clone(),
        Arc::new(Mixed),
        Arc::new(Hooks {
            clock,
            sleeps: Mutex::new(vec![]),
        }),
    )
    .unwrap();
    assert_eq!(catalog.list_collections().await.unwrap().len(), 1);
    catalog.close();
}
#[tokio::test]
async fn transient_retries_bounded_and_timeouts_shrink() {
    struct Retry {
        calls: AtomicUsize,
        timeouts: Mutex<Vec<f64>>,
    }
    impl Fetcher for Retry {
        fn fetch<'a>(
            &'a self,
            url: &'a str,
            ctx: FetchContext,
        ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
            Box::pin(async move {
                self.timeouts
                    .lock()
                    .unwrap()
                    .push(ctx.request_timeout().unwrap());
                let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    return Err(FetchError::Status(503));
                }
                Ok(if url.ends_with("root.json") {
                    ROOT.into()
                } else {
                    CHILD.into()
                })
            })
        }
    }
    let clock = Arc::new(FakeClock::default());
    let hooks = Arc::new(Hooks {
        clock: clock.clone(),
        sleeps: Mutex::new(vec![]),
    });
    let fetch = Arc::new(Retry {
        calls: AtomicUsize::new(0),
        timeouts: Mutex::new(vec![]),
    });
    let catalog = StacCatalog::with_dependencies(
        "https://stac.example.test/root.json",
        CatalogConfig {
            load_deadline_seconds: 7.,
            ..CatalogConfig::default()
        },
        clock,
        fetch.clone(),
        hooks.clone(),
    )
    .unwrap();
    catalog.list_collections().await.unwrap();
    assert_eq!(*hooks.sleeps.lock().unwrap(), [1., 1.]);
    assert_eq!(*fetch.timeouts.lock().unwrap(), [7., 6., 5., 5.]);
    catalog.close();
}
#[path = "support/fixtures.rs"]
mod fixtures;
#[tokio::test]
async fn twelve_worker_limit_survives_retired_generations_and_cancels_queued_work() {
    struct BlockChildren {
        active: AtomicUsize,
        peak: AtomicUsize,
        calls: AtomicUsize,
        gate: Semaphore,
    }
    impl Fetcher for BlockChildren {
        fn fetch<'a>(
            &'a self,
            url: &'a str,
            _: FetchContext,
        ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
            Box::pin(async move {
                if url.ends_with("root.json") {
                    return Ok(serde_json::to_vec(&serde_json::json!({"links":(0..30).map(|i|serde_json::json!({"rel":"child","href":format!("{i}.json")})).collect::<Vec<_>>()})).unwrap());
                }
                self.calls.fetch_add(1, Ordering::SeqCst);
                let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(n, Ordering::SeqCst);
                self.gate.acquire().await.unwrap().forget();
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(CHILD.to_vec())
            })
        }
    }
    let fetch = Arc::new(BlockChildren {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        gate: Semaphore::new(0),
    });
    let clock = Arc::new(FakeClock::default());
    let catalog = StacCatalog::with_dependencies(
        "https://stac.example.test/root.json",
        CatalogConfig {
            cold_wait_seconds: 0.01,
            load_deadline_seconds: 0.05,
            ..CatalogConfig::default()
        },
        clock.clone(),
        fetch.clone(),
        Arc::new(Hooks {
            clock: clock.clone(),
            sleeps: Mutex::new(vec![]),
        }),
    )
    .unwrap();
    for (generation, instant) in [0., 5., 65., 185.].into_iter().enumerate() {
        clock.set(instant);
        assert!(catalog.list_collections().await.is_err());
        eventually(|| !catalog.status().refreshing).await;
        eventually(|| catalog.status().outstanding_workers == 12).await;
        assert_eq!(catalog.status().consecutive_failures, generation as u64 + 1);
        assert_eq!(fetch.peak.load(Ordering::SeqCst), 12);
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 12);
        assert!(catalog.status().loaded_at.is_none());
    }
    catalog.close();
    fetch.gate.add_permits(12);
    eventually(|| catalog.status().outstanding_workers == 0).await;
    assert_eq!(fetch.calls.load(Ordering::SeqCst), 12);
}

#[test]
fn warm_is_best_effort_without_an_async_runtime() {
    let catalog = service(
        Arc::new(Mock::new()),
        Arc::new(FakeClock::default()),
        CatalogConfig::default(),
    );
    catalog.warm();
    assert_eq!(catalog.status().consecutive_failures, 1);
    assert!(!catalog.status().refreshing);
    assert_eq!(
        catalog.status().last_error.as_deref(),
        Some("STAC catalog refresh failed (RuntimeError)")
    );
    catalog.close();
}

#[tokio::test]
async fn public_stac_graphs_refresh_offline_with_input_derived_membership() {
    use std::collections::{BTreeSet, HashMap};
    struct Documents(HashMap<String, Vec<u8>>);
    impl Fetcher for Documents {
        fn fetch<'a>(
            &'a self,
            url: &'a str,
            _: FetchContext,
        ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
            Box::pin(async move {
                Ok(self
                    .0
                    .get(url)
                    .expect("fixture URL must be mapped offline")
                    .clone())
            })
        }
    }
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/stac/manifest.json")).unwrap();
    for (prefix, url, count) in [
        ("production-", "https://stac.dynamical.org/catalog.json", 36),
        (
            "staging-",
            "https://stac-staging.dynamical.org/catalog.json",
            37,
        ),
    ] {
        let mut docs = HashMap::new();
        let mut ids = BTreeSet::new();
        for entry in manifest["documents"].as_array().unwrap() {
            let file = entry["file"].as_str().unwrap();
            if !file.starts_with(prefix) {
                continue;
            }
            let body = fixtures::fixture(file.trim_end_matches(".gz"));
            let input: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if input["type"] == "Collection" {
                ids.insert(input["id"].as_str().unwrap().to_owned());
            }
            docs.insert(entry["url"].as_str().unwrap().to_owned(), body);
        }
        assert_eq!(ids.len(), count);
        let clock = Arc::new(FakeClock::default());
        let catalog = StacCatalog::with_dependencies(
            url,
            CatalogConfig::default(),
            clock.clone(),
            Arc::new(Documents(docs)),
            Arc::new(Hooks {
                clock,
                sleeps: Mutex::new(vec![]),
            }),
        )
        .unwrap();
        let actual = catalog.list_collections().await.unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|collection| collection.id.clone())
                .collect::<BTreeSet<_>>(),
            ids
        );
        assert_eq!(actual.len(), count);
        for collection in actual {
            assert!(!collection.variables.is_empty());
            assert!(!collection.dimensions.is_empty());
            assert!(matches!(collection.kind(), "forecast" | "analysis"));
            assert!(collection.point_query_reason().is_none());
            for variable in collection.variables.values() {
                assert!(
                    variable
                        .dimensions
                        .iter()
                        .all(|name| collection.dimensions.contains_key(name))
                );
            }
        }
        catalog.close();
    }
}
