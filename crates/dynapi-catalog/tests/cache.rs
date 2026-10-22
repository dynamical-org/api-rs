mod support;
use dynapi_catalog::cache::{CacheStatus, DatasetCache, DatasetKey, Profile, Resolved};
use dynapi_catalog::{CatalogError, Collection};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::FakeClock;
fn key(id: &str, snapshot: Option<&str>) -> DatasetKey {
    let value = serde_json::json!({"id":id,"assets":{"icechunk":{"href":"s3://example-bucket/data/","xarray:storage_options":{"client_kwargs":{"region_name":"us-west-2"}}}}});
    DatasetKey::new(
        &Collection::from_stac(&value).unwrap(),
        snapshot,
        Profile::Default,
    )
}
fn open(
    cache: &DatasetCache<Arc<FakeClock>, usize>,
    key: DatasetKey,
    head: &str,
    count: &AtomicUsize,
) -> dynapi_catalog::cache::OpenedDataset<usize> {
    cache
        .open(
            key,
            || {
                Ok::<_, CatalogError>(Resolved {
                    snapshot_id: head.into(),
                    session: (),
                })
            },
            |_| Ok(count.fetch_add(1, Ordering::SeqCst)),
        )
        .unwrap()
}
#[test]
fn main_ttl_revalidation_changed_head_and_failures() {
    let clock = Arc::new(FakeClock::default());
    let cache = DatasetCache::new(clock.clone(), 30., 4).unwrap();
    let count = AtomicUsize::new(0);
    let first = open(&cache, key("a", None), "s1", &count);
    assert_eq!(first.cache_status, CacheStatus::Miss);
    clock.set(29.99);
    assert_eq!(
        open(&cache, key("a", None), "s2", &count).cache_status,
        CacheStatus::Hit
    );
    clock.set(30.);
    let failed = cache.open(
        key("a", None),
        || Err::<Resolved<()>, _>("storage unavailable"),
        |_| Ok(10),
    );
    assert_eq!(failed.unwrap_err(), "storage unavailable");
    let revalidated = open(&cache, key("a", None), "s1", &count);
    assert_eq!(revalidated.cache_status, CacheStatus::Revalidated);
    assert!(Arc::ptr_eq(&first.dataset, &revalidated.dataset));
    clock.set(60.);
    let changed = open(&cache, key("a", None), "s2", &count);
    assert_eq!(changed.cache_status, CacheStatus::Miss);
    assert_eq!(changed.snapshot_id, "s2");
    assert!(!Arc::ptr_eq(&first.dataset, &changed.dataset));
    assert_eq!(count.load(Ordering::SeqCst), 2);
}
#[test]
fn pinned_no_ttl_shared_lru_bounds_and_touch_order() {
    let clock = Arc::new(FakeClock::default());
    let cache = DatasetCache::new(clock.clone(), 30., 2).unwrap();
    let count = AtomicUsize::new(0);
    open(&cache, key("a", Some("pin")), "pin", &count);
    open(&cache, key("a", None), "pin", &count);
    clock.set(10_000.);
    assert_eq!(
        open(&cache, key("a", Some("pin")), "pin", &count).cache_status,
        CacheStatus::Hit
    );
    open(&cache, key("b", None), "pin", &count); // evicts expired main, not recently touched pin
    assert_eq!(
        open(&cache, key("a", Some("pin")), "pin", &count).cache_status,
        CacheStatus::Hit
    );
    assert_eq!(
        open(&cache, key("a", None), "pin", &count).cache_status,
        CacheStatus::Miss
    );
    assert_eq!(cache.len(), 2);
    assert_eq!(count.load(Ordering::SeqCst), 4);
}
#[test]
fn ttl_starts_before_resolution_and_zero_settings_disable_reuse() {
    let clock = Arc::new(FakeClock::default());
    let cache = DatasetCache::new(clock.clone(), 30., 4).unwrap();
    let count = AtomicUsize::new(0);
    open(&cache, key("a", None), "s1", &count);
    clock.set(31.);
    let result = cache
        .open(
            key("a", None),
            || {
                clock.set(36.);
                Ok::<_, ()>(Resolved {
                    snapshot_id: "s1".into(),
                    session: (),
                })
            },
            |_| Ok(99),
        )
        .unwrap();
    assert_eq!(result.cache_status, CacheStatus::Revalidated);
    clock.set(60.5);
    assert_eq!(
        open(&cache, key("a", None), "s1", &count).cache_status,
        CacheStatus::Hit
    );
    clock.set(61.);
    assert_eq!(
        open(&cache, key("a", None), "s1", &count).cache_status,
        CacheStatus::Revalidated
    );
    for (ttl, entries) in [(0., 4), (30., 0)] {
        let cache = DatasetCache::new(clock.clone(), ttl, entries).unwrap();
        for _ in 0..3 {
            assert_eq!(
                open(&cache, key("a", None), "s1", &count).cache_status,
                CacheStatus::Miss
            );
        }
    }
}
#[test]
fn location_profile_and_pins_are_distinct_and_locks_are_reclaimed() {
    let clock = Arc::new(FakeClock::default());
    let cache = DatasetCache::new(clock, 30., 32).unwrap();
    let count = AtomicUsize::new(0);
    let k = key("a", None);
    open(&cache, k.clone(), "s1", &count);
    let mut moved = k.clone();
    if let dynapi_catalog::RepositoryLocation::S3(s) = &mut moved.location {
        s.prefix.push_str("v2/");
    }
    assert_eq!(
        open(&cache, moved, "s1", &count).cache_status,
        CacheStatus::Miss
    );
    let mut routing = k;
    routing.profile = Profile::Routing;
    assert_eq!(
        open(&cache, routing, "s1", &count).cache_status,
        CacheStatus::Miss
    );
    assert_eq!(
        open(&cache, key("a", Some("s1")), "s1", &count).cache_status,
        CacheStatus::Miss
    );
    assert_eq!(cache.lock_entries(), 0);
}
#[test]
fn concurrent_misses_and_expired_revalidations_coalesce() {
    let clock = Arc::new(FakeClock::default());
    let cache = DatasetCache::new(clock.clone(), 30., 32).unwrap();
    let count = AtomicUsize::new(0);
    for instant in [0., 31.] {
        clock.set(instant);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        cache
                            .open(
                                key("a", None),
                                || {
                                    count.fetch_add(1, Ordering::SeqCst);
                                    Ok::<_, ()>(Resolved {
                                        snapshot_id: "s1".into(),
                                        session: (),
                                    })
                                },
                                |_| Ok(7),
                            )
                            .unwrap()
                    })
                })
                .collect();
            let results: Vec<_> = jobs.into_iter().map(|j| j.join().unwrap()).collect();
            let leader = if instant == 0. {
                CacheStatus::Miss
            } else {
                CacheStatus::Revalidated
            };
            assert_eq!(
                results.iter().filter(|r| r.cache_status == leader).count(),
                1
            );
            assert_eq!(
                results
                    .iter()
                    .filter(|r| r.cache_status == CacheStatus::Hit)
                    .count(),
                7
            );
            assert!(
                results
                    .iter()
                    .all(|r| Arc::ptr_eq(&r.dataset, &results[0].dataset))
            );
        });
        assert_eq!(cache.lock_entries(), 0);
    }
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[test]
fn unrelated_keys_open_concurrently_and_hits_do_not_wait_on_a_miss() {
    let cache = DatasetCache::with_defaults(Arc::new(FakeClock::default()));
    let count = AtomicUsize::new(0);
    open(&cache, key("cached", None), "s1", &count);
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let jobs: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|id| {
                scope.spawn(|| {
                    cache
                        .open(
                            key(id, None),
                            || {
                                barrier.wait();
                                // An unrelated hit resolves while both misses hold their keyed locks.
                                assert_eq!(
                                    open(&cache, key("cached", None), "s1", &count).cache_status,
                                    CacheStatus::Hit
                                );
                                Ok::<_, ()>(Resolved {
                                    snapshot_id: "s1".into(),
                                    session: (),
                                })
                            },
                            |_| Ok(10),
                        )
                        .unwrap()
                })
            })
            .collect();
        for job in jobs {
            assert_eq!(job.join().unwrap().cache_status, CacheStatus::Miss);
        }
    });
    assert_eq!(cache.lock_entries(), 0);
}

#[test]
fn repository_cache_coalesces_and_does_not_cache_failures() {
    use dynapi_catalog::cache::RepositoryCache;
    let cache = RepositoryCache::default();
    let k = key("a", None).repository_key();
    assert_eq!(
        cache
            .get_or_open(k.clone(), || Err::<usize, _>("unavailable"))
            .unwrap_err(),
        "unavailable"
    );
    let opens = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    cache
                        .get_or_open(k.clone(), || {
                            Ok::<_, ()>(opens.fetch_add(1, Ordering::SeqCst))
                        })
                        .unwrap()
                })
            })
            .collect();
        let results: Vec<_> = jobs.into_iter().map(|j| j.join().unwrap()).collect();
        assert!(results.iter().all(|r| Arc::ptr_eq(r, &results[0])));
    });
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    let mut routing = k;
    routing.profile = Profile::Routing;
    cache
        .get_or_open(routing, || {
            Ok::<_, ()>(opens.fetch_add(1, Ordering::SeqCst))
        })
        .unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), 2);
}

#[test]
fn opener_failure_never_caches_partial_dataset_and_unknown_pin_error_is_exact() {
    let cache = DatasetCache::<_, usize>::with_defaults(Arc::new(FakeClock::default()));
    let error = cache.open(
        key("a", Some("missing")),
        || {
            Ok(Resolved {
                snapshot_id: "missing".into(),
                session: (),
            })
        },
        |_| Err("open failed"),
    );
    assert_eq!(error.unwrap_err(), "open failed");
    assert!(cache.is_empty());
    assert_eq!(cache.lock_entries(), 0);
    assert_eq!(
        dynapi_catalog::cache::UnknownSnapshotError {
            snapshot_id: "missing".into()
        }
        .to_string(),
        "Unknown snapshot 'missing'"
    );
}

#[test]
fn byte_weight_evicts_lru_and_never_retains_oversized_handles() {
    let cache = DatasetCache::new(Arc::new(FakeClock::default()), 30., 10)
        .unwrap()
        .with_byte_limit(8192, |weight: &usize| *weight * 1024);
    let insert = |id, bytes| {
        cache
            .open(
                key(id, Some("pin")),
                || {
                    Ok::<_, ()>(Resolved {
                        snapshot_id: "pin".into(),
                        session: bytes,
                    })
                },
                Ok,
            )
            .unwrap()
    };
    let a = insert("a", 3);
    insert("b", 4);
    let initial_bytes = cache.metadata_bytes();
    assert!((7168..=8192).contains(&initial_bytes));
    assert_eq!(insert("a", 3).cache_status, CacheStatus::Hit);
    insert("c", 2);
    let retained_bytes = cache.metadata_bytes();
    assert!(retained_bytes < initial_bytes);
    assert_eq!(cache.len(), 2);
    insert("huge", 8);
    assert_eq!(cache.metadata_bytes(), retained_bytes);
    assert_eq!(insert("huge", 8).cache_status, CacheStatus::Miss);
    assert_eq!(*a.dataset, 3); // Eviction cannot invalidate an active reader.
}
