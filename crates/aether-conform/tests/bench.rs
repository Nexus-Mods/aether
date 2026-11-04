//! `aether-conform bench`: a director timed on workloads over the null
//! adapter's ring floor, driven as a child process. The workloads are tiny
//! ones written here, and every run is `--quick`: these tests check what is
//! counted and reported, never how long anything took.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use aether_conform::adapter::Adapter;
use aether_conform::attach::Attached;
use aether_protocol::{Opcode, ReadReply, Reply, Request, RequestFlags, RootId};
use serde_json::{json, Value};

const EXE: &str = env!("CARGO_BIN_EXE_aether-conform");

fn quoted_exe() -> String {
    format!("\"{EXE}\"")
}

/// Every kind of phase, each short enough for `--quick` to keep whole.
fn every_kind() -> Value {
    json!({
        "name": "every-kind",
        "description": "a storm, reads inline and in bulk, a listing and a save",
        "tree": {"roots": [{"id": 0,
            "files": [{"path": "Data/Big.bsa", "contents": {"size": 3000000, "seed": 1}},
                      {"path": "Data/Small.esp", "contents": {"size": 40000, "seed": 2}},
                      {"path": "Saves/Old.ess", "contents": "old"}],
            "writable": ["Saves"]}]},
        "generate": [{"dir": "Data/Meshes", "count": 3, "name": "Rock{n}.nif", "size": 64, "seed": 9}],
        "phases": [
            {"name": "storm", "threads": 2, "repeat": 2, "kind": "storm",
             "ops": ["getattr", "open", "close", "stored_names"], "paths": {"prefix": "Data/Meshes/"}},
            {"name": "bulk", "threads": 1, "repeat": 1, "kind": "sequential_read",
             "path": "Data/Big.bsa", "chunk": 1048576, "bulk": true},
            {"name": "random", "threads": 2, "repeat": 1, "kind": "random_read",
             "path": "Data/Small.esp", "len": 4096, "count": 8, "seed": 3},
            {"name": "list", "threads": 1, "repeat": 3, "kind": "readdir", "path": "Data/Meshes"},
            {"name": "save", "threads": 1, "repeat": 1, "kind": "write",
             "path": "Saves/Old.ess.tmp", "chunk": 1000, "total": 2500,
             "then_close": true, "rename_to": "Saves/Old.ess"}
        ]
    })
}

/// One bulk read of a 1 MiB file in one 1 MiB READ.
fn one_bulk_read() -> Value {
    json!({
        "name": "one-bulk-read",
        "description": "a file read whole in one bulk READ",
        "tree": {"roots": [{"id": 0,
            "files": [{"path": "Data/Big.bsa", "contents": {"size": 1048576, "seed": 1}}]}]},
        "phases": [{"name": "bulk", "threads": 1, "repeat": 1, "kind": "sequential_read",
                    "path": "Data/Big.bsa", "chunk": 1048576, "bulk": true}]
    })
}

fn workloads(list: &[Value]) -> tempfile::TempDir {
    let dir = aether_testkit::tempdir().unwrap();
    for w in list {
        let p = dir
            .path()
            .join(format!("{}.json", w["name"].as_str().unwrap()));
        std::fs::write(p, serde_json::to_vec_pretty(w).unwrap()).unwrap();
    }
    dir
}

fn bench(dir: &Path, director: &str, extra: &[&str]) -> (Output, Option<Value>) {
    let out = dir.join("report.json");
    let _ = std::fs::remove_file(&out);
    let mut args = vec![
        "bench".to_string(),
        "--director".into(),
        director.to_string(),
        "--workloads".into(),
        dir.to_string_lossy().into_owned(),
        "--quick".into(),
        "--timeout".into(),
        "60".into(),
        "--out".into(),
        out.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    let o = Command::new(EXE).args(&args).output().unwrap();
    let report = std::fs::read(&out)
        .ok()
        .map(|b| serde_json::from_slice(&b).unwrap());
    (o, report)
}

fn text(o: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn reference() -> String {
    format!("{} reference-adapter", quoted_exe())
}

/// A report of the every-kind workload against the reference, made once.
fn every_kind_report() -> (tempfile::TempDir, Value) {
    let dir = workloads(&[every_kind()]);
    let (o, report) = bench(dir.path(), &reference(), &[]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    (dir, report.expect("a report"))
}

fn phase<'r>(report: &'r Value, name: &str) -> &'r Value {
    report["workloads"][0]["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("phase {name} in {report:#}"))
}

#[test]
fn bench_against_the_reference_writes_every_opcode_the_workload_sends() {
    let (_dir, r) = every_kind_report();
    assert!(r["machine"]["cpu"].is_string());
    assert_eq!(r["workloads"][0]["name"], "every-kind");
    let ops = |p: &str| -> Vec<String> {
        phase(&r, p)["per_op"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    assert_eq!(ops("storm"), ["close", "getattr", "open", "stored-names"]);
    assert_eq!(ops("bulk"), ["close", "open", "read"]);
    assert_eq!(ops("random"), ["close", "open", "read"]);
    assert_eq!(ops("list"), ["readdir"]);
    assert_eq!(ops("save"), ["close", "open", "rename", "write"]);
    // --quick: one repeat. Each count is the requests of that opcode the
    // phase's threads sent.
    let count = |p: &str, op: &str| phase(&r, p)["per_op"][op]["count"].as_u64().unwrap();
    assert_eq!(phase(&r, "storm")["threads"], 2);
    assert_eq!(count("storm", "getattr"), 2 * 3);
    assert_eq!(phase(&r, "storm")["requests"], 2 * 3 * 4);
    assert_eq!(count("bulk", "read"), 3);
    assert_eq!(count("random", "read"), 2 * 8);
    assert_eq!(count("list", "readdir"), 1);
    assert_eq!(count("save", "write"), 3);
    // The bulk phase read the whole file in bulk.
    let mib_s = phase(&r, "bulk")["bulk_mib_s"].as_f64().unwrap();
    let secs = phase(&r, "bulk")["seconds"].as_f64().unwrap();
    let bytes = mib_s * secs * 1024.0 * 1024.0;
    assert!((bytes - 3_000_000.0).abs() < 1.0, "{bytes}");
    assert!(phase(&r, "random")["bulk_mib_s"].is_null());
    for p in r["workloads"][0]["phases"].as_array().unwrap() {
        assert!(p["requests_per_s"].as_f64().unwrap() > 0.0);
        for (op, v) in p["per_op"].as_object().unwrap() {
            let (p50, p99, max) = (
                v["p50_us"].as_f64().unwrap(),
                v["p99_us"].as_f64().unwrap(),
                v["max_us"].as_f64().unwrap(),
            );
            assert!(0.0 < p50 && p50 <= p99 && p99 <= max, "{op}: {v}");
        }
    }
}

#[test]
fn the_floor_is_measured_and_subtracted() {
    let (_dir, r) = every_kind_report();
    let w = &r["workloads"][0];
    assert!(
        w["floor"].as_str().unwrap().contains("null-adapter"),
        "{}",
        w["floor"]
    );
    // The floor's ring is the director's: same kind and shape.
    assert_eq!(w["floor_geometry"], w["geometry"]);
    assert_eq!(w["geometry"]["ring"], "file");
    for p in w["phases"].as_array().unwrap() {
        assert!(p["floor"]["requests_per_s"].as_f64().unwrap() > 0.0);
        for (op, v) in p["per_op"].as_object().unwrap() {
            for k in ["p50", "p99", "p999"] {
                let d = v[format!("{k}_us")].as_f64().unwrap();
                let f = v[format!("floor_{k}_us")]
                    .as_f64()
                    .unwrap_or_else(|| panic!("{op} floor_{k}_us: {v}"));
                let over = v[format!("over_floor_{k}_us")].as_f64().unwrap();
                assert!(f > 0.0, "{op}: {v}");
                assert!((d - f - over).abs() < 1e-6, "{op} {k}: {v}");
            }
            assert!(v["floor_max_us"].as_f64().unwrap() > 0.0);
        }
    }
}

#[cfg(windows)]
#[test]
fn the_floor_of_a_section_ring_is_a_section_ring() {
    let dir = workloads(&[one_bulk_read()]);
    let director = format!("{} reference-adapter --ring section", quoted_exe());
    let (o, r) = bench(dir.path(), &director, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let r = r.unwrap();
    let w = &r["workloads"][0];
    assert_eq!(w["geometry"]["ring"], "section");
    assert_eq!(w["floor_geometry"], w["geometry"]);
}

/// The floor's READs move what the director's did, in the same form: the
/// same bytes in all and the same in bulk, phase by phase. A floor that
/// answered READs with nothing (or every one inline) would count the data
/// movement as the director's cost.
///
/// The second workload's READs of 128 KiB set no bulk flag, and the
/// reference answers them in bulk (wire.md 3.4.4: `len >= 65536` with an
/// arena), so the floor is sent them flagged.
#[test]
fn the_floor_reads_the_bytes_the_director_read_in_the_same_form() {
    let unflagged = json!({
        "name": "unflagged",
        "description": "READs of 128 KiB with no bulk flag",
        "tree": {"roots": [{"id": 0,
            "files": [{"path": "Data/Big.bsa", "contents": {"size": 300000, "seed": 1}}]}]},
        "phases": [{"name": "unflagged", "threads": 1, "repeat": 1, "kind": "sequential_read",
                    "path": "Data/Big.bsa", "chunk": 131072, "bulk": false}]
    });
    let dir = workloads(&[every_kind(), unflagged]);
    let (o, r) = bench(dir.path(), &reference(), &[]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let r = r.unwrap();
    for w in r["workloads"].as_array().unwrap() {
        for p in w["phases"].as_array().unwrap() {
            for k in ["read_bytes", "bulk_bytes"] {
                assert_eq!(p["floor"][k], p[k], "{} {k}: {p:#}", p["name"]);
            }
        }
    }
    assert_eq!(phase(&r, "bulk")["bulk_bytes"], 3_000_000);
    assert_eq!(phase(&r, "bulk")["read_bytes"], 3_000_000);
    assert_eq!(phase(&r, "random")["read_bytes"], 2 * 8 * 4096);
    assert_eq!(phase(&r, "random")["bulk_bytes"], 0);
    // Two READs of 128 KiB in bulk, and the 37 856 bytes left inline.
    let p = &r["workloads"][1]["phases"][0];
    assert_eq!(p["read_bytes"], 300_000);
    assert_eq!(p["bulk_bytes"], 2 * 131_072);
}

/// A workload a director answers whatever its tree: OPENs and CLOSEs of
/// the generated names.
fn opens_only() -> Value {
    json!({
        "name": "opens",
        "description": "OPEN and CLOSE only",
        "tree": {"roots": [{"id": 0, "files": []}]},
        "generate": [{"dir": "Data", "count": 2, "name": "F{n}.esp", "size": 8, "seed": 1}],
        "phases": [{"name": "opens", "threads": 1, "repeat": 1, "kind": "storm",
                    "ops": ["open", "close"], "paths": {"prefix": "Data/"}}]
    })
}

/// The null adapter on a ring unlike its default in every number.
fn small_null() -> String {
    format!(
        "{} null-adapter --slots 4 --payload-cap 65536 --arena-len 1048576 --workers 2",
        quoted_exe()
    )
}

/// The floor is built on the director's ring in every number, not on the
/// null adapter's default one: here the director's slots, payload cap,
/// arena and workers all differ from that default.
#[test]
fn the_floor_s_ring_has_the_director_s_shape_whatever_it_is() {
    let dir = workloads(&[opens_only()]);
    let (o, r) = bench(dir.path(), &small_null(), &[]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let r = r.unwrap();
    let w = &r["workloads"][0];
    assert_eq!(
        w["geometry"],
        json!({"ring": "file", "slots": 4, "payload_cap": 65536,
               "arena_len": 1048576, "worker_hint": 2})
    );
    assert_eq!(w["floor_geometry"], w["geometry"]);
}

/// One phase run against `director`: the workload's error, which a strict
/// check gave.
fn error_of(director: &str, workload: Value) -> String {
    let dir = workloads(&[workload]);
    let (o, r) = bench(dir.path(), director, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    let r = r.unwrap();
    r["workloads"][0]["error"]
        .as_str()
        .unwrap_or_else(|| panic!("no error: {r:#}"))
        .to_string()
}

#[test]
fn a_handle_0_is_an_error() {
    let director = format!("{} reference-adapter --break fh-from-zero", quoted_exe());
    let e = error_of(&director, one_bulk_read());
    assert!(e.contains("handle 0"), "{e}");
}

#[test]
fn found_0_for_a_file_of_the_tree_is_an_error() {
    let w = json!({
        "name": "stats",
        "description": "GETATTR of files the tree has",
        "tree": {"roots": [{"id": 0, "files": []}]},
        "generate": [{"dir": "Data", "count": 2, "name": "F{n}.esp", "size": 8, "seed": 1}],
        "phases": [{"name": "stats", "threads": 1, "repeat": 1, "kind": "storm",
                    "ops": ["getattr"], "paths": {"prefix": "Data/"}}]
    });
    let e = error_of(&small_null(), w);
    assert!(e.contains("found = 0"), "{e}");
}

#[test]
fn a_short_write_is_an_error() {
    let w = json!({
        "name": "save",
        "description": "a save written in chunks",
        "tree": {"roots": [{"id": 0, "files": [], "writable": ["Saves"]}]},
        "phases": [{"name": "save", "threads": 1, "repeat": 1, "kind": "write",
                    "path": "Saves/New.ess", "chunk": 1000, "total": 2500,
                    "then_close": true}]
    });
    let director = format!("{} reference-adapter --break short-write", quoted_exe());
    let e = error_of(&director, w);
    assert!(e.contains("wrote 500 of 1000 bytes"), "{e}");
}

#[test]
fn a_listing_of_another_length_is_an_error() {
    let w = json!({
        "name": "list",
        "description": "a listing of three files",
        "tree": {"roots": [{"id": 0, "files": []}]},
        "generate": [{"dir": "Data", "count": 3, "name": "F{n}.esp", "size": 8, "seed": 1}],
        "phases": [{"name": "list", "threads": 1, "repeat": 1, "kind": "readdir", "path": "Data"}]
    });
    // The null director lists nothing.
    let e = error_of(&small_null(), w);
    assert!(e.contains("0 entries, where the tree has 3"), "{e}");
}

/// A save written in chunks of `chunk` bytes.
fn saves_in(chunk: u32) -> Value {
    json!({
        "name": "save",
        "description": "a save written in chunks",
        "tree": {"roots": [{"id": 0, "files": [], "writable": ["Saves"]}]},
        "phases": [{"name": "save", "threads": 1, "repeat": 1, "kind": "write",
                    "path": "Saves/New.ess", "chunk": chunk, "total": chunk,
                    "then_close": true}]
    })
}

/// wire.md 3.4.5: a WRITE request is 24 bytes and its data, so on a 64 KiB
/// payload cap a chunk of `65536 - 24` fits and one byte more does not; the
/// one that does not is refused before anything is sent.
#[test]
fn a_write_chunk_fits_the_payload_cap_with_its_24_byte_header() {
    let dir = workloads(&[saves_in(65536 - 24)]);
    let (o, _) = bench(dir.path(), &small_null(), &[]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let e = error_of(&small_null(), saves_in(65536 - 23));
    assert!(
        e.contains("WRITEs of 65513 bytes do not fit the payload cap 65536"),
        "{e}"
    );
}

/// `--clients N` runs N client threads for each of a phase's threads.
#[test]
fn clients_multiplies_each_phase_s_threads() {
    let dir = workloads(&[every_kind()]);
    let (o, r) = bench(dir.path(), &reference(), &["--clients", "2"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let r = r.unwrap();
    assert_eq!(r["clients"], 2);
    assert_eq!(phase(&r, "storm")["threads"], 2 * 2);
    assert_eq!(phase(&r, "storm")["per_op"]["getattr"]["count"], 2 * 2 * 3);
    assert_eq!(phase(&r, "random")["per_op"]["read"]["count"], 2 * 2 * 8);
}

#[test]
fn a_workload_whose_replies_fail_reports_an_error_and_no_numbers() {
    // The director answers a bulk READ with at most 256 KiB: short of the
    // file, so not what the phase expects. The other workload still runs.
    let dir = workloads(&[one_bulk_read(), every_kind()]);
    let director = format!("{} reference-adapter --break short-bulk", quoted_exe());
    let (o, r) = bench(dir.path(), &director, &["--only", "one-bulk-read"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    let r = r.unwrap();
    let w = &r["workloads"][0];
    assert_eq!(w["name"], "one-bulk-read");
    let e = w["error"].as_str().unwrap_or_else(|| panic!("{r:#}"));
    assert!(
        e.contains("read") && e.contains("262144") && e.contains("1048576"),
        "{e}"
    );
    assert!(w.get("phases").is_none(), "{w}");
    assert!(String::from_utf8_lossy(&o.stdout).contains("one-bulk-read: error"));
}

/// `now` with every director p50 made `factor` times what it was.
fn scaled(report: &Value, factor: f64) -> Value {
    let mut r = report.clone();
    for p in r["workloads"][0]["phases"].as_array_mut().unwrap() {
        for (_, v) in p["per_op"].as_object_mut().unwrap() {
            let p50 = v["p50_us"].as_f64().unwrap();
            v["p50_us"] = json!(p50 * factor);
        }
    }
    r
}

fn write_json(dir: &Path, name: &str, v: &Value) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    p
}

#[test]
fn a_baseline_from_another_machine_is_refused() {
    let (dir, r) = every_kind_report();
    let mut other = r.clone();
    other["machine"]["cpu"] = json!("another CPU");
    // Not among the workloads: every `.json` there is one.
    let baselines = aether_testkit::tempdir().unwrap();
    let base = write_json(baselines.path(), "other-machine.json", &other);
    let (o, _) = bench(
        dir.path(),
        &reference(),
        &["--baseline", base.to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("another machine"), "{}", text(&o));
    // A comparison refuses another geometry too.
    let mut moved = r.clone();
    moved["workloads"][0]["geometry"]["slots"] = json!(99);
    let e = aether_conform::bench::compare(&r, &moved).unwrap_err();
    assert!(e.contains("geometry"), "{e}");
    assert!(aether_conform::bench::compare(&r, &r).is_ok());
}

/// However much slower a run is than its baseline, it exits 0; only
/// `--fail-over` turns a rise into a non-zero exit (3).
#[test]
fn fail_over_is_the_only_way_to_a_non_zero_exit() {
    let (dir, r) = every_kind_report();
    // A baseline whose p50s were a thousandth of these: every opcode rose.
    let fast = scaled(&r, 0.001);
    let baselines = aether_testkit::tempdir().unwrap();
    let base = write_json(baselines.path(), "fast-baseline.json", &fast);
    let c = aether_conform::bench::compare(&r, &fast).unwrap();
    assert!(c.worst_rise_pct > 1000.0, "{}", c.worst_rise_pct);
    let (o, _) = bench(
        dir.path(),
        &reference(),
        &["--baseline", base.to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(text(&o).contains("against the baseline"), "{}", text(&o));
    let (o, _) = bench(
        dir.path(),
        &reference(),
        &["--baseline", base.to_str().unwrap(), "--fail-over", "50"],
    );
    assert_eq!(o.status.code(), Some(3), "{}", text(&o));
    assert!(text(&o).contains("over 50%"), "{}", text(&o));
    let (o, _) = bench(dir.path(), &reference(), &["--fail-over", "50"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}

fn null_adapter_round_trip(args: &str) -> Value {
    let cmd = format!("{} null-adapter {args}", quoted_exe());
    let mut a = Adapter::start(&cmd, Duration::from_secs(30)).unwrap();
    // The tree is never read: the scenario need not exist.
    let env = a
        .mount(Path::new("no-such-scenario.json"))
        .unwrap()
        .expect("mounted");
    let ring = Attached::from_env(&env, Duration::from_secs(10)).unwrap();
    let client = ring.client();
    let mut scratch = Vec::new();
    assert_eq!(
        client.call(&Request::Heartbeat {}, &mut scratch),
        Ok(Reply::Empty)
    );
    let attr = client.call(
        &Request::GetAttr {
            root: RootId(0),
            path: "Data/A.esp",
        },
        &mut scratch,
    );
    assert!(
        matches!(&attr, Ok(Reply::GetAttr(a)) if !a.found),
        "{attr:?}"
    );
    // A READ moves the bytes it asks for: inline unflagged, in bulk flagged
    // when the ring has an arena.
    let read = |flags: RequestFlags, len: u32| {
        let mut body = Vec::new();
        Request::Read {
            slot_flags: flags,
            fh: 1,
            offset: 0,
            len,
        }
        .encode(&mut body);
        let (mut replies, slots) = client
            .ring()
            .submit_many_held(&[(Opcode::Read as u32, flags.bits(), body)])
            .unwrap();
        let r = replies.pop().unwrap();
        let got = match Reply::decode(Opcode::Read, flags.bits(), &r.payload) {
            Ok(Reply::Read(ReadReply::Inline(b))) => (b.len() as u32, false),
            Ok(Reply::Read(ReadReply::Bulk { len, .. })) => (len, true),
            other => panic!("{other:?}"),
        };
        client.ring().release_slots(&slots);
        got
    };
    assert_eq!(read(RequestFlags::empty(), 4096), (4096, false));
    if ring.arena_len() > 0 {
        assert_eq!(read(RequestFlags::READ_BULK, 65536), (65536, true));
    }
    let g = ring.geom();
    let shape = json!({
        "file": env.contains_key(aether_env::RING_PATH),
        "slots": g.slot_count,
        "payload_cap": g.payload_cap,
        "arena_len": ring.arena_len(),
        "workers": aether_ipc::ring::worker_hint(ring.seg()),
    });
    drop(ring);
    a.unmount().unwrap();
    // A second mount after an unmount gets a ring of its own.
    let again = a
        .mount(Path::new("x.json"))
        .unwrap()
        .expect("mounted again");
    assert!(Attached::from_env(&again, Duration::from_secs(10)).is_ok());
    a.unmount().unwrap();
    a.finish();
    shape
}

#[test]
fn null_adapter_answers_mount_and_unmount() {
    assert_eq!(
        null_adapter_round_trip(""),
        json!({"file": true, "slots": 8, "payload_cap": 1048576,
               "arena_len": 8 * 1048576, "workers": 4})
    );
    assert_eq!(
        null_adapter_round_trip("--slots 4 --payload-cap 65536 --arena-len 1048576 --workers 2"),
        json!({"file": true, "slots": 4, "payload_cap": 65536,
               "arena_len": 1048576, "workers": 2})
    );
    #[cfg(windows)]
    assert_eq!(
        null_adapter_round_trip("--ring section")["file"],
        json!(false)
    );
}
