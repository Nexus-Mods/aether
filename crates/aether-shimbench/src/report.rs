//! The report: per case the native and the shim latencies, their
//! difference, and the requests per call; the machine it was measured on;
//! the comparison with a baseline from the same machine.
//!
//! ```text
//! { "machine": { "cpu", "cores", "os", "build": { "profile", "rustc", "git" } },
//!   "director": "reference", "iterations": 100000, "warmup": 10000, "timer_ns": 100,
//!   "geometry": { "name": "production", "slots", "payload_cap", "arena_len",
//!                 "serve_threads" },
//!   "cases": [ { "name": "getattr-in-root",
//!                "native": { "count", "min", "p50", "p90", "p99", "p999", "max", "mean",
//!                            "batch", "calls" },
//!                "shim":   { ... },
//!                "overhead_ns": { "p50", "p90", "p99" },
//!                "round_trips_per_call": 1.0, "ring_bytes_per_call": 32.0 },
//!              { "name": "...", "error": "..." } ] }
//! ```
//!
//! Every latency is nanoseconds per call. A sample is one batch of `batch`
//! calls timed together and divided by `batch` (`count` samples, `calls`
//! calls), a batch lasting at least 100 steps of the timer, whose step is
//! `timer_ns`: so a call shorter than the step is measured, and a quantile
//! is over batch means, not single calls. `overhead_ns` is the shim's value
//! less the native one at the same quantile, so it can be negative where the
//! two are within noise.

use crate::launch::{Counted, DirectorKind, GeometryKind, TargetOutput, SERVE_THREADS};
use crate::{warmup, Case};
use serde_json::{json, Value};

/// One case's result.
pub enum CaseResult {
    Measured {
        native: Box<TargetOutput>,
        shim: Box<TargetOutput>,
        counted: Counted,
    },
    Failed(String),
}

/// The report of a run.
pub fn build(
    director: DirectorKind,
    geometry: GeometryKind,
    iterations: u64,
    results: &[(Case, CaseResult)],
) -> Value {
    let g = geometry.geometry();
    let cases: Vec<Value> = results.iter().map(|(c, r)| case_json(*c, r)).collect();
    let timer_ns = results.iter().find_map(|(_, r)| match r {
        CaseResult::Measured { native, .. } => Some(native.timer_ns),
        CaseResult::Failed(_) => None,
    });
    json!({
        "machine": aether_conform::machine::machine(),
        "director": director.name(),
        "iterations": iterations,
        "warmup": warmup(iterations),
        "timer_ns": timer_ns,
        "geometry": {
            "name": geometry.name(),
            "slots": g.slots,
            "payload_cap": g.payload_cap,
            "arena_len": g.arena_len,
            "serve_threads": SERVE_THREADS,
        },
        "cases": cases,
    })
}

fn case_json(case: Case, r: &CaseResult) -> Value {
    match r {
        CaseResult::Failed(e) => json!({ "name": case.name(), "error": e }),
        CaseResult::Measured {
            native,
            shim,
            counted,
        } => {
            let (n, s) = (native.ns, shim.ns);
            let d = |a: u64, b: u64| a as i64 - b as i64;
            // The recorder counted every call of the shim run's batches.
            let per = |v: u64| v as f64 / shim.calls.max(1) as f64;
            let side = |o: &TargetOutput| {
                let mut v = o.ns.to_json();
                v["batch"] = o.line["batch"].clone();
                v["calls"] = json!(o.calls);
                v
            };
            json!({
                "name": case.name(),
                "native": side(native),
                "shim": side(shim),
                "overhead_ns": {
                    "p50": d(s.p50, n.p50),
                    "p90": d(s.p90, n.p90),
                    "p99": d(s.p99, n.p99),
                },
                "round_trips_per_call": per(counted.requests),
                "ring_bytes_per_call": per(counted.ring_bytes),
            })
        }
    }
}

/// The report as a table, one line per case.
pub fn table(report: &Value) -> String {
    let mut out = format!(
        "director {}, geometry {}, at least {} calls per case after {} untimed, timed in \
         batches of at least 100 timer steps of {} ns; nanoseconds per call\n",
        report["director"].as_str().unwrap_or("?"),
        report["geometry"]["name"].as_str().unwrap_or("?"),
        report["iterations"],
        report["warmup"],
        report["timer_ns"],
    );
    out.push_str(&format!(
        "{:<20} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>8} {:>10}\n",
        "case",
        "native p50",
        "native p99",
        "shim p50",
        "shim p99",
        "+p50",
        "+p99",
        "trips",
        "ring B"
    ));
    for c in report["cases"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().unwrap_or("?");
        if let Some(e) = c.get("error").and_then(Value::as_str) {
            out.push_str(&format!("{name:<20} error: {e}\n"));
            continue;
        }
        let q = |side: &str, k: &str| c[side][k].as_u64().unwrap_or(0);
        out.push_str(&format!(
            "{name:<20} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>8.2} {:>10.1}\n",
            q("native", "p50"),
            q("native", "p99"),
            q("shim", "p50"),
            q("shim", "p99"),
            c["overhead_ns"]["p50"].as_i64().unwrap_or(0),
            c["overhead_ns"]["p99"].as_i64().unwrap_or(0),
            c["round_trips_per_call"].as_f64().unwrap_or(0.0),
            c["ring_bytes_per_call"].as_f64().unwrap_or(0.0),
        ));
    }
    out
}

/// What a comparison with a baseline found.
pub struct Comparison {
    /// One line per value compared.
    pub lines: Vec<String>,
    /// The largest rise of a case's shim p50, in percent.
    pub worst_rise_pct: f64,
}

/// `now` against `base`, which must be from the same machine (CPU, cores
/// and OS; the build may differ), the same director and the same geometry.
pub fn compare(now: &Value, base: &Value) -> Result<Comparison, String> {
    if now["geometry"] != base["geometry"] {
        return Err(format!(
            "the baseline was recorded at another geometry ({} here, {} there)",
            now["geometry"], base["geometry"]
        ));
    }
    aether_conform::machine::same_machine(now, base)?;
    if now["director"] != base["director"] {
        return Err(format!(
            "the baseline served {}, this run {}",
            base["director"], now["director"]
        ));
    }
    let mut lines = Vec::new();
    let mut worst = f64::NEG_INFINITY;
    let empty = Vec::new();
    let base_cases = base["cases"].as_array().unwrap_or(&empty);
    for c in now["cases"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().unwrap_or("?");
        let Some(b) = base_cases.iter().find(|b| b["name"] == c["name"]) else {
            lines.push(format!("{name}: not in the baseline"));
            continue;
        };
        if c.get("error").is_some() || b.get("error").is_some() {
            lines.push(format!("{name}: an error on one side, not compared"));
            continue;
        }
        for k in ["p50", "p99"] {
            let (was, is) = (
                b["shim"][k].as_u64().unwrap_or(0),
                c["shim"][k].as_u64().unwrap_or(0),
            );
            let pct = aether_conform::machine::percent(was as f64, is as f64);
            if k == "p50" {
                worst = worst.max(pct);
            }
            lines.push(format!("{name} shim {k}: {was} -> {is} ns ({pct:+.1}%)"));
        }
        let (was, is) = (
            b["overhead_ns"]["p50"].as_i64().unwrap_or(0),
            c["overhead_ns"]["p50"].as_i64().unwrap_or(0),
        );
        lines.push(format!("{name} overhead p50: {was} -> {is} ns"));
        let (was, is) = (
            b["round_trips_per_call"].as_f64().unwrap_or(0.0),
            c["round_trips_per_call"].as_f64().unwrap_or(0.0),
        );
        if was != is {
            lines.push(format!("{name} round trips per call: {was} -> {is}"));
        }
    }
    Ok(Comparison {
        lines,
        worst_rise_pct: if worst.is_finite() { worst } else { 0.0 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(cpu: &str, p50: u64) -> Value {
        json!({
            "machine": { "cpu": cpu, "cores": 8, "os": "w" },
            "director": "reference",
            "cases": [{
                "name": "getattr-in-root",
                "shim": { "p50": p50, "p99": 2 * p50 },
                "overhead_ns": { "p50": p50 / 2 },
                "round_trips_per_call": 1.0,
            }],
        })
    }

    #[test]
    fn a_baseline_from_another_machine_is_refused() {
        let e = compare(&report("a", 100), &report("b", 100)).err().unwrap();
        assert!(e.contains("another machine"), "{e}");
    }

    #[test]
    fn a_baseline_at_another_geometry_is_refused() {
        let mut base = report("a", 100);
        base["geometry"] = json!({ "name": "shim-tests" });
        let mut now = report("a", 100);
        now["geometry"] = json!({ "name": "production" });
        let e = compare(&now, &base).err().unwrap();
        assert!(e.contains("another geometry"), "{e}");
    }

    #[test]
    fn a_rise_is_reported_in_percent() {
        let c = compare(&report("a", 150), &report("a", 100)).unwrap();
        assert_eq!(c.worst_rise_pct, 50.0);
        assert!(c.lines.iter().any(|l| l.contains("100 -> 150 ns (+50.0%)")));
    }
}
