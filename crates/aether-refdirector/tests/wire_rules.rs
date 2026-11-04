//! Where `Reference` answers as `docs/wire.md` says a director does rather
//! than as the shim tests' fake did: paths that leave their root, roots the
//! director does not serve, the fold, directory handles, append handles, and
//! writes it cannot hold. Each test names the section it follows.

use aether_protocol::{AttrResp, OpenFlags, ReadReply, RequestFlags, RootId, Status};
use aether_refdirector::{key, ReadStyle, Reference, MAX_FILE_LEN};
use aether_rpc::{Director, Listing, ReadSlot};

const R0: RootId = RootId(0);
const RW: OpenFlags = OpenFlags::READ.union(OpenFlags::WRITE);

fn inline_slot() -> ReadSlot<'static> {
    ReadSlot {
        slot: 0,
        arena: None,
        inline_cap: usize::MAX,
    }
}

const ABSENT: AttrResp = AttrResp {
    found: false,
    is_dir: false,
    size: 0,
    mtime: 0,
};

/// wire.md 3.2: "a path that would leave its root (`..` above the root)" is
/// an error; 3.5.1: a new director answers it Status::BadRequest.
#[test]
fn a_path_that_leaves_its_root_is_a_bad_request() {
    let mut d = Reference::new()
        .writable_under("")
        .with("a", vec![1], ReadStyle::Whole)
        .with_dir("data");
    for p in ["..", "../a", "data/../../a", "data/../.."] {
        let bad = Err(Status::BadRequest);
        assert_eq!(d.get_attr(R0, p), bad, "{p}");
        assert_eq!(
            d.read_dir(R0, p, &mut Listing::default()),
            bad.map(|_: AttrResp| ())
        );
        assert_eq!(
            d.open(R0, OpenFlags::READ, p).map(|_| ()),
            Err(Status::BadRequest)
        );
        assert_eq!(
            d.open(R0, RW | OpenFlags::CREATE, p).map(|_| ()),
            Err(Status::BadRequest)
        );
        assert_eq!(d.delete(R0, p), Err(Status::BadRequest), "{p}");
        assert_eq!(d.rename(R0, "a", p), Err(Status::BadRequest), "{p}");
        assert_eq!(d.rename(R0, p, "b"), Err(Status::BadRequest), "{p}");
        assert_eq!(d.mkdir(R0, 0o755, p), Err(Status::BadRequest), "{p}");
        assert_eq!(d.stored_names(R0, 0, p), Err(Status::BadRequest), "{p}");
        assert_eq!(d.rename_x(R0, R0, p, "b"), Err(Status::BadRequest), "{p}");
    }
    // `..` that stays inside the root is not an escape.
    assert_eq!(d.get_attr(R0, "data/../a").map(|a| a.found), Ok(false));
    assert_eq!(
        d.contents("a"),
        Some(vec![1]),
        "nothing was deleted or moved"
    );
}

/// A root is served when the reference was built with something under it,
/// or named with `with_root`; root 0 always is.
#[test]
fn an_unknown_root_is_not_found_and_getattr_finds_nothing() {
    let mut d = Reference::new()
        .writable_under("")
        .with(&key(1, "a"), vec![1], ReadStyle::Whole)
        .with_root(3);
    let r9 = RootId(9);
    // 3.4.1: GETATTR on a root the director does not serve is OK, found 0.
    assert_eq!(d.get_attr(r9, "a"), Ok(ABSENT));
    // 3.4.2, 3.4.3, 3.4.8: unknown root Status::NotFound.
    assert_eq!(
        d.read_dir(r9, ".", &mut Listing::default()),
        Err(Status::NotFound)
    );
    assert_eq!(
        d.open(r9, OpenFlags::READ, "a").map(|_| ()),
        Err(Status::NotFound)
    );
    assert_eq!(
        d.open(r9, RW | OpenFlags::CREATE, "a").map(|_| ()),
        Err(Status::NotFound),
        "a writable prefix of every root does not serve an unknown one"
    );
    assert_eq!(d.delete(r9, "a"), Err(Status::NotFound));
    // 3.5.1: a new director answers an unknown root Status::NotFound.
    assert_eq!(d.rename(r9, "a", "b"), Err(Status::NotFound));
    assert_eq!(d.mkdir(r9, 0o755, "a"), Err(Status::NotFound));
    assert_eq!(d.stored_names(r9, 0, "a"), Err(Status::NotFound));
    assert_eq!(d.rename_x(RootId(1), r9, "a", "b"), Err(Status::NotFound));
    assert_eq!(d.rename_x(r9, RootId(1), "a", "b"), Err(Status::NotFound));
    // Served roots.
    let mut l = Listing::default();
    assert_eq!(d.read_dir(RootId(3), ".", &mut l), Ok(()));
    assert!(l.is_empty());
    assert_eq!(d.read_dir(R0, ".", &mut Listing::default()), Ok(()));
    assert!(d.get_attr(RootId(1), "a").unwrap().found);
    let made = d.open(RootId(3), RW | OpenFlags::CREATE, "new").unwrap();
    assert_eq!(made.size, 0);
    assert_eq!(d.contents(&key(3, "new")), Some(Vec::new()));
}

/// 3.2, "Case folding": names match exactly when their folds are equal
/// (`aether_casefold::fold`), not by ASCII lowercase alone.
#[test]
fn names_match_by_the_wires_fold() {
    let mut d =
        Reference::new()
            .writable_under("")
            .with("Données/Été.esp", vec![7; 3], ReadStyle::Whole);
    assert!(d.get_attr(R0, "données/été.esp").unwrap().found);
    assert!(d.get_attr(R0, "DONNÉES/ÉTÉ.ESP").unwrap().found);
    assert_eq!(d.contents("données/été.esp"), Some(vec![7; 3]));
    let fh = d.open(R0, OpenFlags::READ, "données/été.esp").unwrap().fh;
    assert_eq!(d.tally.opens("DONNÉES/ÉTÉ.esp"), 1);
    d.close(fh).unwrap();
}

/// 3.4.3: OpenFlags::WRITE on a root with no writable provider is Status::ReadOnly,
/// whether or not the path exists; a missing path on a root that has one,
/// but not over this path, stays Status::NotFound.
#[test]
fn a_write_open_on_a_root_with_no_writable_provider_is_read_only() {
    let mut d = Reference::new()
        .with("a", vec![1], ReadStyle::Whole)
        .with(&key(1, "b"), vec![1], ReadStyle::Whole)
        .writable_under(&key(1, "saves/"));
    assert_eq!(d.open(R0, RW, "a").map(|_| ()), Err(Status::ReadOnly));
    assert_eq!(
        d.open(R0, RW | OpenFlags::CREATE, "new").map(|_| ()),
        Err(Status::ReadOnly)
    );
    assert_eq!(
        d.open(R0, RW | OpenFlags::CREATE | OpenFlags::EXCL, "a")
            .map(|_| ()),
        Err(Status::ReadOnly)
    );
    let r1 = RootId(1);
    assert_eq!(d.open(r1, RW, "b").map(|_| ()), Err(Status::ReadOnly));
    assert_eq!(
        d.open(r1, RW | OpenFlags::CREATE, "new").map(|_| ()),
        Err(Status::NotFound)
    );
    assert!(d.open(r1, RW | OpenFlags::CREATE, "saves/new").is_ok());
}

/// 3.4.3: on an OpenFlags::APPEND handle the director ignores WRITE offsets.
#[test]
fn writes_through_an_append_handle_go_to_the_end() {
    let mut d = Reference::new()
        .writable_under("")
        .with("log", b"abc".to_vec(), ReadStyle::Whole);
    let fh = d.open(R0, RW | OpenFlags::APPEND, "log").unwrap().fh;
    assert_eq!(d.write(fh, 0, b"de"), Ok(2));
    assert_eq!(d.write(fh, u64::MAX, b"f"), Ok(1));
    assert_eq!(d.contents("log"), Some(b"abcdef".to_vec()));
    let plain = d.open(R0, RW, "log").unwrap().fh;
    assert_eq!(d.write(plain, 0, b"X"), Ok(1));
    assert_eq!(d.contents("log"), Some(b"Xbcdef".to_vec()));
}

/// 3.4.4: READ of a directory handle is Status::IsDir; 3.5: Status::IsDir is "a file
/// operation on a directory handle", so WRITE and SETATTR answer it too.
#[test]
fn file_operations_on_a_directory_handle_are_is_dir() {
    let mut d = Reference::new().writable_under("").with_dir("data");
    let fh = d.open(R0, OpenFlags::READ, "data").unwrap().fh;
    let root = d.open(R0, OpenFlags::READ, ".").unwrap().fh;
    for fh in [fh, root] {
        assert_eq!(
            d.read(inline_slot(), RequestFlags::empty(), fh, 0, 4)
                .map(|_| ()),
            Err(Status::IsDir)
        );
        assert_eq!(d.write(fh, 0, b"x"), Err(Status::IsDir));
        assert_eq!(d.set_attr(fh, 0), Err(Status::IsDir));
        assert_eq!(d.close(fh), Ok(()));
    }
    assert_eq!(d.tally.reads("data"), 1);
}

/// 3.5: Status::NoSpace, "storage full", for a write or a length past what the
/// reference holds ([`MAX_FILE_LEN`]), arithmetic overflow included. The
/// file is left as it was and the reference keeps answering.
#[test]
fn a_write_or_length_past_what_it_holds_is_no_space() {
    let mut d = Reference::new()
        .writable_under("")
        .with("a", b"keep".to_vec(), ReadStyle::Whole);
    let fh = d.open(R0, RW, "a").unwrap().fh;
    let cap = MAX_FILE_LEN as u64;
    assert_eq!(d.write(fh, u64::MAX, b"x"), Err(Status::NoSpace));
    assert_eq!(d.write(fh, u64::MAX - 1, b"xyz"), Err(Status::NoSpace));
    assert_eq!(d.write(fh, cap, b"x"), Err(Status::NoSpace));
    assert_eq!(d.write(fh, cap + 1_000_000, b""), Err(Status::NoSpace));
    assert_eq!(d.set_attr(fh, u64::MAX), Err(Status::NoSpace));
    assert_eq!(d.set_attr(fh, cap + 1), Err(Status::NoSpace));
    assert_eq!(d.contents("a"), Some(b"keep".to_vec()));
    // The last byte the cap allows is accepted, and the table still answers.
    assert_eq!(d.write(fh, cap - 1, b"z"), Ok(1));
    assert_eq!(d.contents("a").map(|b| b.len()), Some(MAX_FILE_LEN));
    assert_eq!(d.set_attr(fh, 2), Ok(()));
    assert_eq!(d.contents("a"), Some(b"ke".to_vec()));
    let r = d
        .read(inline_slot(), RequestFlags::empty(), fh, 0, 10)
        .unwrap();
    assert_eq!(r, ReadReply::Inline(b"ke"));
}

fn names(d: &mut Reference, root: RootId, path: &str) -> Vec<(String, bool)> {
    let mut l = Listing::default();
    d.read_dir(root, path, &mut l).expect("listing");
    l.iter().map(|e| (e.name.to_string(), e.is_dir)).collect()
}

fn read_inline(d: &mut Reference, fh: u64, offset: u64, len: u32) -> Result<Vec<u8>, Status> {
    match d.read(inline_slot(), RequestFlags::empty(), fh, offset, len)? {
        ReadReply::Inline(b) => Ok(b.to_vec()),
        r => panic!("expected an inline reply, got {r:?}"),
    }
}

/// 3.4.9: MKDIR creates a directory; 3.2: it keeps the caller's spelling,
/// which READDIR then answers with, and GETATTR finds it by its fold.
#[test]
fn mkdir_creates_a_directory_listed_as_spelled() {
    let mut d = Reference::new().writable_under("");
    assert_eq!(d.mkdir(R0, 0o755, "Data"), Ok(()));
    assert_eq!(d.mkdir(R0, 0o755, "data/Textures"), Ok(()));
    let attr = d.get_attr(R0, "data").unwrap();
    assert!(attr.found && attr.is_dir);
    assert!(d.get_attr(R0, "DATA/textures").unwrap().is_dir);
    assert_eq!(names(&mut d, R0, "."), vec![("Data".to_string(), true)]);
    assert_eq!(
        names(&mut d, R0, "data"),
        vec![("Textures".to_string(), true)]
    );
    assert_eq!(d.stored_names(R0, 0, "data/textures"), Ok("Data/Textures"));
    // A directory handle opens on it, as on any directory.
    let o = d.open(R0, OpenFlags::READ, "data/textures").unwrap();
    assert!(o.is_dir);
    d.close(o.fh).unwrap();
}

/// wire.md 3.4.9: an existing directory is Status::Exists (a director may
/// answer OK; the reference does not), an existing file Status::IoError where
/// a writable provider serves it and Status::Exists where none does. Outside
/// every writable prefix MKDIR refuses as a write open does (3.4.3), and a
/// broken provider's prefix is Status::IoError.
#[test]
fn mkdir_on_an_existing_path_is_exists_and_refuses_as_a_write_open_does() {
    let mut d = Reference::new().writable_under("").with_dir("data").with(
        "data/a.esp",
        vec![1],
        ReadStyle::Whole,
    );
    assert_eq!(d.mkdir(R0, 0o755, "Data"), Err(Status::Exists));
    assert_eq!(d.mkdir(R0, 0o755, "data/A.ESP"), Err(Status::IoError));
    assert_eq!(d.mkdir(R0, 0o755, "."), Err(Status::Exists));
    assert_eq!(names(&mut d, R0, "."), vec![("data".to_string(), true)]);

    let mut ro = Reference::new()
        .with("a", vec![1], ReadStyle::Whole)
        .writable_under(&key(1, "saves/"))
        .failing_writes_under(&key(2, "broken/"));
    assert_eq!(ro.mkdir(R0, 0o755, "new"), Err(Status::ReadOnly));
    assert_eq!(ro.mkdir(R0, 0o755, "a"), Err(Status::Exists));
    assert_eq!(
        ro.mkdir(RootId(1), 0o755, "elsewhere"),
        Err(Status::NotFound)
    );
    assert_eq!(ro.mkdir(RootId(1), 0o755, "saves/Slot1"), Ok(()));
    assert_eq!(ro.mkdir(RootId(2), 0o755, "broken/x"), Err(Status::IoError));
    assert!(!ro.get_attr(R0, "new").unwrap().found);
}

/// 3.4.12: the stored spelling of each component from `skip` on, joined by
/// `/`; a component the director has no entry for comes back as sent (the
/// SHOULD), so the reply always has `components - skip` names.
#[test]
fn stored_names_answer_stored_spellings_from_skip_on() {
    let mut d = Reference::new().writable_under("").with_dir("Data").with(
        "Data/Meshes/Armor.NIF",
        vec![1],
        ReadStyle::Whole,
    );
    let p = "data/meshes/armor.nif";
    assert_eq!(d.stored_names(R0, 0, p), Ok("Data/Meshes/Armor.NIF"));
    assert_eq!(d.stored_names(R0, 1, p), Ok("Meshes/Armor.NIF"));
    assert_eq!(d.stored_names(R0, 2, p), Ok("Armor.NIF"));
    assert_eq!(d.stored_names(R0, 0, "data"), Ok("Data"));
    assert_eq!(
        d.stored_names(R0, 0, "data/absent/x.esp"),
        Ok("Data/absent/x.esp")
    );
    // A name created through the ring is stored as the caller spelled it;
    // the components above it keep theirs.
    let o = d.open(R0, RW | OpenFlags::CREATE, "DATA/New.ESP").unwrap();
    d.close(o.fh).unwrap();
    assert_eq!(d.stored_names(R0, 0, "data/new.esp"), Ok("Data/New.ESP"));
}

/// 3.2, "Keep the spelling a name was created with": a write OPEN that
/// creates `Data/New.ESP` stores that spelling; READDIR lists it so, and
/// every fold of it finds the file.
#[test]
fn a_created_file_keeps_its_spelling_and_is_found_by_its_fold() {
    let mut d = Reference::new().writable_under("").with_dir("Data");
    let o = d.open(R0, RW | OpenFlags::CREATE, "Data/New.ESP").unwrap();
    assert_eq!(d.write(o.fh, 0, b"xy"), Ok(2));
    d.close(o.fh).unwrap();
    let listed = vec![("New.ESP".to_string(), false)];
    assert_eq!(names(&mut d, R0, "data"), listed);
    let a = d.get_attr(R0, "data/new.esp").unwrap();
    assert!(a.found && !a.is_dir && a.size == 2);
    assert_eq!(d.contents("data/new.esp"), Some(b"xy".to_vec()));
    // Opening an existing name in another case does not respell it.
    let trunc = RW | OpenFlags::CREATE | OpenFlags::TRUNC;
    let o = d.open(R0, trunc, "data/NEW.esp").unwrap();
    d.close(o.fh).unwrap();
    assert_eq!(names(&mut d, R0, "data"), listed);
    // A RENAME target is stored as spelled, a change of case included.
    assert_eq!(d.rename(R0, "data/new.esp", "data/Renamed.Esp"), Ok(()));
    assert_eq!(
        names(&mut d, R0, "data"),
        vec![("Renamed.Esp".to_string(), false)]
    );
    assert_eq!(d.rename(R0, "data/renamed.esp", "data/RENAMED.ESP"), Ok(()));
    assert_eq!(
        names(&mut d, R0, "data"),
        vec![("RENAMED.ESP".to_string(), false)]
    );
    assert_eq!(
        d.stored_names(R0, 0, "data/renamed.esp"),
        Ok("Data/RENAMED.ESP")
    );
}

/// 3.4.13: a file moves between roots (replacing what is there), keeping
/// the target's spelling; a directory is Status::CrossRoot; equal roots
/// behave as RENAME.
#[test]
fn rename_x_moves_a_file_between_roots_and_refuses_a_directory() {
    let r1 = RootId(1);
    let mut d = Reference::new()
        .writable_under("")
        .with("saves/a.sav", b"abc".to_vec(), ReadStyle::Whole)
        .with_dir("saves/sub")
        .with(&key(1, "old.sav"), b"zz".to_vec(), ReadStyle::Whole);
    assert_eq!(d.rename_x(R0, r1, "saves/a.sav", "Moved.SAV"), Ok(()));
    assert_eq!(d.contents("saves/a.sav"), None);
    assert_eq!(d.contents(&key(1, "moved.sav")), Some(b"abc".to_vec()));
    assert_eq!(
        names(&mut d, r1, "."),
        vec![
            ("Moved.SAV".to_string(), false),
            ("old.sav".to_string(), false)
        ]
    );
    // Replacing what is there.
    assert_eq!(d.rename_x(r1, R0, "moved.sav", "x"), Ok(()));
    assert_eq!(d.rename_x(R0, r1, "x", "old.sav"), Ok(()));
    assert_eq!(d.contents(&key(1, "old.sav")), Some(b"abc".to_vec()));
    assert_eq!(d.contents("x"), None);
    // A directory, the root itself included, is Status::CrossRoot.
    assert_eq!(
        d.rename_x(R0, r1, "saves/sub", "sub"),
        Err(Status::CrossRoot)
    );
    assert_eq!(d.rename_x(R0, r1, ".", "sub"), Err(Status::CrossRoot));
    assert!(d.get_attr(R0, "saves/sub").unwrap().is_dir);
    // Onto the target root itself, a directory: Status::IsDir, and the file
    // stays where it was.
    let o = d.open(R0, RW | OpenFlags::CREATE, "keep.sav").unwrap();
    d.close(o.fh).unwrap();
    assert_eq!(d.rename_x(R0, r1, "keep.sav", "."), Err(Status::IsDir));
    assert_eq!(d.rename_x(R0, r1, "keep.sav", ""), Err(Status::IsDir));
    assert_eq!(d.contents("keep.sav"), Some(Vec::new()));
    // A missing source is Status::NotFound.
    assert_eq!(d.rename_x(R0, r1, "absent", "b"), Err(Status::NotFound));
    // Equal roots: a RENAME.
    assert_eq!(d.rename_x(r1, r1, "old.sav", "New.sav"), Ok(()));
    assert_eq!(d.contents(&key(1, "new.sav")), Some(b"abc".to_vec()));
    assert_eq!(d.stored_names(r1, 0, "new.sav"), Ok("New.sav"));
}

/// RENAME_X refuses as RENAME does (3.5.1): a source or target outside every writable prefix is
/// Status::ReadOnly, and nothing moves.
#[test]
fn rename_x_outside_a_writable_prefix_is_read_only() {
    let r1 = RootId(1);
    let mut d = Reference::new()
        .with("sealed.esp", vec![1], ReadStyle::Whole)
        .with("saves/a.sav", vec![2], ReadStyle::Whole)
        .writable_under("saves/")
        .with_root(1);
    assert_eq!(d.rename_x(R0, r1, "sealed.esp", "x"), Err(Status::ReadOnly));
    assert_eq!(
        d.rename_x(R0, r1, "saves/a.sav", "x"),
        Err(Status::ReadOnly)
    );
    assert_eq!(d.contents("sealed.esp"), Some(vec![1]));
    assert_eq!(d.contents("saves/a.sav"), Some(vec![2]));
    assert!(!d.get_attr(r1, "x").unwrap().found);
}

/// `ReadStyle::ShorterThanClaimed(n)` with `n` past the file's length is the
/// whole file, not a panic, and the reference keeps answering.
#[test]
fn shorter_than_claimed_past_the_end_reads_the_whole_file() {
    let mut d = Reference::new().with("f", b"abcde".to_vec(), ReadStyle::ShorterThanClaimed(100));
    let fh = d.open(R0, OpenFlags::READ, "f").unwrap().fh;
    assert_eq!(read_inline(&mut d, fh, 0, 64), Ok(b"abcde".to_vec()));
    assert_eq!(read_inline(&mut d, fh, 3, 64), Ok(b"de".to_vec()));
    assert_eq!(read_inline(&mut d, fh, 50, 64), Ok(Vec::new()));
    assert_eq!(d.close(fh), Ok(()));
    assert!(d.get_attr(R0, "f").unwrap().found);
}

/// wire.md 3.4.8: an empty directory is
/// deleted; one with anything under it is Status::IoError; the root is
/// Status::BadRequest; outside every writable prefix Status::ReadOnly.
#[test]
fn delete_removes_an_empty_directory_and_refuses_a_full_one() {
    let mut d = Reference::new()
        .writable_under("")
        .with_dir("empty")
        .with_dir("full")
        .with("full/a", vec![1], ReadStyle::Whole)
        .with_dir("implied/sub");
    assert_eq!(d.delete(R0, "EMPTY"), Ok(()));
    assert!(!d.get_attr(R0, "empty").unwrap().found);
    assert_eq!(d.delete(R0, "full"), Err(Status::IoError));
    assert!(d.get_attr(R0, "full").unwrap().is_dir);
    assert_eq!(d.contents("full/a"), Some(vec![1]));
    assert_eq!(d.delete(R0, "implied/sub"), Ok(()));
    assert_eq!(d.delete(R0, "."), Err(Status::BadRequest));
    assert_eq!(d.delete(R0, "absent"), Err(Status::NotFound));

    let mut ro = Reference::new().with_dir("sealed");
    assert_eq!(ro.delete(R0, "sealed"), Err(Status::ReadOnly));
    assert!(ro.get_attr(R0, "sealed").unwrap().is_dir);
}

/// wire.md 3.4.7: a directory moves with
/// everything under it, its target spelled as sent; a target inside the
/// source, or the root at either end, is Status::BadRequest; a non-empty
/// directory at the target is Status::IoError; an empty one or a file is
/// replaced.
#[test]
fn rename_moves_a_directory_and_what_is_under_it() {
    let mut d = Reference::new()
        .writable_under("")
        .with_dir("Mods")
        .with_dir("Mods/Inner")
        .with("Mods/Inner/a.esp", vec![1], ReadStyle::Whole)
        .with("Mods/b.esp", vec![2], ReadStyle::Whole)
        .with_dir("full")
        .with("full/x", vec![3], ReadStyle::Whole)
        .with_dir("hollow");
    assert_eq!(d.rename(R0, "mods", "Moved"), Ok(()));
    assert!(!d.get_attr(R0, "mods").unwrap().found);
    assert!(!d.get_attr(R0, "mods/inner").unwrap().found);
    assert_eq!(d.contents("mods/b.esp"), None);
    assert!(d.get_attr(R0, "moved/inner").unwrap().is_dir);
    assert_eq!(d.contents("moved/inner/a.esp"), Some(vec![1]));
    assert_eq!(d.contents("moved/b.esp"), Some(vec![2]));
    assert_eq!(
        d.stored_names(R0, 0, "moved/inner/a.esp"),
        Ok("Moved/Inner/a.esp")
    );
    assert_eq!(
        names(&mut d, R0, "moved"),
        vec![("b.esp".to_string(), false), ("Inner".to_string(), true)]
    );
    // A change of case only respells it.
    assert_eq!(d.rename(R0, "moved", "MOVED"), Ok(()));
    assert_eq!(
        d.stored_names(R0, 0, "moved/inner/a.esp"),
        Ok("MOVED/Inner/a.esp")
    );
    // Into itself, or the root: BadRequest.
    assert_eq!(
        d.rename(R0, "moved", "moved/inner/x"),
        Err(Status::BadRequest)
    );
    assert_eq!(d.rename(R0, ".", "x"), Err(Status::BadRequest));
    assert_eq!(d.rename(R0, "moved/b.esp", "."), Err(Status::BadRequest));
    // Onto a non-empty directory: IoError, nothing moves.
    assert_eq!(d.rename(R0, "moved", "full"), Err(Status::IoError));
    assert_eq!(d.contents("moved/b.esp"), Some(vec![2]));
    assert_eq!(d.contents("full/x"), Some(vec![3]));
    // Onto an empty directory: replaced.
    assert_eq!(d.rename(R0, "moved", "hollow"), Ok(()));
    assert_eq!(d.contents("hollow/b.esp"), Some(vec![2]));
    assert!(!d.get_attr(R0, "moved").unwrap().found);
    // A file onto an empty directory: replaced.
    assert_eq!(d.mkdir(R0, 0o755, "spot"), Ok(()));
    assert_eq!(d.rename(R0, "full/x", "spot"), Ok(()));
    let a = d.get_attr(R0, "spot").unwrap();
    assert!(a.found && !a.is_dir);
    assert_eq!(d.contents("spot"), Some(vec![3]));

    let mut ro = Reference::new().with_dir("sealed");
    assert_eq!(ro.rename(R0, "sealed", "x"), Err(Status::ReadOnly));
}

/// 3.4.7: a handle follows its file through a RENAME, its own or one of a
/// directory above it, and never reaches a file later created at the old
/// name. The shim's save pattern writes, renames into place, then writes
/// through the same handle.
#[test]
fn a_handle_follows_its_file_through_a_rename() {
    let mut d = Reference::new()
        .writable_under("")
        .with_dir("saves")
        .with("saves/final.sav", b"OLD".to_vec(), ReadStyle::Whole)
        .with_dir("dir")
        .with("dir/f.txt", b"abc".to_vec(), ReadStyle::Whole);
    let tmp = d
        .open(R0, RW | OpenFlags::CREATE, "saves/final.tmp")
        .unwrap()
        .fh;
    assert_eq!(d.write(tmp, 0, b"NEWDATA"), Ok(7));
    assert_eq!(d.rename(R0, "saves/final.tmp", "saves/Final.sav"), Ok(()));
    assert_eq!(d.write(tmp, 7, b"!"), Ok(1), "the handle follows its file");
    assert_eq!(d.set_attr(tmp, 8), Ok(()));
    assert_eq!(read_inline(&mut d, tmp, 0, 16), Ok(b"NEWDATA!".to_vec()));
    assert_eq!(d.contents("saves/final.sav"), Some(b"NEWDATA!".to_vec()));
    // A file created at the old name is not the handle's.
    let again = d
        .open(
            R0,
            RW | OpenFlags::CREATE | OpenFlags::EXCL,
            "saves/final.tmp",
        )
        .unwrap()
        .fh;
    assert_eq!(d.write(tmp, 0, b"n"), Ok(1));
    assert_eq!(d.contents("saves/final.tmp"), Some(Vec::new()));
    assert_eq!(d.contents("saves/final.sav"), Some(b"nEWDATA!".to_vec()));
    assert_eq!(d.close(again), Ok(()));
    assert_eq!(d.close(tmp), Ok(()));

    // A file under a renamed directory.
    let f = d.open(R0, OpenFlags::READ, "dir/f.txt").unwrap().fh;
    assert_eq!(d.rename(R0, "dir", "Moved"), Ok(()));
    assert_eq!(read_inline(&mut d, f, 0, 16), Ok(b"abc".to_vec()));
    assert_eq!(d.mkdir(R0, 0o755, "dir"), Ok(()));
    let new = d.open(R0, RW | OpenFlags::CREATE, "dir/f.txt").unwrap().fh;
    assert_eq!(d.write(new, 0, b"zz"), Ok(2));
    assert_eq!(read_inline(&mut d, f, 0, 16), Ok(b"abc".to_vec()));
    assert_eq!(d.close(f), Ok(()));
    assert_eq!(d.close(new), Ok(()));
}

/// 3.4.8: a handle whose file was deleted, or replaced by a RENAME onto its
/// name, fails its READ, WRITE and SETATTR (as the reference chooses), never
/// reaches a file later created at or moved to that name, and closes OK.
#[test]
fn a_handle_whose_file_is_gone_never_reaches_a_new_file_at_its_name() {
    let mut d = Reference::new()
        .writable_under("")
        .with("a.txt", b"old!".to_vec(), ReadStyle::Whole)
        .with("b.txt", b"OLD".to_vec(), ReadStyle::Whole)
        .with("c.txt", b"CCC".to_vec(), ReadStyle::Whole);
    let gone = d.open(R0, RW, "a.txt").unwrap().fh;
    assert_eq!(d.delete(R0, "a.txt"), Ok(()));
    let new = d
        .open(R0, RW | OpenFlags::CREATE | OpenFlags::EXCL, "A.txt")
        .unwrap()
        .fh;
    assert_eq!(d.write(new, 0, b"new"), Ok(3));
    assert_eq!(read_inline(&mut d, gone, 0, 16), Err(Status::IoError));
    assert_eq!(d.write(gone, 0, b"Z"), Err(Status::NotFound));
    assert_eq!(d.set_attr(gone, 0), Err(Status::NotFound));
    assert_eq!(d.contents("a.txt"), Some(b"new".to_vec()));
    assert_eq!(d.close(gone), Ok(()));
    assert_eq!(d.close(new), Ok(()));

    // Replaced by a rename onto its name: the handle does not read the file
    // that replaced its own.
    let replaced = d.open(R0, OpenFlags::READ, "b.txt").unwrap().fh;
    assert_eq!(d.rename(R0, "c.txt", "b.txt"), Ok(()));
    assert_eq!(read_inline(&mut d, replaced, 0, 16), Err(Status::IoError));
    assert_eq!(d.write(replaced, 0, b"x"), Err(Status::NotFound));
    assert_eq!(d.contents("b.txt"), Some(b"CCC".to_vec()));
    assert_eq!(d.close(replaced), Ok(()));
}

/// 3.4.7, 3.4.13: after a RENAME_X the reference's handle follows the file
/// to its new root.
#[test]
fn a_handle_follows_its_file_to_another_root() {
    let r1 = RootId(1);
    let mut d = Reference::new()
        .writable_under("")
        .with("a.bin", b"abcd".to_vec(), ReadStyle::Whole)
        .with_root(1);
    let fh = d.open(R0, RW, "a.bin").unwrap().fh;
    assert_eq!(d.rename_x(R0, r1, "a.bin", "A.bin"), Ok(()));
    assert_eq!(read_inline(&mut d, fh, 0, 16), Ok(b"abcd".to_vec()));
    assert_eq!(d.write(fh, 4, b"e"), Ok(1));
    assert_eq!(d.contents(&key(1, "a.bin")), Some(b"abcde".to_vec()));
    let o = d.open(R0, RW | OpenFlags::CREATE, "a.bin").unwrap().fh;
    assert_eq!(d.write(fh, 0, b"Z"), Ok(1));
    assert_eq!(d.contents("a.bin"), Some(Vec::new()));
    assert_eq!(d.close(o), Ok(()));
    assert_eq!(d.close(fh), Ok(()));
}

/// 3.4.3, 3.4.7, 3.4.9, 3.4.13: a create whose parent is missing, or is a
/// file, is refused (the reference answers Status::NotFound for both), and
/// nothing is created or moved.
#[test]
fn a_create_under_a_missing_parent_or_a_file_is_not_found() {
    let r1 = RootId(1);
    let mut d = Reference::new()
        .writable_under("")
        .with_dir("saves")
        .with("saves/a.txt", b"abc".to_vec(), ReadStyle::Whole)
        .with_root(1);
    for p in ["saves/no/such.txt", "saves/a.txt/below", "none/x"] {
        assert_eq!(
            d.open(R0, RW | OpenFlags::CREATE, p).map(|_| ()),
            Err(Status::NotFound),
            "{p}"
        );
        assert_eq!(d.mkdir(R0, 0o755, p), Err(Status::NotFound), "{p}");
        // Below `from` itself, a `to` inside `from` is answered first.
        let inside = p.starts_with("saves/a.txt/");
        let want = if inside {
            Status::BadRequest
        } else {
            Status::NotFound
        };
        assert_eq!(d.rename(R0, "saves/a.txt", p), Err(want), "{p}");
        assert_eq!(
            d.rename_x(R0, r1, "saves/a.txt", p),
            Err(Status::NotFound),
            "{p}"
        );
        assert!(!d.get_attr(R0, p).unwrap().found, "{p}");
    }
    assert_eq!(names(&mut d, R0, "."), vec![("saves".to_string(), true)]);
    assert_eq!(
        names(&mut d, R0, "saves"),
        vec![("a.txt".to_string(), false)]
    );
    assert_eq!(names(&mut d, r1, "."), Vec::new());
    assert_eq!(d.contents("saves/a.txt"), Some(b"abc".to_vec()));
    // A parent the reference implies (a file below it, a writable prefix)
    // is a directory.
    let mut implied = Reference::new()
        .writable_under("w/")
        .with("data/deep/f", vec![1], ReadStyle::Whole)
        .writable_under("data/");
    assert!(implied.open(R0, RW | OpenFlags::CREATE, "w/new").is_ok());
    assert_eq!(implied.mkdir(R0, 0o755, "data/deep/sub"), Ok(()));
}

/// 3.4.3: OPEN_EXCL on an existing directory the writable provider holds is
/// Status::Exists, before the kind of entry is looked at; any other write
/// open of it is Status::IsDir (with OPEN_CREATE) or Status::IoError, and one
/// no writable provider serves Status::ReadOnly.
#[test]
fn open_excl_on_a_directory_is_exists() {
    let mut d = Reference::new()
        .writable_under("saves/")
        .with_dir("saves")
        .with_dir("saves/dir")
        .with_dir("data");
    for flags in [
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCL,
        OpenFlags::WRITE | OpenFlags::EXCL,
        RW | OpenFlags::CREATE | OpenFlags::EXCL | OpenFlags::TRUNC,
    ] {
        assert_eq!(
            d.open(R0, flags, "saves/DIR").map(|_| ()),
            Err(Status::Exists),
            "{flags:?}"
        );
        assert_eq!(
            d.open(R0, flags, "data").map(|_| ()),
            Err(Status::ReadOnly),
            "{flags:?}"
        );
    }
    assert_eq!(
        d.open(R0, OpenFlags::WRITE | OpenFlags::CREATE, "saves/dir")
            .map(|_| ()),
        Err(Status::IsDir)
    );
    assert_eq!(
        d.open(R0, OpenFlags::WRITE, "saves/dir").map(|_| ()),
        Err(Status::IoError)
    );
    assert!(d.get_attr(R0, "saves/dir").unwrap().is_dir);
}

/// 3.4.7: a directory renamed onto an existing file is Status::IoError, and
/// nothing moves; a file onto an empty directory may replace it, as the
/// reference does.
#[test]
fn a_directory_never_replaces_a_file() {
    let mut d = Reference::new()
        .writable_under("")
        .with_dir("dir")
        .with("dir/x", vec![1], ReadStyle::Whole)
        .with("f.txt", b"f".to_vec(), ReadStyle::Whole)
        .with_dir("empty");
    assert_eq!(d.rename(R0, "dir", "F.TXT"), Err(Status::IoError));
    assert_eq!(d.rename(R0, "empty", "f.txt"), Err(Status::IoError));
    assert_eq!(d.contents("f.txt"), Some(b"f".to_vec()));
    assert_eq!(d.contents("dir/x"), Some(vec![1]));
    assert!(d.get_attr(R0, "empty").unwrap().is_dir);
    assert_eq!(d.rename(R0, "f.txt", "empty"), Ok(()));
    assert_eq!(d.contents("empty"), Some(b"f".to_vec()));
}

/// 3.2: the reference holds no named streams, so every request naming one
/// is a name it does not have (Status::NotFound, GETATTR found = 0), and no
/// entry with `:` in its name is ever stored or listed.
#[test]
fn a_named_stream_is_a_name_the_reference_does_not_have() {
    let r1 = RootId(1);
    let mut d = Reference::new()
        .writable_under("")
        .with_dir("saves")
        .with("saves/x.sav", b"data".to_vec(), ReadStyle::Whole)
        .with_root(1);
    let nf = Err(Status::NotFound);
    for s in [
        "saves/x.sav:stream",
        "saves/x.sav:zone.identifier:$data",
        "saves/n:s",
    ] {
        assert_eq!(d.get_attr(R0, s), Ok(ABSENT), "{s}");
        for flags in [
            OpenFlags::READ,
            RW | OpenFlags::CREATE,
            RW | OpenFlags::CREATE | OpenFlags::TRUNC,
            OpenFlags::WRITE,
        ] {
            assert_eq!(d.open(R0, flags, s).map(|_| ()), nf, "{s} {flags:?}");
        }
        assert_eq!(d.mkdir(R0, 0o755, s), nf, "{s}");
        assert_eq!(d.delete(R0, s), nf, "{s}");
        assert_eq!(d.rename(R0, s, "saves/y.sav"), nf, "{s}");
        assert_eq!(d.rename(R0, "saves/x.sav", s), nf, "{s}");
        assert_eq!(d.rename_x(R0, r1, "saves/x.sav", s), nf, "{s}");
        assert_eq!(d.rename_x(R0, r1, s, "y"), nf, "{s}");
        assert_eq!(d.stored_names(R0, 0, s), Err(Status::NotFound), "{s}");
    }
    assert_eq!(
        names(&mut d, R0, "saves"),
        vec![("x.sav".to_string(), false)]
    );
    assert_eq!(d.contents("saves/x.sav"), Some(b"data".to_vec()));
}
