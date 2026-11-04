//! The fuzz targets' bodies: one function per decoder the shim runs on
//! bytes a director produced (or on bytes shared memory could hold). Each
//! takes any byte string and returns; a panic is the bug. Allocation is
//! bounded by the input (or by a fixed size here), and nothing loops on a
//! count the input only claims.
//!
//! They live in an ordinary crate, not in `fuzz/`, so the same bodies run on
//! stable Rust in every CI run: `tests/replay.rs` feeds them [`seeds`], every
//! file under `fuzz/regressions/` and random bytes. `fuzz/` (cargo-fuzz,
//! nightly, Linux, scheduled) calls the same functions with the engine's
//! inputs.
//!
//! Message targets take `opcode | flags:u32 | payload`: the first byte picks
//! the opcode (modulo the number of opcodes), the next four are the request's
//! slot flags, and the rest is the payload decoded as that opcode's message.
//!
//! The seeds are the golden vectors (`vectors.txt`), framed for the target
//! each belongs to, so no binary file is checked in.
#![forbid(unsafe_code)]

use aether_ipc::layout::{
    RING_HEADER_SIZE, SH_ACK, SH_PAYLOAD_LEN, SH_STATE, SH_STATUS, SLOT_HEADER_SIZE,
};
use aether_ipc::{ring, OwnedSeg};
use aether_ntlayout::{write_dir_info, DirInfoClass, DirItem};
use aether_protocol::shimcfg::{decode_config, encode_config_full};
use aether_protocol::{Opcode, ReadReply, Reply, Request, RootId};
use aether_redirect::{RootMap, VolumeMap};

/// The target names, as `fuzz/fuzz_targets/<name>.rs` and [`seeds`] use them.
pub const TARGETS: [&str; 7] = [
    "reply_decode",
    "request_decode",
    "shim_cfg",
    "ring_response",
    "ring_open",
    "dir_records",
    "shim_env",
];

/// The golden vectors, one `name hex` line each.
const VECTORS: &str = include_str!("../../aether-protocol/tests/golden/vectors.txt");

/// A cursor over the input that reads zeros once it runs out, so a short
/// input still picks every field.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn u8(&mut self) -> u8 {
        match self.0.split_first() {
            Some((b, rest)) => {
                self.0 = rest;
                *b
            }
            None => 0,
        }
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    fn rest(self) -> &'a [u8] {
        self.0
    }
}

/// `opcode | flags | payload`, or `None` for an input too short to have a
/// payload field at all.
fn framed(data: &[u8]) -> Option<(Opcode, u32, &[u8])> {
    let (head, payload) = data.split_at_checked(5)?;
    let op = Opcode::ALL[head[0] as usize % Opcode::ALL.len()];
    let flags = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
    Some((op, flags, payload))
}

/// Decode `data` as a reply and use everything it holds: every READDIR
/// entry, every part of a REG_KEY node, a bulk READ's range in a segment,
/// every component of a STORED_NAMES answer. What decodes encodes to bytes
/// that decode to the same reply.
pub fn reply_decode(data: &[u8]) {
    let Some((op, flags, payload)) = framed(data) else {
        return;
    };
    let Ok(reply) = Reply::decode(op, flags, payload) else {
        return;
    };
    match &reply {
        Reply::ReadDir(list) => {
            let mut n = 0usize;
            for e in list {
                n += 1;
                let _ = (e.name.len(), e.is_dir, e.size, e.mtime);
            }
            assert_eq!(n, list.len());
        }
        Reply::RegKey(k) => {
            if let Some(node) = &k.node {
                let _ = (node.created, node.volatile, node.last_write);
                for v in &node.values {
                    let _ = (v.name.len(), v.ty, v.data.len());
                }
                for t in &node.value_tombstones {
                    let _ = t.len();
                }
                for (folded, (spelling, state)) in &node.children {
                    let _ = (folded.len(), spelling.len(), *state);
                }
            }
        }
        Reply::Read(ReadReply::Bulk { len, arena_offset }) => {
            // What the client does with the claim: copy that range out of
            // its mapping, or refuse it.
            let owned = OwnedSeg::new(4096);
            let mut dest = vec![0u8; (*len as usize).min(8192)];
            let _ = owned.seg().copy_to(*arena_offset as usize, &mut dest);
        }
        Reply::StoredNames(text) => {
            // The shim's check of an answer: one stored spelling per
            // component asked about, each folding to the name asked.
            let asked = text.to_ascii_lowercase();
            for (stored, ask) in text.split('/').zip(asked.split('/')) {
                let _ = aether_casefold::eq_folded(stored, ask);
            }
        }
        _ => {}
    }
    let mut again = Vec::new();
    reply.encode(&mut again);
    assert_eq!(Reply::decode(op, flags, &again).as_ref(), Ok(&reply));
}

/// Decode `data` as a request (the director side's decoder), and check
/// what decoded: it names its opcode, and re-encodes to a payload that
/// decodes to the same request.
pub fn request_decode(data: &[u8]) {
    let Some((op, flags, payload)) = framed(data) else {
        return;
    };
    let Ok(req) = Request::decode(op, flags, payload) else {
        return;
    };
    assert_eq!(req.opcode(), op);
    let mut again = Vec::new();
    req.encode(&mut again);
    assert_eq!(Request::decode(op, req.flags(), &again), Ok(req));
}

/// Decode `data` as the shim's start-up config; what decodes re-encodes to
/// the same bytes (the format has one spelling).
pub fn shim_cfg(data: &[u8]) {
    if let Ok(cfg) = decode_config(data) {
        assert_eq!(encode_config_full(&cfg.root, &cfg.static_imports), data);
    }
}

/// The ring [`ring_response`] builds: two slots of 256 payload bytes, in a
/// segment exactly that size, so a slot past the second lies outside it.
const SLOTS: u32 = 2;
const PAYLOAD_CAP: u32 = 256;

/// A client reading a slot whose header says what `data` says:
///
/// `slot | state | ack_honest | status:i32 | payload_len:u32 | ack:u32 |
/// payload`
///
/// The slot may be past the ring, the state any of six, the ack the
/// request's or not, the length anything. `take_response_into` must answer
/// `Ok`, `Err` or `None` and never read outside the segment or return more
/// than the payload capacity; a bulk READ reply it returns is then taken as
/// the client takes it.
pub fn ring_response(data: &[u8]) {
    const REQ_ID: u64 = 0x0000_0001_0000_0007;
    let stride = (SLOT_HEADER_SIZE + PAYLOAD_CAP as usize).next_multiple_of(8);
    let owned = OwnedSeg::new(RING_HEADER_SIZE + SLOTS as usize * stride);
    let seg = owned.seg();
    let Ok(geom) = ring::init(seg, SLOTS, PAYLOAD_CAP) else {
        panic!("a ring of the segment's own size does not fit it");
    };
    let mut f = Fields(data);
    let slot = u32::from(f.u8() % 4);
    let state = u32::from(f.u8() % 6);
    let honest = f.u8() & 1 == 0;
    let status = f.u32();
    let len = f.u32();
    let ack = f.u32();
    let payload = f.rest();
    // Slot 0 is the one filled; a slot past the ring has no header at all.
    let base = geom.slot_off(0);
    seg.write_bytes(
        base + SLOT_HEADER_SIZE,
        &payload[..payload.len().min(PAYLOAD_CAP as usize)],
    );
    let ack = if honest { REQ_ID as u32 } else { ack };
    seg.write_bytes(base + SH_ACK, &ack.to_le_bytes());
    seg.write_bytes(base + SH_STATUS, &status.to_le_bytes());
    seg.write_bytes(base + SH_PAYLOAD_LEN, &len.to_le_bytes());
    seg.write_bytes(base + SH_STATE, &state.to_le_bytes());

    let mut out = vec![0xAA; 7];
    if let Ok(Some(_)) = ring::take_response_into(seg, &geom, slot, REQ_ID, &mut out) {
        assert!(out.len() <= PAYLOAD_CAP as usize);
        if let Ok(Reply::Read(ReadReply::Bulk { len, arena_offset })) =
            Reply::decode(Opcode::Read, 0, &out)
        {
            let mut dest = vec![0u8; (len as usize).min(8192)];
            let _ = seg.copy_to(arena_offset as usize, &mut dest);
        }
    }
    // The owning form, and the state word, behave the same.
    let _ = ring::take_response(seg, &geom, slot, REQ_ID);
    let _ = ring::slot_state(seg, &geom, slot);
}

/// The segment sizes [`ring_open`] maps: none, too short for the header,
/// the header alone, a few small slots, and two of a real ring's sizes.
const RING_SEGS: [usize; 6] = [0, 47, 48, 96, 4096, 65536];

/// A client attaching to a ring whose header the director wrote
/// (`docs/wire.md` 2.2, 2.4): `size | mapping bytes`, where `size` picks one
/// of [`RING_SEGS`] and the bytes, from offset 0, are the header (magic,
/// version, geometry, worker hint, registry generation) and then the slots;
/// what the input does not cover is zero.
///
/// `ring::open` must accept only a geometry that fits the mapping, with a
/// stride that holds the payload capacity, whatever the numbers (on a
/// 32-bit client too). What a client then works out from it stays inside
/// the mapping: each slot's offset and the end of the last, the data gate
/// the worker hint sizes, the registry generation, and a response taken
/// from every slot.
pub fn ring_open(data: &[u8]) {
    const REQ_ID: u64 = 0x0000_0001_0000_0007;
    let mut f = Fields(data);
    let len = RING_SEGS[usize::from(f.u8()) % RING_SEGS.len()];
    let bytes = f.rest();
    let owned = OwnedSeg::new(len);
    let seg = owned.seg();
    assert!(seg.write_bytes(0, &bytes[..bytes.len().min(len)]));
    let Ok(geom) = ring::open(seg) else {
        return;
    };
    let stride = u64::from(geom.slot_stride);
    assert_eq!(stride % 8, 0, "{geom:?}");
    assert!(
        stride >= SLOT_HEADER_SIZE as u64 + u64::from(geom.payload_cap),
        "{geom:?}"
    );
    let end = RING_HEADER_SIZE as u64 + u64::from(geom.slot_count) * stride;
    assert!(end <= len as u64, "{geom:?} in {len} bytes");
    assert_eq!(geom.slot_off(geom.slot_count) as u64, end);
    let gate = aether_ipc::DataGate::for_ring(seg, &geom);
    assert!(gate.limit() >= 1);
    let _ = ring::reg_generation(seg);
    // Every slot, and the one past the last, which a client never asks for
    // but whose bytes the mapping may still hold.
    for slot in 0..=geom.slot_count.min(64) {
        if let Ok(Some((_, out))) = ring::take_response(seg, &geom, slot, REQ_ID) {
            assert!(out.len() <= geom.payload_cap as usize);
        }
        let _ = ring::slot_state(seg, &geom, slot);
    }
}

/// A READDIR listing as the shim writes it into a caller's buffer. `data`
/// is `buffer_len:u16 | class | single | reply payload`: the buffer is that
/// long, the class is one of the eight the shim marshals (or one it does
/// not), and the payload decodes as a READDIR reply. Whatever the listing
/// holds, the records stay inside the buffer.
pub fn dir_records(data: &[u8]) {
    let mut f = Fields(data);
    let buf_len = usize::from(u16::from_le_bytes([f.u8(), f.u8()]));
    let class = usize::from(f.u8());
    let single = f.u8() & 1 == 1;
    // The classes the shim marshals, and one number it does not.
    const CLASSES: [u32; 9] = [1, 2, 3, 12, 37, 38, 60, 63, 0];
    let class = DirInfoClass::from_u32(CLASSES[class % CLASSES.len()]);
    let Ok(Reply::ReadDir(list)) = Reply::decode(Opcode::ReadDir, 0, f.rest()) else {
        return;
    };
    let Some(class) = class else {
        return;
    };
    let items: Vec<DirItem> = list
        .iter()
        .map(|e| DirItem {
            name: e.name.to_string(),
            is_dir: e.is_dir,
            size: e.size,
            mtime: e.mtime,
        })
        .collect();
    let mut buf = vec![0u8; buf_len];
    let r = write_dir_info(class, &items, &mut buf, single);
    assert!(r.bytes <= buf_len);
    assert!(r.count <= items.len());
    assert!(!single || r.count <= 1);
}

/// The variables [`shim_env`] sets, in the order its input lists them, each
/// with a value a director would give it. The names are `aether_env`'s, so a
/// renamed variable is a compile error here.
const ENV_FIELDS: [(&str, &str); 15] = [
    (aether_env::VIRTUAL_DIR, r"C:\Games\Skyrim"),
    (aether_env::VIRTUAL_ROOTS, r"1=C:\Games\Saves;2=\??\C:\Mods"),
    (
        aether_env::ROOT_BACKINGS,
        r"1=C:/Backing/Saves;2=\\?\D:\Mods",
    ),
    (aether_env::RING_BYTES, "2097152"),
    (aether_env::RING_PAYLOAD_CAP, "1048576"),
    (aether_env::ARENA_LEN, "8388608"),
    (aether_env::ARENA_OFFSET, "65536"),
    (aether_env::RING_PATH, "/tmp/aether/ring"),
    (aether_env::RING_SECTION, r"Local\aether-ring-1"),
    (aether_env::REGISTRY, "1"),
    (aether_env::READY_TIMEOUT_SECS, "180"),
    (aether_env::SHIM_STATS_INTERVAL_MS, "250"),
    (aether_env::SHIM_READ_CACHE_MIB, "256"),
    (aether_env::SHIM_READ_CACHE, "off"),
    (aether_env::ALLOW_DISK_FALLTHROUGH, "yes"),
];

/// Values of the shim's director-controlled environment, as the shim reads
/// them (`aether_env::session`): each parser takes any text and must return,
/// and what it returns keeps the promises its documentation makes. The input
/// is the values of [`ENV_FIELDS`] in order, separated by NUL (which no
/// environment value can hold); an empty field is an unset variable, and a
/// field of one `0x01` byte is one set to the empty string. Bytes that are
/// not UTF-8 read as the replacement character, as the shim reads a value it
/// can, and a variable it cannot is unset.
pub fn shim_env(data: &[u8]) {
    use aether_env::session as s;
    let mut parts = data.split(|b| *b == 0);
    let mut field = || -> Option<String> {
        match parts.next() {
            None | Some([]) => None,
            Some([1]) => Some(String::new()),
            Some(v) => Some(String::from_utf8_lossy(v).into_owned()),
        }
    };
    let [dir, extra, backs, bytes, cap, arena_len, arena_off, path, section, registry, ready, stats, cache_mib, cache, disk] =
        std::array::from_fn(|_| field());
    fn r(v: &Option<String>) -> Option<&str> {
        v.as_deref()
    }

    let roots = s::roots(r(&dir).unwrap_or(""), r(&extra));
    assert_eq!(roots[0].0, 0, "root 0 comes first");
    let mut ids: Vec<u32> = roots.iter().map(|(id, _)| *id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), roots.len(), "an id is declared once");
    assert!(roots.iter().skip(1).all(|(_, p)| !p.is_empty()));

    let backings = s::backings(r(&backs));
    for (_, p) in &backings {
        assert!(!p.contains('/') && *p == p.to_ascii_lowercase());
    }
    // What the shim does with the roots and backings it was given: build
    // its root map, which refuses one that is no usable path (and so fails
    // the load) and never panics.
    let declared: Vec<(RootId, &str)> = roots
        .iter()
        .chain(&backings)
        .map(|(id, p)| (RootId(*id), p.as_str()))
        .collect();
    let _ = RootMap::with_roots(&declared, VolumeMap::empty());
    let ring = s::ring(r(&bytes), r(&cap), r(&arena_len), r(&arena_off));
    let _ = (
        ring.bytes,
        ring.payload_cap,
        ring.arena_len,
        ring.arena_offset,
    );
    let source = s::ring_source(path.clone(), section.clone());
    assert_eq!(
        source.is_none(),
        path.as_deref().is_none_or(str::is_empty) && section.is_none()
    );
    assert!(s::ready_timeout_secs(r(&ready)) >= 1);
    let _ = s::stats_interval_ms(r(&stats));
    assert_ne!(s::read_cache_mib(r(&cache_mib)), Some(0));
    for v in [&registry, &cache, &disk] {
        // The two readings differ only in what they do with no value.
        if s::opt_in(r(v)) {
            assert!(s::opt_out(r(v)));
        }
    }
}

/// The seeds of [`shim_env`], from `contract/env.json`: for each handshake
/// variable the shim reads, an environment that sets it (to the value a
/// director gives it) and nothing else, and one that sets them all.
fn env_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let contract = include_str!("../../../contract/env.json");
    let named: Vec<&str> = contract
        .split('"')
        .filter(|s| s.starts_with("VFS_"))
        .collect();
    let join = |keep: &dyn Fn(&str) -> bool| -> Vec<u8> {
        let mut v = Vec::new();
        for (i, (name, value)) in ENV_FIELDS.iter().enumerate() {
            if i > 0 {
                v.push(0);
            }
            if keep(name) {
                v.extend_from_slice(value.as_bytes());
            }
        }
        v
    };
    let mut out = vec![("shim_env", join(&|_| true))];
    for name in named {
        if ENV_FIELDS.iter().any(|(n, _)| *n == name) {
            out.push(("shim_env", join(&|n| n == name)));
        }
    }
    out
}

/// The handshake variables of `contract/env.json` the shim parses: the
/// seeds' coverage of them, for the test that keeps the two in step.
pub fn parsed_handshake_names() -> Vec<&'static str> {
    ENV_FIELDS
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| aether_env::handshake::is_handshake(n))
        .collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("vectors.txt is hex"))
        .collect()
}

/// `opcode | flags | payload`, for `opcode`'s table name.
fn frame(opcode: &str, payload: &[u8]) -> Vec<u8> {
    let i = Opcode::ALL
        .iter()
        .position(|o| o.name() == opcode)
        .unwrap_or_else(|| panic!("no opcode named {opcode}"));
    let mut v = vec![i as u8, 0, 0, 0, 0];
    v.extend_from_slice(payload);
    v
}

/// The opcode a vector's name belongs to, and whether it is a request (not a
/// reply). Vectors that are no message (the ring header) have none.
fn message_of(name: &str) -> Option<(&'static str, bool)> {
    const BY_PREFIX: [(&str, &str, bool); 25] = [
        ("open-req", "open", true),
        ("open-resp", "open", false),
        ("getattr-resp", "getattr", false),
        ("path-req", "getattr", true),
        ("read-req", "read", true),
        ("read-resp", "read", false),
        ("readdir-resp", "readdir", false),
        ("close-req", "close", true),
        ("write-req", "write", true),
        ("write-resp", "write", false),
        ("mkdir-req", "mkdir", true),
        ("rename-req", "rename", true),
        ("setattr-req", "setattr", true),
        ("names-req", "stored-names", true),
        ("names-resp", "stored-names", false),
        ("reg-path", "reg-lookup", true),
        ("reg-set-value", "reg-set-value", true),
        ("reg-delete-value", "reg-delete-value", true),
        ("reg-create-key", "reg-create-key", true),
        ("reg-rename-key", "reg-rename-key", true),
        ("reg-changed-subtree", "reg-changed", true),
        ("reg-changed-reply", "reg-changed", false),
        ("reg-version-reply", "reg-set-value", false),
        ("reg-lookup-reply", "reg-lookup", false),
        ("reg-key-reply", "reg-key", false),
    ];
    BY_PREFIX
        .iter()
        .find(|(p, _, _)| name.starts_with(p))
        .map(|(_, op, req)| (*op, *req))
}

/// Every seed: the target's name and an input for it, built from the golden
/// vectors. Each message vector seeds its decoder (a reply seeds the ring
/// and directory targets too, as a slot's payload and as a listing), each
/// shim config its own target.
pub fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();
    for line in VECTORS.lines() {
        let Some((name, hex)) = line.split_once(' ') else {
            continue;
        };
        let bytes = unhex(hex.trim());
        if name.starts_with("shim-config") {
            out.push(("shim_cfg", bytes));
            continue;
        }
        if name.starts_with("ring-header") {
            // The header in each size of mapping.
            for size in 0..RING_SEGS.len() as u8 {
                let mut v = vec![size];
                v.extend_from_slice(&bytes);
                out.push(("ring_open", v));
            }
            continue;
        }
        let Some((op, is_req)) = message_of(name) else {
            continue;
        };
        if is_req {
            out.push(("request_decode", frame(op, &bytes)));
            continue;
        }
        out.push(("reply_decode", frame(op, &bytes)));
        // A completed slot answering with this reply: honest, then lying
        // about its length, then about its ack.
        for (ack_lies, len) in [
            (0u8, bytes.len() as u32),
            (0, u32::MAX),
            (1, bytes.len() as u32),
        ] {
            let mut v = vec![0, 4, ack_lies];
            v.extend_from_slice(&0i32.to_le_bytes());
            v.extend_from_slice(&len.to_le_bytes());
            v.extend_from_slice(&7u32.to_le_bytes());
            v.extend_from_slice(&bytes);
            out.push(("ring_response", v));
        }
        if op == "readdir" {
            for class in 0..8u8 {
                let mut v = Vec::new();
                v.extend_from_slice(&1024u16.to_le_bytes());
                v.extend_from_slice(&[class, class & 1]);
                v.extend_from_slice(&bytes);
                out.push(("dir_records", v));
            }
            // A buffer too small for the first record.
            let mut v = vec![8, 0, 1, 0];
            v.extend_from_slice(&bytes);
            out.push(("dir_records", v));
        }
    }
    out.extend(env_seeds());
    out.extend(ring_open_seeds());
    out
}

/// Headers [`ring_open`] starts from beyond the golden one: rings as
/// `ring::init` lays them out in each mapping, with a worker hint and a
/// registry generation set and one slot completed, and headers whose
/// `payload_cap` is within 32 bytes of `u32::MAX` with the stride 32-bit
/// arithmetic would give it.
fn ring_open_seeds() -> Vec<(&'static str, Vec<u8>)> {
    use aether_ipc::layout::{RH_PAYLOAD_CAP, RH_SLOT_COUNT, RH_SLOT_STRIDE};
    let mut out = Vec::new();
    for (size, &len) in RING_SEGS.iter().enumerate() {
        for (slots, cap) in [(1u32, 8u32), (2, 16), (4, 256), (8, 4096), (1, 65_000)] {
            let owned = OwnedSeg::new(len);
            let seg = owned.seg();
            let Ok(geom) = ring::init(seg, slots, cap) else {
                continue;
            };
            ring::set_worker_hint(seg, 4);
            ring::publish_reg_generation(seg, 9);
            let s = geom.slot_off(0);
            seg.write_bytes(s + SH_ACK, &7u32.to_le_bytes());
            seg.write_bytes(s + SH_PAYLOAD_LEN, &4u32.to_le_bytes());
            seg.write_bytes(
                s + SH_STATE,
                &aether_ipc::layout::ST_COMPLETED.to_le_bytes(),
            );
            let mut v = vec![size as u8];
            let mut mapping = vec![0u8; len];
            let _ = seg.copy_to(0, &mut mapping);
            // The header and slot 0's: past them the mapping is zero, as the
            // input's end leaves it, and a short seed mutates quickly.
            let used = geom.slot_off(geom.slot_count).min(s + SLOT_HEADER_SIZE + 4);
            v.extend_from_slice(&mapping[..used]);
            out.push(("ring_open", v));
        }
    }
    let owned = OwnedSeg::new(RING_HEADER_SIZE);
    let seg = owned.seg();
    ring::init(seg, 0, 8).expect("an empty ring fits its header");
    for cap in [u32::MAX, u32::MAX - 7, u32::MAX - 31] {
        let wrapped = 32u32.wrapping_add(cap).wrapping_add(7) & !7;
        seg.write_bytes(RH_SLOT_COUNT, &1u32.to_le_bytes());
        seg.write_bytes(RH_SLOT_STRIDE, &wrapped.to_le_bytes());
        seg.write_bytes(RH_PAYLOAD_CAP, &cap.to_le_bytes());
        let mut v = vec![RING_SEGS.len() as u8 - 1];
        let mut header = vec![0u8; RING_HEADER_SIZE];
        let _ = seg.copy_to(0, &mut header);
        v.extend_from_slice(&header);
        out.push(("ring_open", v));
    }
    out
}

/// Run the body of the target named `target` on `data`.
pub fn run(target: &str, data: &[u8]) {
    match target {
        "reply_decode" => reply_decode(data),
        "request_decode" => request_decode(data),
        "shim_cfg" => shim_cfg(data),
        "ring_response" => ring_response(data),
        "ring_open" => ring_open(data),
        "dir_records" => dir_records(data),
        "shim_env" => shim_env(data),
        other => panic!("no fuzz target named {other}"),
    }
}
