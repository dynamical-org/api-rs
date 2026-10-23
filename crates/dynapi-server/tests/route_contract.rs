#![cfg(feature = "harness")]
//! The committed origin manifest is input metadata, not captured HTTP answers.
mod support;
use serde_json::{Value, json};
use std::time::Duration;
use support::{Reply, Server};

fn manifest() -> Value {
    serde_json::from_str(include_str!("../../../docs/route-contract.v1.json")).unwrap()
}
fn sample(template: &str) -> String {
    template
        .replace("{dataProductId}", "example-missing-product")
        .replace("{snapshotId}", "00000000000000000000")
        .replace("{gridIndices}", "0,0")
}
fn number(value: &Value) -> u16 {
    value.as_u64().unwrap().try_into().unwrap()
}
fn methods(route: &Value) -> Vec<&str> {
    route["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect()
}
fn assert_method_rejection(reply: &Reply, method: &str, allowed: &[&str], status: u16) {
    assert_eq!(reply.status, status);
    assert_eq!(reply.headers["allow"], allowed.join(", "));
    if method == "HEAD" {
        assert!(reply.body.is_empty());
    } else {
        assert_eq!(reply.json(), json!({"detail":"Method Not Allowed"}));
    }
}

#[tokio::test]
async fn every_manifest_operation_dispatches_to_a_real_handler() {
    let server = Server::start(&[]).await;
    server.app.warm_catalog();
    tokio::time::timeout(Duration::from_secs(20), async {
        while server.app.catalog_status().loaded_at.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let document = manifest();
    for route in document["routes"].as_array().unwrap() {
        let template = route["template"].as_str().unwrap();
        let mut path = sample(template);
        if template.starts_with("/v1/data/") {
            path.push_str("?variables=count&initTime=2026-07-30T00:00:00Z");
        }
        for method in methods(route) {
            let reply = server.request(method, &path, &json!({})).await;
            let status = match route["intent"].as_str().unwrap() {
                "redirect" => number(&route["directRedirectStatus"]),
                "static" | "health" | "documentation" => 200,
                "catalog" if template == "/v1/data-products" => 200,
                "catalog" => 404,
                "data" if template.starts_with("/v1/data/") => 404,
                "data" => 422,
                other => panic!("missing dispatch fixture for intent {other}"),
            };
            assert_eq!(
                reply.status,
                status,
                "{method} {path}: {}",
                String::from_utf8_lossy(&reply.body)
            );
            if method == "HEAD" {
                assert!(reply.body.is_empty());
            } else if status == 404 {
                assert_eq!(
                    reply.json(),
                    json!({"detail":"Unknown data product 'example-missing-product'"})
                );
            } else if status == 422 {
                assert!(reply.json()["detail"].is_array());
            } else if route["intent"] == "catalog" {
                assert!(reply.json()["dataProducts"].is_array());
            } else if route["intent"] == "health" {
                assert_eq!(reply.json(), json!({"status":"ok"}));
            }
        }
    }
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|p| p == "/catalog.json" || p.ends_with("/collection.json"))
    );
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}

#[tokio::test]
async fn manifest_method_head_options_and_slash_profiles_match_http() {
    let server = Server::start(&[
        ("DYNAMICAL_API_PUBLIC_SCHEME", "https"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "1000"),
    ])
    .await;
    let _held: Vec<_> = (0..server.app.config.compute_slots)
        .map(|_| server.app.gate.try_acquire().unwrap())
        .collect();
    let document = manifest();
    assert_eq!(
        document["methodBehavior"]["headBody"],
        "suppressed_including_errors"
    );
    for route in document["routes"].as_array().unwrap() {
        let path = sample(route["template"].as_str().unwrap());
        let allowed = methods(route);
        for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"]
            .into_iter()
            .filter(|m| !allowed.contains(m))
        {
            let reply = server.request(method, &path, &Value::Null).await;
            assert_method_rejection(
                &reply,
                method,
                &allowed,
                number(&document["methodBehavior"]["unsupportedStatus"]),
            );
        }
        let head = server.request("HEAD", &path, &Value::Null).await;
        assert!(head.body.is_empty());
        assert_eq!(route["head"]["body"], "suppressed");
        assert_eq!(route["head"]["registered"], allowed.contains(&"HEAD"));
        if route["head"]["registered"] == true {
            assert_eq!(
                head.status,
                if route["intent"] == "redirect" {
                    number(&route["directRedirectStatus"])
                } else {
                    200
                }
            );
        } else {
            assert_method_rejection(
                &head,
                "HEAD",
                &allowed,
                number(&document["methodBehavior"]["unsupportedStatus"]),
            );
        }
        let options = server.request("OPTIONS", &path, &Value::Null).await;
        assert_eq!(
            options.status,
            number(&route["plainOptionsStatus"]),
            "OPTIONS {path}"
        );
        if options.status == 204 {
            assert!(options.body.is_empty());
        } else {
            assert_method_rejection(
                &options,
                "OPTIONS",
                &allowed,
                number(&route["plainOptionsStatus"]),
            );
        }
        if route["slashAlternativeRedirect"] == true {
            for method in ["GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE"] {
                let reply = server
                    .request_with_headers(
                        method,
                        &format!("{path}///?a=%2F&a=2"),
                        &[
                            ("X-Forwarded-Proto", "http"),
                            ("X-Forwarded-Host", "other.example.test"),
                            ("Forwarded", "proto=http;host=other.example.test"),
                        ],
                        &Value::Null,
                    )
                    .await;
                assert_eq!(
                    reply.status,
                    number(&document["pathBehavior"]["slashRedirect"]["status"]),
                    "{method} {path}"
                );
                assert_eq!(
                    reply.headers["location"],
                    format!("https://example.test{path}?a=%2F&a=2")
                );
                assert!(reply.body.is_empty());
            }
        } else {
            let reply = server.request("GET", "//", &Value::Null).await;
            assert_eq!(
                reply.status,
                number(&document["pathBehavior"]["unmatchedStatus"])
            );
            assert_eq!(reply.json(), json!({"detail":"Not Found"}));
        }
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn manifest_preflight_and_decoded_path_profiles_match_http() {
    let server = Server::start(&[("DYNAMICAL_API_ORIGIN_SECRET", "fake-contract-key")]).await;
    let _held: Vec<_> = (0..server.app.config.compute_slots)
        .map(|_| server.app.gate.try_acquire().unwrap())
        .collect();
    let document = manifest();
    let profile = &document["preflight"];
    assert_eq!(profile["scope"], "all_paths");
    assert_eq!(
        profile["requiresHeaders"],
        json!(["Origin", "Access-Control-Request-Method"])
    );
    assert_eq!(profile["beforeOriginAuthentication"], true);
    assert_eq!(profile["beforeRouting"], true);
    let mut paths: Vec<_> = document["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| sample(r["template"].as_str().unwrap()))
        .collect();
    paths.extend([
        "/example-missing-route".into(),
        "/v1/data-products/fixture/extra".into(),
        "/docs/".into(),
    ]);
    for path in paths {
        for (requested, status, body) in [
            ("GET", number(&profile["successStatus"]), "OK"),
            (
                "DELETE",
                number(&profile["rejectionStatus"]),
                "Disallowed CORS method",
            ),
        ] {
            let reply = server
                .request_with_headers(
                    profile["method"].as_str().unwrap(),
                    &path,
                    &[
                        ("Origin", "https://client.example.test"),
                        ("Access-Control-Request-Method", requested),
                    ],
                    &Value::Null,
                )
                .await;
            assert_eq!(reply.status, status, "{path}");
            assert_eq!(reply.body, body.as_bytes());
        }
    }
    assert!(server.sink.0.lock().unwrap().is_empty());
    for headers in [
        vec![("Origin", "https://client.example.test")],
        vec![("Access-Control-Request-Method", "GET")],
    ] {
        assert_eq!(
            server
                .request_with_headers("OPTIONS", "/healthz", &headers, &Value::Null)
                .await
                .status,
            403
        );
    }
    assert_eq!(
        document["pathBehavior"]["percentDecoding"],
        "once_utf8_lossy"
    );
    assert_eq!(
        document["pathBehavior"]["parameters"],
        "one_nonempty_decoded_segment"
    );
    assert_eq!(
        document["pathBehavior"]["staticTerminalLf"],
        "one_tolerated"
    );
    for (method, path, status) in [
        ("GET", "/healthz%0A", 200),
        ("GET", "/healthz%250A", 404),
        ("HEAD", "/v1/data-products/name%252Fextra", 405),
        ("HEAD", "/v1/data-products/name%2Fextra", 404),
        ("HEAD", "/v1/data-products/%FF", 405),
        ("HEAD", "/v1/data-products//fixture", 404),
        ("HEAD", "/v1/data-products/fixture/extra", 404),
    ] {
        let reply = server
            .request_with_headers(
                method,
                path,
                &[("X-Dynamical-Origin-Auth", "fake-contract-key")],
                &Value::Null,
            )
            .await;
        assert_eq!(reply.status, status, "{method} {path}");
        if method == "HEAD" {
            assert!(reply.body.is_empty());
        } else if status == 404 {
            assert_eq!(reply.json(), json!({"detail":"Not Found"}));
        }
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
}
