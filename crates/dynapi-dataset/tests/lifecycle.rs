use dynapi_contract::Value;
use dynapi_dataset::*;
use dynapi_query::{DataReader, Positions, QueryError, Selection, SpatialCrs};
use serde_json::Value as Json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Semaphore, oneshot};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/forecast/repository")
}
fn copy_dir(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let dest = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}
fn positions() -> Positions {
    [
        ("init_time".into(), Selection::Index(0)),
        ("lead_time".into(), Selection::Indices(vec![0, 1, 2])),
        ("latitude".into(), Selection::Index(1)),
        ("longitude".into(), Selection::Index(0)),
    ]
    .into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_is_reused_without_repository_reopening() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repository");
    copy_dir(&fixture(), &path);
    let descriptor = StoreDescriptor::Local(path.to_str().unwrap().into());
    let dataset = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    // Removing the mutable repository object makes any reopen fail; the pinned
    // session still has everything needed to read immutable chunks/manifests.
    std::fs::remove_file(path.join("repo")).unwrap();
    assert!(
        OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
            .await
            .is_err()
    );
    let slots = Arc::new(Semaphore::new(1));
    run_with_permit(
        slots.clone().acquire_owned().await.unwrap(),
        move |handle| {
            let mut reader = dataset.reader(handle);
            let first = reader.read(dataset.view(), "count", &positions())?;
            let second = reader.read(dataset.view(), "count", &positions())?;
            assert_eq!(first, second);
            assert!(first.iter().all(|v| matches!(v, Value::Int(_))));
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(slots.available_permits(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_caller_keeps_permit_until_reads_finish() {
    let descriptor = StoreDescriptor::Local(fixture().to_str().unwrap().into());
    let dataset = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
        .await
        .unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let permit = slots.clone().acquire_owned().await.unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (read_tx, read_rx) = oneshot::channel();
    let caller = tokio::spawn(run_with_permit(permit, move |handle| {
        started_tx.send(()).unwrap();
        // Dropping release_tx also unblocks the worker if this test panics.
        let _ = release_rx.recv();
        let values = dataset
            .reader(handle)
            .read(dataset.view(), "count", &positions())?;
        read_tx.send(values).unwrap();
        Ok(())
    }));
    started_rx.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(slots.clone().try_acquire_owned().is_err());
    release_tx.send(()).unwrap();
    let values = read_rx.await.unwrap();
    assert_eq!(values.len(), 3);
    let _permit = tokio::time::timeout(std::time::Duration::from_secs(5), slots.acquire())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn panic_and_error_release_compute_permits() {
    let slots = Arc::new(Semaphore::new(1));
    let error = run_with_permit(slots.clone().acquire_owned().await.unwrap(), |_| {
        Err::<(), _>(QueryError::internal("synthetic failure"))
    })
    .await
    .unwrap_err();
    assert_eq!(error.status, 500);
    assert_eq!(slots.available_permits(), 1);
    let error = run_with_permit(
        slots.clone().acquire_owned().await.unwrap(),
        |_| -> Result<(), QueryError> { panic!("synthetic panic") },
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, 500);
    assert_eq!(slots.available_permits(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_codec_is_a_clean_internal_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repository");
    copy_dir(&fixture(), &path);
    let storage = icechunk::new_local_filesystem_storage(&path).await.unwrap();
    let repo = icechunk::Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let session = repo.writable_session("main").await.unwrap();
    let store = icechunk::Store::from_session(Arc::new(tokio::sync::RwLock::new(session))).await;
    let raw = store
        .get(
            "temperature_2m/zarr.json",
            &icechunk::format::ByteRange::ALL,
        )
        .await
        .unwrap();
    let mut meta: Json = serde_json::from_slice(&raw).unwrap();
    meta["codecs"][0]["configuration"]["codecs"].as_array_mut().unwrap().insert(0,serde_json::json!({"name":"numcodecs.fixedscaleoffset","configuration":{"offset":0,"scale":0.01,"dtype":"float32","astype":"int16"}}));
    store
        .set(
            "temperature_2m/zarr.json",
            serde_json::to_vec(&meta).unwrap().into(),
        )
        .await
        .unwrap();
    let session = store.session();
    let snapshot = session
        .write()
        .await
        .commit("Synthetic unsupported codec")
        .execute()
        .await
        .unwrap();
    let descriptor = StoreDescriptor::Local(path.to_str().unwrap().into());
    let dataset = OpenedDataset::open(
        &descriptor,
        DatasetVersion::Snapshot(&snapshot.to_string()),
        &[],
    )
    .await
    .unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let error = run_with_permit(slots.acquire_owned().await.unwrap(), move |handle| {
        dataset
            .reader(handle)
            .read(dataset.view(), "temperature_2m", &positions())
    })
    .await
    .unwrap_err();
    assert_eq!(error.status, 500);
    assert!(error.retry_after.is_none());
    assert!(
        error.detail.contains("Expected little-endian bytes"),
        "{error}"
    );
}

#[test]
fn storage_error_mapping_only_retries_typed_virtual_fetches() {
    use dynapi_reader::{ErrorKind, ReaderError};
    for message in [
        "operation timeout (all attempts including retries) occurred after 20s",
        "Error { code: \"SlowDown\", message: \"synthetic\" }",
    ] {
        for virtual_fetch in [false, true] {
            let error = map_reader_error(ReaderError {
                kind: ErrorKind::Storage,
                message: message.into(),
                virtual_chunk_fetch: virtual_fetch,
                exception_class: None,
            });
            assert_eq!(error.status, if virtual_fetch { 503 } else { 500 });
            assert_eq!(error.retry_after, virtual_fetch.then_some(5));
            if !virtual_fetch {
                assert_eq!(error.exception_class, Some("StorageError"));
            }
        }
    }
    for message in [
        "403 AccessDenied",
        "404 NotFound",
        "unrecognized",
        "TimeoutError",
    ] {
        let error = map_reader_error(ReaderError {
            kind: ErrorKind::Storage,
            message: message.into(),
            virtual_chunk_fetch: true,
            exception_class: None,
        });
        assert_eq!(error.status, 500);
        assert_eq!(error.exception_class, Some("StorageError"));
    }
    let error = map_reader_error(ReaderError {
        kind: ErrorKind::Unsupported,
        message: "unsupported codec".into(),
        virtual_chunk_fetch: false,
        exception_class: None,
    });
    assert_eq!(error.status, 500);
    assert!(error.retry_after.is_none());
}

#[test]
fn classifier_uses_explicit_geographic_policy_and_fails_closed() {
    let inputs: Json = serde_json::from_slice(include_bytes!("fixtures/live-crs.json")).unwrap();
    // Exact operations take precedence over the five geographic identities.
    for (name, expected) in [
        ("grib-sphere", SpatialCrs::Geographic),
        (
            "rotated-sphere",
            SpatialCrs::Attested(dynapi_projection::ProjectionKind::HrdpsRotatedPole),
        ),
        ("sphere-6371229", SpatialCrs::Geographic),
        ("sphere-6367470", SpatialCrs::Geographic),
        ("wgs84", SpatialCrs::Geographic),
        ("grib-spheroid", SpatialCrs::Geographic),
        (
            "lambert-conformal",
            SpatialCrs::Attested(dynapi_projection::ProjectionKind::HrrrLambertConformal),
        ),
    ] {
        let wkt = inputs[name].as_str().unwrap();
        assert_eq!(classify_wkt(wkt), expected, "{name}");
        // Exact spelling is part of this conservative policy, even for valid WKT.
        assert_eq!(classify_wkt(&format!(" {wkt}")), SpatialCrs::Projected);
    }
    for wkt in [
        "",
        "GEOGCS[",
        "GEOGCRS[\"made up\"]",
        "EPSG:4326",
        "PROJCS[GEOGCS[]]",
        "GEOGCS[\"bad\",PROJECTION[\"Lambert\"]]",
    ] {
        assert_eq!(classify_wkt(wkt), SpatialCrs::Projected);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn huge_implicit_axis_is_rejected_before_selector_allocation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repository");
    copy_dir(&fixture(), &path);
    let storage = icechunk::new_local_filesystem_storage(&path).await.unwrap();
    let repo = icechunk::Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let session = repo.writable_session("main").await.unwrap();
    let store = icechunk::Store::from_session(Arc::new(tokio::sync::RwLock::new(session))).await;
    let raw = store
        .get("count/zarr.json", &icechunk::format::ByteRange::ALL)
        .await
        .unwrap();
    let mut meta: Json = serde_json::from_slice(&raw).unwrap();
    meta["shape"] = serde_json::json!([100_000_000]);
    meta["dimension_names"] = serde_json::json!(["huge"]);
    meta["chunk_grid"]["configuration"]["chunk_shape"] = serde_json::json!([1]);
    meta["attributes"] = serde_json::json!({});
    store
        .set("large/zarr.json", serde_json::to_vec(&meta).unwrap().into())
        .await
        .unwrap();
    let session = store.session();
    let snapshot = session
        .write()
        .await
        .commit("Synthetic large implicit axis")
        .execute()
        .await
        .unwrap();
    let descriptor = StoreDescriptor::Local(path.to_str().unwrap().into());
    let dataset = OpenedDataset::open(
        &descriptor,
        DatasetVersion::Snapshot(&snapshot.to_string()),
        &[],
    )
    .await
    .unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let error = run_with_permit(slots.acquire_owned().await.unwrap(), move |handle| {
        dataset
            .reader(handle)
            .read(dataset.view(), "large", &Positions::new())
    })
    .await
    .unwrap_err();
    assert_eq!(error.status, 500);
    assert_eq!(error.detail, "Too many selector indices");
}

#[tokio::test]
async fn dangling_global_coordinate_reference_is_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repository");
    copy_dir(&fixture(), &path);
    let storage = icechunk::new_local_filesystem_storage(&path).await.unwrap();
    let repo = icechunk::Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let session = repo.writable_session("main").await.unwrap();
    let store = icechunk::Store::from_session(Arc::new(tokio::sync::RwLock::new(session))).await;
    store.delete("spatial_ref/zarr.json").await.unwrap();
    let session = store.session();
    let snapshot = session
        .write()
        .await
        .commit("Synthetic dangling coordinate reference")
        .execute()
        .await
        .unwrap();
    let descriptor = StoreDescriptor::Local(path.to_str().unwrap().into());
    let dataset = OpenedDataset::open(
        &descriptor,
        DatasetVersion::Snapshot(&snapshot.to_string()),
        &[],
    )
    .await
    .unwrap();
    assert!(dataset.view().spatial_crs.is_none());
}

#[test]
fn admitted_reads_complete_with_a_saturated_two_thread_blocking_pool() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let descriptor = StoreDescriptor::Local(fixture().to_str().unwrap().into());
        let dataset = OpenedDataset::open(&descriptor, DatasetVersion::Branch("main"), &[])
            .await
            .unwrap();
        let slots = Arc::new(Semaphore::new(2));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut jobs = Vec::new();
        for _ in 0..2 {
            let dataset = dataset.clone();
            let barrier = barrier.clone();
            jobs.push(tokio::spawn(run_with_permit(
                slots.clone().acquire_owned().await.unwrap(),
                move |handle| {
                    // Both compute jobs start before either asks the reader for I/O.
                    barrier.wait();
                    for _ in 0..10 {
                        let values = dataset.reader(handle.clone()).read(
                            dataset.view(),
                            "count",
                            &positions(),
                        )?;
                        assert_eq!(values.len(), 3);
                    }
                    Ok(())
                },
            )));
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for job in jobs {
                job.await.unwrap().unwrap();
            }
        })
        .await
        .expect("compute must leave Tokio blocking threads available for reader I/O");
        assert_eq!(slots.available_permits(), 2);
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
}

#[test]
fn runtime_shutdown_during_compute_contains_reader_panics_and_releases_permit() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let descriptor = StoreDescriptor::Local(fixture().to_str().unwrap().into());
    let dataset = runtime
        .block_on(OpenedDataset::open(
            &descriptor,
            DatasetVersion::Branch("main"),
            &[],
        ))
        .unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let permit = slots.clone().try_acquire_owned().unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut worker = std::pin::pin!(run_with_permit(permit, move |handle| {
        started_tx.send(()).unwrap();
        let _ = release_rx.recv();
        dataset
            .reader(handle)
            .read(dataset.view(), "count", &positions())
    }));
    runtime.block_on(async {
        tokio::select! {
            _ = &mut worker => panic!("worker must wait for the gate"),
            _ = started_rx => {}
        }
    });
    assert_eq!(slots.available_permits(), 0);
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    release_tx.send(()).unwrap();
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = observer
        .block_on(async { tokio::time::timeout(std::time::Duration::from_secs(3), worker).await });
    assert_eq!(result.unwrap().unwrap_err().status, 500);
    assert_eq!(slots.available_permits(), 1);
}
