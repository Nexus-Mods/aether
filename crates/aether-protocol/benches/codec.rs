//! The typed codec's cost per golden vector: `encode/<vector>` into a
//! buffer that is reused, and `decode/<vector>` from the encoded bytes.
//! Run by hand with `cargo bench -p aether-protocol`.

#[path = "../tests/common/mod.rs"]
mod common;

use aether_protocol::{Reply, Request};
use common::Msg;
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

fn encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode");
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    for (name, m) in common::vectors() {
        group.bench_function(name, |b| {
            b.iter(|| {
                buf.clear();
                match &m {
                    Msg::Req(r) => r.encode(&mut buf),
                    Msg::Rep(_, r) => r.encode(&mut buf),
                    Msg::Raw(raw) => buf.extend_from_slice(raw),
                }
                black_box(&buf);
            })
        });
    }
    group.finish();
}

fn decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode");
    for (name, m) in common::vectors() {
        let bytes = m.bytes();
        match &m {
            Msg::Req(r) => {
                let (op, flags) = (r.opcode(), r.flags());
                group.bench_function(name, |b| {
                    b.iter(|| black_box(Request::decode(op, flags, black_box(&bytes))))
                });
            }
            Msg::Rep(op, _) => {
                let op = *op;
                group.bench_function(name, |b| {
                    b.iter(|| black_box(Reply::decode(op, 0, black_box(&bytes))))
                });
            }
            // Not a ring message: nothing to decode with the table.
            Msg::Raw(_) => {}
        }
    }
    group.finish();
}

criterion_group!(benches, encode, decode);
criterion_main!(benches);
