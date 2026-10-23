//! Expectations follow the fictional STAC inputs and HTTP/OpenAPI invariants.
use dynapi_catalog::{
    native::{FetchContext, FetchError, Fetcher, MonotonicClock, NativeHooks, StacCatalog},
    refresh::CatalogConfig,
};
use dynapi_server::{
    App,
    config::Config,
    telemetry::{RecordingSink, Stream},
    transport,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const FIXTURE: &[u8] = include_bytes!("fixtures/catalog.json");
#[path = "support/allocation.rs"]
mod allocation;
#[global_allocator]
static ALLOCATOR: allocation::Allocator = allocation::Allocator;

struct FixtureFetcher {
    fixture: Arc<[u8]>,
    failure: bool,
    delay: bool,
    calls: Arc<AtomicUsize>,
}
impl Fetcher for FixtureFetcher {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        _: FetchContext,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.delay {
                std::future::pending::<()>().await;
            }
            if self.failure {
                return Err(FetchError::Status(404));
            }
            let fixture = dynapi_catalog::parse_document(&self.fixture).unwrap();
            let collections = fixture["collections"].as_array().unwrap();
            let body = if url.ends_with("/catalog.json") {
                json!({"links": collections.iter().enumerate().map(|(index, _)| json!({"rel":"child", "href":format!("{index}.json")})).collect::<Vec<_>>()})
            } else {
                collections[url
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .trim_end_matches(".json")
                    .parse::<usize>()
                    .unwrap()]
                .clone()
            };
            Ok(serde_json::to_vec(&body).unwrap())
        })
    }
}
struct Server {
    address: std::net::SocketAddr,
    catalog: StacCatalog,
    app: Arc<App>,
    sink: Arc<RecordingSink>,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(failure: bool, delay: bool, secret: bool) -> Self {
        Self::start_with_fixture(failure, delay, secret, Arc::from(FIXTURE)).await
    }
    async fn start_with_fixture(
        failure: bool,
        delay: bool,
        secret: bool,
        fixture: Arc<[u8]>,
    ) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let catalog = StacCatalog::with_dependencies(
            "https://catalog.example.test/catalog.json",
            CatalogConfig {
                // Only stalled-fetch cases need a short timeout; fixture loads
                // must tolerate parser-worker scheduling on a busy host.
                cold_wait_seconds: if delay { 0.02 } else { 10.0 },
                ..CatalogConfig::default()
            },
            Arc::new(MonotonicClock::default()),
            Arc::new(FixtureFetcher {
                fixture,
                failure,
                delay,
                calls: calls.clone(),
            }),
            Arc::new(NativeHooks),
        )
        .unwrap();
        let mut values = BTreeMap::new();
        if secret {
            values.insert("DYNAMICAL_API_ORIGIN_SECRET".into(), "fake-secret".into());
        }
        let sink = Arc::new(RecordingSink::default());
        let app = App::with_catalog(
            Config::from_values(&values).unwrap(),
            Arc::new(dynapi_server::Monotonic::default()),
            sink.clone(),
            catalog.clone(),
        )
        .unwrap();
        let app = Arc::new(app);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serve = app.clone();
        let task = tokio::spawn(async move {
            transport::run(listener, serve).await.unwrap();
        });
        Self {
            address,
            catalog,
            app,
            sink,
            calls,
            task,
        }
    }
    async fn request(&self, method: &str, path: &str, headers: &str) -> Reply {
        let mut socket = TcpStream::connect(self.address).await.unwrap();
        socket.write_all(format!("{method} {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\nContent-Length: 0\r\n{headers}\r\n").as_bytes()).await.unwrap();
        let mut bytes = vec![];
        tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = std::str::from_utf8(&bytes[..split]).unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let headers = head
            .split("\r\n")
            .skip(1)
            .map(|line| {
                let (key, value) = line.split_once(": ").unwrap();
                (key.to_ascii_lowercase(), value.to_owned())
            })
            .collect();
        Reply {
            status,
            headers,
            body: bytes[split + 4..].to_vec(),
        }
    }
}
struct Reply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    fn no_cache(&self) {
        for key in ["cache-control", "etag", "retry-after"] {
            assert!(!self.headers.contains_key(key), "{key}");
        }
    }
}

#[tokio::test]
async fn catalog_listing_preserves_order_visibility_and_capabilities_without_compute_slots() {
    let server = Server::start(false, false, false).await;
    let _permits: Vec<_> = (0..server.app.config.compute_slots)
        .map(|_| server.app.gate.try_acquire().unwrap())
        .collect();
    let response = server
        .request(
            "GET",
            "/v1/data-products?ignored=yes",
            "If-None-Match: *\r\n",
        )
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers["cache-control"],
        "public, max-age=300, stale-while-revalidate=600"
    );
    assert!(!response.headers.contains_key("etag"));
    assert_eq!(response.headers["content-type"], "application/json");
    let body = response.json();
    let products = body["dataProducts"].as_array().unwrap();
    assert_eq!(
        products
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "z-example-forecast",
            "a-example-analysis",
            "example-unsupported-grid"
        ]
    );
    assert_eq!(
        products[0],
        json!({
            "id":"z-example-forecast", "name":"Example météo ☀", "description":"A fictional forecast.", "type":"forecast",
            "modelId":"example-model", "modelName":"Example Model", "version":"v1", "variableCount":2,
            "capabilities":["forecastTimeSeries"], "links":{"self":"/v1/data-products/z-example-forecast", "catalog":"https://catalog.example.test/z-example-forecast.json"}
        })
    );
    assert_eq!(products[1]["capabilities"], json!(["analysisTimeSeries"]));
    assert_eq!(products[2]["capabilities"], json!([]));
    assert_eq!(products[1]["modelId"], Value::Null);
    assert_eq!(products[1]["links"]["catalog"], Value::Null);
    assert!(std::str::from_utf8(&response.body).unwrap().starts_with("{\"dataProducts\":[{\"id\":\"z-example-forecast\",\"name\":\"Example météo ☀\",\"description\":"));
    assert_eq!(server.calls.load(Ordering::SeqCst), 5);
    assert_eq!(
        server.request("GET", "/v1/data-products", "").await.body,
        response.body
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn catalog_detail_includes_nullable_metadata_and_preserves_numeric_and_field_order() {
    let server = Server::start(false, false, false).await;
    let response = server
        .request("GET", "/v1/data-products/z-example-forecast", "")
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.json()["boundingBox"],
        json!([-180.0, -90.0, 180.0, 90.0])
    );
    assert_eq!(
        response.json()["variables"],
        json!({
            "wind":{"dimensions":["init_time","lead_time","latitude","longitude"],"unit":"m/s","longName":"Example wind","standardName":"wind_speed"},
            "temperature":{"dimensions":["init_time","lead_time","latitude","longitude"],"unit":null,"longName":null,"standardName":null}
        })
    );
    let text = std::str::from_utf8(&response.body).unwrap();
    assert!(text.contains("\"boundingBox\":[-180.0,-90.0,180.0,90.0]"));
    assert!(text.contains("\"longitude\":{\"type\":\"spatial\",\"step\":0.00001,\"offset\":-0.0,\"tiny\":1e-6,\"scale\":1e+16}"));
    assert!(text.find("\"wind\":").unwrap() < text.find("\"temperature\":").unwrap());
    let analysis = server
        .request("GET", "/v1/data-products/a-example-analysis", "")
        .await;
    assert_eq!(analysis.json()["boundingBox"], Value::Null);
    assert_eq!(analysis.json()["license"], "proprietary");
    assert_eq!(analysis.json()["version"], "unknown");
    let unsupported = server
        .request("GET", "/v1/data-products/example-unsupported-grid", "")
        .await;
    assert_eq!(unsupported.status, 200);
    assert_eq!(
        unsupported.json()["boundingBox"],
        json!([null, null, null, 0.0])
    );
}

#[tokio::test]
async fn unknown_and_hidden_products_use_the_public_error_envelope() {
    let server = Server::start(false, false, false).await;
    for (id, expected) in [
        ("example-hidden", "Unknown data product 'example-hidden'"),
        ("missing%27id", "Unknown data product \"missing'id\""),
        ("missing%0Aid", "Unknown data product 'missing\\nid'"),
        ("%E2%98%83", "Unknown data product '☃'"),
    ] {
        let response = server
            .request("GET", &format!("/v1/data-products/{id}"), "")
            .await;
        assert_eq!(response.status, 404);
        assert_eq!(response.json(), json!({"detail":expected}));
        response.no_cache();
    }
}

#[tokio::test]
async fn catalog_load_failure_and_cold_timeout_are_503_without_cache_or_retry_after() {
    for (failure, delay, detail) in [
        (true, false, "STAC catalog refresh failed (HTTPStatusError)"),
        (false, true, "Timed out waiting for STAC catalog"),
    ] {
        let server = Server::start(failure, delay, false).await;
        for path in ["/v1/data-products", "/v1/data-products/unknown"] {
            let response = server
                .request("GET", path, "Origin: https://example.test\r\n")
                .await;
            assert_eq!(response.status, 503);
            assert_eq!(response.json(), json!({"detail":detail}));
            response.no_cache();
            assert_eq!(response.headers["access-control-allow-origin"], "*");
        }
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn catalog_methods_and_redirects_do_not_load_the_catalog() {
    let server = Server::start(false, true, false).await;
    for path in ["/v1/data-products", "/v1/data-products/example"] {
        for method in ["HEAD", "POST", "PUT", "DELETE", "PATCH"] {
            let response = server.request(method, path, "").await;
            assert_eq!(response.status, 405);
            assert_eq!(response.headers["allow"], "GET");
            assert_eq!(response.headers["content-length"], "31");
            if method == "HEAD" {
                assert!(response.body.is_empty());
            } else {
                assert_eq!(response.json(), json!({"detail":"Method Not Allowed"}));
            }
            response.no_cache();
        }
        let response = server
            .request("OPTIONS", path, "Origin: https://example.test\r\n")
            .await;
        assert_eq!(response.status, 204);
        assert!(response.body.is_empty());
        assert!(!response.headers.contains_key("content-type"));
        assert_eq!(response.headers["access-control-allow-origin"], "*");
        for method in ["GET", "HEAD", "OPTIONS", "POST"] {
            let response = server
                .request(method, &format!("{path}///?x=%2F&x=2"), "")
                .await;
            assert_eq!(response.status, 307);
            assert_eq!(
                response.headers["location"],
                format!("http://example.test{path}?x=%2F&x=2")
            );
            response.no_cache();
        }
    }
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
}

const DOC_PATHS: [&str; 4] = ["/openapi.json", "/docs", "/docs/oauth2-redirect", "/redoc"];
#[tokio::test]
async fn documentation_get_head_options_and_redirect_contract() {
    let server = Server::start(false, true, false).await;
    for path in DOC_PATHS {
        let get = server.request("GET", path, "If-None-Match: *\r\n").await;
        assert_eq!(get.status, 200);
        get.no_cache();
        assert_eq!(get.headers["content-length"], get.body.len().to_string());
        assert_eq!(
            get.headers["content-type"],
            if path == "/openapi.json" {
                "application/json"
            } else {
                "text/html; charset=utf-8"
            }
        );
        let head = server.request("HEAD", path, "").await;
        assert_eq!(head.status, 200);
        assert!(head.body.is_empty());
        assert_eq!(
            head.headers["content-length"],
            get.headers["content-length"]
        );
        assert_eq!(head.headers["content-type"], get.headers["content-type"]);
        head.no_cache();
        for method in ["OPTIONS", "POST", "PATCH", "PUT", "DELETE"] {
            let response = server.request(method, path, "").await;
            assert_eq!(response.status, 405);
            let allow = response.headers["allow"]
                .split(", ")
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(allow, ["GET", "HEAD"].into_iter().collect());
            assert_eq!(response.json(), json!({"detail":"Method Not Allowed"}));
            response.no_cache();
        }
        for method in ["GET", "HEAD", "OPTIONS", "POST"] {
            let response = server.request(method, &format!("{path}/?a=1"), "").await;
            assert_eq!(response.status, 307);
            assert_eq!(
                response.headers["location"],
                format!("http://example.test{path}?a=1")
            );
        }
    }
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn documentation_links_and_oauth_flow_are_usable() {
    let server = Server::start(false, true, false).await;
    let swagger = server.request("GET", "/docs", "").await;
    let text = std::str::from_utf8(&swagger.body).unwrap();
    for expected in [
        "url: '/openapi.json'",
        "oauth2RedirectUrl: window.location.origin + '/docs/oauth2-redirect'",
        "SwaggerUIBundle",
        "swagger-ui-dist@5",
        "<title>Dynamical API - Swagger UI</title>",
    ] {
        assert!(text.contains(expected), "{expected}");
    }
    let redirect = server.request("GET", "/docs/oauth2-redirect", "").await;
    let text = std::str::from_utf8(&redirect.body).unwrap();
    for expected in [
        "window.opener.swaggerUIRedirectOauth2",
        "qp.state === sentState",
        "oauth2.callback",
        "oauth2.errCb",
        "window.close()",
        "DOMContentLoaded",
    ] {
        assert!(text.contains(expected));
    }
    let redoc = server.request("GET", "/redoc", "").await;
    let text = std::str::from_utf8(&redoc.body).unwrap();
    assert!(text.contains("<redoc spec-url=\"/openapi.json\"></redoc>"));
    assert!(text.contains("redoc@2/bundles/redoc.standalone.js"));
    let document = server.request("GET", "/openapi.json", "").await;
    assert_eq!(document.json()["openapi"], "3.1.0");
    assert_eq!(document.json()["info"]["title"], "Dynamical API");
    assert_eq!(
        server.request("GET", "/openapi.json", "").await.body,
        document.body
    );
}

#[tokio::test]
async fn origin_guard_and_cors_preflight_cover_every_new_route() {
    let server = Server::start(false, true, true).await;
    for path in DOC_PATHS
        .into_iter()
        .chain(["/v1/data-products", "/v1/data-products/example"])
    {
        for method in ["GET", "HEAD", "OPTIONS"] {
            server.sink.0.lock().unwrap().clear();
            let response = server
                .request(method, path, "Origin: https://example.test\r\n")
                .await;
            assert_eq!(response.status, 403);
            assert_eq!(response.headers["access-control-allow-origin"], "*");
            response.no_cache();
            let records = server.sink.0.lock().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].0, Stream::Terminal);
            let expected_route = if DOC_PATHS.contains(&path) && method != "OPTIONS"
                || path.starts_with("/v1/data-products") && method == "OPTIONS"
            {
                path
            } else {
                "unrouted"
            };
            let expected_route = if expected_route == "/v1/data-products/example" {
                "/v1/data-products/{dataProductId}"
            } else {
                expected_route
            };
            assert_eq!(records[0].1["route"], expected_route);
        }
        let response = server
            .request(
                "OPTIONS",
                path,
                "Origin: https://example.test\r\nAccess-Control-Request-Method: GET\r\n",
            )
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"OK");
        let response = server
            .request(
                "OPTIONS",
                path,
                "Origin: https://example.test\r\nAccess-Control-Request-Method: DELETE\r\n",
            )
            .await;
        assert_eq!(response.status, 400);
    }
    for path in DOC_PATHS {
        assert_eq!(
            server
                .request("GET", path, "X-Dynamical-Origin-Auth: fake-secret\r\n")
                .await
                .status,
            200
        );
    }
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
}

async fn large_metadata_server(projected_in_summary: bool) -> Server {
    let mut fixture: Value = serde_json::from_slice(FIXTURE).unwrap();
    let padding = Value::String("x".repeat(2 * 1024 * 1024));
    if projected_in_summary {
        fixture["collections"][0]["description"] = padding;
    } else {
        fixture["collections"][0]["cube:dimensions"]["latitude"]["unused"] = padding;
    }
    let server = Server::start_with_fixture(
        false,
        false,
        false,
        Arc::from(serde_json::to_vec(&fixture).unwrap()),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while server.catalog.snapshot().await.is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server
}

#[tokio::test(flavor = "current_thread")]
async fn catalog_list_does_not_copy_large_unprojected_metadata() {
    let server = large_metadata_server(false).await;
    let _permits: Vec<_> = (0..server.app.config.compute_slots)
        .map(|_| server.app.gate.try_acquire().unwrap())
        .collect();
    for _ in 0..3 {
        let tracking = allocation::Tracking::start();
        let response = server.request("GET", "/v1/data-products", "").await;
        let stats = tracking.finish();
        assert_eq!(response.status, 200);
        assert!(response.body.len() < 2048);
        assert!(stats.largest < 128 * 1024, "request allocation: {stats:?}");
        assert!(stats.total < 256 * 1024, "request allocations: {stats:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_catalog_projections_fail_before_large_allocations() {
    for projected_in_summary in [false, true] {
        let server = large_metadata_server(projected_in_summary).await;
        let path = if projected_in_summary {
            "/v1/data-products"
        } else {
            "/v1/data-products/z-example-forecast"
        };
        let tracking = allocation::Tracking::start();
        let response = server.request("GET", path, "").await;
        let stats = tracking.finish();
        assert_eq!(response.status, 500);
        assert_eq!(response.body, b"Internal Server Error");
        response.no_cache();
        assert_eq!(response.headers["connection"], "close");
        assert!(stats.largest < 128 * 1024, "request allocation: {stats:?}");
        assert!(stats.total < 256 * 1024, "request allocations: {stats:?}");
        let records = server.sink.0.lock().unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|(_, record)| record["event"] == "api_failure")
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|(stream, record)| *stream == Stream::Terminal
                    && record["errorCategory"] == "catalog_response_too_large")
                .count(),
            1
        );
    }
}
