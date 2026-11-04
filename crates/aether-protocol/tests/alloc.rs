//! The typed codec is allocation-free where the shim uses it: decoding a
//! request or reply borrows from the payload (a REG_KEY reply's node is the
//! one owned value), and encoding into a buffer with room allocates nothing.
//! Counted with a global allocator that counts only on a thread that asks.
#![allow(unsafe_code)] // a GlobalAlloc impl is unsafe by definition

mod common;

use aether_protocol::{KeyReply, Reply, Request, SliceBuf};
use common::Msg;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

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

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `f` and returns how many allocations it made on this thread. The
/// result is dropped after counting stops.
fn allocations<R>(f: impl FnOnce() -> R) -> usize {
    COUNT.set(0);
    ARMED.set(true);
    let r = std::hint::black_box(f());
    ARMED.set(false);
    drop(r);
    COUNT.get()
}

#[test]
fn the_counter_counts() {
    assert_eq!(allocations(|| Vec::<u8>::with_capacity(8)), 1);
    assert_eq!(allocations(|| 1 + 1), 0);
}

fn owns_a_node(r: &Reply<'_>) -> bool {
    matches!(r, Reply::RegKey(KeyReply { node: Some(_), .. }))
}

#[test]
fn decoding_the_golden_messages_allocates_nothing() {
    for (name, m) in common::vectors() {
        let bytes = m.bytes();
        let n = match &m {
            Msg::Req(r) => allocations(|| Request::decode(r.opcode(), r.flags(), &bytes)),
            Msg::Rep(op, r) if !owns_a_node(r) => allocations(|| {
                let rep = Reply::decode(*op, 0, &bytes);
                // A listing is read lazily: walking it allocates nothing either.
                if let Ok(Reply::ReadDir(list)) = &rep {
                    let _ = list.iter().map(|e| e.name.len()).sum::<usize>();
                }
                rep
            }),
            Msg::Rep(..) | Msg::Raw(_) => continue,
        };
        assert_eq!(n, 0, "decoding {name}");
    }
}

#[test]
fn encoding_into_a_buffer_with_room_allocates_nothing() {
    for (name, m) in common::vectors() {
        let len = m.bytes().len();
        let mut v = Vec::with_capacity(len);
        let mut slot = vec![0u8; len];
        let (n_vec, n_slice) = match &m {
            Msg::Req(r) => (
                allocations(|| r.encode(&mut v)),
                allocations(|| {
                    let mut s = SliceBuf::new(&mut slot);
                    r.encode(&mut s);
                    s.finish()
                }),
            ),
            Msg::Rep(_, r) => (
                allocations(|| r.encode(&mut v)),
                allocations(|| {
                    let mut s = SliceBuf::new(&mut slot);
                    r.encode(&mut s);
                    s.finish()
                }),
            ),
            Msg::Raw(_) => continue,
        };
        assert_eq!((n_vec, n_slice), (0, 0), "encoding {name}");
        assert_eq!(v, slot, "{name}: both buffers hold the same bytes");
        assert_eq!(v, m.bytes(), "{name}");
    }
}
