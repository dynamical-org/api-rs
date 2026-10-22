mod support;
use dynapi_catalog::refresh::{CatalogConfig, CatalogState};
use dynapi_catalog::{CatalogError, Collection};
use std::sync::Arc;
use support::FakeClock;
fn collection(version: &str) -> Collection {
    Collection::from_stac(&serde_json::json!({"id":"example","version":version,"assets":{"icechunk":{"href":"https://data.dynamical.org/example/v1/"}}})).unwrap()
}
#[test]
fn cold_flight_negative_cache_backoff_and_recovery() {
    let clock = Arc::new(FakeClock::default());
    let mut state = CatalogState::new(clock.clone(), CatalogConfig::default()).unwrap();
    let first = state.decide().unwrap();
    assert!(first.start);
    assert!(first.snapshot.is_none());
    let flight = first.flight.unwrap();
    assert_eq!(flight.deadline, 30.);
    assert!(!state.decide().unwrap().start);
    state.fail(
        flight,
        &CatalogError::typed("https://fake:fake@example.test/private", "ConnectError"),
        |_, b| b,
    );
    assert_eq!(
        state.status(0).last_error.as_deref(),
        Some("STAC catalog refresh failed (ConnectError)")
    );
    assert_eq!(state.retry_at(), 5.);
    clock.set(4.99);
    assert!(state.decide().unwrap().flight.is_none());
    clock.set(5.);
    let second = state.decide().unwrap().flight.unwrap();
    state.fail(second, &CatalogError::new("bad"), |_, b| b);
    assert_eq!(state.retry_at(), 65.);
    clock.set(65.);
    let third = state.decide().unwrap().flight.unwrap();
    state
        .publish(
            third,
            vec![collection("1")],
            "2026-10-21T00:00:00+00:00".into(),
        )
        .unwrap();
    assert_eq!(state.status(0).consecutive_failures, 0);
    assert!(state.status(0).last_error.is_none());
    assert!(!state.decide().unwrap().start);
}
#[test]
fn stale_snapshot_is_preserved_backoff_caps_and_returned_values_are_isolated() {
    let clock = Arc::new(FakeClock::default());
    let mut state = CatalogState::new(clock.clone(), CatalogConfig::default()).unwrap();
    let flight = state.decide().unwrap().flight.unwrap();
    state
        .publish(flight, vec![collection("1")], "utc".into())
        .unwrap();
    clock.set(300.);
    for (failures, delay) in [
        (1, 30.),
        (2, 60.),
        (3, 120.),
        (4, 240.),
        (5, 300.),
        (6, 300.),
    ] {
        let decision = state.decide().unwrap();
        assert!(decision.start);
        assert_eq!(
            decision.snapshot.unwrap().collections["example"].version,
            "1"
        );
        state.fail(
            decision.flight.unwrap(),
            &CatalogError::new("bad"),
            |_, b| b,
        );
        assert_eq!(state.status(12).consecutive_failures, failures);
        assert_eq!(state.status(12).outstanding_workers, 12);
        assert_eq!(state.retry_at(), clock.now() + delay);
        clock.set(state.retry_at() - 0.01);
        assert!(state.decide().unwrap().flight.is_none());
        clock.set(state.retry_at());
    }
    let mut returned = state.snapshot().unwrap().collections["example"].clone();
    returned.version = "mutated".into();
    assert_eq!(
        state.snapshot().unwrap().collections["example"].version,
        "1"
    );
}
use dynapi_catalog::Clock;
#[test]
fn deadline_close_and_retired_generation_cannot_publish() {
    let clock = Arc::new(FakeClock::default());
    let mut state = CatalogState::new(clock.clone(), CatalogConfig::default()).unwrap();
    let old = state.decide().unwrap().flight.unwrap();
    clock.set(30.);
    assert_eq!(
        state
            .publish(old, vec![collection("old")], "utc".into())
            .unwrap_err()
            .to_string(),
        "STAC catalog load deadline exceeded or flight cancelled"
    );
    state.fail(old, &CatalogError::typed("wait", "TimeoutError"), |_, b| b);
    clock.set(35.);
    let new = state.decide().unwrap().flight.unwrap();
    assert!(
        state
            .publish(old, vec![collection("late")], "utc".into())
            .is_err()
    );
    state
        .publish(new, vec![collection("new")], "utc".into())
        .unwrap();
    state.close();
    state.close();
    assert_eq!(
        state.decide().unwrap_err().to_string(),
        "STAC catalog is closed"
    );
    assert!(!state.status(0).refreshing);
}
