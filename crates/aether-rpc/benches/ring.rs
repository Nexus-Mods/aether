//! The ring's floor in one process: a typed `Client` against `Null` over an
//! `OwnedSeg` with the spin notifier. One round trip for each shape of
//! request (a heartbeat with no payload, GETATTR with a path, a READ whose
//! reply is 4 KiB inline), and the throughput of GETATTR with 1, 2, 4 and 8
//! client threads. What a process boundary adds is measured by the
//! conformance tool's `null-adapter`, not here.
//! Run by hand with `cargo bench -p aether-rpc`.

use aether_ipc::{ring, OwnedSeg, RingClient, RingServer, SpinNotifier};
use aether_protocol::{
    AttrResp, ChangedReply, KeyReply, LookupReply, OpenFlags, OpenResp, ReadReply, Request,
    RequestFlags, RootId, Status,
};
use aether_refdirector::Null;
use aether_rpc::{serve, Client, Director, Listing, ReadSlot};
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Barrier;
use std::time::{Duration, Instant};

const SLOTS: u32 = 8;
const CAP: u32 = 8192;
const BLOCK: usize = 4096;

/// `Null` for everything but READ, which answers 4 KiB inline.
struct Floor {
    null: Null,
    block: Vec<u8>,
}

macro_rules! delegate {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        $(fn $name(&mut self, $($arg: $ty),*) -> $ret {
            self.null.$name($($arg),*)
        })*
    };
}

impl Director for Floor {
    fn read(
        &mut self,
        _: ReadSlot<'_>,
        _: RequestFlags,
        _: u64,
        _: u64,
        len: u32,
    ) -> Result<ReadReply<'_>, Status> {
        Ok(ReadReply::Inline(&self.block[..(len as usize).min(BLOCK)]))
    }
    delegate! {
        get_attr(root: RootId, path: &str) -> Result<AttrResp, Status>;
        read_dir(root: RootId, path: &str, out: &mut Listing) -> Result<(), Status>;
        open(root: RootId, flags: OpenFlags, path: &str) -> Result<OpenResp, Status>;
        write(fh: u64, offset: u64, data: &[u8]) -> Result<u32, Status>;
        set_attr(fh: u64, size: u64) -> Result<(), Status>;
        rename(root: RootId, from: &str, to: &str) -> Result<(), Status>;
        delete(root: RootId, path: &str) -> Result<(), Status>;
        mkdir(root: RootId, mode: u32, path: &str) -> Result<(), Status>;
        close(fh: u64) -> Result<(), Status>;
        heartbeat() -> Result<(), Status>;
        stored_names(root: RootId, skip: u32, path: &str) -> Result<&str, Status>;
        reg_lookup(path: &str) -> Result<LookupReply, Status>;
        reg_key(path: &str) -> Result<KeyReply<'_>, Status>;
        reg_set_value(path: &str, name: &str, ty: u32, data: &[u8]) -> Result<u64, Status>;
        reg_delete_value(path: &str, name: &str) -> Result<u64, Status>;
        reg_create_key(path: &str, volatile: bool) -> Result<u64, Status>;
        reg_delete_key(path: &str) -> Result<u64, Status>;
        reg_rename_key(path: &str, new_leaf: &str) -> Result<u64, Status>;
        reg_changed(path: &str, subtree: bool, since: u64) -> Result<ChangedReply, Status>;
        rename_x(from_root: RootId, to_root: RootId, from: &str, to: &str) -> Result<(), Status>;
        stage_exec(root: RootId, path: &str) -> Result<&str, Status>;
    }
}

fn floor() -> Floor {
    Floor {
        null: Null,
        block: (0..BLOCK).map(|i| i as u8).collect(),
    }
}

/// A ring and a server thread on it for as long as `f` runs.
fn with_server<R>(f: impl FnOnce(&OwnedSeg) -> R) -> R {
    let owned = OwnedSeg::new(1 << 20);
    ring::init(owned.seg(), SLOTS, CAP).unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let server = RingServer::new(owned.seg(), SpinNotifier).unwrap();
            serve(&server, &mut floor(), &stop).unwrap();
        });
        // Stops the server however `f` ends, so a panic fails the bench
        // instead of hanging the scope.
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _stop = Stop(&stop);
        f(&owned)
    })
}

fn client(owned: &OwnedSeg) -> Client<'_, SpinNotifier> {
    Client::new(RingClient::new(owned.seg(), SpinNotifier).unwrap())
}

fn round_trips(c: &mut Criterion) {
    with_server(|owned| {
        let client = client(owned);
        let mut scratch = Vec::with_capacity(CAP as usize);
        let requests = [
            ("heartbeat", Request::Heartbeat {}),
            (
                "getattr",
                Request::GetAttr {
                    root: RootId(0),
                    path: "data/meshes/architecture/whiterun/wrhouse01.nif",
                },
            ),
            (
                "read-4k-inline",
                Request::Read {
                    slot_flags: RequestFlags::empty(),
                    fh: 1,
                    offset: 0,
                    len: BLOCK as u32,
                },
            ),
        ];
        let mut group = c.benchmark_group("ring_round_trip");
        for (name, req) in &requests {
            group.bench_function(*name, |b| {
                b.iter(|| {
                    let reply = client.call(black_box(req), &mut scratch);
                    black_box(reply.is_ok())
                })
            });
        }
        group.finish();
    });
}

fn throughput(c: &mut Criterion) {
    with_server(|owned| {
        let mut group = c.benchmark_group("ring_throughput_getattr");
        group.throughput(Throughput::Elements(1));
        for threads in [1usize, 2, 4, 8] {
            group.bench_function(format!("threads={threads}"), |b| {
                b.iter_custom(|iters| {
                    let per_thread = iters.div_ceil(threads as u64);
                    let gate = Barrier::new(threads);
                    let longest = std::thread::scope(|s| {
                        let handles: Vec<_> = (0..threads)
                            .map(|_| {
                                let gate = &gate;
                                s.spawn(move || {
                                    let client = client(owned);
                                    let req = Request::GetAttr {
                                        root: RootId(0),
                                        path: "data/meshes/a.nif",
                                    };
                                    let mut scratch = Vec::with_capacity(CAP as usize);
                                    gate.wait();
                                    let start = Instant::now();
                                    for _ in 0..per_thread {
                                        black_box(client.call(&req, &mut scratch).is_ok());
                                    }
                                    start.elapsed()
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .map(|h| h.join().unwrap())
                            .max()
                            .unwrap_or_default()
                    });
                    // The run made `per_thread * threads` calls, criterion
                    // asked for `iters`: the time per call is the run's.
                    longest.mul_f64(iters as f64 / (per_thread * threads as u64) as f64)
                })
            });
        }
        group.finish();
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1));
    targets = round_trips, throughput
}
criterion_main!(benches);
