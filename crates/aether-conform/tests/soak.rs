//! `aether-conform soak`: long generated runs with fresh seeds, played here
//! in seconds against the reference adapter, one broken on purpose, and an
//! adapter that cannot serve. The summary's fields, the failures written for
//! replay, and the deduplication of a failure found again.

use std::path::Path;
use std::process::{Command, Output};

use aether_conform::scenario::load_dir;
use serde_json::Value;

const EXE: &str = env!("CARGO_BIN_EXE_aether-conform");

fn quoted_exe() -> String {
    format!("\"{EXE}\"")
}

fn soak(director: &str, extra: &[&str], failures: &Path, summary: &Path) -> (Output, Value) {
    let mut args = vec![
        "soak",
        "--director",
        director,
        "--timeout",
        "20",
        "--failures",
        failures.to_str().unwrap(),
        "--summary",
        summary.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let o = Command::new(EXE).args(&args).output().unwrap();
    let text = std::fs::read_to_string(summary).unwrap_or_else(|e| {
        panic!(
            "no summary ({e}):\n{}\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    });
    let v: Value = serde_json::from_str(&text).expect("a JSON summary");
    // The summary is also what the run prints.
    let printed: Value = serde_json::from_slice(&o.stdout).expect("the summary on stdout");
    assert_eq!(printed, v);
    (o, v)
}

fn reference(args: &str) -> String {
    format!("{} reference-adapter{args}", quoted_exe())
}

/// The generated parts only, small batches, no concurrent rounds: seconds.
const QUICK: [&str; 5] = [
    "--generated-only",
    "--no-concurrent",
    "--steps",
    "8",
    "--batch-cases",
];

fn quick(batch: &'static str, more: &[&'static str]) -> Vec<&'static str> {
    let mut v = QUICK.to_vec();
    v.push(batch);
    v.extend_from_slice(more);
    v
}

#[test]
fn a_clean_soak_ends_zero_and_its_summary_counts_what_ran() {
    let dir = aether_testkit::tempdir().unwrap();
    let (failures, summary) = (dir.path().join("f"), dir.path().join("summary.json"));
    let extra = quick("8", &["--cases", "24", "--seed", "77"]);
    let (o, v) = soak(&reference(""), &extra, &failures, &summary);
    assert_eq!(o.status.code(), Some(0), "{v:#}");
    assert_eq!(v["passed"], true);
    assert_eq!(v["batches"], 3);
    assert_eq!(v["cases"], 24);
    assert_eq!(v["rounds"], 0);
    assert_eq!(v["seed"], 77);
    assert_eq!(v["budget"], serde_json::json!({"cases": 24}));
    assert_eq!(v["failures"], serde_json::json!([]));
    assert_eq!(v["distinct_failures"], 0);
    assert_eq!(v["failures_seen"], 0);
    assert!(v["wall_seconds"].as_f64().unwrap() > 0.0, "{v:#}");
    assert!(v["director"]
        .as_str()
        .unwrap()
        .contains("reference-adapter"));
    // A fresh seed for each batch, all different, the first as given.
    let seeds: Vec<u64> = v["seeds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_u64().unwrap())
        .collect();
    assert_eq!(seeds.len(), 3);
    assert_ne!(seeds[0], seeds[1]);
    assert_ne!(seeds[1], seeds[2]);
    assert_ne!(seeds[0], seeds[2]);
    // The same first seed gives the same seeds again.
    let again = dir.path().join("again.json");
    let (_, w) = soak(&reference(""), &extra, &failures, &again);
    assert_eq!(w["seeds"], v["seeds"]);
}

/// A time budget ends the run after the batch that passes it, and says so.
#[test]
fn a_time_budget_ends_the_soak_after_a_batch() {
    let dir = aether_testkit::tempdir().unwrap();
    let (failures, summary) = (dir.path().join("f"), dir.path().join("s.json"));
    let extra = quick("4", &["--for", "2s"]);
    let (o, v) = soak(&reference(""), &extra, &failures, &summary);
    assert_eq!(o.status.code(), Some(0), "{v:#}");
    assert_eq!(v["budget"], serde_json::json!({"for_seconds": 2.0}));
    assert!(v["batches"].as_u64().unwrap() >= 1, "{v:#}");
    assert!(v["wall_seconds"].as_f64().unwrap() >= 2.0, "{v:#}");
    assert!(v["wall_seconds"].as_f64().unwrap() < 60.0, "{v:#}");
}

/// A director that breaks a rule fails the soak, which keeps going through
/// its batches, writes each distinct shrunk failure with its seed, and
/// counts a failure found again once.
#[test]
fn a_broken_director_is_found_kept_going_past_and_each_failure_written_once() {
    let dir = aether_testkit::tempdir().unwrap();
    let (failures, summary) = (dir.path().join("f"), dir.path().join("s.json"));
    let extra = quick("8", &["--cases", "48", "--seed", "5"]);
    let director = reference(" --break found-for-missing");
    let (o, v) = soak(&director, &extra, &failures, &summary);
    assert_eq!(o.status.code(), Some(1), "{v:#}");
    assert_eq!(v["passed"], false);
    // Every batch ran: a failure ends a batch, not the soak.
    assert!(v["batches"].as_u64().unwrap() >= 6, "{v:#}");
    // The cases a failing batch played stop at its failing case.
    assert!(v["cases"].as_u64().unwrap() >= 48, "{v:#}");
    let seen = v["failures_seen"].as_u64().unwrap();
    let list = v["failures"].as_array().unwrap();
    assert!(seen >= 2, "{v:#}");
    assert_eq!(v["distinct_failures"].as_u64().unwrap(), list.len() as u64);
    assert!(!list.is_empty() && list.len() as u64 <= seen, "{v:#}");
    assert_eq!(
        list.iter()
            .map(|f| f["occurrences"].as_u64().unwrap())
            .sum::<u64>(),
        seen,
        "{v:#}"
    );
    let seeds: Vec<u64> = v["seeds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_u64().unwrap())
        .collect();
    for f in list {
        let id = f["id"].as_str().unwrap();
        assert_eq!(f["kind"], "generated");
        assert_eq!(f["class"], "mismatch at getattr", "{f:#}");
        assert!(seeds.contains(&f["seed"].as_u64().unwrap()), "{f:#}");
        // A folder of its own: the scenario and what replays it.
        let folder = failures.join(id);
        assert!(folder.join(format!("{id}.json")).is_file(), "{f:#}");
        let replay = std::fs::read_to_string(folder.join("replay.txt")).unwrap();
        assert!(replay.contains(&f["seed"].to_string()), "{replay}");
        assert!(replay.contains("--scenarios"), "{replay}");
        assert_eq!(
            f["replay"].as_str().unwrap(),
            replay
                .lines()
                .find(|l| l.contains("--scenarios"))
                .unwrap()
                .trim()
        );
        // The written scenario loads, and plays as any scenario does:
        // against the same broken director it fails, against the reference
        // it passes.
        assert_eq!(load_dir(&folder).unwrap().len(), 1);
        let play = |director: &str| {
            Command::new(EXE)
                .args([
                    "run",
                    "--director",
                    director,
                    "--scenarios",
                    folder.to_str().unwrap(),
                    "--timeout",
                    "20",
                ])
                .output()
                .unwrap()
                .status
                .code()
        };
        assert_eq!(play(&director), Some(1), "{f:#}");
        assert_eq!(play(&reference("")), Some(0), "{f:#}");
    }
}

/// An adapter that cannot serve fails every batch the same way: one failure,
/// counted each time, and the soak stops when batches make no progress
/// rather than spinning for its whole budget.
#[test]
fn an_adapter_that_cannot_serve_is_one_failure_and_ends_the_soak() {
    let dir = aether_testkit::tempdir().unwrap();
    let (failures, summary) = (dir.path().join("f"), dir.path().join("s.json"));
    let director = format!("{} test-adapter --misbehave exit", quoted_exe());
    let extra = quick("4", &["--for", "10m"]);
    let (o, v) = soak(&director, &extra, &failures, &summary);
    assert_eq!(o.status.code(), Some(1), "{v:#}");
    assert_eq!(v["distinct_failures"], 1, "{v:#}");
    assert_eq!(v["failures"][0]["kind"], "adapter");
    assert!(
        v["failures"][0]["occurrences"].as_u64().unwrap() >= 2,
        "{v:#}"
    );
    assert_eq!(v["stopped"], "no progress", "{v:#}");
    assert!(v["wall_seconds"].as_f64().unwrap() < 120.0, "{v:#}");
}

#[test]
fn a_soak_needs_a_budget_and_a_director() {
    let dir = aether_testkit::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(EXE)
            .arg("soak")
            .args(args)
            .arg("--failures")
            .arg(dir.path())
            .output()
            .unwrap()
    };
    let o = run(&["--director", "x"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("--for"));
    let o = run(&["--for", "1m"]);
    assert_eq!(o.status.code(), Some(2));
    let o = run(&["--director", "x", "--for", "1m", "--cases", "5"]);
    assert_eq!(o.status.code(), Some(2));
    let o = run(&["--director", "x", "--for", "soon"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("soon"));
}
