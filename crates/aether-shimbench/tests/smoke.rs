//! The bench end to end, quickly: every case runs natively and under the
//! shim (the injector, the shim DLL, a ring this test's process serves), the
//! report has its shape, and the requests per call the recorder counted in
//! the injected process are the ones the shim's tests record for the same
//! calls in-process (`crates/aether-hooks/tests/harness/costs.txt`). Timed
//! values are never asserted.
//!
//! Needs `aether-injector.exe` and `aether_shim.dll` in this profile
//! (`cargo build -p aether-inject -p aether-shim`): without them each test
//! prints SKIP, or fails under `VFS_TEST_REQUIRE_ALL=1`.
#![cfg(windows)]

use aether_protocol::{Opcode, Status};
use aether_refdirector::harness::{Fault, Match};
use aether_shimbench::launch::{Artifacts, Bench, DirectorKind, GeometryKind, ShimOptions};
use aether_shimbench::Case;
use serde_json::Value;
use std::process::Command;
use std::time::Duration;

/// The shim's counted costs, recorded in-process at the same geometry.
const COSTS: &str = include_str!("../../aether-hooks/tests/harness/costs.txt");

/// The rows of [`COSTS`] that make up one call of each case: the same Win32
/// calls the shim's cost cases make.
fn rows(case: Case) -> &'static [&'static str] {
    match case {
        Case::OpenCloseInRoot => &["open_read_root", "close"],
        Case::GetattrInRoot => &["attributes_in_root"],
        Case::GetattrOutside => &["attributes_outside_root"],
        Case::Read4kCached => &["read_small_cached"],
        // No row reads 4 KiB from a mutable file; the test derives it from
        // read_small_mutable (1 KiB).
        Case::Read4kUncached => &[],
        Case::Read1mBulk => &["read_bulk_1_mib"],
        Case::Findfirst100 => &["list_100_entries"],
        Case::FinalPath => &["final_path_name_warm"],
    }
}

/// `(round_trips, ring_bytes)` of a row of [`COSTS`].
fn recorded(row: &str) -> (u64, u64) {
    let line = COSTS
        .lines()
        .find(|l| l.split_whitespace().next() == Some(row))
        .unwrap_or_else(|| panic!("costs.txt has no row {row}"));
    let field = |name: &str| -> u64 {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix(name))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{row} has no {name}"))
    };
    (field("round_trips="), field("ring_bytes="))
}

/// The injector and the shim, or `None` (SKIP) when they are not built.
fn artifacts() -> Option<Artifacts> {
    match Artifacts::locate() {
        Ok(a) => Some(a),
        Err(e) if aether_env::opt_in("VFS_TEST_REQUIRE_ALL") => panic!("{e}"),
        Err(e) => {
            eprintln!("SKIP: {e}");
            None
        }
    }
}

#[test]
fn every_case_runs_natively_and_under_the_shim() {
    if artifacts().is_none() {
        return;
    }
    let dir = aether_testkit::tempdir().expect("a folder for the report");
    let out = dir.path().join("report.json");
    let st = Command::new(env!("CARGO_BIN_EXE_aether-shimbench"))
        .args([
            "--geometry",
            "shim-tests",
            "--iterations",
            "10",
            "--timeout",
            "120",
        ])
        .arg("--out")
        .arg(&out)
        .status()
        .expect("run aether-shimbench");
    assert_eq!(st.code(), Some(0), "aether-shimbench failed");
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&out).expect("the report")).expect("JSON");

    for k in ["cpu", "cores", "os"] {
        assert!(!report["machine"][k].is_null(), "machine.{k}");
    }
    for k in ["profile", "rustc", "git"] {
        assert!(
            report["machine"]["build"][k].is_string(),
            "machine.build.{k}"
        );
    }
    assert_eq!(report["director"], "reference");
    assert_eq!(report["geometry"]["name"], "shim-tests");
    assert!(
        report["timer_ns"].as_u64().is_some_and(|t| t > 0),
        "timer_ns"
    );
    assert_eq!(report["iterations"], 10);
    let cases = report["cases"].as_array().expect("cases");
    let names: Vec<&str> = cases.iter().map(|c| c["name"].as_str().unwrap()).collect();
    let all: Vec<&str> = Case::ALL.iter().map(|c| c.name()).collect();
    assert_eq!(names, all, "one entry per case, in order");

    for (c, case) in cases.iter().zip(Case::ALL) {
        let name = case.name();
        assert!(c.get("error").is_none(), "{name}: {}", c["error"]);
        for side in ["native", "shim"] {
            // Batches of `batch` calls, at least the 10 asked for.
            let (count, batch, calls) = (
                c[side]["count"].as_u64().unwrap(),
                c[side]["batch"].as_u64().unwrap(),
                c[side]["calls"].as_u64().unwrap(),
            );
            assert!(
                batch >= 1 && count * batch == calls && calls >= 10,
                "{name}: {side}"
            );
            for q in ["min", "p50", "p90", "p99", "p999", "max", "mean"] {
                assert!(c[side][q].is_u64(), "{name}: {side}.{q}");
            }
        }
        for q in ["p50", "p90", "p99"] {
            assert!(c["overhead_ns"][q].is_i64(), "{name}: overhead_ns.{q}");
        }
        let trips = c["round_trips_per_call"].as_f64().expect("round trips");
        let bytes = c["ring_bytes_per_call"].as_f64().expect("ring bytes");
        let want = match rows(case) {
            [] => {
                // 4096 bytes from a mutable file are two inline READs at a
                // 4 KiB payload cap, whose reply holds 4088 bytes of data
                // (docs/wire.md 3.4.4): twice the round trip of the 1 KiB
                // row, with its bytes past the data each time.
                assert_eq!(case, Case::Read4kUncached);
                let (rt, rb) = recorded("read_small_mutable");
                (2 * rt, 4096 + 2 * (rb - 1024))
            }
            rows => rows
                .iter()
                .map(|r| recorded(r))
                .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
        };
        assert_eq!(
            (trips, bytes),
            (want.0 as f64, want.1 as f64),
            "{name}: (round trips, ring bytes) per call against costs.txt's {:?}",
            rows(case)
        );
    }
}

/// The cross-process half of a failed attach: a director that answers the
/// HEARTBEAT with an error fails the launch. The shim writes
/// `fuse-failed:` to the ready file (`docs/wire.md` 6.2) and the injector
/// kills the target, writes the same verdict to `<ready>.injector-error` and
/// exits 3 (6.3).
#[test]
fn a_director_that_refuses_heartbeat_fails_the_launch_with_fuse_failed() {
    let Some(artifacts) = artifacts() else {
        return;
    };
    let bench = Bench::new(artifacts, GeometryKind::ShimTests, Duration::from_secs(120))
        .expect("the bench's folders");
    let refuse = Match {
        op: Some(Opcode::Heartbeat),
        ..Match::default()
    };
    let outcome = bench
        .run_shim(
            Case::GetattrInRoot,
            10,
            &ShimOptions {
                director: DirectorKind::Reference,
                attach_faults: vec![(refuse, Fault::Status(Status::IoError))],
            },
        )
        .expect("the launch ran");
    assert_eq!(outcome.exit, Some(3), "{outcome:?}");
    let ready = outcome.ready.as_deref().unwrap_or("");
    assert!(
        ready.starts_with(aether_env::READY_FUSE_FAILED_PREFIX),
        "ready file {ready:?}"
    );
    let error = outcome.injector_error.as_deref().unwrap_or("");
    assert!(
        error.starts_with(aether_env::INJECTOR_FUSE_FAILED_PREFIX),
        "injector error {error:?}"
    );
    assert!(outcome.output.is_none(), "the target ran: {outcome:?}");
    assert!(outcome.counted.is_none(), "the target reached its loop");
}
