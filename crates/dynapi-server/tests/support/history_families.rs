//! Private A/B repositories; construction and arithmetic are in history_families.md.
use crate::support::{Server, input};
use icechunk::{Repository, Store};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

#[derive(Clone, Copy)]
pub enum Family {
    Analysis,
    Ensemble { change_members: bool },
    Lambert,
    Forecast,
}
impl Family {
    pub fn id(self) -> &'static str {
        match self {
            Self::Analysis => "fixture-history-analysis",
            Self::Ensemble { .. } => "fixture-history-ensemble",
            Self::Lambert => "fixture-history-lambert",
            Self::Forecast => "fixture-history-forecast",
        }
    }
    pub fn members(self, second: bool) -> Option<&'static [i64]> {
        match self {
            Self::Ensemble {
                change_members: true,
            } if second => Some(&[-2, 11]),
            Self::Ensemble { .. } => Some(&[7, -2, 7]),
            _ => None,
        }
    }
    pub fn route(self) -> &'static str {
        if matches!(self, Self::Analysis) {
            "/v1/analyses"
        } else {
            "/v1/forecasts"
        }
    }
    pub fn request(self, variables: &[&str]) -> Value {
        let location = if matches!(self, Self::Lambert) {
            json!({"latitude":38.5,"longitude":-97.5})
        } else {
            json!({"latitude":44,"longitude":0})
        };
        let mut query =
            json!({"dataProductId":self.id(),"location":location,"variables":variables});
        if matches!(self, Self::Analysis) {
            query["startTime"] = json!("2026-07-30T01:00:00Z");
            query["endTime"] = json!("2026-07-30T03:00:00Z");
        } else {
            query["initTime"] = json!("2026-07-30T00:00:00Z");
        }
        json!({"queries":[query]})
    }
}

pub struct History {
    pub root: tempfile::TempDir,
    pub family: Family,
    pub a: String,
    pub server: Server,
}
impl History {
    pub async fn new(family: Family) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(family.id()).join("repository");
        let repository = Repository::create(
            None,
            icechunk::new_local_filesystem_storage(&path).await.unwrap(),
            Default::default(),
            None,
            false,
        )
        .await
        .unwrap();
        drop(repository);
        let a = commit(root.path(), family, false).await;
        let template = match family {
            Family::Analysis => "analysis",
            Family::Lambert => "projected",
            Family::Ensemble { .. } => "ensemble",
            Family::Forecast => "forecast",
        };
        let mut collection = input(template)["collection"].clone();
        collection["id"] = json!(family.id());
        let dimensions = dimensions(family);
        collection["cube:variables"] = json!({
            "count":{"type":"data","dimensions":dimensions,"unit":null},
            "humidity":{"type":"data","dimensions":dimensions,"unit":"%"}
        });
        collection["links"] = json!([{"rel":"self","href":format!("https://example.invalid/{}/collection.json",family.id())}]);
        std::fs::write(
            root.path().join(family.id()).join("collection.json"),
            serde_json::to_vec(&collection).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.path().join("catalog.json"),
            serde_json::to_vec(&json!({"links":[{"rel":"child","href":format!("/{}/collection.json",family.id())}]})).unwrap(),
        )
        .unwrap();
        let server = Server::start_with_catalog_and_root(
            &[("DYNAMICAL_API_DATASET_CACHE_ENTRIES", "2")],
            None,
            Some(root.path().to_owned()),
        )
        .await;
        Self {
            root,
            family,
            a,
            server,
        }
    }
    pub async fn advance(&self) -> String {
        commit(self.root.path(), self.family, true).await
    }
}

fn dimensions(family: Family) -> Vec<&'static str> {
    let mut dimensions = if matches!(family, Family::Analysis) {
        vec!["time"]
    } else {
        vec!["init_time", "lead_time"]
    };
    if family.members(false).is_some() {
        dimensions.push("ensemble_member");
    }
    dimensions.extend(if matches!(family, Family::Lambert) {
        ["y", "x"]
    } else {
        ["latitude", "longitude"]
    });
    dimensions
}

async fn set(store: &Store, key: &str, value: Value) {
    store
        .set(key, serde_json::to_vec(&value).unwrap().into())
        .await
        .unwrap();
}
async fn array(
    store: &Store,
    name: &str,
    dtype: &str,
    dimensions: &[&str],
    shape: &[usize],
    attributes: Value,
    data: Vec<u8>,
) {
    set(store, &format!("{name}/zarr.json"), json!({
        "zarr_format":3,"node_type":"array","shape":shape,"data_type":dtype,
        "dimension_names":dimensions,"chunk_grid":{"name":"regular","configuration":{"chunk_shape":shape}},
        "chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},
        "fill_value":if dtype=="float64" {json!("NaN")} else {json!(0)},
        "codecs":[{"name":"bytes","configuration":{"endian":"little"}}],"attributes":attributes
    })).await;
    let suffix = if shape.is_empty() {
        String::new()
    } else {
        format!("/{}", vec!["0"; shape.len()].join("/"))
    };
    store
        .set(&format!("{name}/c{suffix}"), data.into())
        .await
        .unwrap();
}

async fn commit(root: &Path, family: Family, second: bool) -> String {
    let repository = Repository::open(
        None,
        icechunk::new_local_filesystem_storage(&root.join(family.id()).join("repository"))
            .await
            .unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let session = repository.writable_session("main").await.unwrap();
    let store = Store::from_session(Arc::new(tokio::sync::RwLock::new(session))).await;
    set(
        &store,
        "zarr.json",
        json!({"zarr_format":3,"node_type":"group","attributes":{"coordinates":"spatial_ref"}}),
    )
    .await;
    let projected = matches!(family, Family::Lambert);
    let spatial = if projected { 3 } else { 2 };
    for (name, values, units) in if projected {
        [
            ("y", vec![3000., 0., -3000.], "m"),
            ("x", vec![-3000., 0., 3000.], "m"),
        ]
    } else {
        [
            ("latitude", vec![44., 42.], "degrees_north"),
            ("longitude", vec![0., 60.], "degrees_east"),
        ]
    } {
        array(
            &store,
            name,
            "float64",
            &[name],
            &[spatial],
            json!({"units":units}),
            values.into_iter().flat_map(f64::to_le_bytes).collect(),
        )
        .await;
    }
    let wkts: Value = serde_json::from_str(include_str!(
        "../../../dynapi-dataset/tests/fixtures/live-crs.json"
    ))
    .unwrap();
    let wkt = &wkts[if projected {
        "lambert-conformal"
    } else {
        "wgs84"
    }];
    array(
        &store,
        "spatial_ref",
        "int64",
        &[],
        &[],
        json!({"crs_wkt":wkt}),
        0i64.to_le_bytes().to_vec(),
    )
    .await;
    let size = if second { 3 } else { 2 };
    let analysis = matches!(family, Family::Analysis);
    let epoch_attributes =
        json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"});
    if analysis {
        array(
            &store,
            "time",
            "int64",
            &["time"],
            &[size],
            epoch_attributes,
            (0..size)
                .flat_map(|t| (1_785_369_600i64 + 3600 * t as i64).to_le_bytes())
                .collect(),
        )
        .await;
    } else {
        array(
            &store,
            "init_time",
            "int64",
            &["init_time"],
            &[size],
            epoch_attributes,
            (0..size)
                .flat_map(|r| (1_785_369_600i64 + 21600 * r as i64).to_le_bytes())
                .collect(),
        )
        .await;
        array(
            &store,
            "lead_time",
            "float64",
            &["lead_time"],
            &[size],
            json!({"units":"seconds","dtype":"timedelta64[us]","_FillValue":"AAAAAAAA+H8="}),
            (0..size)
                .flat_map(|t| (3600. * t as f64).to_le_bytes())
                .collect(),
        )
        .await;
    }
    if let Some(ids) = family.members(second) {
        array(
            &store,
            "ensemble_member",
            "int64",
            &["ensemble_member"],
            &[ids.len()],
            json!({}),
            ids.iter().flat_map(|id| id.to_le_bytes()).collect(),
        )
        .await;
    }
    let dimensions = dimensions(family);
    let shape: Vec<_> = dimensions
        .iter()
        .map(|d| match *d {
            "init_time" | "lead_time" | "time" => size,
            "ensemble_member" => family.members(second).unwrap().len(),
            _ => spatial,
        })
        .collect();
    let mut counts = Vec::new();
    let mut humidity = Vec::new();
    for run in 0..if analysis { 1 } else { size } {
        for time in 0..size {
            for member in 0..family.members(second).map_or(1, <[i64]>::len) {
                for cell in 0..spatial * spatial {
                    let count = if second { 2000 } else { 1000 }
                        + 100 * run as i64
                        + 1000 * member as i64
                        + 10 * time as i64
                        + cell as i64;
                    counts.extend(count.to_le_bytes());
                    humidity.extend(
                        (50. + 10. * run as f64 + member as f64 + time as f64 + cell as f64)
                            .to_le_bytes(),
                    );
                }
            }
        }
    }
    array(
        &store,
        "count",
        "int64",
        &dimensions,
        &shape,
        json!({}),
        counts,
    )
    .await;
    if second {
        array(
            &store,
            "humidity",
            "float64",
            &dimensions,
            &shape,
            json!({"units":"%"}),
            humidity,
        )
        .await;
    }
    store
        .session()
        .write()
        .await
        .commit(if second {
            "Synthetic family history B"
        } else {
            "Synthetic family history A"
        })
        .execute()
        .await
        .unwrap()
        .to_string()
}
