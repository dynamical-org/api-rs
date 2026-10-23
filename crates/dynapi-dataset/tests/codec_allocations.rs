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
async fn float_chunk_boundary_and_in_place_scale_obey_unit_staging_charge() {
    use dynapi_reader::{ErrorKind, MAX_CHUNK_BYTES, ReadSession, Selector};
    const MARGIN: usize = 2 * 1024 * 1024;
    let elements = MAX_CHUNK_BYTES / 4;
    for transformed in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut meta = common::axis("data", "float32", elements, elements, json!({}));
        let blosc = json!({"name":"blosc","configuration":{"typesize":4,"cname":"zstd","clevel":3,"shuffle":"shuffle","blocksize":0}});
        meta["codecs"] = if transformed {
            json!([{"name":"scale_offset","configuration":{"scale":2,"offset":3}},{"name":"bytes","configuration":{"endian":"little"}},blosc])
        } else {
            json!([{"name":"bytes","configuration":{"endian":"little"}},blosc])
        };
        let mut frame = Vec::with_capacity(MAX_CHUNK_BYTES + 16);
        frame.extend_from_slice(&[2, 1, 2, 4]);
        frame.extend_from_slice(&(MAX_CHUNK_BYTES as u32).to_le_bytes());
        frame.extend_from_slice(&(MAX_CHUNK_BYTES as u32).to_le_bytes());
        frame.extend_from_slice(&(MAX_CHUNK_BYTES as u32 + 16).to_le_bytes());
        frame.resize(MAX_CHUNK_BYTES + 16, 0);
        common::repository(temp.path(), &[("data", meta.clone(), Some(frame))]).await;
        let descriptor = StoreDescriptor::Local(temp.path().to_str().unwrap().into());
        let session = ReadSession::open(&descriptor, DatasetVersion::Branch("main"), &[])
            .await
            .unwrap();
        let baseline = LIVE.load(Relaxed);
        PEAK.store(baseline, Relaxed);
        let result = session
            .read_dataset_selection("data", &[Selector::Index((elements - 1) as i64)], None)
            .await
            .unwrap();
        let peak = PEAK.load(Relaxed).saturating_sub(baseline);
        // E encoded delivery/cache copies, D full decoded/cache/subset staging;
        // four output bytes, no shard index, and fixed tiny-metadata allowance.
        let charge = 2 * (MAX_CHUNK_BYTES + 16) + 3 * MAX_CHUNK_BYTES + 4;
        eprintln!(
            "transformed={transformed} decoded={MAX_CHUNK_BYTES} peak={peak} charge={charge}"
        );
        assert!(peak <= charge + MARGIN, "peak={peak}, charge={charge}");
        assert_eq!(
            result.bytes,
            if transformed { 3.0f32 } else { 0.0f32 }.to_le_bytes()
        );

        // One additional float is rejected before fetching or allocating a chunk,
        // even for an empty result. Output and inner-chunk bounds are independent.
        meta["shape"] = json!([elements + 1]);
        meta["chunk_grid"]["configuration"]["chunk_shape"] = json!([elements + 1]);
        let too_large = tempfile::tempdir().unwrap();
        common::repository(too_large.path(), &[("data", meta, None)]).await;
        let session = ReadSession::open(
            &StoreDescriptor::Local(too_large.path().to_str().unwrap().into()),
            DatasetVersion::Branch("main"),
            &[],
        )
        .await
        .unwrap();
        let baseline = LIVE.load(Relaxed);
        PEAK.store(baseline, Relaxed);
        let error = session
            .read_dataset_selection("data", &[Selector::Indices(vec![])], None)
            .await
            .unwrap_err();
        let peak = PEAK.load(Relaxed).saturating_sub(baseline);
        assert_eq!(error.kind, ErrorKind::SizeLimit);
        assert!(peak < MARGIN, "preflight allocated {peak}");
    }
}
