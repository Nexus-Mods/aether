//! The case fold on a short ASCII name, a long ASCII path and a name with
//! non-ASCII letters, as `fold` (a new string) and `fold_into` (a kept
//! buffer), and the comparison the registry cache makes (`eq_folded`).
//! Run by hand with `cargo bench -p aether-casefold`.

use aether_casefold::{eq_folded, fold, fold_into};
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

const ASCII_SHORT: &str = "Skyrim.ESM";
const ASCII_LONG: &str =
    "Data/Meshes/Architecture/Whiterun/WRBuildings/WRHouseSmall01/WRHouseSmall01Exterior_LOD_2.NIF";
const NON_ASCII: &str = "Data/Textures/\u{DC}BER/Caf\u{C9}/\u{130}stanbul/\u{41F}\u{440}\u{438}\u{432}\u{435}\u{442}.DDS";

fn cases() -> [(&'static str, &'static str); 3] {
    [
        ("ascii-short", ASCII_SHORT),
        ("ascii-long-path", ASCII_LONG),
        ("non-ascii", NON_ASCII),
    ]
}

fn folds(c: &mut Criterion) {
    let mut group = c.benchmark_group("fold");
    for (name, s) in cases() {
        group.bench_function(name, |b| b.iter(|| black_box(fold(black_box(s)))));
    }
    group.finish();

    let mut group = c.benchmark_group("fold_into");
    let mut out = String::with_capacity(256);
    for (name, s) in cases() {
        group.bench_function(name, |b| {
            b.iter(|| {
                fold_into(black_box(s), &mut out);
                black_box(&out);
            })
        });
    }
    group.finish();

    let mut group = c.benchmark_group("eq_folded");
    for (name, s) in cases() {
        let other = fold(s);
        group.bench_function(name, |b| {
            b.iter(|| black_box(eq_folded(black_box(s), black_box(&other))))
        });
    }
    group.finish();
}

criterion_group!(benches, folds);
criterion_main!(benches);
