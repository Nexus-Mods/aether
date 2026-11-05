//! `aether-conform`: plays conformance scenarios against a director under
//! test. See `docs/conformance.md`.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use aether_conform::adapters::{
    null_adapter, reference_adapter, test_adapter, Break, Misbehave, NullGeometry, RingKind,
};
use aether_conform::bench::{self, BenchOptions};
use aether_conform::concurrent::{self, ConcurrentOptions};
use aether_conform::generate::{self, GenerateOptions};
use aether_conform::report;
use aether_conform::run::{run, RunError, RunOptions};

const USAGE: &str = "\
usage:
  aether-conform run --director \"<command line>\" [--scenarios DIR]
                     [--malformed] [--limits] [--generate [--cases N] [--seed S]
                     [--steps M] [--failures DIR]]
                     [--concurrent N [--concurrent-cases C]]
                     [--report tap|json] [--out FILE] [--timeout SECS]
  aether-conform soak --director \"<command line>\" (--for TIME | --cases N)
                      [--scenarios DIR] [--seed S] [--steps M] [--batch-cases N]
                      [--concurrent N [--concurrent-cases C] | --no-concurrent]
                      [--generated-only] [--failures DIR] [--summary FILE]
                      [--timeout SECS]
  aether-conform reference-adapter [--ring file|section] [--no-arena]
  aether-conform bench --director \"<command line>\" [--workloads DIR]
                       [--only NAME[,NAME...]] [--floor \"<command line>\"]
                       [--clients N] [--quick] [--out FILE] [--baseline FILE
                       [--fail-over PCT]] [--timeout SECS]
  aether-conform null-adapter [--ring file|section] [--slots N]
                       [--payload-cap N] [--arena-len N] [--workers N]

run       starts the director's adapter (the command line) and plays every
          scenario in DIR (default contract/scenarios) against it. The report
          is TAP (default) or JSON, on stdout or in FILE. Every adapter answer
          and ring request has SECS seconds (default 30). Exit code 0 when
          every scenario passes, 1 when one fails, 2 on a usage or adapter
          error.
          --malformed also sends requests that are not valid requests (short,
          trailing bytes, bad UTF-8, unknown opcodes, odd slot flags, an
          over-long header) and checks the answers wire.md gives them.
          --limits also plays requests sized from the director's ring: READs
          of the shim's own sizes, a WRITE of payload_cap, READDIR at the cap,
          READs of one handle in flight together, an abandoned slot, and a
          registry writer against a reader.
          --generate also plays N generated cases (default 64) of at most M
          operations (default 32) from seed S (default: from the clock;
          printed either way), each compared with the reference; the first
          failure is shrunk and written to the failures DIR (default a
          folder in the temporary directory) as a scenario file.
          --concurrent also plays C rounds (default 64) of N clients at once,
          each a generated case under a directory of its own, their requests
          in flight together across every slot of the ring; a failing round
          is written to the failures DIR. --seed and --steps apply too.
soak      plays generated cases and rounds of clients at once in batches,
          each from a fresh seed (drawn from S, default: from the clock,
          printed), against a new adapter process each, until TIME has
          passed (90s, 10m, 2h, 1h30m: the batch that ends past it is the
          last) or N generated cases have been played. A batch plays N
          cases (--batch-cases, default 128) and C rounds (default 64) of
          clients; the first batch also plays the scenarios, the malformed
          requests and the limits, unless --generated-only. A failure ends
          its batch, not the soak: each distinct one is shrunk and written
          to DIR/<id>/ (default a folder in the temporary directory) as a
          scenario file with replay.txt, the seed and the commands that
          replay it. A failure found again is counted, not written again.
          The JSON summary (cases, rounds, seeds, failures, wall time) is
          printed, and written to FILE if given. Exit code 0 when nothing
          failed, 1 when something did, 2 on a usage error.
reference-adapter
          an adapter serving each scenario's tree from the reference
          director, over a ring file (default) or, on Windows, a named
          section. --no-arena gives the ring no bulk arena, so every READ
          is answered inline.
bench     times the director on every workload in DIR (default
          contract/workloads), or the ones --only names: a warm-up pass,
          then each phase with threads x N clients (default 1), latency per
          opcode, requests per second and bulk bandwidth. Then the same
          requests against the floor (default: null-adapter on a ring of the
          director's kind and shape), and per opcode the director's p50,
          p99 and p99.9 less the floor's. Prints a summary and writes the
          JSON report to FILE. --quick plays one repeat of at most 64
          requests per thread: counts, not timings. --baseline prints the
          change against a report from the same machine; only --fail-over
          PCT turns a p50 rise above PCT percent into exit code 3. Exit code
          0 when every workload ran, 1 when one failed (its replies were not
          what the phase expects), 2 on a usage or adapter error. SECS
          (default 120) bounds every adapter answer and request.
null-adapter
          an adapter answering every request OK with the smallest reply,
          whatever tree a mount names: the ring's own cost. Its ring has
          N slots (8), a payload cap of N bytes (1048576), an arena of N
          bytes (8 MiB; 0 for none) and N serving threads (4).";

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
        "bench" => cmd_bench(rest),
        "soak" => cmd_soak(rest),
        "null-adapter" => cmd_null_adapter(rest),
        // `--break` is test-only (`adapters::Break`) and not in the usage.
        "reference-adapter" => match switches(rest, &["--ring", "--break"], &["--no-arena"]) {
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
                let arena = get("--no-arena").is_none();
                code(reference_adapter(kind, arena, broken))
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
                Some(b) => match Break::parse(b) {
                    Some(b) => code(test_adapter(Misbehave::Director(b))),
                    None => usage(&format!(
                        "unknown --misbehave {b:?}: exit, garbage, hang, hang-grandchild, \
                         refuse, or a reference-adapter --break rule"
                    )),
                },
                None => usage("test-adapter needs --misbehave"),
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
        &["--malformed", "--limits", "--generate"],
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
    if timeout > Duration::from_secs(60) {
        // wire.md 2.5: the shim gives up on a request after
        // RESPONSE_DEADLINE, 60 s.
        eprintln!(
            "aether-conform: warning: --timeout {} s is longer than the shim waits for an \
             answer (60 s, wire.md 2.5, RESPONSE_DEADLINE): a director this run accepts may \
             still be one the shim gives up on",
            timeout.as_secs_f64()
        );
    }
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
        scenarios: Some(
            get("--scenarios")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("contract/scenarios")),
        ),
        timeout,
        malformed: has("--malformed"),
        limits: has("--limits"),
        generate,
        concurrent,
    };
    let planned = opts
        .scenarios
        .as_deref()
        .and_then(|d| aether_conform::run::scenario_files(d).ok())
        .map_or(0, |f| f.len())
        + usize::from(opts.malformed)
        + usize::from(opts.limits)
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

fn cmd_soak(args: &[String]) -> ExitCode {
    use aether_conform::soak::{self, Budget, Soak, SoakOptions};
    let f = match switches(
        args,
        &[
            "--director",
            "--for",
            "--cases",
            "--scenarios",
            "--seed",
            "--steps",
            "--batch-cases",
            "--concurrent",
            "--concurrent-cases",
            "--failures",
            "--summary",
            "--timeout",
        ],
        &["--generated-only", "--no-concurrent"],
    ) {
        Ok(f) => f,
        Err(e) => return usage(&e),
    };
    let get = |k: &str| f.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let has = |k: &str| f.iter().any(|(n, _)| n == k);
    let Some(director) = get("--director") else {
        return usage("soak needs --director \"<command line>\"");
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
    let budget = match (get("--for"), get("--cases")) {
        (Some(t), None) => match soak::parse_duration(&t) {
            Ok(d) if d > Duration::ZERO => Budget::For(d),
            Ok(_) => return usage("--for needs a time above zero"),
            Err(e) => return usage(&format!("--for {e}")),
        },
        (None, Some(_)) => match number("--cases", 0) {
            Ok(n) if n > 0 => Budget::Cases(n),
            Ok(_) => return usage("--cases needs a number above zero"),
            Err(e) => return usage(&e),
        },
        _ => return usage("soak needs one budget: --for TIME or --cases N"),
    };
    let timeout = match get("--timeout").map(|t| t.parse::<f64>()) {
        None => Duration::from_secs(30),
        Some(Ok(s)) if s > 0.0 && s.is_finite() => Duration::from_secs_f64(s),
        Some(_) => return usage("--timeout needs a positive number of seconds"),
    };
    let parsed = (
        number("--seed", clock_seed()),
        number("--steps", generate::DEFAULT_STEPS as u64),
        count("--batch-cases", soak::BATCH_CASES),
        count("--concurrent", soak::CLIENTS),
        count("--concurrent-cases", soak::ROUNDS),
    );
    let (seed, steps, batch_cases, clients, rounds) = match parsed {
        (Ok(a), Ok(b), Ok(c), Ok(d), Ok(e)) => (a, b, c, d, e),
        (Err(e), ..)
        | (_, Err(e), ..)
        | (_, _, Err(e), ..)
        | (_, _, _, Err(e), _)
        | (_, _, _, _, Err(e)) => return usage(&e),
    };
    if batch_cases == 0 {
        return usage("--batch-cases needs a number above zero");
    }
    if has("--no-concurrent") && (has("--concurrent") || has("--concurrent-cases")) {
        return usage("--no-concurrent goes with neither --concurrent nor --concurrent-cases");
    }
    if clients == 0 || rounds == 0 {
        return usage("--concurrent and --concurrent-cases need numbers above zero");
    }
    let failures = get("--failures")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("aether-soak-failures-{seed}")));
    eprintln!("aether-conform: soak from seed {seed} (replay the whole soak with --seed {seed})");
    let mut run = Soak::new(SoakOptions {
        director,
        scenarios: get("--scenarios")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("contract/scenarios")),
        timeout,
        budget,
        seed,
        steps: steps.max(1) as usize,
        batch_cases,
        concurrent: (!has("--no-concurrent")).then_some((clients, rounds)),
        fixed: !has("--generated-only"),
        failures,
    });
    if let Err(e) = run.run() {
        eprintln!("aether-conform: {e}");
        return ExitCode::from(2);
    }
    let mut text = serde_json::to_string_pretty(&run.summary()).expect("a summary is JSON");
    text.push('\n');
    let mut written = true;
    if let Some(out) = get("--summary") {
        if let Some(dir) = std::path::Path::new(&out)
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
        {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&out, &text) {
            eprintln!("aether-conform: cannot write {out}: {e}");
            written = false;
        }
    }
    print!("{text}");
    if run.stalled() && run.passed() {
        eprintln!("aether-conform: the soak's batches played nothing, and it stopped");
    }
    ExitCode::from(aether_conform::soak::exit_code(
        run.passed(),
        run.stalled(),
        written,
    ))
}

fn ring_kind(v: Option<&str>) -> Result<RingKind, String> {
    match v {
        None | Some("file") => Ok(RingKind::File),
        Some("section") => Ok(RingKind::Section),
        Some(other) => Err(format!("unknown ring {other:?}: file or section")),
    }
}

fn cmd_null_adapter(args: &[String]) -> ExitCode {
    let f = match flags(
        args,
        &[
            "--ring",
            "--slots",
            "--payload-cap",
            "--arena-len",
            "--workers",
        ],
    ) {
        Ok(f) => f,
        Err(e) => return usage(&e),
    };
    let get = |k: &str| {
        f.iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    };
    let kind = match ring_kind(get("--ring")) {
        Ok(k) => k,
        Err(e) => return usage(&e),
    };
    let mut g = NullGeometry::default();
    for (k, v) in &f {
        let n = match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) if k == "--ring" => continue,
            Err(_) => return usage(&format!("{k} {v:?} is not a whole number")),
        };
        let small = |n: u64| u32::try_from(n).map_err(|_| format!("{k} {n} is too large"));
        let set = match k.as_str() {
            "--slots" => small(n).map(|n| g.slots = n),
            "--payload-cap" => small(n).map(|n| g.payload_cap = n),
            "--arena-len" => usize::try_from(n)
                .map(|n| g.arena_len = n)
                .map_err(|_| format!("{k} {n} is too large")),
            "--workers" => small(n).map(|n| g.workers = n),
            _ => Ok(()),
        };
        if let Err(e) = set {
            return usage(&e);
        }
    }
    code(null_adapter(kind, g))
}

fn cmd_bench(args: &[String]) -> ExitCode {
    let f = match switches(
        args,
        &[
            "--director",
            "--workloads",
            "--only",
            "--floor",
            "--clients",
            "--out",
            "--baseline",
            "--fail-over",
            "--timeout",
        ],
        &["--quick"],
    ) {
        Ok(f) => f,
        Err(e) => return usage(&e),
    };
    let get = |k: &str| f.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let Some(director) = get("--director") else {
        return usage("bench needs --director \"<command line>\"");
    };
    let clients = match get("--clients").map(|v| v.parse::<u32>()) {
        None => 1,
        Some(Ok(n)) if (1..=64).contains(&n) => n,
        Some(_) => return usage("--clients needs a number from 1 to 64"),
    };
    let timeout = match get("--timeout").map(|t| t.parse::<f64>()) {
        None => Duration::from_secs(120),
        Some(Ok(s)) if s > 0.0 && s.is_finite() => Duration::from_secs_f64(s),
        Some(_) => return usage("--timeout needs a positive number of seconds"),
    };
    let fail_over = match get("--fail-over").map(|p| p.parse::<f64>()) {
        None => None,
        Some(Ok(p)) if p.is_finite() => Some(p),
        Some(_) => return usage("--fail-over needs a number of percent"),
    };
    let baseline = get("--baseline").map(PathBuf::from);
    if fail_over.is_some() && baseline.is_none() {
        return usage("--fail-over goes with --baseline");
    }
    // Read before the run, so a baseline that is not a report fails at once.
    let base = match &baseline {
        None => None,
        Some(b) => match std::fs::read_to_string(b)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).map_err(|e| e.to_string()))
        {
            Ok(v) => Some(v),
            Err(e) => return usage(&format!("baseline {}: {e}", b.display())),
        },
    };
    let opts = BenchOptions {
        director,
        workloads: get("--workloads")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("contract/workloads")),
        only: get("--only")
            .map(|o| o.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        floor: get("--floor"),
        clients,
        quick: f.iter().any(|(n, _)| n == "--quick"),
        timeout,
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("aether-conform")),
    };
    let report = match bench::run(&opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("aether-conform: {e}");
            return ExitCode::from(2);
        }
    };
    print!("{}", bench::summary(&report));
    if let Some(out) = get("--out").map(PathBuf::from) {
        if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = serde_json::to_string_pretty(&report).expect("a report is JSON");
        if let Err(e) = std::fs::write(&out, text + "\n") {
            eprintln!("aether-conform: cannot write {}: {e}", out.display());
            return ExitCode::from(2);
        }
    }
    let failed = report["workloads"]
        .as_array()
        .is_some_and(|ws| ws.iter().any(|w| w.get("error").is_some()));
    if let Some(base) = &base {
        match bench::compare(&report, base) {
            Ok(c) => {
                println!("against the baseline:");
                for l in &c.lines {
                    println!("  {l}");
                }
                if let Some(pct) = fail_over {
                    if c.worst_rise_pct > pct {
                        eprintln!(
                            "aether-conform: an opcode's p50 rose {:.1}%, over {pct}%",
                            c.worst_rise_pct
                        );
                        return ExitCode::from(3);
                    }
                }
            }
            Err(e) => {
                eprintln!("aether-conform: {e}");
                return ExitCode::from(2);
            }
        }
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
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
