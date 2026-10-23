#![cfg(feature = "harness")]
//! Snapshot values and response bytes come from the recipe in tests/README.md.
mod support;

use icechunk::Store;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
};
use support::{Reply, Server, fixtures, input};

const INIT: &str = "2026-07-30T00:00:00Z";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

fn copy_dir(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}

async fn set(store: &Store, key: &str, value: Value) {
    store
        .set(key, serde_json::to_vec(&value).unwrap().into())
        .await
        .unwrap();
}

fn array(dtype: &str, dimensions: &[&str], shape: &[usize], attributes: Value) -> Value {
    json!({"zarr_format":3,"node_type":"array","shape":shape,"data_type":dtype,
        "dimension_names":dimensions,"chunk_grid":{"name":"regular","configuration":{"chunk_shape":shape}},
        "chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},
        "fill_value":if dtype.starts_with("float") { json!("NaN") } else { json!(0) },"codecs":[{"name":"bytes","configuration":{"endian":"little"}}],"attributes":attributes})
}

async fn commit(root: &Path, second: bool) -> String {
    let path = root.join("fixture-forecast/repository");
    let repository = icechunk::Repository::open(
        None,
        icechunk::new_local_filesystem_storage(&path).await.unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let session = repository.writable_session("main").await.unwrap();
    let store = Store::from_session(Arc::new(tokio::sync::RwLock::new(session))).await;
    if !second {
        for name in ["temperature_2m", "static", "valid_time"] {
            store.delete(&format!("{name}/zarr.json")).await.unwrap();
        }
        set(
            &store,
            "zarr.json",
            json!({"zarr_format":3,"node_type":"group","attributes":{"coordinates":"spatial_ref"}}),
        )
        .await;
    }
    let size = if second { 3 } else { 2 };
    set(
        &store,
        "init_time/zarr.json",
        array(
            "int64",
            &["init_time"],
            &[size],
            json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"}),
        ),
    )
    .await;
    let runs: Vec<u8> = (0..size as i64)
        .flat_map(|run| (1_785_369_600 + run * 21_600).to_le_bytes())
        .collect();
    store.set("init_time/c/0", runs.into()).await.unwrap();
    set(
        &store,
        "lead_time/zarr.json",
        array(
            "float64",
            &["lead_time"],
            &[size],
            json!({"units":"seconds","dtype":"timedelta64[us]","_FillValue":"AAAAAAAA+H8="}),
        ),
    )
    .await;
    let leads: Vec<u8> = (0..size)
        .flat_map(|lead| (lead as f64 * 3600.).to_le_bytes())
        .collect();
    store.set("lead_time/c/0", leads.into()).await.unwrap();
    let shape = [size, size, 4, 6];
    let dimensions = ["init_time", "lead_time", "latitude", "longitude"];
    set(
        &store,
        "count/zarr.json",
        array("int64", &dimensions, &shape, json!({})),
    )
    .await;
    let base = if second { 2000 } else { 1000 };
    let mut counts = Vec::new();
    let mut humidity = Vec::new();
    for run in 0..size as i64 {
        for lead in 0..size as i64 {
            for cell in 0..24i64 {
                counts.extend_from_slice(&(base + 100 * run + 10 * lead + cell).to_le_bytes());
                humidity.extend_from_slice(
                    &(50. + 10. * run as f64 + lead as f64 + cell as f64).to_le_bytes(),
                );
            }
        }
    }
    store.set("count/c/0/0/0/0", counts.into()).await.unwrap();
    if second {
        set(
            &store,
            "humidity/zarr.json",
            array("float64", &dimensions, &shape, json!({"units":"%"})),
        )
        .await;
        store
            .set("humidity/c/0/0/0/0", humidity.into())
            .await
            .unwrap();
    }
    store
        .session()
        .write()
        .await
        .commit(if second {
            "Synthetic history B"
        } else {
            "Synthetic history A"
        })
        .execute()
        .await
        .unwrap()
        .to_string()
}

async fn history() -> (tempfile::TempDir, String) {
    let temp = tempfile::tempdir().unwrap();
    copy_dir(
        &fixtures().join("forecast/repository"),
        &temp.path().join("fixture-forecast/repository"),
    );
    let snapshot = commit(temp.path(), false).await;
    let mut collection = input("forecast")["collection"].clone();
    // A current catalog may advertise variables absent from historical snapshots.
    collection["cube:variables"] = json!({
        "count":{"type":"data","dimensions":["init_time","lead_time","latitude","longitude"],"unit":null},
        "humidity":{"type":"data","dimensions":["init_time","lead_time","latitude","longitude"],"unit":"%"}
    });
    std::fs::write(
        temp.path().join("fixture-forecast/collection.json"),
        serde_json::to_vec(&collection).unwrap(),
    )
    .unwrap();
    std::fs::write(
        temp.path().join("catalog.json"),
        br#"{"links":[{"rel":"child","href":"/fixture-forecast/collection.json"}]}"#,
    )
    .unwrap();
    (temp, snapshot)
}

fn request(variables: &[&str], init: &str) -> Value {
    json!({"queries":[{"dataProductId":"fixture-forecast","location":{"latitude":44,"longitude":0},"variables":variables,"initTime":init}]})
}

fn canonical(snapshot: &str, variables: &str, hour: &str) -> String {
    format!(
        "/v1/data/fixture-forecast/{snapshot}/points/0,0?variables={variables}&initTime=2026-07-30T{hour}%3A00%3A00Z"
    )
}

fn link(reply: &Reply) -> String {
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    reply.json()["results"][0]["forecasts"][0]["links"]["canonical"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn expected_a(snapshot: &str) -> Vec<u8> {
    // Complete owned JSON spelling/order, with only the newly committed ID substituted.
    let path = canonical(snapshot, "count", "00");
    format!(concat!(
        "{{\"dataProduct\":{{\"id\":\"fixture-forecast\",\"version\":\"1.2.3\"}},",
        "\"point\":{{\"selected\":{{\"latitude\":44.0,\"longitude\":0.0,\"grid\":{{\"latitudeIndex\":0,\"longitudeIndex\":0}}}},\"method\":\"nearest\"}},",
        "\"variables\":{{\"count\":{{\"unit\":null}}}},\"snapshotId\":\"{}\",",
        "\"forecasts\":[{{\"initTime\":\"2026-07-30T00:00:00Z\",\"validTimes\":[\"2026-07-30T00:00:00Z\",\"2026-07-30T01:00:00Z\"],",
        "\"leadTimeHours\":[0.0,1.0],\"links\":{{\"canonical\":\"{}\"}},\"data\":{{\"count\":[1000,1010]}}}}],",
        "\"links\":{{\"catalog\":\"https://example.invalid/forecast/collection.json\"}}}}"
    ), snapshot, path).into_bytes()
}

fn assert_a(reply: &Reply, snapshot: &str, cache: &str) {
    let body = expected_a(snapshot);
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.body, body);
    assert_eq!(
        reply.headers["etag"],
        format!("\"{:x}\"", Sha256::digest(&body))
    );
    assert_eq!(reply.headers["content-length"], body.len().to_string());
    assert_eq!(reply.headers["content-type"], "application/json");
    assert_eq!(reply.headers["cache-control"], IMMUTABLE);
    assert_eq!(reply.headers["x-dynamical-dataset-cache"], cache);
}

#[tokio::test]
async fn historical_canonical_links_survive_main_advance_and_cache_eviction() {
    let (temp, a) = history().await;
    let server = Server::start_with_catalog_and_root(
        &[("DYNAMICAL_API_DATASET_CACHE_ENTRIES", "2")],
        None,
        Some(temp.path().to_owned()),
    )
    .await;
    let query = request(&["count"], INIT);
    let post_a = server.request("POST", "/v1/forecasts", &query).await;
    let link_a = link(&post_a);
    assert_eq!(link_a, canonical(&a, "count", "00"));
    assert_eq!(post_a.json()["results"][0]["snapshotId"], a);
    assert_eq!(post_a.headers["x-dynamical-dataset-cache"], "miss");
    let first_a = server.request("GET", &link_a, &Value::Null).await;
    assert_a(&first_a, &a, "miss");
    assert_a(
        &server.request("GET", &link_a, &Value::Null).await,
        &a,
        "hit",
    );

    let b = commit(temp.path(), true).await;
    assert_ne!(a, b);
    server.clock.0.store(29, Ordering::SeqCst);
    let within_ttl = server.request("POST", "/v1/forecasts", &query).await;
    assert_eq!(within_ttl.body, post_a.body);
    assert_eq!(within_ttl.headers["x-dynamical-dataset-cache"], "hit");
    server.clock.0.store(30, Ordering::SeqCst);
    let post_b = server.request("POST", "/v1/forecasts", &query).await;
    let link_b = link(&post_b);
    assert_eq!(link_b, canonical(&b, "count", "00"));
    assert_ne!(link_a, link_b);
    assert_eq!(post_b.json()["results"][0]["snapshotId"], b);
    assert_eq!(
        post_b.json()["results"][0]["forecasts"][0]["data"]["count"],
        json!([2000, 2010, 2020])
    );
    assert_eq!(post_b.headers["x-dynamical-dataset-cache"], "miss");
    assert_eq!(
        post_b.headers["cache-control"],
        "public, max-age=30, stale-while-revalidate=120"
    );
    assert_ne!(post_a.headers["etag"], post_b.headers["etag"]);
    assert_a(
        &server.request("GET", &link_a, &Value::Null).await,
        &a,
        "hit",
    );
    let canonical_b = server.request("GET", &link_b, &Value::Null).await;
    assert_eq!(canonical_b.status, 200);
    assert_eq!(canonical_b.headers["x-dynamical-dataset-cache"], "miss");
    assert_eq!(
        canonical_b.json()["forecasts"][0],
        post_b.json()["results"][0]["forecasts"][0]
    );
    assert_ne!(canonical_b.headers["etag"], first_a.headers["etag"]);

    // The two immutable keys coexist; warm reads need no snapshot lookup.
    server
        .host
        .reads
        .fail_snapshots
        .store(true, Ordering::SeqCst);
    assert_a(
        &server.request("GET", &link_a, &Value::Null).await,
        &a,
        "hit",
    );
    let hit_b = server.request("GET", &link_b, &Value::Null).await;
    assert_eq!(hit_b.body, canonical_b.body);
    assert_eq!(hit_b.headers["etag"], canonical_b.headers["etag"]);
    assert_eq!(hit_b.headers["x-dynamical-dataset-cache"], "hit");
    server
        .host
        .reads
        .fail_snapshots
        .store(false, Ordering::SeqCst);
    // Main was evicted by the two pinned keys. Reopening it evicts old A.
    let reopened_main = server.request("POST", "/v1/forecasts", &query).await;
    assert_eq!(reopened_main.body, post_b.body);
    assert_eq!(reopened_main.headers["x-dynamical-dataset-cache"], "miss");
    server.host.reads.paths.lock().unwrap().clear();
    assert_a(
        &server.request("GET", &link_a, &Value::Null).await,
        &a,
        "miss",
    );
    assert!(
        server
            .host
            .reads
            .paths
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == &format!("/fixture-forecast/repository/snapshots/{a}"))
    );
    server.clock.0.store(60, Ordering::SeqCst);
    let revalidated = server.request("POST", "/v1/forecasts", &query).await;
    assert_eq!(revalidated.body, post_b.body);
    assert_eq!(
        revalidated.headers["x-dynamical-dataset-cache"],
        "revalidated"
    );
    assert_a(
        &server.request("GET", &link_a, &Value::Null).await,
        &a,
        "hit",
    );
}

fn assert_error(reply: &Reply, status: u16, detail: &str) {
    assert_eq!(
        reply.status,
        status,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.json(), json!({"detail":detail}));
    assert_eq!(reply.headers["content-type"], "application/json");
    assert_eq!(
        reply.headers["content-length"],
        reply.body.len().to_string()
    );
    for absent in ["cache-control", "etag", "retry-after"] {
        assert!(!reply.headers.contains_key(absent));
    }
}

#[tokio::test]
async fn historical_schema_and_appended_times_are_snapshot_specific() {
    let (temp, a) = history().await;
    let server = Server::start_with_catalog_and_root(&[], None, Some(temp.path().to_owned())).await;
    let post_a = server
        .request("POST", "/v1/forecasts", &request(&["count"], INIT))
        .await;
    let link_a = link(&post_a);
    let b = commit(temp.path(), true).await;
    server.clock.0.store(30, Ordering::SeqCst);
    let post_b = server
        .request(
            "POST",
            "/v1/forecasts",
            &request(&["count", "humidity"], "latest"),
        )
        .await;
    let link_b = link(&post_b);
    assert_eq!(link_b, canonical(&b, "count%2Chumidity", "12"));
    let result = &post_b.json()["results"][0];
    assert_eq!(
        result["variables"],
        json!({"count":{"unit":null},"humidity":{"unit":"%"}})
    );
    assert_eq!(result["forecasts"][0]["initTime"], "2026-07-30T12:00:00Z");
    assert_eq!(
        result["forecasts"][0]["validTimes"],
        json!([
            "2026-07-30T12:00:00Z",
            "2026-07-30T13:00:00Z",
            "2026-07-30T14:00:00Z"
        ])
    );
    assert_eq!(
        result["forecasts"][0]["data"],
        json!({"count":[2200,2210,2220],"humidity":[70.,71.,72.]})
    );
    let canonical_b = server.request("GET", &link_b, &Value::Null).await;
    assert_eq!(canonical_b.status, 200);
    assert_eq!(canonical_b.json()["forecasts"][0], result["forecasts"][0]);
    assert_eq!(canonical_b.headers["cache-control"], IMMUTABLE);
    assert_a(
        &server.request("GET", &link_a, &Value::Null).await,
        &a,
        "miss",
    );
    let missing = server
        .request(
            "GET",
            &canonical(&a, "count%2Chumidity", "00"),
            &Value::Null,
        )
        .await;
    assert_error(
        &missing,
        422,
        &format!("Variable(s) listed in the catalog are absent from snapshot {a}: humidity"),
    );
    let absent_run = server
        .request("GET", &canonical(&a, "count", "12"), &Value::Null)
        .await;
    assert_error(
        &absent_run,
        422,
        "Run 2026-07-30T12:00:00Z is not present in this snapshot",
    );
    let absent_lead = server
        .request(
            "GET",
            &format!("{link_a}&validTimeStart=2026-07-30T02%3A00%3A00Z"),
            &Value::Null,
        )
        .await;
    assert_error(
        &absent_lead,
        422,
        "No forecast times match the requested bounds",
    );
    let appended_lead = server
        .request(
            "GET",
            &format!(
                "{}&validTimeStart=2026-07-30T02%3A00%3A00Z",
                canonical(&b, "count", "00")
            ),
            &Value::Null,
        )
        .await;
    assert_eq!(appended_lead.status, 200);
    assert_eq!(
        appended_lead.json()["forecasts"][0]["data"]["count"],
        json!([2020])
    );
    assert_eq!(
        appended_lead.json()["forecasts"][0]["validTimes"],
        json!(["2026-07-30T02:00:00Z"])
    );
    assert_eq!(appended_lead.headers["cache-control"], IMMUTABLE);
}

#[tokio::test]
async fn malformed_unknown_and_branch_snapshot_ids_have_exact_public_404_errors() {
    let (temp, _) = history().await;
    let server = Server::start_with_catalog_and_root(&[], None, Some(temp.path().to_owned())).await;
    for snapshot in [
        "main",
        "not-a-snapshot",
        "00000000000000000000",
        "UUUUUUUUUUUUUUUUUUUU",
    ] {
        let reply = server
            .request("GET", &canonical(snapshot, "count", "00"), &Value::Null)
            .await;
        assert_error(
            &reply,
            404,
            &format!(
                "Unknown snapshot '{snapshot}' for data product 'fixture-forecast'; use the snapshotId from a forecasts or analyses response"
            ),
        );
        assert_eq!(reply.body, format!("{{\"detail\":\"Unknown snapshot '{snapshot}' for data product 'fixture-forecast'; use the snapshotId from a forecasts or analyses response\"}}").as_bytes());
    }
}
