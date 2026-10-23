mod support;
use dynapi_policy::Clock;
#[path = "support/connection.rs"]
mod connection;
#[path = "../../../tests/support/time.rs"]
mod time;
use connection::Connection;
use dynapi_server::{
    App, BODY_LIMIT,
    config::Config,
    telemetry::{RecordingSink, Stream},
    transport,
};
use std::{collections::BTreeMap, sync::Arc};
use time::guard;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
// Socket operations use this generous wall-clock deadlock guard. Transport
// deadlines remain independently configured in each behavior test.
const TEST_GUARD: std::time::Duration = std::time::Duration::from_secs(30);
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
        tokio::time::timeout(TEST_GUARD, socket.read_to_end(&mut response))
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

#[test]
fn readiness_configuration_validates_auth_marker_scheme_and_deadlines() {
    let defaults = Config::from_values(&BTreeMap::new()).unwrap();
    assert_eq!(defaults.public_scheme, "http");
    assert!(defaults.release.is_none());
    assert_eq!(defaults.catalog_wait, std::time::Duration::from_secs(10));
    assert_eq!(defaults.drain_timeout, std::time::Duration::from_secs(30));
    assert_eq!(
        defaults.finalization_timeout,
        std::time::Duration::from_secs(15)
    );
    let required = ("DYNAMICAL_API_REQUIRE_ORIGIN_AUTH".into(), "1".into());
    assert!(Config::from_values(&BTreeMap::from([required.clone()])).is_err());
    for secret in ["", " ", "\t\n", "\u{2003}"] {
        assert!(
            Config::from_values(&BTreeMap::from([
                required.clone(),
                ("DYNAMICAL_API_ORIGIN_SECRET".into(), secret.into()),
            ]))
            .is_err()
        );
    }
    assert!(
        Config::from_values(&BTreeMap::from([
            required,
            (
                "DYNAMICAL_API_ORIGIN_SECRET".into(),
                "example-secret".into()
            ),
        ]))
        .is_ok()
    );
    for (key, values) in [
        (
            "DYNAMICAL_API_PUBLIC_SCHEME",
            vec!["ftp", "HTTPS", "https://", ""],
        ),
        (
            "DYNAMICAL_API_RELEASE",
            vec!["bad\r\nInjected: yes", "bad\0value"],
        ),
        (
            "DYNAMICAL_API_CATALOG_WAIT_SECONDS",
            vec!["-1", "NaN", "inf", "1e300"],
        ),
        (
            "DYNAMICAL_API_DRAIN_TIMEOUT_SECONDS",
            vec!["0", "-1", "NaN", "inf", "1e300"],
        ),
        (
            "DYNAMICAL_API_FINALIZATION_TIMEOUT_SECONDS",
            vec!["0", "-1", "NaN", "inf", "1e300"],
        ),
    ] {
        for value in values {
            assert!(
                Config::from_values(&BTreeMap::from([(key.into(), value.into())])).is_err(),
                "{key}: {value}"
            );
        }
    }
    assert!(
        Config::from_values(&BTreeMap::from([(
            "DYNAMICAL_API_CATALOG_WAIT_SECONDS".into(),
            "0".into()
        )]))
        .is_ok()
    );
    assert!(
        Config::from_values(&BTreeMap::from([(
            "DYNAMICAL_API_DRAIN_TIMEOUT_SECONDS".into(),
            "0.1".into()
        )]))
        .is_ok()
    );
}

#[test]
fn finalization_reserve_covers_stdout_and_configured_sdk_shutdown() {
    let key = "DYNAMICAL_API_FINALIZATION_TIMEOUT_SECONDS";
    for sentry in [false, true] {
        let minimum = if cfg!(feature = "telemetry-delivery") && sentry {
            14.0
        } else {
            10.0
        };
        let mut values = BTreeMap::new();
        if sentry {
            values.insert(
                "SENTRY_DSN".into(),
                "https://public@errors.example.invalid/1".into(),
            );
        }
        values.insert(key.into(), (minimum - 0.001_f64).to_string());
        assert_eq!(
            Config::from_values(&values).unwrap_err(),
            "DYNAMICAL_API_FINALIZATION_TIMEOUT_SECONDS must cover telemetry shutdown"
        );
        values.insert(key.into(), minimum.to_string());
        assert!(Config::from_values(&values).is_ok());
    }
}

#[tokio::test]
async fn public_redirect_scheme_uses_host_and_ignores_forwarded_headers() {
    let server = Server::start(&[("DYNAMICAL_API_PUBLIC_SCHEME", "https")]).await;
    let response = server.request("GET", "/v1/data-products/?a=b", "Forwarded: proto=http;host=other.test\r\nX-Forwarded-Proto: http\r\nX-Forwarded-Host: other.test\r\n", b"").await;
    assert_eq!(status(&response), 307);
    assert!(
        String::from_utf8_lossy(&response)
            .contains("location: https://example.test/v1/data-products?a=b\r\n")
    );
    let root = server.request("GET", "/", "", b"").await;
    assert!(String::from_utf8_lossy(&root).contains("location: https://dynamical.org/api/\r\n"));
}

#[tokio::test]
async fn release_marker_covers_application_responses_without_cors_exposure() {
    let server = Server::start(&[("DYNAMICAL_API_RELEASE", "rust:example")]).await;
    for (method, path, headers, body, expected) in [
        ("GET", "/healthz", "", &b""[..], 200),
        ("GET", "/", "", &b""[..], 301),
        ("GET", "/healthz/", "", &b""[..], 307),
        ("GET", "/missing", "", &b""[..], 404),
        ("POST", "/healthz", "", &b""[..], 405),
        (
            "POST",
            "/v1/forecasts",
            "Content-Type: application/json\r\n",
            &b"{}"[..],
            422,
        ),
        (
            "OPTIONS",
            "/v1/forecasts",
            "Origin: https://example.test\r\nAccess-Control-Request-Method: POST\r\n",
            &b""[..],
            200,
        ),
    ] {
        let response = server.request(method, path, headers, body).await;
        assert_eq!(status(&response), expected);
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.contains("x-dynamical-release: rust:example\r\n"),
            "{text}"
        );
        assert!(
            !text
                .lines()
                .filter(|line| line.starts_with("access-control-expose-headers:"))
                .any(|line| line.to_lowercase().contains("x-dynamical-release"))
        );
    }
    let parser_error = raw_exchange(
        &server,
        b"GET / HTTP/1.1\r\nHost: test\r\nContent-Length: no\r\n\r\n",
    )
    .await;
    assert_eq!(status(&parser_error), 400);
    assert!(!String::from_utf8_lossy(&parser_error).contains("x-dynamical-release"));
    let auth = Server::start(&[
        ("DYNAMICAL_API_RELEASE", "rust:example"),
        ("DYNAMICAL_API_ORIGIN_SECRET", "example-secret"),
    ])
    .await;
    let forbidden = auth.request("GET", "/healthz", "", b"").await;
    assert_eq!(status(&forbidden), 403);
    assert!(String::from_utf8_lossy(&forbidden).contains("x-dynamical-release: rust:example\r\n"));
    let unset = Server::start(&[]).await;
    assert!(
        !String::from_utf8_lossy(&unset.request("GET", "/healthz", "", b"").await)
            .contains("x-dynamical-release")
    );
}

#[cfg(feature = "harness")]
#[tokio::test]
async fn release_marker_covers_unhandled_application_errors() {
    let server = Server::start(&[("DYNAMICAL_API_RELEASE", "rust:example")]).await;
    let response = server
        .request("GET", "/healthz", "X-Dynapi-Harness-Panic: 1\r\n", b"")
        .await;
    assert_eq!(status(&response), 500);
    assert!(String::from_utf8_lossy(&response).contains("x-dynamical-release: rust:example\r\n"));
}

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
        ("/v1/forecasts/runs", "GET", 422),
        ("/v1/analyses", "POST", 422),
        ("/v1/data/example/snapshot/points/0", "GET", 422),
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
            404,
            vec!["example-path"],
        ),
        (
            "/v1/data-products/example-path?dataProductId=example-first&dataProductId=",
            404,
            vec!["example-path"],
        ),
        (
            "/v1/data-products/example-path%0A?dataProductId=example-query",
            404,
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
async fn robots_single_range_has_exact_bytes_and_headers() {
    let server = Server::start(&[]).await;
    let full = server.request("GET", "/robots.txt", "", b"").await;
    let (full_headers, _) = response_parts(&full);
    let response = server
        .request("GET", "/robots.txt", "Range: bytes=0-9\r\n", b"")
        .await;
    assert_eq!(status(&response), 206);
    let (headers, body) = response_parts(&response);
    assert_eq!(body, &include_bytes!("../robots.txt")[..10]);
    assert_eq!(
        headers["content-range"],
        format!("bytes 0-9/{}", dynapi_server::ROBOTS.len())
    );
    assert_eq!(headers["content-length"], "10");
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(headers["accept-ranges"], "bytes");
    assert_eq!(headers["cache-control"], "public, max-age=300");
    assert_eq!(headers["etag"], full_headers["etag"]);
    assert_eq!(headers["last-modified"], full_headers["last-modified"]);
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
async fn robots_suffix_open_empty_and_merged_ranges_preserve_file_metadata() {
    let server = Server::start(&[]).await;
    let asset = include_bytes!("../robots.txt");
    let size = asset.len();
    let full = server.request("GET", "/robots.txt", "", b"").await;
    let (metadata, _) = response_parts(&full);
    for (range, start, end) in [
        ("bytes=0-9", 0, 10),
        ("bytes=-9", size - 9, size),
        ("bytes=10-", 10, size),
        ("bytes=0-9999", 0, size),
        ("bytes=-9999", 0, size),
        ("bytes=5-4", 5, 5),
        ("bytes=0--1", 0, 0),
        ("Bytes \t= +0_0 - +0_9", 0, 10),
        ("bytes=junk,, -, 0-9,bad", 0, 10),
        ("bytes=8-9,0-4,3-7", 0, 10),
        ("bytes=4-5,4-5", 4, 6),
        ("bytes=0-0,1-1", 0, 2),
    ] {
        for method in ["GET", "HEAD"] {
            let response = server
                .request(method, "/robots.txt", &format!("Range: {range}\r\n"), b"")
                .await;
            assert_eq!(status(&response), 206, "{method} {range}");
            let (headers, body) = response_parts(&response);
            assert_eq!(
                headers["content-range"],
                format!("bytes {start}-{}/{size}", end as i64 - 1)
            );
            assert_eq!(headers["content-length"], (end - start).to_string());
            assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
            for field in ["accept-ranges", "etag", "last-modified", "cache-control"] {
                assert_eq!(headers[field], metadata[field]);
            }
            assert_eq!(
                body,
                if method == "HEAD" {
                    b""
                } else {
                    &asset[start..end]
                }
            );
        }
    }
}

#[tokio::test]
async fn robots_invalid_ranges_have_exact_plain_text_errors_without_file_headers() {
    let server = Server::start(&[]).await;
    let size = dynapi_server::ROBOTS.len();
    for (range, expected, message) in [
        ("items=0-9".to_owned(), 400, "Only support bytes range"),
        ("bytes0-9".to_owned(), 400, "Malformed range header."),
        (
            "bytes=".to_owned(),
            400,
            "Range header: range must be requested",
        ),
        (
            "bytes=, -, nonsense".to_owned(),
            400,
            "Range header: range must be requested",
        ),
        (
            "bytes=0-bad".to_owned(),
            400,
            "Range header: range must be requested",
        ),
        (
            "bytes=3-1".to_owned(),
            400,
            "Range header: start must be less than end",
        ),
        (
            "bytes=0--2".to_owned(),
            400,
            "Range header: start must be less than end",
        ),
        ("bytes=-0".to_owned(), 416, ""),
        ("bytes=--1".to_owned(), 416, ""),
        (format!("bytes={size}-"), 416, ""),
        (format!("bytes={size}-1"), 416, ""),
        (format!("bytes=3-1,{size}-"), 416, ""),
    ] {
        for method in ["GET", "HEAD"] {
            let response = server
                .request(
                    method,
                    "/robots.txt",
                    &format!("Range: {range}\r\nOrigin: https://example.test\r\n"),
                    b"",
                )
                .await;
            assert_eq!(status(&response), expected, "{method} {range}");
            let (headers, body) = response_parts(&response);
            assert_eq!(
                body,
                if method == "HEAD" {
                    b""
                } else {
                    message.as_bytes()
                }
            );
            assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
            assert_eq!(headers["content-length"], message.len().to_string());
            assert_eq!(
                headers.get("content-range"),
                (expected == 416)
                    .then(|| format!("bytes */{size}"))
                    .as_ref()
            );
            for absent in ["accept-ranges", "etag", "last-modified", "cache-control"] {
                assert!(!headers.contains_key(absent), "{range}: {absent}");
            }
            assert_eq!(headers["access-control-allow-origin"], "*");
            assert!(headers["access-control-expose-headers"].contains("X-Request-Id"));
            assert_eq!(
                uuid::Uuid::parse_str(&headers["x-request-id"])
                    .unwrap()
                    .get_version_num(),
                4
            );
        }
    }
}

#[tokio::test]
async fn robots_multipart_ranges_have_exact_framing_lengths_and_head_headers() {
    let server = Server::start(&[]).await;
    let asset = include_bytes!("../robots.txt");
    let full = server.request("GET", "/robots.txt", "", b"").await;
    let (metadata, _) = response_parts(&full);
    let mut boundaries = std::collections::BTreeSet::new();
    for (value, ranges) in [
        ("bytes=0-1,5-7", [(0, 2), (5, 8)]),
        ("bytes=5-7,0-1,5-7,bad", [(0, 2), (5, 8)]),
        ("bytes=0--1,5-7", [(0, 0), (5, 8)]),
    ] {
        for method in ["GET", "HEAD"] {
            let response = server
                .request(method, "/robots.txt", &format!("Range: {value}\r\n"), b"")
                .await;
            assert_eq!(status(&response), 206);
            let (headers, body) = response_parts(&response);
            let boundary = headers["content-type"]
                .strip_prefix("multipart/byteranges; boundary=")
                .unwrap();
            assert_eq!(boundary.len(), 26);
            assert!(
                boundary
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
            assert!(boundaries.insert(boundary.to_owned()));
            let mut expected = Vec::new();
            for (start, end) in ranges {
                expected.extend_from_slice(format!("--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Range: bytes {start}-{}/{}\r\n\r\n", end as i64 - 1, asset.len()).as_bytes());
                expected.extend_from_slice(&asset[start..end]);
                expected.extend_from_slice(b"\r\n");
            }
            expected.extend_from_slice(format!("--{boundary}--").as_bytes());
            assert_eq!(headers["content-length"], expected.len().to_string());
            assert_eq!(
                body,
                if method == "HEAD" {
                    b""
                } else {
                    expected.as_slice()
                }
            );
            assert!(!headers.contains_key("content-range"));
            for field in ["accept-ranges", "etag", "last-modified", "cache-control"] {
                assert_eq!(headers[field], metadata[field]);
            }
        }
    }
}

#[tokio::test]
async fn robots_if_range_uses_exact_validators_and_bypasses_invalid_ranges_on_mismatch() {
    let server = Server::start(&[]).await;
    let full = server.request("GET", "/robots.txt", "", b"").await;
    let (metadata, _) = response_parts(&full);
    for validator in [
        metadata["etag"].clone(),
        metadata["last-modified"].clone(),
        format!("W/{}", metadata["etag"]),
        "unknown".into(),
        "Fri, 01 Jan 2100 00:00:00 GMT".into(),
    ] {
        let matching = validator == metadata["etag"] || validator == metadata["last-modified"];
        for method in ["GET", "HEAD"] {
            let response = server
                .request(
                    method,
                    "/robots.txt",
                    &format!(
                        "Range: bytes=0-9\r\nIf-Range: {validator}\r\nIf-None-Match: {}\r\n",
                        metadata["etag"]
                    ),
                    b"",
                )
                .await;
            assert_eq!(status(&response), if matching { 206 } else { 200 });
            let (headers, body) = response_parts(&response);
            let expected = if matching {
                &dynapi_server::ROBOTS[..10]
            } else {
                dynapi_server::ROBOTS
            };
            assert_eq!(body, if method == "HEAD" { b"" } else { expected });
            assert_eq!(headers["content-length"], expected.len().to_string());
            assert_eq!(headers["etag"], metadata["etag"]);
            assert_eq!(headers.contains_key("content-range"), matching);
        }
    }
    let bypass = server
        .request(
            "GET",
            "/robots.txt",
            "Range: malformed\r\nIf-Range: unknown\r\n",
            b"",
        )
        .await;
    assert_eq!(status(&bypass), 200);
    assert_eq!(response_parts(&bypass).1, dynapi_server::ROBOTS);
}

#[tokio::test]
async fn robots_range_integer_grammar_keeps_huge_operands_and_digit_guard_explicit() {
    let server = Server::start(&[]).await;
    for (range, expected) in [
        (format!("bytes={}-", "9".repeat(4300)), 416),
        (format!("bytes=-{}", "9".repeat(4300)), 206),
        (format!("bytes=0-{}", "9".repeat(4300)), 206),
        (format!("bytes=0-{}", "9".repeat(4301)), 400),
        ("bytes=0_-9".into(), 400),
        ("bytes=0__0-9".into(), 400),
    ] {
        let response = server
            .request("GET", "/robots.txt", &format!("Range: {range}\r\n"), b"")
            .await;
        assert_eq!(status(&response), expected);
        if expected == 206 {
            assert_eq!(response_parts(&response).1, dynapi_server::ROBOTS);
        }
    }
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
        tokio::time::timeout(TEST_GUARD, socket.read_to_end(&mut response))
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
    tokio::time::timeout(TEST_GUARD, socket.read_to_end(&mut response))
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
    tokio::time::timeout(TEST_GUARD, socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status(&response), 422);
    assert!(String::from_utf8_lossy(&response).contains("HTTP/1.1 200"));
}

async fn assert_disconnected(socket: &mut (impl tokio::io::AsyncRead + Unpin)) {
    let mut byte = [0];
    let result = guard(socket.read(&mut byte)).await;
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

#[tokio::test(start_paused = true)]
async fn slow_header_deadline_is_not_extended_by_trickling_bytes() {
    let server = Server::start(&[("DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS", "1")]).await;
    let mut connection = Connection::start(&server).await;
    connection
        .send_and_wait_for_read(b"GET /healthz HTTP/1.1\r\nHost:")
        .await;
    tokio::time::advance(std::time::Duration::from_millis(600)).await;
    connection.send_and_wait_for_read(b" test").await;
    tokio::time::advance(std::time::Duration::from_millis(399)).await;
    connection.assert_open().await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    connection.expect_close().await;
    assert!(server.sink.0.lock().unwrap().is_empty());
    assert_eq!(
        status(&guard(server.request("GET", "/healthz", "", b"")).await),
        200
    );
}

async fn health_on_connection(socket: &mut TcpStream) {
    guard(socket.write_all(b"GET /healthz HTTP/1.1\r\nHost: test\r\n\r\n"))
        .await
        .unwrap();
    let mut response = Vec::new();
    guard(async {
        let mut byte = [0];
        while !response.ends_with(b"{\"status\":\"ok\"}") {
            assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
            response.push(byte[0]);
        }
    })
    .await;
    assert_eq!(status(&response), 200);
}

#[tokio::test(start_paused = true)]
async fn idle_keep_alive_uses_five_seconds_and_resets_after_response() {
    let server = Server::start(&[]).await;
    let mut socket = guard(TcpStream::connect(server.address)).await.unwrap();
    health_on_connection(&mut socket).await;
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    health_on_connection(&mut socket).await;
    tokio::time::advance(std::time::Duration::from_secs(4)).await;
    // A complete response proves the connection survived beyond its original
    // deadline; a single pending read could just precede the timer task's poll.
    health_on_connection(&mut socket).await;
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    assert_disconnected(&mut socket).await;
}

#[tokio::test(start_paused = true)]
async fn body_deadline_bounds_fixed_and_chunked_trickle() {
    let server = Server::start(&[("DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS", "1")]).await;
    for framing in [
        "Content-Length: 1000000\r\n",
        "Transfer-Encoding: chunked\r\n",
    ] {
        let mut connection = Connection::start(&server).await;
        connection.send_and_wait_for_read(format!("POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nExpect: 100-continue\r\n{framing}\r\n").as_bytes()).await;
        let mut interim = [0; b"HTTP/1.1 100 Continue\r\n\r\n".len()];
        guard(connection.socket.read_exact(&mut interim))
            .await
            .unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        connection
            .send_and_wait_for_read(if framing.starts_with("Content") {
                b"{"
            } else {
                b"f4240\r\n{"
            })
            .await;
        tokio::time::advance(std::time::Duration::from_millis(600)).await;
        connection.send_and_wait_for_read(b" ").await;
        tokio::time::advance(std::time::Duration::from_millis(399)).await;
        connection.assert_open().await;
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        connection.expect_close().await;
    }
    assert_eq!(
        status(&guard(server.request("GET", "/healthz", "", b"")).await),
        200
    );
}

#[tokio::test(start_paused = true)]
async fn connection_cap_refuses_excess_and_releases_permits() {
    let server = Server::start(&[("DYNAMICAL_API_MAX_CONNECTIONS", "1")]).await;
    let mut first = guard(TcpStream::connect(server.address)).await.unwrap();
    health_on_connection(&mut first).await;
    let mut excess = guard(TcpStream::connect(server.address)).await.unwrap();
    assert_disconnected(&mut excess).await;
    health_on_connection(&mut first).await;
    drop(first);
    wait_for_connection_slot(&server, "").await;
}

async fn wait_for_connection_slot(server: &Server, auth: &str) {
    // EOF can reach the client before the server task drops its permit. Retry
    // until a request is accepted instead of guessing how long task cleanup takes.
    guard(async {
        loop {
            let mut socket = TcpStream::connect(server.address).await.unwrap();
            let request =
                format!("GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{auth}\r\n");
            if socket.write_all(request.as_bytes()).await.is_ok() {
                let mut response = Vec::new();
                if socket.read_to_end(&mut response).await.is_ok() && !response.is_empty() {
                    assert_eq!(status(&response), 200);
                    return;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn partial_next_header_remains_bounded_by_idle_and_header_deadlines() {
    let server = Server::start(&[
        ("DYNAMICAL_API_KEEP_ALIVE_TIMEOUT_SECONDS", "1"),
        ("DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS", "2"),
    ])
    .await;
    let mut connection = Connection::start(&server).await;
    health_on_connection(&mut connection.socket).await;
    tokio::time::advance(std::time::Duration::from_millis(700)).await;
    connection
        .send_and_wait_for_read(b"GET /healthz HTTP/1.1\r\nHost:")
        .await;
    // D10: idle expiry stays armed until a complete request dispatches;
    // arbitrary incoming bytes cannot remove the connection's last timer.
    tokio::time::advance(std::time::Duration::from_millis(299)).await;
    connection.assert_open().await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    connection.expect_close().await;
}

#[tokio::test(start_paused = true)]
async fn completed_fixed_and_chunked_bodies_clear_the_body_deadline() {
    let server = Server::start(&[("DYNAMICAL_API_BODY_READ_TIMEOUT_SECONDS", "1")]).await;
    for (framing, body) in [
        ("Content-Length: 2\r\n", "{}"),
        (
            "Transfer-Encoding: chunked\r\n",
            "2\r\n{}\r\n0\r\nX-Trailer: test\r\n\r\n",
        ),
    ] {
        let mut socket = guard(TcpStream::connect(server.address)).await.unwrap();
        guard(socket.write_all(format!("POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\n{framing}\r\n{body}").as_bytes())).await.unwrap();
        let mut response = Vec::new();
        guard(async {
            let mut byte = [0];
            while !response.ends_with(b"\"input\":{}}]}") {
                assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                response.push(byte[0]);
            }
        })
        .await;
        assert_eq!(status(&response), 422);
        tokio::time::advance(std::time::Duration::from_millis(1200)).await;
        health_on_connection(&mut socket).await;
    }
}

#[tokio::test(start_paused = true)]
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
                let mut socket = guard(TcpStream::connect(server.address)).await.unwrap();
                guard(socket.write_all(format!("POST /v1/forecasts HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n{auth}\r\n").as_bytes())).await.unwrap();
                // The interim response acknowledges that the application is
                // polling the body and its deadline is armed, before time moves.
                let mut interim = [0; b"HTTP/1.1 100 Continue\r\n\r\n".len()];
                guard(socket.read_exact(&mut interim)).await.unwrap();
                assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
                guard(socket.write_all(format!("2\r\n{{}}\r\n{suffix}{pipeline}").as_bytes()))
                    .await
                    .unwrap();
                let mut response = Vec::new();
                if !pipeline.is_empty() && suffix.ends_with("\n\n") {
                    // Hyper can consume the appended CRLF head as trailer bytes,
                    // then accept the earlier LF header terminator. Its response
                    // must finish and remain covered by header/idle expiry.
                    guard(async {
                        let mut byte = [0];
                        while !response.ends_with(b"\"input\":{}}]}") {
                            assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                            response.push(byte[0]);
                        }
                    })
                    .await;
                    assert_eq!(status(&response), 422, "{suffix:?}");
                }
                tokio::time::advance(std::time::Duration::from_secs(1)).await;
                guard(socket.read_to_end(&mut response)).await.unwrap();
                if !pipeline.is_empty() && suffix.ends_with("\n\n") {
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
                wait_for_connection_slot(&server, auth).await;
            }
        }
    }
}

async fn raw_exchange(server: &Server, raw: &[u8]) -> Vec<u8> {
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    socket.write_all(raw).await.unwrap();
    let mut response = Vec::new();
    let result = tokio::time::timeout(TEST_GUARD, socket.read_to_end(&mut response))
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
    let prefix = "GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n";
    let padding = "X-Padding: \r\n\r\n".len();
    // Include Host and Connection in the 100-field limit and all framing in
    // the 417,792-byte limit. Complete heads must obey both explicit limits.
    for (label, fields) in [
        ("100 fields", "X-Test: x\r\n".repeat(98)),
        (
            "417,792 bytes",
            format!(
                "X-Padding: {}\r\n",
                "x".repeat(417_792 - prefix.len() - padding)
            ),
        ),
    ] {
        let response = raw_exchange(&server, format!("{prefix}{fields}\r\n").as_bytes()).await;
        assert_eq!(status(&response), 200, "{label}");
    }
    for (label, fields) in [
        ("101 fields", "X-Test: x\r\n".repeat(99)),
        (
            "417,793 bytes",
            format!(
                "X-Padding: {}\r\n",
                "x".repeat(417_793 - prefix.len() - padding)
            ),
        ),
        ("header count", "X-Test: x\r\n".repeat(101)),
        (
            "header bytes",
            format!("X-Padding: {}\r\n", "x".repeat(430_000)),
        ),
    ] {
        let response = raw_exchange(
            &server,
            format!("GET /healthz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{fields}\r\n")
                .as_bytes(),
        )
        .await;
        assert_eq!(status(&response), 431, "{label}");
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

#[tokio::test(start_paused = true)]
async fn empty_initial_connection_has_a_header_deadline() {
    let server = Server::start(&[("DYNAMICAL_API_HEADER_READ_TIMEOUT_SECONDS", "1")]).await;
    let connection = Connection::start(&server).await;
    tokio::time::advance(std::time::Duration::from_millis(999)).await;
    connection.assert_open().await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    connection.expect_close().await;
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
