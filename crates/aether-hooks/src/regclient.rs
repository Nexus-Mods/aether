//! The shim's registry client: the registry hooks' one way to the director's registry overlay
//! (ring opcodes 15-22, `docs/wire.md` section 4).
//!
//! **Caching.** `REG_LOOKUP` and `REG_KEY` answers are cached per path. The
//! director publishes a registry *generation* in the ring header that every injected process of
//! the session maps, and moves it on every registry write (before the writer gets its reply)
//! and whenever a layer is attached or detached.
//! An answer is cached under the generation read **before** it was asked for, and used only
//! while the published generation still equals it. So a write by any process makes every
//! process's cached answers unusable before that write returns, and a cache hit costs one
//! atomic load and no round trip.
//!
//! An answer may be newer than the generation it is cached under (a write landed between the
//! read of the generation and the director's answer). That is harmless: the write moves the
//! generation, so the entry is never used past it.
//!
//! **Errors.** Every call returns the director's status as `Err`: a request
//! that could not be sent or was never answered is `Status::IoError`, a director with no
//! registry attached `Status::NotSupported`, a `REG_KEY` answer too large for the ring
//! `Status::ReplyTooLarge`. A failed read is counted as a fallback in the shim stats; the caller
//! then serves the real key alone. A failed write is never retried against the real registry;
//! the caller returns `STATUS_UNSUCCESSFUL`.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, OnceLock};

use aether_protocol::{Reply, Request, SliceBuf, Status};
use aether_registry::{path::fold, Lookup, Node};
use aether_rpc::CallError;

use crate::director::{self, FuseClient};

/// Entries per map before the cache starts over. A game reads far fewer distinct keys than
/// this between two writes; the bound only stops a pathological enumeration from growing it
/// without end.
const CACHE_ENTRIES: usize = 4096;

/// Registry virtualisation is on for this process: the host set [`aether_env::REGISTRY`] (it does
/// so only while a registry layer is attached), every registry detour was installed
/// ([`detours_installed`]), and the shim has a director to ask.
///
/// **All or nothing.** Until the install has recorded its outcome this is false, so every
/// registry hook passes straight through; once recorded, the outcome never changes. A process
/// where any registry detour is missing never virtualises its registry at all: a missing write
/// hook would otherwise let writes through a virtual view reach the real registry.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    DETOURS.get() == Some(&Ok(()))
        && *ON.get_or_init(|| aether_env::opt_in(aether_env::REGISTRY))
        && director::global().is_some()
}

/// The outcome of installing the registry detours: `Err(label)` names one that is missing.
/// Set once, after the last registry detour is in.
static DETOURS: OnceLock<Result<(), &'static str>> = OnceLock::new();

/// Whether registry virtualisation may run, given the registry detours that could not be
/// installed: only when none is missing. `Err` names the first one.
pub fn decide(missing: &[&'static str]) -> Result<(), &'static str> {
    match missing.first() {
        Some(m) => Err(m),
        None => Ok(()),
    }
}

/// Record the registry detours' install outcome (once per process; a later call changes
/// nothing). Called by the install after its last registry detour, so no hook sees
/// [`enabled`] go from true to false. A disabled overlay that the host asked for is reported:
/// a log line and the stats flag.
pub fn detours_installed(missing: &[&'static str]) {
    let outcome = decide(missing);
    if DETOURS.set(outcome).is_err() {
        return;
    }
    if let Err(name) = outcome {
        crate::hookstats::note_reg_overlay_disabled(name);
        if aether_env::opt_in(aether_env::REGISTRY) {
            eprintln!("vfs-shim: registry overlay disabled: detour {name} not installed");
        }
    }
}

/// The `Err` label [`overlay_off`] records: the host did not ask for the overlay.
pub const OFF: &str = "VFS_REGISTRY unset";

/// Record that the registry detours were not installed because the host did not turn the
/// overlay on (`VFS_REGISTRY` unset): [`enabled`] stays false for the process. Not a disabled
/// overlay, so nothing is logged and the stats flag stays clear.
pub fn overlay_off() {
    let _ = DETOURS.set(Err(OFF));
}

/// The recorded registry detour outcome: `None` before the install recorded one, `Err(OFF)`
/// when the overlay was off, `Err(name)` when detour `name` was missing. For tests and
/// diagnostics.
pub fn detours_outcome() -> Option<Result<(), &'static str>> {
    DETOURS.get().copied()
}

/// The process's registry client, over [`director::global`].
pub fn global() -> Option<&'static RegClient<'static>> {
    static CLIENT: OnceLock<RegClient<'static>> = OnceLock::new();
    let fc = director::global()?;
    Some(CLIENT.get_or_init(|| RegClient::new(fc)))
}

/// `REG_LOOKUP` through [`global`]: the key's overlay state and whether the overlay holds
/// anything below it. Cached.
pub fn lookup(path: &str) -> Result<(Lookup, bool), Status> {
    match global() {
        Some(c) => c.lookup(path),
        None => read_failed(Status::NotSupported),
    }
}

/// `REG_KEY` through [`global`]: the key's overlay node, if it has one. Cached.
pub fn key(path: &str) -> Result<Option<Node>, Status> {
    match global() {
        Some(c) => c.key(path),
        None => read_failed(Status::NotSupported),
    }
}

/// `REG_KEY` through [`global`], with `f` run on the (cached) node in place instead of a clone.
/// `f` runs under the cache lock: it must be pure (no trampolines, no director, no caller
/// memory).
pub fn with_key<R>(path: &str, f: impl FnOnce(Option<&Node>) -> R) -> Result<R, Status> {
    match global() {
        Some(c) => c.with_key(path, f),
        None => read_failed(Status::NotSupported),
    }
}

/// The registry generation the director last published (0: none, so nothing is cached). An
/// answer read after this returned it is current for as long as it stays the same.
pub fn generation() -> u64 {
    director::global().map_or(0, |c| c.reg_generation())
}

pub fn set_value(path: &str, name: &str, ty: u32, data: &[u8]) -> Result<(), Status> {
    global()
        .ok_or(Status::NotSupported)?
        .set_value(path, name, ty, data)
}

pub fn delete_value(path: &str, name: &str) -> Result<(), Status> {
    global()
        .ok_or(Status::NotSupported)?
        .delete_value(path, name)
}

pub fn create_key(path: &str, volatile: bool) -> Result<(), Status> {
    global()
        .ok_or(Status::NotSupported)?
        .create_key(path, volatile)
}

pub fn delete_key(path: &str) -> Result<(), Status> {
    global().ok_or(Status::NotSupported)?.delete_key(path)
}

pub fn rename_key(path: &str, new_leaf: &str) -> Result<(), Status> {
    global()
        .ok_or(Status::NotSupported)?
        .rename_key(path, new_leaf)
}

/// `REG_CHANGED` through [`global`]: whether the key (and with `subtree` anything below it)
/// changed after overlay version `since`, and the current overlay version. Never cached.
pub fn changed(path: &str, subtree: bool, since: u64) -> Result<(bool, u64), Status> {
    global()
        .ok_or(Status::NotSupported)?
        .changed(path, subtree, since)
}

/// Count a failed read as a fallback to the real registry, and return its status. A read made
/// for a write ([`WriteScope`]) is not served from the real registry: the write is refused, and
/// counted as such.
fn read_failed<T>(status: Status) -> Result<T, Status> {
    if IN_WRITE.with(|w| w.get()) {
        crate::hookstats::note_reg_write_refused();
    } else {
        crate::hookstats::note_reg_read_fallback();
    }
    Err(status)
}

thread_local! {
    static IN_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// While held, this thread is answering a registry write: a director request that fails is
/// counted as a refused write, never as a read fallback (the write hooks hold one for the call).
pub struct WriteScope(bool);

impl WriteScope {
    pub fn enter() -> WriteScope {
        WriteScope(IN_WRITE.with(|w| w.replace(true)))
    }
}

impl Drop for WriteScope {
    fn drop(&mut self) {
        IN_WRITE.with(|w| w.set(self.0));
    }
}

/// The cached answers of one registry generation.
#[derive(Default)]
struct Cache {
    generation: u64,
    lookups: HashMap<String, (Lookup, bool)>,
    keys: HashMap<String, Option<Node>>,
}

impl Cache {
    /// Bring the cache to `generation` (dropping everything when it is newer than what the
    /// cache holds) and say whether its entries are answers for that generation. A reader
    /// holding an older generation than the cache's is told no: it neither uses nor adds.
    fn at(&mut self, generation: u64) -> bool {
        if generation > self.generation {
            self.lookups.clear();
            self.keys.clear();
            self.generation = generation;
        }
        generation == self.generation
    }
}

/// A registry client over one ring: the requests, and the cache of read answers.
///
/// The process has one, [`global`]; a test makes more to play several processes.
pub struct RegClient<'a> {
    fc: &'a FuseClient,
    cache: Mutex<Cache>,
}

impl<'a> RegClient<'a> {
    pub fn new(fc: &'a FuseClient) -> Self {
        RegClient {
            fc,
            cache: Mutex::new(Cache::default()),
        }
    }

    pub fn lookup(&self, path: &str) -> Result<(Lookup, bool), Status> {
        self.cached(
            path,
            |c| &mut c.lookups,
            || {
                // A lookup state outside the table does not decode: a read failure, so the
                // caller serves the real key rather than a guess.
                self.read(&Request::RegLookup { path }, |r| match r {
                    Reply::RegLookup(l) => Some((l.state, l.below)),
                    _ => None,
                })
            },
        )
    }

    pub fn key(&self, path: &str) -> Result<Option<Node>, Status> {
        self.cached(path, |c| &mut c.keys, || self.fetch_key(path))
    }

    /// [`RegClient::key`] with `f` run on the cached node in place (under the cache lock).
    pub fn with_key<R>(&self, path: &str, f: impl FnOnce(Option<&Node>) -> R) -> Result<R, Status> {
        let generation = self.fc.reg_generation();
        let k = fold(path);
        if generation != 0 {
            if let Ok(mut c) = self.cache.lock() {
                if c.at(generation) {
                    if let Some(v) = c.keys.get(&k) {
                        return Ok(f(v.as_ref()));
                    }
                }
            }
        }
        let v = match self.fetch_key(path) {
            Ok(v) => v,
            Err(st) => return read_failed(st),
        };
        let r = f(v.as_ref());
        if generation != 0 {
            if let Ok(mut c) = self.cache.lock() {
                if c.at(generation) {
                    insert_bounded(&mut c.keys, k, v);
                }
            }
        }
        Ok(r)
    }

    /// One `REG_KEY` round trip: the key's node, owned (the reply decodes it).
    fn fetch_key(&self, path: &str) -> Result<Option<Node>, Status> {
        self.read(&Request::RegKey { path }, |r| match r {
            Reply::RegKey(k) => Some(k.node.map(std::borrow::Cow::into_owned)),
            _ => None,
        })
    }

    pub fn set_value(&self, path: &str, name: &str, ty: u32, data: &[u8]) -> Result<(), Status> {
        self.write(&Request::RegSetValue {
            path,
            name,
            ty,
            data,
        })
    }

    pub fn delete_value(&self, path: &str, name: &str) -> Result<(), Status> {
        self.write(&Request::RegDeleteValue { path, name })
    }

    pub fn create_key(&self, path: &str, volatile: bool) -> Result<(), Status> {
        self.write(&Request::RegCreateKey { path, volatile })
    }

    pub fn delete_key(&self, path: &str) -> Result<(), Status> {
        self.write(&Request::RegDeleteKey { path })
    }

    pub fn rename_key(&self, path: &str, new_leaf: &str) -> Result<(), Status> {
        self.write(&Request::RegRenameKey { path, new_leaf })
    }

    pub fn changed(&self, path: &str, subtree: bool, since: u64) -> Result<(bool, u64), Status> {
        let req = Request::RegChanged {
            path,
            subtree,
            since,
        };
        self.read(&req, |r| match r {
            Reply::RegChanged(c) => Some((c.changed, c.version)),
            _ => None,
        })
    }

    /// Send one request. One that cannot fit the ring's payload is refused here with
    /// `Status::BadRequest` and never submitted (the hooks answer `STATUS_INVALID_PARAMETER`,
    /// as for anything over the registry limits, `docs/wire.md` 4.1).
    fn request<T>(
        &self,
        req: &Request<'_>,
        read: impl FnOnce(Reply<'_>) -> Option<T>,
    ) -> Result<T, CallError> {
        if !fits(encoded_len(req), self.fc.payload_cap()) {
            return Err(CallError::Status(Status::BadRequest));
        }
        self.fc.call(req, read)
    }

    /// A read: [`Self::request`], whose failure is a status. An OK reply that does not
    /// decode is `Status::BadRequest`.
    fn read<T>(
        &self,
        req: &Request<'_>,
        read: impl FnOnce(Reply<'_>) -> Option<T>,
    ) -> Result<T, Status> {
        self.request(req, read).map_err(director::failed)
    }

    /// A write. Nothing to invalidate here: the director moves the published generation
    /// before it replies, which makes every cached answer, this client's included, unusable.
    fn write(&self, req: &Request<'_>) -> Result<(), Status> {
        let reply = self.request(req, |r| match r {
            Reply::RegVersion(_) => Some(()),
            _ => None,
        });
        match reply {
            Ok(()) => Ok(()),
            // A reply that does not decode says nothing about the request: a failed write
            // (`STATUS_UNSUCCESSFUL`), not a bad request (`STATUS_INVALID_PARAMETER`).
            Err(CallError::Undecodable) => {
                crate::hookstats::note_reg_write_refused();
                Err(Status::IoError)
            }
            Err(e) => {
                let st = director::failed(e);
                // The director's own answers (over a limit, exists, deleted) are not failures
                // to reach it.
                if !matches!(st, Status::BadRequest | Status::Exists | Status::NotFound) {
                    crate::hookstats::note_reg_write_refused();
                }
                Err(st)
            }
        }
    }

    /// A read answered from `map` while the published generation is the one the answer was
    /// cached under, otherwise by `fetch` (and cached). Generation 0 means the director
    /// published nothing, so nothing is cached.
    fn cached<T: Clone>(
        &self,
        path: &str,
        map: fn(&mut Cache) -> &mut HashMap<String, T>,
        fetch: impl FnOnce() -> Result<T, Status>,
    ) -> Result<T, Status> {
        let generation = self.fc.reg_generation();
        let k = fold(path);
        if generation != 0 {
            if let Ok(mut c) = self.cache.lock() {
                if c.at(generation) {
                    if let Some(v) = map(&mut c).get(&k) {
                        return Ok(v.clone());
                    }
                }
            }
        }
        let v = match fetch() {
            Ok(v) => v,
            Err(st) => return read_failed(st),
        };
        if generation != 0 {
            if let Ok(mut c) = self.cache.lock() {
                if c.at(generation) {
                    insert_bounded(map(&mut c), k, v.clone());
                }
            }
        }
        Ok(v)
    }
}

/// How many bytes `req` encodes to, counted without keeping them: an empty
/// [`SliceBuf`] writes nothing and reports the length the message needed.
fn encoded_len(req: &Request<'_>) -> usize {
    let mut count = SliceBuf::new(&mut []);
    req.encode(&mut count);
    match count.finish() {
        Ok(n) => n,
        Err(o) => o.needed,
    }
}

/// A request of `len` bytes fits one ring payload of `cap` bytes.
fn fits(len: usize, cap: u32) -> bool {
    len <= cap as usize
}

fn insert_bounded<K: Eq + Hash, V>(m: &mut HashMap<K, V>, k: K, v: V) {
    if m.len() >= CACHE_ENTRIES {
        m.clear();
    }
    m.insert(k, v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overlay_runs_only_when_every_registry_detour_is_in() {
        assert_eq!(decide(&[]), Ok(()));
        assert_eq!(decide(&["NtRenameKey"]), Err("NtRenameKey"));
        assert_eq!(
            decide(&["NtSetValueKey", "NtFlushKey"]),
            Err("NtSetValueKey")
        );
    }

    #[test]
    fn a_request_fits_up_to_the_payload_cap_and_not_beyond() {
        assert!(fits(0, 4096));
        assert!(fits(4096, 4096));
        assert!(!fits(4097, 4096));
        // The set-value request for a value of exactly the 1 MiB data limit does not fit
        // a 1 MiB payload: its path and name come on top.
        let data = vec![0; 1 << 20];
        let req = Request::RegSetValue {
            path: r"\Registry\Machine\K",
            name: "v",
            ty: 3,
            data: &data,
        };
        assert!(!fits(encoded_len(&req), 1 << 20));
        // The count is the encoding's length.
        let mut bytes = Vec::new();
        req.encode(&mut bytes);
        assert_eq!(encoded_len(&req), bytes.len());
    }
}
