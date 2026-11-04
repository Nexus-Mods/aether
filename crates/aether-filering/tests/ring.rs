//! A request round-trips between two mappings of one file ring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use aether_filering::FileRing;
use aether_ipc::ring;
use aether_ipc::{RingBacking, RingClient, RingServer, SpinNotifier};

const OP_ECHO: u32 = 900;

#[test]
fn a_client_and_a_server_on_two_mappings_round_trip() {
    let dir = aether_testkit::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    let server_side = FileRing::create(&path, 256 * 1024).unwrap();
    assert_eq!(server_side.len(), 256 * 1024);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 256 * 1024);
    let geom = ring::init(server_side.seg(), 4, 4096).unwrap();

    let client_side = FileRing::open(&path).unwrap();
    assert_eq!(client_side.len(), 256 * 1024);
    assert_eq!(ring::open(client_side.seg()).unwrap(), geom);

    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        s.spawn(|| {
            let server = RingServer::new(server_side.seg(), SpinNotifier).unwrap();
            while !stop.load(Ordering::SeqCst) {
                server
                    .serve_one(|req| {
                        assert_eq!(req.opcode, OP_ECHO);
                        (0, req.payload.clone())
                    })
                    .unwrap();
                thread::yield_now();
            }
        });
        let client = RingClient::new(client_side.seg(), SpinNotifier)
            .unwrap()
            .with_deadline(Duration::from_secs(20));
        for i in 0..50u8 {
            let r = client.submit(OP_ECHO, 0, &[i, 1, 2, 3]).unwrap();
            assert_eq!(r.status, 0);
            assert_eq!(r.payload, vec![i, 1, 2, 3]);
        }
        stop.store(true, Ordering::SeqCst);
    });
}

#[test]
fn open_refuses_a_missing_or_empty_file() {
    let dir = aether_testkit::tempdir().unwrap();
    assert!(FileRing::open(&dir.path().join("absent")).is_err());
    let empty = dir.path().join("empty");
    std::fs::write(&empty, []).unwrap();
    assert!(FileRing::open(&empty).is_err());
}

#[test]
fn create_truncates_and_sizes_an_existing_file() {
    let dir = aether_testkit::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    std::fs::write(&path, vec![0xFFu8; 1 << 20]).unwrap();
    let r = FileRing::create(&path, 64 * 1024).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 * 1024);
    // zero-filled, so a fresh ring initialises cleanly
    ring::init(r.seg(), 2, 256).unwrap();
}

#[test]
fn a_short_file_opens_but_the_ring_refuses_its_segment() {
    let dir = aether_testkit::tempdir().unwrap();
    let path = dir.path().join("short.bin");
    std::fs::write(&path, [0xABu8; 10]).unwrap();
    let r = FileRing::open(&path).unwrap();
    assert_eq!(r.len(), 10);
    assert!(matches!(
        ring::open(r.seg()),
        Err(aether_ipc::ring::IpcError::Layout)
    ));
}
