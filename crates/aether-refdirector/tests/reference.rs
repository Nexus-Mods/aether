//! `Reference` answered directly through its `Director` methods: for each
//! opcode, the reply the shim tests' fake director gave before it became
//! this crate, except where `docs/wire.md` says otherwise (each such case
//! names its section).

use aether_ipc::{DataArena, OwnedSeg};
use aether_protocol::{
    AttrResp, ChangedReply, DirEntry, KeyReply, Lookup, LookupReply, Opcode, OpenFlags, OpenResp,
    ReadReply, RequestFlags, RootId, Status,
};
use aether_refdirector::{key, pattern, ReadStyle, Reference, BULK_THRESHOLD};
use aether_registry::{Overlay, RegError};
use aether_rpc::{Director, Listing, ReadSlot};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

const R0: RootId = RootId(0);

/// A READ's ring context with no bulk arena.
fn inline_slot() -> ReadSlot<'static> {
    ReadSlot {
        slot: 0,
        arena: None,
    }
}

fn open_read(d: &mut Reference, path: &str) -> OpenResp {
    d.open(R0, OpenFlags::READ, path).expect("read open")
}

fn read(d: &mut Reference, fh: u64, offset: u64, len: u32) -> Result<Vec<u8>, Status> {
    match d.read(inline_slot(), RequestFlags::empty(), fh, offset, len)? {
        ReadReply::Inline(b) => Ok(b.to_vec()),
        r => panic!("expected an inline reply, got {r:?}"),
    }
}

fn listing(d: &mut Reference, root: RootId, path: &str) -> Vec<(String, bool, u64)> {
    let mut out = Listing::default();
    d.read_dir(root, path, &mut out).expect("listing");
    out.iter()
        .map(|e| {
            assert_eq!(e.mtime, 0);
            (e.name.to_string(), e.is_dir, e.size)
        })
        .collect()
}

#[test]
fn heartbeat_is_ok() {
    assert_eq!(Reference::new().heartbeat(), Ok(()));
}

#[test]
fn get_attr_answers_files_directories_and_absence_all_ok() {
    let mut d = Reference::new()
        .with("data/a.esp", vec![1; 10], ReadStyle::Whole)
        .with_dir("data")
        .with(&key(1, "data/a.esp"), vec![2; 3], ReadStyle::Whole);
    let file = |size| AttrResp {
        found: true,
        is_dir: false,
        size,
        mtime: 0,
    };
    let dir = AttrResp {
        found: true,
        is_dir: true,
        size: 0,
        mtime: 0,
    };
    let absent = AttrResp {
        found: false,
        is_dir: false,
        size: 0,
        mtime: 0,
    };
    assert_eq!(d.get_attr(R0, "data/a.esp"), Ok(file(10)));
    // Names are folded, as a provider graph looks them up.
    assert_eq!(d.get_attr(R0, "Data/A.ESP"), Ok(file(10)));
    assert_eq!(d.get_attr(R0, "data"), Ok(dir.clone()));
    assert_eq!(
        d.get_attr(R0, "."),
        Ok(dir.clone()),
        "a root is a directory"
    );
    assert_eq!(d.get_attr(R0, "data/."), Ok(dir));
    assert_eq!(d.get_attr(R0, "missing"), Ok(absent.clone()));
    assert_eq!(d.get_attr(RootId(1), "data/a.esp"), Ok(file(3)));
    assert_eq!(d.get_attr(RootId(2), "data/a.esp"), Ok(absent));
}

#[test]
fn read_dir_lists_immediate_children_sorted_whiteouts_included() {
    let mut d = Reference::new()
        .with_dir("data")
        .with_dir("data/sub")
        .with_dir("other")
        .with("data/b.esp", vec![0; 4], ReadStyle::Whole)
        .with("data/a.esp", vec![0; 2], ReadStyle::Whole)
        .with("data/sub/deep.esp", vec![0; 1], ReadStyle::Whole)
        .with("data/gone.esp.__vfs_wh__", Vec::new(), ReadStyle::Whole)
        .with("top.txt", vec![0; 9], ReadStyle::Whole)
        .with(&key(1, "data/x.esp"), vec![0; 5], ReadStyle::Whole);
    let row = |n: &str, d, s| (n.to_string(), d, s);
    assert_eq!(
        listing(&mut d, R0, "data"),
        vec![
            row("a.esp", false, 2),
            row("b.esp", false, 4),
            row("gone.esp.__vfs_wh__", false, 0),
            row("sub", true, 0),
        ]
    );
    assert_eq!(listing(&mut d, R0, "Data/"), listing(&mut d, R0, "data"));
    assert_eq!(
        listing(&mut d, R0, "."),
        vec![
            row("data", true, 0),
            row("other", true, 0),
            row("top.txt", false, 9)
        ]
    );
    assert_eq!(listing(&mut d, R0, ""), listing(&mut d, R0, "."));
    assert_eq!(
        listing(&mut d, RootId(1), "data"),
        vec![row("x.esp", false, 5)]
    );
    assert_eq!(listing(&mut d, R0, "missing"), vec![]);
}

#[test]
fn open_for_read_issues_handles_and_records_the_wire_spelling() {
    let mut d = Reference::new()
        .with("data/a.esp", vec![1; 10], ReadStyle::Whole)
        .with_dir("data");
    let a = d.open(R0, OpenFlags::READ, "Data/A.esp").unwrap();
    assert_eq!(
        a,
        OpenResp {
            fh: 1,
            size: 10,
            is_dir: false,
            ..Default::default()
        }
    );
    let b = d.open(R0, OpenFlags::READ, "data").unwrap();
    assert_eq!(
        b,
        OpenResp {
            fh: 2,
            size: 0,
            is_dir: true,
            ..Default::default()
        }
    );
    assert_eq!(
        d.open(R0, OpenFlags::READ, "missing"),
        Err(Status::NotFound)
    );
    assert_eq!(d.tally.opens("data/a.esp"), 1);
    assert_eq!(d.tally.opens("DATA"), 1);
    assert_eq!(
        d.tally.opens("missing"),
        0,
        "a failed open issues no handle"
    );
    assert_eq!(
        d.tally.wire_opens(),
        vec![
            (OpenFlags::READ.bits(), "Data/A.esp".to_string()),
            (OpenFlags::READ.bits(), "data".to_string()),
            (OpenFlags::READ.bits(), "missing".to_string()),
        ]
    );
}

#[test]
fn open_for_write_answers_as_a_provider_graph_does() {
    let w = OpenFlags::READ | OpenFlags::WRITE;
    let mut d = Reference::new()
        .writable_under("saves/")
        .failing_writes_under("broken/")
        .with("saves/a.sav", vec![1; 5], ReadStyle::Whole)
        .with("data/a.esp", vec![1; 5], ReadStyle::Whole)
        .with("broken/a.sav", vec![1; 5], ReadStyle::Whole);
    // Outside every writable prefix: read-only if served, else not found.
    assert_eq!(d.open(R0, w, "data/a.esp"), Err(Status::ReadOnly));
    assert_eq!(
        d.open(R0, w | OpenFlags::CREATE, "data/new.esp"),
        Err(Status::NotFound)
    );
    // A failing prefix answers I/O error whatever is there.
    assert_eq!(d.open(R0, w, "broken/a.sav"), Err(Status::IoError));
    assert_eq!(d.open(R0, w, "broken/none"), Err(Status::IoError));
    // Inside: the disposition bits.
    assert_eq!(
        d.open(R0, w | OpenFlags::CREATE | OpenFlags::EXCL, "saves/a.sav"),
        Err(Status::Exists)
    );
    assert_eq!(d.open(R0, w, "saves/none.sav"), Err(Status::NotFound));
    let made = d.open(R0, w | OpenFlags::CREATE, "saves/New.sav").unwrap();
    assert_eq!(made.size, 0);
    assert_eq!(d.contents("saves/new.sav"), Some(Vec::new()));
    let kept = d.open(R0, w, "saves/a.sav").unwrap();
    assert_eq!(kept.size, 5);
    let cut = d.open(R0, w | OpenFlags::TRUNC, "saves/a.sav").unwrap();
    assert_eq!(cut.size, 0);
    assert_eq!(d.contents("saves/a.sav"), Some(Vec::new()));
    assert_eq!(d.tally.opens("saves/a.sav"), 2);
    assert_eq!(d.tally.opens("data/a.esp"), 0);
}

#[test]
fn a_write_open_of_a_directory_is_refused_three_ways() {
    let w = OpenFlags::READ | OpenFlags::WRITE;
    let mut d = Reference::new()
        .writable_under("saves")
        .with_dir("saves")
        .with_dir("data");
    assert_eq!(d.open(R0, w, "data"), Err(Status::ReadOnly));
    assert_eq!(
        d.open(R0, w | OpenFlags::CREATE, "saves"),
        Err(Status::IsDir)
    );
    assert_eq!(d.open(R0, w, "saves"), Err(Status::IoError));
}

#[test]
fn read_answers_by_the_files_read_style() {
    let bytes = pattern(1000);
    let mut d = Reference::new()
        .with("whole", bytes.clone(), ReadStyle::Whole)
        .with("short", bytes.clone(), ReadStyle::Short(7))
        .with("error", bytes.clone(), ReadStyle::Error)
        .with("trunc", bytes.clone(), ReadStyle::ShorterThanClaimed(100));
    let whole = open_read(&mut d, "whole").fh;
    assert_eq!(read(&mut d, whole, 0, 4000), Ok(bytes.clone()));
    assert_eq!(read(&mut d, whole, 990, 4000), Ok(bytes[990..].to_vec()));
    assert_eq!(read(&mut d, whole, 2000, 10), Ok(Vec::new()));
    let short = open_read(&mut d, "short").fh;
    assert_eq!(read(&mut d, short, 10, 4000), Ok(bytes[10..17].to_vec()));
    let error = open_read(&mut d, "error");
    assert_eq!(error.size, 1000);
    assert_eq!(read(&mut d, error.fh, 0, 10), Err(Status::IoError));
    let trunc = open_read(&mut d, "trunc");
    assert_eq!(trunc.size, 1000, "OPEN claims the whole file");
    assert_eq!(
        read(&mut d, trunc.fh, 50, 4000),
        Ok(bytes[50..100].to_vec())
    );
    assert_eq!(read(&mut d, trunc.fh, 500, 4000), Ok(Vec::new()));
    // wire.md 3.4.4: an unknown handle is BadFh.
    assert_eq!(read(&mut d, 999, 0, 1), Err(Status::BadFh));
    assert_eq!(d.tally.reads("whole"), 3);
    assert_eq!(d.tally.reads("error"), 1);
    assert_eq!(d.tally.bulk_reads("whole"), 0);
}

#[test]
fn a_read_of_a_file_removed_under_its_handle_is_an_io_error() {
    let mut d = Reference::new()
        .writable_under("")
        .with("a", vec![1; 4], ReadStyle::Whole);
    let fh = open_read(&mut d, "a").fh;
    d.delete(R0, "a").unwrap();
    assert_eq!(read(&mut d, fh, 0, 4), Err(Status::IoError));
}

/// A segment with an arena of two 256 KiB banks at offset 4096.
fn arena_seg() -> OwnedSeg {
    OwnedSeg::new(4096 + 2 * 256 * 1024)
}

#[test]
fn a_bulk_read_fills_the_slots_bank_and_answers_its_offset() {
    let owned = arena_seg();
    let arena = DataArena::new(owned.seg(), 4096, 2 * 256 * 1024, 2);
    let bytes = pattern(300 * 1024);
    let mut d = Reference::new().with("big", bytes.clone(), ReadStyle::Whole);
    let fh = open_read(&mut d, "big").fh;
    let at = |slot| ReadSlot {
        slot,
        arena: Some(&arena),
    };

    // Asked for in the slot flags.
    let r = d
        .read(at(1), RequestFlags::READ_BULK, fh, 10, 1000)
        .unwrap();
    let off = 4096 + 256 * 1024;
    assert_eq!(
        r,
        ReadReply::Bulk {
            len: 1000,
            arena_offset: off as u64
        }
    );
    assert_eq!(
        owned.seg().read_bytes(off, 1000).unwrap(),
        bytes[10..1010].to_vec()
    );

    // At or above the threshold without the flag; capped at the bank size.
    let r = d
        .read(at(0), RequestFlags::empty(), fh, 0, BULK_THRESHOLD * 8)
        .unwrap();
    assert_eq!(
        r,
        ReadReply::Bulk {
            len: 256 * 1024,
            arena_offset: 4096
        }
    );
    assert_eq!(
        owned.seg().read_bytes(4096, 256 * 1024).unwrap(),
        bytes[..256 * 1024].to_vec()
    );

    // Below the threshold and unflagged: inline.
    let r = d
        .read(at(0), RequestFlags::empty(), fh, 0, BULK_THRESHOLD - 1)
        .unwrap();
    assert!(matches!(r, ReadReply::Inline(b) if b == &bytes[..(BULK_THRESHOLD - 1) as usize]));

    // With no arena, a bulk request is answered inline.
    let r = d
        .read(inline_slot(), RequestFlags::READ_BULK, fh, 0, 5)
        .unwrap();
    assert_eq!(r, ReadReply::Inline(&bytes[..5]));

    assert_eq!(d.tally.reads("big"), 4);
    assert_eq!(d.tally.bulk_reads("big"), 2);
}

#[test]
fn write_lands_at_its_offset_and_answers_its_length() {
    let mut d = Reference::new()
        .writable_under("")
        .with("a", b"hello".to_vec(), ReadStyle::Whole);
    let fh = d
        .open(R0, OpenFlags::READ | OpenFlags::WRITE, "a")
        .unwrap()
        .fh;
    assert_eq!(d.write(fh, 1, b"EL"), Ok(2));
    assert_eq!(d.contents("a"), Some(b"hELlo".to_vec()));
    assert_eq!(d.write(fh, 8, b"!"), Ok(1));
    assert_eq!(d.contents("A"), Some(b"hELlo\0\0\0!".to_vec()));
    assert_eq!(d.write(999, 0, b"x"), Err(Status::BadFh));
    assert_eq!(d.tally.writes("a"), 2);
}

#[test]
fn set_attr_resizes_the_handles_file() {
    let mut d = Reference::new()
        .writable_under("")
        .with("a", b"hello".to_vec(), ReadStyle::Whole);
    let fh = open_read(&mut d, "a").fh;
    assert_eq!(d.set_attr(fh, 2), Ok(()));
    assert_eq!(d.contents("a"), Some(b"he".to_vec()));
    assert_eq!(d.set_attr(fh, 4), Ok(()));
    assert_eq!(d.contents("a"), Some(b"he\0\0".to_vec()));
    assert_eq!(d.set_attr(999, 0), Err(Status::BadFh));
    d.delete(R0, "a").unwrap();
    assert_eq!(d.set_attr(fh, 0), Err(Status::NotFound));
}

#[test]
fn delete_refuses_what_is_absent_or_read_only() {
    let mut d = Reference::new()
        .writable_under("saves/")
        .with("saves/a", vec![1], ReadStyle::Whole)
        .with("data/a", vec![1], ReadStyle::Whole);
    assert_eq!(d.delete(R0, "missing"), Err(Status::NotFound));
    assert_eq!(d.delete(R0, "data/a"), Err(Status::ReadOnly));
    assert_eq!(d.delete(R0, "Saves/A"), Ok(()));
    assert_eq!(d.contents("saves/a"), None);
    assert_eq!(d.delete(R0, "saves/a"), Err(Status::NotFound));
    assert_eq!(d.tally.deletes("saves/a"), 2);
    assert_eq!(d.tally.deletes("data/a"), 1);
    assert_eq!(d.tally.deletes("missing"), 1);
}

#[test]
fn rename_moves_within_writable_prefixes_only() {
    let mut d = Reference::new()
        .writable_under("saves/")
        .with("saves/a", vec![1], ReadStyle::Whole)
        .with("data/b", vec![2], ReadStyle::Whole);
    assert_eq!(d.rename(R0, "missing", "saves/x"), Err(Status::NotFound));
    assert_eq!(d.rename(R0, "saves/a", "data/a"), Err(Status::ReadOnly));
    assert_eq!(d.rename(R0, "data/b", "saves/b"), Err(Status::ReadOnly));
    assert_eq!(d.rename(R0, "saves/a", "Saves/C"), Ok(()));
    assert_eq!(d.contents("saves/a"), None);
    assert_eq!(d.contents("saves/c"), Some(vec![1]));
    assert_eq!(d.tally.renames("saves/a"), 2, "counted against the source");
    assert_eq!(d.tally.renames("saves/c"), 0);
    let mut d = Reference::new()
        .writable_under("")
        .with(&key(1, "a"), vec![3], ReadStyle::Whole);
    assert_eq!(d.rename(RootId(1), "a", "b"), Ok(()));
    assert_eq!(d.contents(&key(1, "b")), Some(vec![3]));
}

#[test]
fn close_releases_the_handle_and_refuses_an_unknown_one() {
    let mut d = Reference::new().with("a", vec![1], ReadStyle::Whole);
    let fh = open_read(&mut d, "a").fh;
    assert_eq!(d.close(fh), Ok(()));
    // wire.md 3.4.10: unknown fh is BadFh.
    assert_eq!(d.close(fh), Err(Status::BadFh));
    assert_eq!(d.close(999), Err(Status::BadFh));
    assert_eq!(d.tally.closes("a"), 1);
    assert_eq!(read(&mut d, fh, 0, 1), Err(Status::BadFh));
}

#[test]
fn the_ops_it_does_not_serve_are_not_supported() {
    let mut d = Reference::new();
    assert_eq!(d.mkdir(R0, 0o755, "dir"), Err(Status::NotSupported));
    assert_eq!(d.stored_names(R0, 0, "a"), Err(Status::NotSupported));
    assert_eq!(d.rename_x(R0, R0, "a", "b"), Err(Status::NotSupported));
    assert_eq!(d.stage_exec(R0, "a.exe"), Err(Status::NotSupported));
    // wire.md 3.4.14: a director that stages nothing says so first.
    assert_eq!(
        d.stage_exec(RootId(9), "../x.exe"),
        Err(Status::NotSupported)
    );
}

#[test]
fn opcodes_outside_the_table_are_bad_requests() {
    // wire.md 3.3: any other opcode is ST_BAD_REQUEST.
    let mut d = Reference::new();
    for op in [0, 4, 12, 25, 1000, u32::MAX] {
        assert_eq!(d.unknown(op), Status::BadRequest, "opcode {op}");
    }
}

#[test]
fn an_undecodable_request_is_a_bad_request() {
    // wire.md 3.5 and 3.5.1: a payload that does not decode is ST_BAD_REQUEST.
    for registry in [false, true] {
        let mut d = Reference::new();
        if registry {
            d = d.with_registry();
        }
        for &op in Opcode::ALL {
            assert_eq!(d.undecodable(op, 0, &[]), Status::BadRequest, "{op:?}");
        }
    }
    let mut d = Reference::new().with_registry();
    let path = b"\\Registry\\A";
    let mut body = (path.len() as u32).to_le_bytes().to_vec();
    body.extend_from_slice(path);
    body.push(0xFF);
    assert_eq!(d.undecodable(Opcode::RegKey, 0, &body), Status::BadRequest);
    // The leading path still counts the request, as it did.
    assert_eq!(d.tally.reg(Opcode::RegKey, "\\Registry\\A"), 1);
    d.detach_registry();
    assert_eq!(d.undecodable(Opcode::RegKey, 0, &body), Status::BadRequest);
    assert_eq!(d.tally.reg(Opcode::RegKey, "\\Registry\\A"), 2);
}

#[test]
fn shared_handles_see_one_graph_and_one_tally() {
    let mut a = Reference::new()
        .writable_under("")
        .with("f", vec![1], ReadStyle::Whole);
    let mut b = a.share();
    let fh = open_read(&mut a, "f").fh;
    assert_eq!(read(&mut b, fh, 0, 1), Ok(vec![1]));
    b.delete(R0, "f").unwrap();
    assert_eq!(a.contents("f"), None);
    assert_eq!(a.tally.opens("f"), 1);
    assert_eq!(a.tally.deletes("f"), 1);
}

// The registry layer.

const K: &str = "\\Registry\\Machine\\Software\\Aether";

#[test]
fn without_a_registry_layer_every_registry_op_is_not_supported() {
    let mut d = Reference::new();
    let ns = Err(Status::NotSupported);
    assert_eq!(d.reg_lookup(K).map(|_| ()), ns);
    assert_eq!(d.reg_key(K).map(|_| ()), ns);
    assert_eq!(d.reg_set_value(K, "v", 1, b"x").map(|_| ()), ns);
    assert_eq!(d.reg_delete_value(K, "v").map(|_| ()), ns);
    assert_eq!(d.reg_create_key(K, false).map(|_| ()), ns);
    assert_eq!(d.reg_delete_key(K).map(|_| ()), ns);
    assert_eq!(d.reg_rename_key(K, "B").map(|_| ()), ns);
    assert_eq!(d.reg_changed(K, true, 0).map(|_| ()), ns);
}

/// The reference against an overlay given the same writes: what it answers
/// is the overlay's own answer.
#[test]
fn registry_ops_answer_from_the_overlay() {
    let mut d = Reference::new().with_registry();
    let mut o = Overlay::new();
    let gens = Arc::new(Mutex::new(Vec::new()));
    let seen = gens.clone();
    d.publish_generation(move |g| seen.lock().unwrap().push(g));
    assert_eq!(*gens.lock().unwrap(), vec![1], "published when attached");

    let (state, below) = o.lookup(K);
    assert_eq!(
        d.reg_lookup(K),
        Ok(LookupReply {
            version: o.version(),
            state,
            below
        })
    );
    assert_eq!(state, Lookup::Absent);

    let v = o.create_key(K, true, false, 0).unwrap();
    assert_eq!(d.reg_create_key(K, true), Ok(v));
    assert_eq!(d.reg_create_key(K, true), Err(Status::Exists));
    let v = o.set_value(K, "v", 3, b"data", 0).unwrap();
    assert_eq!(d.reg_set_value(K, "v", 3, b"data"), Ok(v));
    let (state, below) = o.lookup(K);
    assert_eq!(
        d.reg_lookup(K),
        Ok(LookupReply {
            version: o.version(),
            state,
            below
        })
    );
    assert_eq!(state, Lookup::Present { created: true });

    // REG_KEY carries the node; its last-write time is the clock's, so it
    // is compared without it.
    let KeyReply { version, node } = d.reg_key(K).unwrap();
    assert_eq!(version, o.version());
    let mut node = node.expect("a node").into_owned();
    let mut want = o.node(K).unwrap().clone();
    node.last_write = 0;
    want.last_write = 0;
    assert_eq!(node, want);
    assert_eq!(
        d.reg_key("\\Registry\\Machine\\None"),
        Ok(KeyReply {
            version: o.version(),
            node: None
        })
    );

    assert_eq!(
        d.reg_changed(K, false, 0),
        Ok(ChangedReply {
            version: o.version(),
            changed: o.changed_since(K, false, 0)
        })
    );
    assert_eq!(
        d.reg_changed(K, true, o.version()),
        Ok(ChangedReply {
            version: o.version(),
            changed: false
        })
    );

    let v = o.delete_value(K, "v", 0).unwrap();
    assert_eq!(d.reg_delete_value(K, "v"), Ok(v));
    let v = o.rename_key(K, "Renamed", 0).unwrap();
    assert_eq!(d.reg_rename_key(K, "Renamed"), Ok(v));
    let k2 = "\\Registry\\Machine\\Software\\Renamed";
    let v = o.delete_key(k2, 0).unwrap();
    assert_eq!(d.reg_delete_key(k2), Ok(v));
    let e = o.delete_key(k2, 0).unwrap_err();
    assert_eq!(e, RegError::NotFound);
    assert_eq!(d.reg_delete_key(k2), Err(Status::NotFound));

    // Every successful write moved the generation, and nothing else did.
    assert_eq!(*gens.lock().unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(d.tally.reg(Opcode::RegLookup, K), 2);
    assert_eq!(d.tally.reg(Opcode::RegCreateKey, K), 2);
    assert_eq!(d.tally.reg(Opcode::RegDeleteKey, k2), 2);
    assert_eq!(d.tally.reg(Opcode::RegKey, K), 1);
}

#[test]
fn a_registry_path_that_is_not_canonical_is_a_bad_request() {
    let mut d = Reference::new().with_registry();
    for p in [
        "Registry\\Machine",
        "\\Registry\\Machine\\",
        "\\Registry\\\\Machine",
        "\\Registry\\Machine\\.",
        "\\Registry\\Machine\\..\\User",
    ] {
        assert_eq!(d.reg_lookup(p), Err(Status::BadRequest), "{p}");
        assert_eq!(d.reg_key(p).map(|_| ()), Err(Status::BadRequest), "{p}");
        assert_eq!(d.reg_changed(p, false, 0), Err(Status::BadRequest), "{p}");
        assert_eq!(d.reg_set_value(p, "v", 1, b""), Err(Status::BadRequest));
        assert_eq!(d.reg_delete_value(p, "v"), Err(Status::BadRequest));
        assert_eq!(d.reg_create_key(p, false), Err(Status::BadRequest));
        assert_eq!(d.reg_delete_key(p), Err(Status::BadRequest));
        assert_eq!(d.reg_rename_key(p, "x"), Err(Status::BadRequest));
    }
    d.reg_create_key(K, false).unwrap();
    assert_eq!(d.reg_rename_key(K, "."), Err(Status::BadRequest));
    assert_eq!(d.reg_rename_key(K, ".."), Err(Status::BadRequest));
}

#[test]
fn overlay_errors_map_to_their_statuses() {
    let mut d = Reference::new().with_registry();
    let mut o = Overlay::new();
    let long = format!("\\Registry\\Machine\\{}", "x".repeat(300));
    assert_eq!(
        o.create_key(&long, false, false, 0),
        Err(RegError::NameTooLong)
    );
    assert_eq!(d.reg_create_key(&long, false), Err(Status::BadRequest));
    let absent = "\\Registry\\Machine\\Absent";
    assert_eq!(o.rename_key(absent, "B", 0), Err(RegError::NotFound));
    assert_eq!(d.reg_rename_key(absent, "B"), Err(Status::NotFound));
    let v = o.create_key(K, false, false, 0).unwrap();
    assert_eq!(d.reg_create_key(K, false), Ok(v));
    assert_eq!(
        o.create_key(K, false, false, 0),
        Err(RegError::AlreadyExists)
    );
    assert_eq!(d.reg_create_key(K, false), Err(Status::Exists));
}

#[test]
fn a_detached_registry_is_not_supported_and_keeps_its_overlay() {
    let mut d = Reference::new().with_registry();
    let gens = Arc::new(Mutex::new(Vec::new()));
    let seen = gens.clone();
    d.publish_generation(move |g| seen.lock().unwrap().push(g));
    let v = d.reg_create_key(K, false).unwrap();
    d.detach_registry();
    assert_eq!(d.reg_lookup(K), Err(Status::NotSupported));
    assert_eq!(d.reg_create_key(K, false), Err(Status::NotSupported));
    d.attach_registry();
    assert_eq!(
        d.reg_lookup(K).map(|r| (r.version, r.state)),
        Ok((v, Lookup::Present { created: true }))
    );
    assert_eq!(*gens.lock().unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(
        d.tally.reg(Opcode::RegLookup, K),
        2,
        "counted while detached"
    );
}

#[test]
fn reg_key_reply_borrows_nothing_it_does_not_own() {
    // The node comes back owned: the overlay is behind a lock.
    let mut d = Reference::new().with_registry();
    d.reg_create_key(K, false).unwrap();
    let r = d.reg_key(K).unwrap();
    assert!(matches!(r.node, Some(Cow::Owned(_))));
}

#[test]
fn the_pattern_is_fixed() {
    assert_eq!(pattern(4), vec![0x42, 0xE8, 0x3D, 0x63]);
    assert_eq!(pattern(1000).len(), 1000);
    let entry = DirEntry {
        name: "x",
        is_dir: false,
        size: 0,
        mtime: 0,
    };
    let mut l = Listing::default();
    l.push(entry);
    assert_eq!(l.iter().collect::<Vec<_>>(), vec![entry]);
}
