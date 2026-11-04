//! `Client` against `serve` over an in-process ring: every opcode's reply
//! comes back typed, and the framing rules of `docs/wire.md` 3.1 and 3.3
//! hold (reserved and unknown opcodes, undecodable payloads, error statuses
//! with empty payloads).

use aether_ipc::ring;
use aether_ipc::{DataArena, IpcError, OwnedSeg, RingClient, RingServer, SpinNotifier};
use aether_protocol::{
    AttrResp, ChangedReply, DirEntries, DirEntry, KeyReply, Lookup, LookupReply, Opcode, OpenFlags,
    OpenResp, ReadReply, Reply, Request, RequestFlags, RootId, Status,
};
use aether_registry::Node;
use aether_rpc::{serve, serve_with_arena, CallError, Client, Director, Listing, ReadSlot};
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const SLOTS: u32 = 4;
const CAP: u32 = 4096;

/// Answers every request from its fields alone: paths come back in the
/// replies that carry text, and their lengths in the ones that carry numbers.
#[derive(Default)]
struct Echo {
    text: String,
    node: Node,
}

impl Echo {
    fn say(&mut self, parts: &[&str]) -> &str {
        self.text.clear();
        for p in parts {
            self.text.push_str(p);
        }
        &self.text
    }
}

fn missing(path: &str) -> Result<(), Status> {
    if path == "missing" {
        Err(Status::NotFound)
    } else {
        Ok(())
    }
}

impl Director for Echo {
    fn get_attr(&mut self, root: RootId, path: &str) -> Result<AttrResp, Status> {
        missing(path)?;
        if path == "panic" {
            panic!("the director failed");
        }
        Ok(AttrResp {
            found: true,
            is_dir: path.ends_with('/'),
            size: path.len() as u64,
            mtime: i64::from(root.0),
        })
    }
    fn read_dir(&mut self, root: RootId, path: &str, out: &mut Listing) -> Result<(), Status> {
        missing(path)?;
        // `wide...`: the name three more times, for a listing too large.
        if path.starts_with("wide") {
            for _ in 0..3 {
                out.push(DirEntry {
                    name: path,
                    is_dir: false,
                    size: 0,
                    mtime: 0,
                });
            }
        }
        out.push(DirEntry {
            name: path,
            is_dir: false,
            size: u64::from(root.0),
            mtime: -1,
        });
        out.push(DirEntry {
            name: "sub",
            is_dir: true,
            size: 0,
            mtime: 7,
        });
        Ok(())
    }
    fn open(&mut self, root: RootId, flags: OpenFlags, path: &str) -> Result<OpenResp, Status> {
        missing(path)?;
        Ok(OpenResp {
            fh: path.len() as u64,
            size: u64::from(flags.bits()),
            is_dir: false,
            immutable: true,
            mount_gen: root.0,
        })
    }
    fn read(
        &mut self,
        at: ReadSlot<'_>,
        slot_flags: RequestFlags,
        fh: u64,
        offset: u64,
        len: u32,
    ) -> Result<ReadReply<'_>, Status> {
        let all = self.say(&["0123456789abcdef"]).as_bytes();
        let from = (offset as usize).min(all.len());
        let to = (from + len as usize).min(all.len());
        if fh == 0 {
            return Err(Status::BadFh);
        }
        // Bulk when asked and the server has an arena: the bytes go in the
        // slot's bank, the slot number in `fh`'s place is checked by the test.
        if let (true, Some(arena)) = (slot_flags.contains(RequestFlags::READ_BULK), at.arena) {
            let src = &all[from..to];
            let (off, n) = arena
                .fill_bank(at.slot, src.len(), |buf| {
                    buf[..src.len()].copy_from_slice(src);
                    Ok(src.len())
                })
                .map_err(|_| Status::IoError)?;
            return Ok(ReadReply::Bulk {
                len: n as u32,
                arena_offset: off,
            });
        }
        Ok(ReadReply::Inline(&all[from..to]))
    }
    fn write(&mut self, fh: u64, _offset: u64, data: &[u8]) -> Result<u32, Status> {
        if fh == 0 {
            return Err(Status::BadFh);
        }
        Ok(data.len() as u32)
    }
    fn set_attr(&mut self, fh: u64, _size: u64) -> Result<(), Status> {
        if fh == 0 {
            return Err(Status::BadFh);
        }
        Ok(())
    }
    fn rename(&mut self, _root: RootId, from: &str, _to: &str) -> Result<(), Status> {
        missing(from)
    }
    fn delete(&mut self, _root: RootId, path: &str) -> Result<(), Status> {
        missing(path)
    }
    fn mkdir(&mut self, _root: RootId, _mode: u32, path: &str) -> Result<(), Status> {
        missing(path)
    }
    fn close(&mut self, fh: u64) -> Result<(), Status> {
        if fh == 0 {
            return Err(Status::BadFh);
        }
        Ok(())
    }
    fn heartbeat(&mut self) -> Result<(), Status> {
        Ok(())
    }
    fn stored_names(&mut self, _root: RootId, skip: u32, path: &str) -> Result<&str, Status> {
        missing(path)?;
        self.text.clear();
        for _ in 0..skip {
            self.text.push_str(path);
        }
        Ok(&self.text)
    }
    fn reg_lookup(&mut self, path: &str) -> Result<LookupReply, Status> {
        Ok(LookupReply {
            version: path.len() as u64,
            state: Lookup::Present { created: true },
            below: true,
        })
    }
    fn reg_key(&mut self, path: &str) -> Result<KeyReply<'_>, Status> {
        // `big:N`: a node with one value tombstone of N bytes.
        if let Some(n) = path.strip_prefix("big:") {
            self.node.value_tombstones = vec!["x".repeat(n.parse().unwrap())];
            return Ok(KeyReply {
                version: 1,
                node: Some(Cow::Borrowed(&self.node)),
            });
        }
        Ok(KeyReply {
            version: path.len() as u64,
            node: None,
        })
    }
    fn reg_set_value(
        &mut self,
        path: &str,
        name: &str,
        ty: u32,
        data: &[u8],
    ) -> Result<u64, Status> {
        Ok((path.len() + name.len() + data.len()) as u64 + u64::from(ty))
    }
    fn reg_delete_value(&mut self, path: &str, name: &str) -> Result<u64, Status> {
        Ok((path.len() + name.len()) as u64)
    }
    fn reg_create_key(&mut self, path: &str, volatile: bool) -> Result<u64, Status> {
        Ok(path.len() as u64 + u64::from(volatile))
    }
    fn reg_delete_key(&mut self, path: &str) -> Result<u64, Status> {
        Ok(path.len() as u64)
    }
    fn reg_rename_key(&mut self, path: &str, new_leaf: &str) -> Result<u64, Status> {
        Ok((path.len() + new_leaf.len()) as u64)
    }
    fn reg_changed(
        &mut self,
        path: &str,
        subtree: bool,
        since: u64,
    ) -> Result<ChangedReply, Status> {
        Ok(ChangedReply {
            version: since + path.len() as u64,
            changed: subtree,
        })
    }
    fn rename_x(
        &mut self,
        _from_root: RootId,
        _to_root: RootId,
        from: &str,
        _to: &str,
    ) -> Result<(), Status> {
        missing(from)
    }
    fn stage_exec(&mut self, _root: RootId, path: &str) -> Result<&str, Status> {
        missing(path)?;
        Ok(self.say(&["C:/staged/", path]))
    }
    /// CLOSE and SETATTR have answers of their own for a payload that does
    /// not decode; every other opcode keeps the default.
    fn undecodable(&mut self, op: Opcode, _flags: u32, _payload: &[u8]) -> Status {
        match op {
            Opcode::Close => Status::NotFound,
            Opcode::SetAttr => Status::Ok,
            _ => Status::BadRequest,
        }
    }
}

/// Runs `f` with a client whose ring `serve` answers from `d` on another
/// thread.
fn with_director<D: Director + Send, R>(d: D, f: impl FnOnce(&Client<'_, SpinNotifier>) -> R) -> R {
    let owned = OwnedSeg::new(64 * 1024);
    ring::init(owned.seg(), SLOTS, CAP).unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let served = s.spawn(|| {
            let server = RingServer::new(owned.seg(), SpinNotifier).unwrap();
            let mut d = d;
            serve(&server, &mut d, &stop)
        });
        // Stops the server however `f` ends, so a failed assertion fails the
        // test instead of leaving the scope waiting on the server.
        struct Stop<'s>(&'s AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let ring = RingClient::new(owned.seg(), SpinNotifier)
            .unwrap()
            .with_deadline(Duration::from_secs(10));
        let out = {
            let _stop = Stop(&stop);
            f(&Client::new(ring))
        };
        served.join().unwrap().unwrap();
        out
    })
}

/// Runs `f` with a client whose ring is answered, once, by `handler` with
/// raw bytes.
fn with_raw_server<R>(
    handler: impl FnOnce(&aether_ipc::Request) -> (i32, Vec<u8>) + Send,
    f: impl FnOnce(&Client<'_, SpinNotifier>) -> R,
) -> R {
    let owned = OwnedSeg::new(64 * 1024);
    ring::init(owned.seg(), SLOTS, CAP).unwrap();
    std::thread::scope(|s| {
        s.spawn(|| {
            let server = RingServer::new(owned.seg(), SpinNotifier).unwrap();
            let mut handler = Some(handler);
            while handler.is_some() {
                server
                    .serve_one(|req| handler.take().unwrap()(req))
                    .unwrap();
            }
        });
        let ring = RingClient::new(owned.seg(), SpinNotifier)
            .unwrap()
            .with_deadline(Duration::from_secs(10));
        f(&Client::new(ring))
    })
}

const R1: RootId = RootId(1);

fn entries() -> [DirEntry<'static>; 2] {
    [
        DirEntry {
            name: "dir",
            is_dir: false,
            size: 1,
            mtime: -1,
        },
        DirEntry {
            name: "sub",
            is_dir: true,
            size: 0,
            mtime: 7,
        },
    ]
}

#[test]
fn every_opcode_gets_its_reply_back() {
    let listing = entries();
    let cases: Vec<(Request<'_>, Reply<'_>)> = vec![
        (
            Request::GetAttr {
                root: R1,
                path: "a/b/",
            },
            Reply::GetAttr(AttrResp {
                found: true,
                is_dir: true,
                size: 4,
                mtime: 1,
            }),
        ),
        (
            Request::ReadDir {
                root: R1,
                path: "dir",
            },
            Reply::ReadDir(DirEntries::new(&listing)),
        ),
        (
            Request::Open {
                root: R1,
                flags: OpenFlags::READ | OpenFlags::WRITE,
                path: "abc",
            },
            Reply::Open(OpenResp {
                fh: 3,
                size: 3,
                is_dir: false,
                immutable: true,
                mount_gen: 1,
            }),
        ),
        (
            Request::Read {
                slot_flags: RequestFlags::empty(),
                fh: 1,
                offset: 2,
                len: 5,
            },
            Reply::Read(ReadReply::Inline(b"23456")),
        ),
        (
            Request::Write {
                fh: 1,
                offset: 0,
                data: b"hello",
            },
            Reply::Write(5),
        ),
        (Request::SetAttr { fh: 1, size: 9 }, Reply::Empty),
        (
            Request::Rename {
                root: R1,
                from: "a",
                to: "b",
            },
            Reply::Empty,
        ),
        (
            Request::Delete {
                root: R1,
                path: "a",
            },
            Reply::Empty,
        ),
        (
            Request::Mkdir {
                root: R1,
                mode: 0o755,
                path: "a",
            },
            Reply::Empty,
        ),
        (Request::Close { fh: 1 }, Reply::Empty),
        (Request::Heartbeat {}, Reply::Empty),
        (
            Request::StoredNames {
                root: R1,
                skip: 2,
                path: "ab/",
            },
            Reply::StoredNames("ab/ab/"),
        ),
        (
            Request::RegLookup { path: "HKLM\\x" },
            Reply::RegLookup(LookupReply {
                version: 6,
                state: Lookup::Present { created: true },
                below: true,
            }),
        ),
        (
            Request::RegKey { path: "HKLM\\x" },
            Reply::RegKey(KeyReply {
                version: 6,
                node: None,
            }),
        ),
        (
            Request::RegSetValue {
                path: "k",
                name: "v",
                ty: 3,
                data: b"xy",
            },
            Reply::RegVersion(7),
        ),
        (
            Request::RegDeleteValue {
                path: "k",
                name: "vv",
            },
            Reply::RegVersion(3),
        ),
        (
            Request::RegCreateKey {
                path: "kk",
                volatile: true,
            },
            Reply::RegVersion(3),
        ),
        (Request::RegDeleteKey { path: "kkk" }, Reply::RegVersion(3)),
        (
            Request::RegRenameKey {
                path: "k",
                new_leaf: "new",
            },
            Reply::RegVersion(4),
        ),
        (
            Request::RegChanged {
                path: "kk",
                subtree: true,
                since: 40,
            },
            Reply::RegChanged(ChangedReply {
                version: 42,
                changed: true,
            }),
        ),
        (
            Request::RenameX {
                from_root: R1,
                to_root: RootId(2),
                from: "a",
                to: "b",
            },
            Reply::Empty,
        ),
        (
            Request::StageExec {
                root: R1,
                path: "game.exe",
            },
            Reply::StageExec("C:/staged/game.exe"),
        ),
    ];
    // One case per opcode, in table order.
    let ops: Vec<_> = cases.iter().map(|(q, _)| q.opcode()).collect();
    assert_eq!(ops, aether_protocol::Opcode::ALL);
    with_director(Echo::default(), |client| {
        let mut scratch = Vec::new();
        for (req, want) in &cases {
            let got = client.call(req, &mut scratch);
            assert_eq!(got.as_ref(), Ok(want), "{:?}", req.opcode());
        }
    });
}

/// Sends `opcode` with `payload` raw and returns the status and reply
/// payload.
fn raw(client: &Client<'_, SpinNotifier>, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
    let r = client.ring().submit(opcode, 0, payload).unwrap();
    (r.status, r.payload)
}

#[test]
fn reserved_and_unknown_opcodes_are_bad_requests_with_empty_payloads() {
    with_director(Echo::default(), |client| {
        for op in [0, 4, 12, 25, 26, 1000, u32::MAX] {
            for payload in [&b""[..], b"\x01\0\0\0abc"] {
                assert_eq!(
                    raw(client, op, payload),
                    (Status::BadRequest.code(), Vec::new()),
                    "opcode {op}"
                );
            }
        }
    });
}

#[test]
fn an_undecodable_request_is_a_bad_request_with_an_empty_payload() {
    with_director(Echo::default(), |client| {
        let getattr = aether_protocol::Opcode::GetAttr as u32;
        let reg_key = aether_protocol::Opcode::RegKey as u32;
        // No root; a path that is not UTF-8; a strict message with a byte
        // left over.
        for (op, payload) in [
            (getattr, &b"\x01\0"[..]),
            (getattr, b"\x01\0\0\0\xff\xfe"),
            (reg_key, b"\x01\0\0\0kx"),
        ] {
            assert_eq!(
                raw(client, op, payload),
                (Status::BadRequest.code(), Vec::new())
            );
        }
    });
}

#[test]
fn an_error_status_reaches_the_client_with_an_empty_payload() {
    with_director(Echo::default(), |client| {
        let mut scratch = Vec::new();
        let req = Request::Delete {
            root: R1,
            path: "missing",
        };
        assert_eq!(
            client.call(&req, &mut scratch),
            Err(CallError::Status(Status::NotFound))
        );
        let read = Request::Read {
            slot_flags: RequestFlags::empty(),
            fh: 0,
            offset: 0,
            len: 4,
        };
        assert_eq!(
            client.call(&read, &mut scratch),
            Err(CallError::Status(Status::BadFh))
        );
        // On the wire: the status and no payload, even for an opcode whose
        // OK reply has one.
        let mut payload = Vec::new();
        Request::GetAttr {
            root: R1,
            path: "missing",
        }
        .encode(&mut payload);
        assert_eq!(
            raw(client, aether_protocol::Opcode::GetAttr as u32, &payload),
            (Status::NotFound.code(), Vec::new())
        );
    });
}

/// The ring's overflow status (`docs/wire.md` 2.5, step 3).
const OVERFLOW: i32 = i32::MIN;

#[test]
fn a_file_reply_larger_than_the_slot_is_the_overflow_status() {
    with_director(Echo::default(), |client| {
        let mut scratch = Vec::new();
        let path = "x".repeat(CAP as usize / 2);
        let names = Request::StoredNames {
            root: R1,
            skip: 3,
            path: &path,
        };
        // READDIR: what wire.md 3.4.2 says both directors answer.
        let wide = format!("wide{path}");
        let listing = Request::ReadDir {
            root: R1,
            path: &wide,
        };
        for req in [names, listing] {
            assert_eq!(
                client.call(&req, &mut scratch),
                Err(CallError::UnknownStatus(OVERFLOW))
            );
            let mut payload = Vec::new();
            req.encode(&mut payload);
            assert_eq!(
                raw(client, req.opcode() as u32, &payload),
                (OVERFLOW, Vec::new())
            );
        }
    });
}

/// The encoded length of the REG_KEY reply `Echo` gives for `big:n`.
fn reg_key_reply_len(n: usize) -> usize {
    let node = Node {
        value_tombstones: vec!["x".repeat(n)],
        ..Node::default()
    };
    let mut out = Vec::new();
    Reply::RegKey(KeyReply {
        version: 1,
        node: Some(Cow::Borrowed(&node)),
    })
    .encode(&mut out);
    out.len()
}

#[test]
fn a_reg_key_reply_longer_than_the_slot_less_8_is_reply_too_large() {
    let cap = CAP as usize;
    let base = reg_key_reply_len(0);
    with_director(Echo::default(), |client| {
        let mut scratch = Vec::new();
        // `payload_cap - 8` fits; one byte more, up to the slot's whole
        // payload and past it, is ReplyTooLarge (wire.md 4.3), never the
        // ring's overflow.
        for (len, want) in [
            (cap - 8, None),
            (cap - 7, Some(Status::ReplyTooLarge)),
            (cap, Some(Status::ReplyTooLarge)),
            (cap + 1, Some(Status::ReplyTooLarge)),
        ] {
            let n = len - base;
            assert_eq!(reg_key_reply_len(n), len);
            let path = format!("big:{n}");
            let got = client.call(&Request::RegKey { path: &path }, &mut scratch);
            match want {
                None => assert!(
                    matches!(got, Ok(Reply::RegKey(_))),
                    "{len}: {:?}",
                    got.err()
                ),
                Some(s) => assert_eq!(got, Err(CallError::Status(s)), "{len}"),
            }
        }
    });
}

#[test]
fn a_request_whose_length_is_past_the_slot_is_answered_bad_request() {
    with_director(Echo::default(), |client| {
        let seg = client.ring().seg();
        let geom = client.ring().geom();
        // Published by hand, with a header claiming more than the slot holds.
        let slot = ring::claim_free(seg, &geom).unwrap();
        let req_id = ring::publish_request(seg, &geom, slot, 1, 0, b"\x01\0\0\0a").unwrap();
        seg.write_bytes(
            geom.slot_off(slot) + aether_ipc::layout::SH_PAYLOAD_LEN,
            &(CAP + 1).to_le_bytes(),
        );
        let began = std::time::Instant::now();
        let answer = loop {
            if let Some(r) = ring::take_response(seg, &geom, slot, req_id).unwrap() {
                break r;
            }
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "slot never completed"
            );
            std::hint::spin_loop();
        };
        ring::free_slot(seg, &geom, slot).unwrap();
        assert_eq!(answer, (Status::BadRequest.code(), Vec::new()));
        // And the server is still serving.
        let req = Request::Heartbeat {};
        assert_eq!(client.call(&req, &mut Vec::new()), Ok(Reply::Empty));
    });
}

#[test]
fn a_director_that_panics_is_answered_io_error_and_serving_goes_on() {
    with_director(Echo::default(), |client| {
        let mut scratch = Vec::new();
        let req = Request::GetAttr {
            root: R1,
            path: "panic",
        };
        assert_eq!(
            client.call(&req, &mut scratch),
            Err(CallError::Status(Status::IoError))
        );
        let req = Request::Heartbeat {};
        assert_eq!(client.call(&req, &mut scratch), Ok(Reply::Empty));
    });
}

#[test]
fn a_request_larger_than_the_slot_fails_and_gives_its_slot_back() {
    with_director(Echo::default(), |client| {
        let mut scratch = Vec::new();
        let path = "x".repeat(CAP as usize + 1);
        let big = Request::GetAttr {
            root: R1,
            path: &path,
        };
        // More times than there are slots: none is kept.
        for _ in 0..SLOTS * 2 {
            assert_eq!(
                client.call(&big, &mut scratch),
                Err(CallError::Ipc(IpcError::PayloadTooLarge))
            );
        }
        let small = Request::GetAttr {
            root: R1,
            path: "ab",
        };
        assert!(matches!(
            client.call(&small, &mut scratch),
            Ok(Reply::GetAttr(_))
        ));
    });
}

#[test]
fn an_ok_reply_that_does_not_decode_is_undecodable() {
    let req = Request::GetAttr {
        root: R1,
        path: "a",
    };
    let got = with_raw_server(
        |_| (Status::Ok.code(), vec![1, 2, 3]),
        |client| client.call(&req, &mut Vec::new()).map(|_| ()),
    );
    assert_eq!(got, Err(CallError::Undecodable));
}

#[test]
fn an_error_status_is_never_decoded() {
    // A payload that would decode as GETATTR's reply, under an error status.
    let mut body = Vec::new();
    Reply::GetAttr(AttrResp {
        found: true,
        is_dir: false,
        size: 1,
        mtime: 1,
    })
    .encode(&mut body);
    let req = Request::GetAttr {
        root: R1,
        path: "a",
    };
    let got = with_raw_server(
        move |_| (Status::IoError.code(), body),
        |client| client.call(&req, &mut Vec::new()).map(|_| ()),
    );
    assert_eq!(got, Err(CallError::Status(Status::IoError)));
}

#[test]
fn a_status_outside_the_table_is_reported_as_it_came() {
    let req = Request::Heartbeat {};
    let got = with_raw_server(
        |_| (77, Vec::new()),
        |client| client.call(&req, &mut Vec::new()).map(|_| ()),
    );
    assert_eq!(got, Err(CallError::UnknownStatus(77)));
}

#[test]
fn the_request_reaches_the_ring_as_its_table_encoding() {
    let req = Request::Read {
        slot_flags: RequestFlags::empty(),
        fh: 9,
        offset: 10,
        len: 11,
    };
    let mut want = Vec::new();
    req.encode(&mut want);
    let seen = std::sync::Mutex::new(None);
    with_raw_server(
        |r| {
            *seen.lock().unwrap() = Some((r.opcode, r.flags, r.payload.clone()));
            (Status::IoError.code(), Vec::new())
        },
        |client| {
            let _ = client.call(&req, &mut Vec::new());
        },
    );
    assert_eq!(
        seen.into_inner().unwrap(),
        Some((aether_protocol::Opcode::Read as u32, 0, want))
    );
}

/// `call` frees its slot before it returns, so it cannot hand back a bulk
/// READ reply whose data is still in that slot's arena bank: such a request
/// is refused before it claims a slot, and nothing reaches the ring.
#[test]
fn a_bulk_read_is_refused_unsent() {
    let owned = OwnedSeg::new(64 * 1024);
    let geom = ring::init(owned.seg(), SLOTS, CAP).unwrap();
    let ring = RingClient::new(owned.seg(), SpinNotifier)
        .unwrap()
        .with_deadline(Duration::from_millis(100));
    let client = Client::new(ring);
    let req = Request::Read {
        slot_flags: RequestFlags::READ_BULK,
        fh: 9,
        offset: 0,
        len: 64 * 1024,
    };
    assert_eq!(
        client.call(&req, &mut Vec::new()).map(|_| ()),
        Err(CallError::BulkRead)
    );
    for slot in 0..SLOTS {
        assert_eq!(
            ring::slot_state(owned.seg(), &geom, slot),
            Some(aether_ipc::layout::ST_FREE),
            "slot {slot} was touched"
        );
    }
    // The same READ without the bulk flag is sent (and, with nobody serving
    // the ring, times out).
    let inline = Request::Read {
        slot_flags: RequestFlags::empty(),
        fh: 9,
        offset: 0,
        len: 16,
    };
    assert_eq!(
        client.call(&inline, &mut Vec::new()).map(|_| ()),
        Err(CallError::Ipc(IpcError::Timeout))
    );
}

#[test]
fn an_undecodable_request_is_answered_by_the_director() {
    with_director(Echo::default(), |client| {
        assert_eq!(
            raw(client, Opcode::Close as u32, b"\x01"),
            (Status::NotFound.code(), Vec::new())
        );
        // OK goes with an empty payload, the one SETATTR has.
        assert_eq!(
            raw(client, Opcode::SetAttr as u32, b"\x01"),
            (Status::Ok.code(), Vec::new())
        );
        assert_eq!(
            raw(client, Opcode::Write as u32, b"\x01"),
            (Status::BadRequest.code(), Vec::new())
        );
    });
}

#[test]
fn a_server_with_an_arena_hands_read_its_slot_and_the_arena() {
    let ring_len = 64 * 1024;
    let bank = 4096;
    let owned = OwnedSeg::new(ring_len + SLOTS as usize * bank);
    ring::init(owned.seg(), SLOTS, CAP).unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let served = s.spawn(|| {
            let server = RingServer::new(owned.seg(), SpinNotifier).unwrap();
            let arena =
                DataArena::new(owned.seg(), ring_len, SLOTS as usize * bank, SLOTS as usize);
            serve_with_arena(&server, Some(&arena), &mut Echo::default(), &stop)
        });
        let client = RingClient::new(owned.seg(), SpinNotifier)
            .unwrap()
            .with_deadline(Duration::from_secs(10));
        let mut body = Vec::new();
        Request::Read {
            slot_flags: RequestFlags::READ_BULK,
            fh: 1,
            offset: 2,
            len: 5,
        }
        .encode(&mut body);
        let r = client
            .submit(Opcode::Read as u32, RequestFlags::READ_BULK.bits(), &body)
            .unwrap();
        stop.store(true, Ordering::Release);
        served.join().unwrap().unwrap();
        assert_eq!(r.status, 0);
        let Ok(Reply::Read(ReadReply::Bulk { len, arena_offset })) =
            Reply::decode(Opcode::Read, 0, &r.payload)
        else {
            panic!("expected a bulk reply, got {:?}", r.payload);
        };
        assert_eq!(len, 5);
        let off = arena_offset as usize;
        assert_eq!((off - ring_len) % bank, 0, "the start of a bank");
        assert!(off < ring_len + SLOTS as usize * bank);
        assert_eq!(owned.seg().read_bytes(off, 5).unwrap(), b"23456");
    });
}
