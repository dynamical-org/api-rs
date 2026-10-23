#![cfg(feature = "harness")]
//! HTTP values derive from the committed integer input recipe, not a decoder.
#[path = "../../dynapi-reader/tests/support/encoding.rs"]
mod encoding;
mod support;
use encoding::{
    Fixture, blosc_codec, bytes_codec, input, inputs, memcpy, metadata, raw_zstd, shard,
    shard_codec,
};
use serde_json::{Value, json};
use std::path::Path;
use support::{Server, input as dataset_input};

struct Encoding {
    name: String,
    codecs: Value,
    payload: Vec<u8>,
    chunk: usize,
    rank_one: bool,
}
fn cases(spec: &Value) -> Vec<Encoding> {
    let width = spec["width"].as_u64().unwrap() as usize;
    let raw = input(spec["raw"].as_str().unwrap());
    let mut out = vec![Encoding {
        name: "bytes".into(),
        codecs: json!([bytes_codec()]),
        payload: raw.clone(),
        chunk: 256,
        rank_one: false,
    }];
    if width == 1 {
        for (name, codec) in [
            ("bytes_omitted", json!({"name":"bytes"})),
            ("bytes_empty", json!({"name":"bytes","configuration":{}})),
            (
                "bytes_null",
                json!({"name":"bytes","configuration":{"endian":null}}),
            ),
        ] {
            out.push(Encoding {
                name: name.into(),
                codecs: json!([codec]),
                payload: raw.clone(),
                chunk: 256,
                rank_one: false,
            });
        }
    }
    for shuffle in ["noshuffle", "shuffle", "bitshuffle"] {
        for level in 0..=9 {
            for copied in [false, true] {
                let key = if copied { "memcpy" } else { "encoded" };
                out.push(Encoding {
                    name: format!("zstd_{shuffle}_{level}_{key}"),
                    codecs: json!([bytes_codec(), blosc_codec(width, shuffle, level, "zstd")]),
                    payload: if copied {
                        memcpy(&raw, width as u8)
                    } else {
                        input(
                            spec["frames"][format!("{shuffle}_{level}")]
                                .as_str()
                                .unwrap(),
                        )
                    },
                    chunk: 256,
                    rank_one: false,
                });
            }
        }
    }
    if width == 4 {
        for copied in [false, true] {
            out.push(Encoding {
                name: format!("lz4_{}", if copied { "memcpy" } else { "encoded" }),
                codecs: json!([bytes_codec(), blosc_codec(width, "shuffle", 5, "lz4")]),
                payload: if copied {
                    memcpy(&raw, 4)
                } else {
                    input(spec["frames"]["lz4"].as_str().unwrap())
                },
                chunk: 256,
                rank_one: false,
            });
        }
    }
    let base_len = out.len();
    for index in 0..base_len {
        for start in [true, false] {
            let base = &out[index];
            out.push(Encoding {
                name: format!(
                    "{}_index_{}",
                    base.name,
                    if start { "start" } else { "end" }
                ),
                codecs: shard_codec(&[256, 1, 1], base.codecs.clone(), start),
                payload: shard(&[Some(base.payload.clone()), None], start),
                chunk: 512,
                rank_one: false,
            });
        }
    }
    if spec["dtype"] == "int16" {
        out.push(Encoding {name:"standalone_zstd".into(),codecs:json!([bytes_codec(),{"name":"zstd","configuration":{"level":0,"checksum":false}}]),payload:raw_zstd(&raw[..128]),chunk:64,rank_one:true});
    }
    out
}
fn copy(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}
async fn coordinate(fixture: &Fixture, name: &str, values: &[i64], attrs: Value) {
    fixture
        .array(
            name,
            &metadata(
                "int64",
                &[values.len()],
                &[values.len()],
                &[name],
                json!(0),
                json!([bytes_codec()]),
                attrs,
            ),
            Some(values.iter().flat_map(|n| n.to_le_bytes()).collect()),
        )
        .await;
}
async fn product(
    root: &Path,
    spec: &Value,
    forecast: bool,
    encodings: &[Encoding],
    group: usize,
) -> (Value, Vec<String>) {
    let dtype = spec["dtype"].as_str().unwrap();
    let kind = if forecast { "forecast" } else { "analysis" };
    let id = format!("fixture-{kind}-{dtype}-{group:02}");
    let fixture = Fixture::new().await;
    let time = if forecast { "lead_time" } else { "time" };
    coordinate(&fixture, "latitude", &[44], json!({})).await;
    coordinate(&fixture, "longitude", &[0], json!({})).await;
    if forecast {
        coordinate(
            &fixture,
            "init_time",
            &[1_785_369_600],
            json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"}),
        )
        .await;
        let seconds: Vec<u8> = (0..7)
            .flat_map(|i| (i as f64 * 3600.).to_le_bytes())
            .collect();
        fixture.array(time,&metadata("float64",&[7],&[7],&[time],json!("NaN"),json!([bytes_codec()]),json!({"units":"seconds","dtype":"timedelta64[us]","_FillValue":"AAAAAAAA+H8="})),Some(seconds)).await;
    } else {
        coordinate(
            &fixture,
            time,
            &(0..7).map(|i| 1_785_369_600 + i * 3600).collect::<Vec<_>>(),
            json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"}),
        )
        .await;
    }
    let mut variables = serde_json::Map::new();
    let mut names = Vec::new();
    if forecast {
        let anchor = metadata(
            "int64",
            &[1, 7, 1, 1],
            &[1, 7, 1, 1],
            &["init_time", "lead_time", "latitude", "longitude"],
            json!(0),
            json!([bytes_codec()]),
            json!({}),
        );
        fixture.array("anchor", &anchor, None).await;
        fixture
            .store
            .set(
                "anchor/c/0/0/0/0",
                (0i64..7)
                    .flat_map(i64::to_le_bytes)
                    .collect::<Vec<_>>()
                    .into(),
            )
            .await
            .unwrap();
        variables.insert("anchor".into(),json!({"type":"data","dimensions":["init_time","lead_time","latitude","longitude"],"unit":null}));
    }
    for encoding in encodings {
        for missing in [false, true] {
            let name = format!(
                "{}_{}",
                encoding.name,
                if missing { "missing" } else { "stored" }
            );
            let dims = if encoding.rank_one {
                vec![time]
            } else {
                vec![time, "latitude", "longitude"]
            };
            let shape = if encoding.rank_one {
                vec![7]
            } else {
                vec![7, 1, 1]
            };
            let chunk = if encoding.rank_one {
                vec![encoding.chunk]
            } else {
                vec![encoding.chunk, 1, 1]
            };
            let meta = metadata(
                dtype,
                &shape,
                &chunk,
                &dims,
                spec["fill"].clone(),
                encoding.codecs.clone(),
                json!({}),
            );
            // Fixture's array helper uses a rank-one default key; weather arrays use C-order 3D keys.
            fixture.array(&name, &meta, None).await;
            if !missing {
                let key = if encoding.rank_one {
                    format!("{name}/c/0")
                } else {
                    format!("{name}/c/0/0/0")
                };
                fixture
                    .store
                    .set(&key, encoding.payload.clone().into())
                    .await
                    .unwrap();
            }
            variables.insert(
                name.clone(),
                json!({"type":"data","dimensions":dims,"unit":null}),
            );
            names.push(name);
        }
    }
    let _snapshot = fixture.reader().await.snapshot_id().to_owned();
    copy(&fixture.path, &root.join(&id).join("repository"));
    let mut collection = dataset_input(kind)["collection"].clone();
    collection["id"] = json!(id);
    collection["assets"]["icechunk"]["href"] =
        json!(format!("s3://synthetic-fixture/{id}/v1.2.3.icechunk/"));
    collection["cube:variables"] = Value::Object(variables);
    std::fs::write(
        root.join(&id).join("collection.json"),
        serde_json::to_vec(&collection).unwrap(),
    )
    .unwrap();
    (collection, names)
}

#[tokio::test]
async fn every_integer_width_codec_level_shuffle_frame_and_index_location_has_exact_http_values_and_fill()
 {
    let root = tempfile::tempdir().unwrap();
    let inputs = inputs();
    let mut products = Vec::new();
    for spec in inputs["integers"].as_array().unwrap() {
        for forecast in [false, true] {
            let encodings = cases(spec);
            // Small products keep the exhaustive table's sibling scans bounded.
            for (group, encodings) in encodings.chunks(24).enumerate() {
                let (collection, names) =
                    product(root.path(), spec, forecast, encodings, group).await;
                products.push((collection, names, spec.clone(), forecast));
            }
        }
    }
    let links:Vec<_>=products.iter().map(|(c,_,_,_)|json!({"rel":"child","href":format!("/{}/collection.json",c["id"].as_str().unwrap())})).collect();
    std::fs::write(
        root.path().join("catalog.json"),
        serde_json::to_vec(&json!({"links":links})).unwrap(),
    )
    .unwrap();
    let server = Server::start_with_catalog_and_root(
        &[
            ("DYNAMICAL_API_QUERY_RATE_LIMIT_BURST", "100000"),
            ("DYNAMICAL_API_RATE_LIMIT_BURST", "100000"),
        ],
        None,
        Some(root.path().to_owned()),
    )
    .await;
    let mut asserted_variables = 0;
    for (collection, names, spec, forecast) in products {
        let path = if forecast {
            "/v1/forecasts"
        } else {
            "/v1/analyses"
        };
        for names in names.chunks(if forecast { 24 } else { 25 }) {
            let mut query = json!({"dataProductId":collection["id"],"location":{"latitude":44,"longitude":0},"variables":names});
            if forecast {
                query["variables"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("anchor"));
                query["initTime"] = json!("2026-07-30T00:00:00Z");
            } else {
                query["startTime"] = json!("2026-07-30T00:00:00Z");
                query["endTime"] = json!("2026-07-30T07:00:00Z");
            }
            let reply = server
                .request("POST", path, &json!({"queries":[query]}))
                .await;
            assert_eq!(
                reply.status,
                200,
                "{} {path}: {}",
                collection["id"],
                String::from_utf8_lossy(&reply.body)
            );
            let result = reply.json()["results"][0].clone();
            let selected = if forecast {
                &result["forecasts"][0]
            } else {
                &result
            };
            let times: Vec<_> = (0..7)
                .map(|i| format!("2026-07-30T{i:02}:00:00Z"))
                .collect();
            assert_eq!(
                selected[if forecast {
                    "validTimes"
                } else {
                    "analysisTimes"
                }],
                json!(times)
            );
            let canonical = server
                .request(
                    "GET",
                    selected["links"]["canonical"].as_str().unwrap(),
                    &Value::Null,
                )
                .await;
            assert_eq!(
                canonical.status,
                200,
                "{}",
                String::from_utf8_lossy(&canonical.body)
            );
            let canonical = canonical.json();
            let canonical = if forecast {
                &canonical["forecasts"][0]
            } else {
                &canonical
            };
            assert_eq!(canonical["data"], selected["data"]);
            if forecast {
                assert_eq!(selected["data"]["anchor"], json!([0, 1, 2, 3, 4, 5, 6]));
            }
            for name in names {
                let expected = if name.ends_with("_missing") {
                    json!(vec![spec["fill"].as_i64().unwrap(); 7])
                } else {
                    spec["values"].clone()
                };
                assert_eq!(
                    selected["data"][name], expected,
                    "{} {name}",
                    collection["id"]
                );
                assert!(
                    selected["data"][name]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|n| n.as_i64().is_some()),
                    "integer presentation {} {name}",
                    collection["id"]
                );
                assert_eq!(result["variables"][name], json!({"unit":null}));
                asserted_variables += 1;
            }
        }
    }
    // Two data families, every codec/layout above, with stored and absent payloads.
    assert_eq!(asserted_variables, 5248);
}
