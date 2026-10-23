//! Synthetic physical inputs. See ensemble_http.md for dimensions and values.
use crate::support::{Server, input};
use icechunk::{Repository, store::Store};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct Fixture {
    pub server: Server,
    pub snapshot: String,
    pub id: String,
    _root: tempfile::TempDir,
}

#[derive(Clone, Copy)]
pub enum Members {
    Bytes(&'static str, &'static [i64]),
    Blosc(&'static str, &'static [i64], usize),
    Zstd,
    Strings,
}
impl Members {
    pub fn dtype(self) -> &'static str {
        match self {
            Self::Bytes(dtype, _) | Self::Blosc(dtype, _, _) => dtype,
            Self::Zstd => "int16",
            Self::Strings => "string",
        }
    }
    pub fn ids(self) -> &'static [i64] {
        match self {
            Self::Bytes(_, ids) | Self::Blosc(_, ids, _) => ids,
            Self::Zstd | Self::Strings => &[7, -2, 7],
        }
    }
}

fn width(dtype: &str) -> usize {
    match dtype {
        "int8" | "uint8" => 1,
        "int16" | "uint16" => 2,
        "int32" | "uint32" | "float32" => 4,
        "int64" | "float64" => 8,
        _ => panic!("fixture dtype"),
    }
}
fn bytes_codec() -> Value {
    json!({"name":"bytes","configuration":{"endian":"little"}})
}
fn blosc_codec(typesize: usize) -> Value {
    json!({"name":"blosc","configuration":{"cname":"zstd","clevel":3,"shuffle":"shuffle","typesize":typesize,"blocksize":0}})
}
fn zstd(raw: &[u8]) -> Vec<u8> {
    // RFC8878: one single-segment frame, explicit content size, final raw block.
    assert!(raw.len() < 65792);
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd];
    if raw.len() < 256 {
        out.extend([0x20, raw.len() as u8]);
    } else {
        out.push(0x60);
        out.extend(((raw.len() - 256) as u16).to_le_bytes());
    }
    out.extend(&((raw.len() as u32 * 8) | 1).to_le_bytes()[..3]);
    out.extend(raw);
    out
}
fn blosc(raw: &[u8], typesize: usize) -> Vec<u8> {
    // Blosc v1 compressed mode, one unsplit Zstd block and byte shuffle. The
    // final partial item is copied verbatim, including the int16/typesize4 tail.
    let mut shuffled = Vec::with_capacity(raw.len());
    for byte in 0..typesize {
        for item in raw.chunks_exact(typesize) {
            shuffled.push(item[byte]);
        }
    }
    shuffled.extend(raw.chunks_exact(typesize).remainder());
    let payload = zstd(&shuffled);
    let mut frame = vec![2, 1, 0x91, typesize as u8];
    frame.extend((raw.len() as u32).to_le_bytes());
    frame.extend((raw.len() as u32).to_le_bytes());
    frame.extend((24u32 + payload.len() as u32).to_le_bytes());
    frame.extend(20u32.to_le_bytes());
    frame.extend((payload.len() as u32).to_le_bytes());
    frame.extend(payload);
    frame
}
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn coordinates(mut flat: usize, shape: &[usize]) -> Vec<usize> {
    let mut out = vec![0; shape.len()];
    for axis in (0..shape.len()).rev() {
        out[axis] = flat % shape[axis];
        flat /= shape[axis];
    }
    out
}
async fn set(store: &Store, key: &str, bytes: Vec<u8>) {
    store.set(key, bytes.into()).await.unwrap();
}
fn metadata(dims: &[&str], shape: &[usize], chunk: &[usize], dtype: &str, codecs: Value) -> Value {
    json!({"zarr_format":3,"node_type":"array","shape":shape,"data_type":dtype,"dimension_names":dims,
        "chunk_grid":{"name":"regular","configuration":{"chunk_shape":chunk}},
        "chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},
        "fill_value":if dtype.starts_with("float") {json!("NaN")} else {json!(0)},"codecs":codecs,"attributes":{}})
}
async fn coordinate(
    store: &Store,
    name: &str,
    dtype: &str,
    raw: Vec<u8>,
    size: usize,
    attrs: Value,
) {
    let mut meta = metadata(&[name], &[size], &[size], dtype, json!([bytes_codec()]));
    meta["attributes"] = attrs;
    set(
        store,
        &format!("{name}/zarr.json"),
        serde_json::to_vec(&meta).unwrap(),
    )
    .await;
    set(store, &format!("{name}/c/0"), raw).await;
}

async fn field(
    store: &Store,
    name: &str,
    dims: &[&str],
    dtype: &str,
    sharded: bool,
    byte_header: bool,
) {
    let shape: Vec<_> = dims
        .iter()
        .map(|dim| match *dim {
            "init_time" | "latitude" | "longitude" => 2,
            "lead_time" | "ensemble_member" => 3,
            _ => unreachable!(),
        })
        .collect();
    let inner: Vec<_> = dims
        .iter()
        .map(|dim| if *dim == "init_time" { 1 } else { 2 })
        .collect();
    let factors: Vec<_> = dims
        .iter()
        .map(|dim| {
            if sharded && matches!(*dim, "ensemble_member" | "lead_time") {
                2
            } else {
                1
            }
        })
        .collect();
    let outer: Vec<_> = inner.iter().zip(&factors).map(|(a, b)| a * b).collect();
    let header = if byte_header { 1 } else { width(dtype) };
    let codecs = json!([bytes_codec(), blosc_codec(width(dtype))]);
    let codecs = if sharded {
        json!([{"name":"sharding_indexed","configuration":{"chunk_shape":inner,"codecs":codecs,
            "index_codecs":[bytes_codec(),{"name":"crc32c"}],"index_location":"end"}}])
    } else {
        codecs
    };
    let meta = metadata(dims, &shape, &outer, dtype, codecs);
    set(
        store,
        &format!("{name}/zarr.json"),
        serde_json::to_vec(&meta).unwrap(),
    )
    .await;
    let grid: Vec<_> = shape
        .iter()
        .zip(&outer)
        .map(|(a, b)| a.div_ceil(*b))
        .collect();
    for chunk in 0..grid.iter().product() {
        let chunk = coordinates(chunk, &grid);
        let mut payload = Vec::new();
        let mut index = Vec::new();
        for inner_flat in 0..factors.iter().product() {
            let sub = coordinates(inner_flat, &factors);
            let mut raw = Vec::new();
            for flat in 0..inner.iter().product() {
                let local = coordinates(flat, &inner);
                let source: Vec<_> = (0..dims.len())
                    .map(|i| chunk[i] * outer[i] + sub[i] * inner[i] + local[i])
                    .collect();
                let inside = source.iter().zip(&shape).all(|(i, n)| i < n);
                let pos = |dim: &str| dims.iter().position(|d| *d == dim).map_or(0, |i| source[i]);
                let signal = 10000 * pos("init_time")
                    + 1000 * pos("lead_time")
                    + 100 * pos("ensemble_member")
                    + 10 * pos("latitude")
                    + pos("longitude");
                match dtype {
                    "float32" => raw.extend(if inside {
                        (signal as f32 + if name == "temperature" { 0.25 } else { 0.0 })
                            .to_le_bytes()
                    } else {
                        f32::NAN.to_le_bytes()
                    }),
                    "float64" => raw.extend(if inside {
                        ((if name == "orography" {
                            100 + 10 * pos("latitude") + pos("longitude")
                        } else {
                            signal
                        }) as f64
                            + if name == "wind" { 0.5 } else { 0.0 })
                        .to_le_bytes()
                    } else {
                        f64::NAN.to_le_bytes()
                    }),
                    "int64" => raw.extend(if inside {
                        (9_007_199_254_740_993i64 + signal as i64).to_le_bytes()
                    } else {
                        0i64.to_le_bytes()
                    }),
                    _ => unreachable!(),
                }
            }
            let encoded = blosc(&raw, header);
            index.extend((payload.len() as u64).to_le_bytes());
            index.extend((encoded.len() as u64).to_le_bytes());
            payload.extend(encoded);
        }
        if sharded {
            let crc = crc32c(&index);
            payload.extend(index);
            payload.extend(crc.to_le_bytes());
        }
        let key = chunk
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("/");
        set(store, &format!("{name}/c/{key}"), payload).await;
    }
}

pub async fn fixture(members: Members, sharded: bool, byte_header: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let id = "fixture-ensemble-http".to_owned();
    let path = root.path().join(&id);
    std::fs::create_dir(&path).unwrap();
    let storage = icechunk::new_local_filesystem_storage(&path.join("repository"))
        .await
        .unwrap();
    let repo = Repository::create(None, storage, Default::default(), None, false)
        .await
        .unwrap();
    let session = repo.writable_session("main").await.unwrap();
    let store = Store::from_session(Arc::new(RwLock::new(session))).await;
    set(
        &store,
        "zarr.json",
        serde_json::to_vec(&json!({"zarr_format":3,"node_type":"group","attributes":{}})).unwrap(),
    )
    .await;
    coordinate(
        &store,
        "init_time",
        "int64",
        [1785369600i64, 1785391200]
            .into_iter()
            .flat_map(i64::to_le_bytes)
            .collect(),
        2,
        json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"}),
    )
    .await;
    coordinate(
        &store,
        "lead_time",
        "float64",
        [0f64, 21600., 43200.]
            .into_iter()
            .flat_map(f64::to_le_bytes)
            .collect(),
        3,
        json!({"units":"seconds","dtype":"timedelta64[us]","_FillValue":"AAAAAAAA+H8="}),
    )
    .await;
    coordinate(
        &store,
        "latitude",
        "float64",
        [44f64, 40.]
            .into_iter()
            .flat_map(f64::to_le_bytes)
            .collect(),
        2,
        json!({}),
    )
    .await;
    coordinate(
        &store,
        "longitude",
        "float64",
        [-103f64, -99.]
            .into_iter()
            .flat_map(f64::to_le_bytes)
            .collect(),
        2,
        json!({}),
    )
    .await;
    let member_chunk = if matches!(members, Members::Blosc("int16", _, 4) | Members::Zstd) {
        51
    } else {
        3
    };
    let mut raw = Vec::new();
    for &value in members
        .ids()
        .iter()
        .chain(std::iter::repeat_n(&0, member_chunk - 3))
    {
        raw.extend(
            &value.to_le_bytes()[..width(if matches!(members, Members::Strings) {
                "int16"
            } else {
                members.dtype()
            })],
        );
    }
    let codecs = match members {
        Members::Bytes(_, _) => json!([bytes_codec()]),
        Members::Blosc(_, _, size) => {
            raw = blosc(&raw, size);
            json!([bytes_codec(), blosc_codec(size)])
        }
        Members::Zstd => {
            raw = zstd(&raw);
            json!([bytes_codec(),{"name":"zstd","configuration":{"level":0,"checksum":false}}])
        }
        Members::Strings => {
            raw.clear();
            raw.extend(3u32.to_le_bytes());
            for value in ["control", "perturbed", "control"] {
                raw.extend((value.len() as u32).to_le_bytes());
                raw.extend(value.as_bytes());
            }
            json!([{"name":"vlen-utf8"}])
        }
    };
    let mut meta = metadata(
        &["ensemble_member"],
        &[3],
        &[member_chunk],
        members.dtype(),
        codecs,
    );
    if matches!(members, Members::Strings) {
        meta["fill_value"] = json!("");
    }
    set(
        &store,
        "ensemble_member/zarr.json",
        serde_json::to_vec(&meta).unwrap(),
    )
    .await;
    set(&store, "ensemble_member/c/0", raw).await;
    let recipes: &[(&str, &str, &[&str], &str)] = &[
        (
            "temperature",
            "float32",
            &[
                "init_time",
                "lead_time",
                "ensemble_member",
                "latitude",
                "longitude",
            ],
            "K",
        ),
        (
            "wind",
            "float64",
            &[
                "longitude",
                "lead_time",
                "latitude",
                "init_time",
                "ensemble_member",
            ],
            "m s-1",
        ),
        (
            "count",
            "int64",
            &[
                "ensemble_member",
                "longitude",
                "init_time",
                "latitude",
                "lead_time",
            ],
            "",
        ),
        (
            "deterministic",
            "float32",
            &["longitude", "init_time", "latitude", "lead_time"],
            "K",
        ),
        ("orography", "float64", &["latitude", "longitude"], "m"),
    ];
    let mut collection = input("ensemble")["collection"].clone();
    collection["id"] = json!(id);
    collection["cube:variables"] = json!({});
    for &(name, dtype, dims, unit) in recipes {
        field(
            &store,
            name,
            dims,
            dtype,
            sharded && dims.contains(&"ensemble_member"),
            byte_header,
        )
        .await;
        collection["cube:variables"][name] = json!({"type":"data","dimensions":dims,"unit":if unit.is_empty(){Value::Null}else{json!(unit)}});
    }
    let snapshot = store
        .session()
        .write()
        .await
        .commit("Synthetic ensemble input")
        .execute()
        .await
        .unwrap()
        .to_string();
    std::fs::write(
        path.join("collection.json"),
        serde_json::to_vec(&collection).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.path().join("catalog.json"),
        serde_json::to_vec(
            &json!({"links":[{"rel":"child","href":format!("/{id}/collection.json")}]}),
        )
        .unwrap(),
    )
    .unwrap();
    let server = Server::start_with_catalog_and_root(&[], None, Some(root.path().to_owned())).await;
    Fixture {
        server,
        snapshot,
        id,
        _root: root,
    }
}
