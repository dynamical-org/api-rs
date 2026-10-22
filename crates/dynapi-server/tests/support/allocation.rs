//! Per-thread allocation accounting for current-thread HTTP regression tests.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub largest: usize,
    pub total: usize,
}
thread_local! { static ACTIVE: Cell<Option<Stats>> = const { Cell::new(None) }; }

pub struct Allocator;
fn record(size: usize) {
    let _ = ACTIVE.try_with(|active| {
        if let Some(mut stats) = active.get() {
            stats.largest = stats.largest.max(size);
            stats.total = stats.total.saturating_add(size);
            active.set(Some(stats));
        }
    });
}
// SAFETY: all allocation operations delegate to System with the original layout
// and pointer; the thread-local counters never allocate or change allocator data.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller supplies the valid allocation layout unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller supplies the valid allocation layout unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        // SAFETY: forward the caller's allocation and requested size unchanged.
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forward the allocation's original pointer and layout unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}
pub struct Tracking;
impl Tracking {
    pub fn start() -> Self {
        ACTIVE.with(|active| assert!(active.replace(Some(Stats::default())).is_none()));
        Self
    }
    pub fn finish(self) -> Stats {
        ACTIVE.with(|active| active.take().unwrap())
    }
}
impl Drop for Tracking {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(None));
    }
}
