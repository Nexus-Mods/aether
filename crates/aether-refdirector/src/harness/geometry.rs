//! [`Geometry`]: how a test lays out a ring and its arena, the layout a
//! director uses (`docs/wire.md` 2.1, 2.6): the ring first, the arena after
//! it, one bank per slot.
//!
//! ```text
//! Geometry::SHIM_TESTS   slots 8, payload_cap 4096, arena 2 MiB, gap 0
//!   arena_offset()  = 48 + 8 * align8(32 + 4096)     = 33 072
//!   map_bytes()     = max(round_up(offset + arena, 64 KiB), 256 KiB)
//!   arena()         = ArenaShape { offset: 33 072, bank_size: 256 KiB, banks: 8 }
//! ```

use super::ArenaShape;
use aether_ipc::{IpcError, SharedSeg};

/// A ring's slot count and payload cap, and its arena's length and place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// The ring's slot count.
    pub slots: u32,
    /// The ring's `payload_cap`.
    pub payload_cap: u32,
    /// The arena's length, banked per slot; 0 is no arena (every READ
    /// inline).
    pub arena_len: usize,
    /// Bytes between the end of the last slot and the arena's start: 0 puts
    /// the arena right after the ring, as directors do (`docs/wire.md` 2.1).
    pub arena_gap: usize,
}

impl Geometry {
    /// The geometry the shim's tests serve, and the shim's counted costs are
    /// recorded at: 8 slots, a 4 KiB cap (an inline READ holds 4088 bytes, so
    /// a file larger than that spans several round trips), and 8 banks of
    /// 256 KiB, so the client's bank size (`arena_len / slot_count`, at least
    /// 256 KiB) agrees exactly with the one the server hands out.
    pub const SHIM_TESTS: Geometry = Geometry {
        slots: 8,
        payload_cap: 4096,
        arena_len: 8 * 256 * 1024,
        arena_gap: 0,
    };

    /// The control ring's length (header and slots).
    pub fn ring_bytes(&self) -> usize {
        let stride = (32 + self.payload_cap as usize).next_multiple_of(8);
        aether_ipc::layout::RING_HEADER_SIZE + self.slots as usize * stride
    }

    /// The arena's mapping offset (`VFS_ARENA_OFFSET`).
    pub fn arena_offset(&self) -> usize {
        self.ring_bytes() + self.arena_gap
    }

    /// The whole mapping (`VFS_RING_BYTES`): ring and arena, rounded up to
    /// 64 KiB, at least 256 KiB. It names all of it because the shim maps
    /// all of it and a bulk reply's offset is mapping-absolute.
    pub fn map_bytes(&self) -> usize {
        ((self.arena_offset() + self.arena_len + 0xFFFF) & !0xFFFF).max(256 * 1024)
    }

    /// The arena as the serve threads bank it (one bank per slot), or `None`
    /// without one.
    pub fn arena(&self) -> Option<ArenaShape> {
        (self.arena_len > 0).then(|| ArenaShape {
            offset: self.arena_offset() as u64,
            bank_size: (self.arena_len / self.slots as usize) as u64,
            banks: self.slots,
        })
    }

    /// Lay out an empty ring in `seg` (at least [`Self::map_bytes`] long) and
    /// state `worker_hint` in its header when it is not 0
    /// (`docs/wire.md` 2.8).
    pub fn init(&self, seg: &SharedSeg, worker_hint: u32) -> Result<(), IpcError> {
        aether_ipc::ring::init(seg, self.slots, self.payload_cap)?;
        if worker_hint > 0 {
            aether_ipc::ring::set_worker_hint(seg, worker_hint);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shim_tests_geometry_banks_its_arena_after_the_ring() {
        let g = Geometry::SHIM_TESTS;
        assert_eq!(g.ring_bytes(), 48 + 8 * 4128);
        assert_eq!(g.arena_offset(), g.ring_bytes());
        assert_eq!(g.map_bytes() % 0x10000, 0);
        assert!(g.map_bytes() >= g.arena_offset() + g.arena_len);
        let a = g.arena().unwrap();
        assert_eq!((a.bank_size, a.banks), (256 * 1024, 8));
    }

    #[test]
    fn no_arena_is_none_and_a_small_map_is_256_kib() {
        let g = Geometry {
            arena_len: 0,
            ..Geometry::SHIM_TESTS
        };
        assert!(g.arena().is_none());
        assert_eq!(g.map_bytes(), 256 * 1024);
    }
}
