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
    }
}

const ABSENT: AttrResp = AttrResp {
    found: false,
    is_dir: false,
    size: 0,
    mtime: 0,
};

/// wire.md 3.2: "a path that would leave its root (`..` above the root)" is
/// an error; 3.5.1: a new director answers it ST_BAD_REQUEST.
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
    // 3.4.2, 3.4.3, 3.4.8: unknown root ST_NOT_FOUND.
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
    // 3.5.1: a new director answers an unknown root ST_NOT_FOUND.
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

/// 3.4.3: OPEN_WRITE on a root with no writable provider is ST_READ_ONLY,
/// whether or not the path exists; a missing path on a root that has one,
/// but not over this path, stays ST_NOT_FOUND.
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

/// 3.4.3: on an OPEN_APPEND handle the director ignores WRITE offsets.
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

/// 3.4.4: READ of a directory handle is ST_IS_DIR; 3.5: ST_IS_DIR is "a file
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

/// 3.5: ST_NO_SPACE, "storage full", for a write or a length past what the
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
