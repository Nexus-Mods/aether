//! [`serve_threads`]: a reference served on several threads behind one tap
//! chain, as a director with several workers serves a ring (`docs/wire.md`
//! 2.8).

use super::ArenaShape;
use crate::Reference;
use aether_ipc::{DataArena, IpcError, Notifier, RingServer, SharedSeg};
use aether_rpc::{serve_tapped, Tap};
use std::sync::atomic::AtomicBool;
use std::thread::{Scope, ScopedJoinHandle};

/// Serves `d` on `seg` from `threads` threads in `scope` until `stop` is
/// set, each with its own `RingServer` (notified by what `notifier` makes),
/// its own handle on the reference ([`Reference::share`]) and its own clone
/// of `tap`, so the layers' shared state ([`super::Recorder`],
/// [`super::Faults`], [`super::Override`]) sees every thread's requests. With
/// `arena`, bulk READs are answered in its banks. Each thread returns what
/// [`aether_rpc::serve_tapped`] returns; join them all after setting `stop`
/// (and [`super::Faults::clear`], so no delay holds a thread).
///
/// Several threads take several requests at once, which is what
/// [`super::Recorder::max_in_flight`] counts; the ring's workers hint
/// (`aether_ipc::ring::set_worker_hint`) is the caller's to set.
#[allow(clippy::too_many_arguments)]
pub fn serve_threads<'scope, 'env, N, F, T>(
    scope: &'scope Scope<'scope, 'env>,
    threads: usize,
    seg: &'env SharedSeg,
    arena: Option<ArenaShape>,
    notifier: &'env F,
    d: &Reference,
    tap: &T,
    stop: &'env AtomicBool,
) -> Vec<ScopedJoinHandle<'scope, Result<(), IpcError>>>
where
    N: Notifier,
    F: Fn() -> N + Sync,
    T: Tap + Clone + Send + 'scope,
{
    (0..threads)
        .map(|_| {
            let mut d = d.share();
            let mut tap = tap.clone();
            scope.spawn(move || {
                let server = RingServer::new(seg, notifier())?;
                let arena = arena.map(|a| {
                    DataArena::new(
                        seg,
                        a.offset as usize,
                        (a.bank_size * u64::from(a.banks)) as usize,
                        a.banks as usize,
                    )
                });
                serve_tapped(&server, arena.as_ref(), &mut d, &mut tap, stop)
            })
        })
        .collect()
}
