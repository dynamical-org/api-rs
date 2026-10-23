#![cfg(feature = "harness")]
#[path = "support/geo_proxy.rs"]
mod proxy;
mod support;
use dynapi_policy::Permit;
use dynapi_server::{config::Config, telemetry::Stream};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::atomic::Ordering, time::Duration};
use support::*;
use tokio::io::AsyncWriteExt;

const FAIR: &str = "Too many concurrent requests from this caller while the server is busy; reduce concurrency or retry later, and contact feedback@dynamical.org if you need higher limits";
const ORIGIN: &str = "fixture-origin";
const A: &str = "203.0.113.9";
fn headers() -> [(&'static str, &'static str); 3] {
    [
        ("X-Dynamical-Origin-Auth", ORIGIN),
        ("CF-Connecting-IP", A),
        ("Origin", "https://example.test"),
    ]
}
async fn server(extra: &[(&str, &str)]) -> Server {
    let mut values = vec![
        ("DYNAMICAL_API_ORIGIN_SECRET", ORIGIN),
        ("DYNAMICAL_API_FAIR_SHARE_RETRY_AFTER_SECONDS", "7"),
    ];
    values.extend_from_slice(extra);
    Server::start(&values).await
}
fn hold(server: &Server, others: usize, caller: usize) -> Vec<Permit> {
    let mut held: Vec<_> = (0..caller)
        .map(|_| server.app.gate.acquire(Some(A)).unwrap())
        .collect();
    held.extend((0..others).map(|i| {
        server
            .app
            .gate
            .acquire(Some(&format!("other-{i}")))
            .unwrap()
    }));
    held
}
fn routes() -> Vec<(&'static str, String, Value)> {
    vec![
        ("POST", "/v1/forecasts".into(), payload()),
        (
            "POST",
            "/v1/analyses".into(),
            json!({"queries":input("analysis")["cases"][0]["queries"]}),
        ),
        (
            "GET",
            "/v1/forecasts/runs?dataProductId=fixture-forecast".into(),
            Value::Null,
        ),
        (
            "GET",
            format!(
                "/v1/data/fixture-forecast/{}/points/0,0?variables=count&initTime=2026-07-30T00:00:00Z",
                input("forecast")["snapshot_id"].as_str().unwrap()
            ),
            Value::Null,
        ),
    ]
}
fn fair(reply: &Reply) {
    assert_eq!(
        reply.status,
        429,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.json(), json!({"detail":FAIR}));
    assert_eq!(reply.headers["retry-after"], "7");
    assert_eq!(reply.headers["access-control-allow-origin"], "*");
    assert!(reply.headers["access-control-expose-headers"].contains("Retry-After"));
    assert!(reply.headers.contains_key("x-request-id"));
    assert!(!reply.headers.contains_key("cache-control"));
}
#[tokio::test]
async fn boundary_table_over_http() {
    for (other, caller, status) in [
        (3, 0, 404),
        (2, 1, 404),
        (3, 1, 404),
        (2, 2, 429),
        (4, 2, 429),
        (6, 0, 503),
        (5, 1, 503),
        (0, 3, 404),
        (0, 4, 429),
    ] {
        let server = server(&[]).await;
        let held = hold(&server, other, caller);
        let reply = server
            .request_with_headers(
                "GET",
                "/v1/forecasts/runs?dataProductId=missing",
                &headers(),
                &Value::Null,
            )
            .await;
        assert_eq!(
            reply.status,
            status,
            "other={other},caller={caller}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        if status == 429 {
            fair(&reply);
        }
        assert_eq!(server.app.gate.snapshot().in_use, other + caller);
        drop(held);
        assert_eq!(server.app.gate.snapshot().max_caller_slots, 0);
    }
}
#[tokio::test]
async fn all_four_routes_return_fair_share_before_catalog_and_geo_and_emit_ordinary_failures() {
    let server = server(&[("SENTRY_DSN", "https://fake@example.test/1")]).await;
    server.host.reads.gated.store(true, Ordering::SeqCst);
    let held = hold(&server, 2, 2);
    for (method, path, body) in routes() {
        fair(
            &server
                .request_with_headers(method, &path, &headers(), &body)
                .await,
        );
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
    {
        let events = server.sink.0.lock().unwrap();
        let failures: Vec<_> = events
            .iter()
            .filter(|(_, e)| e["event"] == "api_failure")
            .collect();
        assert_eq!(failures.len(), 4);
        assert!(failures.iter().all(|(_, e)| e["status"] == 429));
        assert!(events.iter().all(|(stream, _)| *stream != Stream::Error));
    }
    drop(held);
    // With admission available the same gated product reaches its location policy.
    let reply = server
        .request_with_headers("POST", "/v1/forecasts", &headers(), &payload())
        .await;
    assert_eq!(reply.status, 503);
    assert_ne!(reply.json(), json!({"detail":FAIR}));
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}
#[tokio::test]
async fn preflight_auth_routing_schema_and_rate_precede_fair_share() {
    let server = server(&[("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "100")]).await;
    let held = hold(&server, 4, 2);
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
    assert_eq!(
        server
            .request("POST", "/v1/forecasts", &payload())
            .await
            .status,
        403
    );
    for (method, path, body, status) in [
        ("GET", "/missing", Value::Null, 404),
        ("GET", "/v1/forecasts", Value::Null, 405),
        ("POST", "/v1/forecasts", json!({"queries":"invalid"}), 422),
        ("POST", "/v1/analyses", json!({"queries":"invalid"}), 422),
        ("GET", "/v1/forecasts/runs", Value::Null, 422),
        (
            "GET",
            "/v1/data/fixture-forecast/snapshot/points/0,0",
            Value::Null,
            422,
        ),
    ] {
        assert_eq!(
            server
                .request_with_headers(method, path, &headers(), &body)
                .await
                .status,
            status,
            "{path}"
        );
    }
    assert_eq!(server.app.gate.snapshot().fair_share_refusals, 0);
    assert_eq!(server.app.gate.snapshot().capacity_refusals, 0);
    drop(held);
    let server = self::server(&[
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "1"),
        ("DYNAMICAL_API_QUERY_RATE_LIMIT_REQUESTS_PER_MINUTE", "1"),
    ])
    .await;
    let _held = hold(&server, 4, 2);
    fair(
        &server
            .request_with_headers("POST", "/v1/forecasts", &headers(), &payload())
            .await,
    );
    let rate = server
        .request_with_headers("POST", "/v1/forecasts", &headers(), &payload())
        .await;
    assert_eq!(rate.status, 429);
    assert_eq!(rate.headers["retry-after"], "60");
    assert_eq!(
        rate.json(),
        json!({"detail":"Request rate limit exceeded; contact feedback@dynamical.org if you need higher limits"})
    );
    assert_eq!(server.app.gate.snapshot().fair_share_refusals, 1);
}
#[tokio::test]
async fn worker_identity_and_unidentified_fallbacks_share_the_rate_key() {
    let server = server(&[]).await;
    let mut held = hold(&server, 0, 4);
    let mut worker = headers().to_vec();
    worker.push(("CF-Worker", "w.example"));
    assert_eq!(
        server
            .request_with_headers(
                "GET",
                "/v1/forecasts/runs?dataProductId=missing",
                &worker,
                &Value::Null
            )
            .await
            .status,
        404
    );
    let no_ip = [("X-Dynamical-Origin-Auth", ORIGIN)];
    let empty_ip = [
        ("X-Dynamical-Origin-Auth", ORIGIN),
        ("CF-Connecting-IP", ""),
    ];
    for h in [&no_ip[..], &empty_ip[..]] {
        assert_eq!(
            server
                .request_with_headers(
                    "GET",
                    "/v1/forecasts/runs?dataProductId=missing",
                    h,
                    &Value::Null
                )
                .await
                .status,
            404
        );
    }
    held.extend((0..2).map(|_| server.app.gate.acquire(None).unwrap()));
    for h in [&no_ip[..], &empty_ip[..]] {
        assert_eq!(
            server
                .request_with_headers(
                    "GET",
                    "/v1/forecasts/runs?dataProductId=missing",
                    h,
                    &Value::Null
                )
                .await
                .status,
            503
        );
    }
    assert_eq!(server.app.gate.snapshot().max_caller_slots, 4);
    assert_eq!(server.app.gate.snapshot().fair_share_refusals, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_route_holds_its_caller_after_disconnect_until_physical_work_finishes() {
    for (method, path, body) in routes() {
        let server = server(&[]).await;
        server
            .host
            .reads
            .hold_snapshots
            .store(true, Ordering::SeqCst);
        let socket = server
            .socket_with_headers(method, &path, &headers(), &body)
            .await;
        tokio::time::timeout(
            Duration::from_secs(20),
            server.host.reads.snapshot_started.notified(),
        )
        .await
        .unwrap();
        assert_eq!(server.app.gate.snapshot().max_caller_slots, 1, "{path}");
        let held = hold(&server, 2, 1);
        drop(socket);
        // A request on a different route sees the detached worker's caller count.
        fair(
            &server
                .request_with_headers("POST", "/v1/forecasts", &headers(), &payload())
                .await,
        );
        assert_eq!(server.app.gate.snapshot().in_use, 4);
        server
            .host
            .reads
            .hold_snapshots
            .store(false, Ordering::SeqCst);
        server.host.reads.release.add_permits(100);
        tokio::time::timeout(Duration::from_secs(20), async {
            while server.app.gate.snapshot().in_use != 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(server.app.gate.snapshot().max_caller_slots, 1);
        drop(held);
        assert_eq!(server.app.gate.snapshot().in_use, 0);
        assert_eq!(server.app.gate.snapshot().max_caller_slots, 0);
    }
}
#[tokio::test]
async fn cancellation_before_admission_takes_no_caller_slot() {
    let server = server(&[]).await;
    let mut socket = tokio::net::TcpStream::connect(server.address)
        .await
        .unwrap();
    socket.write_all(format!("POST /v1/forecasts HTTP/1.1\r\nHost: example.test\r\nContent-Length: 100\r\nX-Dynamical-Origin-Auth: {ORIGIN}\r\nCF-Connecting-IP: {A}\r\n\r\n{{").as_bytes()).await.unwrap();
    drop(socket);
    let reply = server
        .request_with_headers(
            "GET",
            "/v1/forecasts/runs?dataProductId=missing",
            &headers(),
            &Value::Null,
        )
        .await;
    assert_eq!(reply.status, 404);
    assert_eq!(server.app.gate.snapshot().in_use, 0);
    assert_eq!(server.app.gate.snapshot().max_caller_slots, 0);
}
#[test]
fn environment_thresholds_are_validated_even_when_off_and_empty_thresholds_are_unset() {
    let config = |values: &[(&str, &str)]| {
        Config::from_values(
            &values
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        )
    };
    let defaults = config(&[]).unwrap().fair_share;
    assert_eq!(
        (
            defaults.enabled,
            defaults.busy_slots,
            defaults.caller_slots,
            defaults.retry_after
        ),
        (true, 4, 2, 5)
    );
    for key in [
        "DYNAMICAL_API_FAIR_SHARE_BUSY_SLOTS",
        "DYNAMICAL_API_FAIR_SHARE_CALLER_SLOTS",
    ] {
        assert_eq!(config(&[(key, "")]).unwrap().fair_share, defaults);
        for invalid in ["0", "-1", "invalid", "1.5", " "] {
            assert!(config(&[(key, invalid)]).is_err(), "{key}={invalid}");
        }
    }
    for invalid in ["0", "", "bad"] {
        assert!(config(&[("DYNAMICAL_API_FAIR_SHARE_RETRY_AFTER_SECONDS", invalid)]).is_err());
    }
    assert!(config(&[("DYNAMICAL_API_FAIR_SHARE_BUSY_SLOTS", "7")]).is_err());
    assert!(
        config(&[
            ("DYNAMICAL_API_FAIR_SHARE", "0"),
            ("DYNAMICAL_API_FAIR_SHARE_BUSY_SLOTS", "0")
        ])
        .is_err()
    );
    assert!(
        !config(&[("DYNAMICAL_API_FAIR_SHARE", "0")])
            .unwrap()
            .fair_share
            .enabled
    );
    assert!(config(&[("DYNAMICAL_API_FAIR_SHARE_CALLER_SLOTS", "8")]).is_ok());
}

#[tokio::test]
async fn fair_share_precedes_location_denial_and_shared_heartbeat_reports_decisions() {
    let proxy = proxy::Proxy::start().await;
    let server = server(&[
        ("DYNAPI_HARNESS_GEO_CHECK_URL", proxy.url.as_str()),
        ("DYNAMICAL_API_GEO_CHECK_SECRET", proxy::SECRET),
    ])
    .await;
    server.host.reads.gated.store(true, Ordering::SeqCst);
    let held = hold(&server, 2, 2);
    fair(
        &server
            .request_with_headers("POST", "/v1/forecasts", &headers(), &payload())
            .await,
    );
    assert_eq!(proxy.count(), 0);
    let heartbeat = server
        .app
        .heartbeat
        .record("fixture", server.app.gate.snapshot());
    assert_eq!(heartbeat["maxCallerSlots"], 2);
    assert_eq!(heartbeat["fairShareRefusals"], 1);
    assert_eq!(heartbeat["capacityRefusals"], 0);
    assert_eq!(heartbeat["fairShareWouldRefuseConservative"], 0);
    drop(held);
    let denied = server
        .request_with_headers("POST", "/v1/forecasts", &headers(), &payload())
        .await;
    assert_eq!(denied.status, 451);
    assert_eq!(proxy.count(), 1);
    let heartbeat = server
        .app
        .heartbeat
        .record("fixture", server.app.gate.snapshot());
    assert_eq!(heartbeat["maxCallerSlots"], 0);
    assert_eq!(heartbeat["fairShareRefusals"], 1);
}
#[tokio::test]
async fn off_switch_and_inert_small_capacity_restore_capacity_only_http() {
    for extra in [
        vec![("DYNAMICAL_API_FAIR_SHARE", "0")],
        vec![("DYNAMICAL_API_COMPUTE_SLOTS", "3")],
    ] {
        let server = server(&extra).await;
        let capacity = server.app.gate.snapshot().capacity;
        let held = hold(&server, 0, capacity);
        let reply = server
            .request_with_headers(
                "GET",
                "/v1/forecasts/runs?dataProductId=missing",
                &headers(),
                &Value::Null,
            )
            .await;
        assert_eq!(reply.status, 503);
        assert!(
            reply.json()["detail"]
                .as_str()
                .unwrap()
                .starts_with("Server busy")
        );
        assert_eq!(server.app.gate.snapshot().max_caller_slots, capacity);
        assert_eq!(server.app.gate.snapshot().fair_share_refusals, 0);
        drop(held);
    }
}
