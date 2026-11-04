//! What the shim makes of the `VFS_*` values a director sets for it, as
//! functions of the text alone.
//!
//! The environment is the director's: it sets these variables for the program
//! the shim runs in, so each value is input from outside the shim and its
//! parsing must take any string. The functions here are that parsing, with no
//! access to the process environment, so the shim (`aether-hooks`), the
//! conformance harness's attach check and the fuzz target read one
//! implementation. A value that does not parse is never an error: it is
//! skipped or replaced by its default, as each function says.

/// The ring's size when `VFS_RING_BYTES` says none.
pub const DEFAULT_RING_BYTES: usize = 2 * 1024 * 1024;
/// The ring's payload capacity when `VFS_RING_PAYLOAD_CAP` says none.
pub const DEFAULT_PAYLOAD_CAP: u32 = 1_048_576;

/// Where the ring's shared memory comes from: a named page-file-backed
/// section (`VFS_RING_SECTION`, the Windows deployment) or a real file
/// (`VFS_RING_PATH`, mapped by path: the only kind of shared memory a shim
/// inside Wine and a native Linux director can both name).
#[derive(Debug, PartialEq, Eq)]
pub enum RingSource {
    Section(String),
    File(String),
}

/// Pick the ring source. **The path wins when both are set**: a session
/// configured for the Wine boundary must never fall back to a named section,
/// which may still be set (the launcher writes its variables together) and
/// may even exist, from an unrelated director on the same machine. An empty
/// path is treated as absent, as an empty section would be. Neither set is
/// `None`.
pub fn ring_source(ring_path: Option<String>, section: Option<String>) -> Option<RingSource> {
    match ring_path.filter(|p| !p.is_empty()) {
        Some(path) => Some(RingSource::File(path)),
        None => section.map(RingSource::Section),
    }
}

/// Whether a switch that is **off unless turned on** is on, given its value:
/// `1`, `true`, `yes` or `on` in any case; anything else, unset included, is
/// off.
pub fn opt_in(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Whether a switch that is **on unless turned off** is on, given its value:
/// off only for `0`, `false`, `no` or `off` in any case.
pub fn opt_out(value: Option<&str>) -> bool {
    value.is_none_or(|v| {
        !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// The numbers that describe the ring and its arena.
#[derive(Debug, PartialEq, Eq)]
pub struct Ring {
    pub bytes: usize,
    pub payload_cap: u32,
    /// 0: no arena.
    pub arena_len: usize,
    /// Where the arena starts, when the environment says; otherwise right
    /// after the last slot.
    pub arena_offset: Option<usize>,
}

/// The ring numbers from the values of `VFS_RING_BYTES`,
/// `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_LEN` and `VFS_ARENA_OFFSET`, each
/// defaulted when absent or not a number.
pub fn ring(
    bytes: Option<&str>,
    payload_cap: Option<&str>,
    arena_len: Option<&str>,
    arena_offset: Option<&str>,
) -> Ring {
    Ring {
        bytes: bytes
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RING_BYTES),
        payload_cap: payload_cap
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_PAYLOAD_CAP),
        arena_len: arena_len.and_then(|s| s.parse().ok()).unwrap_or(0),
        arena_offset: arena_offset.and_then(|s| s.parse().ok()),
    }
}

/// The roots a session declares: root 0 from `VFS_VIRTUAL_DIR`, then the
/// `id=path;id=path` entries of `VFS_VIRTUAL_ROOTS` on top of it.
///
/// Root 0 is seeded first so a session that declares no extra roots is the
/// single-root case. An entry whose id will not parse, or with no path, is
/// skipped; an entry naming id 0 (or any id already present) replaces that
/// root's path, because a caller that spelled it explicitly meant it.
pub fn roots(virtual_dir: &str, spec: Option<&str>) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = vec![(0, virtual_dir.to_string())];
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
        match out.iter_mut().find(|(r, _)| *r == id) {
            Some(slot) => slot.1 = path.to_string(),
            None => out.push((id, path.to_string())),
        }
    }
    out
}

/// Each root's backing directory where it differs from the root's location
/// (`VFS_ROOT_BACKINGS`): `id=path;id=path`, malformed entries skipped, each
/// path normalised as [`normalize_path_for_root`] does.
pub fn backings(spec: Option<&str>) -> Vec<(u32, String)> {
    spec.unwrap_or_default()
        .split(';')
        .filter_map(|entry| {
            let (id, path) = entry.trim().split_once('=')?;
            let id = id.trim().parse::<u32>().ok()?;
            let path = path.trim();
            (!path.is_empty()).then(|| (id, normalize_path_for_root(path)))
        })
        .collect()
}

/// `p` without a leading NT device prefix (`\??\` or `\\?\`).
pub fn strip_nt_device(p: &str) -> &str {
    p.strip_prefix(r"\??\")
        .or_else(|| p.strip_prefix(r"\\?\"))
        .unwrap_or(p)
}

/// A root's path as the shim compares it: no device prefix, backslashes,
/// ASCII lower case.
pub fn normalize_path_for_root(p: &str) -> String {
    strip_nt_device(p).replace('/', "\\").to_ascii_lowercase()
}

/// The ready wait in seconds for a `VFS_READY_TIMEOUT_SECS` value: that
/// number, else [`crate::DEFAULT_READY_TIMEOUT_SECS`]; never below 1.
pub fn ready_timeout_secs(spec: Option<&str>) -> u64 {
    spec.and_then(|s| s.parse().ok())
        .unwrap_or(crate::DEFAULT_READY_TIMEOUT_SECS)
        .max(1)
}

/// How often the stats reporter writes, in milliseconds, for a
/// `VFS_SHIM_STATS_INTERVAL_MS` value: that number, else 250.
pub fn stats_interval_ms(spec: Option<&str>) -> u64 {
    spec.and_then(|s| s.parse().ok()).unwrap_or(250)
}

/// The read cache's budget in MiB for a `VFS_SHIM_READ_CACHE_MIB` value:
/// `None` for one that is absent, not a number, or zero.
pub fn read_cache_mib(spec: Option<&str>) -> Option<usize> {
    spec.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&m| m > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_defaults_when_the_environment_says_nothing_usable() {
        let r = ring(None, Some("x"), Some("-1"), Some(""));
        assert_eq!(r.bytes, DEFAULT_RING_BYTES);
        assert_eq!(r.payload_cap, DEFAULT_PAYLOAD_CAP);
        assert_eq!((r.arena_len, r.arena_offset), (0, None));
        let r = ring(Some("4096"), Some("64"), Some("8192"), Some("1024"));
        assert_eq!((r.bytes, r.payload_cap), (4096, 64));
        assert_eq!((r.arena_len, r.arena_offset), (8192, Some(1024)));
    }

    #[test]
    fn a_path_wins_over_a_section_unless_it_is_empty() {
        let s = || Some("sec".to_string());
        assert_eq!(
            ring_source(Some("f".into()), s()),
            Some(RingSource::File("f".into()))
        );
        assert_eq!(
            ring_source(Some(String::new()), s()),
            Some(RingSource::Section("sec".into()))
        );
        assert_eq!(ring_source(None, None), None);
    }

    #[test]
    fn switches_read_their_values_as_documented() {
        assert!(opt_in(Some("On")) && !opt_in(Some("off")) && !opt_in(None));
        assert!(!opt_in(Some("2")) && !opt_in(Some("")));
        assert!(opt_out(None) && opt_out(Some("x")) && !opt_out(Some("NO")));
    }

    #[test]
    fn extra_roots_follow_root_zero_and_bad_entries_are_skipped() {
        assert_eq!(
            roots(r"C:\g", Some(r"1=C:\a; x=b;2=;;3 = C:\c ;0=C:\z")),
            vec![
                (0, r"C:\z".to_string()),
                (1, r"C:\a".into()),
                (3, r"C:\c".into())
            ]
        );
        assert_eq!(roots("d", None), vec![(0, "d".to_string())]);
    }

    #[test]
    fn backings_are_normalised_and_bad_entries_skipped() {
        assert_eq!(
            backings(Some(r"1=\??\C:/Back;bad;2=")),
            vec![(1, r"c:\back".to_string())]
        );
        assert!(backings(None).is_empty());
    }

    #[test]
    fn timeouts_and_budgets_default_on_bad_values() {
        assert_eq!(ready_timeout_secs(None), 180);
        assert_eq!(ready_timeout_secs(Some("0")), 1);
        assert_eq!(ready_timeout_secs(Some("7")), 7);
        assert_eq!(stats_interval_ms(Some("x")), 250);
        assert_eq!(read_cache_mib(Some(" 16 ")), Some(16));
        assert_eq!(read_cache_mib(Some("0")), None);
    }
}
