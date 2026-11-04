//! `Reference::from_tree`: the reference a scenario's tree mounts answers
//! for the tree's files, directories, writable prefixes and registry keys.

use aether_protocol::{OpenFlags, ReadReply, RequestFlags, RootId, Status};
use aether_refdirector::tree::{fill, Contents, File, RegKey, RegValue, Registry, Root, Tree};
use aether_refdirector::{pattern, Reference};
use aether_rpc::{Director, Listing, ReadSlot};

fn file(path: &str, contents: Contents) -> File {
    File {
        path: path.to_string(),
        contents,
        mtime: 0,
    }
}

fn read_all(d: &mut Reference, root: u32, path: &str) -> Vec<u8> {
    let o = d
        .open(RootId(root), OpenFlags::READ, path)
        .expect("read open");
    let slot = ReadSlot {
        slot: 0,
        arena: None,
        inline_cap: usize::MAX,
    };
    match d
        .read(slot, RequestFlags::empty(), o.fh, 0, 4096)
        .expect("read")
    {
        ReadReply::Inline(b) => b.to_vec(),
        r => panic!("inline reply expected, got {r:?}"),
    }
}

fn tree() -> Tree {
    Tree {
        roots: vec![
            Root {
                id: 0,
                files: vec![
                    file("data/a.esp", Contents::Text("hello".into())),
                    file("data/b.bin", Contents::Hex(vec![0, 1, 0xfe])),
                    file("data/sub/c.dat", Contents::Fill { size: 300, seed: 7 }),
                ],
                dirs: vec!["empty".into()],
                writable: vec!["saves".into()],
            },
            Root {
                id: 2,
                files: vec![file("x.txt", Contents::Text("two".into()))],
                dirs: vec![],
                writable: vec![],
            },
        ],
        registry: None,
    }
}

#[test]
fn getattr_and_read_answer_the_trees_files_with_their_bytes() {
    let mut d = Reference::from_tree(&tree()).unwrap();
    let a = d.get_attr(RootId(0), "data/a.esp").unwrap();
    assert!(a.found && !a.is_dir);
    assert_eq!(a.size, 5);
    assert_eq!(read_all(&mut d, 0, "data/a.esp"), b"hello");
    assert_eq!(read_all(&mut d, 0, "data/b.bin"), vec![0, 1, 0xfe]);
    assert_eq!(read_all(&mut d, 0, "data/sub/c.dat"), fill(300, 7));
    assert_eq!(d.get_attr(RootId(0), "data/sub/c.dat").unwrap().size, 300);
    assert_eq!(read_all(&mut d, 2, "x.txt"), b"two");
    // Root 2's file is not root 0's.
    assert!(!d.get_attr(RootId(0), "x.txt").unwrap().found);
}

#[test]
fn a_files_ancestors_and_the_named_directories_are_directories() {
    let mut d = Reference::from_tree(&tree()).unwrap();
    for p in ["data", "data/sub", "empty"] {
        let a = d.get_attr(RootId(0), p).unwrap();
        assert!(a.found && a.is_dir, "{p}");
    }
    let mut out = Listing::default();
    d.read_dir(RootId(0), "data", &mut out).unwrap();
    let names: Vec<_> = out.iter().map(|e| (e.name.to_string(), e.is_dir)).collect();
    assert_eq!(
        names,
        vec![
            ("a.esp".to_string(), false),
            ("b.bin".to_string(), false),
            ("sub".to_string(), true)
        ]
    );
}

/// A directory keeps the spelling the tree gives it, as a name created
/// through the ring does (wire.md 3.2: READDIR and STORED_NAMES answer with
/// stored spellings): a file's ancestors as the file's path spells them, and a
/// listed or writable directory as listed.
#[test]
fn directories_of_a_tree_keep_their_spelling() {
    let t = Tree {
        roots: vec![Root {
            id: 0,
            files: vec![file("Data/Textures/Sky.DDS", Contents::Text("s".into()))],
            dirs: vec!["Data/Empty Dir".into()],
            writable: vec!["Saves".into()],
        }],
        registry: None,
    };
    let mut d = Reference::from_tree(&t).unwrap();
    let list = |d: &mut Reference, path: &str| {
        let mut out = Listing::default();
        d.read_dir(RootId(0), path, &mut out).unwrap();
        let mut names: Vec<_> = out.iter().map(|e| e.name.to_string()).collect();
        names.sort();
        names
    };
    assert_eq!(list(&mut d, "."), ["Data", "Saves"]);
    assert_eq!(list(&mut d, "data"), ["Empty Dir", "Textures"]);
    assert_eq!(
        d.stored_names(RootId(0), 0, "data/textures/sky.dds")
            .unwrap(),
        "Data/Textures/Sky.DDS"
    );
}

#[test]
fn writes_stick_only_under_a_writable_prefix() {
    let mut d = Reference::from_tree(&tree()).unwrap();
    let w = OpenFlags::WRITE | OpenFlags::CREATE;
    assert!(d.open(RootId(0), w, "saves/s1.sav").is_ok());
    assert_eq!(
        d.open(RootId(0), OpenFlags::WRITE, "data/a.esp"),
        Err(Status::ReadOnly)
    );
    // A root with nothing writable refuses every write open (wire.md 3.4.3).
    assert_eq!(d.open(RootId(2), w, "new.txt"), Err(Status::ReadOnly));
}

#[test]
fn fill_with_seed_zero_is_the_pattern_and_seeds_differ() {
    assert_eq!(fill(1000, 0), pattern(1000));
    assert_ne!(fill(64, 1), fill(64, 2));
}

#[test]
fn a_tree_with_a_registry_serves_its_keys_and_values() {
    let mut t = tree();
    t.registry = Some(Registry {
        keys: vec![RegKey {
            path: r"\Registry\Machine\Software\Cabin".into(),
            volatile: false,
            values: vec![RegValue {
                name: "Path".into(),
                ty: 1,
                data: Contents::Text("C:\\x".into()),
            }],
        }],
    });
    let mut d = Reference::from_tree(&t).unwrap();
    let k = d.reg_key(r"\Registry\Machine\Software\Cabin").unwrap();
    let node = k.node.expect("a node for the tree's key");
    assert_eq!(node.values.len(), 1);
    assert_eq!(node.values[0].name, "Path");
    assert_eq!(node.values[0].data, b"C:\\x");
    // Without a registry in the tree there is no registry layer.
    assert_eq!(
        Reference::from_tree(&tree())
            .unwrap()
            .reg_lookup(r"\Registry\Machine"),
        Err(Status::NotSupported)
    );
}

#[test]
fn a_registry_key_the_overlay_refuses_is_an_error_not_a_panic() {
    let mut t = tree();
    t.registry = Some(Registry {
        keys: vec![RegKey {
            path: r"\Nope".into(),
            volatile: false,
            values: vec![],
        }],
    });
    let e = Reference::from_tree(&t).err().expect("refused");
    assert!(e.contains("Nope"), "{e}");
}
