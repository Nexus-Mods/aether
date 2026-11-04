//! Ring init/open + slot state-machine primitives.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::layout::*;
use crate::seg::SharedSeg;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
    RingFull,
    /// A published request was never answered within [`crate::RESPONSE_DEADLINE`].
    ///
    /// Waiting for a response forever would be a latent process hang, not
    /// merely a slow path: `wait_client` is advisory (the shim's implementation
    /// is a bare `spin_loop()`), so a response that never arrives would leave
    /// the caller looping inside whichever NT call it was serving.
    /// For a game that is a permanent freeze mid-read rather than an I/O error
    /// it could report or retry. `claim_slot` already bounded its sibling loop;
    /// this is the other half.
    Timeout,
    PayloadTooLarge,
    BadResponse,
    Closed,
    Layout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geom {
    pub slot_count: u32,
    pub slot_stride: u32,
    pub payload_cap: u32,
}

impl Geom {
    pub fn slot_off(&self, slot: u32) -> usize {
        RING_HEADER_SIZE + slot as usize * self.slot_stride as usize
    }
    pub(crate) fn payload_off(&self, slot: u32) -> usize {
        self.slot_off(slot) + SLOT_HEADER_SIZE
    }
}

fn state<'a>(seg: &'a SharedSeg, geom: &Geom, slot: u32) -> Option<&'a AtomicU32> {
    seg.atomic_u32(geom.slot_off(slot) + SH_STATE)
}

/// The state word of `slot` right now (one of the `ST_*` constants), or None
/// if the slot lies outside the segment. A snapshot, for diagnostics and
/// tests: any other party may change it the moment it is read.
pub fn slot_state(seg: &SharedSeg, geom: &Geom, slot: u32) -> Option<u32> {
    state(seg, geom, slot).map(|st| st.load(Ordering::Acquire))
}

/// `align8(32 + payload_cap)`, the stride of a slot (`docs/wire.md` 2.1),
/// worked in 64 bits: `None` when it does not fit the header's u32. In a
/// 32-bit `usize` the sum wraps for a cap within 32 bytes of `u32::MAX`.
fn stride_of(payload_cap: u32) -> Option<u32> {
    let n = SLOT_HEADER_SIZE as u64 + u64::from(payload_cap);
    u32::try_from((n + 7) & !7).ok()
}

/// The bytes a ring of `slot_count` slots of `stride` takes, header
/// included, if a `usize` holds them.
fn ring_len(slot_count: u32, stride: u32) -> Option<usize> {
    let total = RING_HEADER_SIZE as u64 + u64::from(slot_count) * u64::from(stride);
    usize::try_from(total).ok()
}

/// Lay out an empty ring in `seg`. Returns its geometry.
pub fn init(seg: &SharedSeg, slot_count: u32, payload_cap: u32) -> Result<Geom, IpcError> {
    let stride = stride_of(payload_cap).ok_or(IpcError::Layout)?;
    match ring_len(slot_count, stride) {
        Some(total) if total <= seg.len() => {}
        _ => return Err(IpcError::Layout),
    }
    seg.write_u32(RH_MAGIC, MAGIC);
    seg.write_u32(RH_VERSION, VERSION);
    seg.write_u32(RH_SLOT_COUNT, slot_count);
    seg.write_u32(RH_SLOT_STRIDE, stride);
    seg.write_u32(RH_PAYLOAD_CAP, payload_cap);
    seg.write_u32(RH_WORKER_HINT, 0);
    seg.write_u64(RH_REQ_SEQ, 0);
    seg.write_u32(RH_SUBMIT_SEQ, 0);
    seg.write_u64(RH_REG_GEN, 0);
    let geom = Geom {
        slot_count,
        slot_stride: stride,
        payload_cap,
    };
    for s in 0..slot_count {
        seg.write_u32(geom.slot_off(s) + SH_STATE, ST_FREE);
    }
    Ok(geom)
}

/// Validate an existing ring; return its geometry.
pub fn open(seg: &SharedSeg) -> Result<Geom, IpcError> {
    if seg.len() < RING_HEADER_SIZE {
        return Err(IpcError::Layout);
    }
    if seg.read_u32(RH_MAGIC) != Some(MAGIC) || seg.read_u32(RH_VERSION) != Some(VERSION) {
        return Err(IpcError::Layout);
    }
    let slot_count = seg.read_u32(RH_SLOT_COUNT).ok_or(IpcError::Layout)?;
    let slot_stride = seg.read_u32(RH_SLOT_STRIDE).ok_or(IpcError::Layout)?;
    let payload_cap = seg.read_u32(RH_PAYLOAD_CAP).ok_or(IpcError::Layout)?;
    // Worked in 64 bits on every target (a 32-bit `usize` wraps), and a
    // header whose numbers do not fit is refused.
    if stride_of(payload_cap) != Some(slot_stride) {
        return Err(IpcError::Layout);
    }
    match ring_len(slot_count, slot_stride) {
        Some(total) if total <= seg.len() => {}
        _ => return Err(IpcError::Layout),
    }
    Ok(Geom {
        slot_count,
        slot_stride,
        payload_cap,
    })
}

/// Server: say how many threads serve this ring. Call once, after [`init`]
/// and before a client attaches.
pub fn set_worker_hint(seg: &SharedSeg, workers: u32) {
    seg.write_u32(RH_WORKER_HINT, workers);
}

/// Client: the server's worker count, or 0 if it did not say.
pub fn worker_hint(seg: &SharedSeg) -> u32 {
    seg.read_u32(RH_WORKER_HINT).unwrap_or(0)
}

/// Server: publish registry generation `generation` ([`RingHeader::reg_gen`])
/// to every client of this ring. Never moves the word backwards, so two
/// publishers racing leave the larger value whatever order their stores land in.
pub fn publish_reg_generation(seg: &SharedSeg, generation: u64) {
    if let Some(w) = seg.atomic_u64(RH_REG_GEN) {
        w.fetch_max(generation, Ordering::AcqRel);
    }
}

/// Client: the registry generation the server last published, or 0 if none.
pub fn reg_generation(seg: &SharedSeg) -> u64 {
    seg.atomic_u64(RH_REG_GEN)
        .map(|w| w.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Claim a FREE slot → CLAIMED. Returns the slot index, or None if the ring is full.
pub fn claim_free(seg: &SharedSeg, geom: &Geom) -> Option<u32> {
    claim_free_from(seg, geom, 0)
}

/// [`claim_free`], looking at `start` first and wrapping round.
///
/// Several client threads that each start at the slot they last held find it
/// free again without contending for slot 0, and keep reusing the same
/// payload pages.
pub fn claim_free_from(seg: &SharedSeg, geom: &Geom, start: u32) -> Option<u32> {
    let n = geom.slot_count;
    for i in 0..n {
        let s = (start.wrapping_add(i)) % n;
        if let Some(st) = state(seg, geom, s) {
            // Looked at before it is swapped: a failed compare-and-swap still
            // takes the slot's cache line exclusively, and a caller waiting on
            // a full ring makes one per slot per pass.
            if st.load(Ordering::Relaxed) == ST_FREE
                && st
                    .compare_exchange(ST_FREE, ST_CLAIMED, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                return Some(s);
            }
        }
    }
    None
}

/// Write a request into a CLAIMED slot and publish SUBMITTED. Returns the req_id.
pub fn publish_request(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    opcode: u32,
    flags: u32,
    payload: &[u8],
) -> Result<u64, IpcError> {
    if payload.len() > geom.payload_cap as usize {
        return Err(IpcError::PayloadTooLarge);
    }
    seg.write_bytes(geom.payload_off(slot), payload);
    publish_written(seg, geom, slot, opcode, flags, payload.len())
}

/// [`publish_request`] for a request whose `len` payload bytes are already in
/// the CLAIMED slot's payload region (written there through
/// [`payload_region`]). Returns the req_id.
pub fn publish_written(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    opcode: u32,
    flags: u32,
    len: usize,
) -> Result<u64, IpcError> {
    if len > geom.payload_cap as usize {
        return Err(IpcError::PayloadTooLarge);
    }
    let base = geom.slot_off(slot);
    let req_id = seg
        .atomic_u64(RH_REQ_SEQ)
        .ok_or(IpcError::Layout)?
        .fetch_add(1, Ordering::Relaxed);
    seg.write_u32(base + SH_OPCODE, opcode);
    seg.write_u32(base + SH_FLAGS, flags);
    seg.write_u32(base + SH_PAYLOAD_LEN, len as u32);
    seg.write_u64(base + SH_REQ_ID, req_id);
    state(seg, geom, slot)
        .ok_or(IpcError::Layout)?
        .store(ST_SUBMITTED, Ordering::Release);
    seg.atomic_u32(RH_SUBMIT_SEQ)
        .ok_or(IpcError::Layout)?
        .fetch_add(1, Ordering::Relaxed);
    Ok(req_id)
}

/// Publish a CLAIMED slot whose header says `claimed` payload bytes, which
/// may be more than `payload_cap`, with whatever the payload region holds.
/// Returns the req_id.
///
/// No client sends this: it is for a conformance tool checking that a
/// director completes such a slot with `ST_BAD_REQUEST` and no payload rather
/// than leaving it `PROCESSING` (`docs/wire.md` 2.5: "A slot whose
/// `payload_len > payload_cap` MUST be completed with `ST_BAD_REQUEST`").
/// Everything else is as [`publish_written`] does it.
pub fn publish_claiming(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    opcode: u32,
    flags: u32,
    claimed: u32,
) -> Result<u64, IpcError> {
    let base = geom.slot_off(slot);
    let req_id = seg
        .atomic_u64(RH_REQ_SEQ)
        .ok_or(IpcError::Layout)?
        .fetch_add(1, Ordering::Relaxed);
    seg.write_u32(base + SH_OPCODE, opcode);
    seg.write_u32(base + SH_FLAGS, flags);
    seg.write_u32(base + SH_PAYLOAD_LEN, claimed);
    seg.write_u64(base + SH_REQ_ID, req_id);
    state(seg, geom, slot)
        .ok_or(IpcError::Layout)?
        .store(ST_SUBMITTED, Ordering::Release);
    seg.atomic_u32(RH_SUBMIT_SEQ)
        .ok_or(IpcError::Layout)?
        .fetch_add(1, Ordering::Relaxed);
    Ok(req_id)
}

/// Server: claim a SUBMITTED slot → PROCESSING. Returns the slot index.
pub fn server_take(seg: &SharedSeg, geom: &Geom) -> Option<u32> {
    server_take_from(seg, geom, 0)
}

/// [`server_take`], looking at `start` first and wrapping round.
///
/// A server thread that starts after the slot it last served cannot pass a
/// waiting high slot over for ever in favour of low ones that keep being
/// resubmitted — which a scan from 0 does once there are more clients in
/// flight than workers.
pub fn server_take_from(seg: &SharedSeg, geom: &Geom, start: u32) -> Option<u32> {
    let n = geom.slot_count;
    for i in 0..n {
        let s = (start.wrapping_add(i)) % n;
        if let Some(st) = state(seg, geom, s) {
            if st
                .compare_exchange(
                    ST_SUBMITTED,
                    ST_PROCESSING,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return Some(s);
            }
        }
    }
    None
}

/// Read (opcode, flags, req_id, payload) from a PROCESSING slot.
pub fn read_request(seg: &SharedSeg, geom: &Geom, slot: u32) -> Option<(u32, u32, u64, Vec<u8>)> {
    let mut payload = Vec::new();
    let head = read_request_into(seg, geom, slot, &mut payload)?;
    if !head.fits(geom) {
        return None;
    }
    Some((head.opcode, head.flags, head.req_id, payload))
}

/// A request's slot header, as [`read_request_into`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestHeader {
    pub opcode: u32,
    pub flags: u32,
    pub req_id: u64,
    /// As the client wrote it: it may claim more than the slot holds.
    pub payload_len: u32,
}

impl RequestHeader {
    /// The payload length is within the ring's `payload_cap`, so the payload
    /// was read.
    pub fn fits(&self, geom: &Geom) -> bool {
        self.payload_len <= geom.payload_cap
    }
}

/// [`read_request`], copying the payload into `out` (cleared first), which
/// grows only when its capacity is short. A header whose payload length is
/// past `payload_cap` is still returned, with `out` empty, so the director
/// can answer it (`docs/wire.md` 2.5: every taken slot is completed). `None`
/// only when the header lies outside the segment.
pub fn read_request_into(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    out: &mut Vec<u8>,
) -> Option<RequestHeader> {
    let base = geom.slot_off(slot);
    let head = RequestHeader {
        opcode: seg.read_u32(base + SH_OPCODE)?,
        flags: seg.read_u32(base + SH_FLAGS)?,
        req_id: seg.read_u64(base + SH_REQ_ID)?,
        payload_len: seg.read_u32(base + SH_PAYLOAD_LEN)?,
    };
    if head.fits(geom) {
        copy_into(seg, geom.payload_off(slot), head.payload_len as usize, out)?;
    } else {
        out.clear();
    }
    Some(head)
}

/// Replace `out` with the `len` bytes at `off`, reusing its capacity.
fn copy_into(seg: &SharedSeg, off: usize, len: usize, out: &mut Vec<u8>) -> Option<()> {
    out.clear();
    out.resize(len, 0);
    seg.copy_to(off, out)
}

/// Run `f` over the payload region of `slot`, all `payload_cap` bytes of it,
/// for a client to write a request into a slot it has CLAIMED before
/// [`publish_written`]. `None` if the region is outside the segment.
pub fn payload_region<R>(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    f: impl FnOnce(&mut [u8]) -> R,
) -> Option<R> {
    seg.with_mut_bytes(geom.payload_off(slot), geom.payload_cap as usize, f)
}

/// What became of a response handed to [`server_complete`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Published: the client that sent the request will read it.
    Delivered,
    /// The client had stopped waiting ([`ST_ABANDONED`]). The response was
    /// dropped and the slot freed.
    Drained,
    /// The slot no longer holds the request this response answers. Nothing
    /// was written.
    Stale,
}

/// Server: write the response to request `req_id` into its slot and publish
/// COMPLETED — or, if the client stopped waiting, free the slot instead.
///
/// From [`server_take`] until this function stores a state the slot is this
/// server thread's alone: a client that gives up moves it PROCESSING →
/// ABANDONED ([`abandon`]) rather than freeing it, so nobody can claim the
/// slot — or its arena bank — while the response is still being produced.
/// That is what makes writing the payload here safe, and what keeps a late
/// reply from landing in a slot a later request owns.
///
/// `req_id` is checked all the same, before anything is written: a slot whose
/// id changed was recycled by a client that does not follow that rule, and
/// the only safe thing to do with this response is drop it.
pub fn server_complete(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    req_id: u64,
    status: i32,
    resp: &[u8],
) -> Result<Completion, IpcError> {
    let base = geom.slot_off(slot);
    let st = state(seg, geom, slot).ok_or(IpcError::Layout)?;
    if seg.read_u64(base + SH_REQ_ID) != Some(req_id) {
        return Ok(Completion::Stale);
    }
    match st.load(Ordering::Acquire) {
        ST_PROCESSING => {}
        ST_ABANDONED => return Ok(drain(st)),
        _ => return Ok(Completion::Stale),
    }
    let (status, len) = sent(geom.payload_cap, status, resp.len());
    if len > 0 {
        seg.write_bytes(geom.payload_off(slot), &resp[..len]);
    }
    seg.write_i32(base + SH_STATUS, status);
    seg.write_u32(base + SH_PAYLOAD_LEN, len as u32);
    seg.write_u32(base + SH_ACK, req_id as u32);
    match st.compare_exchange(
        ST_PROCESSING,
        ST_COMPLETED,
        Ordering::Release,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(Completion::Delivered),
        // The client gave up while the response was being written.
        Err(ST_ABANDONED) => Ok(drain(st)),
        Err(_) => Ok(Completion::Stale),
    }
}

/// The status and payload length [`server_complete`] sends for a reply of
/// `len` bytes with `status`: as given, or the overflow sentinel `i32::MIN`
/// with no payload when `len` is past `payload_cap` (`docs/wire.md` 2.5).
/// Public so a tap that records what the client got applies the same rule.
pub fn sent(payload_cap: u32, status: i32, len: usize) -> (i32, usize) {
    if len > payload_cap as usize {
        (i32::MIN, 0) // overflow sentinel; bulk arena deferred
    } else {
        (status, len)
    }
}

/// ABANDONED → FREE: the late reply is dropped, the slot is claimable again.
fn drain(st: &AtomicU32) -> Completion {
    match st.compare_exchange(ST_ABANDONED, ST_FREE, Ordering::Release, Ordering::Relaxed) {
        Ok(_) => Completion::Drained,
        Err(_) => Completion::Stale,
    }
}

/// Client: if COMPLETED, read (status, payload). `Ok(None)` if not yet
/// completed.
///
/// `req_id` is the id [`publish_request`] returned for this slot. A completed
/// slot whose [`SlotHeader::ack`] names another request holds some other
/// request's reply, and is refused as [`IpcError::BadResponse`] rather than
/// returned as this one's.
pub fn take_response(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    req_id: u64,
) -> Result<Option<(i32, Vec<u8>)>, IpcError> {
    let mut payload = Vec::new();
    Ok(take_response_into(seg, geom, slot, req_id, &mut payload)?.map(|status| (status, payload)))
}

/// [`take_response`], copying the payload into `out` (cleared first), which
/// grows only when its capacity is short. Returns the status. `out` is left
/// as it was while the response has not arrived.
pub fn take_response_into(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    req_id: u64,
    out: &mut Vec<u8>,
) -> Result<Option<i32>, IpcError> {
    let st = state(seg, geom, slot).ok_or(IpcError::Layout)?;
    if st.load(Ordering::Acquire) != ST_COMPLETED {
        return Ok(None);
    }
    let base = geom.slot_off(slot);
    if seg.read_u32(base + SH_ACK) != Some(req_id as u32) {
        return Err(IpcError::BadResponse);
    }
    let status = seg.read_i32(base + SH_STATUS).ok_or(IpcError::Layout)?;
    let len = seg
        .read_u32(base + SH_PAYLOAD_LEN)
        .ok_or(IpcError::Layout)? as usize;
    if len > geom.payload_cap as usize {
        return Err(IpcError::BadResponse);
    }
    copy_into(seg, geom.payload_off(slot), len, out).ok_or(IpcError::Layout)?;
    Ok(Some(status))
}

/// Client: release a slot back to FREE.
///
/// Only for a slot this client owns outright — CLAIMED, or COMPLETED with its
/// response (and arena bank) read. A slot whose request may still be with the
/// server goes through [`abandon`].
pub fn free_slot(seg: &SharedSeg, geom: &Geom, slot: u32) -> Result<(), IpcError> {
    state(seg, geom, slot)
        .ok_or(IpcError::Layout)?
        .store(ST_FREE, Ordering::Release);
    Ok(())
}

/// What [`abandon`] did with the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abandon {
    /// The slot is FREE again: no server had the request, or its response had
    /// already arrived and was discarded.
    Freed,
    /// A server is still processing the request. The slot is ABANDONED and
    /// stays out of use until that server finishes and frees it.
    Retired,
}

/// Client: stop waiting for the request in `slot`.
///
/// The one thing a client must not do is free a slot a server is processing:
/// the server would then write its late response — and, for a bulk read, its
/// arena bank — under whichever request claimed the slot next. So a slot in
/// PROCESSING is handed to the server ([`ST_ABANDONED`]) instead, and every
/// other state, which no server holds, is freed here.
pub fn abandon(seg: &SharedSeg, geom: &Geom, slot: u32) -> Result<Abandon, IpcError> {
    let st = state(seg, geom, slot).ok_or(IpcError::Layout)?;
    loop {
        let (from, to, outcome) = match st.load(Ordering::Acquire) {
            ST_PROCESSING => (ST_PROCESSING, ST_ABANDONED, Abandon::Retired),
            // Never taken, never published, or answered a moment ago.
            s @ (ST_SUBMITTED | ST_CLAIMED | ST_COMPLETED) => (s, ST_FREE, Abandon::Freed),
            ST_ABANDONED => return Ok(Abandon::Retired),
            _ => return Ok(Abandon::Freed),
        };
        // Compare-and-swap, not a store: a server may take or complete the
        // slot between the load and here, and then the right move differs.
        if st
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seg::OwnedSeg;

    fn ring(slots: u32, cap: u32) -> (OwnedSeg, Geom) {
        // Enough bytes: header + slots*(header+cap rounded).
        let stride = align8(SLOT_HEADER_SIZE + cap as usize);
        let owned = OwnedSeg::new(RING_HEADER_SIZE + slots as usize * stride + 16);
        let geom = init(owned.seg(), slots, cap).unwrap();
        (owned, geom)
    }

    #[test]
    fn init_then_open_roundtrips() {
        let (owned, geom) = ring(4, 64);
        let opened = open(owned.seg()).unwrap();
        assert_eq!(opened.slot_count, geom.slot_count);
        assert_eq!(opened.payload_cap, 64);
        assert_eq!(opened.slot_stride, geom.slot_stride);
    }

    /// A `payload_cap` within 32 bytes of `u32::MAX` has no stride a u32
    /// holds. Worked in 32 bits, `32 + payload_cap` wraps to a small number
    /// whose `align8` is a stride a header can state, so a ring of tiny
    /// slots would open claiming slots of 4 GiB. `docs/wire.md` 2.4 refuses
    /// it on every target.
    #[test]
    fn open_refuses_a_payload_cap_whose_stride_wraps() {
        let owned = OwnedSeg::new(4096);
        let seg = owned.seg();
        for cap in [u32::MAX, u32::MAX - 7, u32::MAX - 15, u32::MAX - 31] {
            // What `align8(32 + cap)` comes to in 32-bit arithmetic.
            let wrapped = (32u32.wrapping_add(cap)).wrapping_add(7) & !7;
            seg.write_u32(RH_MAGIC, MAGIC);
            seg.write_u32(RH_VERSION, VERSION);
            seg.write_u32(RH_SLOT_COUNT, 1);
            seg.write_u32(RH_SLOT_STRIDE, wrapped);
            seg.write_u32(RH_PAYLOAD_CAP, cap);
            assert_eq!(open(seg), Err(IpcError::Layout), "payload_cap {cap:#x}");
        }
        // The largest cap whose stride fits: a header may state it, and a
        // segment that small is refused for its size, not its arithmetic.
        let cap = u32::MAX - 39;
        seg.write_u32(RH_SLOT_STRIDE, u32::MAX - 7);
        seg.write_u32(RH_PAYLOAD_CAP, cap);
        assert_eq!(open(seg), Err(IpcError::Layout));
        assert_eq!(init(seg, 1, u32::MAX - 15), Err(IpcError::Layout));
    }

    #[test]
    fn open_rejects_bad_magic() {
        let (owned, _) = ring(2, 32);
        owned.seg().write_u32(RH_MAGIC, 0xDEAD);
        assert_eq!(open(owned.seg()), Err(IpcError::Layout));
    }

    /// A payload layout change (such as every path-carrying request leading
    /// with `root:u32`) is a version change, and an injected
    /// `aether_shim.dll` can go stale silently. A stale shim would send another
    /// shape into a current director, whose decoder would read the first four
    /// bytes of the path as a root id and the rest as a truncated path: a
    /// plausible-looking wrong answer, not a failure.
    ///
    /// The version field is the only thing standing between that and a loud
    /// error, so it gets its own test rather than riding on
    /// `open_rejects_bad_magic`: the magic is unchanged by a payload change,
    /// so magic alone would let a stale shim straight through. `VERSION - 1`
    /// is written here specifically because it is what a
    /// one-generation-stale shim actually stamps.
    #[test]
    fn open_rejects_a_stale_wire_version() {
        let (owned, _) = ring(2, 32);
        assert!(open(owned.seg()).is_ok(), "the current version must open");
        owned.seg().write_u32(RH_VERSION, VERSION - 1);
        assert_eq!(
            open(owned.seg()),
            Err(IpcError::Layout),
            "a ring stamped with the previous wire version must be refused, not misparsed"
        );
    }

    /// A header that claims more than the slot holds reaches the server as
    /// it was written, so a director's answer to it can be checked.
    #[test]
    fn publish_claiming_states_the_length_it_is_given() {
        let (owned, geom) = ring(2, 64);
        let seg = owned.seg();
        let slot = claim_free(seg, &geom).unwrap();
        let req_id = publish_claiming(seg, &geom, slot, Opcode::GetAttr as u32, 3, 65).unwrap();
        let taken = server_take(seg, &geom).unwrap();
        assert_eq!(taken, slot);
        let mut buf = Vec::new();
        let head = read_request_into(seg, &geom, taken, &mut buf).unwrap();
        assert_eq!(
            (head.opcode, head.flags, head.req_id),
            (Opcode::GetAttr as u32, 3, req_id)
        );
        assert!(!head.fits(&geom), "65 bytes do not fit a 64-byte slot");
        assert!(buf.is_empty());
    }

    #[test]
    fn full_primitive_roundtrip() {
        let (owned, geom) = ring(2, 64);
        let seg = owned.seg();

        let slot = claim_free(seg, &geom).unwrap();
        assert_eq!(slot, 0);
        let req_id =
            publish_request(seg, &geom, slot, Opcode::GetAttr as u32, 7, b"hello").unwrap();

        let taken = server_take(seg, &geom).unwrap();
        assert_eq!(taken, slot);
        let (opcode, flags, rid, payload) = read_request(seg, &geom, taken).unwrap();
        assert_eq!(opcode, Opcode::GetAttr as u32);
        assert_eq!(flags, 7);
        assert_eq!(rid, req_id);
        assert_eq!(payload, b"hello");

        assert_eq!(
            server_complete(seg, &geom, taken, rid, 42, b"world!").unwrap(),
            Completion::Delivered
        );

        let (status, resp) = take_response(seg, &geom, slot, req_id).unwrap().unwrap();
        assert_eq!(status, 42);
        assert_eq!(resp, b"world!");

        free_slot(seg, &geom, slot).unwrap();
        // Slot is FREE again → claimable.
        assert_eq!(claim_free(seg, &geom).unwrap(), 0);
    }

    #[test]
    fn the_registry_generation_is_zero_until_published_and_never_goes_back() {
        let owned = OwnedSeg::new(4096);
        init(owned.seg(), 4, 256).unwrap();
        assert_eq!(reg_generation(owned.seg()), 0);
        publish_reg_generation(owned.seg(), 7);
        assert_eq!(reg_generation(owned.seg()), 7);
        // A publisher that lost a race lands its older value late: ignored.
        publish_reg_generation(owned.seg(), 5);
        assert_eq!(reg_generation(owned.seg()), 7);
        publish_reg_generation(owned.seg(), 8);
        assert_eq!(reg_generation(owned.seg()), 8);
        // A re-initialised ring starts over.
        init(owned.seg(), 4, 256).unwrap();
        assert_eq!(reg_generation(owned.seg()), 0);
    }

    #[test]
    fn the_worker_hint_is_zero_until_the_server_says() {
        let (owned, _) = ring(2, 32);
        assert_eq!(worker_hint(owned.seg()), 0);
        set_worker_hint(owned.seg(), 16);
        assert_eq!(worker_hint(owned.seg()), 16);
        // It lives in the header, at offset 20: the ring still opens.
        assert!(open(owned.seg()).is_ok());
    }

    #[test]
    fn a_claim_starts_at_the_hint_and_wraps() {
        let (owned, geom) = ring(4, 32);
        let seg = owned.seg();
        assert_eq!(claim_free_from(seg, &geom, 2), Some(2));
        assert_eq!(claim_free_from(seg, &geom, 2), Some(3));
        assert_eq!(claim_free_from(seg, &geom, 2), Some(0));
        assert_eq!(claim_free_from(seg, &geom, 2), Some(1));
        assert_eq!(claim_free_from(seg, &geom, 2), None, "the ring is full");
        // A hint past the end is taken modulo the slot count, not out of range.
        free_slot(seg, &geom, 1).unwrap();
        assert_eq!(claim_free_from(seg, &geom, 9), Some(1));
    }

    #[test]
    fn a_server_scan_starting_past_a_slot_takes_the_later_one_first() {
        let (owned, geom) = ring(4, 32);
        let seg = owned.seg();
        for _ in 0..4 {
            let s = claim_free(seg, &geom).unwrap();
            publish_request(seg, &geom, s, Opcode::GetAttr as u32, 0, b"").unwrap();
        }
        assert_eq!(server_take_from(seg, &geom, 3), Some(3));
        assert_eq!(server_take_from(seg, &geom, 4), Some(0), "wraps");
        assert_eq!(server_take(seg, &geom), Some(1));
        assert_eq!(server_take_from(seg, &geom, 2), Some(2));
        assert_eq!(server_take(seg, &geom), None);
    }

    /// A request no server took: giving up frees the slot at once, and the
    /// server can no longer take it.
    #[test]
    fn abandoning_an_untaken_request_frees_the_slot() {
        let (owned, geom) = ring(2, 32);
        let seg = owned.seg();
        let slot = claim_free(seg, &geom).unwrap();
        publish_request(seg, &geom, slot, Opcode::GetAttr as u32, 0, b"x").unwrap();
        assert_eq!(abandon(seg, &geom, slot), Ok(Abandon::Freed));
        assert_eq!(slot_state(seg, &geom, slot), Some(ST_FREE));
        assert_eq!(server_take(seg, &geom), None);
    }

    /// **The late reply.** A client that gives up on a request the server is
    /// still processing must not make the slot claimable: the server will
    /// write its response there. The slot comes back only when the server has
    /// finished, and the response it wrote reaches nobody.
    #[test]
    fn an_abandoned_slot_is_not_reused_until_its_late_reply_is_drained() {
        let (owned, geom) = ring(2, 64);
        let seg = owned.seg();

        let slot = claim_free(seg, &geom).unwrap();
        assert_eq!(slot, 0);
        let old_id = publish_request(seg, &geom, slot, Opcode::Read as u32, 0, b"old").unwrap();
        assert_eq!(server_take(seg, &geom), Some(slot));

        // The client's deadline passes while the server is still at work.
        assert_eq!(abandon(seg, &geom, slot), Ok(Abandon::Retired));
        assert_eq!(slot_state(seg, &geom, slot), Some(ST_ABANDONED));
        // A second give-up on the same slot changes nothing.
        assert_eq!(abandon(seg, &geom, slot), Ok(Abandon::Retired));

        // The next request cannot have slot 0, only slot 1; then the ring is full.
        let next = claim_free(seg, &geom).unwrap();
        assert_eq!(next, 1, "the retired slot must not be claimable");
        assert_eq!(claim_free(seg, &geom), None);
        let new_id = publish_request(seg, &geom, next, Opcode::Read as u32, 0, b"new").unwrap();

        // The late reply arrives: dropped, and the slot freed by the server.
        assert_eq!(
            server_complete(seg, &geom, slot, old_id, 0, b"late reply"),
            Ok(Completion::Drained)
        );
        assert_eq!(slot_state(seg, &geom, slot), Some(ST_FREE));
        // The request that came after never sees it.
        assert_eq!(take_response(seg, &geom, next, new_id), Ok(None));

        // And the drained slot is an ordinary slot again.
        let again = claim_free(seg, &geom).unwrap();
        assert_eq!(again, slot);
        let id = publish_request(seg, &geom, again, Opcode::Read as u32, 0, b"fresh").unwrap();
        assert_eq!(take_response(seg, &geom, again, id), Ok(None));
        assert_eq!(server_take(seg, &geom), Some(again));
        assert_eq!(server_take(seg, &geom), Some(next));
        assert_eq!(
            server_complete(seg, &geom, again, id, 7, b"fresh reply"),
            Ok(Completion::Delivered)
        );
        assert_eq!(
            take_response(seg, &geom, again, id),
            Ok(Some((7, b"fresh reply".to_vec())))
        );
    }

    /// The give-up and the completion race; whichever lands second must see
    /// the first. Here the reply wins by a moment: the slot is simply freed.
    #[test]
    fn abandoning_a_request_answered_a_moment_ago_frees_the_slot() {
        let (owned, geom) = ring(1, 32);
        let seg = owned.seg();
        let slot = claim_free(seg, &geom).unwrap();
        let id = publish_request(seg, &geom, slot, Opcode::GetAttr as u32, 0, b"").unwrap();
        server_take(seg, &geom).unwrap();
        assert_eq!(
            server_complete(seg, &geom, slot, id, 0, b"just in time"),
            Ok(Completion::Delivered)
        );
        assert_eq!(abandon(seg, &geom, slot), Ok(Abandon::Freed));
        assert_eq!(claim_free(seg, &geom), Some(slot));
    }

    /// A client that does not follow the rule — it frees a slot the server
    /// holds, as every client did before version 3 — must still not get an
    /// old reply written under its next request. The request id, checked
    /// before anything is written, is what stops it.
    #[test]
    fn a_reply_for_a_recycled_slot_is_dropped_without_writing() {
        let (owned, geom) = ring(1, 64);
        let seg = owned.seg();
        let slot = claim_free(seg, &geom).unwrap();
        let old_id = publish_request(seg, &geom, slot, Opcode::Read as u32, 0, b"old").unwrap();
        server_take(seg, &geom).unwrap();

        // The rule-breaking free, then a new request in the same slot, taken
        // by another server thread.
        free_slot(seg, &geom, slot).unwrap();
        assert_eq!(claim_free(seg, &geom), Some(slot));
        let new_id =
            publish_request(seg, &geom, slot, Opcode::Read as u32, 0, b"new request").unwrap();
        assert_ne!(new_id, old_id);
        assert_eq!(server_take(seg, &geom), Some(slot));

        assert_eq!(
            server_complete(seg, &geom, slot, old_id, 0, b"late"),
            Ok(Completion::Stale)
        );
        // Untouched: still processing, the new request's payload intact.
        assert_eq!(slot_state(seg, &geom, slot), Some(ST_PROCESSING));
        let (_, _, rid, payload) = read_request(seg, &geom, slot).unwrap();
        assert_eq!((rid, payload.as_slice()), (new_id, &b"new request"[..]));
        assert_eq!(take_response(seg, &geom, slot, new_id), Ok(None));

        assert_eq!(
            server_complete(seg, &geom, slot, new_id, 0, b"right"),
            Ok(Completion::Delivered)
        );
        assert_eq!(
            take_response(seg, &geom, slot, new_id),
            Ok(Some((0, b"right".to_vec())))
        );
    }

    /// The client's own check: a completed slot that answers some other
    /// request is an error, never this request's reply.
    #[test]
    fn a_completed_slot_acknowledging_another_request_is_refused() {
        let (owned, geom) = ring(1, 64);
        let seg = owned.seg();
        let slot = claim_free(seg, &geom).unwrap();
        let id = publish_request(seg, &geom, slot, Opcode::Read as u32, 0, b"q").unwrap();
        server_take(seg, &geom).unwrap();
        server_complete(seg, &geom, slot, id, 0, b"a").unwrap();
        assert_eq!(
            take_response(seg, &geom, slot, id + 1),
            Err(IpcError::BadResponse)
        );
        assert_eq!(
            take_response(seg, &geom, slot, id),
            Ok(Some((0, b"a".to_vec())))
        );
    }

    #[test]
    fn payload_too_large_is_rejected() {
        let (owned, geom) = ring(1, 8);
        let seg = owned.seg();
        let slot = claim_free(seg, &geom).unwrap();
        assert_eq!(
            publish_request(
                seg,
                &geom,
                slot,
                Opcode::Read as u32,
                0,
                b"way too long payload"
            ),
            Err(IpcError::PayloadTooLarge)
        );
    }
}
