#![cfg(feature = "harness")]
//! Axes and values are constructed here; see the arithmetic recipe in tests/README.md.
mod support;

use icechunk::Store;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use support::{Server, input};

const SECOND: i64 = 1_000_000_000;
const HOUR: i64 = 3600 * SECOND;
const BASE: i64 = 1_785_369_600 * SECOND;
const T0: &str = "2026-07-30T00:00:00Z";
const T1: &str = "2026-07-30T01:00:00Z";
const T2: &str = "2026-07-30T02:00:00Z";

async fn array(
    store: &Store,
    name: &str,
    dims: &[&str],
    shape: &[usize],
    attrs: Value,
    values: &[i64],
) {
    let metadata = json!({"zarr_format":3,"node_type":"array","shape":shape,"data_type":if name=="lead_time" {"float64"} else {"int64"},
        "dimension_names":dims,"chunk_grid":{"name":"regular","configuration":{"chunk_shape":shape}},
        "chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":if name=="lead_time" {json!("NaN")} else {json!(0)},
        "codecs":[{"name":"bytes","configuration":{"endian":"little"}}],"attributes":attrs});
    store
        .set(
            &format!("{name}/zarr.json"),
            serde_json::to_vec(&metadata).unwrap().into(),
        )
        .await
        .unwrap();
    let key = format!("{name}/c/{}", vec!["0"; dims.len()].join("/"));
    store
        .set(
            &key,
            values
                .iter()
                .flat_map(|n| {
                    if name == "lead_time" {
                        (if *n == i64::MIN {
                            f64::NAN
                        } else {
                            *n as f64 / SECOND as f64
                        })
                        .to_le_bytes()
                    } else {
                        (if matches!(name, "init_time" | "time") && *n != i64::MIN {
                            *n / SECOND
                        } else {
                            *n
                        })
                        .to_le_bytes()
                    }
                })
                .collect::<Vec<_>>()
                .into(),
        )
        .await
        .unwrap();
}

async fn fixture(root: &Path, forecast: bool) -> String {
    let name = if forecast { "forecast" } else { "analysis" };
    let id = format!("fixture-{name}");
    let repo = icechunk::Repository::create(
        None,
        icechunk::new_local_filesystem_storage(&root.join(&id).join("repository"))
            .await
            .unwrap(),
        Default::default(),
        None,
        false,
    )
    .await
    .unwrap();
    let store = Store::from_session(Arc::new(tokio::sync::RwLock::new(
        repo.writable_session("main").await.unwrap(),
    )))
    .await;
    store
        .set(
            "zarr.json",
            br#"{"zarr_format":3,"node_type":"group","attributes":{}}"#
                .to_vec()
                .into(),
        )
        .await
        .unwrap();
    array(&store, "latitude", &["latitude"], &[1], json!({}), &[44]).await;
    array(&store, "longitude", &["longitude"], &[1], json!({}), &[0]).await;
    let dims = if forecast {
        array(
            &store,
            "init_time",
            &["init_time"],
            &[5],
            json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"}),
            &[
                BASE + 2 * HOUR,
                i64::MIN,
                BASE,
                BASE + HOUR,
                BASE + 2 * HOUR,
            ],
        )
        .await;
        array(
            &store,
            "lead_time",
            &["lead_time"],
            &[7],
            json!({"units":"seconds","dtype":"timedelta64[us]","_FillValue":"AAAAAAAA+H8="}),
            &[
                -HOUR / 2,
                0,
                HOUR / 2,
                HOUR,
                HOUR + 1000,
                2 * HOUR,
                i64::MIN,
            ],
        )
        .await;
        let values: Vec<_> = (0..5)
            .flat_map(|r| (0..7).map(move |l| 100 * r + l))
            .collect();
        array(
            &store,
            "count",
            &["init_time", "lead_time", "latitude", "longitude"],
            &[5, 7, 1, 1],
            json!({}),
            &values,
        )
        .await;
        json!(["init_time", "lead_time", "latitude", "longitude"])
    } else {
        array(
            &store,
            "time",
            &["time"],
            &[6],
            json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"}),
            &[
                BASE + HOUR,
                i64::MIN,
                BASE,
                BASE + HOUR,
                BASE + 2 * HOUR,
                BASE + HOUR + SECOND,
            ],
        )
        .await;
        array(
            &store,
            "count",
            &["time", "latitude", "longitude"],
            &[6, 1, 1],
            json!({}),
            &[1000, 1001, 1002, 1003, 1004, 1005],
        )
        .await;
        json!(["time", "latitude", "longitude"])
    };
    let snapshot = store
        .session()
        .write()
        .await
        .commit("Synthetic temporal boundary recipe")
        .execute()
        .await
        .unwrap()
        .to_string();
    let mut collection = input(name)["collection"].clone();
    collection["cube:variables"] = json!({"count":{"type":"data","dimensions":dims,"unit":null}});
    std::fs::write(
        root.join(id).join("collection.json"),
        serde_json::to_vec(&collection).unwrap(),
    )
    .unwrap();
    snapshot
}

async fn server() -> (tempfile::TempDir, Server) {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), true).await;
    fixture(root.path(), false).await;
    std::fs::write(root.path().join("catalog.json"), br#"{"links":[{"rel":"child","href":"/fixture-forecast/collection.json"},{"rel":"child","href":"/fixture-analysis/collection.json"}]}"#).unwrap();
    let server = Server::start_with_catalog_and_root(&[], None, Some(root.path().to_owned())).await;
    (root, server)
}

fn forecast() -> Value {
    json!({"dataProductId":"fixture-forecast","location":{"latitude":44,"longitude":0},"variables":["count"],"maxLeadTimeHours":2})
}

#[tokio::test]
async fn latest_exact_and_strict_run_bounds_choose_last_duplicate_source_values() {
    let (_root, server) = server().await;
    for (name, fields, runs, rows) in [
        ("implicit latest", json!({}), vec![T2], vec![4]),
        (
            "explicit latest",
            json!({"initTime":"latest"}),
            vec![T2],
            vec![4],
        ),
        ("exact duplicate", json!({"initTime":T2}), vec![T2], vec![4]),
        ("exact older", json!({"initTime":T0}), vec![T0], vec![2]),
        (
            "exclusive both",
            json!({"initTimesAfter":T0,"initTimesBefore":T2}),
            vec![T1],
            vec![3],
        ),
        (
            "source order and duplicates",
            json!({"initTimesAfter":"2026-07-29T23:59:59.999999Z","initTimesBefore":"2026-07-30T02:00:00.000001Z"}),
            vec![T2, T0, T1, T2],
            vec![4, 2, 3, 4],
        ),
    ] {
        let mut query = forecast();
        query
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let reply = server
            .request("POST", "/v1/forecasts", &json!({"queries":[query]}))
            .await;
        assert_eq!(
            reply.status,
            200,
            "{name}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        let result = reply.json();
        let selected = result["results"][0]["forecasts"].as_array().unwrap();
        assert_eq!(selected.len(), runs.len(), "{name}");
        for ((item, run), row) in selected.iter().zip(runs).zip(rows) {
            assert_eq!(item["initTime"], run, "{name}");
            let times = match run {
                T0 => vec![
                    "2026-07-29T23:30:00Z",
                    T0,
                    "2026-07-30T00:30:00Z",
                    T1,
                    "2026-07-30T01:00:00.000001Z",
                    T2,
                ],
                T1 => vec![
                    "2026-07-30T00:30:00Z",
                    T1,
                    "2026-07-30T01:30:00Z",
                    T2,
                    "2026-07-30T02:00:00.000001Z",
                    "2026-07-30T03:00:00Z",
                ],
                T2 => vec![
                    "2026-07-30T01:30:00Z",
                    T2,
                    "2026-07-30T02:30:00Z",
                    "2026-07-30T03:00:00Z",
                    "2026-07-30T03:00:00.000001Z",
                    "2026-07-30T04:00:00Z",
                ],
                _ => unreachable!(),
            };
            assert_eq!(item["validTimes"], json!(times), "{name}");
            assert_eq!(
                item["leadTimeHours"],
                json!([-0.5, 0., 0.5, 1., 1. + 999. / 3_600_000_000_000., 2.]),
                "{name}"
            );
            assert_eq!(
                item["data"]["count"],
                json!((0..6).map(|l| 100 * row + l).collect::<Vec<_>>()),
                "{name}"
            );
            let canonical = server
                .request(
                    "GET",
                    item["links"]["canonical"].as_str().unwrap(),
                    &Value::Null,
                )
                .await;
            assert_eq!(canonical.status, 200);
            assert_eq!(canonical.json()["forecasts"][0], *item);
        }
    }
    for (fields, message) in [
        (
            json!({"initTime":"2026-07-30T03:00:00Z"}),
            "initTime 2026-07-30T03:00:00Z is not present in this snapshot",
        ),
        (
            json!({"initTimesAfter":T0,"initTimesBefore":T1}),
            "No forecast runs match the requested initialization-time bounds",
        ),
    ] {
        let mut query = forecast();
        query
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let reply = server
            .request("POST", "/v1/forecasts", &json!({"queries":[query]}))
            .await;
        assert_eq!(reply.status, 422);
        assert_eq!(reply.json(), json!({"detail":message}));
    }
}

#[tokio::test]
async fn forecast_half_open_fractional_and_max_lead_masks_own_times_and_values() {
    let (_root, server) = server().await;
    for (start, end, max, indices, times, leads) in [
        (
            "2026-07-29T23:30:00Z",
            T1,
            2,
            vec![0, 1, 2],
            vec!["2026-07-29T23:30:00Z", T0, "2026-07-30T00:30:00Z"],
            vec![-0.5, 0., 0.5],
        ),
        (
            T0,
            T2,
            1,
            vec![1, 2, 3],
            vec![T0, "2026-07-30T00:30:00Z", T1],
            vec![0., 0.5, 1.],
        ),
        (
            T1,
            "2026-07-30T01:00:00.000001Z",
            2,
            vec![3, 4],
            vec![T1, "2026-07-30T01:00:00.000001Z"],
            vec![1., 1. + 999. / 3_600_000_000_000.],
        ),
        (
            T2,
            "2026-07-30T02:00:00.000001Z",
            2,
            vec![5],
            vec![T2],
            vec![2.],
        ),
        (
            T0,
            "2026-07-30T00:00:00.000001Z",
            0,
            vec![1],
            vec![T0],
            vec![0.],
        ),
    ] {
        let mut query = forecast();
        query["initTime"] = json!(T0);
        query["validTimeStart"] = json!(start);
        query["validTimeEnd"] = json!(end);
        query["maxLeadTimeHours"] = json!(max);
        let reply = server
            .request("POST", "/v1/forecasts", &json!({"queries":[query]}))
            .await;
        assert_eq!(
            reply.status,
            200,
            "{start}..{end}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        let item = reply.json()["results"][0]["forecasts"][0].clone();
        assert_eq!(item["validTimes"], json!(times));
        assert_eq!(item["leadTimeHours"], json!(leads));
        assert_eq!(
            item["data"]["count"],
            json!(indices.iter().map(|l| 200 + l).collect::<Vec<_>>())
        );
        let canonical = server
            .request(
                "GET",
                item["links"]["canonical"].as_str().unwrap(),
                &Value::Null,
            )
            .await;
        assert_eq!(canonical.status, 200);
        assert_eq!(canonical.json()["forecasts"][0], item);
    }
    // 3600.000001 stored seconds decode to 3600s + 999ns. Presentation
    // rounds to one microsecond, but filtering uses the unrounded coordinate.
    let mut query = forecast();
    query["initTime"] = json!(T0);
    query["validTimeStart"] = json!("2026-07-30T01:00:00.000001Z");
    query["validTimeEnd"] = json!(T2);
    let reply = server
        .request("POST", "/v1/forecasts", &json!({"queries":[query.clone()]}))
        .await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json(),
        json!({"detail":"No forecast times match the requested bounds"})
    );
    query["validTimeStart"] = json!("2026-07-30T02:00:00.000001Z");
    query["validTimeEnd"] = json!("2026-07-30T03:00:00Z");
    let reply = server
        .request("POST", "/v1/forecasts", &json!({"queries":[query]}))
        .await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json(),
        json!({"detail":"No forecast times match the requested bounds"})
    );
}

#[tokio::test]
async fn analysis_masks_preserve_unsorted_duplicates_exclude_nat_and_end() {
    let (_root, server) = server().await;
    for (start, end, indices, times) in [
        (
            T0,
            T2,
            vec![0, 2, 3, 5],
            vec![T1, T0, T1, "2026-07-30T01:00:01Z"],
        ),
        (T1, "2026-07-30T01:00:00.000001Z", vec![0, 3], vec![T1, T1]),
        (
            "2026-07-30T01:00:00.000001Z",
            T2,
            vec![5],
            vec!["2026-07-30T01:00:01Z"],
        ),
        (T2, "2026-07-30T02:00:00.000001Z", vec![4], vec![T2]),
        (
            "2026-07-30T01:00:00+01:00",
            "2026-07-30T02:00:00+01:00",
            vec![2],
            vec![T0],
        ),
    ] {
        let query = json!({"dataProductId":"fixture-analysis","location":{"latitude":44,"longitude":0},"variables":["count"],"startTime":start,"endTime":end});
        let reply = server
            .request("POST", "/v1/analyses", &json!({"queries":[query]}))
            .await;
        assert_eq!(
            reply.status,
            200,
            "{start}..{end}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        let item = reply.json()["results"][0].clone();
        assert_eq!(item["analysisTimes"], json!(times));
        assert_eq!(
            item["data"]["count"],
            json!(indices.iter().map(|t| 1000 + t).collect::<Vec<_>>())
        );
        let canonical = server
            .request(
                "GET",
                item["links"]["canonical"].as_str().unwrap(),
                &Value::Null,
            )
            .await;
        assert_eq!(canonical.status, 200);
        assert_eq!(canonical.json()["analysisTimes"], item["analysisTimes"]);
        assert_eq!(canonical.json()["data"], item["data"]);
        if start == T0 {
            let default_path = format!(
                "{}?variables=count",
                item["links"]["canonical"]
                    .as_str()
                    .unwrap()
                    .split('?')
                    .next()
                    .unwrap()
            );
            let latest = server.request("GET", &default_path, &Value::Null).await;
            assert_eq!(latest.status, 200);
            assert_eq!(
                latest.json()["analysisTimes"],
                json!(["2026-07-30T01:00:01Z"])
            );
            assert_eq!(latest.json()["data"]["count"], json!([1005]));
        }
    }
    let reply=server.request("POST","/v1/analyses",&json!({"queries":[{"dataProductId":"fixture-analysis","location":{"latitude":44,"longitude":0},"variables":["count"],"startTime":"2026-07-30T02:00:00.000001Z","endTime":"2026-07-30T03:00:00Z"}]})).await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        reply.json(),
        json!({"detail":"No analysis times match the requested bounds"})
    );
}
