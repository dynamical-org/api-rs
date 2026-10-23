#![cfg(feature = "harness")]
mod support;
use icechunk::Store;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use support::*;

fn axis(name: &str, values: &[f64], units: &str) -> (Value, Vec<u8>) {
    (
        json!({"zarr_format":3,"node_type":"array","shape":[values.len()],"data_type":"float64",
        "dimension_names":[name],"chunk_grid":{"name":"regular","configuration":{"chunk_shape":[values.len()]}},
        "chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":"NaN",
        "codecs":[{"name":"bytes","configuration":{"endian":"little"}}],"attributes":{"units":units}}),
        values.iter().flat_map(|v| v.to_le_bytes()).collect(),
    )
}
async fn set(store: &Store, key: &str, value: Value) {
    store
        .set(key, serde_json::to_vec(&value).unwrap().into())
        .await
        .unwrap();
}
async fn fixture(root: &Path, name: &str, rotated: bool) -> String {
    let path = root.join(name).join("repository");
    std::fs::create_dir_all(&path).unwrap();
    let repo = icechunk::Repository::create(
        None,
        icechunk::new_local_filesystem_storage(&path).await.unwrap(),
        Default::default(),
        None,
        false,
    )
    .await
    .unwrap();
    let session = repo.writable_session("main").await.unwrap();
    let store = Store::from_session(Arc::new(tokio::sync::RwLock::new(session))).await;
    set(
        &store,
        "zarr.json",
        json!({"zarr_format":3,"node_type":"group","attributes":{"coordinates":"spatial_ref"}}),
    )
    .await;
    let scale = if rotated { 1. } else { 3000. };
    let units = if rotated { "degrees" } else { "m" };
    for (name, values, units) in [
        ("x", vec![-scale, 0., scale], units),
        ("y", vec![scale, 0., -scale], units),
        ("init_time", vec![0.], "seconds since 1970-01-01"),
        ("lead_time", vec![0., 3600., 7200.], "seconds"),
    ] {
        let (mut meta, mut data) = axis(name, &values, units);
        if name == "init_time" {
            meta["data_type"] = json!("int64");
            meta["fill_value"] = json!(0);
            meta["attributes"]["calendar"] = json!("proleptic_gregorian");
            data = 1_785_369_600i64.to_le_bytes().to_vec();
        }
        if name == "lead_time" {
            meta["attributes"]["dtype"] = json!("timedelta64[us]");
            meta["attributes"]["_FillValue"] = json!("AAAAAAAA+H8=");
        }
        set(&store, &format!("{name}/zarr.json"), meta).await;
        store
            .set(&format!("{name}/c/0"), data.into())
            .await
            .unwrap();
    }
    let wkts: Value = serde_json::from_str(include_str!(
        "../../dynapi-dataset/tests/fixtures/live-crs.json"
    ))
    .unwrap();
    let wkt = &wkts[if rotated {
        "rotated-sphere"
    } else {
        "lambert-conformal"
    }];
    set(&store, "spatial_ref/zarr.json", json!({"zarr_format":3,"node_type":"array","shape":[],"data_type":"int64","dimension_names":[],
        "chunk_grid":{"name":"regular","configuration":{"chunk_shape":[]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,
        "codecs":[{"name":"bytes","configuration":{"endian":"little"}}],"attributes":{"crs_wkt":wkt,"spatial_ref":wkts["wgs84"]}})).await;
    set(&store, "count/zarr.json", json!({"zarr_format":3,"node_type":"array","shape":[1,3,3,3],"data_type":"int64","dimension_names":["init_time","lead_time","y","x"],
        "chunk_grid":{"name":"regular","configuration":{"chunk_shape":[1,3,3,3]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":-1,
        "codecs":[{"name":"bytes","configuration":{"endian":"little"}}],"attributes":{}})).await;
    let data: Vec<u8> = (0i64..3)
        .flat_map(|lead| {
            (0i64..3).flat_map(move |y| {
                (0i64..3).flat_map(move |x| (100 * lead + 10 * y + x).to_le_bytes())
            })
        })
        .collect();
    store.set("count/c/0/0/0/0", data.into()).await.unwrap();
    let snapshot = store
        .session()
        .write()
        .await
        .commit("Synthetic projected fixture")
        .execute()
        .await
        .unwrap()
        .to_string();
    let mut collection = input("projected")["collection"].clone();
    collection["id"] = json!(name);
    collection["cube:variables"] =
        json!({"count":{"type":"data","dimensions":["init_time","lead_time","y","x"],"unit":null}});
    std::fs::write(
        root.join(name).join("collection.json"),
        serde_json::to_vec(&collection).unwrap(),
    )
    .unwrap();
    snapshot
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projected_forecasts_and_canonical_get_share_cells_values_locations_and_etags() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let names = ["fixture-lambert", "fixture-rotated"];
    let snapshots = [
        fixture(temp.path(), names[0], false).await,
        fixture(temp.path(), names[1], true).await,
    ];
    std::fs::write(temp.path().join("catalog.json"), serde_json::to_vec(&json!({"links":names.map(|n| json!({"rel":"child","href":format!("/{n}/collection.json")}))})).unwrap()).unwrap();
    let server = Server::start_with_catalog_and_root(&[], None, Some(temp.path().to_owned())).await;
    for (i, (latitude, longitude)) in [(38.5, -97.5), (90. - 36.08852, -114.694858)]
        .into_iter()
        .enumerate()
    {
        let mut body = json!({"queries":[{"dataProductId":names[i],"location":{"latitude":latitude,"longitude":longitude},"variables":["count"],"initTime":"2026-07-30T00:00:00Z","maxLeadTimeHours":1}]});
        let post = server.request("POST", "/v1/forecasts", &body).await;
        assert_eq!(
            post.status,
            200,
            "{} {:?}",
            String::from_utf8_lossy(&post.body),
            server.sink.0.lock().unwrap()
        );
        let result = &post.json()["results"][0];
        let selected = &result["point"]["selected"];
        assert_eq!(selected["grid"], json!({"yIndex":1,"xIndex":1}));
        assert!((selected["latitude"].as_f64().unwrap() - latitude).abs() < 1e-12);
        assert!((selected["longitude"].as_f64().unwrap() - longitude).abs() < 1e-12);
        assert_eq!(result["forecasts"][0]["data"]["count"], json!([11, 111]));
        assert_eq!(result["snapshotId"], snapshots[i]);
        assert_eq!(
            post.headers["etag"],
            format!("\"{:x}\"", Sha256::digest(&post.body))
        );
        let path = result["forecasts"][0]["links"]["canonical"]
            .as_str()
            .unwrap();
        assert!(path.contains("/points/1,1?"));
        let get = server.request("GET", path, &Value::Null).await;
        assert_eq!(get.status, 200, "{}", String::from_utf8_lossy(&get.body));
        assert_eq!(get.json()["point"]["selected"], *selected);
        assert_eq!(get.json()["forecasts"][0], result["forecasts"][0]);
        assert!(get.headers["cache-control"].ends_with("immutable"));
        assert_eq!(
            get.headers["etag"],
            format!("\"{:x}\"", Sha256::digest(&get.body))
        );
        let repeat = server.request("GET", path, &Value::Null).await;
        assert_eq!(
            (repeat.body, &repeat.headers["etag"]),
            (get.body, &get.headers["etag"])
        );
        let bad_path = path.replace("/points/1,1?", "/points/3,1?");
        let bad_get = server.request("GET", &bad_path, &Value::Null).await;
        assert_eq!(bad_get.status, 422);
        assert_eq!(
            bad_get.json(),
            json!({"detail":"y_index is outside the grid"})
        );
        body["queries"][0]["location"] = json!({"latitude":0,"longitude":0});
        let outside = server.request("POST", "/v1/forecasts", &body).await;
        assert_eq!(outside.status, 422);
        assert_eq!(
            outside.body,
            br#"{"detail":"Location is outside the data product grid"}"#
        );
        assert!(!outside.headers.contains_key("etag"));
    }
}
