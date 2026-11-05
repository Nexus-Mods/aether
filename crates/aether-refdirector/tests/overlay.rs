//! A root served as an overlay: the tree's `lower` layers, read-only, under
//! its writable upper (`contract/scenarios/README.md`, "Tree"). Each test is
//! one rule of `docs/wire.md` for such a root, named by its section; the
//! rules wire.md does not fix, which the reference answers as the conformance
//! notes propose, say "proposed" (`docs/conformance.md`, "Layered roots").

use aether_protocol::{OpenFlags, ReadReply, RequestFlags, RootId, Status};
use aether_refdirector::tree::{Contents, File, Layer, Root, Tree};
use aether_refdirector::Reference;
use aether_rpc::{Director, Listing, ReadSlot};

const R0: RootId = RootId(0);

fn file(path: &str, text: &str) -> File {
    File {
        path: path.to_string(),
        contents: Contents::Text(text.to_string()),
        mtime: 0,
    }
}

/// Root 0, writable as a whole, holding `Data/Upper.esp` and
/// `Data/Both.esp` in the upper, over two lower layers: the first (the
/// nearer) with another spelling of `both.esp`, `Low.esp`, `Meshes/a.nif`,
/// `Deep/b.dds` and the empty `Empty`; the second with a `low.esp` the
/// first hides and `second.esp`. Root 1 is writable as a whole too.
fn layered() -> Tree {
    Tree {
        roots: vec![
            Root {
                id: 0,
                files: vec![
                    file("Data/Upper.esp", "up"),
                    file("Data/Both.esp", "upper bytes"),
                ],
                writable: vec![".".into()],
                lower: vec![
                    Layer {
                        files: vec![
                            file("DATA/both.ESP", "lower"),
                            file("Data/Low.esp", "low"),
                            file("Data/Meshes/a.nif", "mesh"),
                            file("Data/Deep/b.dds", "b"),
                        ],
                        dirs: vec!["Data/Empty".into()],
                    },
                    Layer {
                        files: vec![
                            file("data/low.esp", "hidden by the first layer"),
                            file("data/second.esp", "second"),
                        ],
                        dirs: vec![],
                    },
                ],
                ..Root::default()
            },
            Root {
                id: 1,
                writable: vec![".".into()],
                ..Root::default()
            },
        ],
        registry: None,
    }
}

fn mount(t: &Tree) -> Reference {
    t.validate().expect("a valid tree");
    Reference::from_tree(t).expect("mounted")
}

fn slot() -> ReadSlot<'static> {
    ReadSlot {
        slot: 0,
        arena: None,
        inline_cap: usize::MAX,
    }
}

fn read(d: &mut Reference, fh: u64) -> Result<Vec<u8>, Status> {
    match d.read(slot(), RequestFlags::empty(), fh, 0, 4096)? {
        ReadReply::Inline(b) => Ok(b.to_vec()),
        r => panic!("inline reply expected, got {r:?}"),
    }
}

fn open(d: &mut Reference, flags: OpenFlags, path: &str) -> Result<u64, Status> {
    d.open(R0, flags, path).map(|o| o.fh)
}

fn read_path(d: &mut Reference, path: &str) -> Vec<u8> {
    let fh = open(d, OpenFlags::READ, path).expect("read open");
    let b = read(d, fh).expect("read");
    d.close(fh).unwrap();
    b
}

fn found(d: &mut Reference, path: &str) -> Option<(bool, u64)> {
    let a = d.get_attr(R0, path).unwrap();
    a.found.then_some((a.is_dir, a.size))
}

/// `(name, is_dir, size)` of a listing, sorted by name.
fn list(d: &mut Reference, path: &str) -> Vec<(String, bool, u64)> {
    let mut out = Listing::default();
    d.read_dir(R0, path, &mut out).unwrap();
    let mut v: Vec<_> = out
        .iter()
        .map(|e| (e.name.to_string(), e.is_dir, e.size))
        .collect();
    v.sort();
    v
}

fn names(d: &mut Reference, path: &str) -> Vec<String> {
    list(d, path).into_iter().map(|e| e.0).collect()
}

fn write(d: &mut Reference, fh: u64, data: &[u8]) {
    assert_eq!(d.write(fh, 0, data), Ok(data.len() as u32));
}

#[test]
fn a_lower_file_is_found_and_read_and_the_nearer_layer_wins() {
    let mut d = mount(&layered());
    assert_eq!(found(&mut d, "data/low.esp"), Some((false, 3)));
    assert_eq!(read_path(&mut d, "data/low.esp"), b"low");
    assert_eq!(read_path(&mut d, "data/second.esp"), b"second");
    assert_eq!(found(&mut d, "data/meshes"), Some((true, 0)));
    assert_eq!(found(&mut d, "data/empty"), Some((true, 0)));
    // The upper's file over the lower's (wire.md 3.4.9: an upper over a
    // read-only lower).
    assert_eq!(read_path(&mut d, "data/both.esp"), b"upper bytes");
}

/// wire.md 3.4.3: a file any request can change cannot be immutable, and a
/// write open of a lower-only file changes what its handles read (3.4.4).
#[test]
fn a_read_open_of_a_lower_only_file_is_not_immutable() {
    let mut d = mount(&layered());
    let o = d.open(R0, OpenFlags::READ, "data/low.esp").unwrap();
    assert!(!o.immutable);
}

/// wire.md 3.4.2, 3.2: one listing of the upper and every lower, a name in
/// several shown once, as the highest layer holding it has it.
#[test]
fn a_listing_merges_the_layers_and_the_upper_wins() {
    let mut d = mount(&layered());
    assert_eq!(
        list(&mut d, "data"),
        [
            ("Both.esp".to_string(), false, 11),
            ("Deep".to_string(), true, 0),
            ("Empty".to_string(), true, 0),
            ("Low.esp".to_string(), false, 3),
            ("Meshes".to_string(), true, 0),
            ("Upper.esp".to_string(), false, 2),
            ("second.esp".to_string(), false, 6),
        ]
    );
    assert_eq!(names(&mut d, "."), ["Data"]);
}

/// wire.md 3.2, 3.4.12: a name keeps the spelling of the layer that shows
/// it, and a create keeps its parents' stored spelling, a lower's too.
#[test]
fn stored_names_come_from_the_layer_that_shows_the_name() {
    let mut d = mount(&layered());
    assert_eq!(
        d.stored_names(R0, 0, "data/meshes/a.nif").unwrap(),
        "Data/Meshes/a.nif"
    );
    assert_eq!(
        d.stored_names(R0, 0, "data/both.esp").unwrap(),
        "Data/Both.esp"
    );
    assert_eq!(
        d.stored_names(R0, 0, "data/second.esp").unwrap(),
        "Data/second.esp"
    );
    open(
        &mut d,
        OpenFlags::WRITE | OpenFlags::CREATE,
        "DATA/MESHES/New.nif",
    )
    .unwrap();
    assert_eq!(
        d.stored_names(R0, 0, "data/meshes/new.nif").unwrap(),
        "Data/Meshes/New.nif"
    );
}

/// A write open of a lower-only file copies it up: the upper's copy has the
/// lower's bytes, WRITEs land in it, and the lower is unchanged (wire.md
/// 3.4.9: the lower is read-only).
#[test]
fn a_write_open_copies_a_lower_file_up_with_its_bytes() {
    let mut d = mount(&layered());
    let w = open(&mut d, OpenFlags::WRITE, "data/low.esp").unwrap();
    assert_eq!(read(&mut d, w).unwrap(), b"low");
    write(&mut d, w, b"LO");
    assert_eq!(read_path(&mut d, "data/low.esp"), b"LOw");
    assert_eq!(d.lower_contents(0, "data/low.esp"), Some(b"low".to_vec()));
    assert_eq!(d.contents("data/low.esp"), Some(b"LOw".to_vec()));
}

/// wire.md 3.4.4: every handle of a file reads the file as it is now, a
/// handle opened on the lower's file before the copy-up included: the
/// WRITEs through another handle, and a truncating OPEN.
#[test]
fn a_handle_opened_before_the_copy_up_reads_the_file_as_it_is_now() {
    let mut d = mount(&layered());
    let old = open(&mut d, OpenFlags::READ, "data/low.esp").unwrap();
    let w = open(&mut d, OpenFlags::WRITE, "data/low.esp").unwrap();
    write(&mut d, w, b"new");
    assert_eq!(read(&mut d, old).unwrap(), b"new");

    let old = open(&mut d, OpenFlags::READ, "data/second.esp").unwrap();
    open(
        &mut d,
        OpenFlags::WRITE | OpenFlags::TRUNC,
        "data/second.esp",
    )
    .unwrap();
    assert_eq!(read(&mut d, old).unwrap(), b"");
    assert_eq!(found(&mut d, "data/second.esp"), Some((false, 0)));
}

/// wire.md 3.4.9: DELETE of a lower-only file hides it (no lower beneath
/// it shows instead), and a create there makes a new, empty file that the
/// old file's handles never reach (3.4.8).
#[test]
fn deleting_a_lower_only_file_hides_it_and_a_create_makes_a_new_one() {
    let mut d = mount(&layered());
    let old = open(&mut d, OpenFlags::READ, "data/low.esp").unwrap();
    assert_eq!(d.delete(R0, "data/low.esp"), Ok(()));
    assert_eq!(found(&mut d, "data/low.esp"), None);
    assert!(!names(&mut d, "data").contains(&"Low.esp".to_string()));
    assert_eq!(
        open(&mut d, OpenFlags::READ, "data/low.esp"),
        Err(Status::NotFound)
    );
    assert_eq!(read(&mut d, old), Err(Status::IoError));
    let w = open(&mut d, OpenFlags::WRITE | OpenFlags::CREATE, "Data/low.ESP").unwrap();
    assert_eq!(read(&mut d, w).unwrap(), b"");
    write(&mut d, w, b"fresh");
    assert_eq!(read(&mut d, old), Err(Status::IoError));
    assert_eq!(
        d.stored_names(R0, 0, "data/low.esp").unwrap(),
        "Data/low.ESP"
    );
}

/// wire.md 3.4.9: DELETE of a lower-only directory with children hides it
/// and everything under it.
#[test]
fn deleting_a_lower_only_directory_with_children_hides_all_of_it() {
    let mut d = mount(&layered());
    let h = open(&mut d, OpenFlags::READ, "data/meshes/a.nif").unwrap();
    assert_eq!(d.delete(R0, "data/meshes"), Ok(()));
    assert_eq!(found(&mut d, "data/meshes"), None);
    assert_eq!(found(&mut d, "data/meshes/a.nif"), None);
    assert!(!names(&mut d, "data").contains(&"Meshes".to_string()));
    assert_eq!(read(&mut d, h), Err(Status::IoError));
}

/// wire.md 3.4.9: what a DELETE hid stays hidden, so a directory made again
/// where it hid a lower one is empty.
#[test]
fn a_directory_made_again_over_a_hidden_one_is_empty() {
    let mut d = mount(&layered());
    d.delete(R0, "data/meshes").unwrap();
    assert_eq!(d.mkdir(R0, 0o755, "Data/meshes"), Ok(()));
    assert_eq!(list(&mut d, "data/meshes"), []);
    assert_eq!(found(&mut d, "data/meshes/a.nif"), None);
}

/// wire.md 3.4.8: a deleted name is missing, so the lower's file under the
/// upper's does not come back.
#[test]
fn deleting_a_name_both_layers_hold_leaves_nothing() {
    let mut d = mount(&layered());
    assert_eq!(d.delete(R0, "data/both.esp"), Ok(()));
    assert_eq!(found(&mut d, "data/both.esp"), None);
    assert!(!names(&mut d, "data").contains(&"Both.esp".to_string()));
}

/// wire.md 3.4.9: a lower-only file is copied up and renamed; its handles
/// follow it (3.4.7), and the old name is gone.
#[test]
fn renaming_a_lower_only_file_copies_it_up_and_its_handles_follow() {
    let mut d = mount(&layered());
    let h = open(&mut d, OpenFlags::READ, "data/low.esp").unwrap();
    assert_eq!(d.rename(R0, "data/low.esp", "Data/Moved.esp"), Ok(()));
    assert_eq!(found(&mut d, "data/low.esp"), None);
    assert_eq!(read_path(&mut d, "data/moved.esp"), b"low");
    assert_eq!(read(&mut d, h).unwrap(), b"low");
    let w = open(&mut d, OpenFlags::WRITE, "data/moved.esp").unwrap();
    write(&mut d, w, b"L");
    assert_eq!(read(&mut d, h).unwrap(), b"Low");
    let n = names(&mut d, "data");
    assert!(n.contains(&"Moved.esp".to_string()) && !n.contains(&"Low.esp".to_string()));
}

/// wire.md 3.4.9: a lower-only directory does not move, and a RENAME of a
/// lower-only name to another case of itself keeps the lower's spelling.
#[test]
fn a_lower_only_directory_does_not_move_and_a_case_rename_keeps_the_spelling() {
    let mut d = mount(&layered());
    assert_eq!(
        d.rename(R0, "data/meshes", "Data/Models"),
        Err(Status::IoError)
    );
    assert_eq!(found(&mut d, "data/meshes/a.nif"), Some((false, 4)));
    assert_eq!(found(&mut d, "data/models"), None);
    assert_eq!(d.rename(R0, "data/meshes", "Data/MESHES"), Ok(()));
    assert_eq!(d.rename(R0, "data/second.esp", "Data/SECOND.esp"), Ok(()));
    assert_eq!(
        d.stored_names(R0, 0, "data/meshes/a.nif").unwrap(),
        "Data/Meshes/a.nif"
    );
    assert_eq!(
        d.stored_names(R0, 0, "data/second.esp").unwrap(),
        "Data/second.esp"
    );
}

/// wire.md 3.4.9: MKDIR over a lower-only file makes the directory in the
/// upper, which hides the file; the file's handles are as for a deleted
/// file (3.4.8).
#[test]
fn mkdir_over_a_lower_only_file_makes_a_directory_that_hides_it() {
    let mut d = mount(&layered());
    let h = open(&mut d, OpenFlags::READ, "data/low.esp").unwrap();
    assert_eq!(d.mkdir(R0, 0o755, "Data/Low.esp"), Ok(()));
    assert_eq!(found(&mut d, "data/low.esp"), Some((true, 0)));
    assert_eq!(list(&mut d, "data/low.esp"), []);
    assert_eq!(read(&mut d, h), Err(Status::IoError));
}

/// Proposed: MKDIR of a directory only a lower holds is `ST_EXISTS` (as of
/// any existing directory; wire.md 3.4.9 lets a director answer `ST_OK`
/// too), and the listing below it is unchanged.
#[test]
fn proposed_mkdir_over_a_lower_only_directory_is_exists() {
    let mut d = mount(&layered());
    assert_eq!(d.mkdir(R0, 0o755, "data/meshes"), Err(Status::Exists));
    assert_eq!(names(&mut d, "data/meshes"), ["a.nif"]);
}

/// Proposed: a name created below a lower-only directory copies the
/// directory up, and a write open of a lower-only file copies the file up:
/// from then on the rules for names the writable provider holds apply
/// (wire.md 3.4.7 to 3.4.9).
#[test]
fn proposed_a_create_below_a_lower_directory_and_a_write_open_copy_up() {
    let mut d = mount(&layered());
    open(
        &mut d,
        OpenFlags::WRITE | OpenFlags::CREATE,
        "data/deep/new.txt",
    )
    .unwrap();
    assert_eq!(d.delete(R0, "data/deep"), Err(Status::IoError));
    assert_eq!(d.rename(R0, "data/deep", "Data/DEEP"), Ok(()));
    assert_eq!(d.stored_names(R0, 0, "data/deep").unwrap(), "Data/DEEP");

    open(&mut d, OpenFlags::WRITE, "data/second.esp").unwrap();
    assert_eq!(d.mkdir(R0, 0o755, "data/second.esp"), Err(Status::IoError));
}

/// A file whose bytes a WRITE changed is the upper's (wire.md 3.4.9: the
/// lower is read-only), so MKDIR over it is `ST_IO_ERROR` and a case-only
/// RENAME respells it (3.4.7, 3.4.9).
#[test]
fn a_written_lower_file_is_the_uppers() {
    let mut d = mount(&layered());
    let w = open(&mut d, OpenFlags::WRITE, "data/low.esp").unwrap();
    write(&mut d, w, b"x");
    assert_eq!(d.mkdir(R0, 0o755, "data/low.esp"), Err(Status::IoError));
    assert_eq!(d.rename(R0, "data/low.esp", "Data/LOW.esp"), Ok(()));
    assert_eq!(
        d.stored_names(R0, 0, "data/low.esp").unwrap(),
        "Data/LOW.esp"
    );
}

/// Proposed: a directory a lower holds, the upper holding it too, does not
/// move (as a lower-only one does not, wire.md 3.4.9); a case-only RENAME
/// of it respells it.
#[test]
fn proposed_a_directory_in_both_layers_does_not_move() {
    let mut d = mount(&layered());
    assert_eq!(d.rename(R0, "data", "Other"), Err(Status::IoError));
    assert_eq!(found(&mut d, "data/low.esp"), Some((false, 3)));
    assert_eq!(d.rename(R0, "data", "DATA"), Ok(()));
    assert_eq!(names(&mut d, "."), ["DATA"]);
    assert_eq!(read_path(&mut d, "data/meshes/a.nif"), b"mesh");
}

/// wire.md 3.4.8: a directory the upper holds with anything under it, a
/// lower's names included, is `ST_IO_ERROR`.
#[test]
fn deleting_an_upper_directory_with_lower_names_under_it_is_an_io_error() {
    let mut d = mount(&layered());
    assert_eq!(d.delete(R0, "data"), Err(Status::IoError));
    assert_eq!(found(&mut d, "data/meshes/a.nif"), Some((false, 4)));
}

/// wire.md 3.4.7: a file at `to` is replaced, a lower-only one too, and
/// once the new file is deleted the lower's does not come back.
#[test]
fn a_rename_onto_a_lower_only_file_replaces_it() {
    let mut d = mount(&layered());
    let h = open(&mut d, OpenFlags::READ, "data/second.esp").unwrap();
    assert_eq!(d.rename(R0, "data/upper.esp", "data/second.esp"), Ok(()));
    assert_eq!(read_path(&mut d, "data/second.esp"), b"up");
    assert_eq!(read(&mut d, h), Err(Status::IoError));
    d.delete(R0, "data/second.esp").unwrap();
    assert_eq!(found(&mut d, "data/second.esp"), None);
}

/// wire.md 3.4.13: RENAME_X copies a lower-only file to the other root and
/// removes it from its own, where it is then missing.
#[test]
fn rename_x_of_a_lower_only_file_moves_it_to_the_other_root() {
    let mut d = mount(&layered());
    assert_eq!(d.rename_x(R0, RootId(1), "data/low.esp", "Low.esp"), Ok(()));
    assert_eq!(found(&mut d, "data/low.esp"), None);
    let o = d.open(RootId(1), OpenFlags::READ, "low.esp").unwrap();
    assert_eq!(read(&mut d, o.fh).unwrap(), b"low");
    assert_eq!(
        d.rename_x(R0, RootId(1), "data/meshes", "Meshes"),
        Err(Status::CrossRoot)
    );
}

/// A truncating write open of a lower-only file leaves it empty for every
/// handle (wire.md 3.4.3, 3.4.4), and the lower unchanged.
#[test]
fn truncating_a_lower_only_file() {
    let mut d = mount(&layered());
    let old = open(&mut d, OpenFlags::READ, "data/meshes/a.nif").unwrap();
    let w = open(
        &mut d,
        OpenFlags::WRITE | OpenFlags::TRUNC,
        "data/meshes/a.nif",
    )
    .unwrap();
    assert_eq!(read(&mut d, w).unwrap(), b"");
    assert_eq!(read(&mut d, old).unwrap(), b"");
    assert_eq!(
        list(&mut d, "data/meshes"),
        [("a.nif".to_string(), false, 0)]
    );
    assert_eq!(
        d.lower_contents(0, "data/meshes/a.nif"),
        Some(b"mesh".to_vec())
    );
}

/// Between layers, the nearer one's file hides everything a farther one has
/// at and below its name, and the upper's directory over a lower's file
/// hides the file and what is below it in farther layers.
#[test]
fn a_file_in_a_nearer_layer_hides_a_farther_directory() {
    let t = Tree {
        roots: vec![Root {
            id: 0,
            dirs: vec!["z".into()],
            writable: vec![".".into()],
            lower: vec![
                Layer {
                    files: vec![file("x", "file"), file("z", "file")],
                    dirs: vec![],
                },
                Layer {
                    files: vec![file("x/y", "under"), file("z/w", "under")],
                    dirs: vec![],
                },
            ],
            ..Root::default()
        }],
        registry: None,
    };
    let mut d = mount(&t);
    assert_eq!(found(&mut d, "x"), Some((false, 4)));
    assert_eq!(found(&mut d, "x/y"), None);
    assert_eq!(found(&mut d, "z"), Some((true, 0)));
    assert_eq!(found(&mut d, "z/w"), None);
    assert_eq!(list(&mut d, "z"), []);
}

/// Under a writable entry: the lower's names there answer as above, and the
/// tree's other names belong to a read-only provider as before.
#[test]
fn a_lower_under_a_writable_entry() {
    let t = Tree {
        roots: vec![Root {
            id: 0,
            files: vec![file("data/a.esp", "a")],
            dirs: vec!["saves".into()],
            writable: vec!["saves".into()],
            lower: vec![Layer {
                files: vec![file("saves/Old.sav", "old")],
                dirs: vec![],
            }],
        }],
        registry: None,
    };
    let mut d = mount(&t);
    assert_eq!(names(&mut d, "saves"), ["Old.sav"]);
    assert_eq!(d.delete(R0, "data/a.esp"), Err(Status::ReadOnly));
    assert_eq!(d.delete(R0, "saves/old.sav"), Ok(()));
    assert_eq!(names(&mut d, "saves"), Vec::<String>::new());
}

#[test]
fn a_tree_whose_lower_is_not_under_a_writable_entry_is_refused() {
    let bad = |lower: Layer, writable: Vec<String>| {
        Tree {
            roots: vec![Root {
                id: 0,
                dirs: vec!["saves".into()],
                writable,
                lower: vec![lower],
                ..Root::default()
            }],
            registry: None,
        }
        .validate()
        .err()
    };
    let e = bad(
        Layer {
            files: vec![file("data/a.esp", "a")],
            dirs: vec![],
        },
        vec!["saves".into()],
    )
    .expect("outside the writable entry");
    assert!(e.contains("data/a.esp"), "{e}");
    assert!(bad(
        Layer {
            files: vec![file("saves/a", "a")],
            dirs: vec![]
        },
        vec![]
    )
    .is_some());
    // The writable entry itself is the upper's.
    assert!(bad(
        Layer {
            files: vec![],
            dirs: vec!["saves".into()]
        },
        vec!["saves".into()]
    )
    .is_some());
    // Within a layer, a name once and of one kind.
    assert!(bad(
        Layer {
            files: vec![file("saves/a", "a"), file("SAVES/A", "b")],
            dirs: vec![]
        },
        vec!["saves".into()]
    )
    .is_some());
    assert!(bad(
        Layer {
            files: vec![file("saves/a", "a")],
            dirs: vec!["saves/a".into()]
        },
        vec!["saves".into()]
    )
    .is_some());
    assert!(bad(
        Layer {
            files: vec![file("saves/../x", "a")],
            dirs: vec![]
        },
        vec!["saves".into()]
    )
    .is_some());
    // Two layers may hold one name.
    let ok = Tree {
        roots: vec![Root {
            id: 0,
            writable: vec![".".into()],
            lower: vec![
                Layer {
                    files: vec![file("a", "1")],
                    dirs: vec![],
                },
                Layer {
                    files: vec![],
                    dirs: vec!["A".into()],
                },
            ],
            ..Root::default()
        }],
        registry: None,
    };
    assert_eq!(ok.validate(), Ok(()));
}
