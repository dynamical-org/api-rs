#![cfg(feature = "harness")]
//! Synthetic catalog failures and HTTP method invariants own expectations.
mod support;

use dynapi_catalog::{
    native::{FetchContext, FetchError, Fetcher, MonotonicClock, NativeHooks, StacCatalog},
    refresh::CatalogConfig,
};
use dynapi_server::telemetry::Stream;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;
use support::Server;

struct Unavailable {
    stalled: bool,
}
impl Fetcher for Unavailable {
    fn fetch<'a>(
        &'a self,
        _: &'a str,
        _: FetchContext,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async move {
            if self.stalled {
                std::future::pending::<()>().await;
            }
            Err(FetchError::Status(404))
        })
    }
}

const CAPTURE: &[(&str, &str)] = &[
    ("SENTRY_DSN", "https://public@errors.example.invalid/1"),
    ("POSTHOG_PROJECT_TOKEN", "fake-project-token"),
    ("DYNAMICAL_API_TIMING_LOG_SAMPLE_RATE", "0"),
];

async fn assert_catalog_errors(stalled: bool) {
    let catalog = StacCatalog::with_dependencies(
        "https://catalog.example.test/catalog.json",
        CatalogConfig {
            cold_wait_seconds: if stalled { 0.001 } else { 5.0 },
            ..CatalogConfig::default()
        },
        Arc::new(MonotonicClock::default()),
        Arc::new(Unavailable { stalled }),
        Arc::new(NativeHooks),
    )
    .unwrap();
    let expected = if stalled {
        "Timed out waiting for STAC catalog"
    } else {
        "STAC catalog refresh failed (HTTPStatusError)"
    };
    // Establish the failed state before observing HTTP. A pending fetch with a
    // short cold-wait budget deterministically exercises the timeout response.
    assert_eq!(catalog.snapshot().await.unwrap_err().to_string(), expected);
    let server = Server::start_with_catalog(CAPTURE, Some(catalog.clone())).await;
    for path in ["/v1/data-products", "/v1/data-products/example-missing"] {
        server.sink.0.lock().unwrap().clear();
        let reply = server.request("GET", path, &Value::Null).await;
        assert_eq!(reply.status, 503, "{path}");
        assert_eq!(
            reply.body,
            json!({"detail":expected}).to_string().as_bytes()
        );
        assert_eq!(reply.headers["content-type"], "application/json");
        assert_eq!(
            reply.headers["content-length"],
            reply.body.len().to_string()
        );
        for name in ["cache-control", "retry-after", "etag"] {
            assert!(!reply.headers.contains_key(name), "{path}: {name}");
        }
        let records = server.sink.0.lock().unwrap();
        let errors = records
            .iter()
            .filter(|(s, _)| *s == Stream::Error)
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1, "{path}: {records:?}");
        assert_eq!(
            errors[0].1["exception"]["type"], "CatalogUnavailable",
            "{path}"
        );
        let terminal = records
            .iter()
            .filter(|(s, _)| *s == Stream::Terminal)
            .collect::<Vec<_>>();
        assert_eq!(terminal.len(), 1, "{path}: {records:?}");
        assert_eq!(terminal[0].1["event"], "api_failure");
        assert_eq!(terminal[0].1["status"], 503);
        assert_eq!(terminal[0].1["dataProductIds"], Value::Null);
        assert_eq!(terminal[0].1["queryCount"], Value::Null);
        let analytics = records
            .iter()
            .filter(|(s, _)| *s == Stream::Analytics)
            .collect::<Vec<_>>();
        assert_eq!(analytics.len(), 1, "{path}: {records:?}");
        assert_eq!(analytics[0].1["properties"]["status"], 503);
        assert_eq!(analytics[0].1["properties"]["query_count"], Value::Null);
    }
    catalog.close();
}

#[tokio::test]
async fn catalog_metadata_failures_keep_the_catalog_error_category_and_http_contract() {
    assert_catalog_errors(false).await;
}

#[tokio::test]
async fn catalog_metadata_cold_waits_keep_the_catalog_error_category_and_http_contract() {
    assert_catalog_errors(true).await;
}

#[tokio::test]
async fn rejected_documentation_methods_have_no_matched_telemetry_route() {
    let server = Server::start(CAPTURE).await;
    for path in ["/openapi.json", "/docs", "/docs/oauth2-redirect", "/redoc"] {
        for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
            server.sink.0.lock().unwrap().clear();
            let reply = server.request(method, path, &Value::Null).await;
            assert_eq!(reply.status, 405, "{method} {path}");
            assert_eq!(reply.body, br#"{"detail":"Method Not Allowed"}"#);
            assert_eq!(reply.headers["allow"], "GET, HEAD");
            let records = server.sink.0.lock().unwrap();
            assert_eq!(records.len(), 1, "{method} {path}: {records:?}");
            assert_eq!(records[0].0, Stream::Terminal);
            assert_eq!(records[0].1["event"], "api_failure");
            assert_eq!(records[0].1["route"], "unrouted", "{method} {path}");
            assert_eq!(records[0].1["status"], 405);
        }
    }
}
