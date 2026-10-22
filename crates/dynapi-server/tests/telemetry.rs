//! Fixture inputs and invariants own expectations; no SDK or external service.
use dynapi_server::{
    App, Monotonic,
    config::Config,
    telemetry::{RecordingSink, Stream},
    transport,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

struct Server {
    address: std::net::SocketAddr,
    app: Arc<App>,
    sink: Arc<RecordingSink>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(values: &[(&str, &str)]) -> Self {
        let values = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let sink = Arc::new(RecordingSink::default());
        let app = Arc::new(
            App::new(
                Config::from_values(&values).unwrap(),
                Arc::new(Monotonic::default()),
                sink.clone(),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serve = app.clone();
        let task = tokio::spawn(async move {
            transport::run(listener, serve).await.unwrap();
        });
        Self {
            address,
            app,
            sink,
            task,
        }
    }
    async fn request(&self, method: &str, path: &str, headers: &[u8], body: &[u8]) -> u16 {
        let mut socket = TcpStream::connect(self.address).await.unwrap();
        socket.write_all(format!("{method} {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\nContent-Length: {}\r\n",body.len()).as_bytes()).await.unwrap();
        socket.write_all(headers).await.unwrap();
        socket.write_all(b"\r\n").await.unwrap();
        socket.write_all(body).await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            socket.read_to_end(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        std::str::from_utf8(&bytes)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }
}

#[tokio::test]
async fn telemetry_condition_fixture_preserves_nulls_routes_and_event_counts() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/telemetry.json")).unwrap();
    for case in cases {
        let mut values = vec![
            ("POSTHOG_PROJECT_TOKEN", "fake-project-token"),
            ("SENTRY_DSN", "https://public@errors.example.invalid/1"),
            ("DYNAMICAL_API_TIMING_LOG_SAMPLE_RATE", "0"),
        ];
        if let Some(secret) = case["secret"].as_str() {
            values.push(("DYNAMICAL_API_ORIGIN_SECRET", secret));
        }
        let server = Server::start(&values).await;
        let method = case["method"].as_str().unwrap();
        let status = server
            .request(
                method,
                case["path"].as_str().unwrap(),
                case["headers"].as_str().unwrap().as_bytes(),
                case["body"].as_str().unwrap().as_bytes(),
            )
            .await;
        assert_eq!(json!(status), case["status"], "{}", case["name"]);
        let records = server.sink.0.lock().unwrap();
        let by_stream = |stream| {
            records
                .iter()
                .filter(|(s, _)| *s == stream)
                .map(|(_, v)| v)
                .collect::<Vec<_>>()
        };
        let terminal = by_stream(Stream::Terminal);
        let analytics = by_stream(Stream::Analytics);
        let errors = by_stream(Stream::Error);
        assert_eq!(
            terminal.len(),
            usize::from(!case["terminal_route"].is_null()),
            "{}",
            case["name"]
        );
        assert_eq!(
            analytics.len(),
            usize::from(!case["analytics_route"].is_null()),
            "{}",
            case["name"]
        );
        assert_eq!(
            errors.len(),
            usize::from(!case["error"].is_null()),
            "{}",
            case["name"]
        );
        if let Some(record) = terminal.first() {
            assert_eq!(record["route"], case["terminal_route"]);
            assert_eq!(record["status"], case["status"]);
            assert_eq!(record["cancelled"], false);
            assert_eq!(record["caller"], Value::Null);
        }
        if let Some(record) = analytics.first() {
            let mut record = (*record).clone();
            let duration = record["properties"]
                .as_object_mut()
                .unwrap()
                .remove("duration_ms")
                .unwrap()
                .as_f64()
                .unwrap();
            assert!(duration >= 0.0);
            assert!((duration * 10.0 - (duration * 10.0).round()).abs() < 1e-6);
            assert_eq!(
                record,
                json!({"event":"api_request","distinct_id":"anonymous", "properties":{"$process_person_profile":false,"route":case["analytics_route"],"method":method,"status":status,"client_kind":"unlabeled","user_agent":"","country":null,"data_product_ids":[],"query_count":null,"work_units":null,"dataset_cache":null}}),
                "{}",
                case["name"]
            );
        }
        if let Some(record) = errors.first() {
            assert_eq!(record["exception"]["type"], case["error"]);
            assert_eq!(
                record["tags"],
                json!({"client_kind":"unlabeled","caller":null})
            );
        }
    }
}

#[tokio::test]
async fn authenticated_identity_and_metadata_are_shared_and_payloads_stay_private() {
    let server = Server::start(&[
        ("DYNAMICAL_API_ORIGIN_SECRET", "Jefe"),
        ("POSTHOG_PROJECT_TOKEN", "fake-project-token"),
        ("SENTRY_DSN", "https://public@errors.example.invalid/1"),
    ])
    .await;
    let ua = "A".repeat(140);
    let headers = format!(
        "X-Dynamical-Origin-Auth: Jefe\r\nX-Dynamical-Viewer-Auth: fake-viewer-secret\r\nCF-Connecting-IP: what do ya want for nothing?\r\nCF-Worker: example.test\r\nCF-IPCountry: ZZ\r\nUser-Agent: {ua}\r\nContent-Type: application/json\r\nAuthorization: Bearer fake-auth-token\r\nCookie: fake-cookie\r\n"
    );
    let body =
        br#"{"latitude":12.3456789,"longitude":23.4567891,"private":"fake-private-body","x":NaN}"#;
    assert_eq!(
        server
            .request("POST", "/v1/forecasts", headers.as_bytes(), body)
            .await,
        500
    );
    let records = server.sink.0.lock().unwrap();
    assert_eq!(records.len(), 3);
    for (stream, record) in records.iter() {
        let caller = match stream {
            Stream::Terminal => &record["caller"],
            Stream::Analytics => &record["distinct_id"],
            Stream::Error => &record["tags"]["caller"],
        };
        assert_eq!(caller, "5bdcc146bf60754e");
        let encoded = record.to_string();
        for forbidden in [
            "what do ya want",
            "Jefe",
            "fake-auth-token",
            "fake-viewer-secret",
            "fake-cookie",
            "fake-private-body",
            "12.3456789",
            "23.4567891",
            "cf-connecting-ip",
            "authorization",
            "x-dynamical-origin-auth",
            "x-dynamical-viewer-auth",
        ] {
            assert!(!encoded.contains(forbidden), "{forbidden}: {encoded}");
        }
        if *stream == Stream::Analytics {
            assert_eq!(record["properties"]["user_agent"], "A".repeat(120));
            assert_eq!(record["properties"]["country"], "ZZ");
        }
    }
}

#[tokio::test]
async fn authentication_encoding_failure_captures_an_untagged_error_only() {
    let server = Server::start(&[
        ("DYNAMICAL_API_ORIGIN_SECRET", "fake-key"),
        ("POSTHOG_PROJECT_TOKEN", "fake-project-token"),
        ("SENTRY_DSN", "https://public@errors.example.invalid/1"),
    ])
    .await;
    assert_eq!(
        server
            .request(
                "POST",
                "/v1/forecasts",
                b"X-Dynamical-Origin-Auth: \xff\r\nCF-Connecting-IP: 192.0.2.1\r\n",
                b""
            )
            .await,
        500
    );
    let records = server.sink.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, Stream::Error);
    assert_eq!(
        records[0].1["exception"]["type"],
        "OriginAuthenticationError"
    );
    assert_eq!(records[0].1["tags"], json!({}));
    assert_eq!(
        server
            .app
            .heartbeat
            .record("local", server.app.gate.snapshot())["inflightAdmitted"],
        0
    );
}

#[tokio::test]
async fn rate_and_busy_rejections_are_not_reported_as_errors() {
    let server = Server::start(&[
        ("SENTRY_DSN", "https://public@errors.example.invalid/1"),
        ("POSTHOG_PROJECT_TOKEN", "fake-project-token"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "2"),
        ("DYNAMICAL_API_COMPUTE_SLOTS", "1"),
    ])
    .await;
    let _held = server.app.gate.try_acquire().unwrap();
    let valid = br#"{"queries":[{"dataProductId":"example-forecast","location":{"latitude":0,"longitude":0},"variables":["temperature_2m"]}]}"#;
    for expected in [503, 503, 429] {
        assert_eq!(
            server
                .request(
                    "POST",
                    "/v1/forecasts",
                    b"Content-Type: application/json\r\n",
                    valid
                )
                .await,
            expected
        );
    }
    let records = server.sink.0.lock().unwrap();
    assert_eq!(records.len(), 6);
    assert!(records.iter().all(|(stream, _)| *stream != Stream::Error));
    let analytics = records
        .iter()
        .filter(|(stream, _)| *stream == Stream::Analytics)
        .map(|(_, r)| r)
        .collect::<Vec<_>>();
    assert_eq!(analytics[2]["properties"]["route"], "unmatched");
}

#[cfg(feature = "harness")]
#[tokio::test]
async fn panic_captures_once_with_request_scoped_tags() {
    let server = Server::start(&[
        ("SENTRY_DSN", "https://public@errors.example.invalid/1"),
        ("POSTHOG_PROJECT_TOKEN", "fake-project-token"),
    ])
    .await;
    for (ua, worker, kind, expected_worker) in [
        (
            "DYNAMICAL-test",
            "example.test",
            "internal",
            json!("example.test"),
        ),
        (
            "SENTRY UPTIME",
            "invalid/worker",
            "uptime_probe",
            Value::Null,
        ),
    ] {
        server.sink.0.lock().unwrap().clear();
        let headers = format!(
            "x-dynapi-harness-panic: 1\r\nUser-Agent: {ua}\r\nCF-Worker: {worker}\r\nCF-Connecting-IP: 192.0.2.1\r\n"
        );
        assert_eq!(
            server
                .request("GET", "/v1/example-missing", headers.as_bytes(), b"")
                .await,
            500
        );
        let records = server.sink.0.lock().unwrap();
        assert_eq!(records.len(), 3);
        let errors = records
            .iter()
            .filter(|(s, _)| *s == Stream::Error)
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].1["exception"]["type"], "HandlerPanic");
        assert_eq!(errors[0].1["tags"]["client_kind"], kind);
        assert_eq!(errors[0].1["tags"]["caller"], Value::Null);
        assert_eq!(errors[0].1["tags"]["cf_worker"], expected_worker);
    }
}

#[test]
fn empty_sink_credentials_disable_capture_flags() {
    for values in [
        BTreeMap::new(),
        BTreeMap::from([
            ("POSTHOG_PROJECT_TOKEN".into(), "".into()),
            ("SENTRY_DSN".into(), "".into()),
        ]),
    ] {
        let config = Config::from_values(&values).unwrap();
        assert!(!config.analytics_enabled);
        assert!(!config.error_reporting_enabled);
    }
}

#[tokio::test]
async fn cancelled_request_releases_admission_and_emits_no_analytics_or_error() {
    let sink = Arc::new(RecordingSink::default());
    let config = Config::from_values(&BTreeMap::from([
        ("POSTHOG_PROJECT_TOKEN".into(), "fake-project-token".into()),
        (
            "SENTRY_DSN".into(),
            "https://public@errors.example.invalid/1".into(),
        ),
    ]))
    .unwrap();
    let app = Arc::new(App::new(config, Arc::new(Monotonic::default()), sink.clone()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let task = tokio::spawn(transport::serve_connection(
        socket,
        app.clone(),
        "192.0.2.1".into(),
    ));
    client.write_all(b"POST /v1/forecasts HTTP/1.1\r\nHost: example.test\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while app.heartbeat.record("local", app.gate.snapshot())["inflightAdmitted"] != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert_eq!(
        app.heartbeat.record("local", app.gate.snapshot())["inflightAdmitted"],
        0
    );
    let records = sink.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, Stream::Terminal);
    assert_eq!(records[0].1["cancelled"], true);
    assert_eq!(records[0].1["status"], Value::Null);
}

#[cfg(feature = "telemetry-delivery")]
#[tokio::test]
async fn startup_failure_is_reported_and_drained_before_process_exit() {
    use http_body_util::BodyExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (events, mut received) = tokio::sync::mpsc::channel(1);
    let collector = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let service =
            hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                let events = events.clone();
                async move {
                    let path = request.uri().path().to_owned();
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    events.send((path, body)).await.unwrap();
                    Ok::<_, std::convert::Infallible>(hyper::Response::new(
                        http_body_util::Full::new(bytes::Bytes::from_static(b"{}")),
                    ))
                }
            });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
            .await;
    });
    let process = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_dynapi-server"))
            .env_clear()
            .env("SENTRY_DSN", format!("http://public@{address}/1"))
            .env("DYNAMICAL_API_ORIGIN_SECRET", "")
            .env("DYNAMICAL_API_HEARTBEAT_SECONDS", "0")
            .output()
            .unwrap()
    });
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), process)
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("DYNAMICAL_API_ORIGIN_SECRET must not be empty")
    );
    let (path, body) = received.recv().await.unwrap();
    assert_eq!(path, "/api/1/envelope/");
    let lines = std::str::from_utf8(&body)
        .unwrap()
        .lines()
        .collect::<Vec<_>>();
    let event: Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(event["exception"]["values"][0]["type"], "StartupError");
    assert!(event.get("request").is_none());
    assert!(event.get("user").is_none());
    collector.abort();
    let _ = collector.await;
}
