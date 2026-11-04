//! A run's report, as TAP (the default) or JSON.
//!
//! TAP (version 13) has one test point per scenario, its description the
//! scenario name with `#`, line breaks and `\` escaped; a failed one carries a
//! YAML block with the scenario's error, if any, and each failed step's
//! request (in the scenario format, so it can be pasted into a scenario;
//! a malformed request, which that format cannot say, is described in a
//! string), expected and got answers and note. After the test points, one
//! comment line per opcode the scenario files sent counts the answers the
//! director gave (`# answers open: exists 3, is-dir 0, ok 40`): every
//! answer a step allows is listed, so one no step drew shows as 0. An
//! adapter error ends the run with `Bail out!`, so the plan's count is not
//! reached.
//!
//! JSON is one object:
//!
//! ```json
//! {"scenarios": [{"name": "files", "file": "...", "passed": false, "error": null,
//!                 "steps": [{"index": 0, "request": {"op": "..."}, "expected": "...",
//!                            "got": "...", "passed": false, "note": "..."}]}],
//!  "seed": null,
//!  "answers": {"open": {"exists": 3, "is-dir": 0, "ok": 40}},
//!  "adapter_error": null}
//! ```
//!
//! `seed` is the generator's seed for a generated run and `null` for
//! scenarios read from files. `answers` counts only the scenario files'
//! steps, as the TAP comment lines do.

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
        let name = tap_name(&s.name);
        if s.passed {
            out.push_str(&format!("ok {n} - {name}\n"));
            continue;
        }
        out.push_str(&format!("not ok {n} - {name}\n"));
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
    for (op, counts) in &report.answers {
        let list: Vec<String> = counts.iter().map(|(a, n)| format!("{a} {n}")).collect();
        out.push_str(&format!("# answers {op}: {}\n", list.join(", ")));
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

/// A test point's description: `#` would start a directive (`# SKIP`) and a
/// line break would end the line, so they are escaped as `\#`, `\n` and
/// `\r`, and `\` itself as `\\`.
fn tap_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '#' => out.push_str("\\#"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
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
        "answers": report.answers,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::ScenarioResult;

    fn report(names: &[(&str, bool)]) -> RunReport {
        RunReport {
            scenarios: names
                .iter()
                .map(|(n, passed)| ScenarioResult {
                    name: n.to_string(),
                    file: "f.json".into(),
                    passed: *passed,
                    steps: Vec::new(),
                    error: None,
                })
                .collect(),
            adapter_error: None,
            seed: None,
            answers: Default::default(),
        }
    }

    /// A `#` in a test point's description starts a directive (`# SKIP`,
    /// `# TODO`) and a newline ends the line, so a scenario named with
    /// either must not change what the point says.
    #[test]
    fn a_tap_name_with_a_hash_or_a_newline_is_escaped() {
        let r = report(&[("a # SKIP x", true), ("two\nlines\r\\ok 9", false)]);
        let out = tap(&r, 2);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[2], r"ok 1 - a \# SKIP x", "{out}");
        assert_eq!(lines[3], r"not ok 2 - two\nlines\r\\ok 9", "{out}");
        assert!(!out.lines().any(|l| l.starts_with("lines")), "{out}");
        assert_eq!(
            out.lines().filter(|l| l.contains("ok ")).count(),
            2,
            "{out}"
        );
    }

    /// Every answer a step allowed is listed, a count of 0 included.
    #[test]
    fn the_answers_are_listed_by_opcode_with_their_zeros() {
        let mut r = report(&[("a", true)]);
        r.answers
            .entry("open".into())
            .or_default()
            .extend([("ok".to_string(), 3), ("is-dir".to_string(), 0)]);
        let out = tap(&r, 1);
        assert!(out.contains("# answers open: is-dir 0, ok 3\n"), "{out}");
        assert_eq!(json(&r)["answers"]["open"]["is-dir"], 0);
    }
}
