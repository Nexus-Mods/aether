//! The one place that maps memory, and so the one place that is `unsafe`.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;

use aether_ipc::SharedSeg;
use memmap2::{MmapMut, MmapOptions};

/// A shared mapping of a whole file and the `SharedSeg` over it.
pub(crate) struct Mapped {
    // `seg` holds a raw pointer into `map`, not a borrow; it is only reachable
    // through `self`, so it never outlives the mapping.
    map: MmapMut,
    seg: SharedSeg,
}

impl Mapped {
    pub(crate) fn new(file: &File) -> io::Result<Mapped> {
        // SAFETY: the mapping is shared, so the other side's writes and ours go
        // to the same pages. Concurrent access from other processes is governed
        // by the ring protocol's atomics, the same contract as `SharedSeg` over
        // a Windows section. This crate never truncates the file while it is
        // mapped.
        let mut map = unsafe { MmapOptions::new().map_mut(file)? };
        let len = map.len();
        let ptr = map.as_mut_ptr();
        // SAFETY: `ptr` is valid for `len` bytes for as long as `map`, which
        // `Mapped` owns. Mappings are page-aligned, which satisfies the ring's
        // 8-byte atomics.
        let seg = unsafe { SharedSeg::from_raw(ptr, len) };
        Ok(Mapped { map, seg })
    }

    pub(crate) fn seg(&self) -> &SharedSeg {
        &self.seg
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn as_mut_ptr(&self) -> *mut u8 {
        // Intentional: the mapping is `MAP_SHARED` read/write and the ring
        // protocol writes through `&self`, so the const-to-mut cast mirrors
        // `SharedSeg`'s raw-pointer contract; no `&mut` alias is created.
        self.map.as_ptr() as *mut u8
    }
}
