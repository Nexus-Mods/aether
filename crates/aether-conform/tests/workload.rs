//! Workloads (`contract/workloads/*.json`): every checked-in one loads, the
//! loader refuses what is not a workload, and `expand` is deterministic.

use std::path::{Path, PathBuf};

use aether_conform::scenario::OwnedRequest;
use aether_conform::workload::{self, Entry, PathSet, PhaseKind, StormOp, Workload};
use serde_json::{json, Value};

fn workloads_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contract/workloads")
}

fn tiny() -> Value {
    json!({
        "name": "tiny",
        "description": "a few files and every kind of phase",
        "tree": {"roots": [{"id": 0,
            "files": [{"path": "Data/A.esp", "contents": {"size": 65536, "seed": 1}},
                      {"path": "Saves/Old.ess", "contents": "old"}],
            "writable": ["Saves"]}]},
        "generate": [{"dir": "Data/Meshes", "count": 12, "name": "Rock{n}.nif",
                      "pad": 2, "size": 16, "seed": 5}],
        "phases": [
            {"name": "storm", "threads": 2, "repeat": 3, "kind": "storm",
             "ops": ["getattr", "open", "close", "stored_names"],
             "paths": {"prefix": "data/meshes/"}},
            {"name": "random", "threads": 2, "repeat": 1, "kind": "random_read",
             "path": "Data/A.esp", "len": 4096, "count": 32, "seed": 9},
            {"name": "seq", "threads": 1, "repeat": 1, "kind": "sequential_read",
             "path": "Data/A.esp", "chunk": 10000, "bulk": true},
            {"name": "list", "threads": 1, "repeat": 2, "kind": "readdir", "path": "Data/Meshes"},
            {"name": "save", "threads": 2, "repeat": 1, "kind": "write",
             "path": "Saves/New.tmp", "chunk": 1000, "total": 2500,
             "then_close": true, "rename_to": "Saves/Old.ess"}
        ]
    })
}

fn load_err(v: &Value) -> String {
    let dir = aether_testkit::tempdir().unwrap();
    let p = dir.path().join("w.json");
    std::fs::write(&p, serde_json::to_vec(v).unwrap()).unwrap();
    let e = workload::load(&p).unwrap_err();
    assert_eq!(e.file, p);
    e.to_string()
}

#[test]
fn each_checked_in_workload_loads() {
    let all = workload::load_dir(&workloads_dir()).unwrap_or_else(|e| panic!("{e}"));
    let names: Vec<&str> = all.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "launch-storm",
            "random-small",
            "readdir-large",
            "save-heavy",
            "sequential-bulk"
        ]
    );
    // What each file was written to be.
    let by = |n: &str| all.iter().find(|w| w.name == n).unwrap();
    let storm = by("launch-storm");
    let files = storm.paths(&PathSet::Prefix("Data/".into())).len();
    assert!((2000..5000).contains(&files), "{files} files");
    assert_eq!(
        by("sequential-bulk").lookup("data/textures0.bsa"),
        Some(Entry::File {
            size: 256 * 1024 * 1024
        })
    );
    assert_eq!(
        by("random-small").lookup("Data/Meshes.bsa"),
        Some(Entry::File {
            size: 64 * 1024 * 1024
        })
    );
    for p in ["Data/Textures/Terrain", "Data/Meshes/Terrain"] {
        assert_eq!(
            by("readdir-large").lookup(p),
            Some(Entry::Dir { entries: 5000 })
        );
    }
    assert!(matches!(
        by("save-heavy").phases[0].kind,
        PhaseKind::Write {
            chunk: 65536,
            then_close: true,
            rename_to: Some(_),
            ..
        }
    ));
}

#[test]
fn an_unknown_kind_fails_naming_the_file_and_the_field() {
    let mut v = tiny();
    v["phases"][1]["kind"] = json!("teleport");
    let e = load_err(&v);
    assert!(e.contains("w.json") && e.contains("phases[1].kind"), "{e}");
    assert!(e.contains("teleport"), "{e}");
}

#[test]
fn an_unknown_field_fails_naming_the_file_and_the_field() {
    let mut v = tiny();
    v["phases"][2]["bulky"] = json!(true);
    let e = load_err(&v);
    assert!(e.contains("w.json") && e.contains("phases[2].bulky"), "{e}");
    let mut v = tiny();
    v["extra"] = json!(1);
    let e = load_err(&v);
    assert!(e.contains(": extra: unknown field"), "{e}");
    let mut v = tiny();
    v["generate"][0]["colour"] = json!("red");
    let e = load_err(&v);
    assert!(e.contains("generate[0].colour"), "{e}");
}

#[test]
fn paths_the_tree_does_not_have_are_refused() {
    let mut v = tiny();
    v["phases"][1]["path"] = json!("Data/Missing.esp");
    assert!(load_err(&v).contains("phases[1].path"));
    let mut v = tiny();
    v["phases"][3]["path"] = json!("Data/A.esp");
    assert!(load_err(&v).contains("not a directory"));
    let mut v = tiny();
    v["phases"][4]["path"] = json!("Data/New.tmp");
    assert!(load_err(&v).contains("writable"));
    let mut v = tiny();
    v["phases"][0]["ops"] = json!(["close", "open"]);
    assert!(load_err(&v).contains("a close with no open"));
    let mut v = tiny();
    v["phases"][0]["ops"] = json!(["open"]);
    assert!(load_err(&v).contains("never closed"));
}

#[test]
fn expand_gives_the_same_requests_twice() {
    let w = Workload::from_json(&tiny()).unwrap();
    for p in &w.phases {
        for t in 0..p.threads {
            let a: Vec<OwnedRequest> = w.expand(p, t).collect();
            let b: Vec<OwnedRequest> = w.expand(p, t).collect();
            assert_eq!(a, b, "phase {}", p.name);
            assert_eq!(a.len() as u64, w.requests_per_thread(p), "phase {}", p.name);
        }
    }
}

#[test]
fn random_reads_differ_per_thread_and_stay_in_the_file() {
    let w = Workload::from_json(&tiny()).unwrap();
    let p = &w.phases[1];
    let offsets = |t| -> Vec<u64> {
        w.expand(p, t)
            .filter_map(|r| match r {
                OwnedRequest::Read { offset, len, .. } => {
                    assert_eq!(len, 4096);
                    assert_eq!(offset % 4096, 0);
                    assert!(offset + 4096 <= 65536);
                    Some(offset)
                }
                _ => None,
            })
            .collect()
    };
    let (a, b) = (offsets(0), offsets(1));
    assert_eq!(a.len(), 32);
    assert_ne!(a, b);
}

#[test]
fn the_storm_sends_ops_times_paths_times_repeat() {
    let w = Workload::from_json(&tiny()).unwrap();
    let p = &w.phases[0];
    let PhaseKind::Storm { ops, paths } = &p.kind else {
        panic!("a storm")
    };
    assert_eq!(ops.len(), 4);
    assert_eq!(ops[3], StormOp::StoredNames);
    let files = w.paths(paths).len();
    assert_eq!(files, 12);
    for t in 0..p.threads {
        let reqs: Vec<OwnedRequest> = w.expand(p, t).collect();
        assert_eq!(reqs.len(), 4 * 12 * 3);
        // Each CLOSE closes the handle its OPEN just got.
        let mut last = 0;
        for r in &reqs {
            match r {
                OwnedRequest::Close { fh } => assert_eq!(*fh, last),
                OwnedRequest::Open { .. } => last += 1,
                _ => {}
            }
        }
        assert_eq!(last, 12 * 3);
    }
    // The two threads start at different files.
    let first = |t| match w.expand(p, t).next() {
        Some(OwnedRequest::GetAttr { path, .. }) => path,
        other => panic!("{other:?}"),
    };
    assert_ne!(first(0), first(1));
}

#[test]
fn writes_cover_the_total_and_each_thread_has_its_own_file() {
    let w = Workload::from_json(&tiny()).unwrap();
    let p = &w.phases[4];
    let reqs: Vec<OwnedRequest> = w.expand(p, 1).collect();
    let written: usize = reqs
        .iter()
        .map(|r| match r {
            OwnedRequest::Write { data, .. } => data.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(written, 2500);
    assert!(matches!(&reqs[0], OwnedRequest::Open { path, .. } if path == "Saves/New.tmp.1"));
    assert!(matches!(&reqs[4], OwnedRequest::Close { fh: 1 }));
    assert!(
        matches!(&reqs[5], OwnedRequest::Rename { from, to, .. } if from == "Saves/New.tmp.1" && to == "Saves/Old.ess.1")
    );
}

/// Every path a workload's requests name for reading or listing is in its
/// tree, and every path it writes is in a directory the tree has.
#[test]
fn every_path_a_workload_names_exists_in_its_tree() {
    let mut all = workload::load_dir(&workloads_dir()).unwrap();
    all.push(Workload::from_json(&tiny()).unwrap());
    for w in &all {
        for p in &w.phases {
            for r in w.expand(p, 0) {
                let at = format!("{} phase {}", w.name, p.name);
                match &r {
                    OwnedRequest::GetAttr { path, .. } | OwnedRequest::StoredNames { path, .. } => {
                        assert!(w.lookup(path).is_some(), "{at}: {path}")
                    }
                    OwnedRequest::Open { path, flags, .. }
                        if !flags.contains(aether_protocol::OpenFlags::WRITE) =>
                    {
                        assert!(
                            matches!(w.lookup(path), Some(Entry::File { .. })),
                            "{at}: {path}"
                        )
                    }
                    OwnedRequest::ReadDir { path, .. } => {
                        assert!(
                            matches!(w.lookup(path), Some(Entry::Dir { .. })),
                            "{at}: {path}"
                        )
                    }
                    OwnedRequest::Open { path, .. } | OwnedRequest::Rename { to: path, .. } => {
                        let parent = path.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
                        assert!(
                            matches!(w.lookup(parent), Some(Entry::Dir { .. })),
                            "{at}: {path}"
                        )
                    }
                    _ => {}
                }
            }
        }
    }
}
