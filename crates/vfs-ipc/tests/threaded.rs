use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use vfs_ipc::ring::init;
use vfs_ipc::{OwnedSeg, RingClient, RingServer, SpinNotifier};

/// The clients' deadline: longer than any run, so a request ends when it is
/// answered, never because the wall clock ran out. Every thread here spins
/// (`SpinNotifier`), so on a runner with two processors and another test
/// spinning beside this one a client can wait a long time for a slot; with the
/// shipped 60 s ([`vfs_ipc::RESPONSE_DEADLINE`]) it gave up with `RingFull`.
/// Finite, so a request that is never answered still fails the run.
const PATIENT: Duration = Duration::from_secs(30 * 60);

/// Tells the server thread to stop when dropped: when the clients are done,
/// or when one of them failed. Without it a client's panic left the server
/// serving for ever and `thread::scope` waiting on it, and a failing run hung
/// until CI's job timeout.
struct Stop<'a>(&'a AtomicBool);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

// Server echoes: status = opcode, response payload = request payload.
fn run(slot_count: u32, payload_cap: u32, client_threads: u32, per_thread: u32) {
    let owned = OwnedSeg::new(64 * 1024);
    init(owned.seg(), slot_count, payload_cap).unwrap();
    let seg = owned.seg();

    let stop = AtomicBool::new(false);
    let handled = AtomicUsize::new(0);

    thread::scope(|scope| {
        // One server thread draining requests until stop AND drained.
        scope.spawn(|| {
            let server = RingServer::new(seg, SpinNotifier).unwrap();
            loop {
                match server.serve_one(|req| (req.opcode as i32, req.payload.clone())) {
                    Ok(true) => {
                        handled.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let _stop = Stop(&stop);

        // Client threads: each submits `per_thread` requests with a unique payload
        // and asserts the echo matches exactly (no cross-talk / no torn payloads).
        let mut handles = Vec::new();
        for c in 0..client_threads {
            handles.push(scope.spawn(move || {
                let client = RingClient::new(seg, SpinNotifier)
                    .unwrap()
                    .with_deadline(PATIENT);
                for i in 0..per_thread {
                    let opcode = 1000 + c;
                    let payload = format!("c{c}-msg{i}").into_bytes();
                    let resp = client.submit(opcode, 0, &payload).unwrap();
                    assert_eq!(resp.status, opcode as i32, "status mismatch");
                    assert_eq!(resp.payload, payload, "echo mismatch / cross-talk");
                }
            }));
        }
        // All clients done → all requests were answered (submit blocks until
        // then). `_stop` lets the server go, now or on a client's panic.
        for h in handles {
            h.join().unwrap();
        }
    });

    assert_eq!(
        handled.load(Ordering::Relaxed),
        (client_threads * per_thread) as usize,
        "server handled the wrong number of requests"
    );
}

#[test]
fn concurrent_round_trip_no_crosstalk() {
    // Comfortable case: more slots than clients.
    run(8, 256, 4, 2_000);
}

#[test]
fn backpressure_more_clients_than_slots() {
    // Only 2 slots but 8 client threads: submit must spin-claim without deadlock
    // or corruption, and every request is still answered correctly.
    run(2, 128, 8, 500);
}
