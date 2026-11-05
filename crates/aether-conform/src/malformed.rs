//! `aether-conform run --malformed`: requests that are not valid requests,
//! sent through the raw ring, and the answers `docs/wire.md` gives them.
//!
//! A scenario names opcodes and fields of the table, so it cannot send a
//! truncated payload, trailing bytes, invalid UTF-8, an opcode outside the
//! table, slot flags other than `read_bulk` on READ, a registry boolean that
//! is neither 0 nor 1, or a slot header claiming more payload than the slot
//! holds. This mode sends each of those on the ring the adapter names
//! ([`aether_ipc::RingClient::submit_many_held`] with the bytes as they are,
//! or [`aether_ipc::ring::publish_claiming`] for the header), checks the
//! answer against what wire.md says, and sends a HEARTBEAT after each to
//! check the director still serves. The tree is built in ([`tree`]): two
//! roots, a writable entry and a registry layer.
//!
//! Every case cites its rule. Where wire.md's 3.5.1 table lets directors
//! answer a short payload differently, the case allows exactly the answers
//! the table lists. An error status must come with an empty payload (3.1:
//! "A director answers every error with an empty payload"; 3.3: "answered `ST_BAD_REQUEST` with an empty payload").

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aether_ipc::{ring, Notifier};
use aether_protocol::{
    AttrResp, Opcode, OpenFlags, OpenResp, ReadReply, Reply, Request, RequestFlags, RootId, Status,
};
use aether_refdirector::tree::{Contents, File, RegKey, RegValue, Registry, Root, Tree};

use crate::adapter::{Adapter, AdapterError};
use crate::attach::Attached;
use crate::compare::{render_expect, render_got, Session};
use crate::run::{ScenarioResult, StepResult};
use crate::scenario::{to_json_string, Expect, OwnedDirEntry, OwnedReply, OwnedRequest, Scenario};

/// The registry key the tree has.
const KEY: &str = r"\Registry\Machine\Software\Conform";

/// The tree the file cases are sent to: root 0 with `data/a.txt` (`abc`)
/// and the writable entry `saves` holding `saves/s.txt` (`s`), and root 1
/// with `r.txt`. No registry layer, which a director need not have (wire.md
/// 4), so a director without one is tested on the file cases too.
pub fn file_tree() -> Tree {
    Tree {
        registry: None,
        ..registry_tree()
    }
}

/// The tree the registry cases are sent to: [`file_tree`]'s roots and a
/// registry layer with [`KEY`] and its value `v`.
pub fn registry_tree() -> Tree {
    let file = |path: &str, text: &str| File {
        path: path.into(),
        contents: Contents::Text(text.into()),
        mtime: 0,
    };
    Tree {
        roots: vec![
            Root {
                id: 0,
                files: vec![file("data/a.txt", "abc"), file("saves/s.txt", "s")],
                dirs: vec![],
                writable: vec!["saves".into()],
                lower: vec![],
            },
            Root {
                id: 1,
                files: vec![file("r.txt", "r")],
                dirs: vec![],
                writable: vec![],
                lower: vec![],
            },
        ],
        registry: Some(Registry {
            keys: vec![RegKey {
                path: KEY.into(),
                volatile: false,
                values: vec![RegValue {
                    name: "v".into(),
                    ty: 4,
                    data: Contents::Hex(vec![1, 0, 0, 0]),
                }],
            }],
        }),
    }
}

/// What a case sends in the slot.
#[derive(Debug, Clone)]
enum Body {
    /// These payload bytes.
    Bytes(Vec<u8>),
    /// A header that claims this many payload bytes and holds none.
    Claiming(u32),
}

/// One malformed request.
#[derive(Debug, Clone)]
struct Case {
    /// The rule and what is sent, as the report shows it.
    what: String,
    opcode: u32,
    flags: u32,
    body: Body,
    expect: Expect,
}

fn encode(r: &OwnedRequest) -> Vec<u8> {
    let mut out = Vec::new();
    r.as_request().encode(&mut out);
    out
}

fn hex(b: &[u8]) -> String {
    if b.len() > 40 {
        let head: String = b[..40].iter().map(|x| format!("{x:02x}")).collect();
        format!("{head}.. ({} bytes)", b.len())
    } else {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}

fn bad() -> Expect {
    Expect::Status(Status::BadRequest)
}

fn either(a: Expect, b: Expect) -> Expect {
    Expect::OneOf(vec![a, b])
}

fn not_found_attr() -> Expect {
    Expect::Reply(OwnedReply::GetAttr(AttrResp {
        found: false,
        is_dir: false,
        size: 0,
        mtime: 0,
    }))
}

fn abc_attr() -> Expect {
    Expect::Reply(OwnedReply::GetAttr(AttrResp {
        found: true,
        is_dir: false,
        size: 3,
        mtime: 0,
    }))
}

fn data(b: &[u8]) -> Expect {
    Expect::Reply(OwnedReply::Read(b.to_vec()))
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A registry `str`: `len:u32 | bytes`.
fn put_str(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

/// Collects cases.
fn adder(v: &mut Vec<Case>) -> impl FnMut(String, u32, u32, Vec<u8>, Expect) + '_ {
    |what, opcode, flags, body, expect| {
        v.push(Case {
            what,
            opcode,
            flags,
            body: Body::Bytes(body),
            expect,
        })
    }
}

/// The registry cases, for [`registry_tree`].
fn registry_cases() -> Vec<Case> {
    let mut v = Vec::new();
    let mut add = adder(&mut v);
    // 3.5.1: the Rust decoders refuse a path that is not UTF-8; a director
    // may decode it with replacement characters and answer the path that
    // gives.
    let mut p = Vec::new();
    put_str(&mut p, b"\\Registry\\Machine\\\xff");
    let substituted = OwnedRequest::RegLookup {
        path: "\\Registry\\Machine\\\u{FFFD}".into(),
    };
    let as_substituted =
        match crate::model::Oracle::new(&registry_tree()).map(|mut o| o.ask(&substituted)) {
            Ok(Ok(r)) => Expect::Reply(r),
            Ok(Err(st)) => Expect::Status(st),
            Err(e) => panic!("the registry tree mounts: {e}"),
        };
    add(
        "wire.md 3.5.1: REG_LOOKUP of a key path that is not UTF-8: ST_BAD_REQUEST, or the \
         lookup of the path with replacement characters"
            .into(),
        Opcode::RegLookup as u32,
        0,
        p,
        either(bad(), as_substituted),
    );

    // 4.1: registry booleans are exactly 0 or 1.
    let create = encode(&OwnedRequest::RegCreateKey {
        path: format!(r"{KEY}\New"),
        volatile: false,
    });
    for b in [2u8, 0xFF] {
        let mut p = create.clone();
        *p.last_mut().expect("a volatile byte") = b;
        add(
            format!("wire.md 4.1: REG_CREATE_KEY with volatile = {b}: ST_BAD_REQUEST"),
            Opcode::RegCreateKey as u32,
            0,
            p,
            bad(),
        );
    }
    let changed = encode(&OwnedRequest::RegChanged {
        path: KEY.into(),
        subtree: false,
        since: 0,
    });
    {
        let mut p = changed.clone();
        let at = p.len() - 9;
        p[at] = 2;
        add(
            "wire.md 4.1: REG_CHANGED with subtree = 2: ST_BAD_REQUEST".into(),
            Opcode::RegChanged as u32,
            0,
            p,
            bad(),
        );
    }

    // 4.1: a registry decoder consumes its input exactly, so trailing bytes
    // are ST_BAD_REQUEST; 3.2, 3.5: a truncated one does not decode either.
    let registry = [
        OwnedRequest::RegLookup { path: KEY.into() },
        OwnedRequest::RegKey { path: KEY.into() },
        OwnedRequest::RegSetValue {
            path: KEY.into(),
            name: "w".into(),
            ty: 4,
            data: vec![1, 0, 0, 0],
        },
        OwnedRequest::RegDeleteValue {
            path: KEY.into(),
            name: "v".into(),
        },
        OwnedRequest::RegCreateKey {
            path: format!(r"{KEY}\Made"),
            volatile: false,
        },
        OwnedRequest::RegDeleteKey {
            path: format!(r"{KEY}\Nope"),
        },
        OwnedRequest::RegRenameKey {
            path: format!(r"{KEY}\Nope"),
            new_leaf: "Other".into(),
        },
        OwnedRequest::RegChanged {
            path: KEY.into(),
            subtree: true,
            since: 0,
        },
    ];
    for r in &registry {
        let op = r.opcode();
        let full = encode(r);
        let mut long = full.clone();
        long.push(0);
        add(
            format!(
                "wire.md 4.1: {} with a trailing byte: ST_BAD_REQUEST",
                op.name()
            ),
            op as u32,
            0,
            long,
            bad(),
        );
        add(
            format!(
                "wire.md 3.2, 3.5: {} cut by one byte: ST_BAD_REQUEST",
                op.name()
            ),
            op as u32,
            0,
            full[..full.len() - 1].to_vec(),
            bad(),
        );
    }
    let mut p = Vec::new();
    put_u32(&mut p, 100);
    p.extend_from_slice(b"\\Regi");
    add(
        "wire.md 3.2, 3.5: REG_LOOKUP whose str len (100) runs past the payload: ST_BAD_REQUEST"
            .into(),
        Opcode::RegLookup as u32,
        0,
        p,
        bad(),
    );
    drop(add);
    v
}

/// The file cases, for a director whose handle `rfh` is `data/a.txt` open
/// for reading and `wfh` is `saves/s.txt` open for writing ([`file_tree`]).
fn file_cases(rfh: u64, wfh: u64) -> Vec<Case> {
    let mut v = Vec::new();
    let mut add = |what: String, op: u32, flags: u32, body: Vec<u8>, expect: Expect| {
        v.push(Case {
            what,
            opcode: op,
            flags,
            body: Body::Bytes(body),
            expect,
        })
    };
    let root0 = RootId(0);
    let getattr = encode(&OwnedRequest::GetAttr {
        root: root0,
        path: "data/a.txt".into(),
    });

    // 3.3: opcodes outside the table.
    for op in [0, 4, 12, 25, 26, 100, 0x7FFF_FFFF, u32::MAX] {
        for (what, body) in [
            ("empty payload", Vec::new()),
            ("a path request", getattr.clone()),
        ] {
            add(
                format!("wire.md 3.3: opcode {op} (not in the table), {what}: ST_BAD_REQUEST"),
                op,
                0,
                body,
                bad(),
            );
        }
    }

    // 3.6: slot flags other than READ's bit 0 are ignored.
    add(
        "wire.md 3.6: GETATTR with slot flags 0xffffffff: as with 0".into(),
        Opcode::GetAttr as u32,
        u32::MAX,
        getattr.clone(),
        abc_attr(),
    );
    add(
        "wire.md 3.6: READDIR with slot flags 0xffffffff: as with 0".into(),
        Opcode::ReadDir as u32,
        u32::MAX,
        encode(&OwnedRequest::ReadDir {
            root: root0,
            path: "data".into(),
        }),
        Expect::Reply(OwnedReply::ReadDir(vec![OwnedDirEntry {
            name: "a.txt".into(),
            is_dir: false,
            size: 3,
            mtime: 0,
        }])),
    );
    add(
        "wire.md 3.6: HEARTBEAT with slot flags 0xffffffff: as with 0".into(),
        Opcode::Heartbeat as u32,
        u32::MAX,
        Vec::new(),
        Expect::Reply(OwnedReply::Empty),
    );
    add(
        "wire.md 3.6: STORED_NAMES with slot flags 0x80000000: as with 0".into(),
        Opcode::StoredNames as u32,
        0x8000_0000,
        encode(&OwnedRequest::StoredNames {
            root: root0,
            skip: 0,
            path: "data/a.txt".into(),
        }),
        Expect::Reply(OwnedReply::StoredNames("data/a.txt".into())),
    );
    let read = |offset: u64, len: u32| {
        encode(&OwnedRequest::Read {
            slot_flags: RequestFlags::empty(),
            fh: rfh,
            offset,
            len,
        })
    };
    add(
        "wire.md 3.6: READ with slot flags 0xfffffffe (bits other than bit 0 ignored)".into(),
        Opcode::Read as u32,
        0xFFFF_FFFE,
        read(0, 16),
        data(b"abc"),
    );
    add(
        "wire.md 3.6: READ with slot flags 0xffffffff (read_bulk and bits ignored)".into(),
        Opcode::Read as u32,
        u32::MAX,
        read(1, 16),
        data(b"bc"),
    );

    // 3.4.4: a READ without its trailing pad.
    for (offset, len, want) in [(0u64, 16u32, &b"abc"[..]), (1, 2, b"bc")] {
        let mut body = read(offset, len);
        body.truncate(20);
        add(
            format!("wire.md 3.4.4: a 20-byte READ (no pad), offset {offset} len {len}: accepted"),
            Opcode::Read as u32,
            0,
            body,
            data(want),
        );
    }

    // 3.5.1: payloads too short for their fixed fields. Each is a prefix of
    // a valid request, so the length is the only thing wrong with it.
    let shorts: Vec<(&str, Opcode, Vec<u8>, usize, Expect)> = vec![
        (
            "GETATTR",
            Opcode::GetAttr,
            getattr.clone(),
            4,
            either(bad(), not_found_attr()),
        ),
        (
            "READDIR",
            Opcode::ReadDir,
            encode(&OwnedRequest::ReadDir {
                root: root0,
                path: "data".into(),
            }),
            4,
            either(bad(), Expect::Status(Status::NotFound)),
        ),
        (
            "OPEN",
            Opcode::Open,
            encode(&OwnedRequest::Open {
                root: root0,
                flags: OpenFlags::READ,
                path: "data/a.txt".into(),
            }),
            8,
            bad(),
        ),
        ("READ", Opcode::Read, read(0, 3), 20, bad()),
        (
            "WRITE",
            Opcode::Write,
            encode(&OwnedRequest::Write {
                fh: wfh,
                offset: 0,
                data: b"s".to_vec(),
            }),
            24,
            bad(),
        ),
        (
            "SETATTR",
            Opcode::SetAttr,
            encode(&OwnedRequest::SetAttr { fh: wfh, size: 1 }),
            16,
            bad(),
        ),
        (
            "CLOSE",
            Opcode::Close,
            encode(&OwnedRequest::Close { fh: rfh }),
            8,
            bad(),
        ),
        (
            "RENAME",
            Opcode::Rename,
            encode(&OwnedRequest::Rename {
                root: root0,
                from: "saves/s.txt".into(),
                to: "saves/t.txt".into(),
            }),
            8,
            bad(),
        ),
        (
            "RENAME_X",
            Opcode::RenameX,
            encode(&OwnedRequest::RenameX {
                from_root: root0,
                to_root: RootId(1),
                from: "saves/s.txt".into(),
                to: "t.txt".into(),
            }),
            12,
            bad(),
        ),
        (
            "DELETE",
            Opcode::Delete,
            encode(&OwnedRequest::Delete {
                root: root0,
                path: "saves/zz.txt".into(),
            }),
            4,
            either(bad(), Expect::Status(Status::NotFound)),
        ),
        (
            "MKDIR",
            Opcode::Mkdir,
            encode(&OwnedRequest::Mkdir {
                root: root0,
                mode: 0o755,
                path: "saves/newdir".into(),
            }),
            8,
            either(bad(), Expect::Status(Status::NotFound)),
        ),
        (
            "STORED_NAMES",
            Opcode::StoredNames,
            encode(&OwnedRequest::StoredNames {
                root: root0,
                skip: 0,
                path: "data/a.txt".into(),
            }),
            8,
            either(bad(), Expect::Status(Status::NotFound)),
        ),
    ];
    for (name, op, full, fixed, expect) in shorts {
        for n in 0..fixed {
            add(
                format!(
                    "wire.md 3.5.1: {name} cut to {n} of its {fixed} fixed bytes: {}",
                    render_expect(&expect)
                ),
                op as u32,
                0,
                full[..n].to_vec(),
                expect.clone(),
            );
        }
    }
    // A WRITE whose data is shorter than its len says (3.4.5).
    let mut w = Vec::new();
    put_u64(&mut w, wfh);
    put_u64(&mut w, 0);
    put_u32(&mut w, 10);
    put_u32(&mut w, 0);
    w.extend_from_slice(b"abc");
    add(
        "wire.md 3.4.5: a WRITE whose len is 10 with 3 data bytes: ST_BAD_REQUEST".into(),
        Opcode::Write as u32,
        0,
        w,
        bad(),
    );
    // A from_len past the end of the payload is short too (3.5.1).
    let mut r = Vec::new();
    put_u32(&mut r, 0);
    put_u32(&mut r, 1000);
    r.extend_from_slice(b"saves/s.txt");
    add(
        "wire.md 3.5.1: a RENAME whose from_len (1000) runs past the payload: ST_BAD_REQUEST"
            .into(),
        Opcode::Rename as u32,
        0,
        r,
        bad(),
    );
    let mut r = Vec::new();
    put_u32(&mut r, 0);
    put_u32(&mut r, 1);
    put_u32(&mut r, 1000);
    r.extend_from_slice(b"saves/s.txt");
    add(
        "wire.md 3.5.1: a RENAME_X whose from_len (1000) runs past the payload: ST_BAD_REQUEST"
            .into(),
        Opcode::RenameX as u32,
        0,
        r,
        bad(),
    );

    // 3.5.1: invalid UTF-8 in a path is ST_BAD_REQUEST, or a director may
    // decode it with replacement characters and carry on, which for these missing paths gives the answer a missing path
    // gets.
    let mut p = Vec::new();
    put_u32(&mut p, 0);
    p.extend_from_slice(b"data/\xff\xfe");
    add(
        "wire.md 3.5.1: GETATTR of a path that is not UTF-8: ST_BAD_REQUEST, or found = 0 for \
         the path with replacement characters"
            .into(),
        Opcode::GetAttr as u32,
        0,
        p,
        either(bad(), not_found_attr()),
    );
    let mut p = Vec::new();
    put_u32(&mut p, 0);
    put_u32(&mut p, OpenFlags::READ.bits());
    p.extend_from_slice(b"data/\xc3");
    add(
        "wire.md 3.5.1: OPEN of a path that is not UTF-8: ST_BAD_REQUEST, or ST_NOT_FOUND for \
         the path with replacement characters"
            .into(),
        Opcode::Open as u32,
        0,
        p,
        either(bad(), Expect::Status(Status::NotFound)),
    );
    let mut p = Vec::new();
    put_u32(&mut p, 0);
    put_u32(&mut p, 7);
    p.extend_from_slice(b"saves/\x80");
    p.extend_from_slice(b"saves/u.txt");
    add(
        "wire.md 3.5.1: RENAME from a path that is not UTF-8: ST_BAD_REQUEST, or ST_NOT_FOUND \
         for the missing path with replacement characters"
            .into(),
        Opcode::Rename as u32,
        0,
        p,
        either(bad(), Expect::Status(Status::NotFound)),
    );
    // 2.5: a header claiming more than the slot holds.
    v.push(Case {
        what: "wire.md 2.5: GETATTR whose header claims payload_cap + 1 bytes: ST_BAD_REQUEST, \
               payload 0"
            .into(),
        opcode: Opcode::GetAttr as u32,
        flags: 0,
        body: Body::Claiming(0),
        expect: bad(),
    });
    v.push(Case {
        what: "wire.md 2.5: GETATTR whose header claims 0xffffffff bytes: ST_BAD_REQUEST, \
               payload 0"
            .into(),
        opcode: Opcode::GetAttr as u32,
        flags: 0,
        body: Body::Claiming(u32::MAX),
        expect: bad(),
    });
    v
}

/// What the director answered.
struct Answer {
    status: i32,
    /// The reply payload.
    payload: Vec<u8>,
    /// A bulk READ reply's data, read from the arena while the slot was held.
    bulk: Option<Vec<u8>>,
}

/// Sends `c` on `ring`. `Err` when the ring gave no answer.
fn send(ring: &Attached, c: &Case) -> Result<Answer, String> {
    match &c.body {
        Body::Bytes(payload) => {
            let client = ring.client();
            let (mut replies, slots) = client
                .ring()
                .submit_many_held(&[(c.opcode, c.flags, payload.clone())])
                .map_err(|e| format!("{e:?}"))?;
            let reply = replies.pop().expect("one reply per request");
            let mut bulk = None;
            if reply.status == Status::Ok.code() && c.opcode == Opcode::Read as u32 {
                if let Ok(Reply::Read(ReadReply::Bulk { len, arena_offset })) =
                    Reply::decode(Opcode::Read, c.flags, &reply.payload)
                {
                    let end = arena_offset.saturating_add(u64::from(len));
                    if end <= ring.ring_bytes() as u64 {
                        bulk = ring.seg().read_bytes(arena_offset as usize, len as usize);
                    }
                }
            }
            client.ring().release_slots(&slots);
            Ok(Answer {
                status: reply.status,
                payload: reply.payload,
                bulk,
            })
        }
        Body::Claiming(extra) => {
            let geom = ring.geom();
            let claimed = if *extra == u32::MAX {
                u32::MAX
            } else {
                geom.payload_cap + 1
            };
            claiming(ring, c.opcode, c.flags, claimed)
        }
    }
}

/// Publishes a header claiming `claimed` payload bytes and waits for its
/// answer, as the ring client waits.
fn claiming(ring: &Attached, opcode: u32, flags: u32, claimed: u32) -> Result<Answer, String> {
    let (seg, geom, wake) = (ring.seg(), ring.geom(), ring.wake());
    let start = Instant::now();
    let slot = loop {
        if let Some(s) = ring::claim_free(seg, &geom) {
            break s;
        }
        if start.elapsed() > ring.deadline() {
            return Err("no free slot".into());
        }
        std::thread::yield_now();
    };
    let req_id = ring::publish_claiming(seg, &geom, slot, opcode, flags, claimed)
        .map_err(|e| format!("{e:?}"))?;
    wake.notify_server();
    loop {
        match ring::take_response(seg, &geom, slot, req_id) {
            Ok(Some((status, payload))) => {
                let _ = ring::free_slot(seg, &geom, slot);
                wake.notify_slot_free();
                return Ok(Answer {
                    status,
                    payload,
                    bulk: None,
                });
            }
            Ok(None) if start.elapsed() <= ring.deadline() => {
                wake.idle_client(slot, start.elapsed());
            }
            other => {
                let _ = ring::abandon(seg, &geom, slot);
                wake.notify_slot_free();
                return Err(match other {
                    Ok(_) => format!(
                        "no answer within {:?}: the slot was not completed (wire.md 2.5: \
                         every taken slot MUST be completed)",
                        ring.deadline()
                    ),
                    Err(e) => format!("{e:?}"),
                });
            }
        }
    }
}

/// The answer as the comparison reads it, or why it is wrong whatever was
/// expected.
fn read_answer(c: &Case, a: &Answer) -> Result<Result<OwnedReply, Status>, String> {
    if a.status != Status::Ok.code() {
        let s = Status::try_from(a.status).map_err(|_| {
            format!(
                "status {}, which the protocol table does not have",
                a.status
            )
        })?;
        if !a.payload.is_empty() {
            return Err(format!(
                "status {} with a {}-byte payload; an error status has an empty payload \
                 (wire.md 3.1)",
                s.name(),
                a.payload.len()
            ));
        }
        return Ok(Err(s));
    }
    let op = Opcode::try_from(c.opcode)
        .map_err(|_| format!("ST_OK to opcode {}, which is not in the table", c.opcode))?;
    let reply = Reply::decode(op, c.flags, &a.payload)
        .map_err(|_| "an OK reply whose payload does not decode (wire.md 3.1)".to_string())?;
    let owned = OwnedReply::from_reply(&reply);
    match (&owned, &a.bulk) {
        (OwnedReply::ReadBulk { .. }, Some(data)) => Ok(Ok(OwnedReply::Read(data.clone()))),
        (OwnedReply::ReadBulk { .. }, None) => {
            Err("a bulk READ reply outside the mapping (wire.md 2.6)".into())
        }
        _ => Ok(Ok(owned)),
    }
}

/// Which tree a mount has, and so which cases it is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mount {
    Files,
    Registry,
}

/// Mounts [`file_tree`] and sends the file cases, then [`registry_tree`] and
/// the registry cases. The result is one scenario named `malformed`, a step
/// per case, numbered across both. `Err` only for an adapter error.
pub fn run(adapter: &mut Adapter, timeout: Duration) -> Result<ScenarioResult, AdapterError> {
    let mut result = ScenarioResult {
        name: "malformed".into(),
        file: PathBuf::from("(built in: aether_conform::malformed)"),
        passed: false,
        steps: Vec::new(),
        error: None,
    };
    for mount in [Mount::Files, Mount::Registry] {
        if let Some(e) = one_mount(adapter, timeout, mount, &mut result.steps)? {
            let what = match mount {
                Mount::Files => "the file cases",
                Mount::Registry => "the registry cases",
            };
            let e = format!("{what}: {e}");
            result.error = Some(match result.error.take() {
                Some(before) => format!("{before}; {e}"),
                None => e,
            });
        }
    }
    result.passed = result.error.is_none() && result.steps.iter().all(|s| s.passed);
    Ok(result)
}

/// Mounts `mount`'s tree, sends its cases onto `out`, and unmounts. What
/// stopped it outside its cases, if anything.
fn one_mount(
    adapter: &mut Adapter,
    timeout: Duration,
    mount: Mount,
    out: &mut Vec<StepResult>,
) -> Result<Option<String>, AdapterError> {
    let file = std::env::temp_dir().join(format!(
        "aether-conform-malformed-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let s = Scenario {
        name: "malformed".into(),
        description: "The tree malformed requests are sent to".into(),
        tree: match mount {
            Mount::Files => file_tree(),
            Mount::Registry => registry_tree(),
        },
        steps: Vec::new(),
    };
    if let Err(e) = std::fs::write(&file, to_json_string(&s)) {
        return Ok(Some(format!("cannot write {}: {e}", file.display())));
    }
    let mounted = adapter.mount(&file);
    let _ = std::fs::remove_file(&file);
    let env = match mounted? {
        Ok(env) => env,
        Err(why) => return Ok(Some(format!("mount refused: {why}"))),
    };
    let error = match Attached::from_env(&env, timeout) {
        Ok(ring) => {
            let e = play(&ring, mount, out)
                .err()
                .map(|e| format!("{}: {e}", ring.describe()));
            drop(ring);
            e
        }
        Err(e) => Some(format!("attach failed: {e}")),
    };
    adapter.unmount()?;
    Ok(error)
}

/// A handle the director opens for `path` with `flags`.
fn open(ring: &Attached, flags: OpenFlags, path: &str) -> Result<u64, String> {
    let mut scratch = Vec::new();
    match ring.client().call(
        &Request::Open {
            root: RootId(0),
            flags,
            path,
        },
        &mut scratch,
    ) {
        Ok(Reply::Open(OpenResp { fh, .. })) => Ok(fh),
        other => Err(format!(
            "OPEN {path} ({flags:?}), to send malformed requests on its handle, gave {other:?}"
        )),
    }
}

fn play(ring: &Attached, mount: Mount, out: &mut Vec<StepResult>) -> Result<(), String> {
    let client = ring.client();
    let mut scratch = Vec::new();
    let heartbeat =
        |scratch: &mut Vec<u8>| client.call(&Request::Heartbeat {}, scratch).map(|_| ());
    heartbeat(&mut scratch).map_err(|e| format!("the HEARTBEAT sent on attach failed: {e}"))?;
    let (cases, handles) = match mount {
        Mount::Files => {
            let rfh = open(ring, OpenFlags::READ, "data/a.txt")?;
            let wfh = open(ring, OpenFlags::WRITE, "saves/s.txt")?;
            (file_cases(rfh, wfh), vec![rfh, wfh])
        }
        Mount::Registry => (registry_cases(), Vec::new()),
    };
    let first = out.len();
    for (i, c) in cases.iter().enumerate() {
        let i = first + i;
        let request = format!(
            "{} [opcode {}, flags {:#x}, payload {}]",
            c.what,
            c.opcode,
            c.flags,
            match &c.body {
                Body::Bytes(b) => hex(b),
                Body::Claiming(_) => "none, header claims more".into(),
            }
        );
        let expected = render_expect(&c.expect);
        let answer = match send(ring, c) {
            Ok(a) => a,
            Err(e) => {
                out.push(StepResult {
                    index: i,
                    request: request.into(),
                    expected,
                    got: format!("no answer: {e}"),
                    passed: false,
                    note: None,
                    answer: None,
                });
                return Err(format!("case {i}: the ring gave no answer: {e}"));
            }
        };
        let (got, mut note) = match read_answer(c, &answer) {
            Ok(got) => {
                let note = Session::new()
                    .check(i, &OwnedRequest::Heartbeat, &c.expect, &got)
                    .err()
                    .map(|m| m.why);
                if let Ok(OwnedReply::Open(o)) = &got {
                    // An OPEN case's handle is not kept.
                    let _ = client.call(&Request::Close { fh: o.fh }, &mut scratch);
                }
                (render_got(&got), note)
            }
            Err(why) => (format!("status {}", answer.status), Some(why)),
        };
        // The director must still serve.
        if let Err(e) = heartbeat(&mut scratch) {
            note = Some(format!(
                "{}the HEARTBEAT after it failed: {e}",
                note.map(|n| format!("{n}; ")).unwrap_or_default()
            ));
        }
        let passed = note.is_none();
        out.push(StepResult {
            index: i,
            request: request.into(),
            expected,
            got,
            passed,
            note,
            answer: None,
        });
    }
    // And still answers a well-formed request as before.
    let n = out.len();
    let got = match client.call(
        &Request::GetAttr {
            root: RootId(0),
            path: "data/a.txt",
        },
        &mut scratch,
    ) {
        Ok(Reply::GetAttr(a)) => Ok(OwnedReply::GetAttr(a)),
        Ok(other) => Ok(OwnedReply::from_reply(&other)),
        Err(aether_rpc::CallError::Status(s)) => Err(s),
        Err(e) => {
            return Err(format!(
                "the GETATTR after the malformed requests failed: {e}"
            ))
        }
    };
    let expect = abc_attr();
    let note = Session::new()
        .check(n, &OwnedRequest::Heartbeat, &expect, &got)
        .err()
        .map(|m| m.why);
    out.push(StepResult {
        index: n,
        request: "after every malformed request: GETATTR data/a.txt, as before".into(),
        expected: render_expect(&expect),
        got: render_got(&got),
        passed: note.is_none(),
        note,
        answer: None,
    });
    for fh in handles {
        let _ = client.call(&Request::Close { fh }, &mut scratch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_registry(op: u32) -> bool {
        (Opcode::RegLookup as u32..=Opcode::RegChanged as u32).contains(&op)
    }

    #[test]
    fn every_case_cites_a_section_and_the_trees_load() {
        let cs: Vec<Case> = file_cases(1, 2)
            .into_iter()
            .chain(registry_cases())
            .collect();
        assert!(cs.len() > 100, "{}", cs.len());
        for c in &cs {
            assert!(c.what.starts_with("wire.md "), "{}", c.what);
        }
        for t in [file_tree(), registry_tree()] {
            aether_refdirector::Reference::from_tree(&t).unwrap();
            t.validate().unwrap();
        }
    }

    /// The file cases go to a tree without a registry layer, which a
    /// director need not have (wire.md 4), and the registry cases to one with.
    #[test]
    fn file_cases_need_no_registry_layer() {
        assert!(file_tree().registry.is_none());
        assert!(registry_tree().registry.is_some());
        assert!(file_cases(1, 2).iter().all(|c| !is_registry(c.opcode)));
        let reg = registry_cases();
        assert!(!reg.is_empty());
        assert!(reg.iter().all(|c| is_registry(c.opcode)));
    }

    /// A director may decode a key path with replacement characters and
    /// answer it (wire.md 3.5.1), so a path that is not
    /// UTF-8 may get the answer the substituted path gets.
    #[test]
    fn a_registry_path_that_is_not_utf8_may_be_substituted() {
        let c = registry_cases()
            .into_iter()
            .find(|c| c.what.contains("not UTF-8"))
            .expect("the case");
        assert!(c.what.contains("3.5.1"), "{}", c.what);
        let Expect::OneOf(alts) = &c.expect else {
            panic!("{:?}", c.expect)
        };
        assert!(alts.contains(&Expect::Status(Status::BadRequest)));
        assert!(
            alts.iter()
                .any(|a| matches!(a, Expect::Reply(OwnedReply::RegLookup(_)))),
            "{alts:?}"
        );
    }

    #[test]
    fn an_error_status_with_a_payload_is_wrong_whatever_was_expected() {
        let c = &file_cases(1, 2)[0];
        let a = Answer {
            status: Status::BadRequest.code(),
            payload: vec![1],
            bulk: None,
        };
        assert!(read_answer(c, &a).unwrap_err().contains("empty payload"));
        let a = Answer {
            status: Status::BadRequest.code(),
            payload: vec![],
            bulk: None,
        };
        assert_eq!(read_answer(c, &a), Ok(Err(Status::BadRequest)));
    }
}
