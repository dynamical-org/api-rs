use super::*;
#[path = "../tests/support/geo_proxy.rs"]
mod proxy;
use proxy::*;
use std::sync::atomic::{AtomicU64, Ordering};
#[derive(Default)]
struct Clock(AtomicU64);
impl dynapi_policy::Clock for Clock {
    fn now(&self) -> f64 {
        self.0.load(Ordering::SeqCst) as f64
    }
}
fn gate(proxy: &Proxy) -> GeoGate {
    let values = [
        ("DYNAMICAL_API_GEO_CHECK_SECRET", SECRET),
        ("DYNAPI_HARNESS_GEO_CHECK_URL", proxy.url.as_str()),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let config = Config::from_values(&values).unwrap();
    assert!(!format!("{config:?}").contains(SECRET));
    GeoGate::new(&config, Arc::new(Clock::default())).unwrap()
}
fn location(region: usize) -> Location {
    Location {
        country: Some("BB".into()),
        region: Some(region.to_string()),
    }
}
#[tokio::test]
async fn identical_checks_coalesce_and_cancelled_callers_leave_bounded_deadline_tasks() {
    let proxy = Proxy::start().await;
    proxy.state.hold.store(true, Ordering::SeqCst);
    let gate = gate(&proxy);
    let mut callers = Vec::new();
    for _ in 0..20 {
        let gate = gate.clone();
        callers.push(tokio::spawn(async move { gate.answer(location(0)).await }));
    }
    proxy.wait_for(1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(proxy.count(), 1);
    for i in 1..MAX_IN_FLIGHT {
        let gate = gate.clone();
        callers.push(tokio::spawn(async move { gate.answer(location(i)).await }));
    }
    proxy.wait_for(MAX_IN_FLIGHT).await;
    assert!(gate.answer(location(MAX_IN_FLIGHT)).await.is_err());
    assert_eq!(proxy.count(), MAX_IN_FLIGHT);
    for caller in callers {
        caller.abort();
    }
    assert_eq!(gate.0.state.lock().unwrap().flights.len(), MAX_IN_FLIGHT);
    tokio::time::timeout(Duration::from_secs(6), async {
        while !gate.0.state.lock().unwrap().flights.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(gate.0.state.lock().unwrap().answers.is_empty());
    proxy.state.hold.store(false, Ordering::SeqCst);
    proxy.state.release.add_permits(100);
    assert!(!gate.answer(location(MAX_IN_FLIGHT)).await.unwrap().allowed);
    assert_eq!(proxy.count(), MAX_IN_FLIGHT + 1);
}
#[tokio::test]
async fn cached_entries_and_owned_bytes_have_separate_hard_bounds() {
    let proxy = Proxy::start().await;
    let gate = gate(&proxy);
    for i in 0..CACHE_ENTRIES {
        assert!(!gate.answer(location(i)).await.unwrap().allowed);
    }
    assert_eq!(gate.0.state.lock().unwrap().answers.len(), CACHE_ENTRIES);
    gate.answer(location(CACHE_ENTRIES)).await.unwrap();
    assert_eq!(gate.0.state.lock().unwrap().answers.len(), 1);
    // A full-sized refusal reaches the byte bound before the entry bound.
    *proxy.state.body.lock().unwrap() = "x".repeat(MAX_DETAIL_BYTES);
    gate.0.state.lock().unwrap().answers.clear();
    let mut cleared = false;
    for i in 0..300 {
        gate.answer(location(i)).await.unwrap();
        let state = gate.0.state.lock().unwrap();
        let bytes: usize = state
            .answers
            .iter()
            .map(|(k, a)| {
                k.bytes() + a.detail.capacity() + std::mem::size_of::<(Location, Answer)>() + 128
            })
            .sum();
        assert!(bytes <= CACHE_BYTES);
        if state.answers.len() == 1 && i > 0 {
            cleared = true;
            break;
        }
    }
    assert!(cleared);
}
#[tokio::test]
async fn transport_errors_and_hard_timeout_are_retryable_and_uncached() {
    let proxy = Proxy::start().await;
    let gate = gate(&proxy);
    proxy.state.hold.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    assert!(gate.answer(location(0)).await.is_err());
    assert!(started.elapsed() >= Duration::from_secs(4));
    assert!(started.elapsed() < Duration::from_secs(6));
    assert!(gate.0.state.lock().unwrap().answers.is_empty());
    proxy.state.hold.store(false, Ordering::SeqCst);
    proxy.state.release.add_permits(10);
    assert!(gate.answer(location(0)).await.is_ok());
    assert_eq!(proxy.count(), 2);
    let mut inner = gate.0;
    Arc::get_mut(&mut inner).unwrap().url = "http://127.0.0.1:1/geo-check".into();
    let gate = GeoGate(inner);
    assert!(gate.answer(location(1)).await.is_err());
    assert!(gate.answer(location(1)).await.is_err());
}
#[test]
fn metadata_selects_any_http_container_on_exact_proxy_host() {
    for (kind, url, expected) in [
        ("http", "https://wn.dynamical.org/a/", true),
        ("http", "http://WN.DYNAMICAL.ORG:80/", true),
        ("s3", "https://wn.dynamical.org/", false),
        ("http", "https://wn.dynamical.org.example.test/", false),
        ("http", "https://example.test/wn.dynamical.org", false),
    ] {
        let mut raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../dynapi-dataset/tests/fixtures/forecast/input.json"
        ))
        .unwrap();
        raw["collection"]["id"] = serde_json::json!("arbitrary-fixture-name");
        let mut product = Collection::from_stac(&raw["collection"]).unwrap();
        product.repository_config["virtual_chunk_containers"] = serde_json::json!([
            {"url_prefix":"https://direct.dynamical.org/","type":"http"},
            {"url_prefix":url,"type":kind}
        ]);
        assert_eq!(required(&product), expected, "{url}");
    }
}
