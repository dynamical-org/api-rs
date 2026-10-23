#![cfg(feature = "harness")]
mod support;
use dynapi_server::telemetry::Stream;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use support::*;

fn heartbeat(server: &Server) -> Value {
    server
        .app
        .heartbeat
        .record("fake-container", server.app.gate.snapshot())
}

#[tokio::test]
async fn response_budget_failure_records_nominal_read_values_without_loading() {
    let server = Server::start(&[("DYNAMICAL_API_MAX_RESPONSE_VALUES", "0")]).await;
    let reply = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(reply.status, 422);
    let records = server.sink.0.lock().unwrap();
    let failure = &records
        .iter()
        .find(|(stream, record)| *stream == Stream::Terminal && record["event"] == "api_failure")
        .unwrap()
        .1;
    // Two 1x2x2x3 chunked variables touch three lead chunks (36 cells
    // each), and the static 2x3 variable touches six cells: 36+36+6=78.
    assert_eq!(failure["readValues"], 78);
    assert_eq!(failure["queryCount"], 1);
    assert_eq!(failure["dataProductIds"], json!(["fixture-forecast"]));
    assert!(failure["timingsMs"]["query_select"].is_number());
    assert_eq!(failure["counters"]["open:miss"], 1);
    assert!(failure["counters"].get("load:product").is_none());
    drop(records);
    let snapshot = heartbeat(&server);
    assert_eq!(snapshot["productsLoaded"], 0);
    assert_eq!(snapshot["loads"], json!({}));
    assert_eq!(snapshot["opens"]["fixture-forecast"]["miss"]["count"], 1);
}

#[tokio::test]
async fn dataset_observations_feed_success_failures_and_cumulative_heartbeats() {
    let server = Server::start(&[]).await;
    for (class, open) in [("product", "miss"), ("warm", "hit")] {
        server.sink.0.lock().unwrap().clear();
        let reply = server.request("POST", "/v1/forecasts", &payload()).await;
        assert_eq!(reply.status, 200);
        let records = server.sink.0.lock().unwrap();
        let success = &records
            .iter()
            .find(|(stream, record)| {
                *stream == Stream::Terminal && record["event"] == "api_request"
            })
            .unwrap()
            .1;
        assert_eq!(success["readValues"], 78);
        assert_eq!(success["counters"][format!("open:{open}")], 1);
        assert_eq!(success["counters"][format!("load:{class}")], 1);
        assert_eq!(success["counters"]["selections"], 1);
        assert_eq!(success["counters"]["readGroups"], 1);
        assert_eq!(success["counters"]["groupedReadValues"], 78);
        assert_eq!(success["counters"]["coTiledQueries"], 0);
        for duration in success["timingsMs"].as_object().unwrap().values() {
            let value = duration.as_f64().unwrap() * 1000.0;
            assert!((value - value.round()).abs() < 1e-6);
        }
        drop(records);
        let snapshot = heartbeat(&server);
        assert_eq!(snapshot["productsLoaded"], 1);
        assert_eq!(snapshot["loads"]["fixture-forecast"][class]["count"], 1);
        assert_eq!(snapshot["opens"]["fixture-forecast"][open]["count"], 1);
        assert_eq!(
            snapshot["loads"]["fixture-forecast"]["product"]["failures"],
            0
        );
        let encoded = snapshot.to_string();
        for forbidden in [
            "latitude",
            "longitude",
            "snapshotId",
            input("forecast")["snapshot_id"].as_str().unwrap(),
            "temperature_2m",
        ] {
            assert!(!encoded.contains(forbidden), "{forbidden}");
        }
    }
}

#[tokio::test]
async fn canonical_analytics_counts_one_query_on_success_and_handler_error() {
    let server = Server::start(&[("POSTHOG_PROJECT_TOKEN", "fake-project-token")]).await;
    let first = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(first.status, 200);
    let canonical = first.json()["results"][0]["forecasts"][0]["links"]["canonical"]
        .as_str()
        .unwrap()
        .to_owned();
    for (path, status) in [
        (canonical.clone(), 200),
        (
            canonical.replace("variables=", "variables=unknown-variable,"),
            422,
        ),
    ] {
        assert_eq!(
            server.request("GET", &path, &Value::Null).await.status,
            status
        );
        let records = server.sink.0.lock().unwrap();
        let analytics = &records
            .iter()
            .rfind(|(stream, _)| *stream == Stream::Analytics)
            .unwrap()
            .1;
        assert_eq!(analytics["properties"]["query_count"], 1);
        assert_eq!(analytics["properties"]["status"], status);
        assert_eq!(
            analytics["properties"]["data_product_ids"],
            json!(["fixture-forecast"])
        );
        let terminal = &records
            .iter()
            .rfind(|(stream, _)| *stream == Stream::Terminal)
            .unwrap()
            .1;
        assert_eq!(terminal["queryCount"], 1);
    }
    assert_eq!(
        server
            .request("GET", &(canonical + "&maxLeadTimeHours=bad"), &Value::Null)
            .await
            .status,
        422
    );
    let records = server.sink.0.lock().unwrap();
    let analytics = &records
        .iter()
        .rfind(|(stream, _)| *stream == Stream::Analytics)
        .unwrap()
        .1;
    assert!(analytics["properties"]["query_count"].is_null());
}

#[tokio::test]
async fn pinned_handles_and_eviction_are_observed_as_new_sessions() {
    let server = Server::start(&[("DYNAMICAL_API_DATASET_CACHE_ENTRIES", "1")]).await;
    let first = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(first.status, 200);
    let path = first.json()["results"][0]["forecasts"][0]["links"]["canonical"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(server.request("GET", &path, &Value::Null).await.status, 200);
    assert_eq!(
        server
            .request("POST", "/v1/forecasts", &payload())
            .await
            .status,
        200
    );
    let snapshot = heartbeat(&server);
    assert_eq!(snapshot["productsLoaded"], 1);
    assert_eq!(snapshot["opens"]["fixture-forecast"]["miss"]["count"], 3);
    assert_eq!(snapshot["loads"]["fixture-forecast"]["product"]["count"], 1);
    assert_eq!(snapshot["loads"]["fixture-forecast"]["session"]["count"], 2);
}

#[tokio::test]
async fn failed_opens_count_before_any_load() {
    let server = Server::start(&[]).await;
    server
        .host
        .reads
        .fail_snapshots
        .store(true, Ordering::SeqCst);
    assert_eq!(
        server
            .request("POST", "/v1/forecasts", &payload())
            .await
            .status,
        500
    );
    let snapshot = heartbeat(&server);
    assert_eq!(snapshot["productsLoaded"], 0);
    assert_eq!(snapshot["opens"]["fixture-forecast"]["failed"]["count"], 1);
    assert_eq!(snapshot["loads"], json!({}));
    let records = server.sink.0.lock().unwrap();
    let failure = &records
        .iter()
        .find(|(stream, _)| *stream == Stream::Terminal)
        .unwrap()
        .1;
    assert_eq!(failure["counters"]["openFail"], 1);
    assert_eq!(failure["readValues"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_data_request_does_not_publish_late_success_and_keeps_worker_metrics() {
    use tokio::io::AsyncWriteExt;
    let server = Server::start(&[]).await;
    assert_eq!(
        server
            .request("POST", "/v1/forecasts", &payload())
            .await
            .status,
        200
    );
    server.sink.0.lock().unwrap().clear();
    server.host.reads.hold_chunks.store(true, Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let task = tokio::spawn(dynapi_server::transport::serve_connection(
        socket,
        server.app.clone(),
        "192.0.2.1".into(),
    ));
    let body = payload().to_string();
    client.write_all(format!("POST /v1/forecasts HTTP/1.1\r\nHost: example.test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server.host.reads.chunk_started.notified(),
    )
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    let snapshot = heartbeat(&server);
    assert_eq!(snapshot["inflightAdmitted"], 0);
    assert_eq!(snapshot["computeSlotsInUse"], 1);
    {
        let records = server.sink.0.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].1["event"], "api_failure");
        assert_eq!(records[0].1["cancelled"], true);
        assert_eq!(records[0].1["readValues"], 78);
    }
    server.host.reads.hold_chunks.store(false, Ordering::SeqCst);
    server.host.reads.release.add_permits(100);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while server.app.gate.snapshot().in_use != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = heartbeat(&server);
    assert_eq!(snapshot["loads"]["fixture-forecast"]["warm"]["count"], 1);
    assert_eq!(snapshot["loads"]["fixture-forecast"]["warm"]["failures"], 0);
    assert_eq!(server.sink.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_loads_count_without_warming_and_catalog_busy_markers_are_distinct() {
    let server = Server::start(&[("SENTRY_DSN", "https://public@errors.example.invalid/1")]).await;
    server.app.inject_read_failure(dynapi_reader::ReaderError {
        kind: dynapi_reader::ErrorKind::Storage,
        message: "fake-private-error".into(),
        virtual_chunk_fetch: false,
        virtual_failure: None,
        exception_class: None,
    });
    let failure = server.request("POST", "/v1/forecasts", &payload()).await;
    assert_eq!(failure.status, 500);
    let snapshot = heartbeat(&server);
    assert_eq!(snapshot["productsLoaded"], 0);
    assert_eq!(
        snapshot["loads"]["fixture-forecast"]["product"]["failures"],
        1
    );
    assert_eq!(
        server
            .request("POST", "/v1/forecasts", &payload())
            .await
            .status,
        200
    );
    let snapshot = heartbeat(&server);
    assert_eq!(snapshot["productsLoaded"], 1);
    assert_eq!(snapshot["loads"]["fixture-forecast"]["product"]["count"], 2);
    assert_eq!(snapshot["opens"]["fixture-forecast"]["hit"]["count"], 1);
    assert!(
        !server
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(_, record)| record.to_string().contains("fake-private-error"))
    );

    let catalog = Server::start(&[("SENTRY_DSN", "https://public@errors.example.invalid/1")]).await;
    catalog
        .host
        .reads
        .fail_catalog
        .store(true, Ordering::SeqCst);
    assert_eq!(
        catalog
            .request("POST", "/v1/forecasts", &payload())
            .await
            .status,
        503
    );
    {
        let records = catalog.sink.0.lock().unwrap();
        let errors = records
            .iter()
            .filter(|(stream, _)| *stream == Stream::Error)
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].1["exception"]["type"], "CatalogUnavailable");
    }
    let busy = Server::start(&[
        ("SENTRY_DSN", "https://public@errors.example.invalid/1"),
        ("DYNAMICAL_API_COMPUTE_SLOTS", "1"),
    ])
    .await;
    let _held = busy.app.gate.try_acquire().unwrap();
    assert_eq!(
        busy.request("POST", "/v1/forecasts", &payload())
            .await
            .status,
        503
    );
    let records = busy.sink.0.lock().unwrap();
    assert!(records.iter().all(|(stream, _)| *stream != Stream::Error));
    let failure = &records
        .iter()
        .find(|(stream, _)| *stream == Stream::Terminal)
        .unwrap()
        .1;
    assert_eq!(failure["readValues"], 0);
    assert_eq!(failure["queryCount"], 1);
}
