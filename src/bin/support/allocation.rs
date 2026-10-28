use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Tracking;
pub(crate) static LIVE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static PEAK: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ACTIVE: AtomicBool = AtomicBool::new(false);
pub(crate) static MAX_REQUEST: AtomicUsize = AtomicUsize::new(usize::MAX);
pub(crate) static MAX_LIVE: AtomicUsize = AtomicUsize::new(usize::MAX);
pub(crate) static LARGEST: AtomicUsize = AtomicUsize::new(0);
pub(crate) static DENIED: AtomicUsize = AtomicUsize::new(0);
pub(crate) static THREADS: AtomicUsize = AtomicUsize::new(0);
thread_local! { static COUNTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

fn reserve(size: usize) -> bool {
    let active = ACTIVE.load(Ordering::Relaxed);
    let result = LIVE.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |live| {
        let next = live.checked_add(size)?;
        if active
            && (size > MAX_REQUEST.load(Ordering::Relaxed)
                || next > MAX_LIVE.load(Ordering::Relaxed))
        {
            None
        } else {
            Some(next)
        }
    });
    let Ok(previous) = result else {
        DENIED.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    if active {
        COUNTED.with(|counted| {
            if !counted.replace(true) {
                THREADS.fetch_add(1, Ordering::Relaxed);
            }
        });
        PEAK.fetch_max(previous + size, Ordering::Relaxed);
        LARGEST.fetch_max(size, Ordering::Relaxed);
    }
    true
}

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !reserve(layout.size()) {
            return std::ptr::null_mut();
        }
        let pointer = unsafe { System.alloc(layout) };
        if pointer.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !reserve(layout.size()) {
            return std::ptr::null_mut();
        }
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if pointer.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if !reserve(size) {
            return std::ptr::null_mut();
        }
        let result = unsafe { System.realloc(pointer, layout, size) };
        LIVE.fetch_sub(
            if result.is_null() {
                size
            } else {
                layout.size()
            },
            Ordering::SeqCst,
        );
        result
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;
