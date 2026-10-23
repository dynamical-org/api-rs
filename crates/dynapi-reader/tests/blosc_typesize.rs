//! Constructed Blosc v1 frames, without captured service or catalog payloads.
use dynapi_reader::{DatasetVersion, ErrorKind, ReadSession, Selector, StoreDescriptor};
use icechunk::{Repository, store::Store};
use serde_json::json;
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::RwLock;
use zarrs::array::{BytesToBytesCodecTraits, CodecOptions, codec::BloscCodec};
use zarrs::metadata_ext::codec::blosc::BloscCodecConfiguration;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "dynapi-blosc-typesize-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/http/repository"),
            &path,
        );
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
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
// Blosc v1: a 16-byte header, one absolute block offset, one compressed-size
// prefix and a Zstd block. Bit 4 disables splitting; bit 0 enables byte shuffle.
// Shuffle transposes the byte position within each header-sized item. For
// typesize 1 it is the identity, independently of the array's numeric dtype.
fn frame(raw: &[u8], typesize: u8, shuffle: &str) -> Vec<u8> {
    if shuffle == "bitshuffle" {
        // The existing pinned encoder constructs the bit-plane permutation.
        // Expectations below remain the explicit input numeric bytes; header
        // typesize 1 bitshuffle is not an identity transform.
        let config: BloscCodecConfiguration = serde_json::from_value(json!({
            "cname":"zstd","clevel":3,"shuffle":"bitshuffle","typesize":typesize,"blocksize":0
        }))
        .unwrap();
        let codec = BloscCodec::new_with_configuration(&config).unwrap();
        let encoded = codec
            .encode(Cow::Borrowed(raw), &CodecOptions::default())
            .unwrap()
            .into_owned();
        assert_eq!(
            encoded[2] & 0x06,
            0x04,
            "must exercise compressed bitshuffle, not memcpy"
        );
        assert_eq!(encoded[3], typesize);
        return encoded;
    }
    let mut shuffled = Vec::with_capacity(raw.len());
    if shuffle == "noshuffle" {
        shuffled.extend_from_slice(raw);
    } else {
        assert_eq!(shuffle, "shuffle");
        for byte in 0..usize::from(typesize) {
            for item in raw.chunks_exact(usize::from(typesize)) {
                shuffled.push(item[byte]);
            }
        }
    }
    let compressed = zstd::bulk::compress(&shuffled, 3).unwrap();
    let flags = if shuffle == "noshuffle" { 0x90 } else { 0x91 };
    let mut frame = vec![2, 1, flags, typesize];
    frame.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    frame.extend_from_slice(&((24 + compressed.len()) as u32).to_le_bytes());
    frame.extend_from_slice(&20u32.to_le_bytes());
    frame.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
    frame.extend_from_slice(&compressed);
    frame
}
fn shard(mut payload: Vec<u8>) -> Vec<u8> {
    let mut index = 0u64.to_le_bytes().to_vec();
    index.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    let mut crc = !0u32;
    for byte in &index {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    index.extend_from_slice(&(!crc).to_le_bytes());
    payload.extend_from_slice(&index);
    payload
}
async fn session(
    dtype: &str,
    width: usize,
    raw: &[u8],
    header: u8,
    metadata: u8,
    sharded: bool,
    shuffle: &str,
) -> (Fixture, ReadSession) {
    let fixture = Fixture::new();
    let storage = icechunk::new_local_filesystem_storage(&fixture.0)
        .await
        .unwrap();
    let repo = Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let writer = Arc::new(RwLock::new(repo.writable_session("main").await.unwrap()));
    let store = Store::from_session(writer.clone()).await;
    let count = raw.len() / width;
    let mut codecs = json!([
        {"name":"bytes","configuration":{"endian":"little"}},
        {"name":"blosc","configuration":{"cname":"zstd","clevel":3,"shuffle":shuffle,"typesize":metadata,"blocksize":0}}
    ]);
    let mut payload = frame(raw, header, shuffle);
    if sharded {
        codecs = json!([{"name":"sharding_indexed","configuration":{"chunk_shape":[count],"codecs":codecs,"index_codecs":[{"name":"bytes","configuration":{"endian":"little"}},{"name":"crc32c"}],"index_location":"end"}}]);
        payload = shard(payload);
    }
    let metadata = json!({"zarr_format":3,"node_type":"array","shape":[count],"data_type":dtype,"chunk_grid":{"name":"regular","configuration":{"chunk_shape":[count]}},"chunk_key_encoding":{"name":"default","configuration":{"separator":"/"}},"fill_value":0,"codecs":codecs,"attributes":{},"dimension_names":["values"],"storage_transformers":[]});
    store
        .set(
            "values/zarr.json",
            serde_json::to_vec(&metadata).unwrap().into(),
        )
        .await
        .unwrap();
    store.set("values/c/0", payload.into()).await.unwrap();
    let snapshot = writer
        .write()
        .await
        .commit("Synthetic byte-shuffled frame")
        .execute()
        .await
        .unwrap()
        .to_string();
    let reader = ReadSession::open(
        &StoreDescriptor::Local(fixture.0.to_str().unwrap().into()),
        DatasetVersion::Snapshot(&snapshot),
        &[],
    )
    .await
    .unwrap();
    (fixture, reader)
}
async fn numeric_header_reads(shuffle: &str) {
    for (dtype, width, raw) in [
        (
            "float32",
            4,
            (0..1024u16)
                .flat_map(|i| (f32::from(i) - 512.5).to_le_bytes())
                .collect::<Vec<_>>(),
        ),
        (
            "int64",
            8,
            (0..1024i64)
                .flat_map(|i| (9_007_199_254_740_993 + i).to_le_bytes())
                .collect::<Vec<_>>(),
        ),
    ] {
        for sharded in [false, true] {
            let (_fixture, reader) =
                session(dtype, width, &raw, 1, width as u8, sharded, shuffle).await;
            let indices = [1023usize, 0, 77, 77];
            let result = reader
                .read_dataset_selection(
                    "values",
                    &[Selector::Indices(indices.map(|i| i as i64).to_vec())],
                    None,
                )
                .await
                .unwrap();
            assert_eq!(result.dtype, dtype);
            assert_eq!(result.shape, [4]);
            assert_eq!(
                result.bytes,
                indices
                    .into_iter()
                    .flat_map(|i| raw[i * width..(i + 1) * width].iter().copied())
                    .collect::<Vec<_>>()
            );
            let full = reader
                .read_dataset_selection("values", &[Selector::Indices((0..1024).collect())], None)
                .await
                .unwrap();
            assert_eq!(full.bytes, raw);
        }
    }
}
#[tokio::test]
async fn byte_typesize_float32_and_int64_decode_header_shuffle_for_partial_and_full_reads() {
    numeric_header_reads("shuffle").await;
}
#[tokio::test]
async fn header_typesize_one_noshuffle_preserves_partial_and_full_numeric_bytes() {
    numeric_header_reads("noshuffle").await;
}
#[tokio::test]
async fn header_typesize_one_bitshuffle_preserves_partial_and_full_numeric_bytes() {
    numeric_header_reads("bitshuffle").await;
}
#[tokio::test]
async fn other_mismatches_and_byte_header_with_nonnative_metadata_stay_rejected() {
    for (dtype, width, header, metadata) in
        [("float32", 4, 2, 4), ("int64", 8, 4, 8), ("int16", 2, 1, 4)]
    {
        let raw = vec![0u8; width * 1024];
        for sharded in [false, true] {
            let (_fixture, reader) =
                session(dtype, width, &raw, header, metadata, sharded, "shuffle").await;
            let error = reader
                .read_dataset_selection("values", &[Selector::Index(0)], None)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Decode);
            assert!(
                error
                    .message
                    .contains("Blosc typesize differs from admitted metadata"),
                "{error}"
            );
        }
    }
}
