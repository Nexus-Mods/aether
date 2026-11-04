//! A run's report, as TAP (the default) or JSON.
//!
//! TAP (version 13) has one test point per scenario; a failed one carries a
//! YAML block with the scenario's error, if any, and each failed step's
//! request (in the scenario format, so it can be pasted into a scenario;
//! a malformed request, which that format cannot say, is described in a
//! string), expected and got answers and note. An adapter error ends the
//! run with `Bail out!`, so the plan's count is not reached.
//!
//! JSON is one object:
//!
//! ```json
//! {"scenarios": [{"name": "files", "file": "...", "passed": false, "error": null,
//!                 "steps": [{"index": 0, "request": {"op": "..."}, "expected": "...",
//!                            "got": "...", "passed": false, "note": "..."}]}],
//!  "seed": null,
//!  "adapter_error": null}
//! ```
//!
//! `seed` is the generator's seed for a generated run and `null` for
//! scenarios read from files.

use serde_json::{json, Value};

use crate::run::{RunReport, ScenarioResult};

/// The report as TAP. `planned` is how many scenarios the run had.
pub fn tap(report: &RunReport, planned: usize) -> String {
    let mut out = String::from("TAP version 13\n");
    out.push_str(&format!("1..{planned}\n"));
    if let Some(seed) = report.seed {
        out.push_str(&format!("# seed {seed}\n"));
    }
    for (n, s) in report.scenarios.iter().enumerate() {
        let n = n + 1;
        if s.passed {
            out.push_str(&format!("ok {n} - {}\n", s.name));
            continue;
        }
        out.push_str(&format!("not ok {n} - {}\n", s.name));
        out.push_str("  ---\n");
        out.push_str(&format!(
            "  file: {}\n",
            yaml_str(&s.file.to_string_lossy())
        ));
        if let Some(e) = &s.error {
            out.push_str(&format!("  error: {}\n", yaml_str(e)));
        }
        let failed: Vec<_> = s.steps.iter().filter(|st| !st.passed).collect();
        if !failed.is_empty() {
            out.push_str("  failures:\n");
            for st in failed {
                out.push_str(&format!("    - index: {}\n", st.index));
                out.push_str(&format!("      request: {}\n", yaml_value(&st.request)));
                out.push_str(&format!("      expected: {}\n", yaml_str(&st.expected)));
                out.push_str(&format!("      got: {}\n", yaml_str(&st.got)));
                if let Some(note) = &st.note {
                    out.push_str(&format!("      note: {}\n", yaml_str(note)));
                }
            }
        }
        out.push_str("  ...\n");
    }
    if let Some(e) = &report.adapter_error {
        for line in e.lines() {
            out.push_str(&format!("# {line}\n"));
        }
        let first = e.lines().next().unwrap_or("");
        out.push_str(&format!("Bail out! {first}\n"));
    }
    out
}

/// A double-quoted YAML scalar, which a JSON string is.
fn yaml_str(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

/// A JSON value as YAML: a string double-quoted, anything else (a request
/// in the scenario format) as JSON, which YAML reads as flow style.
fn yaml_value(v: &Value) -> String {
    match v {
        Value::String(s) => yaml_str(s),
        v => v.to_string(),
    }
}

/// The report as JSON.
pub fn json(report: &RunReport) -> Value {
    json!({
        "scenarios": report.scenarios.iter().map(scenario).collect::<Vec<_>>(),
        "seed": report.seed,
        "adapter_error": report.adapter_error,
    })
}

fn scenario(s: &ScenarioResult) -> Value {
    json!({
        "name": s.name,
        "file": s.file.to_string_lossy(),
        "passed": s.passed,
        "error": s.error,
        "steps": s.steps.iter().map(|st| json!({
            "index": st.index,
            "request": st.request,
            "expected": st.expected,
            "got": st.got,
            "passed": st.passed,
            "note": st.note,
        })).collect::<Vec<_>>(),
    })
}
