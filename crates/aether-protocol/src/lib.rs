//! The pure wire contract for the VFS stack: the protocol table (opcodes,
//! requests, replies and their codecs), statuses, flags and the root id. No
//! OS I/O. The vocabulary is FUSE-style RPC, but nothing here touches
//! `/dev/fuse`. Registry ops use `aether-registry`'s portable node model.
//!
//! The wire is defined once, in `table.rs`: [`Opcode`], [`Request`] and
//! [`Reply`] (which decode borrowing from a payload and encode into any
//! [`Buf`]), with [`Status`], [`OpenFlags`] and [`RequestFlags`] beside them
//! in `status.rs`. Code elsewhere names opcodes and statuses by these types,
//! never by number; only the tests that pin the fixed numbers write them.
#![forbid(unsafe_code)]

mod buf;
mod describe;
mod field;
mod root;
pub mod shimcfg;
mod status;
mod table;
mod wire;

pub use aether_registry::Lookup;
pub use buf::{Buf, Overflow, SliceBuf};
pub use describe::contract_json;
pub use root::RootId;
pub use status::{OpenFlags, RequestFlags, Status};
#[cfg(feature = "testing")]
pub use table::{any_reply_for, any_request, any_request_for};
pub use table::{
    AttrResp, ChangedReply, DirEntries, DirEntry, DirIter, KeyReply, LookupReply, Opcode, OpenResp,
    ReadReply, Reply, Request, READ_RESP_BULK_BIT,
};
pub use wire::DecodeError;

/// The ring's wire version, as `contract/protocol.json` records it. It is the
/// ring header's `version` word (`aether_ipc::layout::VERSION`, which this
/// crate cannot name: that crate depends on this one), and
/// `tests/contract.rs` checks the two equal.
pub const WIRE_VERSION: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use aether_registry::Node;
    use std::borrow::Cow;

    fn req(r: Request<'_>) -> Vec<u8> {
        let mut b = Vec::new();
        r.encode(&mut b);
        b
    }

    fn rep(r: Reply<'_>) -> Vec<u8> {
        let mut b = Vec::new();
        r.encode(&mut b);
        b
    }

    fn dreq(op: Opcode, p: &[u8]) -> Option<Request<'_>> {
        Request::decode(op, 0, p).ok()
    }

    fn drep(op: Opcode, p: &[u8]) -> Option<Reply<'_>> {
        Reply::decode(op, 0, p).ok()
    }

    fn path_req(root: u32, path: &str) -> Vec<u8> {
        req(Request::GetAttr {
            root: RootId(root),
            path,
        })
    }

    #[test]
    fn names_resp_roundtrip() {
        let b = rep(Reply::StoredNames("Data/Skyrim.esm"));
        assert_eq!(b, b"Data/Skyrim.esm");
        assert_eq!(
            drep(Opcode::StoredNames, &b),
            Some(Reply::StoredNames("Data/Skyrim.esm"))
        );
        assert_eq!(drep(Opcode::StoredNames, &[0xff]), None);
    }

    #[test]
    fn open_req_roundtrip() {
        let r = Request::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path: "Data/Skyrim.esm",
        };
        assert_eq!(dreq(Opcode::Open, &req(r)), Some(r));
    }

    /// The same relative path carried for two different roots must produce two distinguishable payloads, and each
    /// must decode back to its own root. Without this the shim can classify a
    /// path as belonging to root 1 and has no field in which to say so.
    #[test]
    fn path_carrying_payloads_carry_the_root() {
        let a = path_req(0, "same.txt");
        let b = path_req(1, "same.txt");
        assert_ne!(a, b, "two roots must not encode to identical bytes");
        for (bytes, root) in [(&a, 0), (&b, 1)] {
            assert_eq!(
                dreq(Opcode::GetAttr, bytes),
                Some(Request::GetAttr {
                    root: RootId(root),
                    path: "same.txt"
                })
            );
        }
        for r in [
            Request::Open {
                root: RootId(7),
                flags: OpenFlags::READ,
                path: "same.txt",
            },
            Request::Mkdir {
                root: RootId(2),
                mode: 493,
                path: "sub/dir",
            },
            Request::Rename {
                root: RootId(3),
                from: "old.txt",
                to: "new.txt",
            },
        ] {
            assert_eq!(dreq(r.opcode(), &req(r)), Some(r));
        }
    }

    /// The exact byte layout, pinned. This is a contract with the injected
    /// DLL: a shim built against the pre-task-5 shape (bare path / bare
    /// `flags|path`) would have its first four path bytes decoded as a root
    /// id. `aether_ipc::layout::VERSION` was bumped to 2 in the same change so
    /// that shim is rejected at ring open rather than silently misparsed —
    /// see `aether_ipc::ring::tests::open_rejects_a_stale_wire_version`.
    #[test]
    fn root_leads_the_wire_layout() {
        assert_eq!(path_req(1, "a"), vec![1, 0, 0, 0, b'a']);
        assert_eq!(
            req(Request::Open {
                root: RootId(1),
                flags: OpenFlags::WRITE,
                path: "a"
            }),
            vec![1, 0, 0, 0, 2, 0, 0, 0, b'a']
        );
        assert_eq!(
            req(Request::Mkdir {
                root: RootId(1),
                mode: 2,
                path: "a"
            }),
            vec![1, 0, 0, 0, 2, 0, 0, 0, b'a']
        );
        assert_eq!(
            req(Request::Rename {
                root: RootId(1),
                from: "a",
                to: "b"
            }),
            vec![1, 0, 0, 0, 1, 0, 0, 0, b'a', b'b']
        );
    }

    #[test]
    fn open_resp_roundtrip() {
        let r = OpenResp {
            fh: 42,
            size: 1000,
            is_dir: false,
            immutable: true,
            mount_gen: 0xA1B2_C3D4,
        };
        assert_eq!(
            drep(Opcode::Open, &rep(Reply::Open(r.clone()))),
            Some(Reply::Open(r))
        );
    }

    /// A reply with zeros in the flags byte and `mount_gen` must decode as
    /// mutable, generation 0, never as cacheable.
    #[test]
    fn an_open_reply_with_zero_padding_is_mutable() {
        let mut b = Vec::new();
        b.extend_from_slice(&7u64.to_le_bytes());
        b.extend_from_slice(&9u64.to_le_bytes());
        b.extend_from_slice(&[0u8; 8]);
        let Some(Reply::Open(r)) = drep(Opcode::Open, &b) else {
            panic!("an OPEN reply");
        };
        assert_eq!((r.fh, r.size, r.immutable, r.mount_gen), (7, 9, false, 0));
    }

    #[test]
    fn read_req_resp_roundtrip() {
        let r = Request::Read {
            slot_flags: RequestFlags::empty(),
            fh: 7,
            offset: 10,
            len: 4,
        };
        assert_eq!(dreq(Opcode::Read, &req(r)), Some(r));
        let data = b"abcd";
        assert_eq!(
            drep(Opcode::Read, &rep(Reply::Read(ReadReply::Inline(data)))),
            Some(Reply::Read(ReadReply::Inline(data)))
        );
    }

    #[test]
    fn write_req_resp_roundtrip() {
        let r = Request::Write {
            fh: 7,
            offset: 10,
            data: b"abc",
        };
        assert_eq!(dreq(Opcode::Write, &req(r)), Some(r));
        assert_eq!(
            drep(Opcode::Write, &rep(Reply::Write(3))),
            Some(Reply::Write(3))
        );
    }

    #[test]
    fn close_req_roundtrip() {
        let r = Request::Close { fh: 99 };
        assert_eq!(dreq(Opcode::Close, &req(r)), Some(r));
    }

    #[test]
    fn mkdir_req_roundtrip() {
        let r = Request::Mkdir {
            root: RootId(0),
            mode: 493,
            path: "sub/dir",
        };
        assert_eq!(dreq(Opcode::Mkdir, &req(r)), Some(r));
    }

    #[test]
    fn rename_req_roundtrip() {
        let r = Request::Rename {
            root: RootId(0),
            from: "old.txt",
            to: "new.txt",
        };
        assert_eq!(dreq(Opcode::Rename, &req(r)), Some(r));
    }

    #[test]
    fn setattr_req_roundtrip() {
        let r = Request::SetAttr { fh: 5, size: 100 };
        assert_eq!(dreq(Opcode::SetAttr, &req(r)), Some(r));
    }

    /// The wire numbers as literals. A change here is a wire break.
    #[test]
    fn wire_numbers_are_fixed() {
        let want: &[(&str, u32)] = &[
            ("getattr", 1),
            ("readdir", 2),
            ("open", 3),
            ("read", 5),
            ("write", 6),
            ("setattr", 7),
            ("rename", 8),
            ("delete", 9),
            ("mkdir", 10),
            ("close", 11),
            ("heartbeat", 13),
            ("stored-names", 14),
            ("reg-lookup", 15),
            ("reg-key", 16),
            ("reg-set-value", 17),
            ("reg-delete-value", 18),
            ("reg-create-key", 19),
            ("reg-delete-key", 20),
            ("reg-rename-key", 21),
            ("reg-changed", 22),
            ("rename-x", 23),
            ("stage-exec", 24),
        ];
        let ops: Vec<(&str, u32)> = Opcode::ALL
            .iter()
            .map(|&op| (op.name(), op as u32))
            .collect();
        assert_eq!(ops, want);
        let sts: &[(&str, i32)] = &[
            ("ok", 0),
            ("not-found", -1),
            ("not-a-directory", -2),
            ("bad-request", -3),
            ("io-error", -4),
            ("is-dir", -5),
            ("bad-fh", -6),
            ("no-space", -7),
            ("not-supported", -8),
            ("read-only", -9),
            ("exists", -10),
            ("reply-too-large", -11),
            ("cross-root", -12),
        ];
        let statuses: Vec<(&str, i32)> = Status::ALL
            .iter()
            .map(|&st| (st.name(), st.code()))
            .collect();
        assert_eq!(statuses, sts);
        assert_eq!(OpenFlags::READ.bits(), 1);
        assert_eq!(OpenFlags::WRITE.bits(), 2);
        assert_eq!(OpenFlags::CREATE.bits(), 4);
        assert_eq!(OpenFlags::EXCL.bits(), 8);
        assert_eq!(OpenFlags::TRUNC.bits(), 16);
        assert_eq!(OpenFlags::APPEND.bits(), 32);
        assert_eq!(RequestFlags::READ_BULK.bits(), 1);
        assert_eq!(READ_RESP_BULK_BIT, 0x8000_0000);
        // Bit 0 of the OPEN reply's flags byte (offset 17, after `is_dir`)
        // is `immutable`.
        let open = rep(Reply::Open(OpenResp {
            immutable: true,
            ..OpenResp::default()
        }));
        assert_eq!(open[17], 1);
    }

    #[test]
    fn short_buffers_decode_none() {
        assert!(dreq(Opcode::Open, &[1, 2]).is_none());
        // A payload carrying only the root and no flags is still short: the
        // pre-task-5 `flags|path` shape decoded a 4-byte prefix, so a bare
        // 4-byte buffer must not now look like a valid OPEN.
        assert!(dreq(Opcode::Open, &[0, 0, 0, 0]).is_none());
        assert!(dreq(Opcode::GetAttr, &[1, 2]).is_none());
        assert!(dreq(Opcode::Mkdir, &[0, 0, 0, 0]).is_none());
        assert!(dreq(Opcode::Rename, &[0, 0, 0, 0]).is_none());
        assert!(dreq(Opcode::Read, &[0u8; 10]).is_none());
        assert!(drep(Opcode::Read, &[1, 0, 0]).is_none());
    }

    /// The file-op decoders are deliberately lenient (unlike the registry
    /// ones): trailing bytes are ignored, any non-zero byte is true, and a
    /// READ request may omit its trailing pad. Pinned so the shared cursor
    /// cannot tighten them by accident.
    #[test]
    fn file_op_decoders_stay_lenient() {
        let mut g = rep(Reply::GetAttr(AttrResp {
            found: true,
            is_dir: true,
            size: 5,
            mtime: 6,
        }));
        g[0] = 7;
        g[1] = 0xff;
        g.extend_from_slice(&[9, 9]);
        let Some(Reply::GetAttr(a)) = drep(Opcode::GetAttr, &g) else {
            panic!("a GETATTR reply");
        };
        assert!(a.found && a.is_dir);
        assert_eq!((a.size, a.mtime), (5, 6));

        let mut c = req(Request::Close { fh: 3 });
        c.push(0);
        assert_eq!(dreq(Opcode::Close, &c), Some(Request::Close { fh: 3 }));
        let set = Request::SetAttr { fh: 1, size: 2 };
        let mut s = req(set);
        s.push(0);
        assert_eq!(dreq(Opcode::SetAttr, &s), Some(set));
        let read = Request::Read {
            slot_flags: RequestFlags::empty(),
            fh: 1,
            offset: 2,
            len: 3,
        };
        let r = req(read);
        assert_eq!(dreq(Opcode::Read, &r[..20]), Some(read));
        assert_eq!(
            drep(Opcode::Write, &rep(Reply::Write(4))[..4]),
            Some(Reply::Write(4))
        );
        let mut rr = rep(Reply::Read(ReadReply::Inline(b"xy")));
        rr.push(0);
        assert_eq!(
            drep(Opcode::Read, &rr),
            Some(Reply::Read(ReadReply::Inline(b"xy")))
        );
        let mut o = rep(Reply::Open(OpenResp::default()));
        o[16] = 2;
        o[18] = 0xaa;
        o.push(0);
        assert!(matches!(drep(Opcode::Open, &o), Some(Reply::Open(r)) if r.is_dir));
        let one = [DirEntry {
            name: "a",
            is_dir: true,
            size: 1,
            mtime: 2,
        }];
        let mut d = rep(Reply::ReadDir(DirEntries::new(&one)));
        d.push(0);
        assert!(matches!(drep(Opcode::ReadDir, &d), Some(Reply::ReadDir(l)) if l.len() == 1));
        let mut w = req(Request::Write {
            fh: 1,
            offset: 2,
            data: b"hi",
        });
        w.extend_from_slice(&[7, 7]);
        assert!(matches!(
            dreq(Opcode::Write, &w),
            Some(Request::Write { data: b"hi", .. })
        ));
        let mut rq = req(read);
        rq.push(9);
        assert_eq!(dreq(Opcode::Read, &rq), Some(read));
        // bulk read reply: trailing bytes ignored, and it is not an inline one
        let bulk_reply = Reply::Read(ReadReply::Bulk {
            len: 5,
            arena_offset: 65536,
        });
        let mut bulk = rep(bulk_reply.clone());
        bulk.push(1);
        assert_eq!(drep(Opcode::Read, &bulk), Some(bulk_reply));
        assert!(matches!(
            drep(Opcode::Read, &rep(Reply::Read(ReadReply::Inline(b"ab")))),
            Some(Reply::Read(ReadReply::Inline(_)))
        ));
        // readdir is_dir: any non-zero byte is true
        let file = [DirEntry {
            name: "a",
            is_dir: false,
            size: 1,
            mtime: 2,
        }];
        let mut dd = rep(Reply::ReadDir(DirEntries::new(&file)));
        let at = 4 + 4 + 1;
        dd[at] = 2;
        let Some(Reply::ReadDir(l)) = drep(Opcode::ReadDir, &dd) else {
            panic!("a READDIR reply");
        };
        assert!(l.iter().next().unwrap().is_dir);
        // a READ reply whose data is cut short is still malformed
        assert!(drep(
            Opcode::Read,
            &rep(Reply::Read(ReadReply::Inline(b"abc")))[..10]
        )
        .is_none());
        let abc = req(Request::Write {
            fh: 1,
            offset: 0,
            data: b"abc",
        });
        assert!(dreq(Opcode::Write, &abc[..25]).is_none());
    }

    #[test]
    fn getattr_resp_roundtrip() {
        let r = AttrResp {
            found: true,
            is_dir: false,
            size: 123,
            mtime: -7,
        };
        assert_eq!(
            drep(Opcode::GetAttr, &rep(Reply::GetAttr(r.clone()))),
            Some(Reply::GetAttr(r))
        );
    }

    #[test]
    fn readdir_resp_roundtrip() {
        let entries = [
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
        let b = rep(Reply::ReadDir(DirEntries::new(&entries)));
        let Some(Reply::ReadDir(l)) = drep(Opcode::ReadDir, &b) else {
            panic!("a READDIR reply");
        };
        assert_eq!(l.iter().collect::<Vec<_>>(), entries);
    }

    // ---- registry ops (15-22) ----

    fn sample_node() -> aether_registry::Node {
        use aether_registry::{Child, Node, Value};
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

    fn key_reply(node: Option<&Node>, version: u64) -> Vec<u8> {
        rep(Reply::RegKey(KeyReply {
            version,
            node: node.map(Cow::Borrowed),
        }))
    }

    /// The node and version of a `REG_KEY` reply, or `None` if it does not decode.
    fn decode_key_reply(b: &[u8]) -> Option<(Option<Node>, u64)> {
        let Reply::RegKey(k) = drep(Opcode::RegKey, b)? else {
            return None;
        };
        Some((k.node.map(Cow::into_owned), k.version))
    }

    #[test]
    fn reg_opcodes_are_15_to_22() {
        assert_eq!(
            [
                Opcode::RegLookup,
                Opcode::RegKey,
                Opcode::RegSetValue,
                Opcode::RegDeleteValue,
                Opcode::RegCreateKey,
                Opcode::RegDeleteKey,
                Opcode::RegRenameKey,
                Opcode::RegChanged
            ]
            .map(|op| op as u32),
            [15, 16, 17, 18, 19, 20, 21, 22]
        );
    }

    #[test]
    fn reg_path_roundtrip_and_malformed() {
        let path = |b: &[u8]| match dreq(Opcode::RegKey, b) {
            Some(Request::RegKey { path }) => Some(path.to_string()),
            _ => None,
        };
        let enc = |p: &str| req(Request::RegKey { path: p });
        let p = enc("\\Registry\\Machine\\Sóftware");
        assert_eq!(path(&p).as_deref(), Some("\\Registry\\Machine\\Sóftware"));
        assert_eq!(path(&enc("")).as_deref(), Some(""));
        assert_eq!(path(&[]), None);
        assert_eq!(path(&[1, 0, 0]), None);
        assert_eq!(path(&[5, 0, 0, 0, b'a']), None, "length past end");
        let mut t = enc("a");
        t.push(0);
        assert_eq!(path(&t), None, "trailing bytes");
        assert_eq!(path(&[1, 0, 0, 0, 0xff]), None, "bad utf8");
        assert_eq!(path(&[0xff, 0xff, 0xff, 0xff]), None, "huge length");
    }

    #[test]
    fn reg_set_value_roundtrip_and_malformed() {
        let r = Request::RegSetValue {
            path: "\\Registry\\A",
            name: "Name",
            ty: 3,
            data: &[9, 8, 7],
        };
        let b = req(r);
        assert_eq!(dreq(Opcode::RegSetValue, &b), Some(r));
        let e = Request::RegSetValue {
            path: "p",
            name: "",
            ty: 0,
            data: &[],
        };
        assert_eq!(dreq(Opcode::RegSetValue, &req(e)), Some(e));
        for n in 0..b.len() {
            assert_eq!(dreq(Opcode::RegSetValue, &b[..n]), None, "truncated at {n}");
        }
        let mut t = b.clone();
        t.push(0);
        assert_eq!(dreq(Opcode::RegSetValue, &t), None);
        // data length larger than the registry allows
        let mut big = req(Request::RegSetValue {
            path: "p",
            name: "n",
            ty: 1,
            data: &[],
        });
        let at = big.len() - 4;
        big[at..].copy_from_slice(&((1u32 << 20) + 1).to_le_bytes());
        assert_eq!(dreq(Opcode::RegSetValue, &big), None);
    }

    #[test]
    fn reg_delete_value_and_rename_roundtrip() {
        let d = Request::RegDeleteValue {
            path: "p",
            name: "v",
        };
        let b = req(d);
        assert_eq!(dreq(Opcode::RegDeleteValue, &b), Some(d));
        assert_eq!(dreq(Opcode::RegDeleteValue, &b[..b.len() - 1]), None);
        let k = Request::RegRenameKey {
            path: "\\Registry\\A",
            new_leaf: "B",
        };
        let r = req(k);
        assert_eq!(dreq(Opcode::RegRenameKey, &r), Some(k));
        assert_eq!(dreq(Opcode::RegRenameKey, &r[..3]), None);
        let mut t = r.clone();
        t.push(1);
        assert_eq!(dreq(Opcode::RegRenameKey, &t), None);
    }

    #[test]
    fn reg_create_key_roundtrip_and_malformed() {
        for volatile in [false, true] {
            let r = Request::RegCreateKey {
                path: "p",
                volatile,
            };
            assert_eq!(dreq(Opcode::RegCreateKey, &req(r)), Some(r));
        }
        let mut b = req(Request::RegCreateKey {
            path: "p",
            volatile: true,
        });
        *b.last_mut().unwrap() = 2;
        assert_eq!(dreq(Opcode::RegCreateKey, &b), None, "flag must be 0 or 1");
        assert_eq!(dreq(Opcode::RegCreateKey, &b[..b.len() - 1]), None);
    }

    #[test]
    fn reg_changed_roundtrip_and_malformed() {
        let r = Request::RegChanged {
            path: "p",
            subtree: true,
            since: 77,
        };
        assert_eq!(dreq(Opcode::RegChanged, &req(r)), Some(r));
        let r = Request::RegChanged {
            path: "p",
            subtree: false,
            since: u64::MAX,
        };
        let b = req(r);
        assert_eq!(dreq(Opcode::RegChanged, &b), Some(r));
        for n in 0..b.len() {
            assert_eq!(dreq(Opcode::RegChanged, &b[..n]), None);
        }
        let reply = Reply::RegChanged(ChangedReply {
            version: 5,
            changed: true,
        });
        let r = rep(reply.clone());
        assert_eq!(drep(Opcode::RegChanged, &r), Some(reply));
        assert_eq!(drep(Opcode::RegChanged, &r[..r.len() - 1]), None);
        let mut bad = r.clone();
        bad[8] = 7;
        assert_eq!(drep(Opcode::RegChanged, &bad), None);
    }

    #[test]
    fn reg_version_and_lookup_reply_roundtrip() {
        assert_eq!(
            drep(Opcode::RegSetValue, &rep(Reply::RegVersion(9))),
            Some(Reply::RegVersion(9))
        );
        assert_eq!(drep(Opcode::RegSetValue, &[0; 7]), None);
        assert_eq!(drep(Opcode::RegSetValue, &[0; 9]), None);
        for state in [
            Lookup::Absent,
            Lookup::Present { created: false },
            Lookup::Present { created: true },
            Lookup::Tombstoned,
        ] {
            for below in [false, true] {
                let reply = Reply::RegLookup(LookupReply {
                    version: 42,
                    state,
                    below,
                });
                assert_eq!(drep(Opcode::RegLookup, &rep(reply.clone())), Some(reply));
            }
        }
        let present = rep(Reply::RegLookup(LookupReply {
            version: 1,
            state: Lookup::Present { created: false },
            below: false,
        }));
        let mut b = present.clone();
        b[8] = 4;
        assert_eq!(drep(Opcode::RegLookup, &b), None, "state out of range");
        let mut b = present.clone();
        b[9] = 2;
        assert_eq!(drep(Opcode::RegLookup, &b), None, "bool out of range");
        assert_eq!(drep(Opcode::RegLookup, &b[..9]), None);
    }

    #[test]
    fn reg_key_reply_roundtrip() {
        let n = sample_node();
        let b = key_reply(Some(&n), 12);
        assert_eq!(decode_key_reply(&b), Some((Some(n), 12)));
        let none = key_reply(None, 13);
        assert_eq!(decode_key_reply(&none), Some((None, 13)));
        let empty = Node::default();
        let b = key_reply(Some(&empty), 0);
        assert_eq!(decode_key_reply(&b), Some((Some(empty), 0)));
    }

    #[test]
    fn reg_key_reply_malformed_is_none_never_panics() {
        let b = key_reply(Some(&sample_node()), 12);
        for n in 0..b.len() {
            assert_eq!(decode_key_reply(&b[..n]), None, "truncated at {n}");
        }
        let mut t = b.clone();
        t.push(0);
        assert_eq!(decode_key_reply(&t), None, "trailing bytes");
        let mut bad_tag = b.clone();
        bad_tag[8] = 9;
        assert_eq!(decode_key_reply(&bad_tag), None);
        // a "no node" reply with trailing bytes
        let mut none = key_reply(None, 1);
        none.push(0);
        assert_eq!(decode_key_reply(&none), None);
        // every single-byte corruption and a huge count must not panic
        for i in 0..b.len() {
            let mut c = b.clone();
            c[i] = 0xff;
            let _ = decode_key_reply(&c);
        }
        let mut huge = key_reply(Some(&Node::default()), 0);
        // values count sits after tag, created, volatile, last_write
        let at = 8 + 1 + 1 + 1 + 8;
        huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_key_reply(&huge), None);
    }

    #[test]
    fn reg_key_reply_enforces_registry_limits() {
        use aether_registry::{Child, Value};
        let long_value = Node {
            values: vec![Value {
                name: "x".repeat(16384),
                ty: 1,
                data: vec![],
            }],
            ..Node::default()
        };
        let b = key_reply(Some(&long_value), 1);
        assert_eq!(decode_key_reply(&b), None, "value name over limit");
        let mut long_child = Node::default();
        long_child
            .children
            .insert("k".into(), ("y".repeat(256), Child::Present));
        let b = key_reply(Some(&long_child), 1);
        assert_eq!(decode_key_reply(&b), None, "child name over limit");
        let ok_value = Node {
            values: vec![Value {
                name: "x".repeat(16383),
                ty: 1,
                data: vec![0; 1 << 20],
            }],
            ..Node::default()
        };
        let b = key_reply(Some(&ok_value), 1);
        assert!(decode_key_reply(&b).is_some());
        let too_big = Node {
            values: vec![Value {
                name: "a".into(),
                ty: 1,
                data: vec![0; (1 << 20) + 1],
            }],
            ..Node::default()
        };
        let b = key_reply(Some(&too_big), 1);
        assert_eq!(decode_key_reply(&b), None, "data over limit");
    }
}
