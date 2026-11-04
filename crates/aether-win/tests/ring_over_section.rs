use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use aether_ipc::layout::Opcode;
use aether_ipc::ring::init;
use aether_ipc::{RingClient, RingServer, SpinNotifier};
use aether_protocol::Status;
use aether_win::SharedMapping;

fn section_name(tag: &str) -> String {
    let pid = std::process::id();
    format!("Local\\aether-win-ringtest-{pid}-{tag}")
}

#[test]
fn ring_round_trip_over_real_section() {
    let mapping = SharedMapping::create(&section_name("ring"), 64 * 1024).unwrap();
    init(mapping.seg(), 4, 4096).unwrap();
    let seg = mapping.seg();
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        // Server thread: echo each request's payload back until stopped and idle.
        scope.spawn(|| {
            let ring = RingServer::new(seg, SpinNotifier).unwrap();
            loop {
                match ring.serve_one(|req| (Status::Ok.code(), req.payload.clone())) {
                    Ok(true) => {}
                    Ok(false) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Client (this thread): submit a request over the real shared section.
        let client = RingClient::new(seg, SpinNotifier).unwrap();
        let resp = client
            .submit(Opcode::GetAttr as u32, 0, b"hello-shared-memory")
            .unwrap();
        assert_eq!(resp.status, Status::Ok.code());
        assert_eq!(resp.payload, b"hello-shared-memory");

        stop.store(true, Ordering::Relaxed);
    });
}
