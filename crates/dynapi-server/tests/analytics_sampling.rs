#![cfg(feature = "harness")]
mod support;
use dynapi_server::{
    App,
    config::Config,
    telemetry::{
        RecordingSink, Stream, Telemetry,
        sampling::{AnalyticsSampler, Sampling, Settings},
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use support::*;
const AUTH: &str = "fixture-origin";
fn headers() -> [(&'static str, &'static str); 2] {
    [
        ("X-Dynamical-Origin-Auth", AUTH),
        ("CF-Connecting-IP", "203.0.113.9"),
    ]
}
fn values() -> [(&'static str, &'static str); 2] {
    [
        ("POSTHOG_PROJECT_TOKEN", "fixture-token"),
        ("DYNAMICAL_API_ORIGIN_SECRET", AUTH),
    ]
}
fn analytics(server: &Server) -> Vec<Value> {
    server
        .sink
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(s, _)| *s == Stream::Analytics)
        .map(|(_, v)| v.clone())
        .collect()
}
fn fixture(
    threshold: u64,
    initial: f64,
) -> (Arc<AnalyticsSampler>, Arc<TestClock>, Arc<Mutex<f64>>) {
    let clock = Arc::new(TestClock::default());
    let draw = Arc::new(Mutex::new(initial));
    let random = draw.clone();
    let sampler = Arc::new(
        AnalyticsSampler::new(
            Settings {
                enabled: true,
                threshold,
                window_seconds: 60,
                weight: 10,
            },
            100,
            clock.clone(),
            Arc::new(move || *random.lock().unwrap()),
        )
        .unwrap(),
    );
    (sampler, clock, draw)
}
#[test]
fn analytics_sampling_settings_are_positive_and_enable_only_exact_one() {
    let config = |v: &[(&str, &str)]| {
        Config::from_values(
            &v.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        )
    };
    assert_eq!(
        config(&[]).unwrap().analytics_sampling,
        Settings {
            enabled: false,
            threshold: 100,
            window_seconds: 3600,
            weight: 10
        }
    );
    assert!(
        config(&[("DYNAMICAL_API_ANALYTICS_SAMPLING", "1")])
            .unwrap()
            .analytics_sampling
            .enabled
    );
    for disabled in ["0", "", "true", " 1"] {
        assert!(
            !config(&[("DYNAMICAL_API_ANALYTICS_SAMPLING", disabled)])
                .unwrap()
                .analytics_sampling
                .enabled
        );
    }
    for key in [
        "DYNAMICAL_API_ANALYTICS_SAMPLING_THRESHOLD",
        "DYNAMICAL_API_ANALYTICS_SAMPLING_WINDOW_SECONDS",
        "DYNAMICAL_API_ANALYTICS_SAMPLING_WEIGHT",
    ] {
        for invalid in ["0", "-1", "", "bad", "1.5"] {
            assert!(config(&[(key, invalid)]).is_err(), "{key}={invalid}");
        }
    }
    let custom = config(&[
        ("DYNAMICAL_API_ANALYTICS_SAMPLING_THRESHOLD", "3"),
        ("DYNAMICAL_API_ANALYTICS_SAMPLING_WINDOW_SECONDS", "60"),
        ("DYNAMICAL_API_ANALYTICS_SAMPLING_WEIGHT", "7"),
    ])
    .unwrap()
    .analytics_sampling;
    assert_eq!(
        (custom.threshold, custom.window_seconds, custom.weight),
        (3, 60, 7)
    );
}
#[tokio::test]
async fn weighted_successes_errors_window_reset_and_worker_identity_follow_http_contract() {
    let (sampler, clock, draw) = fixture(3, 0.05);
    let server = Server::start_with_sampler(&values(), sampler).await;
    for worker in ["first.example", "second.example", "third.example"] {
        let mut wire = headers().to_vec();
        wire.push(("CF-Worker", worker));
        assert_eq!(
            server
                .request_with_headers("GET", "/v1/data-products", &wire, &Value::Null)
                .await
                .status,
            200
        );
    }
    assert_eq!(
        server
            .request_with_headers("GET", "/v1/data-products", &headers(), &Value::Null)
            .await
            .status,
        200
    );
    *draw.lock().unwrap() = 0.1;
    assert_eq!(
        server
            .request_with_headers("GET", "/v1/data-products", &headers(), &Value::Null)
            .await
            .status,
        200
    );
    assert_eq!(
        server
            .request_with_headers("GET", "/v1/data-products/missing", &headers(), &Value::Null)
            .await
            .status,
        404
    );
    let events = analytics(&server);
    assert_eq!(
        events
            .iter()
            .map(|e| e["properties"]["sample_weight"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 1, 1, 10, 1]
    );
    assert!(
        events
            .iter()
            .all(|event| event["distinct_id"] == events[0]["distinct_id"])
    );
    clock.0.store(60, Ordering::SeqCst);
    assert_eq!(
        server
            .request_with_headers("GET", "/v1/data-products", &headers(), &Value::Null)
            .await
            .status,
        200
    );
    assert_eq!(
        analytics(&server).last().unwrap()["properties"]["sample_weight"],
        1
    );
    let other = [
        ("X-Dynamical-Origin-Auth", AUTH),
        ("CF-Connecting-IP", "203.0.113.10"),
    ];
    assert_eq!(
        server
            .request_with_headers("GET", "/v1/data-products", &other, &Value::Null)
            .await
            .status,
        200
    );
    let events = analytics(&server);
    assert_ne!(
        events.last().unwrap()["distinct_id"],
        events[0]["distinct_id"]
    );
}
#[tokio::test]
async fn default_off_and_anonymous_always_emit_integer_unit_weight() {
    let mut config = values().to_vec();
    config.push(("DYNAMICAL_API_ANALYTICS_SAMPLING_THRESHOLD", "1"));
    let server = Server::start(&config).await;
    for _ in 0..5 {
        assert_eq!(
            server
                .request_with_headers("GET", "/v1/data-products", &headers(), &Value::Null)
                .await
                .status,
            200
        );
    }
    assert_eq!(analytics(&server).len(), 5);
    assert!(
        analytics(&server)
            .iter()
            .all(|e| e["properties"]["sample_weight"] == 1)
    );
    let (sampler, _, _) = fixture(1, 0.99);
    let server = Server::start_with_sampler(&values(), sampler).await;
    for ip in [None, Some("")] {
        let mut wire = vec![("X-Dynamical-Origin-Auth", AUTH)];
        if let Some(ip) = ip {
            wire.push(("CF-Connecting-IP", ip));
        }
        for _ in 0..5 {
            assert_eq!(
                server
                    .request_with_headers("GET", "/v1/data-products", &wire, &Value::Null)
                    .await
                    .status,
                200
            );
        }
    }
    let events = analytics(&server);
    assert_eq!(events.len(), 10);
    assert!(
        events
            .iter()
            .all(|e| e["distinct_id"] == "anonymous" && e["properties"]["sample_weight"] == 1)
    );
}
struct Spy(Mutex<Vec<(String, u16)>>);
impl Sampling for Spy {
    fn sample(&self, identity: &str, status: u16) -> Option<u64> {
        self.0.lock().unwrap().push((identity.into(), status));
        Some(1)
    }
}
#[tokio::test]
async fn only_eligible_analytics_count_and_every_non_success_is_retained() {
    let spy = Arc::new(Spy(Mutex::new(vec![])));
    let mut config = values().to_vec();
    config.push(("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "2"));
    let server = Server::start_with_sampler(&config, spy.clone()).await;
    assert_eq!(
        server.request("GET", "/healthz", &Value::Null).await.status,
        403
    );
    assert_eq!(
        server
            .request_with_headers("GET", "/healthz", &headers(), &Value::Null)
            .await
            .status,
        200
    );
    assert_eq!(
        server
            .request("GET", "/v1/data-products", &Value::Null)
            .await
            .status,
        403
    );
    assert_eq!(
        server
            .request_with_headers(
                "OPTIONS",
                "/v1/forecasts",
                &[
                    ("Origin", "https://example.test"),
                    ("Access-Control-Request-Method", "POST")
                ],
                &Value::Null
            )
            .await
            .status,
        200
    );
    for (method, path, body, status) in [
        ("GET", "/v1/data-products", Value::Null, 200),
        ("GET", "/v1/missing", Value::Null, 404),
        ("GET", "/v1/forecasts", Value::Null, 405),
        ("POST", "/v1/forecasts", json!({}), 422),
        ("GET", "/v1/forecasts", Value::Null, 429),
    ] {
        assert_eq!(
            server
                .request_with_headers(method, path, &headers(), &body)
                .await
                .status,
            status
        );
    }
    let calls = spy.0.lock().unwrap().clone();
    assert_eq!(
        calls.iter().map(|(_, status)| *status).collect::<Vec<_>>(),
        vec![200, 404, 405, 422, 429]
    );
    assert!(
        calls
            .iter()
            .all(|(id, _)| id.len() == 16 && *id == calls[0].0)
    );
    let spy = Arc::new(Spy(Mutex::new(vec![])));
    let server = Server::start_with_sampler(&[], spy.clone()).await;
    assert_eq!(
        server
            .request("GET", "/v1/data-products", &Value::Null)
            .await
            .status,
        200
    );
    assert!(spy.0.lock().unwrap().is_empty());
}
#[tokio::test]
async fn dropping_analytics_preserves_response_and_independent_terminal_success() {
    let (sampler, _, _) = fixture(1, 0.5);
    let server = Server::start_with_sampler(&values(), sampler).await;
    assert_eq!(
        server
            .request_with_headers("GET", "/v1/forecasts", &headers(), &Value::Null)
            .await
            .status,
        405
    );
    let response = server
        .request_with_headers("POST", "/v1/forecasts", &headers(), &payload())
        .await;
    assert_eq!(response.status, 200);
    assert!(response.headers.contains_key("etag"));
    assert!(response.headers.contains_key("x-dynamical-work-units"));
    let events = analytics(&server);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["properties"]["status"], 405);
    let records = server.sink.0.lock().unwrap();
    let success: Vec<_> = records
        .iter()
        .filter(|(s, e)| *s == Stream::Terminal && e["event"] == "api_request")
        .collect();
    assert_eq!(success.len(), 1);
    assert_eq!(success[0].1["route"], "/v1/forecasts");
    assert!(success[0].1.get("sample_weight").is_none());
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}
struct BrokenSampler;
impl Sampling for BrokenSampler {
    fn sample(&self, _: &str, _: u16) -> Option<u64> {
        panic!("synthetic sampler failure")
    }
}
#[tokio::test]
async fn failing_sampler_emits_each_eligible_event_at_unit_weight() {
    let server = Server::start_with_sampler(&values(), Arc::new(BrokenSampler)).await;
    for (path, status) in [
        ("/v1/data-products", 200),
        ("/v1/data-products/missing", 404),
    ] {
        assert_eq!(
            server
                .request_with_headers("GET", path, &headers(), &Value::Null)
                .await
                .status,
            status
        );
    }
    let events = analytics(&server);
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e["properties"]["sample_weight"] == 1));
}
struct BrokenCapture {
    attempts: AtomicUsize,
    terminal: RecordingSink,
}
impl Telemetry for BrokenCapture {
    fn emit(&self, stream: Stream, record: Value) {
        if stream == Stream::Analytics {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            panic!("synthetic capture failure");
        }
        self.terminal.emit(stream, record);
    }
}
#[tokio::test]
async fn failing_capture_never_changes_http_responses_or_terminal_failures() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };
    let host = FixtureHost::start().await;
    let sink = Arc::new(BrokenCapture {
        attempts: AtomicUsize::new(0),
        terminal: RecordingSink::default(),
    });
    let mut config: BTreeMap<String, String> = values()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    config.insert(
        "DYNAMICAL_API_STAC_URL".into(),
        format!("{}/catalog.json", host.url),
    );
    config.insert("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST".into(), "1".into());
    let app = Arc::new(
        App::new(
            Config::from_values(&config).unwrap(),
            Arc::new(TestClock::default()),
            sink.clone(),
        )
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(dynapi_server::transport::run(listener, app));
    for (method, path, body, status) in [
        ("GET", "/v1/data-products", "", 200),
        ("POST", "/v1/forecasts", "{}", 422),
        ("POST", "/v1/forecasts", "{}", 429),
    ] {
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(format!("{method} {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\nContent-Type: application/json\r\nX-Dynamical-Origin-Auth: {AUTH}\r\nCF-Connecting-IP: 203.0.113.9\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        let mut response = vec![];
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            socket.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            std::str::from_utf8(&response)
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<u16>()
                .unwrap(),
            status
        );
    }
    assert_eq!(sink.attempts.load(Ordering::SeqCst), 3);
    let failures: Vec<_> = sink
        .terminal
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| e["event"] == "api_failure")
        .map(|(_, e)| e["status"].clone())
        .collect();
    assert_eq!(failures, vec![json!(422), json!(429)]);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_never_consumes_the_analytics_prefix() {
    let (sampler, _, _) = fixture(1, 0.99);
    let server = Server::start_with_sampler(&values(), sampler).await;
    server.host.reads.hold_chunks.store(true, Ordering::SeqCst);
    let socket = server
        .socket_with_headers("POST", "/v1/forecasts", &headers(), &payload())
        .await;
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        server.host.reads.chunk_started.notified(),
    )
    .await
    .unwrap();
    drop(socket);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if server
                .sink
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|(_, e)| e["cancelled"] == true)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        server
            .request_with_headers("GET", "/v1/data-products", &headers(), &Value::Null)
            .await
            .status,
        200
    );
    let events = analytics(&server);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["properties"]["sample_weight"], 1);
    server.host.reads.hold_chunks.store(false, Ordering::SeqCst);
    server.host.reads.release.add_permits(100);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while server.app.gate.snapshot().in_use != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn disabled_terminal_success_sampling_keeps_weighted_analytics_active() {
    let (sampler, _, _) = fixture(1, 0.05);
    let mut config = values().to_vec();
    config.push(("DYNAMICAL_API_TIMING_LOG_SAMPLE_RATE", "0"));
    let server = Server::start_with_sampler(&config, sampler).await;
    for _ in 0..2 {
        assert_eq!(
            server
                .request_with_headers("POST", "/v1/forecasts", &headers(), &payload())
                .await
                .status,
            200
        );
    }
    assert_eq!(
        analytics(&server)
            .iter()
            .map(|e| e["properties"]["sample_weight"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 10]
    );
    assert!(
        server
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|(s, e)| *s != Stream::Terminal || e["event"] != "api_request")
    );
}
