#![cfg(feature = "harness")]
mod support;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::atomic::Ordering};
use support::*;

fn payload() -> Value {
    json!({"queries":input("analysis")["cases"][0]["queries"]})
}
fn no_weather_reads(server: &Server) {
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|p| !p.contains("/chunks/"))
    );
}

#[tokio::test]
async fn analysis_values_half_open_windows_canonical_headers_and_cache_follow_recipe() {
    let server = Server::start(&[]).await;
    for case in input("analysis")["cases"].as_array().unwrap() {
        let post = server
            .request("POST", "/v1/analyses", &json!({"queries":case["queries"]}))
            .await;
        assert_eq!(post.status, 200, "{}", String::from_utf8_lossy(&post.body));
        assert_eq!(
            post.headers["etag"],
            format!("\"{:x}\"", Sha256::digest(&post.body))
        );
        assert_eq!(
            post.headers["cache-control"],
            "public, max-age=30, stale-while-revalidate=120"
        );
        assert!(!post.headers["server-timing"].contains("run_select"));
        for (q, r) in case["queries"]
            .as_array()
            .unwrap()
            .iter()
            .zip(post.json()["results"].as_array().unwrap())
        {
            assert_eq!(r["queryId"], q["queryId"]);
            assert_eq!(r["snapshotId"], input("analysis")["snapshot_id"]);
            assert_eq!(r["variables"]["count"]["unit"], Value::Null);
            let lat = r["point"]["selected"]["grid"]["latitudeIndex"]
                .as_i64()
                .unwrap();
            let lon = r["point"]["selected"]["grid"]["longitudeIndex"]
                .as_i64()
                .unwrap();
            let start = if q["startTime"] == "2026-07-30T00:00:00Z" {
                0
            } else {
                1
            };
            let end = if start == 0 { 5 } else { 4 };
            assert_eq!(
                r["analysisTimes"],
                json!(
                    (start..end)
                        .map(|t| format!("2026-07-30T{t:02}:00:00Z"))
                        .collect::<Vec<_>>()
                )
            );
            assert_eq!(
                r["data"]["count"],
                json!(
                    (start..end)
                        .map(|t| 9_007_199_254_740_993i64 + t * 24 + lat * 6 + lon)
                        .collect::<Vec<_>>()
                )
            );
            assert_eq!(r["data"]["static"], json!([(lat * 6 + lon) as f64]));
            if lat == 0 && lon == 0 {
                assert_eq!(r["data"]["temperature_2m"][0], Value::Null);
                assert_eq!(
                    r["data"]["temperature_2m"][1].as_f64().unwrap().to_bits(),
                    (-0.0f64).to_bits()
                );
            }
            let get = server
                .request(
                    "GET",
                    r["links"]["canonical"].as_str().unwrap(),
                    &Value::Null,
                )
                .await;
            assert_eq!(get.status, 200, "{}", String::from_utf8_lossy(&get.body));
            assert_eq!(get.json()["data"], r["data"]);
            assert_eq!(get.json()["analysisTimes"], r["analysisTimes"]);
            assert_eq!(get.json()["point"]["selected"], r["point"]["selected"]);
            assert!(get.json()["point"].get("requested").is_none());
            assert!(get.headers["cache-control"].ends_with("immutable"));
            assert_eq!(
                get.headers["etag"],
                format!("\"{:x}\"", Sha256::digest(&get.body))
            );
        }
    }
    let first = server.request("POST", "/v1/analyses", &payload()).await;
    let hit = server.request("POST", "/v1/analyses", &payload()).await;
    assert_eq!(first.body, hit.body);
    assert_eq!(first.headers["etag"], hit.headers["etag"]);
    assert_eq!(hit.headers["x-dynamical-dataset-cache"], "hit");
}

#[tokio::test]
async fn analysis_validation_and_selection_are_atomic_before_weather_loading() {
    for (key, detail) in [
        ("DYNAMICAL_API_MAX_RESPONSE_VALUES", "response-value budget"),
        ("DYNAMICAL_API_MAX_READ_VALUES", "read-value budget"),
    ] {
        let server = Server::start(&[(key, "0")]).await;
        let reply = server.request("POST", "/v1/analyses", &payload()).await;
        assert_eq!(reply.status, 422);
        assert!(reply.json()["detail"].as_str().unwrap().contains(detail));
        no_weather_reads(&server);
        assert_eq!(server.app.gate.snapshot().in_use, 0);
    }
    let server = Server::start(&[("DYNAMICAL_API_MAX_READ_VALUES", "0")]).await;
    let mut body = payload();
    let mut second = body["queries"][0].clone();
    second["variables"] = json!(["unknown"]);
    body["queries"].as_array_mut().unwrap().push(second);
    let reply = server.request("POST", "/v1/analyses", &body).await;
    assert_eq!(reply.status, 422);
    assert_eq!(reply.json()["detail"], "Unknown variable(s): unknown");
    no_weather_reads(&server);
    let server = Server::start(&[("DYNAMICAL_API_MAX_RESPONSE_VALUES", "0")]).await;
    body["queries"][1]["dataProductId"] = json!("missing");
    let reply = server.request("POST", "/v1/analyses", &body).await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json()["detail"],
        "Query exceeds the 0-value response-value budget"
    );
    no_weather_reads(&server);
}

#[tokio::test]
async fn analysis_empty_variables_precede_open_and_empty_window_has_public_error() {
    let server = Server::start(&[]).await;
    server
        .host
        .reads
        .fail_snapshots
        .store(true, Ordering::SeqCst);
    let mut body = payload();
    body["queries"][0]["variables"] = json!([" ", "\u{2003}"]);
    let reply = server.request("POST", "/v1/analyses", &body).await;
    assert_eq!(reply.status, 422);
    assert_eq!(reply.json()["detail"], "At least one variable is required");
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
    server
        .host
        .reads
        .fail_snapshots
        .store(false, Ordering::SeqCst);
    body = payload();
    body["queries"][0]["startTime"] = json!("2026-07-30T05:00:00Z");
    body["queries"][0]["endTime"] = json!("2026-07-30T06:00:00Z");
    let reply = server.request("POST", "/v1/analyses", &body).await;
    assert_eq!(reply.status, 422);
    assert!(
        reply.json()["detail"]
            .as_str()
            .unwrap()
            .contains("No analysis times")
    );
    no_weather_reads(&server);
}

#[tokio::test]
async fn runs_are_strict_newest_first_metadata_only_and_have_no_query_response_headers() {
    let server = Server::start(&[
        ("DYNAMICAL_API_MAX_RESPONSE_VALUES", "0"),
        ("DYNAMICAL_API_MAX_READ_VALUES", "0"),
    ])
    .await;
    for (suffix, times) in [
        ("", vec!["2026-07-30T06:00:00Z", "2026-07-30T00:00:00Z"]),
        ("&limit=1.0", vec!["2026-07-30T06:00:00Z"]),
        (
            "&initTimesAfter=2026-07-30T00:00:00Z",
            vec!["2026-07-30T06:00:00Z"],
        ),
        (
            "&initTimesBefore=2026-07-30T06:00:00Z",
            vec!["2026-07-30T00:00:00Z"],
        ),
        ("&initTimesAfter=2026-07-30T06:00:00Z", vec![]),
        (
            "&initTimesAfter=2026-07-30T00:00:00.000001Z&initTimesBefore=2026-07-30T06:00:00Z",
            vec![],
        ),
    ] {
        let reply = server
            .request(
                "GET",
                &format!("/v1/forecasts/runs?dataProductId=fixture-forecast{suffix}"),
                &Value::Null,
            )
            .await;
        assert_eq!(
            reply.status,
            200,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        assert_eq!(
            reply.json(),
            json!({"dataProductId":"fixture-forecast","snapshotId":input("forecast")["snapshot_id"],"runs":times.into_iter().map(|t| json!({"initTime":t})).collect::<Vec<_>>()})
        );
        assert_eq!(
            reply.headers["cache-control"],
            "public, max-age=30, stale-while-revalidate=120"
        );
        for name in [
            "etag",
            "server-timing",
            "x-dynamical-dataset-cache",
            "x-dynamical-work-units",
        ] {
            assert!(!reply.headers.contains_key(name), "{name}");
        }
    }
    no_weather_reads(&server);
}

#[tokio::test]
async fn runs_lookup_type_and_time_guards_precede_open_errors() {
    let server = Server::start(&[]).await;
    server
        .host
        .reads
        .fail_snapshots
        .store(true, Ordering::SeqCst);
    for (suffix, status, detail) in [
        (
            "dataProductId=missing&initTimesAfter=2026-07-30T00:00:00",
            404,
            "Unknown data product 'missing'",
        ),
        (
            "dataProductId=fixture-analysis&initTimesAfter=2026-07-30T00:00:00",
            422,
            "Data product 'fixture-analysis' is analysis, not forecast",
        ),
        (
            "dataProductId=fixture-forecast&initTimesAfter=2026-07-30T00:00:00&initTimesBefore=2026-07-29T00:00:00",
            422,
            "initTimesAfter must include a timezone",
        ),
        (
            "dataProductId=fixture-forecast&initTimesAfter=2026-07-30T00:00:00Z&initTimesBefore=2026-07-30T00:00:00Z",
            422,
            "initTimesAfter must be before initTimesBefore",
        ),
    ] {
        let reply = server
            .request("GET", &format!("/v1/forecasts/runs?{suffix}"), &Value::Null)
            .await;
        assert_eq!(reply.status, status);
        assert_eq!(reply.json()["detail"], detail);
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
    let reply = server
        .request(
            "GET",
            "/v1/forecasts/runs?dataProductId=fixture-forecast",
            &Value::Null,
        )
        .await;
    assert_eq!(reply.status, 500);
    assert!(!reply.headers.contains_key("retry-after"));
}

#[tokio::test]
async fn new_route_schema_checks_precede_compute_and_both_routes_release_permits() {
    let server = Server::start(&[("DYNAMICAL_API_COMPUTE_SLOTS", "1")]).await;
    let permit = server.app.gate.try_acquire().unwrap();
    let invalid = server
        .request(
            "GET",
            "/v1/forecasts/runs?dataProductId=missing&limit=0",
            &Value::Null,
        )
        .await;
    assert_eq!(invalid.status, 422);
    assert_eq!(
        invalid.json()["detail"][0]["loc"],
        json!(["query", "limit"])
    );
    let invalid = server.request("POST", "/v1/analyses", &json!({})).await;
    assert_eq!(invalid.status, 422);
    for (method, path, body) in [
        (
            "GET",
            "/v1/forecasts/runs?dataProductId=missing",
            Value::Null,
        ),
        ("POST", "/v1/analyses", payload()),
    ] {
        let busy = server.request(method, path, &body).await;
        assert_eq!(busy.status, 503);
        assert_eq!(busy.headers["retry-after"], "5");
    }
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
    drop(permit);
    for (method, path, body) in [
        (
            "GET",
            "/v1/forecasts/runs?dataProductId=fixture-forecast",
            Value::Null,
        ),
        ("POST", "/v1/analyses", payload()),
    ] {
        assert_eq!(server.request(method, path, &body).await.status, 200);
        assert_eq!(server.app.gate.snapshot().in_use, 0);
    }
}

fn copy_tree(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}
fn mount(root: &Path, name: &str) -> Value {
    let id = format!("fixture-{name}");
    copy_tree(
        &fixtures().join(name).join("repository"),
        &root.join(&id).join("repository"),
    );
    let mut collection = input(name)["collection"].clone();
    collection["id"] = json!(id);
    collection
}
fn catalog(root: &Path, collections: &[Value]) {
    for c in collections {
        std::fs::write(
            root.join(c["id"].as_str().unwrap()).join("collection.json"),
            serde_json::to_vec(c).unwrap(),
        )
        .unwrap();
    }
    std::fs::write(root.join("catalog.json"),serde_json::to_vec(&json!({"links":collections.iter().map(|c| json!({"rel":"child","href":format!("/{}/collection.json",c["id"].as_str().unwrap())})).collect::<Vec<_>>()})).unwrap()).unwrap();
}

#[tokio::test]
async fn run_discovery_bypasses_point_grid_eligibility() {
    let temp = tempfile::tempdir().unwrap();
    let mut collection = mount(temp.path(), "forecast");
    collection["cube:dimensions"]
        .as_object_mut()
        .unwrap()
        .remove("longitude");
    catalog(temp.path(), &[collection]);
    let server = Server::start_with_catalog_and_root(&[], None, Some(temp.path().to_owned())).await;
    let runs = server
        .request(
            "GET",
            "/v1/forecasts/runs?dataProductId=fixture-forecast",
            &Value::Null,
        )
        .await;
    assert_eq!(runs.status, 200, "{}", String::from_utf8_lossy(&runs.body));
    assert_eq!(runs.json()["runs"].as_array().unwrap().len(), 2);
    let point = server
        .request("POST", "/v1/forecasts", &support::payload())
        .await;
    assert_eq!(point.status, 422);
    assert_eq!(
        point.json()["detail"],
        "Data product has an unsupported spatial grid"
    );
    no_weather_reads(&server);
}

#[tokio::test]
async fn analysis_ensembles_preserve_member_identity_dimension_order_and_integer_values() {
    let temp = tempfile::tempdir().unwrap();
    catalog(temp.path(), &[mount(temp.path(), "ensemble_int16")]);
    let server = Server::start_with_catalog_and_root(&[], None, Some(temp.path().to_owned())).await;
    let body = json!({"queries":[{"dataProductId":"fixture-ensemble_int16","location":{"latitude":41,"longitude":-103},"variables":["temperature_2m","count"],"startTime":"2026-07-30T00:00:00Z","endTime":"2026-07-30T02:00:00Z"}]});
    let post = server.request("POST", "/v1/analyses", &body).await;
    assert_eq!(post.status, 200, "{}", String::from_utf8_lossy(&post.body));
    let r = &post.json()["results"][0];
    assert!(r.get("data").is_none());
    let members = r["ensembleMembers"].as_array().unwrap();
    for (m, identity) in [0, 2, 7].into_iter().enumerate() {
        assert_eq!(members[m]["member"], identity);
        assert_eq!(
            members[m]["data"]["count"],
            json!([
                9_007_199_254_740_996i64 + 100 * m as i64,
                9_007_199_254_740_999i64 + 100 * m as i64
            ])
        );
    }
    let get = server
        .request(
            "GET",
            r["links"]["canonical"].as_str().unwrap(),
            &Value::Null,
        )
        .await;
    assert_eq!(get.status, 200);
    assert_eq!(get.json()["ensembleMembers"], r["ensembleMembers"]);
}

#[tokio::test]
async fn analysis_body_cap_precedes_catalog_and_compute() {
    let server = Server::start(&[("DYNAMICAL_API_COMPUTE_SLOTS", "1")]).await;
    let _permit = server.app.gate.try_acquire().unwrap();
    let mut body = payload();
    body["queries"][0]["queryId"] = json!("x".repeat(dynapi_server::BODY_LIMIT));
    let reply = server.request("POST", "/v1/analyses", &body).await;
    assert_eq!(reply.status, 413);
    assert_eq!(
        reply.json(),
        json!({"detail":"Request body exceeds 16777216 bytes"})
    );
    assert!(server.host.reads.paths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn both_new_routes_preserve_catalog_failure_before_handler_checks() {
    for (method, path, body) in [
        ("POST", "/v1/analyses", payload()),
        (
            "GET",
            "/v1/forecasts/runs?dataProductId=fixture-forecast&initTimesAfter=2026-07-30T00:00:00",
            Value::Null,
        ),
    ] {
        let server = Server::start(&[]).await;
        server.host.reads.fail_catalog.store(true, Ordering::SeqCst);
        let reply = server.request(method, path, &body).await;
        assert_eq!(reply.status, 503);
        assert!(
            reply.json()["detail"]
                .as_str()
                .unwrap()
                .starts_with("STAC catalog")
        );
        assert!(!reply.headers.contains_key("retry-after"));
        assert_eq!(server.app.gate.snapshot().in_use, 0);
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
}

#[tokio::test]
async fn cancelled_analysis_and_runs_keep_their_permit_until_owned_store_work_finishes() {
    use std::time::Duration;
    for analysis in [true, false] {
        let server = Server::start(&[("DYNAMICAL_API_COMPUTE_SLOTS", "1")]).await;
        if analysis {
            server.host.reads.hold_chunks.store(true, Ordering::SeqCst);
        } else {
            server
                .host
                .reads
                .hold_snapshots
                .store(true, Ordering::SeqCst);
        }
        let socket = if analysis {
            server.socket("POST", "/v1/analyses", &payload()).await
        } else {
            server
                .socket(
                    "GET",
                    "/v1/forecasts/runs?dataProductId=fixture-forecast",
                    &Value::Null,
                )
                .await
        };
        let started = if analysis {
            &server.host.reads.chunk_started
        } else {
            &server.host.reads.snapshot_started
        };
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        drop(socket);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(server.app.gate.snapshot().in_use, 1);
        server.host.reads.hold_chunks.store(false, Ordering::SeqCst);
        server
            .host
            .reads
            .hold_snapshots
            .store(false, Ordering::SeqCst);
        server.host.reads.release.add_permits(16);
        tokio::time::timeout(Duration::from_secs(5), async {
            while server.app.gate.snapshot().in_use != 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !server
                .sink
                .0
                .lock()
                .unwrap()
                .iter()
                .any(
                    |(stream, record)| *stream == dynapi_server::telemetry::Stream::Terminal
                        && record["event"] == "api_request"
                        && record["status"] == 200
                )
        );
        if !analysis {
            no_weather_reads(&server);
        }
    }
}

async fn write_fixture_array(root: &Path, id: &str, name: &str, metadata: &Value) -> String {
    let storage = icechunk::new_local_filesystem_storage(&root.join(id).join("repository"))
        .await
        .unwrap();
    let repo = icechunk::Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let writer = std::sync::Arc::new(tokio::sync::RwLock::new(
        repo.writable_session("main").await.unwrap(),
    ));
    let store = icechunk::Store::from_session(writer.clone()).await;
    store
        .set(
            &format!("{name}/zarr.json"),
            serde_json::to_vec(metadata).unwrap().into(),
        )
        .await
        .unwrap();
    writer
        .write()
        .await
        .commit("Synthetic coordinate preflight")
        .execute()
        .await
        .unwrap()
        .to_string()
}
async fn virtual_fixture_chunk(root: &Path, id: &str, key: &str, url: &str) {
    let storage = icechunk::new_local_filesystem_storage(&root.join(id).join("repository"))
        .await
        .unwrap();
    let repo = icechunk::Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let writer = std::sync::Arc::new(tokio::sync::RwLock::new(
        repo.writable_session("main").await.unwrap(),
    ));
    let store = icechunk::Store::from_session(writer.clone()).await;
    store
        .set_virtual_ref(
            key,
            icechunk::format::manifest::VirtualChunkRef {
                location: icechunk::format::manifest::VirtualChunkLocation::from_url(url).unwrap(),
                offset: 0,
                length: 16,
                checksum: None,
            },
            false,
        )
        .await
        .unwrap();
    writer
        .write()
        .await
        .commit("Synthetic unavailable virtual payload")
        .execute()
        .await
        .unwrap();
}

#[tokio::test]
async fn metadata_limit_has_safe_http_422_on_analysis_forecast_runs_and_canonical() {
    let temp = tempfile::tempdir().unwrap();
    let analysis = mount(temp.path(), "analysis");
    let forecast = mount(temp.path(), "forecast");
    catalog(temp.path(), &[analysis, forecast]);
    // This small metadata declares an eager coordinate whose preflight charge
    // exceeds 64 MiB. No large vector or coordinate payload is constructed.
    let metadata = json!({"zarr_format":3,"node_type":"array","shape":[500000],"data_type":"int64","dimension_names":["ensemble_member"],"chunk_grid":{"name":"regular","configuration":{"chunk_shape":[1024]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,"codecs":[{"name":"bytes","configuration":{"endian":"little"}},{"name":"blosc","configuration":{"cname":"zstd","clevel":3,"shuffle":"shuffle","typesize":8,"blocksize":0}}],"attributes":{},"storage_transformers":[]});
    write_fixture_array(
        temp.path(),
        "fixture-analysis",
        "ensemble_member",
        &metadata,
    )
    .await;
    let snapshot = write_fixture_array(
        temp.path(),
        "fixture-forecast",
        "ensemble_member",
        &metadata,
    )
    .await;
    let server = Server::start_with_catalog_and_root(&[], None, Some(temp.path().to_owned())).await;
    for (method, path, body) in [
        ("POST", "/v1/analyses".into(), payload()),
        ("POST", "/v1/forecasts".into(), support::payload()),
        (
            "GET",
            "/v1/forecasts/runs?dataProductId=fixture-forecast".into(),
            Value::Null,
        ),
        (
            "GET",
            format!(
                "/v1/data/fixture-forecast/{snapshot}/points/0,0?variables=count&initTime=2026-07-30T00:00:00Z"
            ),
            Value::Null,
        ),
    ] {
        let reply = server.request(method, &path, &body).await;
        assert_eq!(
            reply.status,
            422,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        assert_eq!(
            reply.json(),
            json!({"detail":"Opened dataset metadata exceeds 67108864 bytes"})
        );
        assert!(!reply.headers.contains_key("etag"));
        assert!(!reply.headers.contains_key("retry-after"));
    }
    no_weather_reads(&server);
}

#[tokio::test]
async fn virtual_analysis_ids_fail_before_repository_io_with_existing_generic_500() {
    let temp = tempfile::tempdir().unwrap();
    let mut collections = Vec::new();
    for id in [
        "noaa-gfs-analysis-virtual",
        "noaa-gefs-analysis-0-25-degree-virtual",
        "noaa-hrrr-analysis-virtual",
    ] {
        std::fs::create_dir(temp.path().join(id)).unwrap();
        let mut collection = input("analysis")["collection"].clone();
        collection["id"] = json!(id);
        collection["assets"]["icechunk"]["icechunk:virtual_chunk_containers"] = json!([{"url_prefix":"s3://synthetic.example.test/chunks/","credentials":{"type":"s3","anonymous":true}}]);
        collections.push(collection);
    }
    catalog(temp.path(), &collections);
    let server = Server::start_with_catalog_and_root(&[], None, Some(temp.path().to_owned())).await;
    for collection in collections {
        let mut body = payload();
        body["queries"][0]["dataProductId"] = collection["id"].clone();
        let reply = server.request("POST", "/v1/analyses", &body).await;
        assert_eq!(reply.status, 500);
        assert_eq!(reply.body, b"Internal Server Error");
        assert_eq!(reply.headers["content-type"], "text/plain; charset=utf-8");
        assert!(!reply.headers.contains_key("etag"));
        assert!(!reply.headers.contains_key("retry-after"));
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
async fn virtual_forecast_runs_use_only_stored_coordinates_without_virtual_authorization() {
    let temp = tempfile::tempdir().unwrap();
    let mut collection = mount(temp.path(), "forecast");
    let id = "noaa-gfs-forecast-virtual";
    std::fs::rename(temp.path().join("fixture-forecast"), temp.path().join(id)).unwrap();
    collection["id"] = json!(id);
    collection["assets"]["icechunk"]["icechunk:virtual_chunk_containers"] = json!([{"url_prefix":"s3://synthetic.example.test/chunks/","credentials":{"type":"s3","anonymous":true}}]);
    catalog(temp.path(), &[collection.clone()]);
    virtual_fixture_chunk(
        temp.path(),
        id,
        "count/c/0/0/0/0",
        "s3://synthetic.example.test/chunks/weather",
    )
    .await;
    let server = Server::start_with_catalog_and_root(&[], None, Some(temp.path().to_owned())).await;
    let path = format!("/v1/forecasts/runs?dataProductId={id}");
    let reply = server.request("GET", &path, &Value::Null).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.json()["runs"].as_array().unwrap().len(), 2);
    assert!(!reply.headers.contains_key("etag"));
    assert_eq!(
        reply.headers["cache-control"],
        "public, max-age=30, stale-while-revalidate=120"
    );
    let mut body = support::payload();
    body["queries"][0]["dataProductId"] = json!(id);
    assert_eq!(
        server.request("POST", "/v1/forecasts", &body).await.status,
        500
    );
    // A virtual coordinate must remain unauthorized even when the product has
    // HTTP containers. The local URL is outside the declaration as well, so a
    // regression cannot send test traffic to an external service.
    let prefix = format!("{}/forbidden/", server.host.url);
    collection["assets"]["icechunk"]["icechunk:virtual_chunk_containers"] = json!([{"url_prefix":"https://chunks.dynamical.org/forbidden/","credentials":{"type":"http"}}]);
    catalog(temp.path(), &[collection]);
    virtual_fixture_chunk(
        temp.path(),
        id,
        "init_time/c/0",
        &format!("{prefix}coordinate"),
    )
    .await;
    let fresh = Server::start_with_catalog_and_root(&[], None, Some(temp.path().to_owned())).await;
    let refused = fresh.request("GET", &path, &Value::Null).await;
    assert_eq!(refused.status, 500);
    assert!(!refused.headers.contains_key("etag"));
    assert!(!refused.headers.contains_key("retry-after"));
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|p| !p.contains("/forbidden/"))
    );
    assert!(
        fresh
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|p| !p.contains("/forbidden/"))
    );
}
