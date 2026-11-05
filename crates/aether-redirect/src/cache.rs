//! The path cache, and the two thread-local scopes that decide what may be cached: the
//! caller's [`UncachedScope`] and the OS-consult re-entry guard.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::RwLock;

use crate::RootVpath;

thread_local! {
    /// Depth counter behind [`UncachedScope`]. A counter rather than a bare
    /// flag so nested or overlapping guards on the same thread compose
    /// correctly: an inner guard's `Drop` must not re-enable caching while an
    /// outer guard (from a caller further up the same call chain) is still
    /// held.
    static SUPPRESS_CACHE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub(crate) fn cache_suppressed() -> bool {
    SUPPRESS_CACHE_DEPTH.with(|c| c.get() > 0)
}

thread_local! {
    /// Guards [`RootMap::compute_under_root`]'s OS-consult branch
    /// (`expand_short_name`) against re-entering itself.
    ///
    /// `expand_short_name` (via `aether_win::final_path_for_open`) opens a real
    /// `CreateFileW` handle on the candidate path to ask the OS what it
    /// actually names. When this crate is consulted from inside an injected
    /// process whose own `NtCreateFile`/`NtOpenFile` are hooked (`aether-hooks`'s
    /// whole reason for existing), that `CreateFileW` call is itself
    /// intercepted and fed back through the very same decision path —
    /// `create_hook` -> `decision_for` -> `RootMap::under_root` ->
    /// `compute_under_root` — for the identical `~`-bearing path, which hits
    /// this same OS-consult branch again, which calls `expand_short_name`
    /// again, without bound. Verified by reproduction: an escape-matrix
    /// vector building an 8.3 short-name spelling of a path under a session's
    /// managed root (any temp-directory session-base name longer than 8.3,
    /// which every real session has: `vfs-daemon-<pid>-<seq>-<id>`) recursed
    /// until the injected process's stack overflowed (`STATUS_STACK_OVERFLOW`,
    /// `0xC00000FD`) — a real crash, not a misclassification, and one none of
    /// this crate's own unit tests can see, since a plain test process has no
    /// hook on `CreateFileW` for the recursion to loop through.
    ///
    /// The break: a re-entrant call finds the guard already held and skips
    /// the OS consult, answering "not recognised here" instead
    /// (`Resolution::OsConsulted(None)`). That does not lose the answer — it
    /// only refuses to ask the OS *again* for the same fact the outer call is
    /// already in the middle of asking. The re-entrant `CreateFileW`'s own
    /// hook invocation then falls through to the
    /// *real* trampoline, which is the actual, unhooked `NtCreateFile` this
    /// whole call chain was trying to reach — so `final_path_for_open`'s
    /// handle open still succeeds against the real filesystem, and the outer
    /// call's `expand_short_name` still returns the resolved long path
    /// exactly as it would have without the nested detour. Nothing is
    /// answered incorrectly; the second and every further attempt to
    /// re-derive the same fact from inside itself is simply skipped.
    static OS_CONSULT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII guard for [`OS_CONSULT_DEPTH`]. `enter()` returns `None` when the
/// guard is already held on this thread — the caller's signal to skip the OS
/// consult rather than recurse into it.
pub(crate) struct OsConsultGuard(());

impl OsConsultGuard {
    pub(crate) fn enter() -> Option<Self> {
        OS_CONSULT_DEPTH.with(|c| {
            if c.get() > 0 {
                None
            } else {
                c.set(1);
                Some(OsConsultGuard(()))
            }
        })
    }
}

impl Drop for OsConsultGuard {
    fn drop(&mut self) {
        OS_CONSULT_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// While one or more of these guards is alive on the current thread, every
/// [`RootMap::under_root`] lookup made on that thread is ineligible for
/// caching — including a lookup `compute_under_root` would otherwise classify
/// as [`Resolution::Deterministic`].
///
/// This exists for one situation `compute_under_root` cannot detect on its
/// own: a caller assembling `nt_path` from something that is itself a
/// snapshot of live, mutable state — for instance `GetFinalPathNameByHandleW`
/// on a directory handle, whose current target is a fact about the
/// filesystem *now*, not a property of the resulting string's bytes.
/// `compute_under_root`'s own `~`-gated OS-consulted tracking (see
/// [`Resolution`]) only catches paths *this crate* sent to the OS itself —
/// a path a caller already resolved via its own OS query before ever handing
/// it to `RootMap` looks, from here, exactly like an ordinary literal path.
/// Caching it under its own bytes would resurrect the same staleness bug
/// `Resolution::OsConsulted` exists to prevent (an 8.3 slot reused, a
/// junction retargeted — here, a handle's target renamed or replaced mid-
/// session), just arriving from outside this crate instead of from inside
/// `compute_under_root`. The caller who knows the provenance must say so
/// explicitly, by holding this guard for the duration of every
/// `RootMap`-backed decision it makes with such a path. See `aether-hooks`'s
/// `parent_dir_of_handle` for the concrete caller-side case.
#[must_use = "the suppression ends as soon as this guard is dropped"]
pub struct UncachedScope(());

impl UncachedScope {
    pub fn enter() -> Self {
        SUPPRESS_CACHE_DEPTH.with(|c| c.set(c.get() + 1));
        UncachedScope(())
    }
}

impl Drop for UncachedScope {
    fn drop(&mut self) {
        SUPPRESS_CACHE_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Default bound on [`PathCache`]'s entry count. Generous enough to hold every
/// distinct path a game session opens (the instrumentation this gate is
/// measured against shows opens repeat heavily, not that the *distinct* set is
/// huge), while still being a hard cap so a session that runs for hours cannot
/// grow the cache without limit.
pub(crate) const DEFAULT_CACHE_CAPACITY: usize = 4096;

/// A bounded cache from a raw NT open-path string to the [`RootMap::under_root`]
/// answer for it (`None` meaning "outside/malformed").
///
/// Keyed on the exact raw string a caller spelled — not on any normalized or
/// canonical form — because the point is to avoid *repeating* the work
/// (including a possible Win32 call; see `RootMap::compute_under_root`) that
/// turns a raw spelling into that answer, and the caller's own instrumentation
/// shows the same raw spelling opened over and over during a game's load.
///
/// # Thread safety
///
/// The shim is a DLL hooking calls inside a game process: many threads call
/// `under_root` concurrently, and this cache must never become a single point
/// where every open — including a cache *hit*, the overwhelmingly common case
/// once warm — is forced to wait for every other thread's open. A `Mutex`
/// guarding one shared map would do exactly that: hits and misses alike take
/// the same exclusive lock.
///
/// Instead this uses a `RwLock`: a hit only needs a *read* lock, so any number
/// of threads can look up a cached answer at the same time without blocking
/// each other. Only a genuine miss — the first time a raw spelling is seen, or
/// one that fell out of the bound — takes the brief exclusive write lock
/// needed to insert it.
///
/// Eviction is FIFO (oldest inserted, not least-recently-used), which is a
/// deliberate trade against a "smarter" LRU: LRU needs to bump an entry's
/// recency on every *hit*, which would force hits through the write lock too
/// and defeat the entire point of using a read lock for them. FIFO needs no
/// mutation on a hit at all, at the cost of being a worse eviction policy
/// under adversarial access patterns — an acceptable trade for a cache sized
/// to comfortably hold a real session's distinct paths.
pub(crate) struct PathCache {
    capacity: usize,
    state: RwLock<PathCacheState>,
}

#[derive(Default)]
pub(crate) struct PathCacheState {
    map: HashMap<String, Option<RootVpath>, BuildHasherDefault<FxHasher>>,
    order: VecDeque<String>,
}

/// The path cache's hash: rustc's FxHash, a word at a time, with its output
/// rotated so the bits the table indexes and tags by are mixed ones (as
/// rustc-hash 2 does). Not resistant to chosen collisions, and it need not
/// be: the keys are the paths this process opens, not input from across a
/// trust boundary. SipHash, the default, costs several times as much per byte
/// on every lookup, a hit included.
#[derive(Default, Clone, Copy)]
pub(crate) struct FxHasher {
    hash: u64,
}

impl FxHasher {
    const K: u64 = 0xf135_7aea_2e62_a9c5;

    fn add(&mut self, word: u64) {
        self.hash = self.hash.wrapping_add(word).wrapping_mul(Self::K);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.add(u64::from_le_bytes(*w));
        }
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            // The length too, so a tail of zeros is not the same as no tail.
            self.add(u64::from_le_bytes(last) ^ ((rest.len() as u64) << 59));
        }
    }

    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    fn finish(&self) -> u64 {
        self.hash.rotate_left(26)
    }
}

impl PathCache {
    pub(crate) fn new(capacity: usize) -> Self {
        PathCache {
            capacity: capacity.max(1),
            state: RwLock::new(PathCacheState::default()),
        }
    }

    /// A lock-poisoning thread (one that panicked while holding the lock) must
    /// not wedge every future open in a long-running game session — recover
    /// the guard rather than propagating the poison.
    /// The cached answer for `key`; a hit shares the cached vpath (one
    /// reference count), and allocates nothing.
    pub(crate) fn get(&self, key: &str) -> Option<Option<RootVpath>> {
        let guard = self.state.read().unwrap_or_else(|e| e.into_inner());
        guard.map.get(key).cloned()
    }

    /// Whether a cached `key` is under a root, read without cloning its hit.
    pub(crate) fn is_inside(&self, key: &str) -> Option<bool> {
        let guard = self.state.read().unwrap_or_else(|e| e.into_inner());
        guard.map.get(key).map(Option::is_some)
    }

    pub(crate) fn insert(&self, key: String, value: Option<RootVpath>) {
        let mut guard = self.state.write().unwrap_or_else(|e| e.into_inner());
        // Another thread may have raced this one to compute and insert the
        // same key; the first writer wins rather than double-counting it in
        // `order` (which would let the same key be evicted, then re-added,
        // silently exceeding the intended capacity accounting).
        if guard.map.contains_key(&key) {
            return;
        }
        if guard.order.len() >= self.capacity {
            if let Some(oldest) = guard.order.pop_front() {
                guard.map.remove(&oldest);
            }
        }
        guard.order.push_back(key.clone());
        guard.map.insert(key, value);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::FxHasher;
    use std::hash::{BuildHasher, BuildHasherDefault};

    fn hash(s: &str) -> u64 {
        BuildHasherDefault::<FxHasher>::default().hash_one(s)
    }

    /// Paths that differ only in their tail, their length or one letter
    /// hash apart, and the bits a hash table indexes and tags by spread.
    #[test]
    fn the_path_hash_tells_near_paths_apart() {
        assert_eq!(hash(r"\??\C:\Games\a.esp"), hash(r"\??\C:\Games\a.esp"));
        let mut paths = Vec::new();
        for i in 0..20_000 {
            paths.push(format!(r"\??\C:\Games\Skyrim\Data\Meshes\m{i}.nif"));
        }
        for tail in ["", "a", "a\0", "ab", "abcdefg", "abcdefgh", "abcdefghi"] {
            paths.push(format!(r"\??\C:\x{tail}"));
        }
        paths.push(r"\??\C:\Games\A.esp".into());
        paths.push(r"\??\C:\Games\a.esp".into());
        let hashes: std::collections::HashSet<u64> = paths.iter().map(|p| hash(p)).collect();
        assert_eq!(hashes.len(), paths.len(), "64-bit collisions");
        // hashbrown's tag is the top 7 bits; its bucket index the low ones.
        let tags: std::collections::HashSet<u64> = hashes.iter().map(|h| h >> 57).collect();
        let low: std::collections::HashSet<u64> = hashes.iter().map(|h| h & 0xfff).collect();
        assert_eq!(tags.len(), 128, "tags used");
        assert!(low.len() > 3900, "low 12 bits: {} of 4096 used", low.len());
    }
}
