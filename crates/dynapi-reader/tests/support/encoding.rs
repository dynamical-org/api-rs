#![allow(dead_code)]
use dynapi_reader::{DatasetVersion, ReadSession, StoreDescriptor};
use icechunk::Store;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

pub struct Fixture {
    pub path: PathBuf,
    pub store: Store,
}
impl Fixture {
    pub async fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "dynapi-encoding-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = icechunk::Repository::create(
            None,
            icechunk::new_local_filesystem_storage(&path).await.unwrap(),
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
        Self { path, store }
    }
    pub async fn array(&self, name: &str, metadata: &Value, payload: Option<Vec<u8>>) {
        if let Some((parent, _)) = name.rsplit_once('/') {
            self.store
                .set(
                    &format!("{parent}/zarr.json"),
                    br#"{"zarr_format":3,"node_type":"group","attributes":{}}"#
                        .to_vec()
                        .into(),
                )
                .await
                .unwrap();
        }
        self.store
            .set(
                &format!("{name}/zarr.json"),
                serde_json::to_vec(metadata).unwrap().into(),
            )
            .await
            .unwrap();
        if let Some(payload) = payload {
            let suffix = if metadata["shape"].as_array().unwrap().is_empty() {
                "c"
            } else {
                "c/0"
            };
            self.store
                .set(&format!("{name}/{suffix}"), payload.into())
                .await
                .unwrap();
        }
    }
    pub async fn reader(&self) -> ReadSession {
        let snapshot = self
            .store
            .session()
            .write()
            .await
            .commit("Synthetic encoding recipe")
            .execute()
            .await
            .unwrap()
            .to_string();
        ReadSession::open(
            &StoreDescriptor::Local(self.path.to_str().unwrap().into()),
            DatasetVersion::Snapshot(&snapshot),
            &[],
        )
        .await
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).unwrap();
    }
}

pub fn metadata(
    dtype: &str,
    shape: &[usize],
    chunk: &[usize],
    dimensions: &[&str],
    fill: Value,
    codecs: Value,
    attributes: Value,
) -> Value {
    json!({"zarr_format":3,"node_type":"array","shape":shape,"data_type":dtype,"dimension_names":dimensions,
        "chunk_grid":{"name":"regular","configuration":{"chunk_shape":chunk}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},
        "fill_value":fill,"codecs":codecs,"attributes":attributes})
}
/// RFC 8878: ordinary single-segment frame and one final raw block.
pub fn raw_zstd(input: &[u8]) -> Vec<u8> {
    assert!(input.len() < 256);
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, input.len() as u8];
    frame.extend_from_slice(&((input.len() as u32 * 8) | 1).to_le_bytes()[..3]);
    frame.extend_from_slice(input);
    frame
}

pub fn input_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../dynapi-reader/tests/fixtures/encoding-breadth")
}
pub fn inputs() -> Value {
    serde_json::from_slice(&std::fs::read(input_root().join("inputs.json")).unwrap()).unwrap()
}
pub fn input(name: &str) -> Vec<u8> {
    std::fs::read(input_root().join(name)).unwrap()
}
pub fn memcpy(raw: &[u8], width: u8) -> Vec<u8> {
    let mut frame = vec![2, 1, 2, width];
    frame.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(raw.len() as u32 + 16).to_le_bytes());
    frame.extend_from_slice(raw);
    frame
}
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
pub fn shard(payloads: &[Option<Vec<u8>>], start: bool) -> Vec<u8> {
    let index_len = payloads.len() * 16 + 4;
    let mut offset = if start { index_len as u64 } else { 0 };
    let mut index = Vec::new();
    let mut data = Vec::new();
    for payload in payloads {
        let (at, len) = if let Some(payload) = payload {
            let at = offset;
            offset += payload.len() as u64;
            data.extend_from_slice(payload);
            (at, payload.len() as u64)
        } else {
            (u64::MAX, u64::MAX)
        };
        index.extend_from_slice(&at.to_le_bytes());
        index.extend_from_slice(&len.to_le_bytes());
    }
    index.extend_from_slice(&crc32c(&index).to_le_bytes());
    if start {
        [index, data].concat()
    } else {
        [data, index].concat()
    }
}
pub fn bytes_codec() -> Value {
    json!({"name":"bytes","configuration":{"endian":"little"}})
}
pub fn blosc_codec(width: usize, shuffle: &str, level: u8, cname: &str) -> Value {
    json!({"name":"blosc","configuration":{"typesize":width,"cname":cname,"clevel":level,"shuffle":shuffle,"blocksize":0}})
}
pub fn shard_codec(inner: &[usize], codecs: Value, start: bool) -> Value {
    json!([{"name":"sharding_indexed","configuration":{"chunk_shape":inner,"codecs":codecs,"index_codecs":[bytes_codec(),{"name":"crc32c"}],"index_location":if start {"start"} else {"end"}}}])
}
