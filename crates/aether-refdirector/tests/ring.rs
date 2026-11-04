//! `Reference` served over a ring with a bulk arena: the bytes that reach a
//! client are the ones the shim tests' fake director sent, bulk READ
//! included.

use aether_ipc::{ring, DataArena, OwnedSeg, RingClient, RingServer, SpinNotifier};
use aether_protocol::{Opcode, OpenFlags, ReadReply, Reply, Request, RequestFlags, RootId, Status};
use aether_refdirector::{pattern, ReadStyle, Reference};
use aether_rpc::serve_with_arena;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SLOTS: u32 = 4;
const CAP: u32 = 4096;
const BANK: usize = 256 * 1024;

fn ring_bytes() -> usize {
    let stride = (32 + CAP as usize).next_multiple_of(8);
    aether_ipc::layout::RING_HEADER_SIZE + SLOTS as usize * stride
}

/// Serves `d` on a fresh ring whose section has an arena of one bank per
/// slot after the ring, and runs `f` with a client on it.
fn with_ring<R>(d: Reference, f: impl FnOnce(&RingClient<'_, SpinNotifier>, &OwnedSeg) -> R) -> R {
    let owned = Arc::new(OwnedSeg::new(ring_bytes() + SLOTS as usize * BANK));
    ring::init(owned.seg(), SLOTS, CAP).unwrap();
    // A no-op for a reference with no registry layer.
    let seg = owned.clone();
    d.publish_generation(move |g| ring::publish_reg_generation(seg.seg(), g));
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let served = s.spawn(|| {
            let server = RingServer::new(owned.seg(), SpinNotifier).unwrap();
            let arena = DataArena::new(
                owned.seg(),
                ring_bytes(),
                SLOTS as usize * BANK,
                SLOTS as usize,
            );
            let mut d = d;
            serve_with_arena(&server, Some(&arena), &mut d, &stop)
        });
        struct Stop<'s>(&'s AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let client = RingClient::new(owned.seg(), SpinNotifier)
            .unwrap()
            .with_deadline(Duration::from_secs(10));
        let out = {
            let _stop = Stop(&stop);
            f(&client, &owned)
        };
        served.join().unwrap().unwrap();
        out
    })
}

fn call(c: &RingClient<'_, SpinNotifier>, req: Request<'_>) -> (i32, Vec<u8>) {
    let mut body = Vec::new();
    req.encode(&mut body);
    let r = c.submit(req.opcode() as u32, req.flags(), &body).unwrap();
    (r.status, r.payload)
}

fn open(c: &RingClient<'_, SpinNotifier>, path: &str) -> u64 {
    let (st, body) = call(
        c,
        Request::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path,
        },
    );
    assert_eq!(st, Status::Ok.code());
    match Reply::decode(Opcode::Open, 0, &body) {
        Ok(Reply::Open(o)) => o.fh,
        r => panic!("not an OPEN reply: {r:?}"),
    }
}

fn bytes_of(reply: Reply<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    reply.encode(&mut out);
    out
}

#[test]
fn a_bulk_read_reaches_the_client_as_the_fake_sent_it() {
    let bytes = pattern(300 * 1024);
    let d = Reference::new().with("big", bytes.clone(), ReadStyle::Whole);
    let tally = d.tally.clone();
    with_ring(d, |c, owned| {
        let fh = open(c, "big");
        for (flags, len) in [
            (RequestFlags::READ_BULK, 1000),
            (RequestFlags::empty(), 128 * 1024),
            (RequestFlags::empty(), 1024 * 1024),
        ] {
            let (st, body) = call(
                c,
                Request::Read {
                    slot_flags: flags,
                    fh,
                    offset: 7,
                    len,
                },
            );
            assert_eq!(st, Status::Ok.code());
            let Ok(Reply::Read(ReadReply::Bulk {
                len: n,
                arena_offset: off,
            })) = Reply::decode(Opcode::Read, flags.bits(), &body)
            else {
                panic!("not a bulk reply");
            };
            let want = (len as usize).min(BANK);
            assert_eq!(n as usize, want);
            assert_eq!(
                body,
                bytes_of(Reply::Read(ReadReply::Bulk {
                    len: n,
                    arena_offset: off
                }))
            );
            let bank = (off as usize - ring_bytes()) / BANK;
            assert!(bank < SLOTS as usize);
            assert_eq!(off as usize, ring_bytes() + bank * BANK);
            assert_eq!(
                owned.seg().read_bytes(off as usize, want).unwrap(),
                bytes[7..7 + want].to_vec()
            );
        }
    });
    assert_eq!(tally.reads("big"), 3);
    assert_eq!(tally.bulk_reads("big"), 3);
}

#[test]
fn an_inline_read_reaches_the_client_as_the_fake_sent_it() {
    let bytes = pattern(5000);
    let d = Reference::new().with("small", bytes.clone(), ReadStyle::Whole);
    with_ring(d, |c, _| {
        let fh = open(c, "small");
        let (st, body) = call(
            c,
            Request::Read {
                slot_flags: RequestFlags::empty(),
                fh,
                offset: 100,
                len: 4000,
            },
        );
        assert_eq!(st, Status::Ok.code());
        assert_eq!(
            body,
            bytes_of(Reply::Read(ReadReply::Inline(&bytes[100..4100])))
        );
    });
}

#[test]
/// wire.md 3.3: any other opcode is ST_BAD_REQUEST with an empty payload.
fn opcodes_outside_the_table_are_bad_requests() {
    with_ring(Reference::new(), |c, _| {
        for op in [0, 4, 12, 25, 1000] {
            let r = c.submit(op, 0, b"anything").unwrap();
            assert_eq!(
                (r.status, r.payload),
                (Status::BadRequest.code(), vec![]),
                "opcode {op}"
            );
        }
    });
}

#[test]
/// wire.md 3.5: a payload that does not decode is ST_BAD_REQUEST, whatever
/// the opcode; 3.2: file-op decoders ignore trailing bytes, so a HEARTBEAT
/// carrying some is still OK.
fn undecodable_requests_are_bad_requests() {
    let d = Reference::new().with_registry();
    with_ring(d, |c, _| {
        let raw = |op: Opcode, body: &[u8]| {
            let r = c.submit(op as u32, 0, body).unwrap();
            assert!(r.payload.is_empty());
            r.status
        };
        assert_eq!(raw(Opcode::Close, &[1, 2]), Status::BadRequest.code());
        assert_eq!(raw(Opcode::Mkdir, &[1]), Status::BadRequest.code());
        assert_eq!(raw(Opcode::StageExec, &[1]), Status::BadRequest.code());
        assert_eq!(raw(Opcode::GetAttr, &[1]), Status::BadRequest.code());
        assert_eq!(raw(Opcode::RegLookup, &[1]), Status::BadRequest.code());
        assert_eq!(raw(Opcode::Heartbeat, &[1, 2, 3]), Status::Ok.code());
    });
}

#[test]
fn a_reg_key_reply_past_the_slot_is_reply_too_large() {
    let d = Reference::new().with_registry();
    let k = "\\Registry\\Machine\\Big";
    with_ring(d, |c, owned| {
        assert_eq!(ring::reg_generation(owned.seg()), 1);
        for name in ["a", "b"] {
            let data = vec![0x5A; 3000];
            let (st, _) = call(
                c,
                Request::RegSetValue {
                    path: k,
                    name,
                    ty: 3,
                    data: &data,
                },
            );
            assert_eq!(st, Status::Ok.code());
        }
        assert_eq!(ring::reg_generation(owned.seg()), 3, "published per write");
        let (st, body) = call(c, Request::RegKey { path: k });
        assert_eq!((st, body), (Status::ReplyTooLarge.code(), vec![]));
    });
}
