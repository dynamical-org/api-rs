#![cfg(feature = "harness")]
//! HTTP expectations are explicit public response/header and fixture contracts.
mod support;

use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use support::{Reply, Server, payload};

const ORIGIN: (&str, &str) = ("Origin", "https://client.example.test");
const AUTH: (&str, &str) = ("X-Dynamical-Origin-Auth", "fake-origin-key");
const EXPOSED: &str = "ETag, Retry-After, X-Request-Id, Server-Timing, X-Dynamical-Dataset-Cache, X-Dynamical-Work-Units";
const RATE_BODY: &[u8] = br#"{"detail":"Request rate limit exceeded; contact feedback@dynamical.org if you need higher limits"}"#;
const BUSY_BODY: &[u8] = br#"{"detail":"Server busy; retry after 5 seconds"}"#;
const METHOD_BODY: &[u8] = br#"{"detail":"Method Not Allowed"}"#;
const CANONICAL: &str = "/v1/data/fixture-forecast/00000000000000000000/points/0,0?variables=count&initTime=2026-07-30T00:00:00Z";

fn request_id(reply: &Reply) -> &str {
    let text = reply.headers["x-request-id"].as_str();
    assert_eq!(text.len(), 32);
    assert!(
        text.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    let id = uuid::Uuid::parse_str(text).unwrap();
    assert_eq!(id.get_version_num(), 4);
    assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    text
}

fn assert_simple_cors(reply: &Reply) {
    assert_eq!(reply.headers["access-control-allow-origin"], "*");
    assert_eq!(reply.headers["access-control-expose-headers"], EXPOSED);
    request_id(reply);
}

fn assert_error(reply: &Reply, status: u16, body: &[u8], retry: Option<&str>) {
    assert_eq!(
        reply.status,
        status,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.body, body);
    assert_eq!(reply.headers["content-type"], "application/json");
    assert_eq!(reply.headers["content-length"], body.len().to_string());
    assert_eq!(reply.headers.get("retry-after").map(String::as_str), retry);
    assert!(!reply.headers.contains_key("cache-control"));
    assert!(!reply.headers.contains_key("etag"));
    assert_simple_cors(reply);
}

#[tokio::test]
async fn preflight_headers_and_request_ids_cover_allowed_and_rejected_requests() {
    let server = Server::start(&[("DYNAMICAL_API_ORIGIN_SECRET", "fake-origin-key")]).await;
    let mut ids = BTreeSet::new();
    for path in ["/v1/forecasts", CANONICAL, "/missing"] {
        for (method, headers, status, body) in [
            ("POST", Some("content-type"), 200, "OK"),
            (
                "GET",
                Some("Accept, Accept-Language, Content-Language, Content-Type"),
                200,
                "OK",
            ),
            ("HEAD", None, 200, "OK"),
            ("OPTIONS", Some("Content-Type"), 200, "OK"),
            (
                "DELETE",
                Some("content-type"),
                400,
                "Disallowed CORS method",
            ),
            (
                "POST",
                Some("Authorization"),
                400,
                "Disallowed CORS headers",
            ),
        ] {
            let mut request_headers = vec![ORIGIN, ("Access-Control-Request-Method", method)];
            if let Some(headers) = headers {
                request_headers.push(("Access-Control-Request-Headers", headers));
            }
            let reply = server
                .request_with_headers("OPTIONS", path, &request_headers, &Value::Null)
                .await;
            assert_eq!(reply.status, status);
            assert_eq!(reply.body, body.as_bytes());
            assert_eq!(reply.headers["content-type"], "text/plain; charset=utf-8");
            assert_eq!(reply.headers["content-length"], body.len().to_string());
            assert_eq!(reply.headers["access-control-allow-origin"], "*");
            assert_eq!(
                reply.headers["access-control-allow-methods"],
                "GET, HEAD, POST, OPTIONS"
            );
            assert_eq!(reply.headers["access-control-max-age"], "86400");
            assert_eq!(
                reply.headers["access-control-allow-headers"],
                "Accept, Accept-Language, Content-Language, Content-Type, content-type"
            );
            assert!(!reply.headers.contains_key("access-control-expose-headers"));
            assert!(!reply.headers.contains_key("cache-control"));
            assert!(ids.insert(request_id(&reply).to_owned()));
        }
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
    assert!(server.sink.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn forbidden_rate_limited_and_unmatched_bodies_are_exact_json() {
    let server = Server::start(&[
        ("DYNAMICAL_API_ORIGIN_SECRET", "fake-origin-key"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "1"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_REQUESTS_PER_MINUTE", "60"),
    ])
    .await;
    for auth in [None, Some(("X-Dynamical-Origin-Auth", "wrong-key"))] {
        let mut headers = vec![ORIGIN];
        headers.extend(auth);
        let reply = server
            .request_with_headers("POST", "/v1/forecasts", &headers, &json!({}))
            .await;
        assert_error(&reply, 403, br#"{"detail":"Forbidden"}"#, None);
    }
    let invalid = server
        .request_with_headers("POST", "/v1/forecasts", &[ORIGIN, AUTH], &json!({}))
        .await;
    assert_eq!(invalid.status, 422);
    let limited = server
        .request_with_headers("POST", "/v1/forecasts", &[ORIGIN, AUTH], &json!({}))
        .await;
    assert_error(&limited, 429, RATE_BODY, Some("1"));
    let missing = server
        .request_with_headers("GET", "/v1/example-missing", &[ORIGIN, AUTH], &Value::Null)
        .await;
    assert_error(&missing, 404, br#"{"detail":"Not Found"}"#, None);
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unsupported_product_reasons_and_type_precedence_precede_dataset_open() {
    let root = tempfile::tempdir().unwrap();
    let fixture: Value = serde_json::from_slice(include_bytes!("fixtures/catalog.json")).unwrap();
    let mut collections = fixture["collections"].as_array().unwrap().clone();
    let mut forecast = collections[2].clone();
    forecast["id"] = json!("example-unsupported-forecast-grid");
    forecast["cube:dimensions"] = json!({"init_time":{},"lead_time":{},"station":{}});
    collections.push(forecast);
    let mut timeless = collections[2].clone();
    timeless["id"] = json!("example-unsupported-time");
    timeless["cube:dimensions"] = json!({"x":{},"y":{}});
    collections.push(timeless);
    let mut links = Vec::new();
    for (index, collection) in collections.iter().enumerate() {
        let name = format!("collection-{index}.json");
        std::fs::write(
            root.path().join(&name),
            serde_json::to_vec(collection).unwrap(),
        )
        .unwrap();
        links.push(json!({"rel":"child","href":name}));
    }
    std::fs::write(
        root.path().join("catalog.json"),
        serde_json::to_vec(&json!({"links":links})).unwrap(),
    )
    .unwrap();
    let server = Server::start_with_catalog_and_root(&[], None, Some(root.path().to_owned())).await;
    server.app.warm_catalog();
    tokio::time::timeout(Duration::from_secs(20), async {
        while server.app.catalog_status().loaded_at.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for (id, post_status, post_body, get_body) in [
        (
            "example-unsupported-grid",
            422,
            &br#"{"detail":"Data product 'example-unsupported-grid' is analysis, not forecast"}"#[..],
            &br#"{"detail":"Data product has an unsupported spatial grid"}"#[..],
        ),
        (
            "example-unsupported-forecast-grid",
            422,
            &br#"{"detail":"Data product has an unsupported spatial grid"}"#[..],
            &br#"{"detail":"Data product has an unsupported spatial grid"}"#[..],
        ),
        (
            "example-unsupported-time",
            404,
            &br#"{"detail":"Unknown data product 'example-unsupported-time'"}"#[..],
            &br#"{"detail":"Data product has unsupported time dimensions"}"#[..],
        ),
    ] {
        let mut body = payload();
        body["queries"][0]["dataProductId"] = json!(id);
        let post = server
            .request_with_headers("POST", "/v1/forecasts", &[ORIGIN], &body)
            .await;
        assert_error(&post, post_status, post_body, None);
        let path = format!(
            "/v1/data/{id}/00000000000000000000/points/0,0?variables=temperature&initTime=2026-07-30T00:00:00Z"
        );
        let get = server
            .request_with_headers("GET", &path, &[ORIGIN], &Value::Null)
            .await;
        assert_error(&get, 422, get_body, None);
    }
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|path| path == "/catalog.json" || path.starts_with("/collection-"))
    );
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}

#[tokio::test]
async fn canonical_admission_returns_exact_busy_and_data_rate_limit_responses() {
    let server = Server::start(&[
        ("DYNAMICAL_API_COMPUTE_SLOTS", "1"),
        ("DYNAMICAL_API_RATE_LIMIT_BURST", "1"),
        ("DYNAMICAL_API_RATE_LIMIT_REQUESTS_PER_MINUTE", "60"),
    ])
    .await;
    let permit = server.app.gate.try_acquire().unwrap();
    let busy = server
        .request_with_headers("GET", CANONICAL, &[ORIGIN], &Value::Null)
        .await;
    assert_error(&busy, 503, BUSY_BODY, Some("5"));
    assert_eq!(server.app.gate.snapshot().in_use, 1);
    drop(permit);
    let limited = server
        .request_with_headers("GET", CANONICAL, &[ORIGIN], &Value::Null)
        .await;
    assert_error(&limited, 429, RATE_BODY, Some("1"));
    assert_eq!(server.app.gate.snapshot().in_use, 0);
    let permit = server.app.gate.try_acquire().unwrap();
    let query_bucket = server
        .request_with_headers("POST", "/v1/forecasts", &[ORIGIN], &payload())
        .await;
    assert_error(&query_bucket, 503, BUSY_BODY, Some("5"));
    drop(permit);
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn http_rate_keys_use_cf_addresses_only_with_origin_authentication() {
    let server = Server::start(&[
        ("DYNAMICAL_API_ORIGIN_SECRET", "fake-origin-key"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "1"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_REQUESTS_PER_MINUTE", "60"),
    ])
    .await;
    for (address, status) in [("192.0.2.1", 422), ("192.0.2.2", 422), ("192.0.2.1", 429)] {
        let reply = server
            .request_with_headers(
                "POST",
                "/v1/forecasts",
                &[ORIGIN, AUTH, ("CF-Connecting-IP", address)],
                &json!({}),
            )
            .await;
        assert_eq!(reply.status, status);
        if status == 429 {
            assert_error(&reply, 429, RATE_BODY, Some("1"));
        }
    }
    let anonymous = Server::start(&[
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "1"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_REQUESTS_PER_MINUTE", "60"),
    ])
    .await;
    for (address, worker, status) in [
        ("192.0.2.1", "one.example.test", 422),
        ("192.0.2.2", "two.example.test", 429),
    ] {
        let reply = anonymous
            .request_with_headers(
                "POST",
                "/v1/forecasts",
                &[
                    ORIGIN,
                    AUTH,
                    ("CF-Connecting-IP", address),
                    ("CF-Worker", worker),
                ],
                &json!({}),
            )
            .await;
        assert_eq!(reply.status, status);
        if status == 429 {
            assert_error(&reply, 429, RATE_BODY, Some("1"));
        }
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
    assert!(anonymous.host.reads.paths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn static_methods_and_robots_cache_headers_are_explicit_over_http() {
    let server = Server::start(&[]).await;
    for (path, allow, success, media, body) in [
        ("/", "GET, HEAD", 301, None, &b""[..]),
        (
            "/robots.txt",
            "GET, HEAD",
            200,
            Some("text/plain; charset=utf-8"),
            &include_bytes!("../robots.txt")[..],
        ),
        (
            "/healthz",
            "GET",
            200,
            Some("application/json"),
            &br#"{"status":"ok"}"#[..],
        ),
    ] {
        for method in ["GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE"] {
            let accepted = method == "GET" || (method == "HEAD" && path != "/healthz");
            let reply = server
                .request_with_headers(method, path, &[ORIGIN], &Value::Null)
                .await;
            assert_eq!(
                reply.status,
                if accepted { success } else { 405 },
                "{method} {path}"
            );
            let expected_body = if accepted { body } else { METHOD_BODY };
            assert_eq!(
                reply.body,
                if method == "HEAD" {
                    &b""[..]
                } else {
                    expected_body
                }
            );
            assert_eq!(
                reply.headers["content-length"],
                expected_body.len().to_string()
            );
            let expected_media = if accepted {
                media
            } else {
                Some("application/json")
            };
            assert_eq!(
                reply.headers.get("content-type").map(String::as_str),
                expected_media
            );
            assert_eq!(
                reply.headers.get("allow").map(String::as_str),
                (!accepted).then_some(allow)
            );
            assert_eq!(
                reply.headers.get("location").map(String::as_str),
                (accepted && path == "/").then_some("https://dynamical.org/api/")
            );
            assert_eq!(
                reply.headers.get("cache-control").map(String::as_str),
                (accepted && path == "/robots.txt").then_some("public, max-age=300")
            );
            assert_simple_cors(&reply);
        }
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn data_route_methods_plain_options_and_slash_redirects_need_no_catalog_or_compute() {
    let server = Server::start(&[]).await;
    let _permits: Vec<_> = (0..server.app.config.compute_slots)
        .map(|_| server.app.gate.try_acquire().unwrap())
        .collect();
    for (path, allowed) in [
        ("/v1/data-products", "GET"),
        ("/v1/data-products/example", "GET"),
        ("/v1/forecasts", "POST"),
        ("/v1/analyses", "POST"),
        ("/v1/forecasts/runs", "GET"),
        ("/v1/data/example/00000000000000000000/points/0,0", "GET"),
    ] {
        for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"]
            .into_iter()
            .filter(|m| *m != allowed)
        {
            let reply = server
                .request_with_headers(method, path, &[ORIGIN], &Value::Null)
                .await;
            assert_eq!(reply.status, 405, "{method} {path}");
            assert_eq!(reply.headers["allow"], allowed);
            assert_eq!(reply.headers["content-type"], "application/json");
            assert_eq!(
                reply.headers["content-length"],
                METHOD_BODY.len().to_string()
            );
            assert_eq!(
                reply.body,
                if method == "HEAD" {
                    &b""[..]
                } else {
                    METHOD_BODY
                }
            );
            assert!(!reply.headers.contains_key("cache-control"));
            assert!(!reply.headers.contains_key("etag"));
            assert_simple_cors(&reply);
        }
        let options = server
            .request_with_headers("OPTIONS", path, &[ORIGIN], &Value::Null)
            .await;
        assert_eq!(options.status, 204);
        assert!(options.body.is_empty());
        assert!(!options.headers.contains_key("content-type"));
        assert!(!options.headers.contains_key("cache-control"));
        assert!(!options.headers.contains_key("etag"));
        assert_simple_cors(&options);
        for method in ["GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE"] {
            let slash = format!("{path}///?x=%2F&x=2");
            let reply = server
                .request_with_headers(method, &slash, &[ORIGIN], &Value::Null)
                .await;
            assert_eq!(reply.status, 307, "{method} {slash}");
            assert!(reply.body.is_empty());
            assert_eq!(reply.headers["content-length"], "0");
            assert_eq!(
                reply.headers["location"],
                format!("http://example.test{path}?x=%2F&x=2")
            );
            assert!(!reply.headers.contains_key("content-type"));
            assert!(!reply.headers.contains_key("cache-control"));
            assert!(!reply.headers.contains_key("etag"));
            assert_simple_cors(&reply);
        }
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
    assert_eq!(
        server.app.gate.snapshot().in_use,
        server.app.config.compute_slots
    );
}
