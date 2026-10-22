mod support;
use dynapi_policy::Clock;
use dynapi_server::{
    App, BODY_LIMIT,
    config::Config,
    telemetry::{RecordingSink, Stream},
    transport,
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
struct Zero;
impl Clock for Zero {
    fn now(&self) -> f64 {
        0.0
    }
}
struct Server {
    _host: support::FixtureHost,
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
        Self::start_on(values, "127.0.0.1:0").await
    }
    async fn start_on(values: &[(&str, &str)], bind: &str) -> Self {
        let host = support::FixtureHost::start().await;
        let stac_url = format!("{}/catalog.json", host.url);
        let mut values = values.to_vec();
        values.push(("DYNAMICAL_API_STAC_URL", &stac_url));
        values.push(("DYNAPI_HARNESS_STORE_BASE_URL", &host.url));
        let config = Config::from_values(
            &values
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
        .unwrap();
        let sink = Arc::new(RecordingSink::default());
        let app = Arc::new(App::new(config, Arc::new(Zero), sink.clone()).unwrap());
        let listener = TcpListener::bind(bind).await.unwrap();
        let address = listener.local_addr().unwrap();
        let serve = app.clone();
        let task = tokio::spawn(async move {
            transport::run(listener, serve).await.unwrap();
        });
        Self {
            _host: host,
            address,
            app,
            sink,
            task,
        }
    }
    async fn request(&self, method: &str, path: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut socket = TcpStream::connect(self.address).await.unwrap();
        let header = format!(
            "{method} {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
            body.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(body).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            socket.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        response
    }
}
fn status(response: &[u8]) -> u16 {
    std::str::from_utf8(response)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}
const VALID: &[u8] = br#"{"queries":[{"dataProductId":"fixture-forecast","location":{"latitude":44,"longitude":0},"variables":["temperature_2m"]}]}"#;

#[tokio::test]
async fn slash_redirect_falls_back_to_accepted_socket_authority() {
    for bind in ["127.0.0.1:0", "127.0.0.2:0", "0.0.0.0:0", "[::1]:0"] {
        let server = Server::start_on(&[], bind).await;
        let local_host = if server.address.ip().is_unspecified() {
            "127.0.0.1".to_owned()
        } else {
            server.address.ip().to_string()
        };
        for host in [
            "example%2Etest",
            "API_SERVICE.example",
            "example.test:",
            "[v1.test]",
            "test!name",
            "[:]",
            "[::1]:",
            "test:abc",
            "",
        ] {
            for path in ["/healthz", "/v1/forecasts", "/v1/data-products/example"] {
                let raw = format!(
                    "GET {path}/ HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                );
                let response = raw_exchange(&server, raw.as_bytes()).await;
                assert_eq!(status(&response), 307, "{bind}: {host}");
                assert!(
                    String::from_utf8_lossy(&response).contains(&format!(
                        "location: http://{local_host}:{}{path}\r\n",
                        server.address.port()
                    )),
                    "{bind}: {host}: {}",
                    String::from_utf8_lossy(&response)
                );
            }
        }
    }
}

#[tokio::test]
async fn slash_redirect_preserves_hosts_matching_starlette_grammar() {
    let server = Server::start(&[]).await;
    for host in [
        "test",
        "test:80",
        "test:8080",
        "TEST.Example-1.",
        "test:00080",
        "test:999999",
        "[::1]:8000",
        "[::ffff:192.0.2.1]",
        "[::]",
    ] {
        let raw = format!(
            "GET /healthz/ HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        );
        let response = raw_exchange(&server, raw.as_bytes()).await;
        assert_eq!(status(&response), 307, "{host}");
        assert!(
            String::from_utf8_lossy(&response)
                .contains(&format!("location: http://{host}/healthz\r\n")),
            "{host}: {}",
            String::from_utf8_lossy(&response)
        );
    }
}

#[tokio::test]
async fn slash_redirect_omits_empty_query_and_preserves_nonempty_query() {
    let server = Server::start(&[]).await;
    for (query, expected) in [("", ""), ("?", ""), ("?x=%2F&x=2", "?x=%2F&x=2")] {
        let response = server
            .request("GET", &format!("/healthz/{query}"), "", b"")
            .await;
        assert_eq!(status(&response), 307);
        assert!(
            String::from_utf8_lossy(&response).contains(&format!(
                "location: http://example.test/healthz{expected}\r\n"
            )),
            "{query}: {}",
            String::from_utf8_lossy(&response)
        );
    }
}

#[test]
fn configuration_defaults_and_empty_secret() {
    let config = Config::from_values(&BTreeMap::new()).unwrap();
    assert_eq!(config.rate.requests_per_minute, 1200);
    assert_eq!(config.rate.burst, 120);
    assert_eq!(config.query_rate.requests_per_minute, 180);
    assert_eq!(config.query_rate.burst, 30);
    assert_eq!(config.compute_slots, 6);
    assert_eq!(config.max_connections, 1024);
    assert_eq!(
        config.header_read_timeout,
        std::time::Duration::from_secs(10)
    );
    assert_eq!(config.body_read_timeout, std::time::Duration::from_secs(30));
    assert_eq!(config.keep_alive_timeout, std::time::Duration::from_secs(5));
    for key in [
        "DYNAMICAL_API_MAX_CONNECTIONS",
        "DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS",
        "DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS",
        "DYNAMICAL_API_KEEP_ALIVE_TIMEOUT_SECONDS",
    ] {
        for value in ["-1", "invalid", "18446744073709551615"] {
            assert!(Config::from_values(&BTreeMap::from([(key.into(), value.into())])).is_err());
        }
    }
    assert_eq!(config.busy_retry_after, 5);
    assert_eq!(config.max_response_values, 100000);
    assert_eq!(config.max_read_values, 450000000);
    assert_eq!(config.timing_sample_rate, 0.1);
    assert_eq!(config.heartbeat_seconds, Some(30.0));
    assert!(
        Config::from_values(&BTreeMap::from([(
            "DYNAMICAL_API_ORIGIN_SECRET".into(),
            "".into()
        )]))
        .is_err()
    );
    for key in [
        "DYNAMICAL_API_COMPUTE_SLOTS",
        "DYNAMICAL_API_MAX_CONNECTIONS",
        "DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS",
        "DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS",
        "DYNAMICAL_API_KEEP_ALIVE_TIMEOUT_SECONDS",
        "DYNAMICAL_API_RATE_LIMIT_BURST",
        "DYNAMICAL_API_QUERY_RATE_LIMIT_REQUESTS_PER_MINUTE",
    ] {
        assert!(Config::from_values(&BTreeMap::from([(key.into(), "0".into())])).is_err());
    }
}
#[tokio::test]
async fn preflight_bypasses_auth_rate_and_telemetry() {
    let server = Server::start(&[("DYNAMICAL_API_ORIGIN_SECRET", "fake-secret")]).await;
    let response = server
        .request(
            "OPTIONS",
            "/missing",
            "Origin: https://example.test\r\nAccess-Control-Request-Method: POST\r\n",
            b"",
        )
        .await;
    assert_eq!(status(&response), 200);
    assert!(server.sink.0.lock().unwrap().is_empty());
    let response = server
        .request(
            "OPTIONS",
            "/v1/forecasts",
            "Origin: https://example.test\r\n",
            b"",
        )
        .await;
    assert_eq!(status(&response), 403);
    let events = server.sink.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, Stream::Terminal);
    assert_eq!(events[0].1["route"], "/v1/forecasts");
    assert_eq!(events[0].1["caller"], serde_json::Value::Null);
}
#[tokio::test]
async fn middleware_url_path_controls_rate_without_changing_routing() {
    for (ordinary, method, routed_status) in [
        ("/v1/forecasts", "POST", 422),
        ("/v1/forecasts/runs", "GET", 501),
        ("/v1/analyses", "POST", 501),
        ("/v1/data/example/snapshot/points/0", "GET", 501),
    ] {
        for suffix in ["%3Ffoo", "%23foo", "%09", "%0D", "%0A"] {
            let server = Server::start(&[
                ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "2"),
                ("DYNAMICAL_API_QUERY_RATE_LIMIT_REQUESTS_PER_MINUTE", "60"),
                ("DYNAMICAL_API_RATE_LIMIT_BURST", "2"),
                ("DYNAMICAL_API_RATE_LIMIT_REQUESTS_PER_MINUTE", "60"),
            ])
            .await;
            // An extra segment keeps data routing unmatched while its URL path
            // still selects the data limiter.
            let altered = if ordinary.starts_with("/v1/data/") {
                format!("{ordinary}{suffix}/extra")
            } else {
                format!("{ordinary}{suffix}")
            };
            let headers = "Content-Type: application/json\r\nOrigin: https://example.test\r\n";
            // Python's static route regex ends with $, which can match before
            // one final LF. Dynamic path parameters retain the decoded LF.
            let admitted = if suffix == "%0A" && !ordinary.starts_with("/v1/data/") {
                routed_status
            } else {
                404
            };
            for expected in [admitted, admitted, 429] {
                let response = server.request(method, &altered, headers, b"{}").await;
                assert_eq!(status(&response), expected, "{altered}");
                let response = String::from_utf8_lossy(&response);
                assert!(response.contains("access-control-allow-origin: *"));
                assert_eq!(response.contains("retry-after: 1\r\n"), expected == 429);
            }
            let response = server.request(method, ordinary, headers, b"{}").await;
            assert_eq!(status(&response), 429, "after {altered}");
        }
    }
}

#[tokio::test]
async fn middleware_url_path_preserves_preflight_and_origin_auth_precedence() {
    for suffix in ["%3Ffoo", "%23foo", "%09", "%0D", "%0A"] {
        let server = Server::start(&[
            ("DYNAMICAL_API_ORIGIN_SECRET", "fake-secret"),
            ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "1"),
            ("DYNAMICAL_API_QUERY_RATE_LIMIT_REQUESTS_PER_MINUTE", "60"),
        ])
        .await;
        let path = format!("/v1/forecasts{suffix}");
        let preflight = "Origin: https://example.test\r\nAccess-Control-Request-Method: POST\r\n";
        assert_eq!(
            status(&server.request("OPTIONS", &path, preflight, b"").await),
            200
        );
        assert!(server.sink.0.lock().unwrap().is_empty());
        let headers = "Content-Type: application/json\r\nOrigin: https://example.test\r\nCF-Connecting-IP: 192.0.2.1\r\n";
        for auth in ["", "X-Dynamical-Origin-Auth: wrong\r\n"] {
            let response = server
                .request("POST", &path, &format!("{headers}{auth}"), b"{}")
                .await;
            assert_eq!(status(&response), 403);
        }
        let authenticated = format!("{headers}X-Dynamical-Origin-Auth: fake-secret\r\n");
        assert_eq!(
            status(&server.request("POST", &path, &authenticated, b"{}").await),
            if suffix == "%0A" { 422 } else { 404 }
        );
        assert_eq!(
            status(
                &server
                    .request("POST", "/v1/forecasts", &authenticated, b"{}")
                    .await
            ),
            429
        );
        // Preflight still bypasses an exhausted bucket and missing auth still wins.
        assert_eq!(
            status(&server.request("OPTIONS", &path, preflight, b"").await),
            200
        );
        assert_eq!(
            status(&server.request("POST", &path, headers, b"{}").await),
            403
        );
    }
}

#[tokio::test]
async fn analytics_url_path_gate_and_last_query_product_match_starlette() {
    let server = Server::start(&[("POSTHOG_PROJECT_TOKEN", "fake-test-token")]).await;
    for (target, expected_status, products) in [
        (
            "/v1/missing?dataProductId=example-first&dataProductId=example-last",
            404,
            vec!["example-last"],
        ),
        (
            "/v1/missing?dataProductId=example-first&dataProductId=",
            404,
            vec![],
        ),
        (
            "/v1/missing?dataProductId=&dataProduct%49d=example+last",
            404,
            vec!["example last"],
        ),
        (
            "/v1/data-products/example-path?dataProductId=example-first&dataProductId=example-last",
            501,
            vec!["example-path"],
        ),
        (
            "/v1/data-products/example-path?dataProductId=example-first&dataProductId=",
            501,
            vec!["example-path"],
        ),
        (
            "/v1/data-products/example-path%0A?dataProductId=example-query",
            501,
            vec!["example-path\n"],
        ),
        (
            "/v%091/missing?dataProductId=example-last",
            404,
            vec!["example-last"],
        ),
    ] {
        server.sink.0.lock().unwrap().clear();
        assert_eq!(
            status(&server.request("GET", target, "", b"").await),
            expected_status
        );
        let events = server.sink.0.lock().unwrap();
        let analytics: Vec<_> = events
            .iter()
            .filter(|(stream, _)| *stream == Stream::Analytics)
            .collect();
        assert_eq!(analytics.len(), 1, "{target}");
        assert_eq!(
            analytics[0].1["properties"]["data_product_ids"],
            serde_json::json!(products),
            "{target}"
        );
    }
    for target in ["/v1%3F/missing", "/v1%23/missing"] {
        server.sink.0.lock().unwrap().clear();
        assert_eq!(status(&server.request("GET", target, "", b"").await), 404);
        assert!(
            server
                .sink
                .0
                .lock()
                .unwrap()
                .iter()
                .all(|(stream, _)| *stream != Stream::Analytics)
        );
    }
}

#[tokio::test]
async fn validation_precedes_compute_but_follows_rate_debit() {
    let server = Server::start(&[
        ("DYNAMICAL_API_COMPUTE_SLOTS", "1"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "2"),
    ])
    .await;
    let _permit = server.app.gate.try_acquire().unwrap();
    let response = server
        .request(
            "POST",
            "/v1/forecasts",
            "Content-Type: application/json\r\n",
            b"{}",
        )
        .await;
    assert_eq!(status(&response), 422);
    let response = server
        .request(
            "POST",
            "/v1/forecasts",
            "Content-Type: application/json\r\n",
            VALID,
        )
        .await;
    assert_eq!(status(&response), 503);
    assert!(String::from_utf8_lossy(&response).contains("retry-after: 5"));
    let response = server
        .request(
            "POST",
            "/v1/forecasts",
            "Content-Type: application/json\r\n",
            b"{}",
        )
        .await;
    assert_eq!(status(&response), 429);
    assert_eq!(server.app.gate.snapshot().in_use, 1);
}
#[cfg(feature = "harness")]
#[tokio::test]
async fn admitted_forecast_returns_data_with_cors_and_request_id() {
    let server = Server::start(&[]).await;
    let response = server
        .request(
            "POST",
            "/v1/forecasts",
            "Content-Type: application/json\r\nOrigin: https://example.test\r\n",
            VALID,
        )
        .await;
    assert_eq!(status(&response), 200);
    let (headers, body) = response_parts(&response);
    assert!(serde_json::from_slice::<serde_json::Value>(body).unwrap()["results"].is_array());
    assert_eq!(headers["content-type"], "application/json");
    let id = uuid::Uuid::parse_str(&headers["x-request-id"]).unwrap();
    assert_eq!(id.get_version_num(), 4);
    assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert_eq!(
        headers["access-control-expose-headers"],
        "ETag, Retry-After, X-Request-Id, Server-Timing, X-Dynamical-Dataset-Cache, X-Dynamical-Work-Units"
    );
}
#[tokio::test]
async fn failure_events_are_unsampled_and_analytics_conditions_match() {
    let server = Server::start(&[
        ("DYNAMICAL_API_TIMING_LOG_SAMPLE_RATE", "0"),
        ("POSTHOG_PROJECT_TOKEN", "fake-test-token"),
    ])
    .await;
    assert_eq!(
        status(&server.request("GET", "/healthz", "", b"").await),
        200
    );
    assert!(server.sink.0.lock().unwrap().is_empty());
    let response = server.request("POST", "/v1/forecasts", "Content-Type: application/json\r\nOrigin: https://example.test\r\nUser-Agent: dynamical-test\r\nCF-Worker: example.org\r\n", b"{\"x\":NaN}").await;
    assert_eq!(status(&response), 500);
    assert!(
        String::from_utf8_lossy(&response).contains("access-control-expose-headers: X-Request-Id")
    );
    let events = server.sink.0.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, Stream::Terminal);
    let event = &events[0].1;
    assert_eq!(event["event"], "api_failure");
    assert_eq!(event["clientKind"], "internal");
    assert_eq!(event["cfWorker"], "example.org");
    assert_eq!(event["queryCount"], serde_json::Value::Null);
    assert_eq!(events[1].0, Stream::Analytics);
    assert_eq!(events[1].1["properties"]["status"], 500);
}
#[tokio::test]
async fn origin_auth_controls_cf_rate_keys_and_first_duplicate_header_wins() {
    let server = Server::start(&[
        ("DYNAMICAL_API_ORIGIN_SECRET", "fake-secret"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "1"),
    ])
    .await;
    let headers = "Content-Type: application/json\r\nX-Dynamical-Origin-Auth: fake-secret\r\nX-Dynamical-Origin-Auth: wrong\r\nCF-Connecting-IP: 192.0.2.1\r\n";
    assert_eq!(
        status(
            &server
                .request("POST", "/v1/forecasts", headers, b"{}")
                .await
        ),
        422
    );
    assert_eq!(
        status(
            &server
                .request("POST", "/v1/forecasts", headers, b"{}")
                .await
        ),
        429
    );
    assert_eq!(
        status(
            &server
                .request(
                    "POST",
                    "/v1/forecasts",
                    &format!("{headers}CF-Worker: example.org\r\n"),
                    b"{}"
                )
                .await
        ),
        422
    );
    let events = server.sink.0.lock().unwrap();
    assert_eq!(events[0].1["caller"].as_str().unwrap().len(), 16);
    assert_eq!(events[0].1["caller"], events[2].1["caller"]);
}
#[tokio::test]
async fn body_cap_is_explicit_and_carries_cors_and_request_id() {
    let server = Server::start(&[]).await;
    // D6 fixes BODY_LIMIT at 16 MiB and declares this exact 413 detail envelope.
    assert_eq!(BODY_LIMIT, 16_777_216);
    let body = vec![b' '; BODY_LIMIT + 1];
    let response = server
        .request(
            "POST",
            "/v1/forecasts",
            "Content-Type: application/json\r\nOrigin: https://example.test\r\n",
            &body,
        )
        .await;
    assert_eq!(status(&response), 413);
    let (headers, body) = response_parts(&response);
    assert_eq!(body, br#"{"detail":"Request body exceeds 16777216 bytes"}"#);
    // The forecast body-cap branch uses detail(), which declares application/json.
    assert_eq!(headers["content-type"], "application/json");
    assert!(headers.contains_key("x-request-id"));
    assert_eq!(headers["access-control-allow-origin"], "*");
}
#[tokio::test]
async fn keep_alive_and_chunk_boundaries_do_not_rewrite_body() {
    let server = Server::start(&[]).await;
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    let requests = b"POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\nPOST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    for fragment in requests.chunks(7) {
        socket.write_all(fragment).await.unwrap();
    }
    let mut response = Vec::new();
    socket.read_to_end(&mut response).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&response)
            .matches("HTTP/1.1 422")
            .count(),
        2
    );
}

#[cfg(feature = "harness")]
#[tokio::test]
async fn handler_panic_returns_the_500_envelope_and_one_terminal_record() {
    let server = Server::start(&[]).await;
    let response = server
        .request(
            "POST",
            "/v1/forecasts",
            "X-Dynapi-Harness-Panic: 1\r\nOrigin: https://example.test\r\n",
            b"",
        )
        .await;
    assert_eq!(status(&response), 500);
    assert!(response.ends_with(b"Internal Server Error"));
    assert!(
        String::from_utf8_lossy(&response).contains("access-control-expose-headers: X-Request-Id")
    );
    assert_eq!(server.sink.0.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn task_cancellation_claims_one_terminal_record_without_analytics() {
    let config = Config::from_values(&BTreeMap::new()).unwrap();
    let sink = Arc::new(RecordingSink::default());
    let app = Arc::new(App::new(config, Arc::new(Zero), sink.clone()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let task = tokio::spawn(transport::serve_connection(socket, app, "127.0.0.1".into()));
    client.write_all(b"POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Length: 100\r\nExpect: 100-continue\r\n\r\n").await.unwrap();
    let mut response = [0; 25];
    // Reading 100 Continue proves the application is awaiting the body.
    client.read_exact(&mut response).await.unwrap();
    assert_eq!(&response, b"HTTP/1.1 100 Continue\r\n\r\n");
    task.abort();
    let _ = task.await;
    let records = sink.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, Stream::Terminal);
    assert_eq!(records[0].1["cancelled"], true);
    assert_eq!(records[0].1["status"], serde_json::Value::Null);
    assert_eq!(records[0].1["route"], "/v1/forecasts");
}

#[tokio::test]
async fn documented_robots_range_difference_is_explicit() {
    let server = Server::start(&[]).await;
    let response = server
        .request("GET", "/robots.txt", "Range: bytes=0-9\r\n", b"")
        .await;
    assert_eq!(status(&response), 200);
    let body = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|end| &response[end + 4..])
        .unwrap();
    assert_eq!(body, include_bytes!("../robots.txt"));
    assert!(!String::from_utf8_lossy(&response).contains("content-range:"));
}

fn response_parts(response: &[u8]) -> (BTreeMap<String, String>, &[u8]) {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    let httparse::Status::Complete(end) = parsed.parse(response).unwrap() else {
        panic!("incomplete response");
    };
    let fields = parsed
        .headers
        .iter()
        .map(|header| {
            (
                header.name.to_ascii_lowercase(),
                std::str::from_utf8(header.value).unwrap().to_owned(),
            )
        })
        .collect();
    (fields, &response[end..])
}

#[tokio::test]
async fn robots_get_head_and_packaged_metadata() {
    use md5::{Digest, Md5};
    let asset = include_bytes!("../robots.txt");
    let metadata = std::fs::metadata(concat!(env!("CARGO_MANIFEST_DIR"), "/robots.txt")).unwrap();
    let modified = metadata.modified().unwrap();
    let seconds = modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let seconds = if seconds.fract() == 0.0 {
        format!("{seconds:.1}")
    } else {
        seconds.to_string()
    };
    let expected_etag = format!(
        "\"{:x}\"",
        Md5::digest(format!("{seconds}-{}", asset.len()))
    );
    let server = Server::start(&[]).await;
    let mut ids = Vec::new();
    for method in ["GET", "HEAD"] {
        let response = server.request(method, "/robots.txt", "", b"").await;
        assert_eq!(status(&response), 200);
        let (headers, body) = response_parts(&response);
        assert_eq!(
            body,
            if method == "GET" {
                asset.as_slice()
            } else {
                b""
            }
        );
        assert_eq!(headers["content-length"], asset.len().to_string());
        assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
        assert_eq!(headers["etag"], expected_etag);
        assert_eq!(headers["last-modified"], httpdate::fmt_http_date(modified));
        assert_eq!(headers["server"], "uvicorn");
        assert!(httpdate::parse_http_date(&headers["date"]).is_ok());
        let request_id = &headers["x-request-id"];
        assert_eq!(request_id.len(), 32);
        assert!(
            request_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        let id = uuid::Uuid::parse_str(request_id).unwrap();
        assert_eq!(id.get_version_num(), 4);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test]
async fn health_and_root_have_explicit_responses() {
    let server = Server::start(&[]).await;
    let response = server.request("GET", "/healthz", "", b"").await;
    assert_eq!(status(&response), 200);
    let (headers, body) = response_parts(&response);
    assert_eq!(body, br#"{"status":"ok"}"#);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["content-length"], "15");
    for method in ["GET", "HEAD"] {
        let response = server.request(method, "/", "", b"").await;
        assert_eq!(status(&response), 301);
        let (headers, body) = response_parts(&response);
        assert!(body.is_empty());
        assert_eq!(headers["location"], "https://dynamical.org/api/");
        assert_eq!(headers["content-length"], "0");
    }
}

#[tokio::test]
async fn hyper_separates_mixed_ending_pipelined_requests() {
    let server = Server::start(&[]).await;
    for ending in ["\n\n", "\r\n\n", "\n\r\n"] {
        let mut socket = TcpStream::connect(server.address).await.unwrap();
        let requests = format!(
            "GET /healthz HTTP/1.1\r\nHost: test{ending}GET /missing HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
        );
        socket.write_all(requests.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            socket.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(status(&response), 200, "ending {ending:?}");
        let response = String::from_utf8(response).unwrap();
        assert_eq!(response.matches("HTTP/1.1 ").count(), 2, "{response}");
        assert!(response.contains("HTTP/1.1 404"), "{response}");
        assert!(response.contains("{\"status\":\"ok\"}"), "{response}");
    }
}

#[tokio::test]
async fn hyper_keeps_body_and_pipelined_authentication_separate() {
    let server = Server::start(&[("DYNAMICAL_API_ORIGIN_SECRET", "fake-secret")]).await;
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    // The second request's authentication must never apply to the first request.
    socket.write_all(b"GET /healthz HTTP/1.1\r\nHost: test\n\nGET /healthz HTTP/1.1\r\nHost: test\r\nX-Dynamical-Origin-Auth: fake-secret\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(status(&response), 403);
    let response = String::from_utf8(response).unwrap();
    assert_eq!(response.matches("HTTP/1.1 ").count(), 2, "{response}");
    assert!(response.contains("HTTP/1.1 200"), "{response}");

    let server = Server::start(&[]).await;
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    socket.write_all(b"POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: 2\n\n{}GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(status(&response), 422);
    assert!(String::from_utf8_lossy(&response).contains("HTTP/1.1 200"));
}

async fn assert_disconnected(socket: &mut TcpStream, within: std::time::Duration) {
    let mut byte = [0];
    let result = tokio::time::timeout(within, socket.read(&mut byte))
        .await
        .expect("connection did not close before deadline");
    match result {
        Ok(0) => (),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ) => {}
        other => panic!("expected disconnect, got {other:?}"),
    }
}

#[tokio::test]
async fn slow_header_deadline_is_not_extended_by_trickling_bytes() {
    let server = Server::start(&[("DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS", "1")]).await;
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    socket
        .write_all(b"GET /healthz HTTP/1.1\r\nHost:")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    socket.write_all(b" test").await.unwrap();
    assert_disconnected(&mut socket, std::time::Duration::from_millis(800)).await;
    assert!(server.sink.0.lock().unwrap().is_empty());
    assert_eq!(
        status(&server.request("GET", "/healthz", "", b"").await),
        200
    );
}

async fn health_on_connection(socket: &mut TcpStream) {
    socket
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut byte = [0];
        while !response.ends_with(b"{\"status\":\"ok\"}") {
            assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
            response.push(byte[0]);
        }
    })
    .await
    .unwrap();
    assert_eq!(status(&response), 200);
}

#[tokio::test]
async fn idle_keep_alive_uses_five_seconds_and_resets_after_response() {
    let server = Server::start(&[]).await;
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    health_on_connection(&mut socket).await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    health_on_connection(&mut socket).await;
    let mut byte = [0];
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), socket.read(&mut byte))
            .await
            .is_err()
    );
    assert_disconnected(&mut socket, std::time::Duration::from_secs(3)).await;
}

#[tokio::test]
async fn body_deadline_bounds_fixed_and_chunked_trickle() {
    let server = Server::start(&[("DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS", "1")]).await;
    for framing in ["Content-Length: 100\r\n", "Transfer-Encoding: chunked\r\n"] {
        let mut socket = TcpStream::connect(server.address).await.unwrap();
        socket
            .write_all(
                format!("POST /v1/forecasts HTTP/1.1\r\nHost: test\r\n{framing}\r\n").as_bytes(),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        socket
            .write_all(if framing.starts_with("Content") {
                b"{"
            } else {
                b"2\r\n{"
            })
            .await
            .unwrap();
        assert_disconnected(&mut socket, std::time::Duration::from_millis(800)).await;
    }
    assert_eq!(
        status(&server.request("GET", "/healthz", "", b"").await),
        200
    );
}

#[tokio::test]
async fn connection_cap_refuses_excess_and_releases_permits() {
    let server = Server::start(&[("DYNAMICAL_API_MAX_CONNECTIONS", "1")]).await;
    let mut first = TcpStream::connect(server.address).await.unwrap();
    health_on_connection(&mut first).await;
    let mut excess = TcpStream::connect(server.address).await.unwrap();
    assert_disconnected(&mut excess, std::time::Duration::from_secs(1)).await;
    drop(first);
    // An exhausted slot is released asynchronously when the first task finishes.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        status(&server.request("GET", "/healthz", "", b"").await),
        200
    );
}

#[tokio::test]
async fn partial_next_header_remains_bounded_by_idle_and_header_deadlines() {
    let server = Server::start(&[
        ("DYNAMICAL_API_KEEP_ALIVE_TIMEOUT_SECONDS", "1"),
        ("DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS", "2"),
    ])
    .await;
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    health_on_connection(&mut socket).await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    socket
        .write_all(b"GET /healthz HTTP/1.1\r\nHost:")
        .await
        .unwrap();
    // D10: idle expiry stays armed until a complete request dispatches;
    // arbitrary incoming bytes cannot remove the connection's last timer.
    assert_disconnected(&mut socket, std::time::Duration::from_millis(800)).await;
}

#[tokio::test]
async fn completed_fixed_and_chunked_bodies_clear_the_body_deadline() {
    let server = Server::start(&[("DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS", "1")]).await;
    for (framing, body) in [
        ("Content-Length: 2\r\n", "{}"),
        (
            "Transfer-Encoding: chunked\r\n",
            "2\r\n{}\r\n0\r\nX-Trailer: test\r\n\r\n",
        ),
    ] {
        let mut socket = TcpStream::connect(server.address).await.unwrap();
        socket.write_all(format!("POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\n{framing}\r\n{body}").as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut byte = [0];
            while !response.ends_with(b"\"input\":{}}]}") {
                assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                response.push(byte[0]);
            }
        })
        .await
        .unwrap();
        assert_eq!(status(&response), 422);
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        health_on_connection(&mut socket).await;
    }
}

#[tokio::test]
async fn lf_trailers_cannot_disable_deadlines_or_retain_connection_permits() {
    for authenticated in [false, true] {
        let mut values = vec![
            ("DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS", "1"),
            ("DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS", "1"),
            ("DYNAMICAL_API_KEEP_ALIVE_TIMEOUT_SECONDS", "1"),
            ("DYNAMICAL_API_MAX_CONNECTIONS", "1"),
        ];
        if authenticated {
            values.push(("DYNAMICAL_API_ORIGIN_SECRET", "fake-secret"));
        }
        let server = Server::start(&values).await;
        let auth = if authenticated {
            "X-Dynamical-Origin-Auth: fake-secret\r\n"
        } else {
            ""
        };
        for pipeline in ["", "GET /healthz HTTP/1.1\r\nHost: test\r\n\r\n"] {
            for suffix in [
                "0\r\n\n",
                "0\r\nX-Foo: bar\n\n",
                "0\r\nX-Foo: bar\r\n baz\r\n\r\n",
                "2\r\n{",
            ] {
                let mut socket = TcpStream::connect(server.address).await.unwrap();
                socket.write_all(format!("POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n{auth}\r\n2\r\n{{}}\r\n{suffix}{pipeline}").as_bytes()).await.unwrap();
                let mut response = Vec::new();
                tokio::time::timeout(
                    std::time::Duration::from_millis(1600),
                    socket.read_to_end(&mut response),
                )
                .await
                .expect("body lost its deadline")
                .unwrap();
                if !pipeline.is_empty() && suffix.ends_with("\n\n") {
                    // Hyper can consume the appended CRLF head as trailer bytes,
                    // then accept the earlier LF header terminator. Its response
                    // must finish and remain covered by header/idle expiry.
                    assert_eq!(status(&response), 422, "{suffix:?}");
                    assert_eq!(
                        String::from_utf8_lossy(&response)
                            .matches("HTTP/1.1 ")
                            .count(),
                        1
                    );
                } else {
                    assert!(
                        response.is_empty(),
                        "suffix={suffix:?}, pipeline={pipeline:?}: {response:?}"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                assert_eq!(
                    status(&server.request("GET", "/healthz", auth, b"").await),
                    200
                );
            }
        }
    }
}

async fn raw_exchange(server: &Server, raw: &[u8]) -> Vec<u8> {
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    socket.write_all(raw).await.unwrap();
    let mut response = Vec::new();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        socket.read_to_end(&mut response),
    )
    .await
    .expect("unbounded connection");
    if let Err(error) = result {
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    }
    response
}

#[tokio::test]
async fn hyper_owns_header_syntax_and_parser_error_responses() {
    let server = Server::start(&[]).await;
    for (fields, body, expected) in [
        ("Content-Length: 2\r\nContent-Length: 2\r\n", "{}", 422),
        ("Content-Length: 2\r\nContent-Length: 02\r\n", "{}", 422),
        ("Content-Length: 2,2\r\n", "{}", 400),
        ("Content-Length: 2\r\nContent-Length: 3\r\n", "{}", 400),
        (
            "Content-Length: +2\r\nTransfer-Encoding: chunked\r\n",
            "2\r\n{}\r\n0\r\n\r\n",
            400,
        ),
        (
            "Transfer-Encoding: chunked\r\nContent-Length: garbage\r\n",
            "2\r\n{}\r\n0\r\n\r\n",
            422,
        ),
        (
            "Transfer-Encoding: gzip, chunked\r\n",
            "2\r\n{}\r\n0\r\n\r\n",
            422,
        ),
        (
            "Transfer-Encoding: identity, chunked\r\n",
            "2\r\n{}\r\n0\r\n\r\n",
            422,
        ),
        (
            "Transfer-Encoding: chunked, chunked\r\n",
            "2\r\n{}\r\n0\r\n\r\n",
            422,
        ),
        (
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
            "2\r\n{}\r\n0\r\n\r\n",
            422,
        ),
        ("Transfer-Encoding: gzip\r\n", "{}", 400),
        ("Content-Length: 2\r\nX-Test: one\r\n two\r\n", "{}", 400),
    ] {
        let response = raw_exchange(&server, format!("POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Type: application/json\r\n{fields}\r\n{body}").as_bytes()).await;
        assert_eq!(status(&response), expected, "{fields:?}");
        if expected == 400 {
            let response = String::from_utf8(response).unwrap();
            assert!(response.ends_with("\r\n\r\n"), "{response}");
            assert!(response.contains("content-length: 0\r\n"), "{response}");
            assert!(response.contains("date:"), "{response}");
            assert!(!response.contains("x-request-id:"), "{response}");
            assert!(!response.contains("server:"), "{response}");
        }
    }
    for host in ["", "Host: a\r\nHost: b\r\n"] {
        let response = raw_exchange(
            &server,
            format!("GET /healthz HTTP/1.1\r\n{host}Connection: close\r\n\r\n").as_bytes(),
        )
        .await;
        assert_eq!(status(&response), 200);
    }
    let response = raw_exchange(
        &server,
        b"GET /healthz HTTP/1.1\nHost: test\nConnection: close\n\n",
    )
    .await;
    assert_eq!(status(&response), 200);
    let response = raw_exchange(
        &server,
        format!(
            "GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\nX-Padding: {}\r\n\r\n",
            "x".repeat(70_000)
        )
        .as_bytes(),
    )
    .await;
    assert_eq!(status(&response), 200);
    for fields in [
        "X-Test: x\r\n".repeat(101),
        format!("X-Padding: {}\r\n", "x".repeat(430_000)),
    ] {
        let response = raw_exchange(
            &server,
            format!("GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{fields}\r\n")
                .as_bytes(),
        )
        .await;
        assert_eq!(status(&response), 431);
        assert!(!String::from_utf8_lossy(&response).contains("x-request-id:"));
    }
}

#[tokio::test]
async fn malformed_chunks_close_without_application_json_or_pipeline_dispatch() {
    let server = Server::start(&[]).await;
    for body in [
        "+2\r\n{}\r\n0\r\n\r\n",
        " 2\r\n{}\r\n0\r\n\r\n",
        "2\r\n{}XX0\r\n\r\n",
        "2\r\n{}\n0\r\n\r\n",
    ] {
        let response = raw_exchange(&server, format!("POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n{body}GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes()).await;
        assert!(response.is_empty(), "{body:?}: {response:?}");
    }
    // Valid framing with invalid JSON remains the application's JSON 400.
    let response = server
        .request(
            "POST",
            "/v1/forecasts",
            "Content-Type: application/json\r\n",
            b"\xff",
        )
        .await;
    assert_eq!(status(&response), 400);
    assert!(String::from_utf8_lossy(&response).contains("There was an error parsing the body"));
    assert!(String::from_utf8_lossy(&response).contains("x-request-id:"));
}

async fn assert_500_closes(server: &Server, headers: &str, body: &str) {
    let request = format!(
        "POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        body.len()
    );
    for pipeline in [
        "",
        "GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
    ] {
        let response = raw_exchange(server, format!("{request}{pipeline}").as_bytes()).await;
        assert_eq!(status(&response), 500);
        assert!(response.ends_with(b"Internal Server Error"));
        let response = String::from_utf8(response).unwrap();
        assert_eq!(response.matches("HTTP/1.1 ").count(), 1, "{response}");
        assert!(response.contains("connection: close\r\n"));
    }
}

#[tokio::test]
async fn unhandled_and_serialization_errors_close_before_queued_requests() {
    let server = Server::start(&[]).await;
    for body in [r#"{"x":NaN}"#, r#"{"x":"\ud800"}"#] {
        assert_500_closes(&server, "", body).await;
    }
    let server = Server::start(&[("DYNAMICAL_API_ORIGIN_SECRET", "fake-secret")]).await;
    assert_500_closes(&server, "X-Dynamical-Origin-Auth: \u{00ff}\r\n", "").await;
}

#[cfg(feature = "harness")]
#[tokio::test]
async fn panic_closes_before_queued_requests() {
    let server = Server::start(&[]).await;
    assert_500_closes(&server, "X-Dynapi-Harness-Panic: 1\r\n", "").await;
}

#[tokio::test]
async fn empty_initial_connection_has_a_header_deadline() {
    let server = Server::start(&[("DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS", "1")]).await;
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    assert_disconnected(&mut socket, std::time::Duration::from_millis(1600)).await;
}

#[tokio::test]
async fn unread_bodies_after_early_responses_do_not_leave_unbounded_connections() {
    let server = Server::start(&[
        ("DYNAMICAL_API_ORIGIN_SECRET", "fake-secret"),
        ("DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS", "1"),
        ("DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS", "1"),
        ("DYNAMICAL_API_KEEP_ALIVE_TIMEOUT_SECONDS", "1"),
    ])
    .await;
    for head in [
        "POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Length: 100\r\n\r\n",
        "POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n",
        "OPTIONS /v1/forecasts HTTP/1.1\r\nHost: test\r\nOrigin: https://example.test\r\nAccess-Control-Request-Method: POST\r\nContent-Length: 100\r\n\r\n",
    ] {
        let response = raw_exchange(&server, head.as_bytes()).await;
        assert!([200, 403].contains(&status(&response)));
    }
}

#[tokio::test]
async fn origin_form_decoding_is_preserved_and_non_origin_forms_are_rejected() {
    let server = Server::start(&[]).await;
    for (target, expected) in [
        ("/healthz", 200),
        ("/%68ealthz", 200),
        ("/healthz?x=%23foo", 200),
        ("/healthz%23foo", 404),
        // D10: Hyper/http discard fragments; no raw request target is exposed.
        ("/healthz#foo", 200),
        ("/%2568ealthz", 404),
        ("/%FFhealthz", 404),
        ("http://example.test/healthz", 400),
        ("example.test:80", 400),
        ("*", 400),
    ] {
        let response = server.request("GET", target, "", b"").await;
        assert_eq!(status(&response), expected, "{target}");
    }
}

#[tokio::test]
async fn body_timeout_records_one_cancellation_and_no_analytics() {
    let server = Server::start(&[
        ("DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS", "1"),
        ("POSTHOG_PROJECT_TOKEN", "fake-test-token"),
    ])
    .await;
    let response = raw_exchange(
        &server,
        b"POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Length: 100\r\n\r\n{",
    )
    .await;
    assert!(response.is_empty());
    let records = server.sink.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, Stream::Terminal);
    assert_eq!(records[0].1["cancelled"], true);
    assert_eq!(records[0].1["status"], serde_json::Value::Null);
}
