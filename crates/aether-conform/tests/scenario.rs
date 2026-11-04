//! Scenarios: the JSON format and its loader, and the comparison rules a
//! `Session` applies to a director's replies.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use aether_conform::compare::Session;
use aether_conform::scenario::{load, load_dir, Expect, OwnedDirEntry, OwnedReply, OwnedRequest};
use aether_protocol::{AttrResp, Opcode, OpenFlags, OpenResp, RequestFlags, RootId, Status};
use aether_refdirector::tree::Contents;
use serde_json::{json, Value};

fn write(dir: &Path, name: &str, v: &Value) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    p
}

fn scenario(steps: Value) -> Value {
    json!({
        "name": "t",
        "description": "a test scenario",
        "tree": {
            "roots": [
                {"id": 0, "files": [{"path": "data/a.esp", "contents": "abc"}], "writable": ["saves"]}
            ]
        },
        "steps": steps
    })
}

/// One request of every opcode, each in the protocol table's names.
fn every_request() -> Vec<Value> {
    let reg = r"\Registry\Machine\Software\T";
    vec![
        json!({"op": "getattr", "root": 0, "path": "data/a.esp"}),
        json!({"op": "readdir", "root": 0, "path": "."}),
        json!({"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"}),
        json!({"op": "read", "fh": 1, "offset": 0, "len": 3, "slot_flags": ["read_bulk"]}),
        json!({"op": "write", "fh": 1, "offset": 2, "data": {"hex": "00ff"}}),
        json!({"op": "setattr", "fh": 1, "size": 9}),
        json!({"op": "rename", "root": 0, "from": "saves/a", "to": "saves/b"}),
        json!({"op": "delete", "root": 0, "path": "saves/b"}),
        json!({"op": "mkdir", "root": 0, "mode": 493, "path": "saves/d"}),
        json!({"op": "close", "fh": 1}),
        json!({"op": "heartbeat"}),
        json!({"op": "stored-names", "root": 0, "skip": 1, "path": "data/a.esp"}),
        json!({"op": "reg-lookup", "path": reg}),
        json!({"op": "reg-key", "path": reg}),
        json!({"op": "reg-set-value", "path": reg, "name": "v", "ty": 3, "data": {"size": 4, "seed": 1}}),
        json!({"op": "reg-delete-value", "path": reg, "name": "v"}),
        json!({"op": "reg-create-key", "path": reg, "volatile": true}),
        json!({"op": "reg-delete-key", "path": reg}),
        json!({"op": "reg-rename-key", "path": reg, "new_leaf": "U"}),
        json!({"op": "reg-changed", "path": reg, "subtree": true, "since": 5}),
        json!({"op": "rename-x", "from_root": 0, "to_root": 1, "from": "saves/a", "to": "b"}),
        json!({"op": "stage-exec", "root": 0, "path": "bin/game.exe"}),
    ]
}

#[test]
fn a_scenario_with_every_request_shape_loads() {
    let dir = aether_testkit::tempdir().unwrap();
    let steps: Vec<Value> = every_request()
        .into_iter()
        .map(|r| json!({"request": r, "expect": {"status": "not-supported"}}))
        .collect();
    let p = write(dir.path(), "every.json", &scenario(Value::Array(steps)));
    let s = load(&p).expect("loads");
    assert_eq!(s.name, "t");
    assert_eq!(s.steps.len(), Opcode::ALL.len());
    let ops: Vec<Opcode> = s
        .steps
        .iter()
        .map(|st| st.request.as_request().opcode())
        .collect();
    assert_eq!(ops, Opcode::ALL.to_vec());
    assert_eq!(
        s.steps[2].request,
        OwnedRequest::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path: "data/a.esp".into()
        }
    );
    assert_eq!(
        s.steps[3].request,
        OwnedRequest::Read {
            slot_flags: RequestFlags::READ_BULK,
            fh: 1,
            offset: 0,
            len: 3
        }
    );
    assert_eq!(
        s.steps[4].request,
        OwnedRequest::Write {
            fh: 1,
            offset: 2,
            data: vec![0, 0xff]
        }
    );
    let OwnedRequest::RegSetValue { data, .. } = &s.steps[14].request else {
        panic!("reg-set-value");
    };
    assert_eq!(*data, Contents::Fill { size: 4, seed: 1 }.bytes());
    assert_eq!(s.steps[0].expect, Expect::Status(Status::NotSupported));
    assert_eq!(s.tree.roots[0].files[0].contents.bytes(), b"abc");
}

/// The loader's field names for every opcode are `contract/protocol.json`'s
/// request fields (padding aside), so a scenario is written in the table's
/// own vocabulary.
#[test]
fn the_loader_knows_exactly_the_protocol_tables_request_fields() {
    let table: Value = serde_json::from_slice(
        &std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contract/protocol.json"
        ))
        .unwrap(),
    )
    .unwrap();
    for op in table["opcodes"].as_array().unwrap() {
        let name = op["name"].as_str().unwrap();
        let fields: BTreeSet<String> = op["request"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .filter(|n| n != "pad")
            .collect();
        let mut ours: BTreeSet<String> = aether_conform::scenario::request_fields(name)
            .unwrap_or_else(|| panic!("the loader has no op {name}"))
            .iter()
            .map(|s| s.to_string())
            .collect();
        if op["slot_flags"].as_bool().unwrap() {
            assert!(ours.remove("slot_flags"), "{name} takes slot_flags");
        }
        assert_eq!(ours, fields, "{name}");
    }
}

fn load_error(steps: Value) -> String {
    let dir = aether_testkit::tempdir().unwrap();
    let p = write(dir.path(), "bad.json", &scenario(steps));
    load(&p).expect_err("must not load").to_string()
}

fn heartbeat_ok() -> Value {
    json!({"request": {"op": "heartbeat"}, "expect": {"status": "ok"}})
}

/// Handles are unique among open handles (wire.md 3.4.3), so an OPEN that
/// expects a number still open in the scenario cannot be right: the
/// comparison would accept a director that issued one handle twice.
#[test]
fn an_open_expecting_a_handle_still_open_is_a_load_error() {
    let open = |fh: u64| {
        json!({"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"},
               "expect": {"reply": {"fh": fh, "size": 3, "is_dir": false, "immutable": false, "mount_gen": 0}}})
    };
    let close = |fh: u64| json!({"request": {"op": "close", "fh": fh}, "expect": {"status": "ok"}});
    let e = load_error(json!([open(1), heartbeat_ok(), open(1)]));
    assert!(
        e.contains("bad.json") && e.contains("step 2") && e.contains("expect.reply.fh"),
        "{e}"
    );
    assert!(e.contains("still open"), "{e}");
    // Also as one alternative of a one_of.
    let alt = json!({"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"},
        "expect": {"one_of": [{"status": "not-found"},
                              {"reply": {"fh": 1, "size": 3, "is_dir": false, "immutable": false, "mount_gen": 0}}],
                   "wire": "3.4.3"}});
    let e = load_error(json!([open(1), alt]));
    assert!(e.contains("step 1") && e.contains("still open"), "{e}");

    // After a CLOSE the number may come back, and other numbers are fine.
    let dir = aether_testkit::tempdir().unwrap();
    let p = write(
        dir.path(),
        "ok.json",
        &scenario(json!([
            open(1),
            open(2),
            close(1),
            open(1),
            close(1),
            close(2)
        ])),
    );
    load(&p).unwrap();
}

#[test]
fn an_unknown_op_names_the_file_the_step_and_the_field() {
    let e = load_error(
        json!([heartbeat_ok(), {"request": {"op": "frobnicate"}, "expect": {"status": "ok"}}]),
    );
    assert!(e.contains("bad.json"), "{e}");
    assert!(e.contains("step 1"), "{e}");
    assert!(e.contains("op") && e.contains("frobnicate"), "{e}");
}

#[test]
fn an_unknown_field_names_the_file_the_step_and_the_field() {
    let e = load_error(json!([
        heartbeat_ok(),
        heartbeat_ok(),
        {"request": {"op": "getattr", "root": 0, "path": "a", "colour": 1}, "expect": {"status": "ok"}}
    ]));
    assert!(
        e.contains("bad.json") && e.contains("step 2") && e.contains("colour"),
        "{e}"
    );
}

#[test]
fn an_unknown_status_names_the_file_the_step_and_the_field() {
    let e = load_error(
        json!([{"request": {"op": "getattr", "root": 0, "path": "a"}, "expect": {"status": "kaput"}}]),
    );
    assert!(
        e.contains("bad.json")
            && e.contains("step 0")
            && e.contains("status")
            && e.contains("kaput"),
        "{e}"
    );
}

#[test]
fn an_unknown_open_flag_and_a_missing_field_are_load_errors() {
    let e = load_error(
        json!([{"request": {"op": "open", "root": 0, "flags": ["sideways"], "path": "a"}, "expect": {"status": "ok"}}]),
    );
    assert!(
        e.contains("step 0") && e.contains("flags") && e.contains("sideways"),
        "{e}"
    );
    let e = load_error(
        json!([{"request": {"op": "open", "root": 0, "path": "a"}, "expect": {"status": "ok"}}]),
    );
    assert!(e.contains("step 0") && e.contains("flags"), "{e}");
}

#[test]
fn a_non_canonical_path_is_a_load_error() {
    for path in ["/a", "a/", "a//b", "a/./b", "a/../b", r"a\b", ""] {
        let e = load_error(
            json!([{"request": {"op": "getattr", "root": 0, "path": path}, "expect": {"status": "ok"}}]),
        );
        assert!(e.contains("step 0") && e.contains("path"), "{path:?}: {e}");
    }
}

/// wire.md 3.2 says what a path that leaves its root gets, so a scenario may
/// send one that starts with `..`; the rest of the non-canonical syntax stays
/// refused.
#[test]
fn a_path_that_leaves_its_root_loads() {
    let dir = aether_testkit::tempdir().unwrap();
    for path in ["..", "../x", "../../x/y", "../a/../b"] {
        let p = write(
            dir.path(),
            "esc.json",
            &scenario(json!([{
                "request": {"op": "getattr", "root": 0, "path": path},
                "expect": {"status": "bad-request"}
            }])),
        );
        let s = load(&p).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        let OwnedRequest::GetAttr { path: got, .. } = &s.steps[0].request else {
            panic!("not a getattr");
        };
        assert_eq!(got, path);
    }
    for path in ["../", "..//x", ".././x", r"..\x", "a/../../x"] {
        let e = load_error(
            json!([{"request": {"op": "getattr", "root": 0, "path": path}, "expect": {"status": "bad-request"}}]),
        );
        assert!(e.contains("step 0") && e.contains("path"), "{path:?}: {e}");
    }
}

#[test]
fn one_of_needs_the_wire_section_that_allows_it() {
    let e = load_error(json!([{
        "request": {"op": "stored-names", "root": 0, "skip": 0, "path": "zz"},
        "expect": {"one_of": [{"reply": {"names": "zz"}}, {"status": "not-found"}]}
    }]));
    assert!(e.contains("step 0") && e.contains("wire"), "{e}");
    let dir = aether_testkit::tempdir().unwrap();
    let p = write(
        dir.path(),
        "ok.json",
        &scenario(json!([{
            "request": {"op": "stored-names", "root": 0, "skip": 0, "path": "zz"},
            "expect": {"one_of": [{"reply": {"names": "zz"}}, {"status": "not-found"}], "wire": "3.4.12"}
        }])),
    );
    let s = load(&p).unwrap();
    assert_eq!(
        s.steps[0].expect,
        Expect::OneOf(vec![
            Expect::Reply(OwnedReply::StoredNames("zz".into())),
            Expect::Status(Status::NotFound)
        ])
    );
    assert_eq!(s.steps[0].cite.as_deref(), Some("3.4.12"));
}

#[test]
fn load_dir_loads_every_json_file_in_name_order() {
    let dir = aether_testkit::tempdir().unwrap();
    let mut b = scenario(json!([heartbeat_ok()]));
    b["name"] = json!("b");
    let mut a = b.clone();
    a["name"] = json!("a");
    write(dir.path(), "2-b.json", &b);
    write(dir.path(), "1-a.json", &a);
    std::fs::write(dir.path().join("README.md"), "not a scenario").unwrap();
    let all = load_dir(dir.path()).unwrap();
    let names: Vec<_> = all.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
}

// ---- comparison ----

/// A request for the steps whose rules do not look at it.
const HB: OwnedRequest = OwnedRequest::Heartbeat;

fn open_resp(fh: u64, size: u64) -> OwnedReply {
    OwnedReply::Open(OpenResp {
        fh,
        size,
        ..Default::default()
    })
}

fn handles_scenario() -> aether_conform::scenario::Scenario {
    let dir = aether_testkit::tempdir().unwrap();
    let open = |p: &str| json!({"op": "open", "root": 0, "flags": ["read"], "path": p});
    let reply = |fh: u64| json!({"reply": {"fh": fh, "size": 3, "is_dir": false, "immutable": false, "mount_gen": 0}});
    let p = write(
        dir.path(),
        "fh.json",
        &scenario(json!([
            {"request": open("data/a.esp"), "expect": reply(1)},
            {"request": open("data/a.esp"), "expect": reply(2)},
            {"request": {"op": "close", "fh": 1}, "expect": {"status": "ok"}},
            {"request": open("data/a.esp"), "expect": reply(3)},
        ])),
    );
    load(&p).unwrap()
}

#[test]
fn handles_compare_up_to_a_consistent_renaming() {
    let s = handles_scenario();
    let mut sess = Session::new();
    let e = |i: usize| s.steps[i].expect.clone();
    sess.check(0, &s.steps[0].request, &e(0), &Ok(open_resp(7, 3)))
        .unwrap();
    sess.check(1, &s.steps[1].request, &e(1), &Ok(open_resp(9, 3)))
        .unwrap();
    // The scenario's fh 1 is the director's 7 in a request.
    assert_eq!(
        sess.translate(&s.steps[2].request),
        OwnedRequest::Close { fh: 7 }
    );
    sess.check(2, &s.steps[2].request, &e(2), &Ok(OwnedReply::Empty))
        .unwrap();
    // Closed, 7 may be issued again (wire.md 3.4.3: unique among open handles).
    sess.check(3, &s.steps[3].request, &e(3), &Ok(open_resp(7, 3)))
        .unwrap();
}

#[test]
fn an_inconsistent_handle_renaming_is_a_mismatch() {
    let s = handles_scenario();
    let mut sess = Session::new();
    let e = |i: usize| s.steps[i].expect.clone();
    sess.check(0, &s.steps[0].request, &e(0), &Ok(open_resp(7, 3)))
        .unwrap();
    // Two open handles cannot both be the director's 7.
    let m = sess
        .check(1, &s.steps[1].request, &e(1), &Ok(open_resp(7, 3)))
        .expect_err("fh 7 is already the scenario's fh 1");
    let text = m.to_string();
    assert!(text.contains("step 1") && text.contains('7'), "{text}");
}

#[test]
fn a_different_status_or_field_is_a_mismatch_showing_both() {
    let s = handles_scenario();
    let mut sess = Session::new();
    let m = sess
        .check(
            0,
            &s.steps[0].request,
            &s.steps[0].expect,
            &Err(Status::NotFound),
        )
        .unwrap_err();
    let text = m.to_string();
    assert!(
        text.contains("not-found") && text.contains("size: 3"),
        "{text}"
    );
    let mut sess = Session::new();
    assert!(sess
        .check(
            0,
            &s.steps[0].request,
            &s.steps[0].expect,
            &Ok(open_resp(7, 4))
        )
        .is_err());
}

fn entry(name: &str, size: u64) -> OwnedDirEntry {
    OwnedDirEntry {
        name: name.into(),
        is_dir: false,
        size,
        mtime: 0,
    }
}

/// wire.md 3.4.2 gives a listing no order, so two listings with the same
/// entries in another order are the same answer; different entries are not.
#[test]
fn listings_compare_as_sets_of_entries() {
    let expected = Expect::Reply(OwnedReply::ReadDir(vec![entry("a", 1), entry("b", 2)]));
    let mut sess = Session::new();
    let got = |v: Vec<OwnedDirEntry>| Ok(OwnedReply::ReadDir(v));
    sess.check(0, &HB, &expected, &got(vec![entry("b", 2), entry("a", 1)]))
        .unwrap();
    assert!(sess
        .check(0, &HB, &expected, &got(vec![entry("a", 1)]))
        .is_err());
    assert!(sess
        .check(0, &HB, &expected, &got(vec![entry("a", 1), entry("a", 1)]))
        .is_err());
    assert!(sess
        .check(0, &HB, &expected, &got(vec![entry("a", 1), entry("b", 3)]))
        .is_err());
    // Names are compared as stored (wire.md 3.2), not folded.
    assert!(sess
        .check(0, &HB, &expected, &got(vec![entry("A", 1), entry("b", 2)]))
        .is_err());
}

/// wire.md 3.2 defines mtime's encoding and that 0 or less is "no time",
/// and nothing about which time a director reports.
#[test]
fn mtimes_are_not_compared() {
    let mut sess = Session::new();
    let attr = |mtime| {
        OwnedReply::GetAttr(AttrResp {
            found: true,
            is_dir: false,
            size: 3,
            mtime,
        })
    };
    sess.check(0, &HB, &Expect::Reply(attr(0)), &Ok(attr(1_700_000_000)))
        .unwrap();
    let mut e = entry("a", 1);
    e.mtime = 42;
    sess.check(
        0,
        &HB,
        &Expect::Reply(OwnedReply::ReadDir(vec![entry("a", 1)])),
        &Ok(OwnedReply::ReadDir(vec![e])),
    )
    .unwrap();
}

#[test]
fn one_of_accepts_any_of_its_answers() {
    let mut sess = Session::new();
    let e = Expect::OneOf(vec![
        Expect::Reply(OwnedReply::StoredNames("zz".into())),
        Expect::Status(Status::NotFound),
    ]);
    sess.check(0, &HB, &e, &Err(Status::NotFound)).unwrap();
    sess.check(0, &HB, &e, &Ok(OwnedReply::StoredNames("zz".into())))
        .unwrap();
    assert!(sess.check(0, &HB, &e, &Err(Status::IoError)).is_err());
}

/// wire.md 4.3: how far a mounted layer's version starts is the director's,
/// how it moves is the overlay's. One offset per session, then.
#[test]
fn registry_versions_compare_up_to_one_offset() {
    let mut sess = Session::new();
    let v = |n| Expect::Reply(OwnedReply::RegVersion(n));
    sess.check(0, &HB, &v(3), &Ok(OwnedReply::RegVersion(10)))
        .unwrap();
    sess.check(1, &HB, &v(4), &Ok(OwnedReply::RegVersion(11)))
        .unwrap();
    assert!(sess
        .check(2, &HB, &v(5), &Ok(OwnedReply::RegVersion(13)))
        .is_err());
    let since = OwnedRequest::RegChanged {
        path: r"\Registry\Machine".into(),
        subtree: false,
        since: 4,
    };
    let OwnedRequest::RegChanged { since, .. } = sess.translate(&since) else {
        panic!("reg-changed");
    };
    assert_eq!(since, 11);
}

/// wire.md 3.4.3: `immutable` is the director's to give, never on a write
/// open or a directory.
#[test]
fn immutable_is_the_directors_except_on_a_write_open() {
    let dir = aether_testkit::tempdir().unwrap();
    let reply =
        json!({"reply": {"fh": 1, "size": 3, "is_dir": false, "immutable": false, "mount_gen": 0}});
    let p = write(
        dir.path(),
        "imm.json",
        &scenario(json!([
            {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"}, "expect": reply},
            {"request": {"op": "close", "fh": 1}, "expect": {"status": "ok"}},
            {"request": {"op": "open", "root": 0, "flags": ["write"], "path": "saves/x"}, "expect": reply},
        ])),
    );
    let s = load(&p).unwrap();
    let imm = |fh| {
        Ok(OwnedReply::Open(OpenResp {
            fh,
            size: 3,
            immutable: true,
            mount_gen: 1,
            ..Default::default()
        }))
    };
    let mut sess = Session::new();
    sess.check(0, &s.steps[0].request, &s.steps[0].expect, &imm(5))
        .unwrap();
    let mut sess = Session::new();
    assert!(sess
        .check(2, &s.steps[2].request, &s.steps[2].expect, &imm(5))
        .is_err());
}

#[test]
fn a_bad_tree_is_a_load_error_naming_the_tree() {
    let dir = aether_testkit::tempdir().unwrap();
    let mut v = scenario(json!([heartbeat_ok()]));
    v["tree"]["roots"][0]["colour"] = json!(1);
    let e = load(&write(dir.path(), "t.json", &v))
        .unwrap_err()
        .to_string();
    assert!(e.contains("tree") && e.contains("colour"), "{e}");
    let mut v = scenario(json!([heartbeat_ok()]));
    v["tree"]["roots"][0]["files"][0]["path"] = json!("data/../a");
    let e = load(&write(dir.path(), "t.json", &v))
        .unwrap_err()
        .to_string();
    assert!(e.contains("tree") && e.contains("canonical"), "{e}");
}

/// The example in `contract/scenarios/README.md` is a scenario that loads.
#[test]
fn the_readmes_example_loads() {
    let readme = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contract/scenarios/README.md"
    ))
    .unwrap()
    .replace("\r\n", "\n");
    let start = readme.find("```json\n").expect("a json block") + "```json\n".len();
    let end = start + readme[start..].find("```").unwrap();
    let dir = aether_testkit::tempdir().unwrap();
    let p = dir.path().join("example.json");
    std::fs::write(&p, &readme[start..end]).unwrap();
    let s = load(&p).unwrap_or_else(|e| panic!("{e}"));
    assert!(!s.steps.is_empty());
}

/// A use-after-close goes to the director with the number it closed, so
/// the step tests that handle and not some other one.
#[test]
fn a_closed_handle_is_sent_as_the_directors_old_number() {
    let s = handles_scenario();
    let mut sess = Session::new();
    let e = |i: usize| s.steps[i].expect.clone();
    sess.check(0, &s.steps[0].request, &e(0), &Ok(open_resp(7, 3)))
        .unwrap();
    sess.check(1, &s.steps[1].request, &e(1), &Ok(open_resp(9, 3)))
        .unwrap();
    sess.check(2, &s.steps[2].request, &e(2), &Ok(OwnedReply::Empty))
        .unwrap();
    let read = OwnedRequest::Read {
        slot_flags: RequestFlags::empty(),
        fh: 1,
        offset: 0,
        len: 3,
    };
    let OwnedRequest::Read { fh, .. } = sess.translate(&read) else {
        panic!("read");
    };
    assert_eq!(fh, 7);
    sess.check(
        4,
        &read,
        &Expect::Status(Status::BadFh),
        &Err(Status::BadFh),
    )
    .unwrap();
    // Once the director issues 7 again, the scenario's closed 1 must not
    // reach the new open: it goes to a number no open handle has.
    sess.check(3, &s.steps[3].request, &e(3), &Ok(open_resp(7, 3)))
        .unwrap();
    let OwnedRequest::Read { fh, .. } = sess.translate(&read) else {
        panic!("read");
    };
    assert!(fh != 7 && fh != 9, "{fh}");
}

/// Among several clients sending at once, a closed handle's old number may
/// already be another client's (wire.md 2.8.1), so a session for one of
/// them sends a stand-in no handle it knows of has, never the old number.
#[test]
fn among_other_clients_a_closed_handle_is_sent_as_a_stand_in() {
    let s = handles_scenario();
    let mut sess = Session::among_others();
    let e = |i: usize| s.steps[i].expect.clone();
    sess.check(0, &s.steps[0].request, &e(0), &Ok(open_resp(7, 3)))
        .unwrap();
    sess.check(1, &s.steps[1].request, &e(1), &Ok(open_resp(9, 3)))
        .unwrap();
    sess.check(2, &s.steps[2].request, &e(2), &Ok(OwnedReply::Empty))
        .unwrap();
    let read = OwnedRequest::Read {
        slot_flags: RequestFlags::empty(),
        fh: 1,
        offset: 0,
        len: 3,
    };
    let OwnedRequest::Read { fh, .. } = sess.translate(&read) else {
        panic!("read");
    };
    assert!(fh != 7 && fh != 9, "{fh}");
    let OwnedRequest::Read { fh: again, .. } = sess.translate(&read) else {
        panic!("read");
    };
    assert_eq!(fh, again, "the same stand-in each time");
}

/// A number no OPEN paired is sent as one no open handle of the director
/// has, the same one each time.
#[test]
fn a_never_paired_handle_is_sent_as_a_number_no_open_handle_has() {
    let s = handles_scenario();
    let mut sess = Session::new();
    // The director's first handle is u64::MAX, which a naive substitute
    // would collide with.
    sess.check(
        0,
        &s.steps[0].request,
        &s.steps[0].expect,
        &Ok(open_resp(u64::MAX, 3)),
    )
    .unwrap();
    let close = |fh| OwnedRequest::Close { fh };
    let a = sess.translate(&close(99));
    let b = sess.translate(&close(99));
    assert_eq!(a, b);
    let OwnedRequest::Close { fh } = a else {
        panic!("close")
    };
    assert!(fh != u64::MAX && fh != 99, "{fh}");
    assert_ne!(sess.translate(&close(98)), a);
}

/// `Status::name()` is `contract/protocol.json`'s name for every status, so
/// a scenario's status names are the table's.
#[test]
fn status_names_are_the_protocol_tables() {
    let table: Value = serde_json::from_slice(
        &std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contract/protocol.json"
        ))
        .unwrap(),
    )
    .unwrap();
    let theirs: Vec<(String, i64)> = table["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_string(),
                s["number"].as_i64().unwrap(),
            )
        })
        .collect();
    let ours: Vec<(String, i64)> = Status::ALL
        .iter()
        .map(|s| (s.name().to_string(), i64::from(s.code())))
        .collect();
    assert_eq!(ours, theirs);
}

#[test]
fn a_registry_key_the_overlay_refuses_is_a_load_error_naming_the_tree() {
    let dir = aether_testkit::tempdir().unwrap();
    for keys in [
        json!([{"path": r"\Nope"}]),
        json!([{"path": r"\Registry\Machine\T", "values": [{"name": "v", "ty": 3, "data": {"size": 1048577}}]}]),
        json!([{"path": r"\Registry\Machine\T", "volatile": true},
               {"path": r"\Registry\Machine\t", "volatile": false}]),
    ] {
        let mut v = scenario(json!([heartbeat_ok()]));
        v["tree"]["registry"] = json!({"keys": keys});
        let e = load(&write(dir.path(), "r.json", &v))
            .expect_err("must not load")
            .to_string();
        assert!(e.contains("r.json") && e.contains(": tree:"), "{keys}: {e}");
    }
}

/// wire.md 3.4.1 and 3.4.3: a directory's size is 0, so a director that
/// reports another size for a directory in GETATTR or OPEN is not conformant.
#[test]
fn a_directorys_size_is_compared_in_getattr_and_open() {
    let dir = aether_testkit::tempdir().unwrap();
    let open =
        json!({"reply": {"fh": 1, "size": 0, "is_dir": true, "immutable": false, "mount_gen": 0}});
    let attr = json!({"reply": {"found": true, "is_dir": true, "size": 0, "mtime": 0}});
    let p = write(
        dir.path(),
        "dsize.json",
        &scenario(json!([
            {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data"}, "expect": open},
            {"request": {"op": "getattr", "root": 0, "path": "data"}, "expect": attr},
        ])),
    );
    let s = load(&p).unwrap();
    let step = |i: usize, got: OwnedReply| {
        Session::new().check(i, &s.steps[i].request, &s.steps[i].expect, &Ok(got))
    };
    let dopen = |size| {
        OwnedReply::Open(OpenResp {
            fh: 9,
            size,
            is_dir: true,
            ..Default::default()
        })
    };
    let attr = |size| {
        OwnedReply::GetAttr(AttrResp {
            found: true,
            is_dir: true,
            size,
            mtime: 5,
        })
    };
    assert!(step(0, dopen(0)).is_ok());
    assert!(step(0, dopen(4096)).is_err());
    assert!(step(1, attr(0)).is_ok());
    assert!(step(1, attr(4096)).is_err());
}

/// The load error for a path that is neither canonical nor an escape is one
/// line.
#[test]
fn the_non_canonical_path_message_is_one_line() {
    let e = load_error(
        json!([{"request": {"op": "getattr", "root": 0, "path": "a//b"}, "expect": {"status": "ok"}}]),
    );
    assert!(e.contains("canonical") && !e.contains('\n'), "{e:?}");
    assert!(!e.contains("  "), "{e:?}");
}

/// A step names the client that sends it (`"client"`, default 0); handles
/// are paired across clients, so one client may present another's handle
/// (wire.md 2.8.1), and a number open in any client is open for all.
#[test]
fn a_step_names_its_client_and_handles_are_counted_across_clients() {
    let dir = aether_testkit::tempdir().unwrap();
    let p = write(
        dir.path(),
        "clients.json",
        &scenario(json!([
            {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"},
             "expect": {"reply": {"fh": 1, "size": 3, "is_dir": false, "immutable": true, "mount_gen": 0}}},
            {"client": 2, "request": {"op": "read", "fh": 1, "offset": 0, "len": 3},
             "expect": {"reply": {"data": "abc"}}},
            {"client": 1, "request": {"op": "close", "fh": 1}, "expect": {"status": "ok"}}
        ])),
    );
    let s = load(&p).unwrap();
    let clients: Vec<u32> = s.steps.iter().map(|st| st.client).collect();
    assert_eq!(clients, [0, 2, 1]);
    // Written back only where it is not 0, and read back the same.
    let text = aether_conform::scenario::to_json_string(&s);
    assert_eq!(text.matches("\"client\"").count(), 2, "{text}");
    std::fs::write(&p, text).unwrap();
    assert_eq!(load(&p).unwrap(), s);

    let e = load_error(json!([
        {"request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"},
         "expect": {"reply": {"fh": 1, "size": 3, "is_dir": false, "immutable": true, "mount_gen": 0}}},
        {"client": 1, "request": {"op": "open", "root": 0, "flags": ["read"], "path": "data/a.esp"},
         "expect": {"reply": {"fh": 1, "size": 3, "is_dir": false, "immutable": true, "mount_gen": 0}}}
    ]));
    assert!(e.contains("step 1") && e.contains("still open"), "{e}");
    let e = load_error(json!([
        {"client": "one", "request": {"op": "heartbeat"}, "expect": {"status": "ok"}}
    ]));
    assert!(e.contains("step 0") && e.contains("client"), "{e}");
}
