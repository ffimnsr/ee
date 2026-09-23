//! Dependency-free counting allocator for the §4 render benchmark gate.
//!
//! A `#[global_allocator]` here would normally impose per-allocation overhead
//! on every binary that links the lib; this one is a transparent pass-through
//! unless [`arm`] is set (one relaxed atomic load per allocation when
//! disarmed). When armed, it tracks the peak *live* byte count so the bench
//! can assert that one render window's allocations are bounded and flat
//! across file positions (no per-render allocation proportional to file
//! size).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static ARMED: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// Arm the counter and zero its state. Allocations made while armed (any
/// thread, including the core render thread) are counted.
pub(crate) fn arm() {
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    ARMED.store(true, Ordering::Release);
}

pub(crate) fn disarm() {
    ARMED.store(false, Ordering::Release);
}

/// Peak live bytes observed since the last [`arm`].
pub(crate) fn peak_bytes() -> u64 {
    PEAK.load(Ordering::Relaxed) as u64
}

struct CountingAlloc;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ARMED.load(Ordering::Relaxed) || layout.size() == 0 || ptr.is_null() {
            return ptr;
        }
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        if live < (1 << 63) {
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ARMED.load(Ordering::Relaxed) && layout.size() > 0 {
            let _ = LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !ARMED.load(Ordering::Relaxed) || layout.size() == 0 || new_ptr.is_null() {
            return new_ptr;
        }
        let delta = new_size as isize - layout.size() as isize;
        let old = LIVE.fetch_add(delta.max(0) as usize, Ordering::Relaxed);
        let live =
            if delta >= 0 { old + delta as usize } else { old.saturating_sub((-delta) as usize) };
        // fetch_add already accounted the positive delta; a shrink needs a
        // store back so LIVE actually drops and the peak stays honest.
        if delta < 0 {
            LIVE.store(live, Ordering::Relaxed);
        }
        if live < (1 << 63) {
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        new_ptr
    }
}
