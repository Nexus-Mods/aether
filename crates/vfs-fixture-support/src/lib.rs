//! What the test fixtures (`vfs-fixture-*`) share.
//!
//! A harness reads the shim's hook-stats report (`VFS_SHIM_STATS_LOG`) after a
//! fixture exits, and asserts on it: an open was routed, a hook ran, a path was
//! classified. That report is a periodic snapshot with no exit flush
//! (`vfs_shim::hookstats::banner` says why there is none), so a fixture whose
//! last calls come microseconds before exit must stay alive until a report
//! that covers them is on disk. [`outlive_a_covering_report`] is that wait.

use std::path::Path;
use std::time::{Duration, Instant};

/// How long [`outlive_a_covering_report`] waits before it gives up.
pub const REPORT_DEADLINE: Duration = Duration::from_secs(30);

/// Stay alive until the shim's stats report on disk was rendered from a
/// snapshot taken after this call: everything the caller did before it is in
/// the report a harness reads once the process has exited. `Ok` at once when
/// `VFS_SHIM_STATS_LOG` is unset (no report to cover); `Err` with the reason
/// when no covering report came within [`REPORT_DEADLINE`], for the caller to
/// print and exit non-zero on, so the harness sees why rather than a stale
/// count.
///
/// Waiting a fixed number of reporter ticks is not enough, and is what this
/// replaced: a sleep bounds how long this thread waits, not whether the
/// reporter thread is scheduled, renders and renames in that time. On a
/// loaded 2-vCPU CI runner it often isn't, and the harness read a report from
/// the middle of the run, or none at all.
pub fn outlive_a_covering_report() -> Result<(), String> {
    let Some(log) = vfs_env::raw(vfs_env::SHIM_STATS_LOG) else {
        return Ok(());
    };
    let interval_ms: u64 = vfs_env::parsed_or(vfs_env::SHIM_STATS_INTERVAL_MS, 250);
    let poll = Duration::from_millis(interval_ms.clamp(1, 50));
    wait_for_a_report_after_now(Path::new(&log), poll, REPORT_DEADLINE)
}

/// Wait until the report at `log` has been replaced twice since this call,
/// reading it every `poll`; `Err` after `deadline`.
///
/// Why twice: the shim's reporter is one thread that, tick after tick,
/// snapshots its counters, writes a temp file and renames it over the report.
/// The report read now (or its absence) is the baseline. The first time the
/// file reads different, a rename landed after now, but its snapshot may
/// predate now. The second time, a later tick's rename landed, and that tick
/// snapshotted after the first rename finished, so after now. Two consecutive
/// reports never read the same (the banner's elapsed time moves by a tick, and
/// these reads are themselves in the report's path counts); were they ever to,
/// a replacement would go unseen and this would wait a tick longer, never
/// less.
pub fn wait_for_a_report_after_now(
    log: &Path,
    poll: Duration,
    deadline: Duration,
) -> Result<(), String> {
    let until = Instant::now() + deadline;
    let mut seen = std::fs::read(log).ok();
    let mut replaced = 0;
    while replaced < 2 {
        if Instant::now() >= until {
            return Err(format!(
                "the shim's stats report at {} was replaced {replaced} of 2 times in {deadline:?}; \
                 no report covers this run",
                log.display()
            ));
        }
        std::thread::sleep(poll);
        let now = std::fs::read(log).ok();
        if now.is_some() && now != seen {
            replaced += 1;
            seen = now;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    /// A stand-in for the shim's reporter: every `tick`, snapshot `counter`,
    /// take `render` (the shim formats its report between the two), then write
    /// it to `log` by a temp file and a rename, until `stop`. A render longer
    /// than the tick makes most renames carry a snapshot older than the
    /// rename itself, which a wait for one replacement would read as current.
    fn reporter(
        log: std::path::PathBuf,
        counter: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        tick: Duration,
        render: Duration,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(tick);
                let snap = counter.load(Ordering::SeqCst);
                std::thread::sleep(render);
                n += 1;
                let tmp = log.with_extension("tmp");
                // as the shim's write_report: a failed write or rename is
                // retried by the next tick
                if std::fs::write(
                    &tmp,
                    format!(
                        "tick {n} counter {snap}
"
                    ),
                )
                .is_ok()
                {
                    let _ = std::fs::rename(&tmp, &log);
                }
            }
        })
    }

    #[test]
    fn the_report_after_the_wait_holds_what_came_before_it() {
        let dir = vfs_testkit::tempdir().unwrap();
        let log = dir.path().join("shim-stats.log");
        let counter = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let t = reporter(
            log.clone(),
            counter.clone(),
            stop.clone(),
            Duration::from_millis(1),
            Duration::from_millis(4),
        );
        for round in 1..=20u64 {
            counter.store(round, Ordering::SeqCst);
            wait_for_a_report_after_now(&log, Duration::from_millis(1), REPORT_DEADLINE).unwrap();
            let text = std::fs::read_to_string(&log).unwrap();
            assert!(
                text.ends_with(&format!("counter {round}\n")),
                "round {round}: {text}"
            );
        }
        stop.store(true, Ordering::SeqCst);
        t.join().unwrap();
    }

    #[test]
    fn a_report_that_never_changes_is_an_error_naming_it() {
        let dir = vfs_testkit::tempdir().unwrap();
        let log = dir.path().join("shim-stats.log");
        std::fs::write(&log, "a report nobody rewrites\n").unwrap();
        let e =
            wait_for_a_report_after_now(&log, Duration::from_millis(1), Duration::from_millis(30))
                .unwrap_err();
        assert!(
            e.contains("shim-stats.log") && e.contains("replaced 0 of 2"),
            "{e}"
        );
    }

    #[test]
    fn no_report_yet_counts_its_first_appearance_as_one_replacement() {
        let dir = vfs_testkit::tempdir().unwrap();
        let log = dir.path().join("shim-stats.log");
        let counter = Arc::new(AtomicU64::new(7));
        let stop = Arc::new(AtomicBool::new(false));
        let t = reporter(
            log.clone(),
            counter,
            stop.clone(),
            Duration::from_millis(2),
            Duration::ZERO,
        );
        wait_for_a_report_after_now(&log, Duration::from_millis(1), REPORT_DEADLINE).unwrap();
        stop.store(true, Ordering::SeqCst);
        t.join().unwrap();
        assert!(std::fs::read_to_string(&log).unwrap().contains("counter 7"));
    }
}
