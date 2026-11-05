//! `aether-conform soak`: the generated runs of `run`, for as long as there
//! is time for, with a fresh seed for each batch.
//!
//! CI plays 1024 generated cases a seed, which finds the failures that
//! most sequences show. A rare one needs far more. A soak plays batches
//! until its budget (a time, or a number of generated cases) is spent: each
//! batch is one `run` against a director of its own (a new adapter process),
//! of generated cases and, unless told not to, rounds of clients at once,
//! from a seed drawn from the soak's first by a fixed mixing, so `--seed`
//! replays the whole soak. The first batch also plays what `run` plays
//! without `--generate`: the scenarios, the malformed requests and the
//! limits.
//!
//! A failure ends its batch, not the soak. Each is kept once: failures are
//! the same when they are of one kind and class and, for a generated case
//! or round, their shrunk scenarios are the same tree and steps (the name
//! and description carry the seed, so they differ). A repeat only counts.
//! Each distinct failure is written to `<failures>/<id>/` as its scenario
//! (`<id>.json`, which `run --scenarios` replays) and `replay.txt`: the seed
//! that found it, and the commands that replay it.
//!
//! The summary ([`Soak::summary`]) is JSON: what ran, the seeds, each
//! failure, the wall time. A soak whose batches make no progress (the
//! adapter exits at once, say) stops after [`STALLED`] of them rather than
//! spin until its budget.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::generate::GenerateOptions;
use crate::run::{run, RunError, RunOptions, RunReport, ScenarioResult};
use crate::scenario::{self, to_json_string, Scenario};

/// Batches in a row that played no case and no round, after which the soak
/// stops.
pub const STALLED: u32 = 10;

/// Generated cases in a batch, unless `--batch-cases` says.
pub const BATCH_CASES: u32 = 128;
/// Clients at once in a batch's rounds, unless `--concurrent` says.
pub const CLIENTS: u32 = 4;
/// Rounds in a batch, unless `--concurrent-cases` says: enough for every
/// slot of a ring to carry a request, which a run checks.
pub const ROUNDS: u32 = 64;

/// How long a soak runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Budget {
    /// Until a batch ends past this time.
    For(Duration),
    /// Until this many generated cases have been played.
    Cases(u64),
}

/// What a soak does.
#[derive(Debug, Clone)]
pub struct SoakOptions {
    pub director: String,
    pub scenarios: PathBuf,
    pub timeout: Duration,
    pub budget: Budget,
    /// The first batch's seed; the others come from it ([`batch_seed`]).
    pub seed: u64,
    /// The most operations in a generated case.
    pub steps: usize,
    pub batch_cases: u32,
    /// Clients at once and rounds a batch, or none.
    pub concurrent: Option<(u32, u32)>,
    /// Whether the first batch plays the scenarios, the malformed requests
    /// and the limits.
    pub fixed: bool,
    /// Where each distinct failure is written.
    pub failures: PathBuf,
}

/// A duration as `--for` writes it: whole numbers with units, `90s`, `10m`,
/// `2h`, or several together (`1h30m`); a bare number is seconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let bad = || format!("{text:?} is not a time: write 90s, 10m, 2h or 1h30m");
    let mut total = 0u64;
    let mut digits = String::new();
    let mut any = false;
    for c in text.trim().chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            _ => return Err(bad()),
        };
        let n: u64 = digits.parse().map_err(|_| bad())?;
        total = total.saturating_add(n.saturating_mul(unit));
        digits.clear();
        any = true;
    }
    if !digits.is_empty() {
        // A trailing bare number is seconds, and only when it stands alone.
        if any {
            return Err(bad());
        }
        total = digits.parse().map_err(|_| bad())?;
    } else if !any {
        return Err(bad());
    }
    Ok(Duration::from_secs(total))
}

/// The seed of batch `n` of a soak that began at `first`: splitmix64 of
/// `first + n`, so that seeds differ in every bit from one batch to the next
/// and batch 0 is `first` itself.
pub fn batch_seed(first: u64, n: u32) -> u64 {
    if n == 0 {
        return first;
    }
    let mut z = first.wrapping_add(u64::from(n).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One distinct failure.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// The folder's name: the shrunk scenario's (`generated-<seed>-<case>`),
    /// or the kind and class for a failure with no scenario.
    pub id: String,
    /// `generated`, `concurrent`, `suite` (a scenario, the malformed
    /// requests or the limits), `adapter` or `generator`.
    pub kind: String,
    /// How it failed: `mismatch at getattr`, `error: mount refused`, ...
    pub class: String,
    pub seed: u64,
    pub batch: u32,
    /// The case or round of the seed that failed, for the generated ones.
    pub case: Option<u32>,
    /// Times it was found, this first one included.
    pub occurrences: u32,
    /// The scenario file written, for the ones that have one.
    pub file: Option<PathBuf>,
    /// One command that replays it.
    pub replay: String,
    /// What the run said of it.
    pub message: String,
}

/// The failures found so far, each once.
#[derive(Debug, Default)]
pub struct Failures {
    /// Key to the position in `list`.
    seen: BTreeMap<String, usize>,
    pub list: Vec<Failure>,
    /// Times any failure was found, repeats included.
    pub total: u64,
}

impl Failures {
    /// Records a failure of `kind` and `class` whose shrunk scenario is
    /// `scenario`, if it has one. Whether it is new: a failure found again
    /// (same kind, class, and tree and steps) only counts, in `failure`.
    pub fn add(&mut self, failure: Failure, scenario: Option<&Scenario>) -> bool {
        self.total += 1;
        let key = format!(
            "{}|{}|{}",
            failure.kind,
            failure.class,
            scenario.map(canonical).unwrap_or_default()
        );
        match self.seen.get(&key) {
            Some(&at) => {
                self.list[at].occurrences += 1;
                false
            }
            None => {
                self.seen.insert(key, self.list.len());
                self.list.push(failure);
                true
            }
        }
    }
}

/// `s` as JSON without what names it (its name and description hold the
/// seed it came from), so that scenarios that differ only there are equal.
fn canonical(s: &Scenario) -> String {
    let mut v = scenario::to_json(s);
    if let Some(o) = v.as_object_mut() {
        o.remove("name");
        o.remove("description");
    }
    v.to_string()
}

/// How the first failing step of `r` failed, as generation classes
/// failures: a mismatch at an opcode, or an error outside the steps by its
/// kind (the text before its first `:`).
pub fn class_of(r: &ScenarioResult) -> String {
    if let Some(st) = r.steps.iter().find(|st| !st.passed) {
        return format!("mismatch at {}", st.request["op"].as_str().unwrap_or("?"));
    }
    match r.error.as_deref() {
        Some(e) => format!("error: {}", e.split(':').next().unwrap_or(e).trim()),
        None => "failed".into(),
    }
}

/// The number after the last `-` of the failing round's or case's scenario
/// name (`generated-5-17` is case 17), the scenario's own, with `-cK`
/// (a client's) left off a round's.
fn case_of(name: &str) -> Option<u32> {
    let name = name.rsplit_once("-c").map_or(name, |(a, b)| {
        if b.chars().all(|c| c.is_ascii_digit()) {
            a
        } else {
            name
        }
    });
    name.rsplit('-').next()?.parse().ok()
}

/// The state of a soak.
pub struct Soak {
    opts: SoakOptions,
    started: Instant,
    pub failures: Failures,
    seeds: Vec<u64>,
    cases: u64,
    rounds: u64,
    suite: u64,
    warnings: Vec<String>,
    stopped: Option<&'static str>,
}

impl Soak {
    pub fn new(opts: SoakOptions) -> Soak {
        Soak {
            opts,
            started: Instant::now(),
            failures: Failures::default(),
            seeds: Vec::new(),
            cases: 0,
            rounds: 0,
            suite: 0,
            warnings: Vec::new(),
            stopped: None,
        }
    }

    fn done(&self) -> bool {
        match self.opts.budget {
            Budget::For(t) => self.started.elapsed() >= t,
            Budget::Cases(n) => self.cases >= n,
        }
    }

    /// Plays batches until the budget is spent, or batches stall. `Err`
    /// only for what is wrong with the options (no scenarios to play).
    pub fn run(&mut self) -> Result<(), String> {
        let mut stalled = 0u32;
        let mut batch = 0u32;
        loop {
            let seed = batch_seed(self.opts.seed, batch);
            let progress = self.batch(batch, seed)?;
            batch += 1;
            stalled = if progress { 0 } else { stalled + 1 };
            if stalled >= STALLED {
                self.stopped = Some("no progress");
                eprintln!(
                    "aether-conform: soak: {STALLED} batches in a row played nothing; stopping"
                );
                return Ok(());
            }
            if self.done() {
                return Ok(());
            }
        }
    }

    /// Plays batch `n` from `seed`; whether it played a case or a round.
    fn batch(&mut self, n: u32, seed: u64) -> Result<bool, String> {
        self.seeds.push(seed);
        let first = n == 0;
        let o = &self.opts;
        let cases = match o.budget {
            Budget::Cases(total) => u64::from(o.batch_cases).min(total - self.cases.min(total)),
            Budget::For(_) => u64::from(o.batch_cases),
        } as u32;
        let work =
            std::env::temp_dir().join(format!("aether-soak-{}-{n}-{seed:x}", std::process::id()));
        let play = RunOptions {
            director: o.director.clone(),
            scenarios: (first && o.fixed).then(|| o.scenarios.clone()),
            timeout: o.timeout,
            malformed: first && o.fixed,
            limits: first && o.fixed,
            generate: Some(GenerateOptions {
                cases,
                seed,
                steps: o.steps,
                failures: work.clone(),
            }),
            concurrent: o.concurrent.map(|(clients, rounds)| {
                crate::concurrent::ConcurrentOptions {
                    clients,
                    cases: rounds,
                    seed,
                    steps: o.steps,
                    failures: work.clone(),
                }
            }),
        };
        let (cases_planned, rounds_planned) = (cases, o.concurrent.map_or(0, |(_, r)| r));
        let before = self.failures.total;
        let outcome = run(&play);
        let mut played = (0u64, 0u64);
        match outcome {
            Ok(report) => played = self.record(n, seed, &report, cases_planned, rounds_planned),
            Err(RunError::Scenarios(e)) => {
                let _ = std::fs::remove_dir_all(&work);
                return Err(e);
            }
            Err(RunError::Adapter(e)) => self.adapter_failure(n, seed, &e.to_string()),
            Err(RunError::Generator(e)) => self.generator_failure(n, seed, &e),
        }
        let _ = std::fs::remove_dir_all(&work);
        self.cases += played.0;
        self.rounds += played.1;
        eprintln!(
            "aether-conform: soak: batch {n} (seed {seed}): {} cases and {} rounds so far, {} \
             distinct failure(s) ({} new in this batch), {:.0} s",
            self.cases,
            self.rounds,
            self.failures.list.len(),
            self.failures.total - before,
            self.started.elapsed().as_secs_f64()
        );
        Ok(played.0 + played.1 > 0)
    }

    /// Takes the failures of a batch's report; the cases and rounds it
    /// played.
    fn record(
        &mut self,
        batch: u32,
        seed: u64,
        report: &RunReport,
        cases: u32,
        rounds: u32,
    ) -> (u64, u64) {
        let (mut played_cases, mut played_rounds) = (0u64, 0u64);
        self.suite += report
            .scenarios
            .iter()
            .filter(|s| s.name != "generated" && s.name != "concurrent")
            .count() as u64;
        for r in &report.scenarios {
            let case = case_of_result(r);
            match (r.name.as_str(), r.passed) {
                ("generated", true) => played_cases += u64::from(cases),
                ("generated", false) => played_cases += u64::from(case.map_or(cases, |c| c + 1)),
                ("concurrent", true) => played_rounds += u64::from(rounds),
                ("concurrent", false) => played_rounds += u64::from(case.map_or(rounds, |c| c + 1)),
                _ => {}
            }
            if r.passed {
                continue;
            }
            let message = r.error.clone().unwrap_or_default();
            if r.name == "concurrent" && message.contains("never carried a request") {
                // Too few rounds for every slot to be used: not the
                // director's failure.
                self.warnings.push(format!("batch {batch}: {message}"));
                continue;
            }
            self.scenario_failure(batch, seed, r, case, message);
        }
        if let Some(e) = &report.adapter_error {
            self.adapter_failure(batch, seed, e);
        }
        (played_cases, played_rounds)
    }

    fn scenario_failure(
        &mut self,
        batch: u32,
        seed: u64,
        r: &ScenarioResult,
        case: Option<u32>,
        message: String,
    ) {
        let class = class_of(r);
        let generated = r.name == "generated" || r.name == "concurrent";
        let shrunk = if generated && r.file.extension().is_some_and(|x| x == "json") {
            scenario::load(&r.file).ok()
        } else {
            None
        };
        let director = shell_quote(&self.opts.director);
        let dir = self.opts.failures.clone();
        let id = match &shrunk {
            Some(s) => s.name.clone(),
            None => format!("{}-{}", slug(&r.name), slug(&class)),
        };
        let folder = dir.join(&id);
        let (replay, by_seed) = match (&shrunk, r.name.as_str()) {
            (Some(_), name) => {
                let by_seed = if name == "generated" {
                    format!(
                        "aether-conform run --director {director} --generate --cases {} --seed {seed} --steps {}",
                        case.map_or(1, |c| c + 1),
                        self.opts.steps
                    )
                } else {
                    let (clients, _) = self.opts.concurrent.unwrap_or((CLIENTS, ROUNDS));
                    format!(
                        "aether-conform run --director {director} --concurrent {clients} --concurrent-cases {} --seed {seed} --steps {}",
                        case.map_or(1, |c| c + 1),
                        self.opts.steps
                    )
                };
                (
                    format!(
                        "aether-conform run --director {director} --scenarios {}",
                        folder.display()
                    ),
                    Some(by_seed),
                )
            }
            (None, "malformed") => (
                format!(
                    "aether-conform run --director {director} --scenarios {} --malformed",
                    self.opts.scenarios.display()
                ),
                None,
            ),
            (None, "limits") => (
                format!(
                    "aether-conform run --director {director} --scenarios {} --limits",
                    self.opts.scenarios.display()
                ),
                None,
            ),
            (None, _) => (
                format!(
                    "aether-conform run --director {director} --scenarios {}",
                    self.opts.scenarios.display()
                ),
                None,
            ),
        };
        let kind = if generated { r.name.as_str() } else { "suite" };
        let failure = Failure {
            id: id.clone(),
            kind: kind.into(),
            class: if generated {
                class
            } else {
                format!("{}: {class}", r.name)
            },
            seed,
            batch,
            case,
            occurrences: 1,
            file: shrunk.as_ref().map(|_| folder.join(format!("{id}.json"))),
            replay: replay.clone(),
            message: message.clone(),
        };
        let fresh = self.failures.add(failure.clone(), shrunk.as_ref());
        if !fresh {
            return;
        }
        if let Err(e) = write_failure(&folder, &id, shrunk.as_ref(), &failure, by_seed.as_deref()) {
            self.warnings
                .push(format!("cannot write {}: {e}", folder.display()));
        }
    }

    /// An adapter that could not start, or stopped answering.
    fn adapter_failure(&mut self, batch: u32, seed: u64, message: &str) {
        // The first line names the kind of trouble; what varies (a path, a
        // pid) comes after, so the failure is the same each time.
        let class = format!(
            "error: {}",
            message.split(':').next().unwrap_or(message).trim()
        );
        self.other_failure("adapter", class, batch, seed, message);
    }

    /// The reference answered a generated request otherwise than wire.md
    /// says: the generator has no expectation to give it.
    fn generator_failure(&mut self, batch: u32, seed: u64, message: &str) {
        let class = "error: the generator".to_string();
        self.other_failure("generator", class, batch, seed, message);
    }

    fn other_failure(&mut self, kind: &str, class: String, batch: u32, seed: u64, message: &str) {
        let id = format!("{kind}-{}", slug(&class));
        let folder = self.opts.failures.join(&id);
        let replay = format!(
            "aether-conform run --director {} --generate --cases {} --seed {seed} --steps {}",
            shell_quote(&self.opts.director),
            self.opts.batch_cases,
            self.opts.steps
        );
        let failure = Failure {
            id: id.clone(),
            kind: kind.into(),
            class,
            seed,
            batch,
            case: None,
            occurrences: 1,
            file: None,
            replay,
            message: message.to_string(),
        };
        let fresh = self.failures.add(failure.clone(), None);
        if fresh {
            if let Err(e) = write_failure(&folder, &id, None, &failure, None) {
                self.warnings
                    .push(format!("cannot write {}: {e}", folder.display()));
            }
        }
    }

    /// The summary, as `--summary` and standard output give it.
    pub fn summary(&self) -> Value {
        let budget = match self.opts.budget {
            Budget::For(t) => json!({"for_seconds": t.as_secs_f64()}),
            Budget::Cases(n) => json!({"cases": n}),
        };
        let failures: Vec<Value> = self
            .failures
            .list
            .iter()
            .map(|f| {
                json!({
                    "id": f.id,
                    "kind": f.kind,
                    "class": f.class,
                    "seed": f.seed,
                    "batch": f.batch,
                    "case": f.case,
                    "occurrences": f.occurrences,
                    "file": f.file.as_ref().map(|p| p.to_string_lossy()),
                    "replay": f.replay,
                    "message": f.message,
                })
            })
            .collect();
        let mut v = json!({
            "director": self.opts.director,
            "seed": self.opts.seed,
            "seeds": self.seeds,
            "batches": self.seeds.len(),
            "budget": budget,
            "cases": self.cases,
            "rounds": self.rounds,
            "suite_scenarios": self.suite,
            "failures_seen": self.failures.total,
            "distinct_failures": self.failures.list.len(),
            "failures": failures,
            "warnings": self.warnings,
            "wall_seconds": self.started.elapsed().as_secs_f64(),
            "passed": self.failures.total == 0,
        });
        if let Some(why) = self.stopped {
            v["stopped"] = json!(why);
        }
        v
    }

    pub fn passed(&self) -> bool {
        self.failures.total == 0
    }
}

/// The case or round a failed result is of, from its scenario's name.
fn case_of_result(r: &ScenarioResult) -> Option<u32> {
    let stem = r.file.file_stem()?.to_str()?;
    if stem.starts_with("generated-") || stem.starts_with("concurrent-") {
        case_of(stem)
    } else {
        None
    }
}

/// `command` as one word of a shell command line: in double quotes, or in
/// single quotes when it holds double ones (a quoted program path).
fn shell_quote(command: &str) -> String {
    if command.contains('"') {
        format!("'{command}'")
    } else {
        format!("\"{command}\"")
    }
}

/// `text` as a folder name: letters and digits, the rest a dash.
fn slug(text: &str) -> String {
    let s: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if s.is_empty() {
        "failure".into()
    } else {
        s
    }
}

/// Writes `<folder>/<id>.json` (the scenario, if there is one) and
/// `<folder>/replay.txt`.
fn write_failure(
    folder: &Path,
    id: &str,
    scenario: Option<&Scenario>,
    f: &Failure,
    by_seed: Option<&str>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(folder)?;
    if let Some(s) = scenario {
        std::fs::write(folder.join(format!("{id}.json")), to_json_string(s))?;
    }
    let mut text = format!(
        "kind: {}\nclass: {}\nseed: {} (batch {}{})\n",
        f.kind,
        f.class,
        f.seed,
        f.batch,
        f.case.map_or(String::new(), |c| format!(", case {c}"))
    );
    if !f.message.is_empty() {
        text.push_str(&format!("what the run said: {}\n", f.message));
    }
    text.push_str("replay it:\n");
    text.push_str(&format!("  {}\n", f.replay));
    if let Some(cmd) = by_seed {
        text.push_str("or the run that found it, from its seed:\n");
        text.push_str(&format!("  {cmd}\n"));
    }
    std::fs::write(folder.join("replay.txt"), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_whole_numbers_with_units() {
        let secs = |t: &str| parse_duration(t).map(|d| d.as_secs());
        assert_eq!(secs("90s"), Ok(90));
        assert_eq!(secs("10m"), Ok(600));
        assert_eq!(secs("2h"), Ok(7200));
        assert_eq!(secs("1h30m"), Ok(5400));
        assert_eq!(secs("45"), Ok(45));
        for bad in ["", "m", "1.5h", "soon", "3m5", "-1m", "1d"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
        assert!(parse_duration("soon").unwrap_err().contains("soon"));
    }

    #[test]
    fn batch_seeds_start_at_the_first_and_never_repeat() {
        assert_eq!(batch_seed(42, 0), 42);
        let seeds: std::collections::BTreeSet<u64> =
            (0..10_000).map(|n| batch_seed(42, n)).collect();
        assert_eq!(seeds.len(), 10_000);
        assert_eq!(batch_seed(42, 7), batch_seed(42, 7));
        assert_ne!(batch_seed(42, 7), batch_seed(43, 7));
    }

    fn failure(kind: &str, class: &str, seed: u64) -> Failure {
        Failure {
            id: format!("generated-{seed}-0"),
            kind: kind.into(),
            class: class.into(),
            seed,
            batch: 0,
            case: Some(0),
            occurrences: 1,
            file: None,
            replay: String::new(),
            message: String::new(),
        }
    }

    fn scenario(name: &str, path: &str) -> Scenario {
        use crate::scenario::{Expect, OwnedRequest, Step};
        use aether_protocol::RootId;
        Scenario {
            name: name.into(),
            description: format!("from {name}"),
            tree: crate::tree::Tree {
                roots: vec![crate::tree::Root::default()],
                registry: None,
            },
            steps: vec![Step {
                request: OwnedRequest::GetAttr {
                    root: RootId(0),
                    path: path.into(),
                },
                expect: Expect::Status(aether_protocol::Status::NotFound),
                cite: None,
                note: None,
                client: 0,
                literal_fh: false,
                choice: None,
            }],
        }
    }

    /// The same failure found from another seed (another name, one tree and
    /// steps) is counted, not listed again; another class, or the same class
    /// with other steps, is another failure.
    #[test]
    fn a_failure_found_again_is_counted_once() {
        let mut f = Failures::default();
        let a = scenario("generated-1-3", "a");
        assert!(f.add(failure("generated", "mismatch at getattr", 1), Some(&a)));
        let again = scenario("generated-9-0", "a");
        assert!(!f.add(failure("generated", "mismatch at getattr", 9), Some(&again)));
        assert!(!f.add(
            failure("generated", "mismatch at getattr", 10),
            Some(&again)
        ));
        assert_eq!(f.list.len(), 1);
        assert_eq!(f.list[0].occurrences, 3);
        assert_eq!(f.list[0].seed, 1, "the first finding is kept");
        assert_eq!(f.total, 3);
        let other_steps = scenario("generated-4-0", "b");
        assert!(f.add(
            failure("generated", "mismatch at getattr", 4),
            Some(&other_steps)
        ));
        assert!(f.add(failure("generated", "mismatch at readdir", 5), Some(&again)));
        assert!(f.add(
            failure("concurrent", "mismatch at getattr", 6),
            Some(&again)
        ));
        assert_eq!(f.list.len(), 4);
        assert_eq!(f.total, 6);
        // Failures with no scenario are one by kind and class.
        assert!(f.add(failure("adapter", "error: exited", 1), None));
        assert!(!f.add(failure("adapter", "error: exited", 2), None));
        assert!(f.add(failure("adapter", "error: attach failed", 3), None));
    }

    #[test]
    fn a_director_is_quoted_for_a_shell() {
        assert_eq!(
            shell_quote("x reference-adapter"),
            "\"x reference-adapter\""
        );
        assert_eq!(shell_quote("\"a b\" y"), "'\"a b\" y'");
    }

    #[test]
    fn a_case_is_read_from_a_scenario_name() {
        assert_eq!(case_of("generated-5-17"), Some(17));
        assert_eq!(case_of("concurrent-5-3"), Some(3));
        assert_eq!(case_of("concurrent-5-3-c1"), Some(3));
        assert_eq!(case_of("overlay-listing"), None);
    }
}
