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
    let o = conform(&["help"]);
    assert!(
        !stdout(&o).contains("--break"),
        "test-only, not in the usage"
    );
}
