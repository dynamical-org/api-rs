#![cfg(feature = "harness")]
#[path = "support/geo_proxy.rs"]
mod proxy;
mod support;
use proxy::*;
use serde_json::json;
use std::sync::atomic::Ordering;
use support::*;

async fn server(proxy: &Proxy, secret: Option<&str>, origin: bool) -> Server {
    let mut values = vec![
        ("DYNAPI_HARNESS_GEO_CHECK_URL", proxy.url.as_str()),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "10000"),
        (
            "DYNAMICAL_API_QUERY_RATE_LIMIT_REQUESTS_PER_MINUTE",
            "10000",
        ),
        ("DYNAMICAL_API_COMPUTE_SLOTS", "1"),
    ];
    if let Some(secret) = secret {
        values.push(("DYNAMICAL_API_GEO_CHECK_SECRET", secret));
    }
    if origin {
        values.push(("DYNAMICAL_API_ORIGIN_SECRET", ORIGIN));
    }
    let server = Server::start(&values).await;
    server.host.reads.gated.store(true, Ordering::SeqCst);
    server
}
fn viewer<'a>(country: &'a str, region: &'a str) -> Vec<(&'static str, &'a str)> {
    vec![
        ("X-Dynamical-Origin-Auth", ORIGIN),
        ("X-Dynamical-Viewer-Auth", ORIGIN),
        ("X-Dynamical-Viewer-Country", country),
        ("X-Dynamical-Viewer-Region-Code", region),
    ]
}
#[tokio::test]
async fn a_refused_mixed_batch_never_opens_any_dataset_and_releases_admission() {
    let proxy = Proxy::start().await;
    let server = server(&proxy, Some(SECRET), true).await;
    let mut body = payload();
    let mut direct = body["queries"][0].clone();
    direct["dataProductId"] = json!("fixture-ensemble");
    body["queries"].as_array_mut().unwrap().insert(0, direct);
    let reply = server
        .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R1"), &body)
        .await;
    assert_eq!(
        reply.status,
        451,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.json(), json!({"detail":BLOCKED}));
    assert_eq!(reply.headers["cache-control"], "private, no-store");
    assert!(!reply.headers.contains_key("etag"));
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|p| !p.contains("/repository/"))
    );
    assert_eq!(proxy.count(), 1);
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}

fn canonical() -> String {
    format!(
        "/v1/data/fixture-forecast/{}/points/0,0?variables=count&initTime=2026-07-30T00:00:00Z",
        input("forecast")["snapshot_id"].as_str().unwrap()
    )
}
fn private(reply: &Reply, status: u16) {
    assert_eq!(
        reply.status,
        status,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.headers["cache-control"], "private, no-store");
}
#[tokio::test]
async fn allowed_post_and_canonical_keep_data_and_etags_but_never_shared_cache() {
    let proxy = Proxy::start().await;
    let server = server(&proxy, Some(SECRET), true).await;
    let post = server
        .request_with_headers("POST", "/v1/forecasts", &viewer("AA", "R1"), &payload())
        .await;
    private(&post, 200);
    assert!(post.headers.contains_key("etag"));
    let get = server
        .request_with_headers("GET", &canonical(), &viewer("AA", "R1"), &json!(null))
        .await;
    private(&get, 200);
    assert!(get.headers.contains_key("etag"));
    assert_eq!(proxy.count(), 1);
    let captured = proxy.state.requests.lock().unwrap();
    assert_eq!(captured[0][":request"], "GET /geo-check HTTP/1.1");
    assert_eq!(captured[0]["x-geo-check-secret"], SECRET);
    assert_eq!(captured[0]["x-geo-check-country"], "AA");
    assert_eq!(captured[0]["x-geo-check-region-code"], "R1");
    assert!(!captured[0].contains_key("x-dynamical-origin-auth"));
}
#[tokio::test]
async fn location_needs_both_current_auth_headers_and_nonempty_fields() {
    let proxy = Proxy::start().await;
    for (origin, headers) in [
        (
            true,
            vec![
                ("X-Dynamical-Origin-Auth", ORIGIN),
                ("X-Dynamical-Viewer-Country", "AA"),
                ("X-Dynamical-Viewer-Region-Code", "R1"),
            ],
        ),
        (
            true,
            vec![
                ("X-Dynamical-Origin-Auth", ORIGIN),
                ("X-Dynamical-Viewer-Auth", "old-rule"),
                ("X-Dynamical-Viewer-Country", "AA"),
            ],
        ),
        (true, viewer("", "")),
        (false, viewer("AA", "R1")),
    ] {
        let server = server(&proxy, Some(SECRET), origin).await;
        private(
            &server
                .request_with_headers("POST", "/v1/forecasts", &headers, &payload())
                .await,
            451,
        );
        let captured = proxy.state.requests.lock().unwrap();
        let last = captured.last().unwrap();
        assert!(!last.contains_key("x-geo-check-country"));
        assert!(!last.contains_key("x-geo-check-region-code"));
    }
    let server = server(&proxy, Some(SECRET), true).await;
    let before = proxy.count();
    let reply = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(reply.status, 403);
    assert_eq!(proxy.count(), before);
    assert_ne!(
        reply.headers.get("cache-control").map(String::as_str),
        Some("private, no-store")
    );
}
#[tokio::test]
async fn absent_secret_fails_closed_without_asking_but_direct_and_unknown_are_unchanged() {
    let proxy = Proxy::start().await;
    for secret in [None, Some("")] {
        let server = server(&proxy, secret, false).await;
        let reply = server.request("POST", "/v1/forecasts", &payload()).await;
        private(&reply, 503);
        assert_eq!(reply.headers["retry-after"], "5");
        let mut body = payload();
        body["queries"][0]["dataProductId"] = json!("fixture-ensemble");
        let reply = server.request("POST", "/v1/forecasts", &body).await;
        assert_eq!(
            reply.status,
            200,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        assert!(reply.headers["cache-control"].starts_with("public"));
        body["queries"][0]["dataProductId"] = json!("missing-product");
        let reply = server.request("POST", "/v1/forecasts", &body).await;
        assert_eq!(reply.status, 404);
        assert_ne!(
            reply.headers.get("cache-control").map(String::as_str),
            Some("private, no-store")
        );
    }
    assert_eq!(proxy.count(), 0);
}
#[tokio::test]
async fn responses_after_identification_are_private_including_validation_errors() {
    let proxy = Proxy::start().await;
    let server = server(&proxy, Some(SECRET), true).await;
    let path = canonical().replace("/0,0?", "/bad,0?");
    private(
        &server
            .request_with_headers("GET", &path, &viewer("AA", "R1"), &json!(null))
            .await,
        422,
    );
    private(
        &server
            .request_with_headers("GET", &canonical(), &viewer("BB", "R1"), &json!(null))
            .await,
        451,
    );
    let malformed = server
        .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R2"), &json!({}))
        .await;
    assert_eq!(malformed.status, 422);
    assert_ne!(
        malformed.headers.get("cache-control").map(String::as_str),
        Some("private, no-store")
    );
    assert_eq!(proxy.count(), 2);
}
fn analysis_payload() -> serde_json::Value {
    json!({"queries":input("analysis")["cases"][0]["queries"]})
}
#[tokio::test]
async fn runs_and_analyses_admit_before_checks_and_refuse_before_store_work() {
    let proxy = Proxy::start().await;
    for (method, path, body) in [
        (
            "GET",
            "/v1/forecasts/runs?dataProductId=fixture-forecast",
            json!(null),
        ),
        ("POST", "/v1/analyses", analysis_payload()),
    ] {
        let server = server(&proxy, Some(SECRET), true).await;
        let before = proxy.count();
        let permit = server.app.gate.try_acquire().unwrap();
        let busy = server
            .request_with_headers(method, path, &viewer("BB", "R1"), &body)
            .await;
        assert_eq!(busy.status, 503);
        assert_eq!(busy.headers["retry-after"], "5");
        assert!(!busy.headers.contains_key("cache-control"));
        assert_eq!(proxy.count(), before);
        assert!(server.host.reads.paths.lock().unwrap().is_empty());
        drop(permit);
        private(
            &server
                .request_with_headers(method, path, &viewer("BB", "R1"), &body)
                .await,
            451,
        );
        assert_eq!(proxy.count(), before + 1);
        assert!(
            server
                .host
                .reads
                .paths
                .lock()
                .unwrap()
                .iter()
                .all(|p| !p.contains("/repository/"))
        );
        assert_eq!(server.app.gate.snapshot().in_use, 0);
        let allowed = server
            .request_with_headers(method, path, &viewer("AA", "R1"), &body)
            .await;
        private(&allowed, 200);
        if method == "GET" {
            assert!(!allowed.headers.contains_key("etag"));
            assert_eq!(allowed.json()["runs"].as_array().unwrap().len(), 2);
        } else {
            assert!(allowed.headers.contains_key("etag"));
            assert_eq!(
                allowed.json()["results"].as_array().unwrap().len(),
                body["queries"].as_array().unwrap().len()
            );
        }
        assert_eq!(server.app.gate.snapshot().in_use, 0);
    }
}
#[tokio::test]
async fn runs_and_analyses_fail_closed_without_geo_configuration_before_store_work() {
    let proxy = Proxy::start().await;
    for (method, path, body) in [
        (
            "GET",
            "/v1/forecasts/runs?dataProductId=fixture-forecast",
            json!(null),
        ),
        ("POST", "/v1/analyses", analysis_payload()),
    ] {
        let server = server(&proxy, None, true).await;
        let reply = server
            .request_with_headers(method, path, &viewer("AA", "R1"), &body)
            .await;
        private(&reply, 503);
        assert_eq!(reply.headers["retry-after"], "5");
        assert!(
            server
                .host
                .reads
                .paths
                .lock()
                .unwrap()
                .iter()
                .all(|p| !p.contains("/repository/"))
        );
        assert_eq!(server.app.gate.snapshot().in_use, 0);
    }
    assert_eq!(proxy.count(), 0);
}
#[tokio::test]
async fn runs_and_analyses_check_hidden_gated_products_before_type_rejection() {
    let proxy = Proxy::start().await;
    let temp = tempfile::tempdir().unwrap();
    let mut collection = input("forecast")["collection"].clone();
    collection["cube:dimensions"]
        .as_object_mut()
        .unwrap()
        .remove("init_time");
    collection["assets"]["icechunk"]["icechunk:virtual_chunk_containers"] = json!([{
        "url_prefix":"https://wn.dynamical.org/chunks/", "credentials":{"type":"http"}
    }]);
    std::fs::write(
        temp.path().join("collection.json"),
        serde_json::to_vec(&collection).unwrap(),
    )
    .unwrap();
    std::fs::write(
        temp.path().join("catalog.json"),
        json!({"links":[{"rel":"child","href":"collection.json"}]}).to_string(),
    )
    .unwrap();
    let server = Server::start_with_catalog_and_root(
        &[
            ("DYNAPI_HARNESS_GEO_CHECK_URL", &proxy.url),
            ("DYNAMICAL_API_GEO_CHECK_SECRET", SECRET),
            ("DYNAMICAL_API_ORIGIN_SECRET", ORIGIN),
        ],
        None,
        Some(temp.path().to_owned()),
    )
    .await;
    let mut analysis = analysis_payload();
    for query in analysis["queries"].as_array_mut().unwrap() {
        query["dataProductId"] = json!("fixture-forecast");
    }
    for (method, path, body) in [
        (
            "GET",
            "/v1/forecasts/runs?dataProductId=fixture-forecast",
            json!(null),
        ),
        ("POST", "/v1/analyses", analysis),
    ] {
        private(
            &server
                .request_with_headers(method, path, &viewer("BB", "R1"), &body)
                .await,
            451,
        );
        private(
            &server
                .request_with_headers(method, path, &viewer("AA", "R1"), &body)
                .await,
            404,
        );
    }
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|p| !p.contains("/repository/"))
    );
}
#[tokio::test]
async fn unsuccessful_checks_redirects_and_oversized_details_are_uncached() {
    let proxy = Proxy::start().await;
    let server = server(&proxy, Some(SECRET), true).await;
    for status in [200, 302, 404, 500] {
        proxy.state.status.store(status, Ordering::SeqCst);
        let before = proxy.count();
        for _ in 0..2 {
            let reply = server
                .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R1"), &payload())
                .await;
            private(&reply, 503);
            assert_eq!(reply.headers["retry-after"], "5");
        }
        assert_eq!(proxy.count(), before + 2);
    }
    proxy.state.status.store(451, Ordering::SeqCst);
    *proxy.state.body.lock().unwrap() = "x".repeat(16385);
    for chunked in [false, true] {
        proxy.state.chunked.store(chunked, Ordering::SeqCst);
        for _ in 0..2 {
            private(
                &server
                    .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R1"), &payload())
                    .await,
                503,
            );
        }
    }
    assert_eq!(proxy.count(), 12);
}
#[tokio::test]
async fn denial_text_trims_python_whitespace_and_empty_text_has_fallback() {
    let proxy = Proxy::start().await;
    let server = server(&proxy, Some(SECRET), true).await;
    for (region, text, expected) in [
        ("R1", "\u{1c}\u{a0}local refusal\n", "local refusal"),
        (
            "R2",
            " \n\t",
            "WeatherNext data is not available from this service in your location.",
        ),
    ] {
        *proxy.state.body.lock().unwrap() = text.into();
        let reply = server
            .request_with_headers("POST", "/v1/forecasts", &viewer("BB", region), &payload())
            .await;
        private(&reply, 451);
        assert_eq!(reply.json(), json!({"detail":expected}));
    }
    *proxy.state.body.lock().unwrap() = SECRET.into();
    let reply = server
        .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R3"), &payload())
        .await;
    private(&reply, 503);
    assert!(!String::from_utf8_lossy(&reply.body).contains(SECRET));
    assert!(!format!("{:?}", server.sink.0.lock().unwrap()).contains(SECRET));
}
#[tokio::test]
async fn allow_and_deny_cache_keys_include_country_region_and_expire_at_five_minutes() {
    let proxy = Proxy::start().await;
    let server = server(&proxy, Some(SECRET), true).await;
    for (country, region, status, count) in [
        ("AA", "R1", 200, 1),
        ("AA", "R1", 200, 1),
        ("BB", "R1", 451, 2),
        ("BB", "R1", 451, 2),
        ("BB", "R2", 451, 3),
        ("AA", "R2", 200, 4),
    ] {
        private(
            &server
                .request_with_headers(
                    "POST",
                    "/v1/forecasts",
                    &viewer(country, region),
                    &payload(),
                )
                .await,
            status,
        );
        assert_eq!(proxy.count(), count);
    }
    server.clock.0.store(299, Ordering::SeqCst);
    private(
        &server
            .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R1"), &payload())
            .await,
        451,
    );
    assert_eq!(proxy.count(), 4);
    server.clock.0.store(300, Ordering::SeqCst);
    private(
        &server
            .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R1"), &payload())
            .await,
        451,
    );
    assert_eq!(proxy.count(), 5);
}

struct Recipes {
    gated: std::sync::atomic::AtomicBool,
    fail: std::sync::atomic::AtomicBool,
}
impl dynapi_catalog::native::Fetcher for Recipes {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        _context: dynapi_catalog::native::FetchContext,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<u8>, dynapi_catalog::native::FetchError>>
    {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err(dynapi_catalog::native::FetchError::Status(404));
            }
            let body = if url.ends_with("catalog.json") {
                json!({"links":[{"rel":"child","href":"forecast.json"}]})
            } else {
                let mut body = input("forecast")["collection"].clone();
                if self.gated.load(Ordering::SeqCst) {
                    body["version"] = json!("proxy-fixture");
                    body["assets"]["icechunk"]["icechunk:virtual_chunk_containers"] = json!([{"url_prefix":"https://wn.dynamical.org/chunks/","credentials":{"type":"http"}}]);
                } else {
                    body["version"] = json!("direct-fixture");
                }
                body
            };
            Ok(serde_json::to_vec(&body).unwrap())
        })
    }
}
async fn recipe_server(
    proxy: &Proxy,
    fail: bool,
) -> (
    Server,
    std::sync::Arc<Recipes>,
    dynapi_catalog::native::StacCatalog,
    std::sync::Arc<TestClock>,
) {
    let recipes = std::sync::Arc::new(Recipes {
        gated: std::sync::atomic::AtomicBool::new(true),
        fail: std::sync::atomic::AtomicBool::new(fail),
    });
    let clock = std::sync::Arc::new(TestClock::default());
    let catalog = dynapi_catalog::native::StacCatalog::with_dependencies(
        "https://catalog.example.test/catalog.json",
        dynapi_catalog::refresh::CatalogConfig::default(),
        clock.clone(),
        recipes.clone(),
        std::sync::Arc::new(dynapi_catalog::native::NativeHooks),
    )
    .unwrap();
    let server = Server::start_with_catalog(
        &[
            ("DYNAPI_HARNESS_GEO_CHECK_URL", &proxy.url),
            ("DYNAMICAL_API_GEO_CHECK_SECRET", SECRET),
            ("DYNAMICAL_API_ORIGIN_SECRET", ORIGIN),
        ],
        Some(catalog.clone()),
    )
    .await;
    (server, recipes, catalog, clock)
}
#[tokio::test]
async fn failed_catalog_lookup_does_not_skip_the_next_check() {
    let proxy = Proxy::start().await;
    let (server, recipes, _catalog, clock) = recipe_server(&proxy, true).await;
    let reply = server
        .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R1"), &payload())
        .await;
    assert_eq!(reply.status, 503);
    assert_eq!(proxy.count(), 0);
    recipes.fail.store(false, Ordering::SeqCst);
    clock.0.store(5, Ordering::SeqCst);
    private(
        &server
            .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R1"), &payload())
            .await,
        451,
    );
    assert_eq!(proxy.count(), 1);
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|p| !p.contains("/repository/"))
    );
}
#[tokio::test]
async fn catalog_refresh_during_check_opens_exactly_the_checked_recipe() {
    let proxy = Proxy::start().await;
    let (server, recipes, catalog, clock) = recipe_server(&proxy, false).await;
    proxy.state.hold.store(true, Ordering::SeqCst);
    let refresh = async {
        proxy.wait_for(1).await;
        recipes.gated.store(false, Ordering::SeqCst);
        clock.0.store(300, Ordering::SeqCst);
        server.app.warm_catalog();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while catalog.status().refreshing {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            catalog.snapshot().await.unwrap().collections["fixture-forecast"].version,
            "direct-fixture"
        );
        proxy.state.hold.store(false, Ordering::SeqCst);
        proxy.state.release.add_permits(1);
    };
    let body = payload();
    let headers = viewer("AA", "R1");
    let (reply, ()) = tokio::join!(
        server.request_with_headers("POST", "/v1/forecasts", &headers, &body),
        refresh
    );
    private(&reply, 200);
    assert_eq!(
        reply.json()["results"][0]["dataProduct"]["version"],
        "proxy-fixture"
    );
    // The changed container recipe must open a new handle.
    let next = server
        .request_with_headers("POST", "/v1/forecasts", &viewer("BB", "R1"), &payload())
        .await;
    assert_eq!(next.status, 200);
    assert_eq!(next.headers["x-dynamical-dataset-cache"], "miss");
    assert_eq!(
        next.json()["results"][0]["dataProduct"]["version"],
        "direct-fixture"
    );
    assert!(next.headers["cache-control"].starts_with("public"));
    assert_eq!(proxy.count(), 1);
}

#[tokio::test]
async fn forecast_and_canonical_busy_precede_catalog_unknown_products_and_geo_checks() {
    let proxy = Proxy::start().await;
    let server = server(&proxy, Some(SECRET), true).await;
    server.host.reads.fail_catalog.store(true, Ordering::SeqCst);
    let permit = server.app.gate.try_acquire().unwrap();
    for missing in [false, true] {
        let mut body = payload();
        let path = if missing {
            body["queries"][0]["dataProductId"] = json!("missing-product");
            canonical().replace("fixture-forecast", "missing-product")
        } else {
            canonical()
        };
        for (method, path, body) in [
            ("POST", "/v1/forecasts".to_owned(), body),
            ("GET", path, json!(null)),
        ] {
            let reply = server
                .request_with_headers(method, &path, &viewer("BB", "R1"), &body)
                .await;
            assert_eq!(reply.status, 503);
            assert_eq!(
                reply.json(),
                json!({"detail":"Server busy; retry after 5 seconds"})
            );
            assert_eq!(reply.headers["retry-after"], "5");
            assert!(!reply.headers.contains_key("cache-control"));
        }
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
    assert_eq!(proxy.count(), 0);
    drop(permit);
    let reply = server
        .request_with_headers("GET", &canonical(), &viewer("AA", "R1"), &json!(null))
        .await;
    assert_eq!(reply.status, 503);
    assert!(!reply.headers.contains_key("retry-after"));
    assert_eq!(proxy.count(), 0);
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}
