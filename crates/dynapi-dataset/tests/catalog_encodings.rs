//! Synthetic codec inputs and expectations from the Zarr/Zstd specifications.
use dynapi_dataset::{DatasetVersion, OpenedDataset, StoreDescriptor};
use dynapi_query::Coordinate;
use dynapi_reader::{ErrorKind, ReadSession, Selector};
use serde_json::{Value as Json, json};
mod common;

fn blosc_memcpy(input: &[u8], typesize: u8) -> Vec<u8> {
    // Blosc v2: memcpy flag, exact decoded/block/encoded lengths, then raw bytes.
    let mut frame = vec![2, 1, 2, typesize];
    frame.extend_from_slice(&(input.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(input.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(input.len() as u32 + 16).to_le_bytes());
    frame.extend_from_slice(input);
    frame
}

fn raw_zstd(input: &[u8]) -> Vec<u8> {
    // RFC 8878 single-segment frame, one-byte content size, one final raw block.
    assert!(input.len() < 256);
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, input.len() as u8];
    frame.extend_from_slice(&((input.len() as u32 * 8) | 1).to_le_bytes()[..3]);
    frame.extend_from_slice(input);
    frame
}

fn blosc() -> Json {
    json!({"name":"blosc","configuration":{"typesize":4,"cname":"lz4","clevel":5,"shuffle":"shuffle","blocksize":0}})
}

async fn read(
    meta: Json,
    data: Option<Vec<u8>>,
    selection: &[Selector],
) -> Result<dynapi_reader::TypedBuffer, dynapi_reader::ReaderError> {
    let temp = tempfile::tempdir().unwrap();
    common::repository(temp.path(), &[("data", meta, data)]).await;
    let session = ReadSession::open(
        &StoreDescriptor::Local(temp.path().to_str().unwrap().into()),
        DatasetVersion::Branch("main"),
        &[],
    )
    .await
    .unwrap();
    session
        .read_dataset_selection("data", selection, None)
        .await
}

#[tokio::test]
async fn leading_scale_offset_decodes_in_float32_before_selection_and_preserves_fill() {
    let mut meta = common::axis("data", "float32", 4, 4, json!({}));
    meta["codecs"] = json!([
        {"name":"scale_offset","configuration":{"scale":2,"offset":3}},
        {"name":"bytes","configuration":{"endian":"little"}},blosc()
    ]);
    let input: Vec<_> = [2.0f32, 4.0, 6.0, -2.0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let buffer = read(
        meta.clone(),
        Some(blosc_memcpy(&input, 4)),
        &[Selector::Indices(vec![3, 1, 3, 0])],
    )
    .await
    .unwrap();
    let expected: Vec<_> = [2.0f32, 5.0, 2.0, 4.0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    assert_eq!(buffer.dtype, "float32");
    assert_eq!(buffer.shape, [4]);
    assert_eq!(buffer.bytes, expected);
    let missing = read(meta.clone(), None, &[Selector::Index(2)])
        .await
        .unwrap();
    assert!(f32::from_le_bytes(missing.bytes.try_into().unwrap()).is_nan());
    let empty = read(meta, None, &[Selector::Indices(vec![])])
        .await
        .unwrap();
    assert!(empty.bytes.is_empty());

    // The scale is first rounded to float32: 1 / float32(.001) is 0x4479ffff,
    // whereas widening the division to float64 then casting produces 1000.
    let mut meta = common::axis("data", "float32", 1, 1, json!({}));
    meta["codecs"] = json!([{"name":"scale_offset","configuration":{"scale":0.001}},{"name":"bytes","configuration":{"endian":"little"}},blosc()]);
    let buffer = read(
        meta,
        Some(blosc_memcpy(&1.0f32.to_le_bytes(), 4)),
        &[Selector::Index(0)],
    )
    .await
    .unwrap();
    assert_eq!(buffer.bytes, 0x4479ffffu32.to_le_bytes());
}

#[tokio::test]
async fn scale_offset_rejects_unattested_dtype_parameters_and_chain_order() {
    for codec in [
        json!({"name":"scale_offset","configuration":{"scale":0}}),
        json!({"name":"scale_offset","configuration":{"scale":1e-100}}),
        json!({"name":"scale_offset","configuration":{"offset":1e100}}),
        json!({"name":"scale_offset","configuration":{"scale":"0.001"}}),
        json!({"name":"scale_offset","configuration":{"scale":2,"extra":0}}),
    ] {
        let mut meta = common::axis("data", "float32", 1, 1, json!({}));
        meta["codecs"] =
            json!([codec,{"name":"bytes","configuration":{"endian":"little"}},blosc()]);
        let error = read(meta, None, &[Selector::Indices(vec![])])
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
    }
    for dtype in ["int16", "float64"] {
        let mut meta = common::axis("data", dtype, 1, 1, json!({}));
        meta["codecs"] = json!([{"name":"scale_offset","configuration":{"offset":3}},{"name":"bytes","configuration":{"endian":"little"}}]);
        assert_eq!(
            read(meta, None, &[Selector::Index(0)])
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
    }
}

#[tokio::test]
async fn int16_standalone_zstd_members_open_with_exact_values_and_edge_padding() {
    for (size, chunk) in [(3, 4), (31, 31), (64, 64)] {
        let temp = tempfile::tempdir().unwrap();
        let mut meta = common::axis("ensemble_member", "int16", size, chunk, json!({}));
        meta["codecs"] = json!([{"name":"bytes","configuration":{"endian":"little"}},{"name":"zstd","configuration":{"level":0,"checksum":false}}]);
        let values: Vec<i16> = (0..chunk).map(|v| v as i16 - 1).collect();
        let bytes: Vec<_> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        common::repository(
            temp.path(),
            &[("ensemble_member", meta, Some(raw_zstd(&bytes)))],
        )
        .await;
        let ds = OpenedDataset::open(
            &StoreDescriptor::Local(temp.path().to_str().unwrap().into()),
            DatasetVersion::Branch("main"),
            &[],
        )
        .await
        .unwrap();
        let Coordinate::Members(members) = &ds.view().coordinates["ensemble_member"] else {
            panic!("members")
        };
        assert_eq!(
            members,
            &values[..size]
                .iter()
                .map(|&v| dynapi_contract::Value::from(i64::from(v)))
                .collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn standalone_zstd_frame_and_metadata_bounds_are_typed() {
    let mut meta = common::axis("data", "int16", 3, 3, json!({}));
    meta["codecs"] = json!([{"name":"bytes","configuration":{"endian":"little"}},{"name":"zstd","configuration":{"level":0,"checksum":false}}]);
    let input = [-32768i16, 0, 32767]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect::<Vec<_>>();
    let encoded = raw_zstd(&input);
    let got = read(
        meta.clone(),
        Some(encoded.clone()),
        &[Selector::Indices(vec![2, 0, 2])],
    )
    .await
    .unwrap();
    assert_eq!(
        got.bytes,
        [32767i16, -32768, 32767]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>()
    );
    for frame in [
        raw_zstd(&[0; 8]),
        [encoded.clone(), vec![0]].concat(),
        vec![0; 257],
        encoded[..encoded.len() - 1].to_vec(),
    ] {
        assert_eq!(
            read(meta.clone(), Some(frame), &[Selector::Index(0)])
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Decode
        );
    }
    meta["codecs"][1]["configuration"]["level"] = json!(1);
    let error = read(meta, None, &[Selector::Index(0)]).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(!error.message.to_lowercase().contains("blosc"));
}

#[tokio::test]
async fn unsupported_sibling_stays_lazy_and_fails_only_when_selected() {
    let temp = tempfile::tempdir().unwrap();
    let good = common::axis("axis", "float32", 2, 2, json!({}));
    let mut bad = good.clone();
    bad["codecs"] = json!([{"name":"bytes","configuration":{"endian":"little"}},{"name":"gzip","configuration":{"level":1}}]);
    common::repository(
        temp.path(),
        &[
            (
                "good",
                good,
                Some([1.0f32, 2.0].iter().flat_map(|v| v.to_le_bytes()).collect()),
            ),
            ("unsupported", bad, None),
        ],
    )
    .await;
    let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
    let ds = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    assert!(ds.view().variables.contains_key("unsupported"));
    let session = ReadSession::open(&descriptor, DatasetVersion::Snapshot(ds.snapshot_id()), &[])
        .await
        .unwrap();
    assert_eq!(
        session
            .read_dataset_selection("good", &[Selector::Index(1)], None)
            .await
            .unwrap()
            .bytes,
        2.0f32.to_le_bytes()
    );
    assert_eq!(
        session
            .read_dataset_selection("unsupported", &[Selector::Index(0)], None)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
}

#[tokio::test]
async fn deployed_scale_offset_values_and_frame_validation_are_exact() {
    for (configuration, input, expected) in [
        (
            json!({"offset":-273.15}),
            vec![273.15f32, 274.15],
            vec![0.0f32, 1.0],
        ),
        (
            json!({"scale":9.80665}),
            vec![9.80665f32, 19.6133],
            vec![1.0f32, 2.0],
        ),
    ] {
        let mut meta = common::axis("data", "float32", 2, 2, json!({}));
        meta["codecs"] = json!([{"name":"scale_offset","configuration":configuration},{"name":"bytes","configuration":{"endian":"little"}},blosc()]);
        let mut frame = blosc_memcpy(
            &input
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
            4,
        );
        assert_eq!(
            read(
                meta.clone(),
                Some(frame.clone()),
                &[Selector::Indices(vec![0, 1])]
            )
            .await
            .unwrap()
            .bytes,
            expected
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>()
        );
        frame[3] = 8;
        assert_eq!(
            read(meta, Some(frame), &[Selector::Index(0)])
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Decode
        );
    }
}

#[tokio::test]
async fn oversized_historical_pressure_units_fail_before_codec_construction() {
    let mut meta = common::axis("data", "float32", 1, 1, json!({}));
    meta["shape"] = json!([1, 4, 1, 721, 1440, 13]);
    meta["dimension_names"] = json!([
        "init_time",
        "ensemble_member",
        "lead_time",
        "y",
        "x",
        "pressure_level"
    ]);
    meta["chunk_grid"]["configuration"]["chunk_shape"] = meta["shape"].clone();
    meta["codecs"] = json!([{"name":"transpose","configuration":{"order":[0,1,2,5,3,4]}},{"name":"bytes","configuration":{"endian":"little"}},blosc()]);
    let selectors = (0..6).map(|_| Selector::Index(0)).collect::<Vec<_>>();
    assert_eq!(
        read(meta, None, &selectors).await.unwrap_err().kind,
        ErrorKind::SizeLimit
    );
}
