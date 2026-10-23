#![cfg(feature = "harness")]
mod support;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use support::*;

fn phases(header: &str) -> Vec<&str> {
    header
        .split(", ")
        .map(|s| s.split(';').next().unwrap())
        .collect()
}

#[tokio::test]
async fn canonical_grid_indices_use_decimal_integer_syntax() {
    let server = Server::start(&[]).await;
    let snapshot = input("forecast")["snapshot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (indices, accepted) in [
        ("0,0", true),
        ("+0,-0", true),
        ("%20%2B0%20,0", true),
        ("0_0,0", true),
        ("0.0,0", false),
        ("0,0.00", false),
        ("_0,0", false),
        ("0_,0", false),
        ("0__0,0", false),
        ("+_0,0", false),
        ("0_.0,0", false),
        ("%D9%A0,%D9%A0", true),
        ("%EF%BC%90,%EF%BC%90", true),
        ("%D9%A0_%EF%BC%90,0", true),
        ("%C2%B2,0", false),
    ] {
        let path = format!(
            "/v1/data/fixture-forecast/{snapshot}/points/{indices}?variables=count&initTime=2026-07-30T00:00:00Z"
        );
        let reply = server.request("GET", &path, &Value::Null).await;
        assert_eq!(reply.status, if accepted { 200 } else { 422 }, "{indices}");
        if accepted {
            assert!(reply.headers["cache-control"].ends_with("immutable"));
            assert_eq!(
                reply.json()["point"]["selected"]["grid"],
                json!({"latitudeIndex":0,"longitudeIndex":0})
            );
        } else {
            assert_eq!(
                reply.json(),
                json!({"detail":"Grid indices must be two integers"})
            );
        }
    }
}

#[tokio::test]
async fn forecasts_and_canonical_links_own_values_units_headers_and_usage() {
    let server = Server::start(&[("POSTHOG_PROJECT_TOKEN", "synthetic-test-token")]).await;
    let reply = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let result = reply.json()["results"][0].clone();
    assert_eq!(
        result["point"]["selected"],
        json!({"latitude":44.,"longitude":0.,"grid":{"latitudeIndex":0,"longitudeIndex":0}})
    );
    assert_eq!(result["snapshotId"], input("forecast")["snapshot_id"]);
    assert_eq!(
        result["variables"]["temperature_2m"]["unit"],
        "degree_Celsius"
    );
    let forecast = &result["forecasts"][0];
    assert_eq!(
        forecast["data"]["count"],
        json!(
            (0..5)
                .map(|lead| 9_007_199_254_740_993i64 + lead * 24)
                .collect::<Vec<_>>()
        )
    );
    assert_eq!(forecast["data"]["static"], json!([0.]));
    let temperatures = forecast["data"]["temperature_2m"].as_array().unwrap();
    assert!(temperatures[0].is_null());
    for (actual, expected) in temperatures[1..].iter().zip([
        -0.0,
        f64::from(1e-5f32),
        f64::from(f32::from_bits(1)),
        f64::from(3e38f32),
    ]) {
        assert_eq!(actual.as_f64().unwrap().to_bits(), expected.to_bits());
    }
    assert_eq!(forecast["initTime"], "2026-07-30T00:00:00Z");
    assert_eq!(forecast["leadTimeHours"], json!([0., 1., 2., 3., 4.]));
    assert_eq!(reply.headers["x-dynamical-dataset-cache"], "miss");
    assert_eq!(
        phases(&reply.headers["server-timing"]),
        [
            "catalog",
            "dataset_open",
            "point_select",
            "run_select",
            "query_select",
            "zarr_load",
            "response_build",
            "json_encode",
            "handler",
            "dataset_cache"
        ]
    );
    assert_eq!(
        reply.headers["cache-control"],
        "public, max-age=30, stale-while-revalidate=120"
    );
    let canonical = server
        .request(
            "GET",
            forecast["links"]["canonical"].as_str().unwrap(),
            &Value::Null,
        )
        .await;
    assert_eq!(
        canonical.status,
        200,
        "{}",
        String::from_utf8_lossy(&canonical.body)
    );
    assert_eq!(canonical.json()["forecasts"][0], *forecast);
    assert!(canonical.headers["cache-control"].ends_with("immutable"));
    assert!(!phases(&canonical.headers["server-timing"]).contains(&"run_select"));
    let hit = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(hit.body, reply.body);
    assert_eq!(hit.headers["etag"], reply.headers["etag"]);
    assert_eq!(hit.headers["x-dynamical-dataset-cache"], "hit");
    server.clock.0.store(30, Ordering::SeqCst);
    let revalidated = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(
        revalidated.headers["x-dynamical-dataset-cache"],
        "revalidated"
    );
    let pinned = server
        .request(
            "GET",
            forecast["links"]["canonical"].as_str().unwrap(),
            &Value::Null,
        )
        .await;
    assert_eq!(pinned.headers["x-dynamical-dataset-cache"], "hit");
    let events = server.sink.0.lock().unwrap();
    let properties = &events
        .iter()
        .find(|(stream, e)| {
            *stream == dynapi_server::telemetry::Stream::Analytics && e["event"] == "api_request"
        })
        .unwrap()
        .1["properties"];
    assert_eq!(properties["data_product_ids"], json!(["fixture-forecast"]));
    assert_eq!(properties["query_count"], 1);
    assert!(
        !properties
            .as_object()
            .unwrap()
            .contains_key("response_bytes")
    );
    assert!(!properties.as_object().unwrap().contains_key("timings_ms"));
    assert_eq!(
        properties["work_units"].as_u64().unwrap().to_string(),
        reply.headers["x-dynamical-work-units"]
    );
    let metric = &events
        .iter()
        .find(|(stream, e)| {
            *stream == dynapi_server::telemetry::Stream::Terminal && e["event"] == "api_request"
        })
        .unwrap()
        .1;
    assert_eq!(metric["dataProductIds"], json!(["fixture-forecast"]));
    assert_eq!(metric["queryCount"], 1);
    assert_eq!(metric["responseBytes"], reply.body.len());
    assert!(metric["workUnits"].is_number());
    assert!(metric["timingsMs"]["catalog"].is_number());
}

#[tokio::test]
async fn windows_runs_ensembles_and_representations_follow_fixture_recipes() {
    let server = Server::start(&[]).await;
    for name in ["forecast", "ensemble"] {
        let spec = input(name);
        for case in spec["cases"].as_array().unwrap() {
            let mut body = json!({"queries":case["queries"]});
            // Body schema accepts all three representation forms, with routing off.
            for mode in [None, Some("auto"), Some("requested")] {
                for q in body["queries"].as_array_mut().unwrap() {
                    q.as_object_mut().unwrap().remove("representation");
                    if let Some(mode) = mode {
                        q["representation"] = json!(mode);
                    }
                }
                let reply = server.request("POST", "/v1/forecasts", &body).await;
                assert_eq!(
                    reply.status,
                    200,
                    "{name} {}: {}",
                    case["name"],
                    String::from_utf8_lossy(&reply.body)
                );
                for (q, result) in body["queries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .zip(reply.json()["results"].as_array().unwrap())
                {
                    let forecasts = result["forecasts"].as_array().unwrap();
                    assert!(!forecasts.is_empty());
                    if q["initTime"].is_string() && q["initTime"] != "latest" {
                        assert_eq!(forecasts[0]["initTime"], q["initTime"]);
                    }
                    if q["initTimesAfter"].is_string() {
                        assert_eq!(forecasts.len(), 2);
                    }
                    if q["maxLeadTimeHours"] == 2 {
                        assert!(
                            forecasts[0]["leadTimeHours"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .all(|n| n.as_f64().unwrap() <= 2.)
                        );
                    }
                    for forecast in forecasts {
                        if name == "ensemble" {
                            assert!(forecast["ensembleMembers"].is_array());
                        } else {
                            assert!(forecast["data"].is_object());
                        }
                        let canonical = server
                            .request(
                                "GET",
                                forecast["links"]["canonical"].as_str().unwrap(),
                                &Value::Null,
                            )
                            .await;
                        assert_eq!(canonical.status, 200);
                        assert_eq!(canonical.json()["forecasts"][0], *forecast);
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn both_budgets_and_selection_errors_precede_loading() {
    for (key, detail) in [
        ("DYNAMICAL_API_MAX_RESPONSE_VALUES", "response-value budget"),
        ("DYNAMICAL_API_MAX_READ_VALUES", "read-value budget"),
    ] {
        let server = Server::start(&[(key, "0")]).await;
        let reply = server.request("POST", "/v1/forecasts", &payload()).await;
        assert_eq!(reply.status, 422);
        assert!(reply.json()["detail"].as_str().unwrap().contains(detail));
        assert_eq!(server.app.gate.snapshot().in_use, 0);
    }
    let server = Server::start(&[("DYNAMICAL_API_MAX_READ_VALUES", "0")]).await;
    let mut body = payload();
    body["queries"].as_array_mut().unwrap().push(json!({"dataProductId":"fixture-forecast","location":{"latitude":44,"longitude":0},"variables":["unknown"]}));
    let reply = server.request("POST", "/v1/forecasts", &body).await;
    assert_eq!(reply.status, 422);
    assert_eq!(reply.json()["detail"], "Unknown variable(s): unknown");
}

#[tokio::test]
async fn product_variable_snapshot_and_canonical_validation_errors_are_public_only() {
    let server = Server::start(&[]).await;
    for (id, expected) in [("missing", 404), ("fixture-analysis", 422)] {
        let mut body = payload();
        body["queries"][0]["dataProductId"] = json!(id);
        let reply = server.request("POST", "/v1/forecasts", &body).await;
        assert_eq!(reply.status, expected);
        if expected == 404 {
            assert_eq!(
                reply.body,
                br#"{"detail":"Unknown data product 'missing'"}"#
            );
        }
    }
    let snapshot = input("forecast")["snapshot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let base = format!("/v1/data/fixture-forecast/{snapshot}/points/0,0");
    for (suffix, kind, name) in [
        ("", "missing", "variables"),
        ("?variables=", "string_too_short", "variables"),
        (
            "?variables=count&initTime=bad",
            "datetime_from_date_parsing",
            "initTime",
        ),
        (
            "?variables=count&maxLeadTimeHours=-1",
            "greater_than_equal",
            "maxLeadTimeHours",
        ),
        (
            "?variables=count&maxLeadTimeHours=no",
            "int_parsing",
            "maxLeadTimeHours",
        ),
    ] {
        let reply = server
            .request("GET", &format!("{base}{suffix}"), &Value::Null)
            .await;
        assert_eq!(reply.status, 422);
        let error = reply.json()["detail"][0].clone();
        assert_eq!(error["type"], kind);
        assert_eq!(error["loc"], json!(["query", name]));
    }
    for (path, expected) in [
        (
            format!("{base}?variables=count"),
            "Canonical forecast queries require initTime",
        ),
        (
            format!("{base}?variables=count&initTime=2026-07-30"),
            "initTime must include a timezone",
        ),
        (
            format!("/v1/data/fixture-forecast/{snapshot}/points/a,1?variables=count"),
            "Grid indices must be two integers",
        ),
        (
            format!(
                "/v1/data/fixture-forecast/{snapshot}/points/99999999999999999999999,0?variables=count&initTime=2026-07-30T00:00:00Z"
            ),
            "latitude_index is outside the grid",
        ),
        (
            "/v1/data/fixture-forecast/00000000000000000000/points/0,0?variables=count&initTime=2026-07-30T00:00:00Z".to_owned(),
            "Unknown snapshot '00000000000000000000' for data product 'fixture-forecast'; use the snapshotId from a forecasts or analyses response",
        ),
    ] {
        let reply = server.request("GET", &path, &Value::Null).await;
        assert_eq!(
            reply.status,
            if path.contains("/00000000000000000000/") {
                404
            } else {
                422
            },
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        assert_eq!(reply.json()["detail"], expected);
    }
    for bad in [json!("bad"), Value::Null] {
        let mut body = payload();
        body["queries"][0]["representation"] = bad.clone();
        let reply = server.request("POST", "/v1/forecasts", &body).await;
        assert_eq!(reply.status, 422);
        assert_eq!(
            reply.json()["detail"][0],
            json!({"type":"literal_error","loc":["body","queries",0,"representation"],"msg":"Input should be 'auto' or 'requested'","input":bad,"ctx":{"expected":"'auto' or 'requested'"}})
        );
    }
}

#[tokio::test]
async fn analysis_canonical_and_projected_outside_use_owned_http_contracts() {
    let server = Server::start(&[]).await;
    let path = format!(
        "/v1/data/fixture-analysis/{}/points/0,0?variables=temperature_2m&startTime=2026-07-30T01:00:00Z&endTime=2026-07-30T02:00:00Z",
        input("analysis")["snapshot_id"].as_str().unwrap()
    );
    let reply = server.request("GET", &path, &Value::Null).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(
        reply.json()["analysisTimes"],
        json!(["2026-07-30T01:00:00Z"])
    );
    let mut body = payload();
    body["queries"][0]["dataProductId"] = json!("fixture-projected");
    let reply = server.request("POST", "/v1/forecasts", &body).await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json(),
        json!({"detail":"Location is outside the data product grid"})
    );
    assert!(!reply.headers.contains_key("etag"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_client_holds_compute_slot_until_the_read_finishes() {
    let server = Server::start(&[("DYNAMICAL_API_COMPUTE_SLOTS", "1")]).await;
    assert_eq!(
        server
            .request("POST", "/v1/forecasts", &payload())
            .await
            .status,
        200
    );
    server.host.reads.hold_chunks.store(true, Ordering::SeqCst);
    let socket = server.socket("POST", "/v1/forecasts", &payload()).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        server.host.reads.chunk_started.notified(),
    )
    .await
    .unwrap();
    assert_eq!(server.app.gate.snapshot().in_use, 1);
    drop(socket);
    let busy = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(busy.status, 503);
    assert_eq!(busy.headers["retry-after"], "5");
    assert!(
        busy.json()["detail"]
            .as_str()
            .unwrap()
            .starts_with("Server busy")
    );
    assert_eq!(server.app.gate.snapshot().in_use, 1);
    server.host.reads.hold_chunks.store(false, Ordering::SeqCst);
    server.host.reads.release.add_permits(100);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while server.app.gate.snapshot().in_use != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        server
            .request("POST", "/v1/forecasts", &payload())
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn typed_storage_failures_reach_http_without_internal_diagnostics() {
    let server = Server::start(&[]).await;
    for (virtual_chunk_fetch, diagnostic, expected) in [
        (
            true,
            "operation timeout (all attempts including retries) occurred after 90 seconds",
            503,
        ),
        (
            true,
            "Error { code: \"SlowDown\", message: synthetic throttle }",
            503,
        ),
        (
            false,
            "operation timeout (all attempts including retries) occurred after 90 seconds",
            500,
        ),
        (true, "synthetic permanent upstream 403", 500),
    ] {
        server.app.inject_read_failure(dynapi_reader::ReaderError {
            kind: dynapi_reader::ErrorKind::Storage,
            message: diagnostic.into(),
            virtual_chunk_fetch,
            virtual_failure: None,
            exception_class: Some("StorageError"),
        });
        let reply = server.request("POST", "/v1/forecasts", &payload()).await;
        assert_eq!(reply.status, expected);
        if expected == 503 {
            assert_eq!(reply.headers["retry-after"], "5");
            assert_eq!(
                reply.body,
                br#"{"detail":"Upstream data temporarily unavailable"}"#
            );
        } else {
            assert_eq!(reply.body, b"Internal Server Error");
        }
        assert_eq!(server.app.gate.snapshot().in_use, 0);
        assert!(!String::from_utf8_lossy(&reply.body).contains(diagnostic));
    }
}

#[tokio::test]
async fn batch_response_budget_precedes_read_budget_and_later_product_lookup() {
    let server = Server::start(&[
        ("DYNAMICAL_API_MAX_RESPONSE_VALUES", "11"),
        ("DYNAMICAL_API_MAX_READ_VALUES", "0"),
    ])
    .await;
    let mut body = payload();
    let query = body["queries"][0].clone();
    body["queries"].as_array_mut().unwrap().push(query);
    let reply = server.request("POST", "/v1/forecasts", &body).await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json()["detail"],
        "Query exceeds the 11-value response-value budget"
    );
    body["queries"][1]["dataProductId"] = json!("missing");
    let server = Server::start(&[("DYNAMICAL_API_MAX_RESPONSE_VALUES", "0")]).await;
    let reply = server.request("POST", "/v1/forecasts", &body).await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json()["detail"],
        "Query exceeds the 0-value response-value budget"
    );
}

#[tokio::test]
async fn disabled_dataset_cache_and_zero_ttl_never_report_hits() {
    for key in [
        "DYNAMICAL_API_DATASET_CACHE_ENTRIES",
        "DYNAMICAL_API_MAIN_DATASET_TTL_SECONDS",
    ] {
        let server = Server::start(&[(key, "0")]).await;
        for _ in 0..2 {
            let reply = server.request("POST", "/v1/forecasts", &payload()).await;
            assert_eq!(reply.status, 200);
            assert_eq!(reply.headers["x-dynamical-dataset-cache"], "miss");
        }
    }
}

#[tokio::test]
async fn catalog_not_loaded_is_503_and_failure_usage_retains_handler_context() {
    let server = Server::start(&[("POSTHOG_PROJECT_TOKEN", "synthetic-test-token")]).await;
    server.host.reads.fail_catalog.store(true, Ordering::SeqCst);
    let reply = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(reply.status, 503);
    assert_eq!(
        reply.json()["detail"],
        "STAC catalog refresh failed (HTTPStatusError)"
    );
    assert!(!reply.headers.contains_key("retry-after"));
    let events = server.sink.0.lock().unwrap();
    let usage = &events
        .iter()
        .find(|(stream, e)| {
            *stream == dynapi_server::telemetry::Stream::Analytics && e["event"] == "api_request"
        })
        .unwrap()
        .1["properties"];
    assert_eq!(usage["query_count"], 1);
    assert_eq!(usage["data_product_ids"], json!(["fixture-forecast"]));
    assert!(!usage.as_object().unwrap().contains_key("response_bytes"));
    assert!(!usage.as_object().unwrap().contains_key("timings_ms"));
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}

#[tokio::test]
async fn response_byte_admission_rejects_large_labels_despite_tiny_value_count() {
    let server = Server::start(&[]).await;
    server
        .host
        .reads
        .version_bytes
        .store(3 * 1024 * 1024, Ordering::SeqCst);
    let reply = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json()["detail"],
        "Query exceeds the 67108864-byte response materialization budget"
    );
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}

#[tokio::test]
async fn real_upstream_read_failure_returns_generic_500() {
    let server = Server::start(&[]).await;
    server.host.reads.fail_chunks.store(true, Ordering::SeqCst);
    let reply = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(reply.status, 500);
    assert_eq!(reply.body, b"Internal Server Error");
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.contains("/chunks/"))
    );
    assert_eq!(server.app.gate.snapshot().in_use, 0);
}

#[tokio::test]
async fn main_and_pinned_snapshots_share_the_configured_lru_capacity() {
    let server = Server::start(&[("DYNAMICAL_API_DATASET_CACHE_ENTRIES", "1")]).await;
    let first = server.request("POST", "/v1/forecasts", &payload()).await;
    let link = first.json()["results"][0]["forecasts"][0]["links"]["canonical"]
        .as_str()
        .unwrap()
        .to_owned();
    for _ in 0..2 {
        let canonical = server.request("GET", &link, &Value::Null).await;
        assert_eq!(canonical.status, 200);
        assert_eq!(canonical.headers["x-dynamical-dataset-cache"], "miss");
        let main = server.request("POST", "/v1/forecasts", &payload()).await;
        assert_eq!(main.body, first.body);
        assert_eq!(main.headers["x-dynamical-dataset-cache"], "miss");
    }
}

#[tokio::test]
async fn upstream_snapshot_failure_is_not_an_unknown_snapshot() {
    let server = Server::start(&[]).await;
    server
        .host
        .reads
        .fail_snapshots
        .store(true, Ordering::SeqCst);
    let path = format!(
        "/v1/data/fixture-forecast/{}/points/0,0?variables=count&initTime=2026-07-30T00:00:00Z",
        input("forecast")["snapshot_id"].as_str().unwrap()
    );
    let reply = server.request("GET", &path, &Value::Null).await;
    assert_eq!(
        reply.status,
        500,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.body, b"Internal Server Error");
}

#[tokio::test]
async fn blank_variables_are_rejected_before_any_store_open() {
    let server = Server::start(&[]).await;
    server.host.reads.fail_chunks.store(true, Ordering::SeqCst);
    let mut body = payload();
    body["queries"][0]["variables"] = json!([" ", "\t"]);
    let reply = server.request("POST", "/v1/forecasts", &body).await;
    assert_eq!(reply.status, 422);
    assert_eq!(reply.json()["detail"], "At least one variable is required");
    assert!(
        !server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.contains("/repository/"))
    );
}
