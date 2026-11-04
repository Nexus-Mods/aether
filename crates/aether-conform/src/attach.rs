//! Attaching to a director's ring from the handshake variables a mount
//! answered with, the way the shim attaches (`docs/wire.md` 2.4, 2.9, 2.10,
//! 5.3; `crates/aether-hooks/src/director.rs`, `try_init_from_env` and
//! `connect_source`).
//!
//! - **Which ring.** A non-empty `VFS_RING_PATH` wins over
//!   `VFS_RING_SECTION` (2.10: "`VFS_RING_PATH` wins over
//!   `VFS_RING_SECTION` when both are set; an empty `VFS_RING_PATH` counts
//!   as unset"). A file is mapped with [`aether_filering::FileRing::open`]
//!   and must be at least `VFS_RING_BYTES` long; a section (Windows only) is
//!   opened by name with `aether_win::SharedMapping::open` at
//!   `VFS_RING_BYTES`.
//! - **Numbers.** `VFS_RING_BYTES`, `VFS_RING_PAYLOAD_CAP` and
//!   `VFS_ARENA_LEN` are read with the shim's defaults when unparsable
//!   (2 097 152, 1 048 576, 0).
//! - **Roots.** The roots must be ones the shim accepts ([`shim_roots`]):
//!   `VFS_VIRTUAL_DIR` set, and every root the shim builds from it,
//!   `VFS_VIRTUAL_ROOTS`, `VFS_LAUNCH_IMAGE` and `VFS_ROOT_BACKINGS` usable
//!   by `aether_redirect::RootMap::with_roots`, the call the shim makes. The
//!   header is checked as 2.4 says, and beyond what the shim
//!   checks, two rules a director must meet: the header's `payload_cap`
//!   equals `VFS_RING_PAYLOAD_CAP` (5.3: "Must equal the header's
//!   `payload_cap`") and, when `VFS_ARENA_OFFSET` is given, the arena lies
//!   inside the first `VFS_RING_BYTES` bytes (2.6).
//! - **Waking** (2.11). On a file ring nothing is signalled: the client
//!   spins, as the shim does, and the director must not sleep on an event.
//!   On a section the client opens `VFS_SERVER_EV` with the shim's rights
//!   (`EVENT_MODIFY_STATE`, `aether_win::WakeEvent`) and sets it after
//!   publishing a request and after freeing a slot, as the shim does; a
//!   failed open is tolerated (no wake), as it is in the shim. Either way a
//!   reply that has not come within the ring's spin budget is waited for by
//!   yielding and then sleeping a millisecond at a time.

use std::path::Path;
use std::time::Duration;

use aether_ipc::{ring, Geom, Notifier, RingBacking, RingClient, SharedSeg};
use aether_protocol::RootId;
use aether_redirect::{RootMap, VolumeMap};
use aether_rpc::Client;

use crate::adapter::Env;

/// The shim's defaults for numbers it cannot parse (`try_init_from_env`).
const DEFAULT_RING_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_PAYLOAD_CAP: u32 = 1_048_576;

/// After this long without a reply, a wait sleeps instead of yielding (the
/// shim's `IDLE_YIELD`).
const IDLE_YIELD: Duration = Duration::from_millis(20);

enum Backing {
    File(aether_filering::FileRing),
    #[cfg(windows)]
    Section(aether_win::SharedMapping),
}

impl Backing {
    fn seg(&self) -> &SharedSeg {
        match self {
            Backing::File(f) => f.seg(),
            #[cfg(windows)]
            Backing::Section(s) => s.seg(),
        }
    }
}

/// A ring attached from a mount's environment.
pub struct Attached {
    backing: Backing,
    geom: Geom,
    ring_bytes: usize,
    arena_len: usize,
    #[cfg(windows)]
    server_ev: Option<aether_win::WakeEvent>,
    deadline: Duration,
    what: String,
}

impl Attached {
    /// Attaches to the ring `env` names. `deadline` bounds every request.
    pub fn from_env(env: &Env, deadline: Duration) -> Result<Attached, String> {
        let get = |k: &str| env.get(k).cloned();
        let ring_bytes: usize = get(aether_env::RING_BYTES)
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RING_BYTES);
        let payload_cap: u32 = get(aether_env::RING_PAYLOAD_CAP)
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_PAYLOAD_CAP);
        let arena_len: usize = get(aether_env::ARENA_LEN)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        shim_roots(env)?;
        let path = get(aether_env::RING_PATH).filter(|p| !p.is_empty());
        let section = get(aether_env::RING_SECTION);
        let (backing, what) = match (path, section) {
            (Some(path), _) => {
                let f = aether_filering::FileRing::open(Path::new(&path))
                    .map_err(|e| format!("open ring file {path}: {e}"))?;
                if f.len() < ring_bytes {
                    return Err(format!(
                        "ring file {path} is {} bytes, shorter than {}={ring_bytes} \
                         (the shim refuses it, wire.md 2.10)",
                        f.len(),
                        aether_env::RING_BYTES
                    ));
                }
                (Backing::File(f), format!("file {path}"))
            }
            (None, Some(name)) => open_section(&name, ring_bytes)?,
            (None, None) => {
                return Err(format!(
                    "neither {} nor {} is set",
                    aether_env::RING_PATH,
                    aether_env::RING_SECTION
                ))
            }
        };
        let geom = ring::open(backing.seg()).map_err(|e| format!("ring open: {e:?}"))?;
        let ring_end = aether_ipc::layout::RING_HEADER_SIZE
            + geom.slot_count as usize * geom.slot_stride as usize;
        if ring_end > ring_bytes {
            return Err(format!(
                "the ring's slots end at {ring_end}, past {}={ring_bytes} (wire.md 2.4)",
                aether_env::RING_BYTES
            ));
        }
        if geom.payload_cap != payload_cap {
            return Err(format!(
                "the header's payload_cap is {}, {} says {payload_cap} (wire.md 5.3: they must be equal)",
                geom.payload_cap,
                aether_env::RING_PAYLOAD_CAP
            ));
        }
        if let Some(off) = get(aether_env::ARENA_OFFSET).and_then(|s| s.parse::<usize>().ok()) {
            if arena_len > 0 && off.saturating_add(arena_len) > ring_bytes {
                return Err(format!(
                    "the arena ({}={off}, {}={arena_len}) runs past {}={ring_bytes} (wire.md 2.6)",
                    aether_env::ARENA_OFFSET,
                    aether_env::ARENA_LEN,
                    aether_env::RING_BYTES
                ));
            }
        }
        #[cfg(windows)]
        let server_ev = match &backing {
            Backing::Section(_) => open_server_event(env),
            Backing::File(_) => None,
        };
        Ok(Attached {
            backing,
            geom,
            ring_bytes,
            arena_len,
            #[cfg(windows)]
            server_ev,
            deadline,
            what,
        })
    }

    /// A typed client on the ring.
    pub fn client(&self) -> Client<'_, Wake<'_>> {
        let wake = Wake {
            #[cfg(windows)]
            server_ev: self.server_ev.as_ref(),
            _seg: std::marker::PhantomData,
        };
        Client::new(
            RingClient::with_geom(self.backing.seg(), self.geom, wake).with_deadline(self.deadline),
        )
    }

    pub fn seg(&self) -> &SharedSeg {
        self.backing.seg()
    }

    /// The ring's geometry, as its header gave it.
    pub fn geom(&self) -> Geom {
        self.geom
    }

    /// The notifier the client uses, for a request sent without it (a
    /// header the ring client would refuse to write).
    pub fn wake(&self) -> Wake<'_> {
        Wake {
            #[cfg(windows)]
            server_ev: self.server_ev.as_ref(),
            _seg: std::marker::PhantomData,
        }
    }

    /// How long a request may take.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// `VFS_RING_BYTES`: how much of the mapping the shim would map.
    pub fn ring_bytes(&self) -> usize {
        self.ring_bytes
    }

    /// `VFS_ARENA_LEN`; 0 means the shim sends no bulk READ.
    pub fn arena_len(&self) -> usize {
        self.arena_len
    }

    /// Which ring this is, for messages.
    pub fn describe(&self) -> &str {
        &self.what
    }
}

#[cfg(windows)]
fn open_section(name: &str, ring_bytes: usize) -> Result<(Backing, String), String> {
    let s = aether_win::SharedMapping::open(name, ring_bytes)
        .map_err(|e| format!("open section {name}: {e}"))?;
    Ok((Backing::Section(s), format!("section {name}")))
}

#[cfg(not(windows))]
fn open_section(name: &str, _ring_bytes: usize) -> Result<(Backing, String), String> {
    Err(format!(
        "{}={name} names a Windows section, which this OS cannot open; give {} (wire.md 2.10)",
        aether_env::RING_SECTION,
        aether_env::RING_PATH
    ))
}

/// The director's wake event, if `VFS_SERVER_EV` names one that opens with
/// `EVENT_MODIFY_STATE`, the rights the shim asks for
/// (`crates/aether-hooks/src/director.rs`, `connect_source`). The client
/// event is not opened, as the shim does not open it.
#[cfg(windows)]
fn open_server_event(env: &Env) -> Option<aether_win::WakeEvent> {
    aether_win::WakeEvent::open(env.get(aether_env::SERVER_EV)?).ok()
}

/// Whether the shim would accept the roots `env` declares, checked as the
/// shim checks them (`crates/aether-hooks/src/director.rs`):
/// `try_init_from_env` refuses an unset `VFS_VIRTUAL_DIR`; `roots_from_env`
/// (`merge_extra_roots`) adds `VFS_VIRTUAL_ROOTS`; `connect_source` adds the
/// staged launch directory (`stage_root_from_env`) and the backings
/// (`backings_from_env`, `parse_backings`) and hands them all to
/// `RootMap::with_roots`, whose error fails the load ("managed root is not a
/// usable path"). That last call is the shim's own, from `aether-redirect`;
/// the parsing before it is copied from `director.rs`, which lives in a
/// Windows-only crate. The volume map is left empty: it adds aliases, never
/// refuses a root.
pub fn shim_roots(env: &Env) -> Result<(), String> {
    let get = |k: &str| env.get(k).map(String::as_str);
    let Some(virtual_dir) = get(aether_env::VIRTUAL_DIR) else {
        return Err(format!(
            "{} is not set: the shim fails its load without it (wire.md 5.3)",
            aether_env::VIRTUAL_DIR
        ));
    };
    let mut routing = merge_extra_roots(virtual_dir, get(aether_env::VIRTUAL_ROOTS));
    if let Some(stage) = get(aether_env::LAUNCH_IMAGE).and_then(stage_root) {
        routing.push((RootId::DEFAULT, stage));
    }
    routing.extend(parse_backings(get(aether_env::ROOT_BACKINGS)));
    let refs: Vec<(RootId, &str)> = routing.iter().map(|(id, p)| (*id, p.as_str())).collect();
    RootMap::with_roots(&refs, VolumeMap::empty())
        .map(|_| ())
        .map_err(|e| {
            format!(
            "the shim refuses these roots (managed root is not a usable path: {e:?}): {routing:?}"
        )
        })
}

/// `director.rs`, `merge_extra_roots`.
fn merge_extra_roots(virtual_dir: &str, spec: Option<&str>) -> Vec<(RootId, String)> {
    let mut out: Vec<(RootId, String)> = vec![(RootId::DEFAULT, virtual_dir.to_string())];
    let Some(extra) = spec else {
        return out;
    };
    for entry in extra.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((id, path)) = entry.split_once('=') else {
            continue;
        };
        let (Ok(id), path) = (id.trim().parse::<u32>(), path.trim()) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        match out.iter_mut().find(|(r, _)| r.0 == id) {
            Some(slot) => slot.1 = path.to_string(),
            None => out.push((RootId(id), path.to_string())),
        }
    }
    out
}

/// `director.rs`, `parse_backings`.
fn parse_backings(spec: Option<&str>) -> Vec<(RootId, String)> {
    spec.unwrap_or_default()
        .split(';')
        .filter_map(|entry| {
            let (id, path) = entry.trim().split_once('=')?;
            let id = id.trim().parse::<u32>().ok()?;
            let path = path.trim();
            (!path.is_empty()).then(|| (RootId(id), normalize_path_for_root(path)))
        })
        .collect()
}

/// `director.rs`, `stage_root_from_env`: the launch image's directory,
/// which the shim takes with a Windows `Path::parent`. This reproduces that
/// for the forms a director gives: `C:\dir\game.exe` gives `C:\dir`,
/// `C:\game.exe` gives `C:\`, a drive-less rooted `\game.exe` or
/// `/game.exe` gives `\` (which the shim then refuses as an empty root),
/// and a bare `game.exe` gives nothing (the shim's empty parent, skipped).
fn stage_root(image: &str) -> Option<String> {
    let cut = image.rfind(['\\', '/'])?;
    let mut dir = image[..cut].to_string();
    if dir.is_empty() {
        // The separator is the root itself.
        dir.push('\\');
    } else if dir.len() == 2 && dir.ends_with(':') {
        dir.push('\\');
    }
    Some(normalize_path_for_root(&dir))
}

/// `director.rs`, `normalize_path_for_root` (with `strip_nt_device`).
fn normalize_path_for_root(p: &str) -> String {
    let p = p
        .strip_prefix(r"\??\")
        .or_else(|| p.strip_prefix(r"\\?\"))
        .unwrap_or(p);
    p.replace('/', "\\").to_ascii_lowercase()
}

/// The client's notifier: wakes the director's event on a section, nothing
/// on a file, and waits for a reply by spinning, then yielding, then
/// sleeping.
pub struct Wake<'a> {
    #[cfg(windows)]
    server_ev: Option<&'a aether_win::WakeEvent>,
    _seg: std::marker::PhantomData<&'a ()>,
}

impl Notifier for Wake<'_> {
    fn notify_server(&self) {
        #[cfg(windows)]
        if let Some(ev) = self.server_ev {
            ev.set();
        }
    }
    fn wait_client(&self, _slot: u32) {
        core::hint::spin_loop();
    }
    fn idle_client(&self, _slot: u32, waited: Duration) {
        if waited < IDLE_YIELD {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn notify_slot_free(&self) {
        #[cfg(windows)]
        if let Some(ev) = self.server_ev {
            ev.set();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn roots_the_shim_accepts_pass() {
        for e in [
            env(&[(aether_env::VIRTUAL_DIR, r"C:\game")]),
            env(&[(aether_env::VIRTUAL_DIR, r"C:\")]),
            env(&[
                (aether_env::VIRTUAL_DIR, r"C:\game"),
                (aether_env::VIRTUAL_ROOTS, r"1=C:\saves; 2= ;junk;x=C:\y"),
                (aether_env::ROOT_BACKINGS, r"1=D:\real\saves;3="),
                (aether_env::LAUNCH_IMAGE, r"C:\stage\game.exe"),
            ]),
        ] {
            assert_eq!(shim_roots(&e), Ok(()), "{e:?}");
        }
    }

    #[test]
    fn roots_the_shim_refuses_fail() {
        for e in [
            env(&[]),
            env(&[(aether_env::VIRTUAL_DIR, "")]),
            env(&[(aether_env::VIRTUAL_DIR, r"\")]),
            env(&[(aether_env::VIRTUAL_DIR, ".")]),
            env(&[(aether_env::VIRTUAL_DIR, "..")]),
            // An entry for root 0 replaces VFS_VIRTUAL_DIR.
            env(&[
                (aether_env::VIRTUAL_DIR, r"C:\game"),
                (aether_env::VIRTUAL_ROOTS, r"0=\"),
            ]),
            env(&[
                (aether_env::VIRTUAL_DIR, r"C:\game"),
                (aether_env::VIRTUAL_ROOTS, r"1=/"),
            ]),
            env(&[
                (aether_env::VIRTUAL_DIR, r"C:\game"),
                // A drive-less rooted image's directory is `\`, an empty root.
                (aether_env::LAUNCH_IMAGE, r"\game.exe"),
            ]),
            env(&[
                (aether_env::VIRTUAL_DIR, r"C:\game"),
                (aether_env::LAUNCH_IMAGE, "/game.exe"),
            ]),
            env(&[
                (aether_env::VIRTUAL_DIR, r"C:\game"),
                // The NT prefix is stripped, which leaves nothing.
                (aether_env::ROOT_BACKINGS, r"1=\\?\"),
            ]),
        ] {
            assert!(shim_roots(&e).is_err(), "{e:?}");
        }
    }
}
