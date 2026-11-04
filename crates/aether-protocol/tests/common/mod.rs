//! The golden vectors as typed messages, shared by `golden.rs` (which pins
//! their bytes), `typed.rs` (malformed variants of them) and `alloc.rs`
//! (counted allocations).
#![allow(dead_code)]

use aether_protocol::shimcfg::{encode_config, encode_config_full, StaticImport};
use aether_protocol::{
    AttrResp, ChangedReply, DirEntries, DirEntry, KeyReply, Lookup, LookupReply, Opcode, OpenFlags,
    OpenResp, ReadReply, Reply, Request, RequestFlags, RootId,
};
use aether_registry::{Child, Node, Value};
use std::borrow::Cow;

/// One golden vector.
pub enum Msg {
    Req(Request<'static>),
    /// A reply, with the opcode of the request it answers.
    Rep(Opcode, Reply<'static>),
    /// Not a ring message (the shim config, the ring header): bytes only.
    Raw(Vec<u8>),
}

impl Msg {
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Msg::Req(r) => r.encode(&mut b),
            Msg::Rep(_, r) => r.encode(&mut b),
            Msg::Raw(raw) => b.extend_from_slice(raw),
        }
        b
    }
}

pub fn sample_node() -> Node {
    let mut n = Node {
        values: vec![
            Value {
                name: "Path".into(),
                ty: 1,
                data: vec![1, 2, 3],
            },
            Value {
                name: String::new(),
                ty: 4,
                data: vec![],
            },
        ],
        value_tombstones: vec!["gone".into()],
        created: true,
        volatile: true,
        last_write: 0x0123_4567_89ab_cdef,
        ..Node::default()
    };
    n.children
        .insert("sub".into(), ("Sub".into(), Child::Present));
    n.children
        .insert("dead".into(), ("Dead".into(), Child::Tombstone));
    n
}

static TWO_ENTRIES: [DirEntry<'static>; 2] = [
    DirEntry {
        name: "a.esp",
        is_dir: false,
        size: 10,
        mtime: 1,
    },
    DirEntry {
        name: "sub",
        is_dir: true,
        size: 0,
        mtime: 0,
    },
];

const SOFTWARE: &str = r"\Registry\Machine\Software";

/// Every line of `golden/vectors.txt`, in order.
pub fn vectors() -> Vec<(&'static str, Msg)> {
    use Msg::{Raw, Rep, Req};
    vec![
        (
            "shim-config-v2-root-runtime",
            Raw(encode_config(r"C:\GameLayers\runtime")),
        ),
        (
            "open-req-read-skyrim",
            Req(Request::Open {
                root: RootId(0),
                flags: OpenFlags::READ,
                path: "Data/Skyrim.esm",
            }),
        ),
        (
            "getattr-resp-file-123",
            Rep(
                Opcode::GetAttr,
                Reply::GetAttr(AttrResp {
                    found: true,
                    is_dir: false,
                    size: 123,
                    mtime: -7,
                }),
            ),
        ),
        (
            "read-req-fh7-off10-len4",
            Req(Request::Read {
                slot_flags: RequestFlags::empty(),
                fh: 7,
                offset: 10,
                len: 4,
            }),
        ),
        (
            "read-resp-abcd",
            Rep(Opcode::Read, Reply::Read(ReadReply::Inline(b"abcd"))),
        ),
        (
            "readdir-resp-two",
            Rep(
                Opcode::ReadDir,
                Reply::ReadDir(DirEntries::new(&TWO_ENTRIES)),
            ),
        ),
        ("close-req-99", Req(Request::Close { fh: 99 })),
        (
            "open-resp-fh42-size1000",
            Rep(
                Opcode::Open,
                Reply::Open(OpenResp {
                    fh: 42,
                    size: 1000,
                    ..Default::default()
                }),
            ),
        ),
        (
            "read-resp-bulk-len5-off65536",
            Rep(
                Opcode::Read,
                Reply::Read(ReadReply::Bulk {
                    len: 5,
                    arena_offset: 65536,
                }),
            ),
        ),
        (
            "write-req-fh7-off10-abc",
            Req(Request::Write {
                fh: 7,
                offset: 10,
                data: b"abc",
            }),
        ),
        ("write-resp-3", Rep(Opcode::Write, Reply::Write(3))),
        (
            "mkdir-req-mode493-dir",
            Req(Request::Mkdir {
                root: RootId(0),
                mode: 493,
                path: "sub/dir",
            }),
        ),
        (
            "rename-req-a-b",
            Req(Request::Rename {
                root: RootId(0),
                from: "old.txt",
                to: "new.txt",
            }),
        ),
        (
            "setattr-req-fh5-size100",
            Req(Request::SetAttr { fh: 5, size: 100 }),
        ),
        ("ring-header-slots4-cap256", {
            use aether_ipc::seg::OwnedSeg;
            let owned = OwnedSeg::new(4096);
            aether_ipc::ring::init(owned.seg(), 4, 256).unwrap();
            Raw(owned
                .seg()
                .read_bytes(0, aether_ipc::layout::RING_HEADER_SIZE)
                .unwrap())
        }),
        // Added with the descriptor's removal: the flags, stored names,
        // shim-config extras and every registry codec.
        (
            "open-resp-immutable-gen-0xa1b2c3d4",
            Rep(
                Opcode::Open,
                Reply::Open(OpenResp {
                    fh: 42,
                    size: 1000,
                    is_dir: false,
                    immutable: true,
                    mount_gen: 0xA1B2_C3D4,
                }),
            ),
        ),
        (
            "path-req-root1-a",
            Req(Request::GetAttr {
                root: RootId(1),
                path: "a",
            }),
        ),
        (
            "names-req-root2-skip1",
            Req(Request::StoredNames {
                root: RootId(2),
                skip: 1,
                path: "Data/Textures",
            }),
        ),
        (
            "names-resp-two",
            Rep(Opcode::StoredNames, Reply::StoredNames("Data/Skyrim.esm")),
        ),
        (
            "shim-config-v2-one-static-import",
            Raw(encode_config_full(
                r"C:\GameLayers\runtime",
                &[StaticImport {
                    dll_name: "d3d11.dll".into(),
                    backing_path: r"C:\GameLayers\d3d11.dll".into(),
                }],
            )),
        ),
        ("reg-path", Req(Request::RegKey { path: SOFTWARE })),
        (
            "reg-set-value",
            Req(Request::RegSetValue {
                path: SOFTWARE,
                name: "Name",
                ty: 3,
                data: &[9, 8, 7],
            }),
        ),
        (
            "reg-delete-value",
            Req(Request::RegDeleteValue {
                path: SOFTWARE,
                name: "Name",
            }),
        ),
        (
            "reg-create-key-volatile",
            Req(Request::RegCreateKey {
                path: SOFTWARE,
                volatile: true,
            }),
        ),
        (
            "reg-rename-key",
            Req(Request::RegRenameKey {
                path: SOFTWARE,
                new_leaf: "Other",
            }),
        ),
        (
            "reg-changed-subtree-since77",
            Req(Request::RegChanged {
                path: r"\Registry\Machine",
                subtree: true,
                since: 77,
            }),
        ),
        (
            "reg-changed-reply-true-5",
            Rep(
                Opcode::RegChanged,
                Reply::RegChanged(ChangedReply {
                    version: 5,
                    changed: true,
                }),
            ),
        ),
        (
            "reg-version-reply-9",
            Rep(Opcode::RegSetValue, Reply::RegVersion(9)),
        ),
        (
            "reg-lookup-reply-state1-below-42",
            Rep(
                Opcode::RegLookup,
                Reply::RegLookup(LookupReply {
                    version: 42,
                    state: Lookup::Present { created: false },
                    below: true,
                }),
            ),
        ),
        (
            "reg-key-reply-none-13",
            Rep(
                Opcode::RegKey,
                Reply::RegKey(KeyReply {
                    version: 13,
                    node: None,
                }),
            ),
        ),
        (
            "reg-key-reply-sample-node-12",
            Rep(
                Opcode::RegKey,
                Reply::RegKey(KeyReply {
                    version: 12,
                    node: Some(Cow::Owned(sample_node())),
                }),
            ),
        ),
    ]
}
