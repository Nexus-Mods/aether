//! The spelling cache (`FuseClient::names`, an
//! `aether_redirect::finalname::NameCache`) request by request: a name
//! query whose components are all known costs no round trip, one under a
//! known directory asks STORED_NAMES only for what it does not know
//! (`skip`), and nothing is learned from a reply that is not an answer.
//!
//! The rule, as `FuseClient::stored_names` and `names_changed` state it:
//! entries are kept per root and folded path, used for `NAME_TTL_MS`, and
//! dropped at and below a path this process creates, renames or deletes. A
//! reply with one name per component asked about is remembered; any other
//! reply, and a failed request, leaves the caller's spelling and caches
//! nothing.
//!
//! The tree: `Mods/Sub/F.esp` and `Mods/G.esp`, stored in that case, under
//! a writable `mods/`. The tests open them in other cases, so the answer
//! shows which spelling the shim used.
#![allow(unsafe_code)] // Win32 calls

use crate::fakedirector::{pattern, Fake, Fault, Match, ReadStyle, Served};
use crate::fileops::{close, create, names, open_read, serve, tail, wide};
use aether_protocol::{Opcode, Request, Status};
use aether_refdirector::harness::{drop_last_name, Mark};
use std::path::Path;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::{DeleteFileW, MoveFileExW, CREATE_ALWAYS};

fn fake() -> Fake {
    Fake::new()
        .with_dir("Mods")
        .with_dir("Mods/Sub")
        .with("Mods/Sub/F.esp", pattern(64), ReadStyle::Whole)
        .with("Mods/G.esp", pattern(64), ReadStyle::Whole)
        .writable_under("mods/")
}

/// Every request since `mark`, which must all be STORED_NAMES, as
/// `(skip, path)`.
fn asked(served: &Served, mark: Mark) -> Vec<(u32, String)> {
    let seen = served.recorder.since(mark);
    seen.iter()
        .map(|r| match r.request() {
            Some(Request::StoredNames { skip, path, .. }) => (skip, path.to_string()),
            other => panic!("not a STORED_NAMES: {other:?} in {seen:#?}"),
        })
        .collect()
}

fn sn(skip: u32, path: &str) -> (u32, String) {
    (skip, path.to_string())
}

/// `tail(root, h)` and the STORED_NAMES it sent.
fn query(
    served: &Served,
    root: &Path,
    h: windows_sys::Win32::Foundation::HANDLE,
) -> (String, Vec<(u32, String)>) {
    let mark = served.recorder.mark();
    let t = tail(root, h);
    (t, asked(served, mark))
}

fn file(root: &Path) -> std::path::PathBuf {
    root.join("mods").join("SUB").join("f.ESP")
}

#[test]
fn a_repeated_final_path_query_costs_one_stored_names_then_none() {
    isolate!();
    let (root, served) = serve(fake());
    let h = open_read(&file(&root));
    assert_eq!(
        query(served, &root, h),
        (r"\Mods\Sub\F.esp".into(), vec![sn(0, "mods/sub/f.esp")])
    );
    for _ in 0..3 {
        assert_eq!(query(served, &root, h), (r"\Mods\Sub\F.esp".into(), vec![]));
    }
    close(h);
}

#[test]
fn a_file_under_a_known_directory_asks_only_for_its_last_component() {
    isolate!();
    let (root, served) = serve(fake());
    let d = open_read(&root.join("MODS").join("sub"));
    assert_eq!(
        query(served, &root, d),
        (r"\Mods\Sub".into(), vec![sn(0, "mods/sub")])
    );
    let h = open_read(&file(&root));
    assert_eq!(
        query(served, &root, h),
        (r"\Mods\Sub\F.esp".into(), vec![sn(2, "mods/sub/f.esp")]),
        "skip is the known depth"
    );
    // A sibling of the directory: only `mods` is known.
    let g = open_read(&root.join("mods").join("g.ESP"));
    assert_eq!(
        query(served, &root, g),
        (r"\Mods\G.esp".into(), vec![sn(1, "mods/g.esp")])
    );
    for h in [d, h, g] {
        close(h);
    }
}

#[test]
fn a_spelling_is_asked_again_after_its_ttl() {
    isolate!();
    let (root, served) = serve(fake());
    let h = open_read(&file(&root));
    assert_eq!(query(served, &root, h).1, [sn(0, "mods/sub/f.esp")]);
    assert_eq!(query(served, &root, h).1, []);

    aether_hooks::director::advance_name_clock(aether_hooks::director::NAME_TTL_MS_FOR_TESTS);
    assert_eq!(
        query(served, &root, h),
        (r"\Mods\Sub\F.esp".into(), vec![sn(0, "mods/sub/f.esp")]),
        "every component is stale, so all are asked for"
    );
    assert_eq!(query(served, &root, h).1, [], "and remembered again");
    close(h);
}

#[test]
fn creating_renaming_or_deleting_through_this_process_forgets_the_path_and_below() {
    isolate!();
    let (root, served) = serve(fake());
    let f = open_read(&file(&root));
    let g_path = root.join("mods").join("g.ESP");
    let g = open_read(&g_path);
    assert_eq!(query(served, &root, f).1, [sn(0, "mods/sub/f.esp")]);
    assert_eq!(query(served, &root, g).1, [sn(1, "mods/g.esp")]);
    assert_eq!(query(served, &root, f).1, []);
    assert_eq!(query(served, &root, g).1, []);

    // A create (an overwrite is one) forgets that path only.
    let w = create(&g_path, GENERIC_WRITE, CREATE_ALWAYS, 0).expect("overwrite G.esp");
    close(w);
    assert_eq!(
        query(served, &root, g).1,
        [sn(1, "mods/g.esp")],
        "after the create"
    );
    assert_eq!(
        query(served, &root, f).1,
        [],
        "a sibling's directory is kept"
    );

    // A delete forgets that path only.
    let wg = wide(&g_path);
    // SAFETY: a NUL-terminated name.
    assert_ne!(unsafe { DeleteFileW(wg.as_ptr()) }, 0, "DeleteFileW");
    assert_eq!(
        query(served, &root, g).1,
        [sn(1, "mods/g.esp")],
        "after the delete"
    );
    assert_eq!(query(served, &root, f).1, []);

    // A directory rename forgets it and everything below it, at both names.
    let from = wide(&root.join("mods").join("sub"));
    let to = wide(&root.join("mods").join("Sub2"));
    // SAFETY: NUL-terminated names.
    assert_ne!(
        unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) },
        0,
        "MoveFileExW"
    );
    assert_eq!(
        query(served, &root, f).1,
        [sn(1, "mods/sub/f.esp")],
        "`mods` kept, `mods/sub` and below forgotten"
    );
    assert_eq!(names(&root.join("mods")), ["Sub2"]);
    close(f);
    close(g);
}

/// Arm `fault` on every STORED_NAMES, query `h` once, and check that the
/// caller's spelling came back and that every name query the shim answered
/// within that one call crossed (none was answered from what an earlier
/// one in the call taught). Then disarm, and check the next query crosses
/// once and is remembered: the faulted replies taught nothing.
fn assert_teaches_nothing(fault: Fault) {
    let (root, served) = serve(fake());
    let id = served.faults.arm(
        Match {
            op: Some(Opcode::StoredNames),
            ..Match::default()
        },
        fault,
    );
    let h = open_read(&file(&root));
    for n in 1..=2 {
        let (spelled, sent) = query(served, &root, h);
        assert_eq!(
            spelled, r"\mods\SUB\f.ESP",
            "query {n}: the caller's spelling"
        );
        // `GetFinalPathNameByHandleW` asks the shim for the name more than
        // once (`NtQueryObject`, `FileNameInformation`); each asks again.
        assert!(!sent.is_empty(), "query {n}");
        assert!(
            sent.iter().all(|s| *s == sn(0, "mods/sub/f.esp")),
            "query {n}: {sent:?}"
        );
        assert_eq!(
            served.faults.applied(id) as usize,
            sent.len() * n,
            "query {n}"
        );
    }
    served.faults.clear();
    assert_eq!(
        query(served, &root, h),
        (r"\Mods\Sub\F.esp".into(), vec![sn(0, "mods/sub/f.esp")]),
        "nothing was remembered, so the next query crosses"
    );
    assert_eq!(query(served, &root, h).1, []);
    close(h);
}

#[test]
fn a_reply_with_the_wrong_number_of_names_teaches_nothing() {
    isolate!();
    // Two names for three components.
    assert_teaches_nothing(Fault::Rewrite(drop_last_name));
}

#[test]
fn a_failed_stored_names_leaves_the_caller_s_spelling_and_caches_nothing() {
    isolate!();
    assert_teaches_nothing(Fault::Status(Status::IoError));
}

/// `docs/wire.md` 3.4.12: `ST_NOT_FOUND` is a director's answer for a path it does not hold.
#[test]
fn stored_names_not_found_falls_back_and_teaches_nothing() {
    isolate!();
    assert_teaches_nothing(Fault::Status(Status::NotFound));
}

/// A director that serves a writable root as an overlay keeps a
/// lower-only name's spelling across a case-only rename (`docs/wire.md`
/// 3.4.9, "Names only in a read-only lower").
#[test]
fn a_case_only_rename_of_a_lower_only_name_keeps_the_lower_s_spelling() {
    isolate!();
    let (root, served) = serve(
        Fake::new()
            .with_dir("Data")
            .with_lower("Data/Lower.esp", pattern(64))
            .writable_under("data/"),
    );
    let data = root.join("Data");
    let h = open_read(&data.join("lower.esp"));
    assert_eq!(
        query(served, &root, h),
        (r"\Data\Lower.esp".into(), vec![sn(0, "data/lower.esp")])
    );
    assert_eq!(query(served, &root, h).1, []);

    let from = wide(&data.join("Lower.esp"));
    let to = wide(&data.join("LOWER.esp"));
    // SAFETY: NUL-terminated names.
    assert_ne!(
        unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) },
        0,
        "MoveFileExW: {}",
        crate::fileops::last_error()
    );
    assert_eq!(
        query(served, &root, h),
        (r"\Data\Lower.esp".into(), vec![sn(1, "data/lower.esp")]),
        "the rename dropped the name, and the director still spells it Lower.esp"
    );
    assert_eq!(names(&data), ["Lower.esp"]);
    close(h);
}

/// A spelling-cache hit allocates nothing: what `final_path` asks the cache
/// (`FuseClient::with_stored_names`) is answered from it, borrowed, with no
/// request and no allocation. `final_path`'s own work (splitting the path,
/// building its answer) is not the cache's and is not counted here.
#[test]
fn a_cached_spelling_allocates_nothing() {
    isolate!();
    let (root, served) = serve(fake());
    let h = open_read(&file(&root));
    assert_eq!(query(served, &root, h).1, [sn(0, "mods/sub/f.esp")]);
    let client = aether_hooks::director::global().expect("attached");
    let under: Vec<String> = ["mods", "sub", "f.esp"].map(String::from).to_vec();
    let mark = served.recorder.mark();
    let (spelled, n) =
        aether_testkit::alloc::allocations(|| client.spelled_length_for_tests(0, &under));
    assert_eq!(
        spelled,
        "ModsSubF.esp".len(),
        "every component from the cache"
    );
    assert_eq!(n, 0, "allocations on a hit");
    assert_eq!(served.recorder.total_since(mark), 0);
    close(h);
}
