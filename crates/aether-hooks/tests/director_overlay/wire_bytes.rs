//! The bytes the shim's two clients put on the ring are the golden vectors'
//! (`crates/aether-protocol/tests/golden/vectors.txt`): one file request
//! through `FuseClient` and one registry request through `RegClient`.
//!
//! The far side is a raw `RingServer` that records each request's opcode,
//! slot flags and payload and answers `NotFound`, so what is checked is the
//! request exactly as it crossed the ring, not what a director made of it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_hooks::director::FuseClient;
use aether_hooks::regclient::RegClient;
use aether_ipc::{RingServer, SpinNotifier};
use aether_protocol::{Opcode, Status};
use aether_redirect::RootId;
use aether_win::SharedMapping;

/// One golden vector's bytes, by name.
fn golden(name: &str) -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../aether-protocol/tests/golden/vectors.txt"
    );
    let text = std::fs::read_to_string(path).expect("golden vectors");
    let hex = text
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("no golden vector {name}"));
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

type Seen = Arc<Mutex<Vec<(u32, u32, Vec<u8>)>>>;

/// A ring nobody but `serve` answers, served until `want` requests were seen.
fn recording_ring(want: usize) -> (String, usize, Seen) {
    let name = format!("Local\\aether-shim-golden-{}", std::process::id());
    let bytes = 256 * 1024;
    let mapping: &'static SharedMapping = Box::leak(Box::new(
        SharedMapping::create(&name, bytes).expect("section"),
    ));
    aether_ipc::ring::init(mapping.seg(), 8, 4096).expect("ring init");
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        let server = RingServer::new(mapping.seg(), SpinNotifier).expect("ring open");
        let until = Instant::now() + Duration::from_secs(30);
        while log.lock().unwrap().len() < want && Instant::now() < until {
            let _ = server.serve_one(|req| {
                log.lock()
                    .unwrap()
                    .push((req.opcode, req.flags, req.payload.clone()));
                (Status::NotFound.code(), Vec::new())
            });
        }
    });
    (name, bytes, seen)
}

#[test]
fn the_director_and_registry_clients_send_the_golden_bytes() {
    isolate!();
    let (name, bytes, seen) = recording_ring(2);
    let fc = FuseClient::connect(
        &name,
        &[(RootId::DEFAULT, r"C:\aether-golden-root".to_string())],
        4096,
        bytes,
        0,
    )
    .expect("client");

    assert!(fc.open(RootId::DEFAULT, "Data/Skyrim.esm").is_err());
    let reg = RegClient::new(&fc);
    assert!(reg
        .set_value(r"\Registry\Machine\Software", "Name", 3, &[9, 8, 7])
        .is_err());

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            (Opcode::Open as u32, 0, golden("open-req-read-skyrim")),
            (Opcode::RegSetValue as u32, 0, golden("reg-set-value")),
        ]
    );
}
