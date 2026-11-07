//! `aether-conform run --limits`: requests sized from the director's own
//! ring, which a scenario file cannot be, since the sizes are known only once
//! a tree is mounted.
//!
//! A mount of an empty tree gives the geometry (`payload_cap`, `slot_count`,
//! `VFS_ARENA_LEN`); then a tree sized from it is mounted and played:
//!
//! - **READs of exactly what the shim asks for** (wire.md 2.6, 3.4.4). The
//!   shim reads in bulk chunks of `clamp(VFS_ARENA_LEN / slot_count, 256 KiB,
//!   1 MiB)` with `FLAG_READ_BULK`, a remainder of 64 KiB and up in bulk too,
//!   and anything shorter inline, at most 65 535 bytes; with no arena it
//!   reads inline chunks of `payload_cap - 8` (`FuseClient::bulk_bank_bytes`,
//!   `aether_ipc::read_fragmented`). Its read cache fetches runs of 16
//!   blocks of 64 KiB through the same path. A short answer is end of file
//!   to the shim (3.4.4: "`n < len` means end of file"), so each of these
//!   must come back whole, unless at the end of the file. A READ one byte
//!   over the bulk chunk may come back at most a bank (3.4.4: `n <=
//!   min(len, bank_size)`).
//! - **A WRITE of `payload_cap - 24` bytes**, a request of exactly
//!   `payload_cap`, as `FuseClient::write` cuts its chunks (3.4.5), twice,
//!   then read back.
//! - **READDIR at the cap** (3.4.2, 2.5 step 3): a listing whose reply is
//!   exactly `payload_cap` bytes comes back whole; one a byte longer is the
//!   overflow status `i32::MIN` with an empty payload, never a shorter list.
//! - **Several READs of one handle in flight** (2.5, "Order of service": a
//!   pipelined read publishes 4 requests at once, 8 for reads of 4 MiB or
//!   more): published together at different offsets, out of order, held
//!   until every one is completed and only then copied out, as
//!   `read_fragmented` copies them, with one more READ answered in between;
//!   each must be its own offset's bytes, in its own slot's bank (2.6). A
//!   director that keeps a position per handle, or writes a bank after its
//!   slot is `COMPLETED`, fails.
//! - **An abandoned slot** (2.5): a slow READ given up while `PROCESSING`
//!   must end `FREE`, never `COMPLETED`, and every slot must serve after.
//!   A director this run cannot catch processing (it answers too fast) is
//!   not failed; the step says the rule was not exercised.
//! - **A registry write against a reader** (4.5): while one client writes a
//!   value over and over, another reads it, each read after reading
//!   `reg_gen`; a read made at a generation the write's reply had already
//!   published must see that write ("it moves on every successful registry
//!   write, before the write's reply is published"; a director that moves
//!   it before the write is visible lets the shim cache the old answer
//!   under the new generation). A director with no registry layer skips it,
//!   and one whose mount answer says `"registry": false` is mounted a tree
//!   without a registry layer and sent no registry request.
//! - **Warnings**, which never fail: `worker_hint` 0 or above `slot_count`
//!   (2.8), and the median HEARTBEAT round trip (2.11).
//!
//! The report has one entry, `limits`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aether_ipc::{layout, ring, Notifier};
use aether_protocol::{Opcode, OpenFlags, OpenResp, Reply, Request, RequestFlags, RootId, Status};
use aether_refdirector::tree::{fill, Contents, File, RegKey, Registry, Root, Tree};

use crate::adapter::{Adapter, AdapterError, Env};
use crate::attach::Attached;
use crate::clients::{call_on, Answer};
use crate::compare::render_got;
use crate::run::{ScenarioResult, StepResult};
use crate::scenario::{
    to_json_string, Expect, OwnedDirEntry, OwnedReply, OwnedRequest, Scenario, Step,
};

/// The shim's bulk threshold (`BULK_THRESHOLD`, wire.md 3.4.4).
const BULK_THRESHOLD: u32 = 65_536;
/// The read cache's run: 16 blocks of 64 KiB (`readcache::config`).
const CACHE_RUN: u32 = 16 * 64 * 1024;
/// The big file's seed, and the bytes past its last whole chunk.
const BIG_SEED: u64 = 41;
const BIG_TAIL: u64 = 4321;
/// The registry key the writer and reader share.
const KEY: &str = r"\Registry\Machine\Software\ConformLimits";
/// How many writes the registry writer makes.
const REG_WRITES: u32 = 300;
/// How many HEARTBEATs the round trip is measured over.
const HEARTBEATS: usize = 200;
/// How often a slow READ is tried before the abandoned-slot rule is called
/// not exercised.
const ABANDON_TRIES: u32 = 100;

/// The ring's numbers, from a mount of an empty tree.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    cap: u32,
    slots: u32,
    arena: usize,
}

impl Geometry {
    /// wire.md 2.6: `max(arena_len / banks, 4096)`.
    fn bank(&self) -> u64 {
        (self.arena as u64 / u64::from(self.slots.max(1))).max(4096)
    }

    /// The shim's read chunk: a bulk chunk with an arena, else the inline
    /// cap (`FuseClient::bulk_bank_bytes`).
    fn chunk(&self) -> u32 {
        if self.arena == 0 {
            self.cap.saturating_sub(8)
        } else {
            (self.arena / self.slots.max(1) as usize).clamp(256 * 1024, 1024 * 1024) as u32
        }
    }

    fn big(&self) -> u64 {
        8 * u64::from(self.chunk()) + BIG_TAIL
    }
}

/// Plays the limits part against the adapter's director: one entry,
/// `limits`. `Err` only for an adapter error.
pub fn run(adapter: &mut Adapter, timeout: Duration) -> Result<ScenarioResult, AdapterError> {
    let mut result = ScenarioResult {
        name: "limits".into(),
        file: PathBuf::from("(built in: aether_conform::limits)"),
        passed: false,
        steps: Vec::new(),
        error: None,
        skipped: None,
    };
    let file = std::env::temp_dir().join(format!(
        "aether-conform-limits-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let outcome = (|| -> Result<Option<String>, AdapterError> {
        let geom = match probe(adapter, &file, timeout)? {
            Ok(g) => g,
            Err(e) => return Ok(Some(e)),
        };
        let s = scenario(geom, adapter.serves_registry());
        let tree_only = Scenario {
            steps: Vec::new(),
            ..s.clone()
        };
        if let Err(e) = std::fs::write(&file, to_json_string(&tree_only)) {
            return Ok(Some(format!("cannot write {}: {e}", file.display())));
        }
        let env = match adapter.mount(&file)? {
            Ok(env) => env,
            Err(why) => return Ok(Some(format!("mount refused: {why}"))),
        };
        let error = match Attached::from_env(&env, timeout) {
            Ok(ring) => {
                let e = play(&ring, &env, timeout, &s, geom, &mut result.steps)
                    .err()
                    .map(|e| format!("{}: {e}", ring.describe()));
                drop(ring);
                e
            }
            Err(e) => Some(format!("attach failed: {e}")),
        };
        adapter.unmount()?;
        Ok(error)
    })();
    let _ = std::fs::remove_file(&file);
    result.error = outcome?;
    result.passed = result.error.is_none() && result.steps.iter().all(|s| s.passed);
    Ok(result)
}

/// The geometry of the director's ring, from a mount of an empty tree.
fn probe(
    adapter: &mut Adapter,
    file: &std::path::Path,
    timeout: Duration,
) -> Result<Result<Geometry, String>, AdapterError> {
    let s = Scenario {
        name: "limits-probe".into(),
        description: "An empty root, to learn the ring's geometry".into(),
        tree: Tree {
            roots: vec![Root::default()],
            registry: None,
        },
        steps: Vec::new(),
    };
    if let Err(e) = std::fs::write(file, to_json_string(&s)) {
        return Ok(Err(format!("cannot write {}: {e}", file.display())));
    }
    let env = match adapter.mount(file)? {
        Ok(env) => env,
        Err(why) => return Ok(Err(format!("mount refused: {why}"))),
    };
    let geom = Attached::from_env(&env, timeout)
        .map(|ring| Geometry {
            cap: ring.geom().payload_cap,
            slots: ring.geom().slot_count.max(1),
            arena: ring.arena_len(),
        })
        .map_err(|e| format!("attach failed: {e}"));
    adapter.unmount()?;
    Ok(geom)
}

/// Names for a directory whose READDIR reply is `total` bytes: `4 + sum(21 +
/// name_len)` (wire.md 3.4.2), names of 200 to 254 ASCII bytes.
fn listing_names(prefix: &str, total: u64) -> Vec<String> {
    const L: u64 = 200;
    let room = total - 4;
    let n = room / (21 + L);
    let mut left = room - n * (21 + L);
    let mut lens: Vec<u64> = vec![L; n as usize];
    if left >= 22 {
        lens.push(left - 21);
        left = 0;
    }
    for l in lens.iter_mut() {
        let add = left.min(254 - *l);
        *l += add;
        left -= add;
    }
    assert_eq!(left, 0, "a listing of {total} bytes");
    lens.iter()
        .enumerate()
        .map(|(i, &l)| {
            let head = format!("{prefix}{i:05}-");
            let pad = "x".repeat(l as usize - head.len());
            format!("{head}{pad}")
        })
        .collect()
}

/// The tree and steps for `g`; the tree has a registry layer only when
/// `registry` says the director serves one.
fn scenario(g: Geometry, registry: bool) -> Scenario {
    let cap = u64::from(g.cap);
    let fit = listing_names("f", cap);
    let over = listing_names("o", cap + 1);
    let mut files = vec![File {
        path: "big.bin".into(),
        contents: Contents::Fill {
            size: g.big(),
            seed: BIG_SEED,
        },
        mtime: 0,
    }];
    for (dir, names) in [("fit", &fit), ("over", &over)] {
        files.extend(names.iter().map(|n| File {
            path: format!("{dir}/{n}"),
            contents: Contents::Text(String::new()),
            mtime: 0,
        }));
    }
    let tree = Tree {
        roots: vec![Root {
            id: 0,
            files,
            dirs: vec!["w".into()],
            writable: vec!["w".into()],
            lower: vec![],
        }],
        registry: registry.then(|| Registry {
            keys: vec![RegKey {
                path: KEY.into(),
                volatile: false,
                values: Vec::new(),
            }],
        }),
    };
    let big = fill(g.big() as usize, BIG_SEED);
    let mut steps = Vec::new();
    let mut add = |request: OwnedRequest, expect: Expect, note: &str| {
        let cite = matches!(expect, Expect::OneOf(_)).then(|| "3.4.4".to_string());
        steps.push(Step {
            request,
            expect,
            cite,
            note: Some(note.to_string()),
            client: 0,
            literal_fh: false,
            choice: None,
        });
    };
    add(
        OwnedRequest::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path: "big.bin".into(),
        },
        Expect::Reply(OwnedReply::Open(OpenResp {
            fh: 1,
            size: g.big(),
            is_dir: false,
            immutable: true,
            mount_gen: 0,
        })),
        "the file the READs read",
    );
    let bank = g.bank();
    // The answer to a READ of `len` at `offset`: all of it to the end of the
    // file, or, where a bulk answer cannot hold it, at most a bank. An inline
    // answer holds at most `payload_cap - 8` bytes, and holds that many when
    // the file has them (3.4.4).
    let read = |offset: u64, len: u32, bulk: bool| -> Expect {
        let may_be_bulk = g.arena > 0 && (bulk || len >= BULK_THRESHOLD);
        let mut end = (offset + u64::from(len)).min(g.big());
        if !may_be_bulk {
            end = end.min(offset + u64::from(g.cap - 8));
        }
        let whole = Expect::Reply(OwnedReply::Read(
            big[offset as usize..end as usize].to_vec(),
        ));
        let banked = (offset + bank).min(end);
        if may_be_bulk && banked < end {
            Expect::OneOf(vec![
                whole,
                Expect::Reply(OwnedReply::Read(
                    big[offset as usize..banked as usize].to_vec(),
                )),
            ])
        } else {
            whole
        }
    };
    let flags = |bulk: bool| {
        if bulk {
            RequestFlags::READ_BULK
        } else {
            RequestFlags::empty()
        }
    };
    let chunk = g.chunk();
    let mut reads: Vec<(u64, u32, bool, String)> = Vec::new();
    if g.arena > 0 {
        let c = u64::from(chunk);
        reads.extend([
            (0, chunk, true, format!("the shim's bulk chunk, {chunk} bytes (clamp(VFS_ARENA_LEN / slot_count, 256 KiB, 1 MiB), wire.md 2.6): whole, or the shim reads end of file")),
            (c, chunk, true, "the second chunk".into()),
            (7 * c, chunk, true, "the last whole chunk".into()),
            (8 * c, chunk, true, format!("a chunk at the last {BIG_TAIL} bytes: end of file")),
            (g.big() - 1, chunk, true, "a chunk at the last byte".into()),
            (0, chunk + 1, true, "a byte over the chunk: whole, or at most a bank (3.4.4: n <= min(len, bank_size))".into()),
            (3, BULK_THRESHOLD, true, "a remainder of 64 KiB, the shortest the shim reads in bulk".into()),
            (12_345, chunk - 1, true, "a remainder a byte under the chunk".into()),
            (3 * 65_536, CACHE_RUN.min(chunk), true, "the read cache's run of 16 blocks of 64 KiB".into()),
            (99, BULK_THRESHOLD - 1, false, "65 535 bytes inline, the longest inline READ the shim sends with an arena".into()),
            (5, BULK_THRESHOLD, false, "64 KiB without the flag: bulk by size (3.4.4)".into()),
        ]);
    }
    let inline = g.cap - 8;
    reads.extend([
        (0, inline, false, format!("payload_cap - 8 = {inline} bytes, the most an inline reply holds (3.4.4), the shim's chunk with no arena")),
        (u64::from(inline), inline, false, "the next inline chunk".into()),
        (17, inline - 1, false, "payload_cap - 9 bytes".into()),
    ]);
    if g.arena == 0 {
        reads.extend([
            (0, inline + 1, false, format!("a byte over payload_cap - 8 with no arena: inline, {inline} bytes (3.4.4: n <= min(len, payload_cap - 8))")),
            (5, 2 * g.cap, false, format!("2 x payload_cap with no arena: inline, {inline} bytes (3.4.4)")),
        ]);
    }
    for (offset, len, bulk, note) in reads {
        add(
            OwnedRequest::Read {
                slot_flags: flags(bulk),
                fh: 1,
                offset,
                len,
            },
            read(offset, len, bulk),
            &note,
        );
    }
    // A WRITE of exactly payload_cap bytes of request, twice, read back.
    let piece = (g.cap - 24) as usize;
    let one = fill(piece, 51);
    let two = fill(piece, 52);
    let written: Vec<u8> = one.iter().chain(&two).copied().collect();
    add(
        OwnedRequest::Open {
            root: RootId(0),
            flags: OpenFlags::WRITE | OpenFlags::CREATE,
            path: "w/x.bin".into(),
        },
        Expect::Reply(OwnedReply::Open(OpenResp {
            fh: 2,
            ..OpenResp::default()
        })),
        "a file to write",
    );
    add(
        OwnedRequest::Write {
            fh: 2,
            offset: 0,
            data: one.clone(),
        },
        Expect::Reply(OwnedReply::Write(piece as u32)),
        "payload_cap - 24 bytes, a request of exactly payload_cap (3.4.5: the shim sends at most VFS_RING_PAYLOAD_CAP - 24 bytes per WRITE)",
    );
    add(
        OwnedRequest::Write {
            fh: 2,
            offset: piece as u64,
            data: two,
        },
        Expect::Reply(OwnedReply::Write(piece as u32)),
        "the next one, as FuseClient::write cuts a longer write",
    );
    add(
        OwnedRequest::Close { fh: 2 },
        Expect::Reply(OwnedReply::Empty),
        "",
    );
    add(
        OwnedRequest::GetAttr {
            root: RootId(0),
            path: "w/x.bin".into(),
        },
        Expect::Reply(OwnedReply::GetAttr(aether_protocol::AttrResp {
            found: true,
            is_dir: false,
            size: written.len() as u64,
            mtime: 0,
        })),
        "both writes, every byte (3.4.5)",
    );
    add(
        OwnedRequest::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path: "w/x.bin".into(),
        },
        Expect::Reply(OwnedReply::Open(OpenResp {
            fh: 3,
            size: written.len() as u64,
            ..OpenResp::default()
        })),
        "",
    );
    for (offset, len) in [
        (piece as u64 - 10, 20u32),
        (0, 4096),
        (written.len() as u64 - 100, 4096),
    ] {
        let end = (offset as usize + len as usize).min(written.len());
        add(
            OwnedRequest::Read {
                slot_flags: RequestFlags::empty(),
                fh: 3,
                offset,
                len,
            },
            Expect::Reply(OwnedReply::Read(written[offset as usize..end].to_vec())),
            "the written bytes back",
        );
    }
    add(
        OwnedRequest::Close { fh: 3 },
        Expect::Reply(OwnedReply::Empty),
        "",
    );
    add(
        OwnedRequest::ReadDir {
            root: RootId(0),
            path: "fit".into(),
        },
        Expect::Reply(OwnedReply::ReadDir(
            fit.iter()
                .map(|n| OwnedDirEntry {
                    name: n.clone(),
                    is_dir: false,
                    size: 0,
                    mtime: 0,
                })
                .collect(),
        )),
        &format!(
            "{} entries, a reply of exactly payload_cap = {cap} bytes: it fits, and comes back whole (3.4.2)",
            fit.len()
        ),
    );
    add(
        OwnedRequest::Close { fh: 1 },
        Expect::Reply(OwnedReply::Empty),
        "",
    );
    Scenario {
        name: "limits".into(),
        description: "Requests sized from the director's ring (docs/conformance.md, \"Limits\")"
            .into(),
        tree,
        steps,
    }
}

/// Plays `s`'s steps, then the checks no step can say.
fn play(
    ring: &Attached,
    env: &Env,
    timeout: Duration,
    s: &Scenario,
    g: Geometry,
    out: &mut Vec<StepResult>,
) -> Result<(), String> {
    crate::run::play(ring, env, timeout, s, out)?;
    let mut next = out.len();
    let mut push = |request: String, expected: String, got: String, why: Option<String>| {
        out.push(StepResult {
            index: next,
            request: request.into(),
            expected,
            got,
            passed: passes(&why),
            note: why,
            answer: None,
        });
        next += 1;
    };
    // READDIR one byte over the cap.
    let (status, payload) = raw(
        ring,
        0,
        &OwnedRequest::ReadDir {
            root: RootId(0),
            path: "over".into(),
        },
    )?;
    push(
        format!("READDIR over: a listing of payload_cap + 1 = {} bytes", u64::from(g.cap) + 1),
        format!("status {} (overflow), payload length 0", i32::MIN),
        format!("status {status}, payload length {}", payload.len()),
        (status != i32::MIN || !payload.is_empty()).then(|| {
            "a listing larger than payload_cap is answered with the overflow status i32::MIN and an \
             empty payload (wire.md 3.4.2: \"The whole listing is one reply and must fit \
             payload_cap\"; the overflow status i32::MIN, 2.5 step 3), never a shorter list, \
             which the shim would show as the whole folder"
                .to_string()
        }),
    );
    for (what, expected, got, why) in pipelined(ring, g)? {
        push(what, expected, got, why);
    }
    let (what, expected, got, why) = abandoned(ring, g)?;
    push(what, expected, got, why);
    let (what, expected, got, why) = if s.tree.registry.is_some() {
        registry_race(ring, env, timeout)?
    } else {
        (
            format!(
                "{REG_WRITES} REG_SET_VALUEs of {KEY} from one client while another reads it \
                 with REG_KEY"
            ),
            "a read at a generation a write's reply published sees that write".into(),
            "no registry request sent".into(),
            Some(
                "not exercised: the director serves no registry (its mount answer says \
                 \"registry\": false)"
                    .into(),
            ),
        )
    };
    push(what, expected, got, why);
    // Warnings.
    let hint = ring::worker_hint(ring.seg());
    push(
        "worker_hint in the ring header".into(),
        format!("1 to slot_count = {}", g.slots),
        hint.to_string(),
        (hint == 0 || hint > g.slots).then(|| {
            format!(
                "warning: worker_hint {hint}: the shim then sizes its data limit as for {} \
                 threads (wire.md 2.8: 0 means 4, and the hint is capped at slot_count); a \
                 director should publish its real thread count",
                if hint == 0 { 4 } else { g.slots }
            )
        }),
    );
    let client = ring.client();
    let mut scratch = Vec::new();
    let mut times = Vec::with_capacity(HEARTBEATS);
    for _ in 0..HEARTBEATS {
        let t = Instant::now();
        client
            .call(&Request::Heartbeat {}, &mut scratch)
            .map_err(|e| format!("a HEARTBEAT failed: {e}"))?;
        times.push(t.elapsed());
    }
    times.sort();
    let median = times[times.len() / 2];
    push(
        format!(
            "{HEARTBEATS} HEARTBEATs one after another, on {}",
            ring.describe()
        ),
        "a round trip (reported, never failed)".into(),
        format!(
            "median {:.1} µs, slowest {:.1} µs",
            median.as_secs_f64() * 1e6,
            times[times.len() - 1].as_secs_f64() * 1e6
        ),
        Some(format!(
            "warning: the wake latency, which wire.md 2.11 leaves to the director (a lost wake \
             costs latency, never a request){}",
            if median > Duration::from_millis(2) {
                "; over 2 ms, which a game's many small reads would feel"
            } else {
                ""
            }
        )),
    );
    Ok(())
}

/// Sends `req` claiming from slot `start` and waits for its answer: the raw
/// status and payload.
fn raw(ring: &Attached, start: u32, req: &OwnedRequest) -> Result<(i32, Vec<u8>), String> {
    let held = Held::publish(ring, &[(start, req)])?;
    let (status, payload) = held.answers[0].clone();
    held.release(ring);
    Ok((status, payload))
}

/// Requests published together and held, every one completed, until
/// [`Held::release`].
struct Held {
    /// (slot, opcode, flags) of each request.
    slots: Vec<(u32, Opcode, u32)>,
    answers: Vec<(i32, Vec<u8>)>,
}

impl Held {
    /// Publishes every request (each claiming from its start), then waits
    /// until all are completed, freeing none: a pipelined read's way
    /// (`submit_many_held`). `Err` when a slot did not come free or an answer
    /// did not come; what was published is then given up (`abandon`).
    fn publish(ring: &Attached, reqs: &[(u32, &OwnedRequest)]) -> Result<Held, String> {
        let (seg, geom, wake) = (ring.seg(), ring.geom(), ring.wake());
        let began = Instant::now();
        let mut held: Vec<(u32, u64, Opcode, u32)> = Vec::new();
        let give_up = |held: &[(u32, u64, Opcode, u32)]| {
            for &(slot, ..) in held {
                let _ = ring::abandon(seg, &geom, slot);
            }
            wake.notify_slot_free();
        };
        for &(start, req) in reqs {
            let request = req.as_request();
            let mut body = Vec::new();
            request.encode(&mut body);
            let slot = loop {
                if let Some(s) = ring::claim_free_from(seg, &geom, start % geom.slot_count.max(1)) {
                    break s;
                }
                if began.elapsed() > ring.deadline() {
                    give_up(&held);
                    return Err("no free slot (wire.md 2.5)".into());
                }
                wake.idle_client(start, began.elapsed());
            };
            let (op, flags) = (request.opcode(), request.flags());
            match ring::publish_request(seg, &geom, slot, op as u32, flags, &body) {
                Ok(id) => held.push((slot, id, op, flags)),
                Err(e) => {
                    let _ = ring::free_slot(seg, &geom, slot);
                    give_up(&held);
                    return Err(format!("publishing on slot {slot}: {e:?}"));
                }
            }
        }
        wake.notify_server();
        let published = Instant::now();
        let mut answers = Vec::new();
        for (n, &(slot, req_id, ..)) in held.iter().enumerate() {
            loop {
                match ring::take_response(seg, &geom, slot, req_id) {
                    Ok(Some(r)) => {
                        answers.push(r);
                        break;
                    }
                    Ok(None) if published.elapsed() <= ring.deadline() => {
                        wake.idle_client(slot, published.elapsed())
                    }
                    other => {
                        give_up(&held[n..]);
                        for &(s, ..) in &held[..n] {
                            let _ = ring::free_slot(seg, &geom, s);
                        }
                        return Err(match other {
                            Ok(_) => format!(
                                "no answer within {} s to the request in slot {slot} (wire.md \
                                 2.5: every taken slot MUST be completed)",
                                ring.deadline().as_secs_f64()
                            ),
                            Err(e) => format!("slot {slot}: {e:?} (wire.md 2.5)"),
                        });
                    }
                }
            }
        }
        Ok(Held {
            slots: held.iter().map(|&(s, _, op, f)| (s, op, f)).collect(),
            answers,
        })
    }

    fn release(self, ring: &Attached) {
        for (slot, ..) in &self.slots {
            let _ = ring::free_slot(ring.seg(), &ring.geom(), *slot);
        }
        ring.wake().notify_slot_free();
    }
}

/// A handle on `path`, from client 0.
fn open(ring: &Attached, path: &str) -> Result<u64, String> {
    let mut scratch = Vec::new();
    match ring.client().call(
        &Request::Open {
            root: RootId(0),
            flags: OpenFlags::READ,
            path,
        },
        &mut scratch,
    ) {
        Ok(Reply::Open(o)) => Ok(o.fh),
        other => Err(format!("OPEN {path} gave {other:?}")),
    }
}

fn close(ring: &Attached, fh: u64) {
    let mut scratch = Vec::new();
    let _ = ring.client().call(&Request::Close { fh }, &mut scratch);
}

type Check = (String, String, String, Option<String>);

/// A set of READs: what it is, and each one's (offset, length, bulk).
type ReadSet = (String, Vec<(u64, u32, bool)>);

/// Several READs of one handle in flight at once (wire.md 2.8): the
/// shim's pipeline of 4, and of 8 (one slot is kept for a READ sent while
/// they are held), each set at different offsets published out of order.
fn pipelined(ring: &Attached, g: Geometry) -> Result<Vec<Check>, String> {
    let big = fill(g.big() as usize, BIG_SEED);
    let fh = open(ring, "big.bin")?;
    let chunk = u64::from(g.chunk());
    let bulk = g.arena > 0;
    let order = [3u64, 1, 0, 2, 7, 5, 4, 6];
    let mut sets: Vec<ReadSet> = Vec::new();
    for depth in [4usize, 8] {
        let depth = depth.min(g.slots as usize - 1);
        if depth < 2 {
            continue;
        }
        let offs: Vec<(u64, u32, bool)> = order
            .iter()
            .filter(|&&k| (k as usize) < depth)
            .map(|&k| (k * chunk, chunk as u32, bulk))
            .collect();
        sets.push((format!("{depth} READs of {chunk} bytes"), offs));
    }
    if bulk && g.slots > 4 {
        let n = u64::from(BULK_THRESHOLD - 1);
        sets.push((
            "4 inline READs of 65 535 bytes".into(),
            [2u64, 0, 3, 1]
                .iter()
                .map(|&k| (k * n + 17, n as u32, false))
                .collect(),
        ));
    }
    let mut out = Vec::new();
    for (what, offs) in sets {
        let reqs: Vec<OwnedRequest> = offs
            .iter()
            .map(|&(offset, len, b)| OwnedRequest::Read {
                slot_flags: if b {
                    RequestFlags::READ_BULK
                } else {
                    RequestFlags::empty()
                },
                fh,
                offset,
                len,
            })
            .collect();
        let batch: Vec<(u32, &OwnedRequest)> = reqs
            .iter()
            .enumerate()
            .map(|(i, r)| (i as u32, r))
            .collect();
        let held = Held::publish(ring, &batch)?;
        // Another READ answered while they are held: a director that writes
        // a completed slot's bank after its reply writes it now.
        let between = OwnedRequest::Read {
            slot_flags: if bulk {
                RequestFlags::READ_BULK
            } else {
                RequestFlags::empty()
            },
            fh,
            offset: 1,
            len: chunk as u32,
        };
        let between_ok = call_on(ring, reqs.len() as u32, &between).map(|o| o.answer);
        let mut bad = Vec::new();
        for (i, ((slot, op, flags), (status, payload))) in
            held.slots.iter().zip(&held.answers).enumerate()
        {
            let (offset, len, _) = offs[i];
            let end = (offset + u64::from(len)).min(g.big()) as usize;
            let want = &big[offset as usize..end];
            let (answer, _) = crate::clients::decode(ring, *slot, *op, *flags, *status, payload);
            match answer {
                Answer::Got(Ok(OwnedReply::Read(d))) if d == want => {}
                Answer::Got(g) => bad.push(format!(
                    "the READ at {offset} (slot {slot}) got {}",
                    render_got(&g)
                )),
                Answer::Bad(why) => bad.push(format!("the READ at {offset}: {why}")),
            }
        }
        held.release(ring);
        if let Err(e) = between_ok {
            bad.push(format!(
                "the READ sent while they were held got no answer: {e}"
            ));
        }
        let offsets: Vec<String> = offs.iter().map(|o| o.0.to_string()).collect();
        out.push((
            format!(
                "{what} of one handle, published at once at offsets {} (in that order), held \
                 until all were completed, one more READ answered meanwhile, then copied out",
                offsets.join(", ")
            ),
            "each its own offset's bytes, in its own slot's bank".into(),
            if bad.is_empty() {
                "as expected".into()
            } else {
                bad.join("; ")
            },
            (!bad.is_empty()).then(|| {
                "several READs of one handle in flight are each answered at their own offset \
                 and in their own slot's bank, and a completed slot's bank is not written again \
                 until the client frees it (wire.md 2.5, \"Order of service\": a pipelined read \
                 publishes up to 4 requests at once, 8 for reads of 4 MiB or more; 2.6: the \
                 client reads the bank while the slot is COMPLETED and frees it only after \
                 copying it out)"
                    .to_string()
            }),
        ));
    }
    close(ring, fh);
    Ok(out)
}

/// A slow READ given up while `PROCESSING` (wire.md 2.5): the slot must end `FREE`,
/// and every slot serve after.
fn abandoned(ring: &Attached, g: Geometry) -> Result<Check, String> {
    let (seg, geom, wake) = (ring.seg(), ring.geom(), ring.wake());
    let fh = open(ring, "big.bin")?;
    let req = OwnedRequest::Read {
        slot_flags: if g.arena > 0 {
            RequestFlags::READ_BULK
        } else {
            RequestFlags::empty()
        },
        fh,
        offset: 0,
        len: g.chunk(),
    };
    let request = req.as_request();
    let mut body = Vec::new();
    request.encode(&mut body);
    let what = "a READ given up (ABANDONED) while the director is PROCESSING it".to_string();
    let expected = "the slot ends FREE, never COMPLETED; then every slot serves".to_string();
    let mut caught = None;
    for _ in 0..ABANDON_TRIES {
        let began = Instant::now();
        let slot = loop {
            if let Some(s) = ring::claim_free_from(seg, &geom, 0) {
                break s;
            }
            if began.elapsed() > ring.deadline() {
                close(ring, fh);
                return Err("no free slot (wire.md 2.5)".into());
            }
            wake.idle_client(0, began.elapsed());
        };
        let id = ring::publish_request(
            seg,
            &geom,
            slot,
            Opcode::Read as u32,
            request.flags(),
            &body,
        )
        .map_err(|e| format!("{e:?}"))?;
        wake.notify_server();
        // Wait for the director to take it, then give it up at once.
        while ring::slot_state(seg, &geom, slot) == Some(layout::ST_SUBMITTED)
            && began.elapsed() < ring.deadline()
        {
            core::hint::spin_loop();
        }
        match ring::abandon(seg, &geom, slot) {
            Ok(ring::Abandon::Retired) => {
                caught = Some((slot, id));
                break;
            }
            // Answered before it could be given up: try again.
            _ => wake.notify_slot_free(),
        }
    }
    let Some((slot, _)) = caught else {
        close(ring, fh);
        return Ok(not_caught(what, expected));
    };
    let began = Instant::now();
    let end = loop {
        match ring::slot_state(seg, &geom, slot) {
            Some(layout::ST_ABANDONED) if began.elapsed() <= ring.deadline() => {
                wake.idle_client(slot, began.elapsed())
            }
            s => break s,
        }
    };
    let mut why = match end {
        Some(layout::ST_FREE) | Some(layout::ST_CLAIMED) | Some(layout::ST_SUBMITTED) => None,
        Some(layout::ST_COMPLETED) => Some(format!(
            "slot {slot} went from ABANDONED to COMPLETED (wire.md 2.5: \"a director must never \
             store COMPLETED over ABANDONED\": the slot is unusable for ever)"
        )),
        Some(layout::ST_ABANDONED) => Some(format!(
            "slot {slot} was still ABANDONED after {} s (wire.md 2.5: the director finishes an \
             abandoned request and frees the slot)",
            ring.deadline().as_secs_f64()
        )),
        other => Some(format!("slot {slot} is in state {other:?} (wire.md 2.5)")),
    };
    let mut got = format!("slot {slot} ended in state {}", state_name(end));
    for n in 0..g.slots {
        match call_on(ring, n, &OwnedRequest::Heartbeat) {
            Ok(o) if o.answer == Answer::Got(Ok(OwnedReply::Empty)) => {}
            other => {
                got.push_str(&format!("; a HEARTBEAT from slot {n} got {other:?}"));
                why.get_or_insert_with(|| {
                    "after an abandoned request every slot must still serve (wire.md 2.5)".into()
                });
                break;
            }
        }
    }
    close(ring, fh);
    Ok((what, expected, got, why))
}

/// The abandoned-slot step of a director never caught `PROCESSING`: not
/// exercised, which passes.
fn not_caught(what: String, expected: String) -> Check {
    (
        what,
        expected,
        format!("answered each of {ABANDON_TRIES} times before it could be given up"),
        Some(
            "not exercised: this director answered too fast to be caught PROCESSING \
             (wire.md 2.5: it must never store COMPLETED over ABANDONED)"
                .into(),
        ),
    )
}

/// Whether a check with this note passes: no note, a warning, or a rule
/// the run could not exercise.
fn passes(why: &Option<String>) -> bool {
    why.as_ref()
        .is_none_or(|w| w.starts_with("warning: ") || w.starts_with("not exercised: "))
}

fn state_name(s: Option<u32>) -> String {
    match s {
        Some(layout::ST_FREE) => "FREE".into(),
        Some(layout::ST_CLAIMED) => "CLAIMED (by another request since)".into(),
        Some(layout::ST_SUBMITTED) => "SUBMITTED (another request since)".into(),
        Some(layout::ST_PROCESSING) => "PROCESSING".into(),
        Some(layout::ST_COMPLETED) => "COMPLETED".into(),
        Some(layout::ST_ABANDONED) => "ABANDONED".into(),
        other => format!("{other:?}"),
    }
}

/// A registry writer and a reader at once (wire.md 4.5): every read made at a
/// generation a write's reply had already published sees that write.
fn registry_race(ring: &Attached, env: &Env, timeout: Duration) -> Result<Check, String> {
    let what = format!(
        "{REG_WRITES} REG_SET_VALUEs of {KEY} from one client while another reads it with REG_KEY"
    )
    .to_string();
    let expected = "a read at a generation a write's reply published sees that write".to_string();
    let client = ring.client();
    let mut scratch = Vec::new();
    let set = |k: u32, scratch: &mut Vec<u8>| {
        client
            .call(
                &Request::RegSetValue {
                    path: KEY,
                    name: "n",
                    ty: 4,
                    data: &k.to_le_bytes(),
                },
                scratch,
            )
            .map(|_| ())
    };
    match set(0, &mut scratch) {
        Ok(_) => {}
        Err(aether_rpc::CallError::Status(Status::NotSupported)) => {
            return Ok((
                what,
                expected,
                "ST_NOT_SUPPORTED".into(),
                Some("not exercised: the director has no registry layer (wire.md 4)".into()),
            ))
        }
        Err(e) => return Err(format!("the first REG_SET_VALUE failed: {e}")),
    }
    let reader = Attached::from_env(env, timeout)?;
    let done = std::sync::atomic::AtomicBool::new(false);
    // Set once the reader has had its first answer: the writes wait for it,
    // so that they overlap reads however late the reader's thread starts.
    let reading = std::sync::atomic::AtomicBool::new(false);
    let (gens, samples) = std::thread::scope(|sc| -> Result<_, String> {
        let r = sc.spawn(|| -> Result<Vec<(u64, u32)>, String> {
            let c = reader.client();
            let mut scratch = Vec::new();
            let mut samples = Vec::new();
            loop {
                let last = done.load(std::sync::atomic::Ordering::Acquire);
                let g = reader.reg_gen();
                match c.call(&Request::RegKey { path: KEY }, &mut scratch) {
                    Ok(Reply::RegKey(k)) => {
                        let v = k
                            .node
                            .as_ref()
                            .and_then(|n| {
                                n.values
                                    .iter()
                                    .find(|v| v.name == "n")
                                    .map(|v| v.data.clone())
                            })
                            .and_then(|d| <[u8; 4]>::try_from(d.as_slice()).ok())
                            .map(u32::from_le_bytes)
                            .ok_or("a REG_KEY reply without the value n")?;
                        samples.push((g, v));
                        reading.store(true, std::sync::atomic::Ordering::Release);
                    }
                    other => return Err(format!("a REG_KEY got {other:?}")),
                }
                if last {
                    return Ok(samples);
                }
            }
        });
        let mut gens = Vec::new();
        let mut scratch = Vec::new();
        let mut failed = None;
        let began = Instant::now();
        while !reading.load(std::sync::atomic::Ordering::Acquire) && !r.is_finished() {
            if began.elapsed() > ring.deadline() {
                failed = Some("the reader had no REG_KEY answer in time".to_string());
                break;
            }
            std::thread::yield_now();
        }
        for k in (1..=REG_WRITES).take_while(|_| failed.is_none()) {
            if let Err(e) = set(k, &mut scratch) {
                failed = Some(format!("REG_SET_VALUE {k} failed: {e}"));
                break;
            }
            gens.push((ring.reg_gen(), k));
        }
        done.store(true, std::sync::atomic::Ordering::Release);
        let samples = r.join().map_err(|_| "the reader panicked".to_string())??;
        match failed {
            Some(e) => Err(e),
            None => Ok((gens, samples)),
        }
    })?;
    // `gens`: the generation read right after write k's reply, which is at
    // least the one the write published, and grows with k.
    let mut bad = None;
    for &(g, v) in &samples {
        let seen = gens
            .iter()
            .take_while(|(gk, _)| *gk <= g)
            .last()
            .map(|&(_, k)| k);
        if let Some(k) = seen {
            if v < k {
                bad = Some(format!(
                    "a REG_KEY asked at generation {g} returned write {v}, though write {k}'s \
                     reply had published generation {g} or less"
                ));
                break;
            }
        }
    }
    Ok((
        what,
        expected,
        bad.clone()
            .unwrap_or_else(|| format!("as expected, over {} reads", samples.len())),
        bad.map(|_| {
            "reg_gen moves only once the write is visible: the shim caches a REG_KEY answer \
             under the generation it read before asking, so an answer older than that \
             generation's write would be served stale (wire.md 4.5: \"It moves on every \
             successful registry write, before the write's reply is published\")"
                .to_string()
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_names_make_a_reply_of_exactly_the_size_asked() {
        for total in [1_048_576u64, 1_048_577, 4096, 4097, 65_536] {
            let names = listing_names("f", total);
            let size: u64 = 4 + names.iter().map(|n| 21 + n.len() as u64).sum::<u64>();
            assert_eq!(size, total);
            let unique: std::collections::BTreeSet<&String> = names.iter().collect();
            assert_eq!(unique.len(), names.len());
            assert!(names.iter().all(|n| n.len() <= 254));
        }
    }

    /// A director too fast to be caught processing is reported, not failed.
    #[test]
    fn a_director_never_caught_processing_passes_as_not_exercised() {
        let (.., got, why) = not_caught("what".into(), "expected".into());
        assert!(passes(&why), "{why:?}");
        assert!(why.unwrap().starts_with("not exercised: "));
        assert!(got.contains(&ABANDON_TRIES.to_string()), "{got}");
        assert!(!passes(&Some(
            "slot 3 went from ABANDONED to COMPLETED".into()
        )));
    }

    #[test]
    fn the_chunk_is_the_shims() {
        let g = Geometry {
            cap: 1_048_576,
            slots: 8,
            arena: 8 << 20,
        };
        assert_eq!(g.chunk(), 1 << 20);
        let g = Geometry {
            arena: 8 * 300 * 1024,
            ..g
        };
        assert_eq!(g.chunk(), 300 * 1024);
        let g = Geometry { arena: 0, ..g };
        assert_eq!(g.chunk(), 1_048_568);
    }
}
