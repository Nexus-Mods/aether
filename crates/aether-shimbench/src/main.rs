//! What each Win32 call costs under the shim, against the same call made
//! natively:
//!
//! ```text
//! aether-shimbench [--director reference|null] [--iterations N] [--cases LIST]
//!                  [--out FILE] [--baseline FILE] [--fail-over PCT] [--timeout SECS]
//! ```
//!
//! Per case (`--cases a,b,...`, default all) it runs `aether-shimbench-target`
//! natively and then under the shim (see the library's docs), prints a table
//! and writes the JSON report to `--out`. `--baseline` prints the change
//! against a report from the same machine; only `--fail-over PCT` makes a
//! rise of a shim p50 above `PCT` percent exit non-zero, and nothing on CI
//! passes it: these are one machine's timings.
//!
//! Needs `aether-injector.exe` and `aether_shim.dll` built in the same cargo
//! profile: `cargo build --release -p aether-inject -p aether-shim` before
//! `cargo run --release -p aether-shimbench`.
//!
//! Exit codes: 0 done, 1 a case failed (natively or under the shim; it has
//! an error in the report instead of numbers), 2 usage or setup, 3 a rise
//! past `--fail-over`.

#[cfg(not(windows))]
fn main() {
    eprintln!("aether-shimbench runs on Windows only");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    std::process::exit(windows::main())
}

#[cfg(windows)]
mod windows {
    use aether_shimbench::launch::{Artifacts, Bench, DirectorKind, ShimOptions};
    use aether_shimbench::report::{self, CaseResult};
    use aether_shimbench::Case;
    use std::path::PathBuf;
    use std::time::Duration;

    struct Args {
        director: DirectorKind,
        iterations: u64,
        cases: Vec<Case>,
        out: Option<PathBuf>,
        baseline: Option<PathBuf>,
        fail_over: Option<f64>,
        timeout: Duration,
    }

    fn usage(why: &str) -> ! {
        let names: Vec<&str> = Case::ALL.iter().map(|c| c.name()).collect();
        eprintln!(
            "aether-shimbench: {why}\nusage: aether-shimbench [--director reference|null] \
             [--iterations N] [--cases LIST] [--out FILE] [--baseline FILE] [--fail-over PCT] \
             [--timeout SECS]\ncases: {}",
            names.join(",")
        );
        std::process::exit(2);
    }

    fn parse() -> Args {
        let mut args = Args {
            director: DirectorKind::Reference,
            iterations: 10_000,
            cases: Case::ALL.to_vec(),
            out: None,
            baseline: None,
            fail_over: None,
            timeout: Duration::from_secs(600),
        };
        let mut a = std::env::args().skip(1);
        while let Some(flag) = a.next() {
            let mut value = || {
                a.next()
                    .unwrap_or_else(|| usage(&format!("{flag} needs a value")))
            };
            match flag.as_str() {
                "--director" => {
                    let v = value();
                    args.director = DirectorKind::parse(&v)
                        .unwrap_or_else(|| usage(&format!("no director {v}")));
                }
                "--iterations" => {
                    args.iterations = value()
                        .parse()
                        .ok()
                        .filter(|&n| n > 0)
                        .unwrap_or_else(|| usage("--iterations takes a count above 0"));
                }
                "--cases" => {
                    args.cases = value()
                        .split(',')
                        .map(|n| {
                            Case::parse(n.trim()).unwrap_or_else(|| usage(&format!("no case {n}")))
                        })
                        .collect();
                }
                "--out" => args.out = Some(value().into()),
                "--baseline" => args.baseline = Some(value().into()),
                "--fail-over" => {
                    args.fail_over = Some(
                        value()
                            .parse()
                            .unwrap_or_else(|_| usage("--fail-over takes a percentage")),
                    );
                }
                "--timeout" => {
                    args.timeout = Duration::from_secs(
                        value()
                            .parse()
                            .unwrap_or_else(|_| usage("--timeout takes seconds")),
                    );
                }
                other => usage(&format!("unknown argument {other}")),
            }
        }
        args
    }

    pub fn main() -> i32 {
        let args = parse();
        let artifacts = Artifacts::locate().unwrap_or_else(|e| usage(&e));
        let bench = Bench::new(artifacts, args.timeout)
            .unwrap_or_else(|e| usage(&format!("the bench's folders: {e}")));
        let opts = ShimOptions {
            director: args.director,
            attach_faults: Vec::new(),
        };
        let mut results = Vec::new();
        for &case in &args.cases {
            eprintln!("aether-shimbench: {}", case.name());
            let r = bench.run_native(case, args.iterations).and_then(|native| {
                let (shim, counted) = bench
                    .run_shim(case, args.iterations, &opts)?
                    .result()
                    .map_err(|e| format!("under the shim: {e}"))?;
                Ok(CaseResult::Measured {
                    native,
                    shim,
                    counted,
                    iterations: args.iterations,
                })
            });
            results.push((case, r.unwrap_or_else(CaseResult::Failed)));
        }
        let report = report::build(args.director, args.iterations, &results);
        print!("{}", report::table(&report));
        if let Some(out) = &args.out {
            if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(dir);
            }
            let text = serde_json::to_string_pretty(&report).expect("a report is JSON");
            if let Err(e) = std::fs::write(out, text + "\n") {
                eprintln!("aether-shimbench: write {}: {e}", out.display());
                return 2;
            }
        }
        let failed = results
            .iter()
            .any(|(_, r)| matches!(r, CaseResult::Failed(_)));
        if let Some(base) = &args.baseline {
            let text = std::fs::read_to_string(base)
                .unwrap_or_else(|e| usage(&format!("read {}: {e}", base.display())));
            let base: serde_json::Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| usage(&format!("{} is not a report: {e}", base.display())));
            match report::compare(&report, &base) {
                Ok(c) => {
                    println!("against the baseline:");
                    for l in &c.lines {
                        println!("  {l}");
                    }
                    if let Some(pct) = args.fail_over {
                        if c.worst_rise_pct > pct {
                            eprintln!(
                                "aether-shimbench: a shim p50 rose {:.1}%, over {pct}%",
                                c.worst_rise_pct
                            );
                            return 3;
                        }
                    }
                }
                Err(e) => usage(&e),
            }
        }
        i32::from(failed)
    }
}
