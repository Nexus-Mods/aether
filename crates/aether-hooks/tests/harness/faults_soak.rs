//! A soak of the generated fault properties (`faults_file`,
//! `faults_registry`): the same properties, over and over with fresh seeds
//! for a stretch of time, each failure kept as a seed and its shrunk
//! sequence in a folder. CI runs one seed a property; a failure that needs
//! rare sequences needs many.
//!
//! `soak_generated_faults` is ignored, so only a run that asks for it plays
//! it (`cargo test -p aether-hooks --test harness soak_ -- --ignored
//! --nocapture`). It is set by the environment:
//!
//! - `AETHER_FAULTS_SOAK`: how long, as `90s`, `10m`, `2h` or `1h30m` (a bare
//!   number is seconds). A round that starts before the time is up is played
//!   whole. Unset: one round.
//! - `AETHER_FAULTS_SEED`: the first round's seed (a `u64`); each later round
//!   has one drawn from it. Unset: from the clock, printed. The same seed
//!   and `AETHER_FAULTS_ROUND_CASES` play the same rounds.
//! - `AETHER_FAULTS_ROUND_CASES`: cases of each property in a round (default
//!   256).
//! - `AETHER_FAULTS_FAILURES`: the folder a failure is written to, as
//!   `<property>-<seed>.txt` (default `target/faults-failures`).
//!
//! A round plays each property in a process of its own (the shim and the
//! director are one per process, kept from case to case, so many rounds in
//! one process would run out of the fresh names a case uses). A property
//! that fails writes its file and the soak goes on; the test fails at the
//! end if any file was written. A file's first three lines are the property,
//! the seed and the command that replays the round, then the shortest
//! sequence that still fails and everything the process wrote.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The properties a round plays, as libtest names them.
const PROPERTIES: [&str; 2] = [
    "faults_file::generated_faults_never_crash_or_hang_a_file_call",
    "faults_registry::generated_faults_never_crash_or_hang_a_registry_call",
];

/// Cases of each property in a round, when `AETHER_FAULTS_ROUND_CASES` is
/// not set.
const ROUND_CASES: u32 = 256;

/// What one process of one property did.
pub(crate) struct Outcome {
    pub(crate) passed: bool,
    pub(crate) output: String,
}

/// What a soak did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) rounds: u32,
    /// The files written, one for each failed property of a round.
    pub(crate) failures: Vec<PathBuf>,
}

/// A time as `AETHER_FAULTS_SOAK` writes it.
pub(crate) fn parse_duration(text: &str) -> Option<Duration> {
    let (mut total, mut digits, mut any) = (0u64, String::new(), false);
    for c in text.trim().chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            _ => return None,
        };
        total += digits.parse::<u64>().ok()? * unit;
        digits.clear();
        any = true;
    }
    match (digits.is_empty(), any) {
        (true, true) => Some(Duration::from_secs(total)),
        (false, false) => Some(Duration::from_secs(digits.parse().ok()?)),
        _ => None,
    }
}

/// The seed of round `n` from the first: splitmix64 of `first + n`.
pub(crate) fn round_seed(first: u64, n: u32) -> u64 {
    if n == 0 {
        return first;
    }
    let mut z = first.wrapping_add(u64::from(n).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The shortest failing sequence a property's panic message prints.
pub(crate) fn shortest_sequence(output: &str) -> Option<&str> {
    let at = output.find("the shortest failing sequence:")?;
    let rest = &output[at..];
    let end = rest.find("\nnote: run with").unwrap_or(rest.len());
    Some(rest[..end].trim_end())
}

/// The command that replays `property` for `seed` and `cases`.
fn replay(property: &str, seed: u64, cases: u32) -> String {
    format!(
        "$env:AETHER_FAULTS_SEED={seed}; $env:PROPTEST_CASES={cases}; cargo test -p aether-hooks \
         --test harness {property} -- --exact --nocapture"
    )
}

/// Plays rounds until `budget` has passed (at least one), each property of
/// each with its seed and `cases` cases through `play`, and writes a file in
/// `dir` for each that fails.
pub(crate) fn soak(
    budget: Duration,
    first_seed: u64,
    cases: u32,
    dir: &Path,
    mut play: impl FnMut(&str, u64, u32) -> Outcome,
) -> Report {
    let started = Instant::now();
    let mut report = Report {
        rounds: 0,
        failures: Vec::new(),
    };
    loop {
        let seed = round_seed(first_seed, report.rounds);
        for property in PROPERTIES {
            let outcome = play(property, seed, cases);
            eprintln!(
                "faults soak: round {} seed {seed} {property}: {}",
                report.rounds,
                if outcome.passed { "ok" } else { "FAILED" }
            );
            if outcome.passed {
                continue;
            }
            std::fs::create_dir_all(dir).expect("the failures folder");
            let name = property.rsplit("::").next().unwrap_or(property);
            let file = dir.join(format!("{name}-{seed}.txt"));
            let sequence = shortest_sequence(&outcome.output).unwrap_or("(none printed)");
            let text = format!(
                "{property}\nseed {seed}\n{}\n\n{sequence}\n\nwhat the process wrote:\n{}\n",
                replay(property, seed, cases),
                outcome.output
            );
            std::fs::write(&file, text).expect("a failure file");
            report.failures.push(file);
        }
        report.rounds += 1;
        if started.elapsed() >= budget {
            return report;
        }
    }
}

/// The soak `AETHER_FAULTS_SOAK` and the other variables ask for.
#[test]
#[ignore = "a soak: cargo test -p aether-hooks --test harness soak_ -- --ignored"]
fn soak_generated_faults() {
    let var = |k: &str| std::env::var(k).ok();
    let budget = match var("AETHER_FAULTS_SOAK") {
        None => Duration::ZERO,
        Some(t) => parse_duration(&t).unwrap_or_else(|| {
            panic!("AETHER_FAULTS_SOAK {t:?} is not a time: write 90s, 10m, 2h or 1h30m")
        }),
    };
    let seed = var("AETHER_FAULTS_SEED")
        .map(|v| v.parse::<u64>().expect("AETHER_FAULTS_SEED is a u64"))
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos() as u64)
        });
    let cases = var("AETHER_FAULTS_ROUND_CASES")
        .map(|v| {
            v.parse::<u32>()
                .expect("AETHER_FAULTS_ROUND_CASES is a number")
        })
        .unwrap_or(ROUND_CASES);
    let dir = PathBuf::from(
        var("AETHER_FAULTS_FAILURES").unwrap_or_else(|| "target/faults-failures".into()),
    );
    eprintln!(
        "faults soak: {budget:?}, first seed {seed} (AETHER_FAULTS_SEED), {cases} cases a \
         property and round, failures in {}",
        dir.display()
    );
    let report = soak(budget, seed, cases, &dir, |property, seed, cases| {
        let (s, c) = (seed.to_string(), cases.to_string());
        let child = crate::common::run_child(
            property,
            &[("AETHER_FAULTS_SEED", &s), ("PROPTEST_CASES", &c)],
        );
        Outcome {
            passed: child.passed,
            output: child.output,
        }
    });
    eprintln!(
        "faults soak: {} round(s), {} failure(s)",
        report.rounds,
        report.failures.len()
    );
    assert!(
        report.failures.is_empty(),
        "failures written: {:#?}",
        report.failures
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passed() -> Outcome {
        Outcome {
            passed: true,
            output: String::new(),
        }
    }

    #[test]
    fn times_are_whole_numbers_with_units() {
        let secs = |t: &str| parse_duration(t).map(|d| d.as_secs());
        assert_eq!(secs("90s"), Some(90));
        assert_eq!(secs("10m"), Some(600));
        assert_eq!(secs("2h"), Some(7200));
        assert_eq!(secs("1h30m"), Some(5400));
        assert_eq!(secs("45"), Some(45));
        for bad in ["", "m", "1.5h", "soon", "3m5", "1d"] {
            assert_eq!(secs(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn round_seeds_start_at_the_first_and_differ() {
        assert_eq!(round_seed(9, 0), 9);
        let all: std::collections::BTreeSet<u64> = (0..1000).map(|n| round_seed(9, n)).collect();
        assert_eq!(all.len(), 1000);
    }

    #[test]
    fn the_shortest_sequence_is_cut_from_a_panic_message() {
        let out = "noise\nfile calls (seed 7): boom\nthe shortest failing sequence:\n[\n    Step,\n]\nnote: run with `RUST_BACKTRACE=1`";
        assert_eq!(
            shortest_sequence(out),
            Some("the shortest failing sequence:\n[\n    Step,\n]")
        );
        assert_eq!(shortest_sequence("all fine"), None);
    }

    /// Rounds go on until the time is up, each with a seed of its own; a
    /// property that fails leaves a file with its seed, the replay command
    /// and the sequence, and the rounds after it are still played.
    #[test]
    fn a_failing_property_is_written_with_its_seed_and_the_soak_goes_on() {
        let dir = aether_testkit::tempdir().unwrap();
        let mut seen = Vec::new();
        let report = soak(
            Duration::from_millis(30),
            11,
            64,
            &dir.path().join("out"),
            |property, seed, cases| {
                seen.push((property.to_string(), seed, cases));
                std::thread::sleep(Duration::from_millis(10));
                if seen.len() == 2 {
                    Outcome {
                        passed: false,
                        output: "the shortest failing sequence:\n[Step]\nnote: run with x".into(),
                    }
                } else {
                    passed()
                }
            },
        );
        assert!(report.rounds >= 2, "{report:?}");
        assert_eq!(seen.len() as u32, report.rounds * 2);
        assert!(seen.iter().all(|(_, _, cases)| *cases == 64));
        let seeds: std::collections::BTreeSet<u64> = seen.iter().map(|s| s.1).collect();
        assert_eq!(seeds.len() as u32, report.rounds, "a seed a round");
        assert_eq!(seen[0].1, 11);
        assert_eq!(report.failures.len(), 1);
        let text = std::fs::read_to_string(&report.failures[0]).unwrap();
        let mut lines = text.lines();
        assert!(lines.next().unwrap().starts_with("faults_registry::"));
        assert_eq!(lines.next().unwrap(), format!("seed {}", seen[1].1));
        assert!(lines
            .next()
            .unwrap()
            .contains(&format!("AETHER_FAULTS_SEED={}", seen[1].1)));
        assert!(text.contains("the shortest failing sequence:\n[Step]"));
    }

    /// One round is played whatever the budget, and passing rounds leave no
    /// folder.
    #[test]
    fn a_soak_plays_one_round_at_least_and_a_clean_one_writes_nothing() {
        let dir = aether_testkit::tempdir().unwrap();
        let out = dir.path().join("out");
        let report = soak(Duration::ZERO, 1, 8, &out, |_, _, _| passed());
        assert_eq!(
            report,
            Report {
                rounds: 1,
                failures: Vec::new()
            }
        );
        assert!(!out.exists());
    }
}
