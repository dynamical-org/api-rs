//! A separate test binary isolates the process-wide counting allocator.
use dynapi_dataset::*;
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
mod common;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn add(n: usize) {
    let total = LIVE.fetch_add(n, Relaxed) + n;
    PEAK.fetch_max(total, Relaxed);
}
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(p, layout, size) };
        if !out.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
            add(size);
        }
        out
    }
}
#[global_allocator]
static ALLOC: Counting = Counting;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinate_staging_peak_obeys_cap_before_selectors_or_groups() {
    implicit_index_capacity_obeys_charge().await;
    // One MiB covers runtime task/thread initialization and this tiny repository's
    // upstream snapshot/manifest/cache allocations, which the adapter cannot charge.
    // It is independent of axis length and below 3.1% of the smallest tested cap.
    const MARGIN: usize = 1024 * 1024;
    for chunk in [4096, 1] {
        let temp = tempfile::tempdir().unwrap();
        let mut meta = common::axis(
            "lead_time",
            "float64",
            1_048_576,
            chunk,
            json!({"units":"seconds","dtype":"timedelta64[us]","_FillValue":"AAAAAAAA+H8="}),
        );
        // Match the synthetic fixture's stored dtype, CF attributes and codec chain.
        meta["codecs"] = json!([
            {"name":"bytes","configuration":{"endian":"little"}},
            {"name":"blosc","configuration":{"typesize":8,"cname":"zstd","clevel":3,"shuffle":"shuffle","blocksize":0}}
        ]);
        common::repository(temp.path(), &[("lead_time", meta, None)]).await;
        let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
        for limit in [34_000_000, MAX_METADATA_BYTES, 256 * 1024 * 1024] {
            let baseline = LIVE.load(Relaxed);
            PEAK.store(baseline, Relaxed);
            let opened = OpenedDataset::open_bounded(
                &descriptor,
                DatasetVersion::Branch("main"),
                &[],
                limit,
            )
            .await;
            let peak = PEAK.load(Relaxed).saturating_sub(baseline);
            eprintln!(
                "chunk={chunk} limit={limit} peak={peak} charge={:?}",
                opened.as_ref().map(|d| d.metadata_bytes())
            );
            assert!(peak <= limit + MARGIN, "peak={peak}, limit={limit}");
            if chunk == 1 {
                assert!(
                    matches!(opened, Err(OpenError::Reader(e)) if e.kind == dynapi_reader::ErrorKind::SizeLimit)
                );
                assert!(
                    peak < MARGIN,
                    "cardinality must fail before selector/group allocation"
                );
            } else if limit <= MAX_METADATA_BYTES {
                assert!(matches!(opened, Err(OpenError::MetadataLimit { .. })));
                assert!(peak < MARGIN, "budget must fail before selector allocation");
            } else {
                let dataset = opened.unwrap();
                assert!(peak <= dataset.metadata_bytes() + MARGIN);
                assert!(
                    matches!(&dataset.view().coordinates["lead_time"], dynapi_query::Coordinate::Duration(v) if v.len()==1_048_576 && v.iter().all(|x|*x==i64::MIN))
                );
            }
        }
        if chunk == 1 {
            // Also exercise arbitrary reader selectors, bypassing the adapter's
            // full-axis preflight. The counting set may hold at most MAX_CHUNKS
            // entries, never one million per-selector grouping vectors.
            let session =
                dynapi_reader::ReadSession::open(&descriptor, DatasetVersion::Branch("main"), &[])
                    .await
                    .unwrap();
            let selectors = [dynapi_reader::Selector::Indices((0..1_048_576).collect())];
            let baseline = LIVE.load(Relaxed);
            PEAK.store(baseline, Relaxed);
            let error = session
                .read_dataset_selection("lead_time", &selectors, None)
                .await
                .unwrap_err();
            let peak = PEAK.load(Relaxed).saturating_sub(baseline);
            eprintln!("reader chunk=1 preflight peak={peak}");
            assert_eq!(error.kind, dynapi_reader::ErrorKind::SizeLimit);
            assert!(
                peak < 4 * MARGIN,
                "bounded cardinality set, not selector groups"
            );
        }
    }
}

async fn implicit_index_capacity_obeys_charge() {
    // A fixed 256 KiB covers the tiny payload-free repository's excluded upstream
    // snapshot/cache and runtime allocations, independent of N and <2.7% of 10 MB.
    const MARGIN: usize = 256 * 1024;
    for (size, limit) in [(1_048_577, 10_000_000), (4_194_305, MAX_METADATA_BYTES)] {
        let temp = tempfile::tempdir().unwrap();
        let mut meta = common::axis("data", "float32", size, 1024, json!({}));
        meta["dimension_names"] = json!(["y"]);
        common::repository(temp.path(), &[("data", meta, None)]).await;
        let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
        let baseline = LIVE.load(Relaxed);
        PEAK.store(baseline, Relaxed);
        let dataset =
            OpenedDataset::open_bounded(&descriptor, DatasetVersion::Branch("main"), &[], limit)
                .await
                .unwrap();
        let peak = PEAK.load(Relaxed).saturating_sub(baseline);
        let retained = LIVE.load(Relaxed).saturating_sub(baseline);
        let charge = dataset.metadata_bytes();
        let dynapi_query::Coordinate::Int64(values) = &dataset.view().coordinates["y"] else {
            panic!("implicit y index must be int64");
        };
        eprintln!(
            "implicit size={size} limit={limit} capacity={} retained={retained} peak={peak} charge={charge}",
            values.capacity()
        );
        assert_eq!(values.len(), size);
        assert_eq!(values.capacity(), size, "retain only the charged slots");
        assert_eq!((values[0], values[size - 1]), (0, (size - 1) as i64));
        assert!(values.capacity() * size_of::<i64>() <= charge);
        assert!(charge <= limit);
        assert!(retained <= charge + MARGIN);
        assert!(peak <= charge + MARGIN);
        assert!(peak <= limit + MARGIN);
    }
}
