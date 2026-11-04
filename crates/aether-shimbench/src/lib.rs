//! `aether-shimbench`: what one Win32 call costs under the shim.
//!
//! A case is one kind of call (`aether-shimbench-target <case> ...` makes it
//! many times and times each). The launcher ([`launch`]) runs the target
//! twice per case: natively, against [`write_tree`]'s files on disk, and
//! under the shim, started by `aether-injector` with `aether_shim.dll` in it
//! and its ring served in this process by the reference director (or the
//! null one) behind a headers-only [`aether_refdirector::Recorder`]. The
//! report ([`report`]) holds both latencies, their difference, and the
//! requests per call the recorder saw during the timed loop.
//!
//! Timed numbers are this machine's: nothing compares them with a fixed
//! value, and CI only runs a smoke pass that checks the counts.
//!
//! The tree, the same on disk and in the reference:
//!
//! ```text
//! data/a.esp              1 MiB, aether_refdirector::pattern, read-only (immutable)
//! data/list/f000..f099.esp  64 bytes each
//! saves/s.ess             1 MiB, writable prefix saves/ (mutable)
//! ```
//!
//! and outside every root, a real file `outside/real.txt`.

#[cfg(windows)]
pub mod launch;
#[cfg(windows)]
pub mod report;

use serde_json::{json, Value};

/// One kind of call the target makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// `CreateFileW` (read, `OPEN_EXISTING`) and `CloseHandle` of `data/a.esp`.
    OpenCloseInRoot,
    /// `GetFileAttributesExW` of `data/a.esp`.
    GetattrInRoot,
    /// `GetFileAttributesExW` of a real file outside every root (passthrough).
    GetattrOutside,
    /// `ReadFile` of 4 KiB at offset 0 of `data/a.esp`, immutable: after the
    /// first block, read-cache hits.
    Read4kCached,
    /// `ReadFile` of 4 KiB at offset 0 of `saves/s.ess`, mutable: never cached.
    Read4kUncached,
    /// `ReadFile` of 1 MiB at offset 0 of `data/a.esp`: bulk READs.
    Read1mBulk,
    /// `FindFirstFileW`, `FindNextFileW` to the end and `FindClose` over the
    /// 100 entries of `data/list`.
    Findfirst100,
    /// `GetFinalPathNameByHandleW` on a handle of `data/a.esp`.
    FinalPath,
}

impl Case {
    /// Every case, in the order a run takes them.
    pub const ALL: [Case; 8] = [
        Case::OpenCloseInRoot,
        Case::GetattrInRoot,
        Case::GetattrOutside,
        Case::Read4kCached,
        Case::Read4kUncached,
        Case::Read1mBulk,
        Case::Findfirst100,
        Case::FinalPath,
    ];

    /// The case's name on the command line and in the report.
    pub fn name(self) -> &'static str {
        match self {
            Case::OpenCloseInRoot => "open-close-in-root",
            Case::GetattrInRoot => "getattr-in-root",
            Case::GetattrOutside => "getattr-outside",
            Case::Read4kCached => "read-4k-cached",
            Case::Read4kUncached => "read-4k-uncached",
            Case::Read1mBulk => "read-1m-bulk",
            Case::Findfirst100 => "findfirst-100",
            Case::FinalPath => "final-path",
        }
    }

    pub fn parse(name: &str) -> Option<Case> {
        Case::ALL.into_iter().find(|c| c.name() == name)
    }

    /// What the case works on, relative to the tree's top (or, for
    /// [`Case::GetattrOutside`], to the folder outside every root).
    pub fn rel_path(self) -> &'static str {
        match self {
            Case::OpenCloseInRoot
            | Case::GetattrInRoot
            | Case::Read4kCached
            | Case::Read1mBulk
            | Case::FinalPath => "data/a.esp",
            Case::GetattrOutside => "real.txt",
            Case::Read4kUncached => "saves/s.ess",
            Case::Findfirst100 => "data/list",
        }
    }

    /// Whether [`Self::rel_path`] is outside every root.
    pub fn outside(self) -> bool {
        self == Case::GetattrOutside
    }
}

/// One mebibyte.
pub const MIB: usize = 1024 * 1024;
/// The entries of `data/list`.
pub const LISTED: usize = 100;
/// The bytes of `outside/real.txt`.
pub const OUTSIDE_BYTES: &[u8] = b"on disk";

/// Every file of the tree: its path under the top and its bytes.
pub fn tree_files() -> Vec<(String, Vec<u8>)> {
    use aether_refdirector::pattern;
    let mut files = vec![
        ("data/a.esp".to_string(), pattern(MIB)),
        ("saves/s.ess".to_string(), pattern(MIB)),
    ];
    for i in 0..LISTED {
        files.push((format!("data/list/f{i:03}.esp"), pattern(64)));
    }
    files
}

/// Write the tree under `top`, as files on disk (the native run's).
pub fn write_tree(top: &std::path::Path) -> std::io::Result<()> {
    for (rel, bytes) in tree_files() {
        let p = top.join(rel);
        std::fs::create_dir_all(p.parent().expect("a file under the top"))?;
        std::fs::write(p, bytes)?;
    }
    Ok(())
}

/// Per-call nanoseconds of a timed loop, summarised from the sorted sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub count: u64,
    pub min: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
    pub mean: u64,
}

impl Summary {
    /// The summary of `ns` (sorted here). Quantile `q` is the sample at
    /// index `ceil(q * n) - 1`, the nearest-rank rule.
    pub fn of(ns: &mut [u64]) -> Summary {
        if ns.is_empty() {
            return Summary::default();
        }
        ns.sort_unstable();
        let n = ns.len();
        let at = |q: f64| ns[((q * n as f64).ceil() as usize).clamp(1, n) - 1];
        Summary {
            count: n as u64,
            min: ns[0],
            p50: at(0.50),
            p90: at(0.90),
            p99: at(0.99),
            p999: at(0.999),
            max: ns[n - 1],
            mean: (ns.iter().map(|&v| u128::from(v)).sum::<u128>() / n as u128) as u64,
        }
    }

    pub fn to_json(self) -> Value {
        json!({
            "count": self.count, "min": self.min, "p50": self.p50, "p90": self.p90,
            "p99": self.p99, "p999": self.p999, "max": self.max, "mean": self.mean,
        })
    }

    pub fn from_json(v: &Value) -> Option<Summary> {
        let f = |k: &str| v.get(k)?.as_u64();
        Some(Summary {
            count: f("count")?,
            min: f("min")?,
            p50: f("p50")?,
            p90: f("p90")?,
            p99: f("p99")?,
            p999: f("p999")?,
            max: f("max")?,
            mean: f("mean")?,
        })
    }
}

/// The untimed calls before the timed loop: a tenth of the timed count,
/// at least 10 and at most 10 000. The first fills one-time state (a
/// read-cache block, the spelling cache, the OS's file cache).
pub fn warmup(iterations: u64) -> u64 {
    (iterations / 10).clamp(10, 10_000)
}

/// How many timer ticks a timed batch should last at least: at 100 ticks
/// the timer's step is at most 1% of a batch.
pub const BATCH_TICKS: u64 = 100;
/// The most calls in one batch.
pub const MAX_BATCH: u64 = 1000;

/// How many calls the target times together, so that a batch of calls
/// averaging `mean_ns` lasts [`BATCH_TICKS`] steps of a timer that steps
/// by `resolution_ns`: 1 for calls that long already, at most [`MAX_BATCH`].
/// A sample is then the batch's time divided by its calls.
pub fn batch_size(mean_ns: u64, resolution_ns: u64) -> u64 {
    (BATCH_TICKS * resolution_ns.max(1))
        .div_ceil(mean_ns.max(1))
        .clamp(1, MAX_BATCH)
}

/// How the target timed a case: `batches` samples of `batch` calls each,
/// with a timer that steps by `timer_ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub batch: u64,
    pub batches: u64,
    pub timer_ns: u64,
}

impl Timing {
    /// The timed calls: `batch * batches`, at least the iterations asked for.
    pub fn calls(&self) -> u64 {
        self.batch * self.batches
    }
}

/// The line the target writes: the case, its counts, how it timed them and
/// the summary of per-call nanoseconds (one sample per batch).
pub fn target_line(case: Case, iterations: u64, t: Timing, lenient: bool, ns: Summary) -> Value {
    json!({
        "case": case.name(),
        "iterations": iterations,
        "warmup": warmup(iterations),
        "batch": t.batch,
        "calls": t.calls(),
        "timer_ns": t.timer_ns,
        "lenient": lenient,
        "ns": ns.to_json(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_case_parses_back_from_its_name() {
        for c in Case::ALL {
            assert_eq!(Case::parse(c.name()), Some(c));
        }
        assert_eq!(Case::parse("nope"), None);
    }

    #[test]
    fn the_summary_takes_nearest_ranks() {
        let mut ns: Vec<u64> = (1..=1000).rev().collect();
        let s = Summary::of(&mut ns);
        assert_eq!((s.count, s.min, s.max), (1000, 1, 1000));
        assert_eq!((s.p50, s.p90, s.p99, s.p999), (500, 900, 990, 999));
        assert_eq!(s.mean, 500);
        assert_eq!(Summary::from_json(&s.to_json()), Some(s));
        assert_eq!(Summary::of(&mut []), Summary::default());
    }

    #[test]
    fn a_batch_lasts_a_hundred_timer_steps() {
        // 100 ns calls on a 100 ns timer: 100 calls make 100 steps.
        assert_eq!(batch_size(100, 100), 100);
        assert_eq!(batch_size(15_000, 100), 1);
        assert_eq!(batch_size(0, 100), MAX_BATCH);
        let t = Timing {
            batch: 100,
            batches: 3,
            timer_ns: 100,
        };
        assert_eq!(t.calls(), 300);
    }

    #[test]
    fn warmup_is_bounded() {
        assert_eq!(warmup(10), 10);
        assert_eq!(warmup(1000), 100);
        assert_eq!(warmup(1_000_000), 10_000);
    }
}
