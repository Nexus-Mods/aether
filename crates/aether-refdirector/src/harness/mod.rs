//! The shim tests' harness: [`aether_rpc::Tap`]s that record what crosses
//! the ring ([`Recorder`]), give the replies a director should not
//! ([`Faults`]) and change the fields of the ones it may ([`Override`]).
//!
//! Each is a handle over shared state (`Arc<Mutex<_>>`): a test keeps one
//! clone and serves through another, so what the serve thread writes the
//! test thread reads. The shim's fake director chains them as
//! `(recorder, (overrides, faults))`: both taps of a pair see every request
//! (`aether_rpc`'s pair), the recorder sees each reply last, as the client
//! gets it, and the faults first, as the director gave it.
//!
//! A lock here is never held while a fault sleeps or a rewrite runs, and one
//! poisoned by a panic elsewhere is taken anyway: a tap that panics fails its
//! request (the ring answers `Status::IoError`) and leaves the layers
//! working.
//!
//! ```text
//! Recorded { opcode: 3, flags: 0, payload: <OPEN request>, slot: 1,
//!            status: 0, reply: <24-byte OPEN reply> }
//! Match { op: Some(Opcode::Read), path_prefix: Some("data/"),
//!         nth: Some(2), times: None }        // the second READ under data/
//! ```

mod fault;
mod override_;
mod record;

pub use fault::{
    drop_last_name, set_reserved_open_flag_bits, ArenaShape, BulkLie, Fault, FaultId, Faults,
    Match, OwnedReplyBytes,
};
pub use override_::{OpenEdit, Override};
pub use record::{Mark, Recorded, Recorder};

use std::sync::{Mutex, MutexGuard, PoisonError};

/// `m`'s guard, poisoned or not: the state behind every lock here is valid
/// between statements, so a panic elsewhere leaves nothing half done.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
