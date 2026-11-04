//! An allocation counter for tests that a path allocates nothing, or no more
//! than it did.
//!
//! [`Counting`] is a `GlobalAlloc` that forwards every call to `System` and
//! counts allocations (`alloc`, `alloc_zeroed`, `realloc`) made on a thread
//! that has asked, through [`allocations`]. Other threads, a director's
//! serve thread for one, are never counted. A test binary installs it once:
//!
//! ```text
//! #[global_allocator]
//! static A: aether_testkit::alloc::Counting = aether_testkit::alloc::Counting;
//!
//! let (reply, n) = aether_testkit::alloc::allocations(|| client.call(&req));
//! assert_eq!(n, 0);
//! ```
//!
//! In a binary that does not install it, [`allocations`] counts 0.

// A GlobalAlloc impl is unsafe by definition.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// The counting allocator. See the module docs.
pub struct Counting;

thread_local! {
    // Const-initialised and without a destructor, so reading them from the
    // allocator never allocates.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    if ARMED.try_with(Cell::get).unwrap_or(false) {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }
}

// SAFETY: every call is forwarded to `System` unchanged; the counting
// touches only thread-local cells.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: the caller's contract is passed on as is.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Runs `f` and returns its result with the number of allocations it made on
/// this thread. Counting stops when `f` returns, so dropping the result is
/// the caller's and is not counted.
pub fn allocations<R>(f: impl FnOnce() -> R) -> (R, usize) {
    COUNT.set(0);
    ARMED.set(true);
    let r = std::hint::black_box(f());
    ARMED.set(false);
    (r, COUNT.get())
}
