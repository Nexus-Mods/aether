//! The read cache's cost: `read` of 4 KiB served from memory (a hit), and
//! one that has to fetch its unit (a miss, with a fetch that copies from
//! memory), on 1 and 4 threads; and `register`, which every open of a file
//! the cache may hold goes through. The time per read is the wall time of
//! the whole run divided by the reads all threads made.
//! Run by hand with `cargo bench -p aether-ipc`.

use aether_ipc::{CacheConfig, FileRef, ReadCache};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use std::hint::black_box;
use std::sync::Barrier;
use std::time::{Duration, Instant};

const READ: usize = 4096;
const BLOCK: usize = 64 * 1024;
/// A file far larger than any bench reads.
const SIZE: u64 = 1 << 40;

/// What a fetch copies from: the director's bytes, in memory.
fn source() -> Vec<u8> {
    (0..BLOCK).map(|i| (i * 7 + i / 251) as u8).collect()
}

fn fetch(src: &[u8], _off: u64, buf: &mut [u8]) -> Result<usize, i32> {
    for chunk in buf.chunks_mut(src.len()) {
        chunk.copy_from_slice(&src[..chunk.len()]);
    }
    Ok(buf.len())
}

/// `threads` threads each make `per_thread` calls of `work(thread, i)`,
/// started together; the longest thread's time is the run's.
fn run(threads: usize, iters: u64, work: &(dyn Fn(usize, u64) + Sync)) -> Duration {
    let per_thread = iters.div_ceil(threads as u64);
    let gate = Barrier::new(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let gate = &gate;
                s.spawn(move || {
                    gate.wait();
                    let start = Instant::now();
                    for i in 0..per_thread {
                        work(t, i);
                    }
                    start.elapsed()
                })
            })
            .collect();
        let longest = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .max()
            .unwrap_or_default();
        // The run made `per_thread * threads` calls, criterion asked for
        // `iters`: scale so that the time per call is the whole run's.
        longest.mul_f64(iters as f64 / (per_thread * threads as u64) as f64)
    })
}

fn register(cache: &ReadCache, path: &str) -> FileRef {
    cache.register(0, path, SIZE, 1, true, false)
}

fn hits(c: &mut Criterion) {
    let src = source();
    let mut group = c.benchmark_group("readcache_hit_4k");
    for threads in [1usize, 4] {
        // Every thread reads the one file: they meet on its lock.
        group.bench_function(format!("one-file/threads={threads}"), |b| {
            let cache = ReadCache::default();
            let f = register(&cache, "data/skyrim.esm");
            let mut warm = vec![0u8; READ];
            cache.read(&f, 0, &mut warm, |o, b| fetch(&src, o, b));
            b.iter_custom(|iters| {
                run(threads, iters, &|_, i| {
                    let mut buf = [0u8; READ];
                    // 16 aligned 4 KiB reads inside the cached unit.
                    let off = (i % 16) * READ as u64;
                    black_box(cache.read(&f, off, &mut buf, |_, _| Err(5)));
                })
            })
        });
        // Every thread reads a file of its own: only the file table and
        // the counters are shared.
        group.bench_function(format!("own-file/threads={threads}"), |b| {
            let cache = ReadCache::default();
            let files: Vec<FileRef> = (0..threads)
                .map(|t| {
                    let f = register(&cache, &format!("data/file{t}.bsa"));
                    let mut warm = vec![0u8; READ];
                    cache.read(&f, 0, &mut warm, |o, b| fetch(&src, o, b));
                    f
                })
                .collect();
            b.iter_custom(|iters| {
                run(threads, iters, &|t, i| {
                    let mut buf = [0u8; READ];
                    let off = (i % 16) * READ as u64;
                    black_box(cache.read(&files[t], off, &mut buf, |_, _| Err(5)));
                })
            })
        });
    }
    group.finish();
}

fn misses(c: &mut Criterion) {
    let src = source();
    let mut group = c.benchmark_group("readcache_miss_4k");
    // One read of a fresh cache: the miss, fetch included, once.
    group.bench_function("fresh/threads=1", |b| {
        b.iter_batched_ref(
            || {
                let cache = ReadCache::new(CacheConfig::default());
                let f = register(&cache, "data/skyrim.esm");
                (cache, f)
            },
            |(cache, f)| {
                let mut buf = [0u8; READ];
                black_box(cache.read(f, 0, &mut buf, |o, b| fetch(&src, o, b)))
            },
            BatchSize::SmallInput,
        )
    });
    // Every read lands in a unit nobody has read: two units apart, so the
    // misses are not a sequential reader's and each fetches one unit.
    for threads in [1usize, 4] {
        group.bench_function(format!("distinct-units/threads={threads}"), |b| {
            b.iter_custom(|iters| {
                let cache = ReadCache::default();
                let files: Vec<FileRef> = (0..threads)
                    .map(|t| register(&cache, &format!("data/file{t}.bsa")))
                    .collect();
                run(threads, iters, &|t, i| {
                    let mut buf = [0u8; READ];
                    let off = i * 2 * BLOCK as u64;
                    black_box(cache.read(&files[t], off, &mut buf, |o, b| fetch(&src, o, b)));
                })
            })
        });
    }
    group.finish();
}

fn registering(c: &mut Criterion) {
    let mut group = c.benchmark_group("readcache_register");
    let cache = ReadCache::default();
    register(&cache, "data/meshes/a.nif");
    group.bench_function("known-file", |b| {
        b.iter(|| black_box(register(&cache, black_box("data/meshes/a.nif"))))
    });
    // After a delete, every open looks at the set of blocked paths.
    let blocked = ReadCache::default();
    for i in 0..64 {
        blocked.invalidate_path(0, &format!("data/old/dir{i}"));
    }
    register(&blocked, "data/meshes/a.nif");
    group.bench_function("known-file/64-blocked-paths", |b| {
        b.iter(|| black_box(register(&blocked, black_box("data/meshes/a.nif"))))
    });
    group.finish();
}

criterion_group!(benches, hits, misses, registering);
criterion_main!(benches);
