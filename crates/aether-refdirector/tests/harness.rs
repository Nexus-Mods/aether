//! The harness layers served over a real ring: what a `Recorder` keeps, what
//! each `Fault` does to the bytes a client gets, what an `Override` changes
//! in an OPEN reply, and the reference's test knobs. The tap chain is the one
//! the shim's fake director uses: `(recorder, (overrides, faults))`.

use aether_ipc::{ring, IpcError, OwnedSeg, RingClient, SharedSeg, SpinNotifier};
use aether_protocol::{
    Opcode, OpenFlags, OpenResp, ReadReply, Reply, Request, RequestFlags, RootId, Status,
};
use aether_refdirector::harness::{
    drop_last_name, serve_threads, set_reserved_open_flag_bits, ArenaShape, BulkLie, Fault, Faults,
    Match, OpenEdit, Override, Recorder, MAX_DELAY,
};
use aether_refdirector::{pattern, ReadStyle, Reference, BULK_THRESHOLD};
use aether_rpc::{CallError, Client};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SLOTS: u32 = 4;
const CAP: u32 = 4096;
const BANK: usize = 256 * 1024;
const R0: RootId = RootId(0);

fn ring_len() -> usize {
    let stride = (32 + CAP as usize).next_multiple_of(8);
    aether_ipc::layout::RING_HEADER_SIZE + SLOTS as usize * stride
}

/// The arena as the server lays it out: one bank per slot after the ring.
fn arena_shape() -> ArenaShape {
    ArenaShape {
        offset: ring_len() as u64,
        bank_size: BANK as u64,
        banks: SLOTS,
    }
}

/// The three layers a test arms, read from the test thread while the serve
/// thread writes them.
struct Rig {
    recorder: Recorder,
    faults: Faults,
    overrides: Override,
}

type C<'a> = Client<'a, SpinNotifier>;

/// Serves `d` behind `(recorder, (overrides, faults))` on a fresh ring with
/// an arena, and runs `f` with a typed client on it.
fn served<R>(d: Reference, f: impl FnOnce(&C<'_>, &Rig) -> R) -> R {
    served_on(1, Recorder::new(), d, |c, rig, _| f(c, rig))
}

/// [`served`] on `threads` serve threads ([`serve_threads`]), recording with
/// `recorder`; `f` also gets the segment, for clients of its own.
fn served_on<R>(
    threads: usize,
    recorder: Recorder,
    d: Reference,
    f: impl FnOnce(&C<'_>, &Rig, &SharedSeg) -> R,
) -> R {
    let owned = Arc::new(OwnedSeg::new(ring_len() + SLOTS as usize * BANK));
    ring::init(owned.seg(), SLOTS, CAP).unwrap();
    let seg = owned.clone();
    d.publish_generation(move |g| ring::publish_reg_generation(seg.seg(), g));
    let rig = Rig {
        recorder,
        faults: Faults::new(CAP).with_arena(arena_shape()),
        overrides: Override::new(),
    };
    let tap = (
        rig.recorder.clone(),
        (rig.overrides.clone(), rig.faults.clone()),
    );
    let stop = AtomicBool::new(false);
    let spin = || SpinNotifier;
    std::thread::scope(|s| {
        let servers = serve_threads(
            s,
            threads,
            owned.seg(),
            Some(arena_shape()),
            &spin,
            &d,
            &tap,
            &stop,
        );
        // Stops the servers however `f` ends, and lets a held delay go, so
        // a failed assertion fails the test instead of hanging the scope.
        struct Stop<'s>(&'s AtomicBool, &'s Faults);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
                self.1.clear();
            }
        }
        let client = Client::new(
            RingClient::new(owned.seg(), SpinNotifier)
                .unwrap()
                .with_deadline(Duration::from_secs(10)),
        );
        let out = {
            let _stop = Stop(&stop, &rig.faults);
            f(&client, &rig, owned.seg())
        };
        for t in servers {
            t.join().unwrap().unwrap();
        }
        out
    })
}

fn getattr(c: &C<'_>, path: &str) -> Result<aether_protocol::AttrResp, CallError> {
    let mut s = Vec::new();
    match c.call(&Request::GetAttr { root: R0, path }, &mut s)? {
        Reply::GetAttr(a) => Ok(a),
        r => panic!("not a GETATTR reply: {r:?}"),
    }
}

fn open_with(c: &C<'_>, flags: OpenFlags, path: &str) -> Result<OpenResp, CallError> {
    let mut s = Vec::new();
    match c.call(
        &Request::Open {
            root: R0,
            flags,
            path,
        },
        &mut s,
    )? {
        Reply::Open(o) => Ok(o),
        r => panic!("not an OPEN reply: {r:?}"),
    }
}

fn open(c: &C<'_>, path: &str) -> OpenResp {
    open_with(c, OpenFlags::READ, path).expect("read open")
}

/// A READ reply, owned.
#[derive(Debug, PartialEq, Eq)]
enum Got {
    Inline(Vec<u8>),
    Bulk { len: u32, arena_offset: u64 },
}

/// A READ through the typed client; `len` of [`BULK_THRESHOLD`] or more is
/// answered in bulk by the reference.
fn read(c: &C<'_>, fh: u64, offset: u64, len: u32) -> Result<Got, CallError> {
    let mut s = Vec::new();
    match c.call(
        &Request::Read {
            slot_flags: RequestFlags::empty(),
            fh,
            offset,
            len,
        },
        &mut s,
    )? {
        Reply::Read(ReadReply::Inline(b)) => Ok(Got::Inline(b.to_vec())),
        Reply::Read(ReadReply::Bulk { len, arena_offset }) => Ok(Got::Bulk { len, arena_offset }),
        r => panic!("not a READ reply: {r:?}"),
    }
}

fn inline(r: Got) -> Vec<u8> {
    match r {
        Got::Inline(b) => b,
        r => panic!("not inline: {r:?}"),
    }
}

fn empty(c: &C<'_>, req: Request<'_>) -> Result<(), CallError> {
    let mut s = Vec::new();
    c.call(&req, &mut s).map(|_| ())
}

fn listing(c: &C<'_>, path: &str) -> Vec<(String, bool, u64, i64)> {
    let mut s = Vec::new();
    match c
        .call(&Request::ReadDir { root: R0, path }, &mut s)
        .unwrap()
    {
        Reply::ReadDir(es) => es
            .iter()
            .map(|e| (e.name.to_string(), e.is_dir, e.size, e.mtime))
            .collect(),
        r => panic!("not a READDIR reply: {r:?}"),
    }
}

fn stored_names(c: &C<'_>, path: &str) -> String {
    let mut s = Vec::new();
    match c
        .call(
            &Request::StoredNames {
                root: R0,
                skip: 0,
                path,
            },
            &mut s,
        )
        .unwrap()
    {
        Reply::StoredNames(n) => n.to_string(),
        r => panic!("not a STORED_NAMES reply: {r:?}"),
    }
}

fn on(op: Opcode) -> Match {
    Match {
        op: Some(op),
        ..Match::default()
    }
}

fn small() -> Reference {
    Reference::new()
        .with_dir("Data")
        .with("Data/a.esp", pattern(5000), ReadStyle::Whole)
        .with("other.txt", b"other".to_vec(), ReadStyle::Whole)
}

#[test]
fn the_recorder_keeps_every_request_in_order() {
    served(small(), |c, rig| {
        let m = rig.recorder.mark();
        getattr(c, "data/a.esp").unwrap();
        let fh = open(c, "data/a.esp").fh;
        read(c, fh, 0, 100).unwrap();
        empty(c, Request::Close { fh }).unwrap();
        let got = rig.recorder.since(m);
        let ops: Vec<_> = got.iter().map(|r| r.op()).collect();
        assert_eq!(
            ops,
            [
                Some(Opcode::GetAttr),
                Some(Opcode::Open),
                Some(Opcode::Read),
                Some(Opcode::Close)
            ]
        );
        assert_eq!(rig.recorder.ops_since(m), ops);
        // Each record holds the request as sent and the reply as answered,
        // decodable on the test thread.
        match got[1].request() {
            Some(Request::Open { flags, path, .. }) => {
                assert_eq!((flags, path), (OpenFlags::READ, "data/a.esp"))
            }
            r => panic!("{r:?}"),
        }
        assert!(got.iter().all(|r| r.status == Status::Ok.code()));
        assert_eq!(got[2].reply.len(), 8 + 100);
        // A mark taken later sees nothing earlier.
        let later = rig.recorder.mark();
        assert_eq!(rig.recorder.total_since(later), 0);
        assert_eq!(rig.recorder.total_since(m), 4);
    });
}

#[test]
fn an_unknown_opcode_is_recorded_as_its_number() {
    served(small(), |c, rig| {
        let m = rig.recorder.mark();
        let r = c.ring().submit(1000, 7, b"junk").unwrap();
        assert_eq!(r.status, Status::BadRequest.code());
        let got = rig.recorder.since(m);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].opcode, got[0].flags, got[0].op()), (1000, 7, None));
        assert_eq!(got[0].payload, b"junk");
        assert!(got[0].request().is_none());
    });
}

#[test]
fn count_since_counts_one_opcode_after_a_mark() {
    served(small(), |c, rig| {
        getattr(c, "data/a.esp").unwrap();
        let m = rig.recorder.mark();
        for _ in 0..3 {
            getattr(c, "data/a.esp").unwrap();
        }
        open(c, "other.txt");
        assert_eq!(rig.recorder.count_since(m, Opcode::GetAttr), 3);
        assert_eq!(rig.recorder.count_since(m, Opcode::Open), 1);
        assert_eq!(rig.recorder.count_since(m, Opcode::Read), 0);
        assert_eq!(rig.recorder.total_since(m), 4);
    });
}

#[test]
fn ring_bytes_counts_a_bulk_reads_length() {
    let d = Reference::new().with("big", pattern(300 * 1024), ReadStyle::Whole);
    served(d, |c, rig| {
        let fh = open(c, "big").fh;
        let m = rig.recorder.mark();
        let Got::Bulk { len, .. } = read(c, fh, 0, BULK_THRESHOLD).unwrap() else {
            panic!("not bulk");
        };
        assert_eq!(len, BULK_THRESHOLD);
        // 24 bytes of request, 16 of reply, and the bulk data.
        assert_eq!(rig.recorder.ring_bytes_since(m), 24 + 16 + u64::from(len));
        let m = rig.recorder.mark();
        read(c, fh, 0, 100).unwrap();
        assert_eq!(rig.recorder.ring_bytes_since(m), 24 + 8 + 100);
    });
}

#[test]
fn a_status_fault_fires_on_the_nth_match_only() {
    let d = small().writable_under("Data");
    let tally = d.tally.clone();
    served(d, |c, rig| {
        let id = rig.faults.arm(
            Match {
                nth: Some(2),
                ..on(Opcode::GetAttr)
            },
            Fault::Status(Status::IoError),
        );
        assert!(getattr(c, "data/a.esp").is_ok());
        assert_eq!(
            getattr(c, "data/a.esp"),
            Err(CallError::Status(Status::IoError))
        );
        assert!(getattr(c, "data/a.esp").is_ok());
        assert_eq!(rig.faults.applied(id), 1);
        // A status fault answers without the director: a faulted WRITE
        // changes nothing.
        let fh = open_with(c, OpenFlags::WRITE, "data/a.esp").unwrap().fh;
        let w = rig
            .faults
            .arm(on(Opcode::Write), Fault::RawStatus(Status::NoSpace.code()));
        let data = [1u8; 10];
        assert_eq!(
            empty(
                c,
                Request::Write {
                    fh,
                    offset: 0,
                    data: &data
                }
            ),
            Err(CallError::Status(Status::NoSpace))
        );
        assert_eq!(rig.faults.applied(w), 1);
        assert_eq!(tally.writes("data/a.esp"), 0);
        // Cleared, nothing fires; the counts stay.
        rig.faults.clear();
        assert!(getattr(c, "data/a.esp").is_ok());
        assert_eq!(rig.faults.applied(id), 1);
    });
}

#[test]
fn times_and_a_path_prefix_narrow_the_match() {
    served(small(), |c, rig| {
        let id = rig.faults.arm(
            Match {
                path_prefix: Some("DATA/".into()),
                times: Some(2),
                ..on(Opcode::GetAttr)
            },
            Fault::RawStatus(-99),
        );
        assert!(getattr(c, "other.txt").is_ok());
        assert_eq!(getattr(c, "Data/A.esp"), Err(CallError::UnknownStatus(-99)));
        assert_eq!(getattr(c, "data/a.esp"), Err(CallError::UnknownStatus(-99)));
        assert!(getattr(c, "data/a.esp").is_ok());
        assert_eq!(rig.faults.applied(id), 2);
    });
}

#[test]
fn a_path_prefix_reaches_requests_on_a_handle_opened_there() {
    served(small(), |c, rig| {
        let a = open(c, "data/a.esp").fh;
        let o = open(c, "other.txt").fh;
        let id = rig.faults.arm(
            Match {
                path_prefix: Some("data".into()),
                ..on(Opcode::Read)
            },
            Fault::Status(Status::IoError),
        );
        assert_eq!(read(c, o, 0, 5).map(inline), Ok(b"other".to_vec()));
        assert_eq!(read(c, a, 0, 5), Err(CallError::Status(Status::IoError)));
        assert_eq!(rig.faults.applied(id), 1);
    });
}

#[test]
fn truncate_and_trailing_reach_the_client_as_bytes() {
    let d = small().with_registry();
    served(d, |c, rig| {
        let good = c
            .ring()
            .submit(Opcode::GetAttr as u32, 0, &path_req("data/a.esp"))
            .unwrap()
            .payload;
        assert_eq!(good.len(), 18);

        let t = rig.faults.arm(on(Opcode::GetAttr), Fault::Truncate(10));
        assert_eq!(rig.faults.applied(t), 0, "armed, not yet fired");
        let r = c
            .ring()
            .submit(Opcode::GetAttr as u32, 0, &path_req("data/a.esp"))
            .unwrap();
        assert_eq!((r.status, r.payload), (0, good[..10].to_vec()));
        assert_eq!(rig.faults.applied(t), 1);
        rig.faults.clear();
        rig.faults.arm(on(Opcode::GetAttr), Fault::Truncate(10));
        assert_eq!(getattr(c, "data/a.esp"), Err(CallError::Undecodable));
        rig.faults.clear();

        // Trailing bytes on a file reply reach the client; the file ops
        // decode leniently (wire.md 3.2), so the reply still reads.
        rig.faults
            .arm(on(Opcode::GetAttr), Fault::Trailing(vec![0xEE; 3]));
        let r = c
            .ring()
            .submit(Opcode::GetAttr as u32, 0, &path_req("data/a.esp"))
            .unwrap();
        let mut want = good.clone();
        want.extend_from_slice(&[0xEE; 3]);
        assert_eq!(r.payload, want);
        assert!(getattr(c, "data/a.esp").is_ok());
        rig.faults.clear();

        // A registry reply is strict: trailing bytes fail its decode.
        let mut s = Vec::new();
        let lookup = Request::RegLookup {
            path: "\\Registry\\Machine\\Software",
        };
        assert!(c.call(&lookup, &mut s).is_ok());
        rig.faults
            .arm(on(Opcode::RegLookup), Fault::Trailing(vec![1]));
        assert_eq!(
            c.call(&lookup, &mut s).map(|_| ()),
            Err(CallError::Undecodable)
        );
        rig.faults.clear();

        // Garbage replaces the reply and says OK.
        rig.faults
            .arm(on(Opcode::Open), Fault::Garbage(vec![1, 2, 3]));
        let r = c
            .ring()
            .submit(Opcode::Open as u32, 0, &open_req("data/a.esp"))
            .unwrap();
        assert_eq!((r.status, r.payload), (0, vec![1, 2, 3]));
    });
}

/// GETATTR's request payload for `path` on root 0.
fn path_req(path: &str) -> Vec<u8> {
    let mut b = Vec::new();
    Request::GetAttr { root: R0, path }.encode(&mut b);
    b
}

fn open_req(path: &str) -> Vec<u8> {
    let mut b = Vec::new();
    Request::Open {
        root: R0,
        flags: OpenFlags::READ,
        path,
    }
    .encode(&mut b);
    b
}

#[test]
fn oversize_becomes_overflow() {
    served(small(), |c, rig| {
        let id = rig.faults.arm(on(Opcode::GetAttr), Fault::Oversize);
        let m = rig.recorder.mark();
        assert_eq!(
            getattr(c, "data/a.esp"),
            Err(CallError::UnknownStatus(i32::MIN))
        );
        assert_eq!(rig.faults.applied(id), 1);
        // Recorded as the client got it: the overflow, no payload.
        let r = &rig.recorder.since(m)[0];
        assert_eq!((r.status, r.reply.len(), r.reply_len), (i32::MIN, 0, 0));
        let request = path_req("data/a.esp").len() as u64;
        assert_eq!(rig.recorder.ring_bytes_since(m), request);
    });
}

#[test]
fn a_reply_too_large_is_recorded_as_sent() {
    let d = small().with_registry();
    served(d, |c, rig| {
        let k = "\\Registry\\Machine\\Big";
        let mut s = Vec::new();
        for name in ["a", "b"] {
            let data = vec![0x5A; 3000];
            let set = Request::RegSetValue {
                path: k,
                name,
                ty: 3,
                data: &data,
            };
            c.call(&set, &mut s).unwrap();
        }
        let m = rig.recorder.mark();
        assert_eq!(
            c.call(&Request::RegKey { path: k }, &mut s).map(|_| ()),
            Err(CallError::Status(Status::ReplyTooLarge))
        );
        let r = &rig.recorder.since(m)[0];
        assert_eq!((r.status, r.reply_len), (Status::ReplyTooLarge.code(), 0));
    });
}

#[test]
fn a_request_the_faults_answered_is_recorded_with_the_fault_s_status() {
    served(small(), |c, rig| {
        rig.faults.arm(on(Opcode::GetAttr), Fault::RawStatus(-99));
        let m = rig.recorder.mark();
        assert!(getattr(c, "data/a.esp").is_err());
        let got = rig.recorder.since(m);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].op(), got[0].status), (Some(Opcode::GetAttr), -99));
    });
}

#[test]
fn a_headers_only_recorder_keeps_sizes_not_bytes() {
    let d = Reference::new()
        .with("big", pattern(300 * 1024), ReadStyle::Whole)
        .with("small", pattern(100), ReadStyle::Whole);
    served_on(1, Recorder::headers_only(), d, |c, rig, _| {
        let fh = open(c, "big").fh;
        let m = rig.recorder.mark();
        read(c, fh, 0, BULK_THRESHOLD).unwrap();
        read(c, fh, 0, 100).unwrap();
        let got = rig.recorder.since(m);
        assert_eq!(rig.recorder.ops_since(m), [Some(Opcode::Read); 2]);
        assert!(got
            .iter()
            .all(|r| r.payload.is_empty() && r.reply.is_empty()));
        assert_eq!(
            got.iter()
                .map(|r| (r.request_len, r.reply_len, r.bulk_len))
                .collect::<Vec<_>>(),
            [(24, 16, BULK_THRESHOLD), (24, 108, 0)]
        );
        assert_eq!(
            rig.recorder.ring_bytes_since(m),
            24 + 16 + u64::from(BULK_THRESHOLD) + 24 + 108
        );
        assert!(got[0].request().is_none(), "no bytes to decode");
    });
}

#[test]
fn several_serve_threads_hold_requests_in_flight_at_once() {
    served_on(4, Recorder::new(), small(), |_, rig, seg| {
        let id = rig.faults.arm(on(Opcode::GetAttr), Fault::Delay(MAX_DELAY));
        let m = rig.recorder.mark();
        std::thread::scope(|s| {
            let calls: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(move || {
                        let c = Client::new(
                            RingClient::new(seg, SpinNotifier)
                                .unwrap()
                                .with_deadline(Duration::from_secs(30)),
                        );
                        getattr(&c, "data/a.esp").is_ok()
                    })
                })
                .collect();
            // Four requests held and none answered: four threads serving.
            assert!(rig.faults.wait_applied(id, 4, Duration::from_secs(10)));
            assert_eq!(rig.recorder.in_flight(), 4);
            assert_eq!(rig.recorder.total_since(m), 0);
            rig.faults.release();
            assert!(calls.into_iter().all(|t| t.join().unwrap()));
        });
        assert_eq!(rig.recorder.max_in_flight(), 4);
        assert_eq!(rig.recorder.in_flight(), 0);
        assert!(rig.recorder.wait_for(m, 4, Duration::from_secs(10)));
    });
}

#[test]
fn a_delay_past_the_deadline_is_waited_out_on_the_recorder() {
    // The pattern for a fault case past the client's deadline: the call
    // times out, the test waits for the recorder to show the late reply
    // sent, and the next call is answered. Only the faulted call has the
    // short deadline: the next one goes through the patient client, on the
    // same ring and slots, so a serve thread that waits for a processor
    // cannot fail it.
    served(small(), |c, rig| {
        let impatient = Client::new(
            RingClient::new(c.ring().seg(), SpinNotifier)
                .unwrap()
                .with_deadline(Duration::from_millis(100)),
        );
        let late = rig.faults.arm(
            Match {
                times: Some(1),
                ..on(Opcode::GetAttr)
            },
            Fault::Delay(Duration::from_millis(400)),
        );
        let m = rig.recorder.mark();
        assert!(matches!(
            getattr(&impatient, "data/a.esp"),
            Err(CallError::Ipc(IpcError::Timeout))
        ));
        // The late reply went to an abandoned slot; wait for it, not for
        // a guessed time.
        assert!(rig.recorder.wait_for(m, 1, Duration::from_secs(10)));
        assert!(getattr(c, "data/a.esp").is_ok());
        assert_eq!(
            rig.recorder.ops_since(m),
            [Some(Opcode::GetAttr); 2],
            "the late reply, then the next call's"
        );
        assert_eq!(rig.faults.applied(late), 1, "the next call was held too");
    });
}

#[test]
fn each_bulk_lie_names_an_offset_the_arena_does_not_hold() {
    let d = Reference::new().with("big", pattern(300 * 1024), ReadStyle::Whole);
    let shape = arena_shape();
    let arena_end = shape.offset + shape.bank_size * u64::from(shape.banks);
    served(d, |c, rig| {
        let fh = open(c, "big").fh;
        let bulk = |lie| {
            rig.faults.clear();
            let id = rig.faults.arm(on(Opcode::Read), Fault::BulkOutside(lie));
            let m = rig.recorder.mark();
            let r = read(c, fh, 0, BULK_THRESHOLD).unwrap();
            assert_eq!(rig.faults.applied(id), 1);
            let slot = rig.recorder.since(m)[0].slot;
            match r {
                Got::Bulk { len, arena_offset } => (slot, len, arena_offset),
                r => panic!("not bulk: {r:?}"),
            }
        };
        let (_, len, off) = bulk(BulkLie::PastArena);
        assert!(off >= arena_end, "{off} inside the arena");
        assert!(len > 0);

        let (slot, _, off) = bulk(BulkLie::PastBank);
        assert!(off >= shape.offset && off < arena_end);
        let bank = (off - shape.offset) / shape.bank_size;
        assert_ne!(bank, u64::from(slot % shape.banks), "the slot's own bank");

        let (_, len, off) = bulk(BulkLie::Wrapping);
        assert!(off.checked_add(u64::from(len)).is_none());

        let (slot, len, off) = bulk(BulkLie::LenOverRequest);
        assert!(len > BULK_THRESHOLD);
        assert_eq!(
            off,
            shape.offset + u64::from(slot % shape.banks) * shape.bank_size
        );

        // An inline answer is lied about in bulk too.
        let (_, len, off) = {
            rig.faults.clear();
            rig.faults
                .arm(on(Opcode::Read), Fault::BulkOutside(BulkLie::PastArena));
            let m = rig.recorder.mark();
            let Got::Bulk { len, arena_offset } = read(c, fh, 0, 100).unwrap() else {
                panic!("not bulk");
            };
            (rig.recorder.since(m)[0].slot, len, arena_offset)
        };
        assert_eq!((len, off), (100, arena_end));
    });
}

#[test]
fn a_delay_holds_the_reply() {
    served(small(), |c, rig| {
        let id = rig.faults.arm(on(Opcode::GetAttr), Fault::Delay(MAX_DELAY));
        let m = rig.recorder.mark();
        std::thread::scope(|s| {
            let call = s.spawn(|| getattr(c, "data/a.esp"));
            // Taken, held, not answered: and the faults' lock is free while
            // it sleeps, so the test reads and arms.
            assert!(rig.faults.wait_applied(id, 1, Duration::from_secs(10)));
            assert_eq!(rig.recorder.in_flight(), 1);
            assert_eq!(rig.recorder.total_since(m), 0);
            let other = rig.faults.arm(on(Opcode::Open), Fault::Truncate(0));
            assert_eq!(rig.faults.applied(other), 0);
            rig.faults.release();
            assert!(call.join().unwrap().is_ok());
        });
        assert_eq!(rig.recorder.total_since(m), 1);
    });
}

#[test]
fn a_short_delay_ends_by_itself() {
    served(small(), |c, rig| {
        let id = rig
            .faults
            .arm(on(Opcode::GetAttr), Fault::Delay(Duration::from_millis(20)));
        assert!(getattr(c, "data/a.esp").is_ok());
        assert_eq!(rig.faults.applied(id), 1);
    });
}

#[test]
fn a_rewrite_edits_the_typed_reply() {
    let d = small().with("Data/Sub/B.esp", vec![1], ReadStyle::Whole);
    served(d, |c, rig| {
        assert_eq!(stored_names(c, "data/sub/b.esp"), "Data/Sub/B.esp");
        let id = rig
            .faults
            .arm(on(Opcode::StoredNames), Fault::Rewrite(drop_last_name));
        assert_eq!(stored_names(c, "data/sub/b.esp"), "Data/Sub");
        assert_eq!(rig.faults.applied(id), 1);

        rig.faults.arm(
            on(Opcode::Open),
            Fault::Rewrite(set_reserved_open_flag_bits),
        );
        let m = rig.recorder.mark();
        let o = open(c, "data/a.esp");
        assert!(o.immutable, "bit 0 kept");
        assert_eq!(rig.recorder.since(m)[0].reply[17], 0xFF);
    });
}

#[test]
fn a_panicking_rewrite_leaves_every_layer_working() {
    served(small(), |c, rig| {
        let id = rig.faults.arm(
            Match {
                times: Some(1),
                ..on(Opcode::GetAttr)
            },
            Fault::Rewrite(|_| panic!("a rewrite that panics")),
        );
        // The ring completes the slot (aether-rpc's catch_unwind).
        assert_eq!(
            getattr(c, "data/a.esp"),
            Err(CallError::Status(Status::IoError))
        );
        assert_eq!(rig.faults.applied(id), 1);
        // No lock was poisoned: every layer still answers and records.
        let m = rig.recorder.mark();
        let again = rig.faults.arm(on(Opcode::Open), Fault::Truncate(0));
        rig.overrides.open("", OpenEdit::default());
        assert!(getattr(c, "data/a.esp").is_ok());
        assert_eq!(
            open_with(c, OpenFlags::READ, "data/a.esp"),
            Err(CallError::Undecodable)
        );
        assert_eq!(rig.faults.applied(again), 1);
        assert_eq!(rig.recorder.total_since(m), 2);
    });
}

#[test]
fn an_override_sets_open_fields_and_leaves_the_rest() {
    served(small(), |c, rig| {
        let before = open(c, "data/a.esp");
        assert_eq!((before.immutable, before.mount_gen), (true, 0));
        rig.overrides.open(
            "DATA/",
            OpenEdit {
                immutable: Some(false),
                mount_gen: Some(7),
                ..OpenEdit::default()
            },
        );
        let after = open(c, "Data/A.esp");
        assert_eq!(
            after,
            OpenResp {
                fh: after.fh,
                mount_gen: 7,
                immutable: false,
                ..before
            }
        );
        assert_ne!(after.fh, before.fh, "the director's own handle");
        // Outside the prefix, as the director answered.
        let other = open(c, "other.txt");
        assert_eq!((other.immutable, other.mount_gen, other.size), (true, 0, 5));
        // A later rule for the same prefix adds to the first.
        rig.overrides.open(
            "data/a",
            OpenEdit {
                size: Some(9),
                ..OpenEdit::default()
            },
        );
        let edited = open(c, "data/a.esp");
        assert_eq!(
            (edited.immutable, edited.mount_gen, edited.size),
            (false, 7, 9)
        );
        // A refused open is left alone.
        assert_eq!(
            open_with(c, OpenFlags::READ, "data/missing"),
            Err(CallError::Status(Status::NotFound))
        );
        rig.overrides.clear();
        assert_eq!(open(c, "data/a.esp").mount_gen, 0);
    });
}

#[test]
fn set_mount_gen_reaches_open_replies() {
    let d = small();
    let knob = d.share();
    served(d, |c, _| {
        assert_eq!(open(c, "data/a.esp").mount_gen, 0);
        knob.set_mount_gen(3);
        assert_eq!(open(c, "data/a.esp").mount_gen, 3);
        assert_eq!(open(c, "data").mount_gen, 3, "a directory too");
        knob.set_mount_gen(4);
        assert_eq!(open(c, "other.txt").mount_gen, 4);
    });
}

#[test]
fn replace_content_serves_new_bytes_at_a_new_size() {
    let d = small();
    let knob = d.share();
    served(d, |c, _| {
        let old = open(c, "data/a.esp");
        assert_eq!(old.size, 5000);
        knob.replace_content("Data/A.esp", b"remounted".to_vec());
        let new = open(c, "data/a.esp");
        assert_eq!(new.size, 9);
        assert!(new.immutable);
        assert_eq!(
            read(c, new.fh, 0, 100).map(inline),
            Ok(b"remounted".to_vec())
        );
        assert_eq!(getattr(c, "data/a.esp").unwrap().size, 9);
        // A handle opened before the remount reads the new bytes too.
        assert_eq!(
            read(c, old.fh, 0, 100).map(inline),
            Ok(b"remounted".to_vec())
        );
        // The stored spelling is the one the file had.
        assert_eq!(stored_names(c, "data/a.esp"), "Data/a.esp");
        // A vpath the graph did not hold is added.
        knob.replace_content("Data/New.esp", vec![7; 3]);
        assert_eq!(open(c, "data/new.esp").size, 3);
    });
}

#[test]
fn reissue_fh_gives_the_lowest_closed_handle_again() {
    served(small().reissue_fh(), |c, _| {
        let a = open(c, "data/a.esp").fh;
        let b = open(c, "other.txt").fh;
        assert_eq!((a, b), (1, 2));
        empty(c, Request::Close { fh: b }).unwrap();
        empty(c, Request::Close { fh: a }).unwrap();
        // Unique among open handles (wire.md 3.4.3), and reused.
        let again = open(c, "other.txt").fh;
        assert_eq!(again, a);
        assert_eq!(read(c, again, 0, 5).map(inline), Ok(b"other".to_vec()));
        assert_eq!(open(c, "data/a.esp").fh, b);
        assert_eq!(open(c, "data/a.esp").fh, 3);
    });
    // Without it a closed handle is never issued again.
    served(small(), |c, _| {
        let a = open(c, "data/a.esp").fh;
        empty(c, Request::Close { fh: a }).unwrap();
        assert_eq!(open(c, "data/a.esp").fh, a + 1);
    });
}

#[test]
fn an_override_can_set_the_reserved_open_flag_bits() {
    served(small(), |c, rig| {
        rig.overrides.open(
            "data/",
            OpenEdit {
                reserved_flags: Some(0xFE),
                ..OpenEdit::default()
            },
        );
        let m = rig.recorder.mark();
        let o = open(c, "data/a.esp");
        assert!(o.immutable, "bit 0 as the director sent it");
        assert_eq!(rig.recorder.since(m)[0].reply[17], 0xFF);
        rig.overrides.clear();
        rig.overrides.open(
            "data/",
            OpenEdit {
                immutable: Some(false),
                reserved_flags: Some(0x0E),
                ..OpenEdit::default()
            },
        );
        let m = rig.recorder.mark();
        assert!(!open(c, "data/a.esp").immutable);
        assert_eq!(rig.recorder.since(m)[0].reply[17], 0x0E);
    });
}

#[test]
fn fh_base_zero_issues_zero_first() {
    served(small().with_fh_base(0), |c, _| {
        assert_eq!(open(c, "data/a.esp").fh, 0);
        assert_eq!(open(c, "other.txt").fh, 1);
        assert_eq!(read(c, 0, 0, 3).map(inline), Ok(pattern(3)));
        assert_eq!(empty(c, Request::Close { fh: 0 }), Ok(()));
    });
    let high = (1u64 << 63) + 1;
    served(small().with_fh_base(high), |c, _| {
        assert_eq!(open(c, "data/a.esp").fh, high);
        assert_eq!(read(c, high, 0, 3).map(inline), Ok(pattern(3)));
    });
}

#[test]
fn mkdir_existing_ok_answers_ok() {
    let mkdir = |path: &'static str| Request::Mkdir {
        root: R0,
        mode: 0o755,
        path,
    };
    let d = Reference::new()
        .writable_under("Data")
        .with_dir("Data/Sub")
        .with("Data/f.esp", vec![1], ReadStyle::Whole)
        .with_dir("Sealed")
        .mkdir_existing_ok();
    served(d, |c, _| {
        assert_eq!(empty(c, mkdir("data/sub")), Ok(()));
        assert_eq!(empty(c, mkdir("Data/New")), Ok(()));
        assert_eq!(empty(c, mkdir("data/new")), Ok(()));
        // A file is still a file, and a read-only provider still says it
        // exists.
        assert_eq!(
            empty(c, mkdir("data/f.esp")),
            Err(CallError::Status(Status::IoError))
        );
        assert_eq!(
            empty(c, mkdir("sealed")),
            Err(CallError::Status(Status::Exists))
        );
    });
}

#[test]
fn inline_reads_never_answer_bulk() {
    let d = Reference::new()
        .with("big", pattern(300 * 1024), ReadStyle::Whole)
        .inline_reads(CAP);
    served(d, |c, _| {
        let fh = open(c, "big").fh;
        let b = inline(read(c, fh, 10, BULK_THRESHOLD).unwrap());
        assert_eq!(b, pattern(300 * 1024)[10..10 + CAP as usize - 8].to_vec());
        assert_eq!(inline(read(c, fh, 0, 100).unwrap()), pattern(100));
        // The bulk flag is ignored too.
        let mut body = Vec::new();
        Request::Read {
            slot_flags: RequestFlags::READ_BULK,
            fh,
            offset: 0,
            len: 1000,
        }
        .encode(&mut body);
        let r = c
            .ring()
            .submit(Opcode::Read as u32, RequestFlags::READ_BULK.bits(), &body)
            .unwrap();
        assert_eq!(
            Reply::decode(Opcode::Read, 0, &r.payload).unwrap(),
            Reply::Read(ReadReply::Inline(&pattern(1000)))
        );
    });
}

#[test]
fn staging_answers_the_path() {
    let d = small()
        .with("Bin/Game.exe", vec![0x4D, 0x5A], ReadStyle::Whole)
        .staging("Bin/Game.exe", "C:\\stage\\Game.exe");
    served(d, |c, _| {
        let mut s = Vec::new();
        let stage = |path| Request::StageExec { root: R0, path };
        assert_eq!(
            c.call(&stage("bin/game.exe"), &mut s).unwrap(),
            Reply::StageExec("C:\\stage\\Game.exe")
        );
        assert_eq!(
            c.call(&stage("data/a.esp"), &mut s).map(|_| ()),
            Err(CallError::Status(Status::NotFound))
        );
    });
}

#[test]
fn with_mtime_reaches_getattr_readdir_and_open() {
    let d = small()
        .with_mtime("Data/a.esp", 1_700_000_000)
        .with_mtime("Data", 1_600_000_000);
    served(d, |c, _| {
        assert_eq!(getattr(c, "data/a.esp").unwrap().mtime, 1_700_000_000);
        assert_eq!(getattr(c, "data").unwrap().mtime, 1_600_000_000);
        assert_eq!(getattr(c, "other.txt").unwrap().mtime, 0);
        assert_eq!(
            listing(c, "data"),
            [("a.esp".to_string(), false, 5000, 1_700_000_000)]
        );
        assert_eq!(
            listing(c, "."),
            [
                ("Data".to_string(), true, 0, 1_600_000_000),
                ("other.txt".to_string(), false, 5, 0)
            ]
        );
        // OPEN carries no time (wire.md 3.4.3); it answers the same file
        // GETATTR describes.
        let o = open(c, "data/a.esp");
        assert_eq!((o.size, o.is_dir), (5000, false));
    });
}

#[test]
fn a_lower_only_name_is_answered_as_wire_md_3_4_9_says() {
    let d = Reference::new()
        .writable_under("")
        .with_dir("Data")
        .with("Data/Upper.esp", vec![1], ReadStyle::Whole)
        .with_lower("Data/Lower.esp", vec![2, 2])
        .with_lower("Data/Copied.esp", vec![3, 3, 3])
        .with_lower("Data/Shadow.esp", vec![4])
        .with_lower_dir("Data/Gone")
        .with_lower("Data/Gone/child.esp", vec![5])
        .with_lower_dir("Data/Fixed");
    let knob = d.share();
    let rename = |from, to| Request::Rename { root: R0, from, to };
    served(d, |c, _| {
        // DELETE of a lower-only directory with children hides it.
        assert_eq!(
            empty(
                c,
                Request::Delete {
                    root: R0,
                    path: "data/gone"
                }
            ),
            Ok(())
        );
        assert!(!getattr(c, "data/gone").unwrap().found);
        assert!(!getattr(c, "data/gone/child.esp").unwrap().found);
        assert!(!listing(c, "data").iter().any(|e| e.0 == "Gone"));

        // RENAME of a lower-only directory is IO_ERROR; a lower-only file is
        // copied up and renamed.
        assert_eq!(
            empty(c, rename("data/fixed", "data/moved")),
            Err(CallError::Status(Status::IoError))
        );
        assert!(getattr(c, "data/fixed").unwrap().is_dir);
        // A name created under a lower directory copies it up, and a
        // directory a lower holds still does not move (the reference's
        // answer where wire.md speaks only of lower-only directories).
        open_with(
            c,
            OpenFlags::WRITE | OpenFlags::CREATE,
            "Data/Fixed/new.esp",
        )
        .unwrap();
        assert_eq!(
            empty(c, rename("data/fixed", "data/moved")),
            Err(CallError::Status(Status::IoError))
        );
        assert!(getattr(c, "data/fixed/new.esp").unwrap().found);
        assert_eq!(empty(c, rename("data/copied.esp", "Data/Up.esp")), Ok(()));
        assert_eq!(knob.contents("data/up.esp"), Some(vec![3, 3, 3]));
        assert!(!getattr(c, "data/copied.esp").unwrap().found);

        // RENAME to another case of itself keeps the lower's spelling.
        assert_eq!(empty(c, rename("data/lower.esp", "Data/LOWER.ESP")), Ok(()));
        assert_eq!(stored_names(c, "data/lower.esp"), "Data/Lower.esp");
        // An upper name is respelled, as before.
        assert_eq!(empty(c, rename("data/upper.esp", "Data/UPPER.esp")), Ok(()));
        assert_eq!(stored_names(c, "data/upper.esp"), "Data/UPPER.esp");

        // MKDIR over a lower-only file makes the directory, which hides it.
        assert_eq!(
            empty(
                c,
                Request::Mkdir {
                    root: R0,
                    mode: 0o755,
                    path: "Data/Shadow.esp"
                }
            ),
            Ok(())
        );
        let a = getattr(c, "data/shadow.esp").unwrap();
        assert!(a.found && a.is_dir);
        // MKDIR over an upper file is still IO_ERROR (wire.md 3.4.9).
        assert_eq!(
            empty(
                c,
                Request::Mkdir {
                    root: R0,
                    mode: 0o755,
                    path: "Data/Up.esp"
                }
            ),
            Err(CallError::Status(Status::IoError))
        );
        // A write open copies a lower file up: a case-only rename then
        // respells it.
        open_with(c, OpenFlags::WRITE, "data/lower.esp").unwrap();
        assert_eq!(empty(c, rename("data/lower.esp", "Data/lower.esp")), Ok(()));
        assert_eq!(stored_names(c, "data/lower.esp"), "Data/lower.esp");
    });
}

#[test]
fn the_reference_without_knobs_answers_as_before() {
    let d = Reference::new()
        .writable_under("")
        .with_dir("Data")
        .with_dir("Data/Full")
        .with("Data/Full/x.esp", vec![1], ReadStyle::Whole)
        .with("Data/a.esp", pattern(300 * 1024), ReadStyle::Whole);
    served(d, |c, _| {
        let o = open(c, "data/a.esp");
        assert_eq!((o.fh, o.mount_gen), (1, 0));
        assert!(matches!(
            read(c, o.fh, 0, BULK_THRESHOLD).unwrap(),
            Got::Bulk { .. }
        ));
        assert_eq!(getattr(c, "data/a.esp").unwrap().mtime, 0);
        assert!(listing(c, "data").iter().all(|e| e.3 == 0));
        assert_eq!(
            empty(
                c,
                Request::Mkdir {
                    root: R0,
                    mode: 0o755,
                    path: "data/full"
                }
            ),
            Err(CallError::Status(Status::Exists))
        );
        assert_eq!(
            empty(
                c,
                Request::Delete {
                    root: R0,
                    path: "data/full"
                }
            ),
            Err(CallError::Status(Status::IoError))
        );
        assert_eq!(
            empty(
                c,
                Request::Rename {
                    root: R0,
                    from: "data/full",
                    to: "data/elsewhere"
                }
            ),
            Ok(())
        );
        let mut s = Vec::new();
        assert_eq!(
            c.call(
                &Request::StageExec {
                    root: R0,
                    path: "data/a.esp"
                },
                &mut s
            )
            .map(|_| ()),
            Err(CallError::Status(Status::NotSupported))
        );
    });
}
