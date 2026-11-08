//! Counts allocations on the test's thread while a hot path runs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

struct Counting;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocated() {
    let _ = ALLOCATIONS.try_with(|count| {
        if let Some(n) = count.get() {
            count.set(Some(n + 1));
        }
    });
}

// SAFETY: every allocation and deallocation delegates to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated();
        // SAFETY: the caller supplies a valid allocation layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated();
        // SAFETY: the caller supplies a valid allocation layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout came from the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated();
        // SAFETY: the caller supplies the original allocation and valid new size.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

/// Runs `f` with allocation counting enabled only on the calling thread.
pub(crate) fn count(f: impl FnOnce()) -> usize {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ALLOCATIONS.set(None);
        }
    }

    ALLOCATIONS.set(Some(0));
    let _reset = Reset;
    f();
    ALLOCATIONS.get().expect("allocation counting is enabled")
}
