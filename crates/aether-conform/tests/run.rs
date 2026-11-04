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

/// Adds `--failures <failures>` to `args` when they generate cases or
/// rounds (`--generate`, `--concurrent`) and name no folder: the default is
/// a folder in the temporary directory named by the seed alone, which tests
/// with one seed would share.
fn own_failures<'a>(args: &mut Vec<&'a str>, failures: &'a Path) {
    let generates = args
        .iter()
        .any(|a| *a == "--generate" || *a == "--concurrent");
    if generates && !args.contains(&"--failures") {
        args.extend(["--failures", failures.to_str().unwrap()]);
    }
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
    let failures = aether_testkit::tempdir().unwrap();
    own_failures(&mut args, failures.path());
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
    for k in ["expected", "got"] {
        assert!(step[k].is_string(), "{k}: {step}");
    }
    // The request as the scenario wrote it, in the scenario format.
    assert_eq!(step["request"]["op"], "read", "{step}");
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
    assert_eq!(
        step["request"],
        json!({"op": "getattr", "root": 0, "path": "a.txt"}),
        "{step}"
    );
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
    let line = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("request: "))
        .unwrap_or_else(|| panic!("no request line: {out}"));
    let request: Value = serde_json::from_str(line).unwrap();
    assert_eq!(
        request,
        json!({"op": "getattr", "root": 0, "path": "a.txt"}),
        "{out}"
    );
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

/// Whether process `pid` is still running (a zombie counts as gone).
fn alive(pid: u32) -> bool {
    if cfg!(windows) {
        let o = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).contains("aether-conform")
    } else {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // The state follows the command name, which is in parentheses.
            Ok(stat) => !stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
            Err(_) => Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|s| s.success()),
        }
    }
}

#[test]
fn a_hung_adapter_is_killed_with_the_processes_it_started() {
    let (o, took) = misbehaving("hang-grandchild");
    let all = all_output(&o);
    assert_eq!(o.status.code(), Some(2), "{all}");
    assert!(all.contains("no answer within 2 s"), "{all}");
    assert!(took < Duration::from_secs(15), "{took:?}: {all}");
    let pid: u32 = all
        .split("test adapter: grandchild pid ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no grandchild pid in: {all}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(pid), "the grandchild {pid} outlived the run: {all}");
}

/// The committed suite (`contract/scenarios`) passes against the reference:
/// the check CI runs with the built binary, also made by `cargo test`.
#[test]
fn the_committed_scenarios_pass_against_the_reference_adapter() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let o = run_reference(&dir, "", &[]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}\n{}", stderr(&o));
    assert!(!out.contains("not ok"), "{out}");
    // The ones the read-cache and read-only checks rest on among them.
    for name in [
        "cache-immutable-readonly",
        "cache-overwrite-same-size",
        "readonly-root-refuses-changes",
        "multi-handle-shared",
        "multi-handles-unique",
        "multi-registry-gen",
        "multi-write-visible",
    ] {
        assert!(out.contains(&format!(" - {name}\n")), "{name}: {out}");
    }
}

/// The committed suite against `test-adapter --misbehave <how>`, as a JSON
/// report: each scenario's name, whether it passed, and its failing steps'
/// notes.
fn committed_against(how: &str) -> Vec<(String, bool, Vec<String>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let out = aether_testkit::tempdir().unwrap();
    let report = out.path().join("report.json");
    let director = format!("{} test-adapter --misbehave {how}", quoted_exe());
    let o = conform(&[
        "run",
        "--director",
        &director,
        "--scenarios",
        dir.to_str().unwrap(),
        "--timeout",
        "20",
        "--report",
        "json",
        "--out",
        report.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", all_output(&o));
    let v: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    v["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let notes = s["steps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|st| st["passed"] == false)
                .map(|st| st["note"].as_str().unwrap_or("").to_string())
                .collect();
            (
                s["name"].as_str().unwrap().to_string(),
                s["passed"] == true,
                notes,
            )
        })
        .collect()
}

/// wire.md 3.4.3: a director that calls a writable file's read opens
/// immutable under one `mount_gen`, and lets it be overwritten at the same
/// size, fails the read-cache check, and only that.
#[test]
fn a_director_marking_writable_files_immutable_fails_the_read_cache_check() {
    let results = committed_against("stale-immutable");
    let (_, passed, notes) = results
        .iter()
        .find(|(n, ..)| n == "cache-overwrite-same-size")
        .expect("the scenario ran");
    assert!(!passed, "{results:?}");
    assert!(!notes.is_empty(), "{results:?}");
    for n in notes {
        assert!(n.contains("wire.md 3.4.3"), "{n}");
        assert!(n.contains("stale bytes"), "{n}");
    }
    let failed: Vec<&str> = results
        .iter()
        .filter(|(_, p, _)| !p)
        .map(|(n, ..)| n.as_str())
        .collect();
    assert_eq!(failed, ["cache-overwrite-same-size"], "{results:?}");
}

/// wire.md 4.5: a director that never moves `reg_gen` after a write fails
/// every scenario with a successful registry write, citing 4.5, and no other.
#[test]
fn a_director_that_never_moves_reg_gen_fails_the_registry_write_scenarios() {
    let results = committed_against("frozen-reg-gen");
    let failed: Vec<&str> = results
        .iter()
        .filter(|(_, p, _)| !p)
        .map(|(n, ..)| n.as_str())
        .collect();
    for name in [
        "registry-create-key",
        "registry-delete-key",
        "registry-delete-value",
        "registry-rename-key",
        "registry-values",
        "multi-registry-gen",
    ] {
        assert!(failed.contains(&name), "{name}: {results:?}");
    }
    for (name, passed, notes) in &results {
        if !passed {
            assert!(
                name.starts_with("registry-") || name == "multi-registry-gen",
                "{name}: {notes:?}"
            );
            assert!(
                notes.iter().all(|n| n.contains("wire.md 4.5")),
                "{name}: {notes:?}"
            );
        }
    }
    assert!(
        results.iter().any(|(n, p, _)| n == "registry-none" && *p),
        "no registry, no generation to move: {results:?}"
    );
}

// ---------------------------------------------------------------------------
// Several clients on one ring (wire.md 2.5 to 2.8.1).

/// A folder holding the committed `multi-*` scenarios, which play steps from
/// client processes of their own.
fn multi_scenarios() -> TempDir {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let dir = aether_testkit::tempdir().unwrap();
    for e in std::fs::read_dir(&from).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("multi-") && name.ends_with(".json") {
            std::fs::copy(&p, dir.path().join(&name)).unwrap();
        }
    }
    dir
}

/// One scenario of a JSON report: name, passed, error, failing steps' notes.
type Entry = (String, bool, Option<String>, Vec<String>);

/// `run` against `director` (the arguments after the binary) over the
/// scenarios in `dir`, with `extra` arguments, as a JSON report: its exit
/// code, its entries, the `file` of the `concurrent` entry (if any), and
/// everything the run printed. A run that generates is given `--failures`
/// in a folder of its own unless `extra` names one ([`own_failures`]).
fn run_json(
    director: &str,
    dir: &Path,
    timeout: &str,
    extra: &[&str],
) -> (Option<i32>, Vec<Entry>, String, String) {
    let out = aether_testkit::tempdir().unwrap();
    let report = out.path().join("report.json");
    let director = format!("{} {director}", quoted_exe());
    let failures = out.path().join("failures");
    let mut args = vec![
        "run",
        "--director",
        &director,
        "--scenarios",
        dir.to_str().unwrap(),
        "--timeout",
        timeout,
        "--report",
        "json",
        "--out",
        report.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    own_failures(&mut args, &failures);
    let o = conform(&args);
    let all = all_output(&o);
    let v: Value = match std::fs::read(&report) {
        Ok(b) => serde_json::from_slice(&b).unwrap(),
        Err(e) => panic!("no report ({e}): {all}"),
    };
    let mut concurrent = String::new();
    let entries = v["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            if s["name"] == "concurrent" {
                concurrent = s["file"].as_str().unwrap_or("").to_string();
            }
            let notes = s["steps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|st| st["passed"] == false)
                .map(|st| st["note"].as_str().unwrap_or("").to_string())
                .collect();
            (
                s["name"].as_str().unwrap().to_string(),
                s["passed"] == true,
                s["error"].as_str().map(str::to_string),
                notes,
            )
        })
        .collect();
    (o.status.code(), entries, concurrent, all)
}

fn entry<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
    entries
        .iter()
        .find(|e| e.0 == name)
        .unwrap_or_else(|| panic!("no {name} in {entries:?}"))
}

/// Four clients at once, each a generated case, their requests in flight
/// together, against the reference over a file ring: every reply matches,
/// every slot carries a request and every bank a bulk reply, and more than
/// one request was in flight at once.
#[test]
fn concurrent_clients_pass_against_the_reference_on_a_file_ring() {
    let dir = multi_scenarios();
    let (code, entries, summary, all) = run_json(
        "reference-adapter",
        dir.path(),
        "20",
        &["--concurrent", "4", "--concurrent-cases", "48"],
    );
    assert_eq!(code, Some(0), "{entries:?}\n{all}");
    assert!(entries.iter().all(|e| e.1), "{entries:?}");
    assert_eq!(entries.len(), 5, "{entries:?}");
    assert!(
        summary.contains("8 of 8 slots and 8 banks used"),
        "{summary}"
    );
    let most: u32 = summary
        .rsplit("at most ")
        .next()
        .and_then(|r| r.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{summary}"));
    assert!(most >= 2, "requests were not in flight together: {summary}");
}

/// The same over a named section, with its events.
#[cfg(windows)]
#[test]
fn several_clients_pass_against_the_reference_on_a_named_section() {
    let dir = multi_scenarios();
    let (code, entries, summary, all) = run_json(
        "reference-adapter --ring section",
        dir.path(),
        "20",
        &["--concurrent", "4", "--concurrent-cases", "16"],
    );
    assert_eq!(code, Some(0), "{entries:?}\n{all}");
    assert!(entries.iter().all(|e| e.1), "{entries:?}");
    assert!(summary.contains("8 of 8 slots"), "{summary}");
}

const CONCURRENT_FEW: [&str; 6] = [
    "--concurrent",
    "4",
    "--concurrent-cases",
    "8",
    "--seed",
    "11",
];

/// The committed suite against `test-adapter --misbehave <how>`, with the
/// concurrent clients: the entries, and everything the run printed.
fn committed_with(how: &str, timeout: &str) -> (Vec<Entry>, String) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let (code, entries, _, all) = run_json(
        &format!("test-adapter --misbehave {how}"),
        &dir,
        timeout,
        &CONCURRENT_FEW,
    );
    assert_eq!(code, Some(1), "{entries:?}\n{all}");
    (entries, all)
}

/// A failing concurrent round is written inside the test's own folder,
/// never to the run's default (`aether-conform-failures-<seed>` in the
/// temporary directory), which every test with that seed would share.
#[test]
fn a_failing_round_is_written_inside_the_tests_own_folder() {
    let dir = multi_scenarios();
    let (code, entries, file, all) = run_json(
        "test-adapter --misbehave per-slot-fh",
        dir.path(),
        "20",
        &CONCURRENT_FEW,
    );
    assert_eq!(code, Some(1), "{all}");
    assert!(!entry(&entries, "concurrent").1, "{entries:?}");
    assert!(
        !file.contains("aether-conform-failures-11"),
        "written to the shared default folder: {file}"
    );
    assert!(
        Path::new(&file).starts_with(aether_testkit::scratch_root()),
        "{file}"
    );
}

fn failed(entries: &[Entry]) -> Vec<&str> {
    entries
        .iter()
        .filter(|e| !e.1)
        .map(|e| e.0.as_str())
        .collect()
}

/// wire.md 2.8.1: handles are ring-global. A director that numbers handles
/// per slot (per connection) passes every single-client scenario, since one
/// client sending one request at a time only uses slot 0, and fails the
/// scenarios where another process presents or is issued a handle, and the
/// concurrent clients.
#[test]
fn a_director_numbering_handles_per_slot_fails_only_with_several_clients() {
    let (entries, all) = committed_with("per-slot-fh", "20");
    assert_eq!(
        failed(&entries),
        ["multi-handle-shared", "multi-handles-unique", "concurrent"],
        "{all}"
    );
    let unique = entry(&entries, "multi-handles-unique");
    assert!(
        unique.3.iter().any(|n| n.contains("wire.md 3.4.3")),
        "{unique:?}"
    );
    let concurrent = entry(&entries, "concurrent");
    assert!(
        concurrent
            .3
            .iter()
            .any(|n| n.contains("wire.md 2.8.1") || n.contains("wire.md 3.4.3")),
        "{concurrent:?}"
    );
}

/// wire.md 2.5: every slot a client publishes is taken and completed. A
/// director that only ever takes slot 0 passes every single-client
/// scenario and fails every one with a second client, and the concurrent
/// clients, at the timeout.
#[test]
fn a_director_serving_only_slot_zero_fails_every_second_client() {
    // The failures come at the timeout, so it is short; it bounds only the
    // runs expected to fail. The single-client suite is played on its own,
    // with the usual timeout, so a slow machine cannot fail it.
    let dir = multi_scenarios();
    let (code, entries, _, all) = run_json(
        "test-adapter --misbehave slot-zero-only",
        dir.path(),
        "2",
        &CONCURRENT_FEW,
    );
    assert_eq!(code, Some(1), "{all}");
    assert_eq!(
        failed(&entries),
        [
            "multi-handle-shared",
            "multi-handles-unique",
            "multi-registry-gen",
            "multi-write-visible",
            "concurrent"
        ],
        "{all}"
    );
    for e in entries.iter().filter(|e| !e.1) {
        let error = e.2.as_deref().unwrap_or("");
        assert!(error.contains("no answer within 2 s"), "{e:?}");
        assert!(error.contains("wire.md 2.5"), "{e:?}");
    }
    let single = single_client_scenarios();
    let (code, entries, _, all) = run_json(
        "test-adapter --misbehave slot-zero-only",
        single.path(),
        "20",
        &[],
    );
    assert_eq!(failed(&entries), Vec::<&str>::new(), "{all}");
    assert_eq!(code, Some(0), "{all}");
}

/// A folder holding the committed scenarios but the `multi-*` ones: every
/// step of them sent by client 0.
fn single_client_scenarios() -> TempDir {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let dir = aether_testkit::tempdir().unwrap();
    for e in std::fs::read_dir(&from).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("multi-") && name.ends_with(".json") {
            std::fs::copy(&p, dir.path().join(&name)).unwrap();
        }
    }
    dir
}

/// wire.md 2.6: slot `s` owns bank `s % banks`. A director that writes every
/// bulk reply into bank 0 passes every single-client scenario and fails the
/// bulk READ from client 1 (slot 1), and the concurrent clients.
#[test]
fn a_director_writing_every_bulk_reply_into_bank_zero_fails_on_other_slots() {
    let (entries, all) = committed_with("bank-zero", "20");
    assert_eq!(
        failed(&entries),
        ["multi-handle-shared", "concurrent"],
        "{all}"
    );
    let shared = entry(&entries, "multi-handle-shared");
    assert!(
        shared
            .3
            .iter()
            .any(|n| n.contains("wire.md 2.6") && n.contains("not in bank 1")),
        "{shared:?}"
    );
    let concurrent = entry(&entries, "concurrent");
    let error = concurrent.2.as_deref().unwrap_or("");
    assert!(
        concurrent.3.iter().any(|n| n.contains("wire.md 2.6")) || error.contains("2.6"),
        "{concurrent:?}"
    );
}

/// wire.md 2.5: a reply goes into the slot of the request it answers. A
/// director that swaps the replies of two requests in flight together
/// passes every scenario, whose steps go one at a time, and fails the
/// concurrent clients, whose requests each name their own client's paths.
#[test]
fn a_director_swapping_replies_in_flight_together_fails_the_concurrent_clients() {
    let (entries, all) = committed_with("swap-replies", "20");
    assert_eq!(failed(&entries), ["concurrent"], "{all}");
    // Failed for a reply in the wrong slot, not a timeout. Which mismatch
    // shows first depends on which two requests were swapped: another
    // client's answer, a status meant for another request, or a payload of
    // another opcode's shape that does not decode.
    let concurrent = entry(&entries, "concurrent");
    let error = concurrent.2.as_deref().unwrap_or("");
    assert!(!error.contains("no answer within"), "{concurrent:?}");
    let mismatch = |s: &str| {
        s.contains("wire.md 2.5")
            || s.starts_with("a different")
            || s.starts_with("none of the allowed")
            || s.contains("does not decode")
            || s.contains(" got Got(")
    };
    assert!(
        mismatch(error) || concurrent.3.iter().any(|n| mismatch(n)),
        "{concurrent:?}"
    );
}

/// wire.md 3.4.3: "`fh` is any u64 the director chooses". A director that
/// numbers handles down from `u64::MAX` passes everything, the concurrent
/// clients included: no client sends a number of its own choosing for a
/// handle it has none for, which could be another client's live handle.
#[test]
fn a_director_numbering_handles_down_from_u64_max_passes_everything() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let failures = aether_testkit::tempdir().unwrap();
    let (code, entries, _, all) = run_json(
        "test-adapter --misbehave downward-fh",
        &dir,
        "20",
        &[
            "--generate",
            "--cases",
            "64",
            "--concurrent",
            "4",
            "--concurrent-cases",
            "48",
            "--seed",
            "11",
            "--failures",
            failures.path().to_str().unwrap(),
        ],
    );
    assert_eq!(failed(&entries), Vec::<&str>::new(), "{all}");
    assert_eq!(code, Some(0), "{all}");
}

/// wire.md 2.8.1: "a process that is killed never sends CLOSE for its open
/// handles". A client process attached from the mount's environment opens
/// handles and is killed; the director goes on answering this process's
/// client and a new client process, which reads through a handle the killed
/// one left open (the director keeps them until the session ends), and new
/// handles do not collide with those.
fn a_killed_client(ring: &str) {
    use aether_conform::adapter::Adapter;
    use aether_conform::attach::Attached;
    use aether_conform::clients::{call_on, Answer, ProcessClient};
    use aether_conform::scenario::{OwnedReply, OwnedRequest};
    use aether_protocol::{OpenFlags, RequestFlags, RootId};

    let timeout = Duration::from_secs(20);
    let dir = aether_testkit::tempdir().unwrap();
    let file = write(
        dir.path(),
        "killed.json",
        &json!({
            "name": "killed",
            "description": "a tree for a client that is killed",
            "tree": {"roots": [{"id": 0,
                "files": [{"path": "a.txt", "contents": "alpha"},
                          {"path": "b.txt", "contents": "beta"}],
                "writable": ["w"]}]},
            "steps": []
        }),
    );
    let mut adapter = Adapter::start(
        &format!("{} reference-adapter --ring {ring}", quoted_exe()),
        timeout,
    )
    .unwrap();
    let env = adapter.mount(&file).unwrap().unwrap();
    let open = |path: &str| OwnedRequest::Open {
        root: RootId(0),
        flags: OpenFlags::READ,
        path: path.into(),
    };
    let read = |fh: u64| OwnedRequest::Read {
        slot_flags: RequestFlags::empty(),
        fh,
        offset: 0,
        len: 16,
    };
    let fh_of = |a: &Answer| match a {
        Answer::Got(Ok(OwnedReply::Open(o))) => o.fh,
        other => panic!("not an open: {other:?}"),
    };

    let mut doomed = ProcessClient::start(Path::new(EXE), 1, &env, timeout).unwrap();
    let left: Vec<u64> = ["a.txt", "b.txt", "a.txt"]
        .iter()
        .map(|p| fh_of(&doomed.send(&open(p)).unwrap().outcome.answer))
        .collect();
    assert_eq!(doomed.kill(), None);
    assert!(
        doomed.send(&OwnedRequest::Heartbeat).is_err(),
        "a killed client cannot answer"
    );

    // This process's own client goes on.
    let me = Attached::from_env(&env, timeout).unwrap();
    let hb = call_on(&me, 0, &OwnedRequest::Heartbeat).unwrap();
    assert_eq!(hb.answer, Answer::Got(Ok(OwnedReply::Empty)));
    let mine = fh_of(&call_on(&me, 0, &open("b.txt")).unwrap().answer);
    assert!(
        !left.contains(&mine),
        "{mine} is still open in the killed client: {left:?}"
    );
    assert_eq!(
        call_on(&me, 0, &read(mine)).unwrap().answer,
        Answer::Got(Ok(OwnedReply::Read(b"beta".to_vec())))
    );

    // A new client process, on another slot, reads through a handle the
    // killed one left open, and is issued a number of its own.
    let mut next = ProcessClient::start(Path::new(EXE), 2, &env, timeout).unwrap();
    let r = next.send(&read(left[0])).unwrap();
    assert_eq!(r.outcome.slot, 2);
    assert_eq!(
        r.outcome.answer,
        Answer::Got(Ok(OwnedReply::Read(b"alpha".to_vec())))
    );
    let theirs = fh_of(&next.send(&open("a.txt")).unwrap().outcome.answer);
    assert!(
        !left.contains(&theirs) && theirs != mine,
        "{theirs}: {left:?}, {mine}"
    );
    drop(next);
    drop(me);
    adapter.unmount().unwrap();
    adapter.finish();
}

#[test]
fn a_killed_client_process_leaves_the_director_serving_the_others_on_a_file_ring() {
    a_killed_client("file");
}

#[cfg(windows)]
#[test]
fn a_killed_client_process_leaves_the_director_serving_the_others_on_a_named_section() {
    a_killed_client("section");
}

// ---------------------------------------------------------------------------
// Requests sized from the ring (`--limits`, `aether_conform::limits`).

/// A folder holding the one scenario `heartbeat`, for runs whose part under
/// test is a built-in one.
fn heartbeat_only() -> TempDir {
    let from =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios/heartbeat.json");
    let dir = aether_testkit::tempdir().unwrap();
    std::fs::copy(&from, dir.path().join("heartbeat.json")).unwrap();
    dir
}

/// `run --limits` against `director`: its exit code, and the `limits` entry.
fn limits_against(director: &str) -> (Option<i32>, Entry, String) {
    let dir = heartbeat_only();
    let (code, entries, _, all) = run_json(director, dir.path(), "20", &["--limits"]);
    assert!(entry(&entries, "heartbeat").1, "{all}");
    (code, entry(&entries, "limits").clone(), all)
}

/// The reference meets every limit, over a file ring.
#[test]
fn the_reference_passes_the_limits_on_a_file_ring() {
    let (code, limits, all) = limits_against("reference-adapter");
    assert!(limits.1, "{limits:?}\n{all}");
    assert_eq!(code, Some(0), "{all}");
}

/// The same over a named section.
#[cfg(windows)]
#[test]
fn the_reference_passes_the_limits_on_a_named_section() {
    let (code, limits, all) = limits_against("reference-adapter --ring section");
    assert!(limits.1, "{limits:?}\n{all}");
    assert_eq!(code, Some(0), "{all}");
}

/// The same on a ring with no bulk arena, where every READ is answered
/// inline: one longer than `payload_cap - 8` is answered with that many
/// bytes (wire.md 3.4.4), never overflowing the reply.
#[test]
fn the_reference_passes_the_limits_without_an_arena() {
    let (code, limits, all) = limits_against("reference-adapter --no-arena");
    assert!(
        limits.1,
        "{limits:?}
{all}"
    );
    assert_eq!(code, Some(0), "{all}");
}

/// The committed scenarios, the malformed requests and the generated cases
/// pass on a ring with no bulk arena too.
#[test]
fn the_committed_scenarios_pass_without_an_arena() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let failures = aether_testkit::tempdir().unwrap();
    let (code, entries, _, all) = run_json(
        "reference-adapter --no-arena",
        &dir,
        "20",
        &[
            "--malformed",
            "--generate",
            "--cases",
            "32",
            "--seed",
            "5",
            "--failures",
            failures.path().to_str().unwrap(),
        ],
    );
    assert_eq!(failed(&entries), Vec::<&str>::new(), "{all}");
    assert_eq!(code, Some(0), "{all}");
}

/// `run --limits` against `test-adapter --misbehave <how>`: the failing
/// steps' notes, the run having failed.
fn limits_failing(how: &str) -> Vec<String> {
    let (code, limits, all) = limits_against(&format!("test-adapter --misbehave {how}"));
    assert_eq!(code, Some(1), "{all}");
    assert!(!limits.1, "{limits:?}");
    assert_eq!(limits.2, None, "{limits:?}");
    limits.3
}

/// wire.md 2.6 and 3.4.4: a director whose bulk answers stop at 256 KiB
/// fails the READs of the shim's own chunk, which it would read as end of
/// file.
#[test]
fn a_director_answering_short_bulk_reads_fails_the_shims_chunk() {
    let notes = limits_failing("short-bulk");
    assert!(
        notes
            .iter()
            .any(|n| n.contains("the shim's bulk chunk") && n.contains("wire.md 2.6")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("read cache's run")),
        "{notes:?}"
    );
}

/// wire.md 3.4.2 and 2.5 step 3: a listing over the cap cut short and
/// answered OK fails, and only that.
#[test]
fn a_director_truncating_a_listing_over_the_cap_fails() {
    let notes = limits_failing("truncate-readdir");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("overflow status i32::MIN"), "{notes:?}");
    assert!(notes[0].contains("wire.md 3.4.2"), "{notes:?}");
}

/// wire.md 2.5 and 2.6: a director that writes a bank again after its slot
/// is completed fails the READs of one handle held together, and only them.
#[test]
fn a_director_writing_a_completed_slots_bank_fails_the_held_reads() {
    let notes = limits_failing("late-bank");
    assert!(!notes.is_empty());
    for n in &notes {
        assert!(n.contains("COMPLETED") && n.contains("2.6"), "{notes:?}");
    }
}

/// wire.md 2.5: a director that stores COMPLETED over ABANDONED fails the
/// abandoned slot.
#[test]
fn a_director_completing_an_abandoned_slot_fails() {
    let notes = limits_failing("complete-abandoned");
    assert!(
        notes
            .iter()
            .any(|n| n.contains("never store COMPLETED over ABANDONED")),
        "{notes:?}"
    );
}

/// wire.md 4.5: a director that moves `reg_gen` before the write is visible
/// fails the registry writer against a reader, and only that.
#[test]
fn a_director_moving_reg_gen_before_the_write_fails_the_reader() {
    let notes = limits_failing("bump-before-write");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("wire.md 4.5"), "{notes:?}");
}

// ---------------------------------------------------------------------------
// What a mount's environment says of its tree (`attach::against_tree`).

/// The committed suite against `test-adapter --misbehave <how>`: the
/// scenarios that failed, each with its error, every failure having one
/// that names `cite`.
fn environment_failures(how: &str, cite: &str) -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let (code, entries, _, all) =
        run_json(&format!("test-adapter --misbehave {how}"), &dir, "20", &[]);
    assert_eq!(code, Some(1), "{all}");
    let failed: Vec<&Entry> = entries.iter().filter(|e| !e.1).collect();
    for e in &failed {
        let error = e.2.as_deref().unwrap_or("");
        assert!(error.contains("the mount's environment"), "{e:?}");
        assert!(error.contains(cite), "{e:?}");
        assert!(e.3.is_empty(), "only the environment fails: {e:?}");
    }
    failed.iter().map(|e| e.0.clone()).collect()
}

/// wire.md 5.3: `VFS_REGISTRY` is set while a registry layer is attached. A
/// director that leaves it out fails every scenario whose tree has a
/// registry, and no other.
#[test]
fn a_mount_without_vfs_registry_for_a_registry_fails_those_scenarios() {
    let failed = environment_failures("no-registry-env", "VFS_REGISTRY");
    assert!(
        failed.contains(&"registry-values".to_string()),
        "{failed:?}"
    );
    assert!(
        failed.contains(&"multi-registry-gen".to_string()),
        "{failed:?}"
    );
    for name in &failed {
        assert!(name.contains("registry"), "{failed:?}");
    }
    assert!(!failed.contains(&"registry-none".to_string()), "{failed:?}");
}

/// wire.md 3.2 and 5.3: every root served is declared. A director that
/// leaves `VFS_VIRTUAL_ROOTS` out fails the scenarios with a root besides 0.
#[test]
fn a_mount_not_declaring_its_roots_fails_the_scenarios_with_several() {
    let failed = environment_failures("undeclared-roots", "wire.md 3.2, 5.3");
    assert!(failed.contains(&"roots-isolated".to_string()), "{failed:?}");
    assert!(!failed.contains(&"heartbeat".to_string()), "{failed:?}");
}

/// wire.md 5.1: a shim.cfg with a byte after its table fails every mount.
#[test]
fn a_shim_cfg_that_does_not_decode_fails_every_mount() {
    let failed = environment_failures("bad-shim-cfg", "wire.md 5.1");
    assert!(failed.contains(&"heartbeat".to_string()), "{failed:?}");
}

/// wire.md 5.2: a launch sets or removes every TRANSPORT name. A director
/// that passes on its own environment's fails every mount.
#[test]
fn a_mount_passing_on_a_stale_session_fails_every_mount() {
    let failed = environment_failures("inherit-env", "wire.md 5.2");
    assert!(failed.contains(&"heartbeat".to_string()), "{failed:?}");
}

// ---------------------------------------------------------------------------
// Rules of single replies beyond equality (`compare.rs`, `run.rs`).

/// The committed suite against `test-adapter --misbehave <how>`: the
/// failed scenarios' names and every failing step's note.
fn failing_notes(how: &str) -> (Vec<String>, Vec<String>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let (code, entries, _, all) =
        run_json(&format!("test-adapter --misbehave {how}"), &dir, "20", &[]);
    assert_eq!(code, Some(1), "{all}");
    let failed: Vec<&Entry> = entries.iter().filter(|e| !e.1).collect();
    (
        failed.iter().map(|e| e.0.clone()).collect(),
        failed.iter().flat_map(|e| e.3.clone()).collect(),
    )
}

/// wire.md 3.4.3: the shim keeps `fh` 0 for a directory it made with
/// MKDIR. A director that numbers handles from 0 fails at the OPEN that
/// issues 0.
#[test]
fn a_director_issuing_fh_0_fails() {
    let (failed, notes) = failing_notes("fh-from-zero");
    assert!(failed.contains(&"open-read".to_string()), "{failed:?}");
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("fh 0") && n.contains("wire.md 3.4.3")),
        "{notes:?}"
    );
}

/// wire.md 3.2: `mtime` is seconds. A director reporting milliseconds fails
/// every GETATTR and READDIR that finds something, citing 3.2.
#[test]
fn a_director_reporting_mtime_in_milliseconds_fails() {
    let (failed, notes) = failing_notes("mtime-millis");
    assert!(failed.contains(&"getattr-files".to_string()), "{failed:?}");
    assert!(
        failed.contains(&"readdir-listing".to_string()),
        "{failed:?}"
    );
    for n in &notes {
        assert!(n.contains("wire.md 3.2, Timestamps"), "{n}");
    }
}

/// wire.md 3.4.3: an OPEN reply's other flag bits and bytes 18-19 are zero.
#[test]
fn a_director_setting_reserved_open_reply_bits_fails() {
    let (failed, notes) = failing_notes("open-flag-bits");
    assert!(failed.contains(&"open-read".to_string()), "{failed:?}");
    assert!(
        notes
            .iter()
            .any(|n| n.contains("only bit 0 (immutable) may be set")),
        "{notes:?}"
    );
}

/// wire.md 3.4.14: a director that stages answers the real path of a copy
/// of the file. One that does passes; one whose copy has other bytes fails
/// `stage-exec`, and only that.
#[cfg(windows)]
#[test]
fn a_staged_copy_must_hold_the_served_bytes() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/scenarios");
    let (code, entries, _, all) = run_json("test-adapter --misbehave stage-copy", &dir, "20", &[]);
    assert_eq!(code, Some(0), "{all}");
    let stage = entry(&entries, "stage-exec");
    assert!(stage.1, "{stage:?}");
    let (failed, notes) = failing_notes("stage-wrong-bytes");
    assert_eq!(failed, ["stage-exec"], "{notes:?}");
    assert!(
        notes.iter().any(|n| n.contains("wire.md 3.4.14")),
        "{notes:?}"
    );
}

/// wire.md 3.4.4: a handle serves READ whatever access it was opened with.
/// A director refusing READ through a write-only handle fails
/// `write-only-read`, and only that.
#[test]
fn a_director_refusing_reads_through_write_only_handles_fails() {
    let (failed, _) = failing_notes("write-only-unreadable");
    assert_eq!(failed, ["write-only-read"]);
}

/// wire.md 3.4.5, 3.4.6: what a file grows by reads as zeros.
#[test]
fn a_director_filling_growth_with_garbage_fails() {
    let (failed, _) = failing_notes("growth-garbage");
    assert!(failed.contains(&"write-growth".to_string()), "{failed:?}");
}

/// wire.md 3.2: a stream the director does not hold is not its base file.
#[test]
fn a_director_serving_a_stream_as_its_base_file_fails() {
    let (failed, _) = failing_notes("stream-base");
    assert_eq!(failed, ["names-stream"]);
}

/// wire.md 4.2: the current user's hives are keys like any other.
#[test]
fn a_director_refusing_the_user_hives_fails() {
    let (failed, _) = failing_notes("machine-only-registry");
    assert_eq!(failed, ["registry-current-user"]);
}
