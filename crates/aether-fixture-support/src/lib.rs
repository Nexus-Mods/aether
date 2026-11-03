//! What the test fixtures (`aether-fixture-*`) share.
//!
//! A harness reads the shim's hook-stats report (`VFS_SHIM_STATS_LOG`) after a
//! fixture exits, and asserts on it: an open was routed, a hook ran, a path was
//! classified. That report is a periodic snapshot with no exit flush
//! (`aether_shim::hookstats::banner` says why there is none), so a fixture whose
//! last calls come microseconds before exit must stay alive until a report
//! that covers them is on disk. [`outlive_a_covering_report`] is that wait,
//! and a fixture exits [`NO_COVERING_REPORT_EXIT`] when it gives up.
//!
//! The wait needs the shim to be there: a fixture run with
//! `VFS_SHIM_STATS_LOG` set but no shim injected (nothing writes the report)
//! waits [`REPORT_DEADLINE`], 30 s, and then exits
//! [`NO_COVERING_REPORT_EXIT`].
//!
//! The fixtures' own reads of the report are file opens and reads like any
//! other, so they show up in the report they wait for: as `NtCreateFile` and
//! `NtReadFile` calls, and as the report's path in the open-path counts. The
//! path is outside every managed root, so no under-root outcome counts them.

use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long [`outlive_a_covering_report`] waits before it gives up.
pub const REPORT_DEADLINE: Duration = Duration::from_secs(30);

/// The exit code of a fixture whose run no stats report covered: every
/// fixture that waits in [`outlive_a_covering_report`] exits this when it
/// returns `Err`, so a harness reads one code for one cause.
pub const NO_COVERING_REPORT_EXIT: i32 = 21;

/// Stay alive until the shim's stats report on disk was rendered by this
/// process from a snapshot taken after this call: everything the caller did
/// before it is in the report a harness reads once the process has exited.
/// `Ok` at once when `VFS_SHIM_STATS_LOG` is unset (no report to cover); `Err`
/// with the reason when no covering report came within [`REPORT_DEADLINE`],
/// for the caller to print and exit [`NO_COVERING_REPORT_EXIT`] on, so the
/// harness sees why rather than a stale count.
///
/// Waiting a fixed number of reporter ticks is not enough, and is what this
/// replaced: a sleep bounds how long this thread waits, not whether the
/// reporter thread is scheduled, renders and renames in that time. On a
/// loaded 2-vCPU CI runner it often isn't, and the harness read a report from
/// the middle of the run, or none at all.
pub fn outlive_a_covering_report() -> Result<(), String> {
    let Some(log) = aether_env::raw(aether_env::SHIM_STATS_LOG) else {
        return Ok(());
    };
    let interval_ms: u64 = aether_env::parsed_or(aether_env::SHIM_STATS_INTERVAL_MS, 250);
    let poll = Duration::from_millis(interval_ms.clamp(1, 50));
    wait_for_a_report_after_now(Path::new(&log), poll, REPORT_DEADLINE)
}

/// [`wait_for_own_reports`] over the file at `log`, for this process.
pub fn wait_for_a_report_after_now(
    log: &Path,
    poll: Duration,
    deadline: Duration,
) -> Result<(), String> {
    wait_for_own_reports(|| std::fs::read(log), std::process::id(), poll, deadline)
        .map_err(|e| format!("the shim's stats report at {}: {e}", log.display()))
}

/// The pid a report names in its `vfs-shim hook stats (pid N)` line, if any.
pub fn report_pid(report: &[u8]) -> Option<u32> {
    const PREFIX: &str = "vfs-shim hook stats (pid ";
    String::from_utf8_lossy(report)
        .lines()
        .find_map(|line| line.strip_prefix(PREFIX)?.strip_suffix(')')?.parse().ok())
}

/// Wait until two reports from process `pid` have replaced one another since
/// this call, reading the report with `read` every `poll`; `Err` after
/// `deadline`.
///
/// Why twice: the shim's reporter is one thread per process that, tick after
/// tick, snapshots its counters, writes a temp file and renames it over the
/// report. The report read now (or its absence) is the baseline. The first
/// time the file holds a report of `pid`'s other than the last one seen, a
/// rename of `pid`'s landed after now, but its snapshot may predate now. The
/// second time, a later tick's rename landed, and that tick snapshotted after
/// the first rename finished, so after now. Two consecutive reports never read
/// the same (the banner's elapsed time moves by a tick, and these reads are
/// themselves in the report's path counts); were they ever to, a replacement
/// would go unseen and this would wait a tick longer, never less.
///
/// Only `pid`'s reports count: the shim injects itself into children, which
/// inherit `VFS_SHIM_STATS_LOG` and write the same file, and a child's report
/// says nothing about this process's snapshot. A report that names no pid is
/// not counted either.
///
/// Only a missing file means there is no report. Any other failed read (a
/// sharing violation or access denied while a rename replaces the file) says
/// nothing about what is there: the baseline read is retried until it
/// succeeds, and a failed read in the loop is skipped. Taking a failed
/// baseline for "no report" would count the unchanged report as the first
/// replacement.
pub fn wait_for_own_reports(
    mut read: impl FnMut() -> std::io::Result<Vec<u8>>,
    pid: u32,
    poll: Duration,
    deadline: Duration,
) -> Result<(), String> {
    let until = Instant::now() + deadline;
    let gave_up = |replaced: u32, last_error: &Option<std::io::Error>| {
        let error = last_error
            .as_ref()
            .map(|e| format!(" (last failed read: {e})"))
            .unwrap_or_default();
        format!(
            "process {pid}'s report was replaced {replaced} of 2 times in {deadline:?}; \
             no report covers this run{error}"
        )
    };
    let mut last_error = None;
    let mut seen = loop {
        match read() {
            Ok(report) => break (report_pid(&report) == Some(pid)).then_some(report),
            Err(e) if e.kind() == ErrorKind::NotFound => break None,
            Err(e) => last_error = Some(e),
        }
        if Instant::now() >= until {
            return Err(gave_up(0, &last_error));
        }
        std::thread::sleep(poll);
    };
    let mut replaced = 0;
    while replaced < 2 {
        if Instant::now() >= until {
            return Err(gave_up(replaced, &last_error));
        }
        std::thread::sleep(poll);
        match read() {
            Ok(report) if report_pid(&report) == Some(pid) && seen.as_ref() != Some(&report) => {
                replaced += 1;
                seen = Some(report);
            }
            Ok(_) => {}
            Err(e) => last_error = Some(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::Error;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    const SHORT: Duration = Duration::from_millis(50);
    const POLL: Duration = Duration::from_millis(1);

    fn report(pid: u32, tick: u32) -> Vec<u8> {
        format!("SNAPSHOT at t+0.{tick:03}s\nvfs-shim hook stats (pid {pid})\n  TOTAL\n")
            .into_bytes()
    }

    /// A reader that answers `script` in order, then repeats its last answer.
    fn scripted(script: Vec<std::io::Result<Vec<u8>>>) -> impl FnMut() -> std::io::Result<Vec<u8>> {
        let mut script: VecDeque<_> = script.into();
        let mut last: Option<Vec<u8>> = None;
        move || match script.pop_front() {
            Some(Ok(r)) => {
                last = Some(r.clone());
                Ok(r)
            }
            Some(Err(e)) => Err(e),
            None => last.clone().ok_or_else(|| Error::from(ErrorKind::NotFound)),
        }
    }

    fn denied() -> std::io::Result<Vec<u8>> {
        Err(Error::from(ErrorKind::PermissionDenied))
    }

    #[test]
    fn the_pid_comes_from_the_hook_stats_line() {
        assert_eq!(report_pid(&report(9192, 1)), Some(9192));
        assert_eq!(report_pid(b"SNAPSHOT at t+0.001s\n"), None);
    }

    #[test]
    fn two_own_replacements_after_the_baseline_cover_the_run() {
        let me = 7;
        let read = scripted(vec![
            Ok(report(me, 1)),
            Ok(report(me, 2)),
            Ok(report(me, 3)),
        ]);
        wait_for_own_reports(read, me, POLL, SHORT).unwrap();
    }

    #[test]
    fn a_failed_baseline_read_is_retried_not_taken_for_no_report() {
        // the report at the baseline is tick 1; a denied read must not make
        // tick 1 look new, so tick 2 is the only replacement
        let me = 7;
        let read = scripted(vec![denied(), Ok(report(me, 1)), Ok(report(me, 2))]);
        let e = wait_for_own_reports(read, me, POLL, SHORT).unwrap_err();
        assert!(e.contains("replaced 1 of 2"), "{e}");
    }

    #[test]
    fn a_baseline_that_never_reads_is_an_error_naming_the_failure() {
        let read = || -> std::io::Result<Vec<u8>> { denied() };
        let e = wait_for_own_reports(read, 7, POLL, SHORT).unwrap_err();
        assert!(
            e.contains("replaced 0 of 2") && e.contains("last failed read"),
            "{e}"
        );
    }

    #[test]
    fn failed_reads_in_the_loop_are_skipped() {
        let me = 7;
        let read = scripted(vec![
            Ok(report(me, 1)),
            denied(),
            Ok(report(me, 2)),
            denied(),
            Ok(report(me, 3)),
        ]);
        wait_for_own_reports(read, me, POLL, SHORT).unwrap();
    }

    #[test]
    fn another_process_report_is_not_a_replacement() {
        // a child's reports, however many, don't count; nor does this
        // process's report coming back after a child's
        let (me, child) = (7, 8);
        let read = scripted(vec![
            Ok(report(me, 1)),
            Ok(report(child, 1)),
            Ok(report(me, 1)),
            Ok(report(child, 2)),
            Ok(report(child, 3)),
        ]);
        let e = wait_for_own_reports(read, me, POLL, SHORT).unwrap_err();
        assert!(e.contains("replaced 0 of 2"), "{e}");
    }

    #[test]
    fn own_reports_between_a_childs_count() {
        let (me, child) = (7, 8);
        let read = scripted(vec![
            Ok(report(child, 1)),
            Ok(report(me, 1)),
            Ok(report(child, 2)),
            Ok(report(me, 2)),
        ]);
        wait_for_own_reports(read, me, POLL, SHORT).unwrap();
    }

    #[test]
    fn a_missing_report_is_the_baseline_and_its_first_appearance_counts() {
        let me = 7;
        let read = scripted(vec![
            Err(Error::from(ErrorKind::NotFound)),
            Ok(report(me, 1)),
            Ok(report(me, 2)),
        ]);
        wait_for_own_reports(read, me, POLL, SHORT).unwrap();
    }

    /// A stand-in for the shim's reporter in this process: every `tick`,
    /// snapshot `counter`, take `render` (the shim formats its report between
    /// the two), then write it to `log` by a temp file and a rename, until
    /// `stop`. A render longer than the tick makes most renames carry a
    /// snapshot older than the rename itself, which a wait for one
    /// replacement would read as current.
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
                let body = format!(
                    "tick {n}\nvfs-shim hook stats (pid {})\ncounter {snap}\n",
                    std::process::id()
                );
                // as the shim's write_report: a failed write or rename is
                // retried by the next tick
                if std::fs::write(&tmp, body).is_ok() {
                    let _ = std::fs::rename(&tmp, &log);
                }
            }
        })
    }

    #[test]
    fn the_report_after_the_wait_holds_what_came_before_it() {
        let dir = aether_testkit::tempdir().unwrap();
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
            wait_for_a_report_after_now(&log, POLL, REPORT_DEADLINE).unwrap();
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
        let dir = aether_testkit::tempdir().unwrap();
        let log = dir.path().join("shim-stats.log");
        std::fs::write(&log, report(std::process::id(), 1)).unwrap();
        let e = wait_for_a_report_after_now(&log, POLL, Duration::from_millis(30)).unwrap_err();
        assert!(
            e.contains("shim-stats.log") && e.contains("replaced 0 of 2"),
            "{e}"
        );
    }
}
