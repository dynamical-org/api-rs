use dynapi_policy::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now(&self) -> f64 {
        self.0.load(Ordering::SeqCst) as f64 / 1000.0
    }
}
impl FakeClock {
    fn set(&self, millis: u64) {
        self.0.store(millis, Ordering::SeqCst);
    }
}
#[test]
fn token_refill_retry_and_clock_regression_match_python() {
    let clock = Arc::new(FakeClock::default());
    let mut limiter = RateLimiter::new(
        RateProfile {
            requests_per_minute: 12,
            burst: 2,
            max_clients: 3,
        },
        clock.clone(),
    )
    .unwrap();
    assert_eq!(limiter.check("a"), None);
    assert_eq!(limiter.check("a"), None);
    assert_eq!(limiter.check("a"), Some(5));
    clock.set(1001);
    assert_eq!(limiter.check("a"), Some(4));
    clock.set(0);
    assert_eq!(limiter.check("a"), Some(4));
    clock.set(4000);
    assert_eq!(limiter.check("a"), None);
    clock.set(100000);
    assert_eq!(limiter.check("a"), None);
    assert_eq!(limiter.check("a"), None);
    assert_eq!(limiter.check("a"), Some(5));
}
#[test]
fn rejected_clients_refresh_lru_and_eviction_is_bounded() {
    let mut limiter = RateLimiter::new(
        RateProfile {
            requests_per_minute: 60,
            burst: 1,
            max_clients: 2,
        },
        FakeClock::default(),
    )
    .unwrap();
    assert_eq!(limiter.check("a"), None);
    assert_eq!(limiter.check("b"), None);
    assert_eq!(limiter.check("a"), Some(1));
    assert_eq!(limiter.check("c"), None);
    assert_eq!(limiter.clients(), 2);
    assert_eq!(limiter.check("a"), Some(1));
    assert_eq!(limiter.check("b"), None);
}
#[test]
fn permit_follows_work_across_caller_cancellation_and_unwind() {
    let clock = Arc::new(FakeClock::default());
    let gate = Arc::new(ComputeGate::new(1, 5, clock.clone()).unwrap());
    let permit = gate.try_acquire().unwrap();
    clock.set(1500);
    assert_eq!(gate.snapshot().oldest_age_seconds, Some(1.5));
    assert!(gate.try_acquire().is_none());
    assert_eq!(gate.retry_after(), 5);
    let (send, receive) = std::sync::mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        let _permit = permit;
        let _ = receive.recv();
    });
    // Dropping the caller has no ownership of the worker's permit.
    assert!(gate.try_acquire().is_none());
    drop(send);
    worker.join().unwrap();
    assert_eq!(gate.snapshot().in_use, 0);
    let _ = std::panic::catch_unwind(|| {
        let _permit = gate.try_acquire().unwrap();
        panic!("worker fails");
    });
    assert_eq!(gate.snapshot().in_use, 0);
}
#[test]
fn trust_cf_metadata_only_after_auth_and_normalize_the_entire_worker() {
    assert_eq!(
        client_key("peer", false, Some("ip"), Some("example.org")),
        "peer"
    );
    assert_eq!(
        client_key("peer", true, Some("ip"), Some(" \u{1c}example.org\u{85}")),
        "ip|example.org"
    );
    assert_eq!(client_key("peer", true, Some(""), Some("a/b")), "");
    assert_eq!(cf_worker(Some(&format!("{}!", "a".repeat(100)))), None);
    assert_eq!(cf_worker(Some(&"a".repeat(101))).unwrap().len(), 100);
    assert_eq!(cf_worker(Some("é")), None);
}
#[test]
fn cors_preflight_is_distinct_from_simple_and_500() {
    assert!(preflight("OPTIONS", false, Some("POST"), None, false).is_none());
    assert!(preflight("POST", true, Some("POST"), None, false).is_none());
    let ok = preflight(
        "OPTIONS",
        true,
        Some("POST"),
        Some("Content-Type, ACCEPT"),
        false,
    )
    .unwrap();
    assert_eq!(ok.status, 200);
    assert_eq!(ok.body, "OK");
    assert_eq!(ok.headers.last().unwrap().1, ALLOW_HEADERS);
    let bad = preflight("OPTIONS", true, Some("put"), Some("Authorization"), false).unwrap();
    assert_eq!(bad.status, 400);
    assert_eq!(bad.body, "Disallowed CORS method, headers");
    assert_eq!(simple_cors(true, false)[0].1, "*");
    assert_eq!(simple_cors(true, true)[1].1, "X-Request-Id");
    assert!(simple_cors(false, false).is_empty());
}
#[test]
fn cache_and_timing_policy_preserve_order_and_accumulate() {
    assert_eq!(
        cache_control("HEAD", "/robots.txt", 200),
        Some("public, max-age=300")
    );
    assert_eq!(cache_control("GET", "/v1/forecasts/runs", 500), None);
    assert_eq!(cache_control("HEAD", "/v1/forecasts/runs", 200), None);
    let mut t = ServerTimings::default();
    t.add("catalog", 0.125);
    t.add("open", 2.5);
    t.add("catalog", 1.0);
    t.describe("dataset_cache", "miss");
    t.describe("dataset_cache", "hit");
    t.describe("dataset_cache", "hit");
    t.count("open", 1);
    t.count("open", 2);
    assert_eq!(t.counters["open"], 3);
    assert_eq!(
        t.header(),
        "catalog;dur=1.125, open;dur=2.500, dataset_cache;desc=\"hit|miss\""
    );
}
#[test]
fn invalid_bounds_are_rejected() {
    assert!(
        RateLimiter::new(
            RateProfile {
                requests_per_minute: 0,
                burst: 1,
                max_clients: 1
            },
            FakeClock::default()
        )
        .is_err()
    );
    assert!(ComputeGate::new(0, 1, FakeClock::default()).is_err());
    assert!(ComputeGate::new(1, 0, FakeClock::default()).is_err());
}
