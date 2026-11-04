//! `aether-conform run --generate` and `--malformed`: generated operation
//! sequences compared with the reference, and requests that are not valid
//! requests at all. The built-in `reference-adapter` is the director, and its
//! hidden test-only `--break` switch flips one rule so that the checks have
//! something to find.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

const EXE: &str = env!("CARGO_BIN_EXE_aether-conform");

fn conform(args: &[&str]) -> Output {
    Command::new(EXE).args(args).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn director(adapter_args: &str) -> String {
    format!("\"{EXE}\" reference-adapter{adapter_args}")
}

/// A folder holding one trivial scenario, so a run's file part passes
/// whatever the adapter breaks and only the mode under test can fail.
fn heartbeat_only(dir: &Path) -> PathBuf {
    let d = dir.join("scenarios");
    std::fs::create_dir_all(&d).unwrap();
    let s = json!({
        "name": "heartbeat",
        "description": "HEARTBEAT",
        "tree": {"roots": [{"id": 0}]},
        "steps": [{"request": {"op": "heartbeat"}, "expect": {"status": "ok"}}]
    });
    std::fs::write(d.join("heartbeat.json"), serde_json::to_vec(&s).unwrap()).unwrap();
    d
}

fn json_report(o: &Output, file: &Path) -> Value {
    let bytes = std::fs::read(file)
        .unwrap_or_else(|e| panic!("no report ({e}): {}\n{}", stdout(o), stderr(o)));
    serde_json::from_slice(&bytes).unwrap()
}

fn scenario<'v>(report: &'v Value, name: &str) -> &'v Value {
    report["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name} in {report}"))
}

fn first_failure(s: &Value) -> Value {
    s["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|st| st["passed"] == false)
        .cloned()
        .unwrap_or_else(|| panic!("no failed step in {s}"))
}

#[test]
fn generated_cases_pass_against_the_reference() {
    let tmp = aether_testkit::tempdir().unwrap();
    let scenarios = heartbeat_only(tmp.path());
    let report = tmp.path().join("report.json");
    let o = conform(&[
        "run",
        "--director",
        &director(""),
        "--scenarios",
        scenarios.to_str().unwrap(),
        "--generate",
        "--cases",
        "64",
        "--seed",
        "1",
        "--failures",
        tmp.path().join("failures").to_str().unwrap(),
        "--report",
        "json",
        "--out",
        report.to_str().unwrap(),
    ]);
    let v = json_report(&o, &report);
    assert_eq!(o.status.code(), Some(0), "{v:#}\n{}", stderr(&o));
    assert_eq!(v["seed"], 1);
    let g = scenario(&v, "generated");
    assert_eq!(g["passed"], true, "{g:#}");
    assert!(stderr(&o).contains("seed 1"), "{}", stderr(&o));
    assert!(
        !tmp.path().join("failures").exists()
            || std::fs::read_dir(tmp.path().join("failures"))
                .unwrap()
                .next()
                .is_none(),
        "a passing run writes no failure"
    );
}

#[test]
fn generation_finds_a_broken_rule_and_its_shrunk_scenario_replays() {
    let tmp = aether_testkit::tempdir().unwrap();
    let scenarios = heartbeat_only(tmp.path());
    let failures = tmp.path().join("failures");
    let report = tmp.path().join("report.json");
    let broken = director(" --break found-for-missing");
    let o = conform(&[
        "run",
        "--director",
        &broken,
        "--scenarios",
        scenarios.to_str().unwrap(),
        "--generate",
        "--cases",
        "64",
        "--seed",
        "7",
        "--failures",
        failures.to_str().unwrap(),
        "--report",
        "json",
        "--out",
        report.to_str().unwrap(),
    ]);
    let v = json_report(&o, &report);
    assert_eq!(o.status.code(), Some(1), "{v:#}\n{}", stderr(&o));
    let g = scenario(&v, "generated");
    assert_eq!(g["passed"], false, "{g:#}");
    let failed = first_failure(g);
    assert!(
        failed["got"].as_str().unwrap().contains("found: true"),
        "{failed:#}"
    );

    // The shrunk case, written as an ordinary scenario.
    let written: Vec<PathBuf> = std::fs::read_dir(&failures)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(written.len(), 1, "{written:?}");
    let file = &written[0];
    assert_eq!(g["file"].as_str().unwrap(), file.to_str().unwrap());
    let s: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let steps = s["steps"].as_array().unwrap();
    assert!(
        steps.len() <= 3,
        "shrunk to a few steps, not {}: {s:#}",
        steps.len()
    );

    // Replaying it in scenario mode fails the same way...
    let replay = tmp.path().join("replay.json");
    let o = conform(&[
        "run",
        "--director",
        &broken,
        "--scenarios",
        failures.to_str().unwrap(),
        "--report",
        "json",
        "--out",
        replay.to_str().unwrap(),
    ]);
    let r = json_report(&o, &replay);
    assert_eq!(o.status.code(), Some(1), "{r:#}");
    let again = first_failure(&r["scenarios"][0]);
    for k in ["index", "request", "expected", "got"] {
        assert_eq!(again[k], failed[k], "{k}: {again:#} vs {failed:#}");
    }

    // ...and passes against the reference itself, so its expectations are
    // the reference's answers.
    let o = conform(&[
        "run",
        "--director",
        &director(""),
        "--scenarios",
        failures.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
}

#[test]
fn the_same_seed_gives_the_same_cases() {
    let a = aether_conform::generate::cases(11, 16, 24).unwrap();
    let b = aether_conform::generate::cases(11, 16, 24).unwrap();
    assert_eq!(a.len(), 16);
    assert_eq!(a, b);
    let c = aether_conform::generate::cases(12, 16, 24).unwrap();
    assert_ne!(a, c, "another seed, other cases");
    // Every case loads back from what is written for it.
    let tmp = aether_testkit::tempdir().unwrap();
    for (i, s) in a.iter().enumerate() {
        let p = tmp.path().join(format!("{i}.json"));
        std::fs::write(&p, aether_conform::scenario::to_json_string(s)).unwrap();
        assert_eq!(&aether_conform::load(&p).unwrap(), s, "case {i}");
    }
}

#[test]
fn malformed_requests_against_the_reference_pass() {
    let tmp = aether_testkit::tempdir().unwrap();
    let scenarios = heartbeat_only(tmp.path());
    let report = tmp.path().join("report.json");
    let o = conform(&[
        "run",
        "--director",
        &director(""),
        "--scenarios",
        scenarios.to_str().unwrap(),
        "--malformed",
        "--report",
        "json",
        "--out",
        report.to_str().unwrap(),
    ]);
    let v = json_report(&o, &report);
    assert_eq!(o.status.code(), Some(0), "{v:#}\n{}", stderr(&o));
    let m = scenario(&v, "malformed");
    assert_eq!(m["passed"], true, "{m:#}");
    let steps = m["steps"].as_array().unwrap();
    assert!(steps.len() > 100, "{}", steps.len());
    // Every rule of the brief is there, each citing its section.
    for rule in [
        "3.3", "3.4.4", "3.4.5", "3.5.1", "3.6", "4.1", "2.5", "UTF-8",
    ] {
        assert!(
            steps
                .iter()
                .any(|st| st["request"].as_str().unwrap().contains(rule)),
            "no step for {rule}"
        );
    }
}

#[cfg(windows)]
#[test]
fn malformed_requests_against_the_reference_pass_on_a_named_section() {
    let tmp = aether_testkit::tempdir().unwrap();
    let scenarios = heartbeat_only(tmp.path());
    let o = conform(&[
        "run",
        "--director",
        &director(" --ring section"),
        "--scenarios",
        scenarios.to_str().unwrap(),
        "--malformed",
        "--generate",
        "--cases",
        "8",
        "--seed",
        "3",
        "--failures",
        tmp.path().join("failures").to_str().unwrap(),
    ]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{}", stderr(&o));
    assert!(out.contains("ok 2 - malformed"), "{out}");
    assert!(out.contains("ok 3 - generated"), "{out}");
}

#[test]
fn malformed_mode_fails_a_director_that_answers_them_otherwise() {
    let tmp = aether_testkit::tempdir().unwrap();
    let scenarios = heartbeat_only(tmp.path());
    let report = tmp.path().join("report.json");
    let o = conform(&[
        "run",
        "--director",
        &director(" --break io-error-for-bad-request"),
        "--scenarios",
        scenarios.to_str().unwrap(),
        "--malformed",
        "--report",
        "json",
        "--out",
        report.to_str().unwrap(),
    ]);
    let v = json_report(&o, &report);
    assert_eq!(o.status.code(), Some(1), "{v:#}");
    let m = scenario(&v, "malformed");
    assert_eq!(m["passed"], false);
    let f = first_failure(m);
    assert!(f["got"].as_str().unwrap().contains("io-error"), "{f:#}");
}

#[test]
fn the_break_switch_is_not_reachable_without_the_flag() {
    // The plain reference adapter passes a generated run (above); an
    // unknown --break value is a usage error, not a silent pass.
    let o = conform(&["reference-adapter", "--break", "everything"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    // `help` prints the usage on stdout, a usage error on stderr: neither
    // names the switch.
    let o = conform(&["help"]);
    assert!(stdout(&o).contains("reference-adapter"), "{}", stdout(&o));
    let e = conform(&["frobnicate"]);
    assert!(stderr(&e).contains("reference-adapter"), "{}", stderr(&e));
    for text in [stdout(&o), stderr(&o), stderr(&e)] {
        assert!(
            !text.contains("--break"),
            "test-only, not in the usage: {text}"
        );
    }
}

/// A generated run of `cases` cases of `seed` against the reference adapter
/// breaking `rule`, beside one HEARTBEAT scenario, so that only the
/// generator can fail it: it fails, its case shrinks to at most `steps`
/// steps whose first failure is at an `op` request, and the shrunk case,
/// replayed against the reference, passes (its expectations are wire.md's).
fn generator_catches(rule: &str, cases: u32, seed: u64, steps: usize, op: &str) {
    let tmp = aether_testkit::tempdir().unwrap();
    let scenarios = heartbeat_only(tmp.path());
    let failures = tmp.path().join("failures");
    let report = tmp.path().join("report.json");
    let o = conform(&[
        "run",
        "--director",
        &director(&format!(" --break {rule}")),
        "--scenarios",
        scenarios.to_str().unwrap(),
        "--generate",
        "--cases",
        &cases.to_string(),
        "--seed",
        &seed.to_string(),
        "--failures",
        failures.to_str().unwrap(),
        "--report",
        "json",
        "--out",
        report.to_str().unwrap(),
    ]);
    let v = json_report(&o, &report);
    assert_eq!(
        o.status.code(),
        Some(1),
        "{rule}: {v:#}
{}",
        stderr(&o)
    );
    let g = scenario(&v, "generated");
    let failed = first_failure(g);
    assert_eq!(failed["request"]["op"], op, "{rule}: {failed:#}");
    let file = g["file"].as_str().unwrap();
    let s: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let n = s["steps"].as_array().unwrap().len();
    assert!(
        n <= steps,
        "{rule}: shrunk to {n} steps, not at most {steps}: {s:#}"
    );
    let o = conform(&[
        "run",
        "--director",
        &director(""),
        "--scenarios",
        failures.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(0), "{rule}: {}", stdout(&o));
}

/// One test per rule the generator now sends requests for, each a defect
/// it finds (`reference-adapter --break`) that no generated case reached
/// before: (test, rule, cases, seed, the op the shrunk case fails at).
macro_rules! caught_by_the_generator {
    ($($name:ident: $rule:literal, $cases:literal, $seed:literal, $op:literal;)*) => {
        $(
            #[test]
            fn $name() {
                generator_catches($rule, $cases, $seed, 8, $op);
            }
        )*
    };
}

caught_by_the_generator! {
    // wire.md 3.4.7, 3.4.8: a handle follows its file through a RENAME, and
    // never reaches a file later at its old name.
    handles_hold_their_file: "handles-by-path", 256, 7, "write";
    // 3.4.3, 3.4.4: an append lands at the end other handles' changes left.
    appends_land_at_the_shared_end: "append-own-end", 256, 7, "getattr";
    // 3.4.5, 3.4.6: a WRITE past the end leaves zeros, cut bytes included.
    a_gap_past_the_end_reads_as_zeros: "truncate-keeps-bytes", 256, 7, "read";
    // 3.4.4: a handle opened without OPEN_READ is read.
    a_write_only_handle_is_read: "write-only-unreadable", 256, 7, "read";
    // 3.4.7: chains on one name (a directory renamed away and made again).
    a_directory_made_again_is_empty: "rename-dir-ghosts", 1024, 2, "readdir";
    // 2.8.1: steps from several clients present each other's handles.
    handles_are_ring_global: "per-slot-fh", 256, 7, "open";
    // 3.4.3, 3.4.9: a create in a missing directory makes nothing.
    a_create_in_a_missing_directory_is_not_found: "create-makes-parents", 256, 7, "open";
    // 3.4.9, 3.5: a create below a file.
    a_create_below_a_file_is_not_found: "file-parent-io-error", 256, 7, "mkdir";
    // 3.4.3: a write OPEN of a directory.
    a_write_open_of_a_directory_is_refused: "dir-write-open", 256, 7, "open";
    // 3.4.2: READDIR of a file.
    a_file_is_not_listed: "readdir-file-lists-itself", 256, 7, "readdir";
    // 3.4.7: a file onto an empty directory, replaced or refused.
    a_file_onto_an_empty_directory_moves_or_not: "file-onto-dir-kept", 1024, 3, "rename";
    // 3.4.7: a RENAME onto an ancestor of `from`.
    a_rename_onto_an_ancestor_is_an_io_error: "ancestor-bad-request", 1024, 1, "rename";
    // 3.4.7: a RENAME to the same name in its stored spelling.
    a_rename_to_itself_is_ok: "same-name-exists", 1024, 2, "rename";
    // 3.4.13: RENAME_X onto a directory of another root.
    a_rename_x_onto_a_directory_is_refused: "rename-x-onto-dir-ok", 1024, 2, "rename-x";
    // 3.4.4: READ offsets far past the end.
    a_read_far_past_the_end_is_empty: "read-offset-wraps", 256, 7, "read";
    // 3.4.5, 3.4.6: sizes no director holds.
    a_size_past_any_file_is_refused: "huge-write-ok", 256, 7, "write";
    // 3.4.12: STORED_NAMES with a `skip` of every component.
    a_skip_of_every_component_names_none: "skip-past-end-echo", 256, 7, "stored-names";
    // 3.2: a create's parents keep their stored spelling.
    a_create_keeps_its_parents_spelling: "parent-respelled", 256, 7, "readdir";
    // 3.2: a named stream is not its base file.
    a_named_stream_is_not_its_file: "stream-base", 256, 7, "getattr";
    // 4.3: REG_DELETE_KEY of a key no layer holds, in every registry case.
    an_unknown_key_is_tombstoned: "delete-unrecorded-not-found", 8, 2, "reg-delete-key";
}

/// A concurrent run of `rounds` rounds of 4 clients against the reference
/// adapter breaking `rule`: it fails in the clients' meeting, and the error
/// says which (wire.md 2.8.1).
fn a_meeting_catches(rule: &str, rounds: u32, what: &str) {
    let tmp = aether_testkit::tempdir().unwrap();
    let scenarios = heartbeat_only(tmp.path());
    let report = tmp.path().join("report.json");
    let o = conform(&[
        "run",
        "--director",
        &director(&format!(" --break {rule}")),
        "--scenarios",
        scenarios.to_str().unwrap(),
        "--concurrent",
        "4",
        "--concurrent-cases",
        &rounds.to_string(),
        "--seed",
        "7",
        "--failures",
        tmp.path().join("failures").to_str().unwrap(),
        "--report",
        "json",
        "--out",
        report.to_str().unwrap(),
    ]);
    let v = json_report(&o, &report);
    assert_eq!(
        o.status.code(),
        Some(1),
        "{rule}: {v:#}
{}",
        stderr(&o)
    );
    let c = scenario(&v, "concurrent");
    let error = c["error"].as_str().unwrap_or("");
    assert!(
        error.contains("meeting") && error.contains(what) && error.contains("wire.md 2.8.1"),
        "{rule}: {c:#}"
    );
}

#[test]
fn concurrent_exclusive_creates_meet_and_one_wins() {
    a_meeting_catches("racy-excl", 4, "OPEN_EXCL creates");
}

#[test]
fn concurrent_key_creates_meet_and_one_wins() {
    a_meeting_catches("racy-reg-create", 4, "REG_CREATE_KEYs");
}

#[test]
fn concurrent_appends_land_whole() {
    a_meeting_catches("torn-append", 4, "appending");
}

#[test]
fn concurrent_disjoint_writes_all_land() {
    a_meeting_catches("lost-update", 4, "disjoint ranges");
}

#[test]
fn a_rename_is_never_seen_half_done() {
    // The break holds the RENAME's middle until another request of the
    // meeting has arrived (`hold_for_another`), so a lookup lands in it
    // whatever the scheduling; a few rounds are a margin, not a hope. The
    // lookup may land while the copy at `from` is still being written, or
    // the listing between the halves: either is a request in flight with the
    // RENAME seeing a mixture.
    a_meeting_catches("rename-in-steps", 8, "in flight with Rename");
}

#[test]
fn a_create_racing_a_kind_change_or_a_rename_lands_whole() {
    a_meeting_catches("racy-create", 8, "a create");
}
