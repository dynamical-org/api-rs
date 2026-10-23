//! Current-thread Rust allocation evidence, not process/native memory peaks.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Counts {
    pub total: usize,
    pub live: usize,
    pub peak: usize,
    pub foreign_deallocation: bool,
}
thread_local! { static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) }; }

struct Counter;
// Test-only process counters pair allocations and frees across upstream blocking
// threads. Thread-local Counts cannot prove retention when ownership crosses one.
static PROCESS_LIVE: AtomicUsize = AtomicUsize::new(0);
static PROCESS_PEAK: AtomicUsize = AtomicUsize::new(0);
static PROCESS_MEASURING: AtomicBool = AtomicBool::new(false);
fn change(allocate: usize, release: usize) {
    let peak = PROCESS_LIVE
        .fetch_add(allocate, Ordering::SeqCst)
        .saturating_add(allocate);
    if PROCESS_MEASURING.load(Ordering::SeqCst) {
        PROCESS_PEAK.fetch_max(peak, Ordering::SeqCst);
    }
    PROCESS_LIVE.fetch_sub(release, Ordering::SeqCst);
    let _ = COUNTS.try_with(|state| {
        if let Some(mut counts) = state.get() {
            counts.total = counts.total.saturating_add(allocate);
            // Conservatively include old/new backing overlap during realloc.
            counts.peak = counts.peak.max(counts.live.saturating_add(allocate));
            if release > counts.live {
                counts.foreign_deallocation = true;
            }
            counts.live = counts.live.saturating_add(allocate).saturating_sub(release);
            state.set(Some(counts));
        }
    });
}
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        change(layout.size(), 0);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        change(layout.size(), 0);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        change(size, layout.size());
        unsafe { System.realloc(pointer, layout, size) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        change(0, layout.size());
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static COUNTER: Counter = Counter;

pub(crate) struct Measurement;
impl Measurement {
    pub(crate) fn start() -> Self {
        COUNTS.with(|state| {
            assert!(state.get().is_none());
            state.set(Some(Counts::default()));
        });
        Self
    }
    pub(crate) fn counts(&self) -> Counts {
        COUNTS.with(|state| state.get().unwrap())
    }
    // Preserve the existing qualification helper's cumulative-byte convention.
    pub(crate) fn bytes(&self) -> usize {
        self.counts().total
    }
}
impl Drop for Measurement {
    fn drop(&mut self) {
        COUNTS.with(|state| state.set(None));
    }
}

/// Use only in an isolated test subprocess: all Rust allocations on all threads
/// are counted, including unrelated runtime/client state. Native malloc/RSS is
/// outside this measurement. Existing thread-local decoder witnesses stay scoped.
pub(crate) struct AllThreads {
    pub baseline: usize,
}
impl AllThreads {
    pub fn start() -> Self {
        assert!(!PROCESS_MEASURING.swap(true, Ordering::SeqCst));
        let baseline = PROCESS_LIVE.load(Ordering::SeqCst);
        PROCESS_PEAK.store(baseline, Ordering::SeqCst);
        Self { baseline }
    }
    pub fn live(&self) -> usize {
        PROCESS_LIVE.load(Ordering::SeqCst)
    }
    pub fn peak(&self) -> usize {
        PROCESS_PEAK.load(Ordering::SeqCst)
    }
}
impl Drop for AllThreads {
    fn drop(&mut self) {
        PROCESS_MEASURING.store(false, Ordering::SeqCst);
    }
}
