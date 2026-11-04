//! `aether-conform`: plays conformance scenarios against a director under
//! test. See `docs/conformance.md`.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use aether_conform::adapters::{reference_adapter, test_adapter, Misbehave, RingKind};
use aether_conform::report;
use aether_conform::run::{run, RunError, RunOptions};

const USAGE: &str = "\
usage:
  aether-conform run --director \"<command line>\" [--scenarios DIR]
                     [--report tap|json] [--out FILE] [--timeout SECS]
  aether-conform reference-adapter [--ring file|section]

run       starts the director's adapter (the command line) and plays every
          scenario in DIR (default contract/scenarios) against it. The report
          is TAP (default) or JSON, on stdout or in FILE. Every adapter answer
          and ring request has SECS seconds (default 30). Exit code 0 when
          every scenario passes, 1 when one fails, 2 on a usage or adapter
          error.
reference-adapter
          an adapter serving each scenario's tree from the reference
          director, over a ring file (default) or, on Windows, a named
          section.";

fn usage(msg: &str) -> ExitCode {
    eprintln!("aether-conform: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        return usage("no command");
    };
    match cmd.as_str() {
        "run" => cmd_run(rest),
        "reference-adapter" => match flags(rest, &["--ring"]) {
            Ok(f) => match f.first().map(|(_, v)| v.as_str()) {
                None | Some("file") => code(reference_adapter(RingKind::File)),
                Some("section") => code(reference_adapter(RingKind::Section)),
                Some(other) => usage(&format!("unknown ring {other:?}: file or section")),
            },
            Err(e) => usage(&e),
        },
        // Adapters that misbehave on purpose, for the tests.
        "test-adapter" => match flags(rest, &["--misbehave"]) {
            Ok(f) => match f.first().map(|(_, v)| v.as_str()) {
                Some("exit") => code(test_adapter(Misbehave::Exit)),
                Some("garbage") => code(test_adapter(Misbehave::Garbage)),
                Some("hang") => code(test_adapter(Misbehave::Hang)),
                Some("hang-grandchild") => code(test_adapter(Misbehave::HangGrandchild)),
                Some("refuse") => code(test_adapter(Misbehave::Refuse)),
                _ => usage("--misbehave exit|garbage|hang|hang-grandchild|refuse"),
            },
            Err(e) => usage(&e),
        },
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => usage(&format!("unknown command {other:?}")),
    }
}

fn code(n: i32) -> ExitCode {
    ExitCode::from(u8::try_from(n).unwrap_or(2))
}

/// `--name value` pairs, each name one of `known`.
fn flags(args: &[String], known: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(name) = it.next() {
        if !known.contains(&name.as_str()) {
            return Err(format!("unknown option {name:?}"));
        }
        let Some(value) = it.next() else {
            return Err(format!("{name} needs a value"));
        };
        out.push((name.clone(), value.clone()));
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Tap,
    Json,
}

fn cmd_run(args: &[String]) -> ExitCode {
    let f = match flags(
        args,
        &[
            "--director",
            "--scenarios",
            "--report",
            "--out",
            "--timeout",
        ],
    ) {
        Ok(f) => f,
        Err(e) => return usage(&e),
    };
    let get = |k: &str| f.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let Some(director) = get("--director") else {
        return usage("run needs --director \"<command line>\"");
    };
    let format = match get("--report").as_deref() {
        None | Some("tap") => Format::Tap,
        Some("json") => Format::Json,
        Some(other) => return usage(&format!("unknown report format {other:?}: tap or json")),
    };
    let timeout = match get("--timeout") {
        None => Duration::from_secs(30),
        Some(t) => match t.parse::<f64>() {
            Ok(s) if s > 0.0 && s.is_finite() => Duration::from_secs_f64(s),
            _ => {
                return usage(&format!(
                    "--timeout {t:?} is not a positive number of seconds"
                ))
            }
        },
    };
    let opts = RunOptions {
        director,
        scenarios: get("--scenarios")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("contract/scenarios")),
        timeout,
    };
    let planned = aether_conform::run::scenario_files(&opts.scenarios)
        .map(|f| f.len())
        .unwrap_or(0);
    let result = match run(&opts) {
        Ok(r) => r,
        Err(RunError::Scenarios(e)) => {
            eprintln!("aether-conform: {e}");
            return ExitCode::from(2);
        }
        Err(RunError::Adapter(e)) => {
            eprintln!("aether-conform: {e}");
            return ExitCode::from(2);
        }
    };
    let text = match format {
        Format::Tap => report::tap(&result, planned),
        Format::Json => {
            let mut s = serde_json::to_string_pretty(&report::json(&result)).expect("a JSON value");
            s.push('\n');
            s
        }
    };
    match get("--out") {
        Some(out) => {
            if let Err(e) = std::fs::write(&out, text) {
                eprintln!("aether-conform: cannot write {out}: {e}");
                return ExitCode::from(2);
            }
        }
        None => print!("{text}"),
    }
    if let Some(e) = &result.adapter_error {
        eprintln!("aether-conform: {e}");
        return ExitCode::from(2);
    }
    if result.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
