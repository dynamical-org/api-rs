//! Large epoch axes use exact synthetic values, never saved service answers.
use dynapi_dataset::{DatasetVersion, OpenError, OpenedDataset, StoreDescriptor};
use dynapi_query::{
    CatalogVariable, Collection, Coordinate, Kind, QueryOptions, RequestDatetime, SelectedPoint,
    Selection, select_query,
};
use serde_json::json;
mod common;

const BATCH: usize = 262_144;
fn seconds(index: usize) -> i64 {
    match index {
        0 => i64::MIN,
        i if i == BATCH - 1 || i == BATCH => 2,
        i if i == BATCH + 1 => 7,
        i if i == 2 * BATCH => 1,
        _ => 20,
    }
}
fn frame(bytes: &[u8]) -> Vec<u8> {
    // Blosc v2 memcpy frame from the published format; payload is explicit i64 LE.
    let mut frame = vec![2, 1, 2, 8];
    frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(bytes.len() as u32 + 16).to_le_bytes());
    frame.extend_from_slice(bytes);
    frame
}
async fn fixture(path: &std::path::Path, overflow: bool) {
    let size = 2 * BATCH + 1;
    let mut meta = common::axis(
        "time",
        "int64",
        size,
        size,
        json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"}),
    );
    meta["codecs"] = json!([
        {"name":"bytes","configuration":{"endian":"little"}},
        {"name":"blosc","configuration":{"typesize":8,"cname":"zstd","clevel":3,"shuffle":"shuffle","blocksize":0}}
    ]);
    let raw: Vec<_> = (0..size)
        .flat_map(|i| {
            if overflow && i == 2 * BATCH {
                i64::MAX
            } else {
                seconds(i)
            }
            .to_le_bytes()
        })
        .collect();
    let mut count = common::axis("count", "float32", size, 4096, json!({}));
    count["dimension_names"] = json!(["time"]);
    common::repository(
        path,
        &[("time", meta, Some(frame(&raw))), ("count", count, None)],
    )
    .await;
}

#[tokio::test]
async fn batched_epochs_keep_nat_gaps_duplicates_source_order_and_analysis_bounds() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path(), false).await;
    let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
    let ds = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    let full = OpenedDataset::open_bounded(
        &descriptor,
        DatasetVersion::Branch("main"),
        &[],
        256 * 1024 * 1024,
    )
    .await
    .unwrap();
    let Coordinate::Time(values) = &ds.view().coordinates["time"] else {
        panic!("time");
    };
    assert_eq!(values.len(), 2 * BATCH + 1);
    assert_eq!(values.capacity(), values.len());
    for (i, actual) in values.iter().enumerate() {
        let raw = seconds(i);
        assert_eq!(
            *actual,
            if raw == i64::MIN {
                raw
            } else {
                raw * 1_000_000_000
            }
        );
    }
    let Coordinate::Time(full_values) = &full.view().coordinates["time"] else {
        panic!("time");
    };
    assert_eq!(values, full_values);
    assert!(ds.metadata_bytes() < full.metadata_bytes());
    let collection = Collection {
        id: "fixture-epochs".into(),
        kind: Kind::Analysis,
        version: None,
        catalog_url: None,
        variables: [(
            "count".into(),
            CatalogVariable {
                dimensions: vec!["time".into()],
                unit: None,
            },
        )]
        .into(),
        point_query_reason: None,
    };
    let options = QueryOptions {
        variables: vec!["count".into()],
        start: Some(RequestDatetime::parse_str("1970-01-01T00:00:02Z").unwrap()),
        end: Some(RequestDatetime::parse_str("1970-01-01T00:00:07Z").unwrap()),
        ..Default::default()
    };
    let point = SelectedPoint {
        latitude: 0.,
        longitude: 0.,
        indexers: vec![],
    };
    let selected = select_query(
        ds.view(),
        &collection,
        point.clone(),
        options,
        ds.snapshot_id(),
        None,
    )
    .unwrap();
    assert_eq!(
        selected.positions()["time"],
        Selection::Indices(vec![BATCH - 1, BATCH])
    );
    let latest = select_query(
        ds.view(),
        &collection,
        point,
        QueryOptions {
            variables: vec!["count".into()],
            ..Default::default()
        },
        ds.snapshot_id(),
        None,
    )
    .unwrap();
    assert_eq!(
        latest.positions()["time"],
        Selection::Indices(vec![2 * BATCH])
    );
}

#[tokio::test]
async fn invalid_epoch_in_a_later_batch_cannot_return_a_partial_dataset() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path(), true).await;
    let error = OpenedDataset::open(
        &StoreDescriptor::Local(temp.path().to_str().unwrap().into()),
        DatasetVersion::Branch("main"),
        &[],
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(error,OpenError::Reader(e) if e.kind==dynapi_reader::ErrorKind::Decode && e.message=="CF time outside datetime64[ns]/timedelta64[ns] range")
    );
}

#[tokio::test]
async fn epoch_batches_cross_unaligned_physical_chunks_without_changing_values() {
    const CHUNK: usize = 100_000;
    let size = 2 * BATCH + 1;
    let temp = tempfile::tempdir().unwrap();
    let mut meta = common::axis(
        "time",
        "int64",
        size,
        CHUNK,
        json!({"units":"seconds since 1970-01-01","calendar":"proleptic_gregorian"}),
    );
    meta["codecs"] = json!([
        {"name":"bytes","configuration":{"endian":"little"}},
        {"name":"blosc","configuration":{"typesize":8,"cname":"zstd","clevel":3,"shuffle":"shuffle","blocksize":0}}
    ]);
    common::repository(temp.path(), &[("time", meta, None)]).await;
    let storage = icechunk::new_local_filesystem_storage(temp.path())
        .await
        .unwrap();
    let repo = icechunk::Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let session = repo.writable_session("main").await.unwrap();
    let store =
        icechunk::Store::from_session(std::sync::Arc::new(tokio::sync::RwLock::new(session))).await;
    for chunk in 0..size.div_ceil(CHUNK) {
        let raw: Vec<_> = (0..CHUNK)
            .flat_map(|offset| {
                // Distinct signed values reveal dropped/reordered/duplicated chunks
                // and conversion errors at either selector-batch boundary.
                (chunk as i64 * CHUNK as i64 + offset as i64 - 250_000).to_le_bytes()
            })
            .collect();
        store
            .set(&format!("time/c/{chunk}"), frame(&raw).into())
            .await
            .unwrap();
    }
    store
        .session()
        .write()
        .await
        .commit("Synthetic unaligned epoch chunks")
        .execute()
        .await
        .unwrap();
    let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
    let batched = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    let full = OpenedDataset::open_bounded(
        &descriptor,
        DatasetVersion::Branch("main"),
        &[],
        256 * 1024 * 1024,
    )
    .await
    .unwrap();
    let Coordinate::Time(values) = &batched.view().coordinates["time"] else {
        panic!("time");
    };
    let Coordinate::Time(full_values) = &full.view().coordinates["time"] else {
        panic!("time");
    };
    assert_eq!(values, full_values);
    assert_eq!(values.len(), size);
    assert_eq!(values.capacity(), size);
    assert!(
        batched.metadata_bytes() < full.metadata_bytes(),
        "exercise the bounded plan"
    );
    for (i, &value) in values.iter().enumerate() {
        assert_eq!(value, (i as i64 - 250_000) * 1_000_000_000, "index {i}");
    }
}
