//! `aether-conform`: plays conformance scenarios against a director under
//! test. See `docs/conformance.md`.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use aether_conform::adapters::{reference_adapter, test_adapter, Break, Misbehave, RingKind};
use aether_conform::concurrent::{self, ConcurrentOptions};
use aether_conform::generate::{self, GenerateOptions};
use aether_conform::report;
use aether_conform::run::{run, RunError, RunOptions};

const USAGE: &str = "\
usage:
  aether-conform run --director \"<command line>\" [--scenarios DIR]
                     [--malformed] [--generate [--cases N] [--seed S]
                     [--steps M] [--failures DIR]]
                     [--concurrent N [--concurrent-cases C]]
                     [--report tap|json] [--out FILE] [--timeout SECS]
  aether-conform reference-adapter [--ring file|section]

run       starts the director's adapter (the command line) and plays every
          scenario in DIR (default contract/scenarios) against it. The report
          is TAP (default) or JSON, on stdout or in FILE. Every adapter answer
          and ring request has SECS seconds (default 30). Exit code 0 when
          every scenario passes, 1 when one fails, 2 on a usage or adapter
          error.
          --malformed also sends requests that are not valid requests (short,
          trailing bytes, bad UTF-8, unknown opcodes, odd slot flags, an
          over-long header) and checks the answers wire.md gives them.
          --generate also plays N generated cases (default 64) of at most M
          operations (default 32) from seed S (default: from the clock;
          printed either way), each compared with the reference; the first
          failure is shrunk and written to the failures DIR (default a
          folder in the temporary directory) as a scenario file.
          --concurrent also plays C rounds (default 64) of N clients at once,
          each a generated case under a directory of its own, their requests
          in flight together across every slot of the ring; a failing round
          is written to the failures DIR. --seed and --steps apply too.
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
        // `--break` is test-only (`adapters::Break`) and not in the usage.
        "reference-adapter" => match flags(rest, &["--ring", "--break"]) {
            Ok(f) => {
                let get = |k: &str| {
                    f.iter()
                        .rev()
                        .find(|(n, _)| n == k)
                        .map(|(_, v)| v.as_str())
                };
                let kind = match get("--ring") {
                    None | Some("file") => RingKind::File,
                    Some("section") => RingKind::Section,
                    Some(other) => {
                        return usage(&format!("unknown ring {other:?}: file or section"))
                    }
                };
                let broken = match get("--break") {
                    None => None,
                    Some(b) => match Break::parse(b) {
                        Some(b) => Some(b),
                        None => return usage(&format!("unknown --break {b:?}")),
                    },
                };
                code(reference_adapter(kind, broken))
            }
            Err(e) => usage(&e),
        },
        // A client of the ring the environment names, driven by a run
        // (`aether_conform::clients`); not for people, so not in the usage.
        "client-worker" => match flags(rest, &["--client", "--timeout"]) {
            Ok(f) => {
                let get = |k: &str| f.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v);
                let client = get("--client").and_then(|v| v.parse::<u32>().ok());
                let timeout = get("--timeout")
                    .and_then(|v| v.parse::<f64>().ok())
                    .filter(|s| *s > 0.0 && s.is_finite())
                    .map(Duration::from_secs_f64)
                    .unwrap_or(Duration::from_secs(30));
                match client {
                    Some(k) => code(aether_conform::clients::client_worker(k, timeout)),
                    None => usage("client-worker needs --client N"),
                }
            }
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
                // The reference breaking a rule only checks across steps or
                // across clients find (`adapters::Break`).
                Some(
                    b @ ("stale-immutable" | "frozen-reg-gen" | "swap-replies" | "per-slot-fh"
                    | "slot-zero-only" | "bank-zero"),
                ) => match Break::parse(b) {
                    Some(b) => code(test_adapter(Misbehave::Director(b))),
                    None => usage(&format!("unknown --misbehave {b:?}")),
                },
                _ => usage(
                    "--misbehave exit|garbage|hang|hang-grandchild|refuse|stale-immutable|\
                     frozen-reg-gen|swap-replies|per-slot-fh|slot-zero-only|bank-zero",
                ),
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
    switches(args, known, &[])
}

/// [`flags`], where the names in `bare` take no value (and are given the
/// value `""`).
fn switches(
    args: &[String],
    known: &[&str],
    bare: &[&str],
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(name) = it.next() {
        if bare.contains(&name.as_str()) {
            out.push((name.clone(), String::new()));
            continue;
        }
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
    let f = match switches(
        args,
        &[
            "--director",
            "--scenarios",
            "--report",
            "--out",
            "--timeout",
            "--cases",
            "--seed",
            "--steps",
            "--failures",
            "--concurrent",
            "--concurrent-cases",
        ],
        &["--malformed", "--generate"],
    ) {
        Ok(f) => f,
        Err(e) => return usage(&e),
    };
    let get = |k: &str| f.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let has = |k: &str| f.iter().any(|(n, _)| n == k);
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
    let number = |k: &str, default: u64| -> Result<u64, String> {
        match get(k) {
            None => Ok(default),
            Some(v) => v
                .parse::<u64>()
                .map_err(|_| format!("{k} {v:?} is not a whole number")),
        }
    };
    let count = |k: &str, default: u32| -> Result<u32, String> {
        number(k, u64::from(default))
            .and_then(|n| u32::try_from(n).map_err(|_| format!("{k} {n} is too many")))
    };
    let generating = has("--generate");
    let concurrent = match get("--concurrent").map(|_| count("--concurrent", 0)) {
        None => None,
        Some(Ok(n)) if n >= 1 => Some(n),
        Some(Ok(_)) => return usage("--concurrent needs at least 1 client"),
        Some(Err(e)) => return usage(&e),
    };
    if !generating && has("--cases") {
        return usage("--cases goes with --generate");
    }
    if concurrent.is_none() && has("--concurrent-cases") {
        return usage("--concurrent-cases goes with --concurrent");
    }
    if !generating && concurrent.is_none() {
        for k in ["--seed", "--steps", "--failures"] {
            if has(k) {
                return usage(&format!("{k} goes with --generate or --concurrent"));
            }
        }
    }
    let (seed, steps) = match (
        number("--seed", clock_seed()),
        number("--steps", generate::DEFAULT_STEPS as u64),
    ) {
        (Ok(a), Ok(b)) => (a, b.max(1) as usize),
        (Err(e), _) | (_, Err(e)) => return usage(&e),
    };
    let failures = get("--failures")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("aether-conform-failures-{seed}")));
    let generate = if generating {
        let cases = match count("--cases", generate::DEFAULT_CASES) {
            Ok(n) => n,
            Err(e) => return usage(&e),
        };
        eprintln!(
            "aether-conform: generating {cases} cases from seed {seed} (replay with --seed {seed})"
        );
        Some(GenerateOptions {
            cases,
            seed,
            steps,
            failures: failures.clone(),
        })
    } else {
        None
    };
    let concurrent = match concurrent {
        None => None,
        Some(clients) => {
            let cases = match count("--concurrent-cases", concurrent::DEFAULT_CASES) {
                Ok(n) => n,
                Err(e) => return usage(&e),
            };
            eprintln!(
                "aether-conform: {clients} clients at once, {cases} cases each, from seed \
                 {seed} (replay with --seed {seed})"
            );
            Some(ConcurrentOptions {
                clients,
                cases,
                seed,
                steps,
                failures,
            })
        }
    };
    let opts = RunOptions {
        director,
        scenarios: get("--scenarios")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("contract/scenarios")),
        timeout,
        malformed: has("--malformed"),
        generate,
        concurrent,
    };
    let planned = aether_conform::run::scenario_files(&opts.scenarios)
        .map(|f| f.len())
        .unwrap_or(0)
        + usize::from(opts.malformed)
        + usize::from(opts.generate.is_some())
        + usize::from(opts.concurrent.is_some());
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
        Err(RunError::Generator(e)) => {
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

/// A seed for a run that names none: from the clock and the process, and
/// printed, so the run can be replayed.
fn clock_seed() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^ u64::from(std::process::id()).rotate_left(32)
}
