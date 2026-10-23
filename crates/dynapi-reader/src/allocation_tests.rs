//! Current-thread Rust allocation evidence, not process/native memory peaks.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
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
fn change(allocate: usize, release: usize) {
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
