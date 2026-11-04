//! `aether-conform run`: a director under test behind an adapter, driven as a
//! child process. The built-in `reference-adapter` is the director here, and
//! the hidden `test-adapter` stands in for adapters that misbehave.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use aether_refdirector::tree::fill;
use serde_json::{json, Value};
use tempfile::TempDir;

const EXE: &str = env!("CARGO_BIN_EXE_aether-conform");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn write(dir: &Path, name: &str, v: &Value) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    p
}

/// Opens, reads (inline, bulk by flag and bulk by size), writes and lists.
fn files_scenario() -> Value {
    let big = fill(200_000, 3);
    json!({
        "name": "files",
        "description": "reads inline and in bulk, a write under a writable prefix, a listing",
        "tree": {"roots": [{"id": 0,
            "files": [{"path": "data/a.esp", "contents": "abc"},
                      {"path": "data/big.bsa", "contents": {"size": 200000, "seed": 3}}],
            "writable": ["saves"]}]},
        "steps": [
            {"request": {"op": "heartbeat"}, "expect": {"status": "ok"}},
            {"request": {"op": "getattr", "root": 0, "path": "data/a.esp"},
             "expect": {"reply": {"found": true, "is_dir": false, "size": 3, "mtime": 0}}},
            {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/big.bsa"},
             "expect": {"reply": {"fh": 1, "size": 200000, "is_dir": false, "immutable": false, "mount_gen": 0}}},
            {"request": {"op": "read", "fh": 1, "offset": 0, "len": 16, "slot_flags": ["read_bulk"]},
             "expect": {"reply": {"data": {"hex": hex(&big[..16])}}}},
            {"request": {"op": "read", "fh": 1, "offset": 100, "len": 131072},
             "expect": {"reply": {"data": {"hex": hex(&big[100..100 + 131072])}}}},
            {"request": {"op": "read", "fh": 1, "offset": 7, "len": 5},
             "expect": {"reply": {"data": {"hex": hex(&big[7..12])}}}},
            {"request": {"op": "close", "fh": 1}, "expect": {"status": "ok"}},
            {"request": {"op": "open", "root": 0, "flags": ["write", "create"], "path": "saves/new.txt"},
             "expect": {"reply": {"fh": 2, "size": 0, "is_dir": false, "immutable": false, "mount_gen": 0}}},
            {"request": {"op": "write", "fh": 2, "offset": 0, "data": "hi"},
             "expect": {"reply": {"written": 2}}},
            {"request": {"op": "close", "fh": 2}, "expect": {"status": "ok"}},
            {"request": {"op": "readdir", "root": 0, "path": "saves"},
             "expect": {"reply": {"entries": [{"name": "new.txt", "is_dir": false, "size": 2, "mtime": 0}]}}},
            {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "nowhere"},
             "expect": {"status": "not-found"}}
        ]
    })
}

/// Two handles at once, a use after close, and the root's listing.
fn handles_scenario() -> Value {
    json!({
        "name": "handles",
        "description": "handles paired by the session, and a closed one refused",
        "tree": {"roots": [{"id": 0,
            "files": [{"path": "data/a.esp", "contents": "abc"},
                      {"path": "data/b.esp", "contents": "defg"}],
            "dirs": ["data/empty"]}]},
        "steps": [
            {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"},
             "expect": {"reply": {"fh": 10, "size": 3, "is_dir": false, "immutable": false, "mount_gen": 0}}},
            {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/b.esp"},
             "expect": {"reply": {"fh": 11, "size": 4, "is_dir": false, "immutable": false, "mount_gen": 0}}},
            {"request": {"op": "read", "fh": 11, "offset": 0, "len": 16},
             "expect": {"reply": {"data": "defg"}}},
            {"request": {"op": "read", "fh": 10, "offset": 1, "len": 16},
             "expect": {"reply": {"data": "bc"}}},
            {"request": {"op": "close", "fh": 10}, "expect": {"status": "ok"}},
            {"request": {"op": "read", "fh": 10, "offset": 0, "len": 1},
             "expect": {"status": "bad-fh"}},
            {"request": {"op": "close", "fh": 11}, "expect": {"status": "ok"}},
            {"request": {"op": "readdir", "root": 0, "path": "data"},
             "expect": {"reply": {"entries": [
                 {"name": "empty", "is_dir": true, "size": 0, "mtime": 0},
                 {"name": "b.esp", "is_dir": false, "size": 4, "mtime": 0},
                 {"name": "a.esp", "is_dir": false, "size": 3, "mtime": 0}]}}}
        ]
    })
}

fn two_scenarios() -> TempDir {
    let dir = aether_testkit::tempdir().unwrap();
    write(dir.path(), "01-files.json", &files_scenario());
    write(dir.path(), "02-handles.json", &handles_scenario());
    dir
}

fn quoted_exe() -> String {
    format!("\"{EXE}\"")
}

fn conform(args: &[&str]) -> Output {
    Command::new(EXE).args(args).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn run_reference(dir: &Path, adapter_args: &str, extra: &[&str]) -> Output {
    let director = format!("{} reference-adapter{adapter_args}", quoted_exe());
    let scenarios = dir.to_str().unwrap();
    let mut args = vec![
        "run",
        "--director",
        &director,
        "--scenarios",
        scenarios,
        "--timeout",
        "20",
    ];
    args.extend_from_slice(extra);
    conform(&args)
}

#[test]
fn run_passes_two_scenarios_against_the_reference_adapter_on_a_file_ring() {
    let dir = two_scenarios();
    let o = run_reference(dir.path(), "", &[]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{}", stderr(&o));
    assert!(out.contains("1..2"), "{out}");
    assert!(out.contains("ok 1 - files"), "{out}");
    assert!(out.contains("ok 2 - handles"), "{out}");
    assert!(!out.contains("not ok"), "{out}");
}

#[test]
fn a_json_report_lists_every_step_of_every_scenario() {
    let dir = two_scenarios();
    let report = dir.path().join("report.out");
    let o = run_reference(
        dir.path(),
        "",
        &["--report", "json", "--out", report.to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(v["seed"], Value::Null);
    let s = v["scenarios"].as_array().unwrap();
    assert_eq!(s.len(), 2);
    assert_eq!(s[0]["name"], "files");
    assert_eq!(s[0]["passed"], true);
    assert_eq!(s[0]["steps"].as_array().unwrap().len(), 12);
    assert_eq!(s[1]["name"], "handles");
    assert_eq!(s[1]["steps"].as_array().unwrap().len(), 8);
    let step = &s[1]["steps"][5];
    assert_eq!(step["index"], 5);
    assert_eq!(step["passed"], true);
    for k in ["request", "expected", "got"] {
        assert!(step[k].is_string(), "{k}: {step}");
    }
    assert!(step["got"].as_str().unwrap().contains("bad-fh"), "{step}");
}

#[cfg(windows)]
#[test]
fn run_passes_two_scenarios_against_the_reference_adapter_on_a_named_section() {
    let dir = two_scenarios();
    let o = run_reference(dir.path(), " --ring section", &[]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{}", stderr(&o));
    assert!(out.contains("ok 1 - files"), "{out}");
    assert!(out.contains("ok 2 - handles"), "{out}");
}

#[test]
fn a_wrong_expectation_fails_with_both_values_in_the_report() {
    let dir = aether_testkit::tempdir().unwrap();
    write(
        dir.path(),
        "wrong.json",
        &json!({
            "name": "wrong",
            "description": "expects the wrong size",
            "tree": {"roots": [{"id": 0, "files": [{"path": "a.txt", "contents": "abc"}]}]},
            "steps": [
                {"request": {"op": "getattr", "root": 0, "path": "a.txt"},
                 "expect": {"reply": {"found": true, "is_dir": false, "size": 4, "mtime": 0}}},
                {"request": {"op": "heartbeat"}, "expect": {"status": "ok"}}
            ]
        }),
    );
    let report = dir.path().join("report.out");
    let o = run_reference(
        dir.path(),
        "",
        &["--report", "json", "--out", report.to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let v: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    let s = &v["scenarios"][0];
    assert_eq!(s["passed"], false);
    let step = &s["steps"][0];
    assert_eq!(step["passed"], false);
    assert!(
        step["expected"].as_str().unwrap().contains("size: 4"),
        "{step}"
    );
    assert!(step["got"].as_str().unwrap().contains("size: 3"), "{step}");
    assert!(!step["note"].as_str().unwrap_or("").is_empty(), "{step}");
    assert_eq!(s["steps"][1]["passed"], true);

    // And in TAP, on stdout.
    let o = run_reference(dir.path(), "", &[]);
    assert_eq!(o.status.code(), Some(1));
    let out = stdout(&o);
    assert!(out.contains("not ok 1 - wrong"), "{out}");
    assert!(out.contains("size: 4") && out.contains("size: 3"), "{out}");
}

/// Runs `test-adapter --misbehave <how>` with a 2 s timeout and returns its
/// output and how long it took.
fn misbehaving(how: &str) -> (Output, Duration) {
    let dir = two_scenarios();
    let director = format!("{} test-adapter --misbehave {how}", quoted_exe());
    let t = Instant::now();
    let o = conform(&[
        "run",
        "--director",
        &director,
        "--scenarios",
        dir.path().to_str().unwrap(),
        "--timeout",
        "2",
    ]);
    (o, t.elapsed())
}

fn all_output(o: &Output) -> String {
    format!("{}\n{}", stdout(o), stderr(o))
}

#[test]
fn an_adapter_that_exits_at_once_ends_the_run_with_its_stderr() {
    let (o, took) = misbehaving("exit");
    let all = all_output(&o);
    assert_eq!(o.status.code(), Some(2), "{all}");
    assert!(all.contains("exited"), "{all}");
    assert!(all.contains("test adapter: exiting"), "{all}");
    assert!(stdout(&o).contains("not ok 1 - files"), "{all}");
    assert!(!stdout(&o).contains("handles"), "the run ends: {all}");
    assert!(took < Duration::from_secs(15), "{took:?}");
}

#[test]
fn an_adapter_that_prints_garbage_ends_the_run_naming_the_line() {
    let (o, took) = misbehaving("garbage");
    let all = all_output(&o);
    assert_eq!(o.status.code(), Some(2), "{all}");
    assert!(all.contains("not JSON"), "{all}");
    assert!(all.contains("this is not json"), "{all}");
    assert!(all.contains("test adapter: answering garbage"), "{all}");
    assert!(took < Duration::from_secs(15), "{took:?}");
}

#[test]
fn an_adapter_that_never_answers_ends_the_run_at_the_timeout() {
    let (o, took) = misbehaving("hang");
    let all = all_output(&o);
    assert_eq!(o.status.code(), Some(2), "{all}");
    assert!(all.contains("no answer within 2 s"), "{all}");
    assert!(all.contains("test adapter: not answering"), "{all}");
    assert!(took >= Duration::from_secs(2), "{took:?}");
    assert!(took < Duration::from_secs(15), "{took:?}");
}

#[test]
fn a_refused_mount_fails_that_scenario_and_the_run_goes_on() {
    let (o, _) = misbehaving("refuse");
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{}", all_output(&o));
    assert!(out.contains("not ok 1 - files"), "{out}");
    assert!(out.contains("not ok 2 - handles"), "{out}");
    assert!(out.contains("mount refused: no trees today"), "{out}");
}

#[test]
fn usage_errors_exit_2() {
    let o = conform(&["run"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--director"), "{}", stderr(&o));
    let o = conform(&["run", "--director", "x", "--report", "yaml"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("yaml"), "{}", stderr(&o));
    let o = conform(&["frobnicate"]);
    assert_eq!(o.status.code(), Some(2));
    let dir = aether_testkit::tempdir().unwrap();
    let director = format!("{} reference-adapter", quoted_exe());
    let o = conform(&[
        "run",
        "--director",
        &director,
        "--scenarios",
        dir.path().to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(2), "an empty folder is not a pass");
    assert!(stderr(&o).contains("no scenarios"), "{}", stderr(&o));
}
