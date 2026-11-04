//! A `Tap` on the serve loop sees each request after it is decoded and
//! dispatched, and may rewrite the reply before the ring completes the slot
//! (`docs/wire.md` 2.5): the hook the shim's test harness records and
//! corrupts answers through.

use aether_ipc::ring;
use aether_ipc::{OwnedSeg, RequestView, RingClient, RingServer, SpinNotifier};
use aether_protocol::{
    AttrResp, ChangedReply, KeyReply, LookupReply, Opcode, OpenFlags, OpenResp, ReadReply, Request,
    RequestFlags, RootId, SliceBuf, Status,
};
use aether_rpc::{serve_tapped, Director, Listing, ReadSlot, Tap, TapAction};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SLOTS: u32 = 4;
const CAP: u32 = 256;

/// Answers GETATTR with the path's length as the size and HEARTBEAT with
/// success, counts those calls, and refuses everything else.
struct Counting {
    calls: Arc<AtomicU32>,
}

impl Director for Counting {
    fn get_attr(&mut self, _root: RootId, path: &str) -> Result<AttrResp, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AttrResp {
            found: true,
            is_dir: false,
            size: path.len() as u64,
            mtime: 0,
        })
    }
    fn heartbeat(&mut self) -> Result<(), Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn read_dir(&mut self, _: RootId, _: &str, _: &mut Listing) -> Result<(), Status> {
        Err(Status::BadRequest)
    }
    fn open(&mut self, _: RootId, _: OpenFlags, _: &str) -> Result<OpenResp, Status> {
        Err(Status::BadRequest)
    }
    fn read(
        &mut self,
        _: ReadSlot<'_>,
        _: RequestFlags,
        _: u64,
        _: u64,
        _: u32,
    ) -> Result<ReadReply<'_>, Status> {
        Err(Status::BadRequest)
    }
    fn write(&mut self, _: u64, _: u64, _: &[u8]) -> Result<u32, Status> {
        Err(Status::BadRequest)
    }
    fn set_attr(&mut self, _: u64, _: u64) -> Result<(), Status> {
        Err(Status::BadRequest)
    }
    fn rename(&mut self, _: RootId, _: &str, _: &str) -> Result<(), Status> {
        Err(Status::BadRequest)
    }
    fn delete(&mut self, _: RootId, _: &str) -> Result<(), Status> {
        Err(Status::BadRequest)
    }
    fn mkdir(&mut self, _: RootId, _: u32, _: &str) -> Result<(), Status> {
        Err(Status::BadRequest)
    }
    fn close(&mut self, _: u64) -> Result<(), Status> {
        Err(Status::BadRequest)
    }
    fn stored_names(&mut self, _: RootId, _: u32, _: &str) -> Result<&str, Status> {
        Err(Status::BadRequest)
    }
    fn reg_lookup(&mut self, _: &str) -> Result<LookupReply, Status> {
        Err(Status::BadRequest)
    }
    fn reg_key(&mut self, _: &str) -> Result<KeyReply<'_>, Status> {
        Err(Status::BadRequest)
    }
    fn reg_set_value(&mut self, _: &str, _: &str, _: u32, _: &[u8]) -> Result<u64, Status> {
        Err(Status::BadRequest)
    }
    fn reg_delete_value(&mut self, _: &str, _: &str) -> Result<u64, Status> {
        Err(Status::BadRequest)
    }
    fn reg_create_key(&mut self, _: &str, _: bool) -> Result<u64, Status> {
        Err(Status::BadRequest)
    }
    fn reg_delete_key(&mut self, _: &str) -> Result<u64, Status> {
        Err(Status::BadRequest)
    }
    fn reg_rename_key(&mut self, _: &str, _: &str) -> Result<u64, Status> {
        Err(Status::BadRequest)
    }
    fn reg_changed(&mut self, _: &str, _: bool, _: u64) -> Result<ChangedReply, Status> {
        Err(Status::BadRequest)
    }
    fn rename_x(&mut self, _: RootId, _: RootId, _: &str, _: &str) -> Result<(), Status> {
        Err(Status::BadRequest)
    }
    fn stage_exec(&mut self, _: RootId, _: &str) -> Result<&str, Status> {
        Err(Status::BadRequest)
    }
}

/// Runs `f` with a ring client whose requests `serve_tapped` answers from a
/// counting director through `tap`; `f` also gets the director's call count.
fn with_tap<T: Tap + Send, R>(
    tap: T,
    f: impl FnOnce(&RingClient<'_, SpinNotifier>, &AtomicU32) -> R,
) -> R {
    let owned = OwnedSeg::new(64 * 1024);
    ring::init(owned.seg(), SLOTS, CAP).unwrap();
    let stop = AtomicBool::new(false);
    let calls = Arc::new(AtomicU32::new(0));
    std::thread::scope(|s| {
        let served = s.spawn(|| {
            let server = RingServer::new(owned.seg(), SpinNotifier).unwrap();
            let mut d = Counting {
                calls: calls.clone(),
            };
            let mut tap = tap;
            serve_tapped(&server, None, &mut d, &mut tap, &stop)
        });
        // Stops the server however `f` ends, so a failed assertion fails the
        // test instead of leaving the scope waiting on the server.
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
            f(&client, &calls)
        };
        served.join().unwrap().unwrap();
        out
    })
}

/// One raw call: the status and the reply payload, undecoded.
fn call(client: &RingClient<'_, SpinNotifier>, req: &Request<'_>) -> (i32, Vec<u8>) {
    let mut reply = Vec::new();
    let status = client
        .submit_in_place(
            req.opcode() as u32,
            req.flags(),
            |slot| {
                let mut out = SliceBuf::new(slot);
                req.encode(&mut out);
                out.finish().ok()
            },
            &mut reply,
        )
        .unwrap();
    (status, reply)
}

fn getattr(path: &str) -> Request<'_> {
    Request::GetAttr {
        root: RootId(1),
        path,
    }
}

/// A request as the tap saw it: opcode, flags and payload.
type SeenRequest = (u32, u32, Vec<u8>);

/// Each request seen, in order.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<SeenRequest>>>);

impl Tap for Seen {
    fn on_request(&mut self, view: &RequestView<'_>) -> TapAction {
        self.0
            .lock()
            .unwrap()
            .push((view.opcode, view.flags, view.payload.to_vec()));
        TapAction::Pass
    }
}

#[test]
fn a_tap_sees_every_request_with_its_opcode_and_payload() {
    let seen = Seen::default();
    with_tap(seen.clone(), |c, _| {
        call(c, &getattr("a/b"));
        call(c, &Request::Heartbeat {});
        call(c, &getattr("c"));
    });
    let seen = seen.0.lock().unwrap();
    let ops: Vec<u32> = seen.iter().map(|s| s.0).collect();
    assert_eq!(
        ops,
        [
            Opcode::GetAttr as u32,
            Opcode::Heartbeat as u32,
            Opcode::GetAttr as u32
        ]
    );
    let mut expect = Vec::new();
    getattr("a/b").encode(&mut expect);
    assert_eq!(seen[0].2, expect);
    assert!(seen[1].2.is_empty());
}

#[test]
fn an_unknown_opcode_reaches_the_tap_as_its_raw_number() {
    // The tap works on bytes, not on the decoded request.
    let seen = Seen::default();
    with_tap(seen.clone(), |c, _| {
        let mut reply = Vec::new();
        let status = c.submit_in_place(99, 0, |_| Some(0), &mut reply).unwrap();
        assert_eq!(status, Status::BadRequest.code());
    });
    assert_eq!(seen.0.lock().unwrap()[0].0, 99);
}

/// Replaces every reply with a fixed status and bytes.
struct Replace(i32, Vec<u8>);

impl Tap for Replace {
    fn on_reply(&mut self, _: &RequestView<'_>, status: &mut i32, payload: &mut Vec<u8>) {
        *status = self.0;
        payload.clear();
        payload.extend_from_slice(&self.1);
    }
}

#[test]
fn on_reply_can_replace_the_status_and_the_payload() {
    let tap = Replace(Status::IoError.code(), vec![9, 8, 7, 6, 5]);
    with_tap(tap, |c, calls| {
        let (status, reply) = call(c, &getattr("x"));
        assert_eq!(status, Status::IoError.code());
        assert_eq!(reply, [9, 8, 7, 6, 5]);
        // The director still answered; the tap only rewrote its reply.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}

/// Answers every request itself, with an empty payload.
struct AnswerAll(Status);

impl Tap for AnswerAll {
    fn on_request(&mut self, _: &RequestView<'_>) -> TapAction {
        TapAction::Answer {
            status: self.0.code(),
        }
    }
}

#[test]
fn answer_skips_the_director() {
    with_tap(AnswerAll(Status::NotFound), |c, calls| {
        let (status, reply) = call(c, &getattr("x"));
        assert_eq!(status, Status::NotFound.code());
        assert!(reply.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

/// Answers itself, then lets `on_reply` fill the payload.
struct AnswerWith(Vec<u8>);

impl Tap for AnswerWith {
    fn on_request(&mut self, _: &RequestView<'_>) -> TapAction {
        TapAction::Answer { status: 0 }
    }
    fn on_reply(&mut self, _: &RequestView<'_>, _: &mut i32, payload: &mut Vec<u8>) {
        payload.extend_from_slice(&self.0);
    }
}

#[test]
fn an_answered_request_takes_its_payload_from_on_reply() {
    with_tap(AnswerWith(vec![1, 2, 3]), |c, calls| {
        let (status, reply) = call(c, &Request::Heartbeat {});
        assert_eq!(status, 0);
        assert_eq!(reply, [1, 2, 3]);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn a_payload_past_the_cap_becomes_the_overflow_status() {
    // `docs/wire.md` 2.5: a reply longer than `payload_cap` is the ring's
    // overflow, `i32::MIN` with no payload, whoever made it too long.
    with_tap(Replace(0, vec![0xAB; CAP as usize + 1]), |c, _| {
        let (status, reply) = call(c, &getattr("x"));
        assert_eq!(status, i32::MIN);
        assert!(reply.is_empty());
    });
    // A payload of exactly the cap still goes through.
    with_tap(Replace(0, vec![0xAB; CAP as usize]), |c, _| {
        let (status, reply) = call(c, &getattr("x"));
        assert_eq!(status, 0);
        assert_eq!(reply.len(), CAP as usize);
    });
}

/// Appends its name to a shared log in each hook.
struct Named(&'static str, Arc<Mutex<Vec<String>>>);

impl Tap for Named {
    fn on_request(&mut self, _: &RequestView<'_>) -> TapAction {
        self.1.lock().unwrap().push(format!("{} request", self.0));
        TapAction::Pass
    }
    fn on_reply(&mut self, _: &RequestView<'_>, _: &mut i32, _: &mut Vec<u8>) {
        self.1.lock().unwrap().push(format!("{} reply", self.0));
    }
}

#[test]
fn a_pair_of_taps_runs_in_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let pair = (Named("a", log.clone()), Named("b", log.clone()));
    with_tap(pair, |c, _| {
        call(c, &getattr("x"));
    });
    assert_eq!(
        *log.lock().unwrap(),
        ["a request", "b request", "b reply", "a reply"]
    );
}

#[test]
fn in_a_pair_the_first_answer_wins_and_both_taps_still_see_the_request() {
    // A recorder placed first records what a fault layer behind it answers.
    let log = Arc::new(Mutex::new(Vec::new()));
    let pair = (Named("a", log.clone()), AnswerAll(Status::NotFound));
    with_tap(pair, |c, calls| {
        let (status, _) = call(c, &getattr("x"));
        assert_eq!(status, Status::NotFound.code());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
    assert_eq!(*log.lock().unwrap(), ["a request", "a reply"]);
    let log = Arc::new(Mutex::new(Vec::new()));
    let pair = (AnswerAll(Status::IoError), Named("b", log.clone()));
    with_tap(pair, |c, _| {
        let (status, _) = call(c, &getattr("x"));
        assert_eq!(status, Status::IoError.code());
    });
    assert_eq!(*log.lock().unwrap(), ["b request", "b reply"]);
}

/// Panics in the chosen hook on its first request only.
struct PanicOnce {
    in_reply: bool,
    fired: bool,
}

impl Tap for PanicOnce {
    fn on_request(&mut self, _: &RequestView<'_>) -> TapAction {
        if !self.in_reply && !std::mem::replace(&mut self.fired, true) {
            panic!("the tap failed in on_request");
        }
        TapAction::Pass
    }
    fn on_reply(&mut self, _: &RequestView<'_>, _: &mut i32, _: &mut Vec<u8>) {
        if self.in_reply && !std::mem::replace(&mut self.fired, true) {
            panic!("the tap failed in on_reply");
        }
    }
}

// `docs/wire.md` 2.7: a handler that fails must still complete its slot.
// The tap runs inside the ring's handler, so its panic is answered `IoError`
// with no payload and serving goes on.
fn a_panicking_tap_still_completes_its_slot(in_reply: bool) {
    let tap = PanicOnce {
        in_reply,
        fired: false,
    };
    with_tap(tap, |c, _| {
        let (status, reply) = call(c, &getattr("x"));
        assert_eq!(status, Status::IoError.code());
        assert!(reply.is_empty());
        let (status, reply) = call(c, &getattr("x"));
        assert_eq!(status, 0);
        assert!(!reply.is_empty());
    });
}

#[test]
fn a_tap_that_panics_in_on_request_still_completes_its_slot_with_io_error() {
    a_panicking_tap_still_completes_its_slot(false);
}

#[test]
fn a_tap_that_panics_in_on_reply_still_completes_its_slot_with_io_error() {
    a_panicking_tap_still_completes_its_slot(true);
}

#[test]
fn in_a_pair_where_both_answer_the_first_decides() {
    let pair = (AnswerAll(Status::IoError), AnswerAll(Status::NotFound));
    with_tap(pair, |c, _| {
        let (status, _) = call(c, &getattr("x"));
        assert_eq!(status, Status::IoError.code());
    });
}
