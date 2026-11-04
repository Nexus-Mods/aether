//! What an open's path costs before it can cross the ring: which root a path
//! is under (`RootMap::resolve`, answered from the cache and recomputed under
//! an `UncachedScope`, for a path inside the root, one outside it and an 8.3
//! spelling the OS has to expand), the stream-suffix split, the vpath
//! normalization, and the spelling cache's lookups.
//! Run by hand with `cargo bench -p aether-redirect`.

use aether_redirect::finalname::NameCache;
use aether_redirect::{normalize_vpath, split_stream_suffix, RootMap, UncachedScope, VolumeMap};
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

const INSIDE: &str = r"\??\C:\Games\Skyrim\Data\Meshes\Architecture\Whiterun\WRHouse01.nif";
const OUTSIDE: &str = r"\??\C:\Windows\System32\drivers\etc\hosts";

/// A root and an 8.3 spelling inside it. On Windows the spelling is of a
/// folder every machine has, so the OS expands it; elsewhere, or if 8.3
/// names are off, the lookup finds nothing and fails fast.
fn short_name_case() -> (&'static str, &'static str) {
    if cfg!(windows) {
        (
            r"C:\Program Files",
            r"\??\C:\PROGRA~1\Common Files\microsoft shared\a.dll",
        )
    } else {
        (r"C:\Games\Skyrim", r"\??\C:\Games\SKYRIM~1\Data\a.esm")
    }
}

fn under_root(c: &mut Criterion) {
    let map = RootMap::new(r"C:\Games\Skyrim", VolumeMap::empty()).unwrap();
    let (short_root, short) = short_name_case();
    let short_map = RootMap::new(short_root, VolumeMap::empty()).unwrap();

    let mut group = c.benchmark_group("under_root");
    for (name, path) in [("inside", INSIDE), ("outside", OUTSIDE)] {
        // Warm: the first call fills the cache.
        black_box(map.resolve(path));
        group.bench_function(format!("{name}/cached"), |b| {
            b.iter(|| black_box(map.resolve(black_box(path))))
        });
        group.bench_function(format!("{name}/uncached"), |b| {
            let _scope = UncachedScope::enter();
            b.iter(|| black_box(map.resolve(black_box(path))))
        });
    }
    // An OS-consulted answer is never cached: both are the same work.
    group.bench_function("short-name/cached", |b| {
        b.iter(|| black_box(short_map.resolve(black_box(short))))
    });
    group.bench_function("short-name/uncached", |b| {
        let _scope = UncachedScope::enter();
        b.iter(|| black_box(short_map.resolve(black_box(short))))
    });
    group.bench_function("inside/contains-cached", |b| {
        black_box(map.contains(INSIDE));
        b.iter(|| black_box(map.contains(black_box(INSIDE))))
    });
    group.finish();
}

fn streams_and_vpaths(c: &mut Criterion) {
    let plain = r"C:\Games\Skyrim\Data\Skyrim.esm";
    let stream = r"C:\Games\Skyrim\Data\Skyrim.esm:Zone.Identifier:$DATA";
    let mut group = c.benchmark_group("split_stream_suffix");
    group.bench_function("plain", |b| {
        b.iter(|| black_box(split_stream_suffix(black_box(plain))))
    });
    group.bench_function("stream", |b| {
        b.iter(|| black_box(split_stream_suffix(black_box(stream))))
    });
    group.finish();

    let mut group = c.benchmark_group("normalize_vpath");
    for (name, raw) in [
        ("clean", "Data/Meshes/Architecture/a.nif"),
        ("backslashes", r"Data\Meshes\Architecture\a.nif"),
        ("dotdot", r"\??\Data\Meshes\.\Old\..\Architecture\a.nif"),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| black_box(normalize_vpath(black_box(raw))))
        });
    }
    group.finish();
}

fn names(c: &mut Criterion) {
    let under: Vec<String> = "data/interface/fonts/a.ttf"
        .split('/')
        .map(str::to_string)
        .collect();
    let spelled: Vec<String> = ["Data", "Interface", "Fonts", "A.ttf"]
        .map(str::to_string)
        .to_vec();
    let mut cache = NameCache::new(60_000, 4096);
    cache.remember(0, &under, 0, &spelled, 0);
    let other: Vec<String> = "data/other/b.esp".split('/').map(str::to_string).collect();

    let mut group = c.benchmark_group("name_cache");
    group.bench_function("leading_with/hit", |b| {
        b.iter(|| {
            let mut len = 0;
            let known = cache.leading_with(0, black_box(&under), 10, |n| len += n.len());
            black_box((known, len))
        })
    });
    group.bench_function("leading_with/partial", |b| {
        b.iter(|| black_box(cache.leading_with(0, black_box(&other), 10, |_| ())))
    });
    group.bench_function("leading_with/stale", |b| {
        b.iter(|| black_box(cache.leading_with(0, black_box(&under), 1_000_000, |_| ())))
    });
    group.bench_function("hit/leading_with+spelling", |b| {
        b.iter(|| {
            let mut len = 0;
            if cache.leading_with(0, black_box(&under), 10, |_| ()) == under.len() {
                for i in 0..under.len() {
                    len += cache.spelling(0, &under, i).map_or(0, str::len);
                }
            }
            black_box(len)
        })
    });
    group.finish();
}

criterion_group!(benches, under_root, streams_and_vpaths, names);
criterion_main!(benches);
