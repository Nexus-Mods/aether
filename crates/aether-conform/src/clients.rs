//! The ring's other clients: requests sent on a slot the caller picks, and
//! client processes of their own.
//!
//! wire.md 2.8.1 ("One ring per session") makes promises about every
//! process of a launch sharing the director's ring: handles are ring-global
//! and any process may present any handle; a killed process never sends
//! CLOSE; `reg_gen` is how one process's registry write reaches the others.
//! A run with one client, sending one request at a time from one thread,
//! tests none of it, and since a thread's claim starts at the slot it last
//! held (2.5: "A client scans from a per-thread hint"), it only ever uses
//! slot 0 and bank 0. So:
//!
//! - [`call_on`] sends a request the way the ring client does, but claims
//!   from the slot the caller names, so a run can put requests in every slot
//!   and read bulk replies from every bank. It checks what only a client on
//!   a slot other than 0 can see: a bulk reply's data lies in the bank of
//!   the slot it answers (2.6: "Slot `s` owns bank `s % banks`"; 2.5: the
//!   bank belongs to the slot's owner), and a completed slot acknowledges the
//!   request it holds (2.5, "The client's read").
//! - [`ProcessClient`] is a client in a process of its own: `aether-conform
//!   client-worker` ([`client_worker`]), started with the mount's handshake
//!   variables in its environment, attaches to the ring from them as a shim
//!   would ([`crate::attach`]), sends its HEARTBEAT, and then sends the
//!   requests the run gives it on stdin, one JSON line each, answering each
//!   on stdout. Client `k` claims from slot `k` (mod the slot count), so two
//!   clients of a scenario use two slots.
//!
//! The worker's protocol, one object per line:
//!
//! ```text
//! (on start)  -> {"ok":true,"ring":"file /tmp/x.ring"} or {"ok":false,"error":"..."}
//! {"request":{"op":"getattr","root":0,"path":"a"}}
//!     -> {"answer":{"reply":{...}},"slot":1,"gen_before":3,"gen_after":3}
//!     or {"bad":"why no expectation can match it","slot":1,...}
//!     or {"error":"the ring gave no answer: ..."}
//! ```
//!
//! Requests and answers are in the scenario format
//! ([`crate::scenario::request_json`], [`crate::scenario::answer_json`]); a
//! bulk reply is read from the worker's own mapping of the arena before it
//! answers. `gen_before` and `gen_after` are the registry generation the
//! worker read in the ring header just before the request and just after the
//! reply, from its own mapping (wire.md 4.5).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use aether_ipc::{ring, Notifier};
use aether_protocol::{Opcode, Reply, RequestFlags, Status};
use serde_json::{json, Value};

use crate::adapter::{Adapter, Env};
use crate::attach::Attached;
use crate::scenario::{answer_json, parse_answer, parse_request_value, OwnedReply, OwnedRequest};

/// What a request got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// An answer to compare: a reply, or an error status.
    Got(Result<OwnedReply, Status>),
    /// An answer no expectation can match: an unknown status, an OK payload
    /// that does not decode, a bulk reply outside its slot's bank, a reply
    /// that acknowledges another request.
    Bad(String),
}

/// A request's answer and where it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub answer: Answer,
    /// The slot the request was published in.
    pub slot: u32,
    /// For a bulk READ reply read from the arena, the bank it was read
    /// from.
    pub bank: Option<u32>,
}

/// After this long without a reply, a wait sleeps instead of yielding (the
/// shim's `IDLE_YIELD`, as [`crate::attach::Wake`] has it).
const SPIN: Duration = Duration::from_millis(1);

/// Sends `req` on `ring`, claiming the first free slot from `start` on
/// (wrapping), as a client thread whose hint is `start` does (wire.md 2.5).
/// Waits for the answer up to the ring's deadline; a bulk READ reply is read
/// from the arena before the slot is freed (2.6). `Err` when no slot came
/// free or no answer came in time: the slot is then given up as wire.md 2.5
/// says (`abandon`), never freed under the director.
///
/// A READ asking for `read_bulk` on a ring with no arena must not be sent:
/// the caller clears the flag, as the shim never sets it then (2.6).
pub fn call_on(ring: &Attached, start: u32, req: &OwnedRequest) -> Result<Outcome, String> {
    let mut out = call_batch(ring, &[(start, req)])?;
    Ok(out.pop().expect("one outcome per request"))
}

/// [`call_on`] for several requests in flight at once, each claiming from
/// its own start (so a batch can put a request in every slot), all
/// published before any answer is awaited, as a pipelined read publishes its
/// requests (wire.md 2.5, "Order of service"). The outcomes are in the
/// order of `reqs`; one deadline covers the batch. `Err` when a slot did not
/// come free or an answer did not come: every slot the batch holds is then
/// given up.
pub fn call_batch(ring: &Attached, reqs: &[(u32, &OwnedRequest)]) -> Result<Vec<Outcome>, String> {
    let seg = ring.seg();
    let geom = ring.geom();
    let wake = ring.wake();
    let deadline = ring.deadline();
    let began = Instant::now();
    // (slot, req_id, opcode, flags) of each request published so far.
    let mut held: Vec<(u32, u64, Opcode, u32)> = Vec::with_capacity(reqs.len());
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
        let (op, flags) = (request.opcode(), request.flags());
        let slot = loop {
            if let Some(s) = ring::claim_free_from(seg, &geom, start % geom.slot_count.max(1)) {
                break s;
            }
            let waited = began.elapsed();
            if waited > deadline {
                give_up(&held);
                return Err(format!(
                    "no free slot within {} s (wire.md 2.5)",
                    deadline.as_secs_f64()
                ));
            }
            idle(&wake, start, waited);
        };
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
    let mut out = Vec::with_capacity(held.len());
    for (n, &(slot, req_id, op, flags)) in held.iter().enumerate() {
        let taken = loop {
            match ring::take_response(seg, &geom, slot, req_id) {
                Ok(Some(r)) => break Ok(r),
                Ok(None) => {}
                // COMPLETED, but not with this request's reply.
                Err(aether_ipc::IpcError::BadResponse) => break Err(()),
                Err(e) => {
                    give_up(&held[n..]);
                    return Err(format!("slot {slot}: {e:?}"));
                }
            }
            let waited = published.elapsed();
            if waited > deadline {
                give_up(&held[n..]);
                return Err(format!(
                    "no answer within {} s to the request in slot {slot} (wire.md 2.5: every \
                     slot a client publishes is taken and completed, whichever slot it is)",
                    deadline.as_secs_f64()
                ));
            }
            idle(&wake, slot, waited);
        };
        let (answer, bank) = match taken {
            Ok((status, payload)) => decode(ring, slot, op, flags, status, &payload),
            Err(()) => (
                Answer::Bad(format!(
                    "slot {slot} was completed with an ack that is not this request's req_id, \
                     or a payload_len past payload_cap (wire.md 2.5: the director stores ack = \
                     low32(req_id) of the request it answers, and a reply fits the slot)"
                )),
                None,
            ),
        };
        // Copied out, bank included: the slot is the client's to free.
        let _ = ring::free_slot(seg, &geom, slot);
        wake.notify_slot_free();
        out.push(Outcome { answer, slot, bank });
    }
    Ok(out)
}

/// Spins, then yields, then sleeps, as the shim's client waits.
fn idle(wake: &crate::attach::Wake<'_>, slot: u32, waited: Duration) {
    if waited < SPIN {
        core::hint::spin_loop();
    } else {
        wake.idle_client(slot, waited);
    }
}

/// The answer in a completed slot: its status, or its decoded reply, a bulk
/// one read from the arena once it is found in the slot's own bank.
pub(crate) fn decode(
    ring: &Attached,
    slot: u32,
    op: Opcode,
    flags: u32,
    status: i32,
    payload: &[u8],
) -> (Answer, Option<u32>) {
    if status != Status::Ok.code() {
        return match Status::try_from(status) {
            Ok(s) if !payload.is_empty() => (
                Answer::Bad(format!(
                    "{} with a {}-byte payload (wire.md 3.1: \"A director answers every \
                     error with an empty payload\")",
                    s.name(),
                    payload.len()
                )),
                None,
            ),
            Ok(s) => (Answer::Got(Err(s)), None),
            Err(_) => (
                Answer::Bad(format!(
                    "status {status}, which the protocol table does not have"
                )),
                None,
            ),
        };
    }
    if op == Opcode::Open {
        if let Some(why) = crate::compare::open_reply_reserved(payload) {
            return (Answer::Bad(why), None);
        }
    }
    let Ok(reply) = Reply::decode(op, flags, payload) else {
        return (
            Answer::Bad(format!(
                "an OK reply whose payload does not decode as a {} reply (wire.md 3.1)",
                op.name()
            )),
            None,
        );
    };
    match OwnedReply::from_reply(&reply) {
        // No data, so no bank to be in.
        OwnedReply::ReadBulk { len: 0, .. } => {
            (Answer::Got(Ok(OwnedReply::Read(Vec::new()))), None)
        }
        OwnedReply::ReadBulk { len, arena_offset } => {
            let banks = ring.geom().slot_count.max(1);
            let own = slot % banks;
            let end = arena_offset.saturating_add(u64::from(len));
            // Without VFS_ARENA_OFFSET the banks' places are only assumed, so
            // the data need only lie in the mapping (`Attached::arena_offset_given`).
            let given = ring.arena_offset_given();
            let placed = if given {
                ring.bank_of(arena_offset, len)
            } else {
                Some(own)
            };
            match placed {
                Some(b) if b == own && end <= ring.ring_bytes() as u64 => {
                    match ring.seg().read_bytes(arena_offset as usize, len as usize) {
                        Some(data) => (Answer::Got(Ok(OwnedReply::Read(data))), given.then_some(b)),
                        None => (
                            Answer::Bad(format!(
                                "a bulk reply of {len} bytes at {arena_offset}, outside the \
                                 mapping (wire.md 2.6)"
                            )),
                            None,
                        ),
                    }
                }
                other => (
                    Answer::Bad(format!(
                        "a bulk reply in slot {slot} of {len} bytes at mapping offset \
                         {arena_offset}, which is {} and not in bank {own} (the arena is {} \
                         bytes at {}; wire.md 2.6: \"Slot `s` owns bank `s % banks`\", and 2.5: \
                         the bank belongs to the slot's owner, so data in another bank is \
                         another request's)",
                        match other {
                            Some(b) => format!("in bank {b}"),
                            None => "in no bank of the arena".into(),
                        },
                        ring.arena_len(),
                        ring.arena_offset(),
                    )),
                    None,
                ),
            }
        }
        r => (Answer::Got(Ok(r)), None),
    }
}

/// Whether `req` is a data request, which the shim keeps below its data
/// limit (wire.md 2.8: "READ, WRITE, SETATTR and write OPENs").
pub fn is_data(req: &OwnedRequest) -> bool {
    match req {
        OwnedRequest::Read { .. } | OwnedRequest::Write { .. } | OwnedRequest::SetAttr { .. } => {
            true
        }
        OwnedRequest::Open { flags, .. } => flags.contains(aether_protocol::OpenFlags::WRITE),
        _ => false,
    }
}

/// `req` as it may be sent on `ring`: a READ's `read_bulk` cleared when the
/// ring has no arena, as the shim never sets it then (wire.md 2.6). Whether
/// it was cleared.
pub fn fit_to_ring(ring: &Attached, req: &mut OwnedRequest) -> bool {
    if let OwnedRequest::Read { slot_flags, .. } = req {
        if ring.arena_len() == 0 && slot_flags.contains(RequestFlags::READ_BULK) {
            slot_flags.remove(RequestFlags::READ_BULK);
            return true;
        }
    }
    false
}

/// What a client process answered for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub outcome: Outcome,
    /// The registry generation the client read before the request and
    /// after the reply, in its own mapping.
    pub gen_before: u64,
    pub gen_after: u64,
}

/// A client in a process of its own: `aether-conform client-worker`.
pub struct ProcessClient {
    child: Adapter,
    client: u32,
    ring: String,
}

impl ProcessClient {
    /// Starts client `client` from the binary `exe` (the `aether-conform`
    /// binary) with `env`, a mount's handshake variables, as its environment
    /// (any `VFS_*` variable of this process's own environment removed
    /// first), and waits until it has attached and sent its HEARTBEAT.
    /// `timeout` bounds every request and every answer.
    pub fn start(
        exe: &Path,
        client: u32,
        env: &Env,
        timeout: Duration,
    ) -> Result<ProcessClient, String> {
        let mut cmd = Command::new(exe);
        cmd.args([
            "client-worker",
            "--client",
            &client.to_string(),
            "--timeout",
            &timeout.as_secs_f64().to_string(),
        ]);
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("VFS_") {
                cmd.env_remove(k);
            }
        }
        cmd.envs(env);
        // Its answers take up to the ring's deadline; the line a little
        // longer.
        let mut child = Adapter::spawn(cmd, "client-worker", timeout + Duration::from_secs(5))
            .map_err(|e| format!("client {client}: {e}"))?;
        let ready = child
            .request(&json!({"op": "hello"}))
            .map_err(|e| format!("client {client}: {e}"))?;
        match ready.get("ok") {
            Some(Value::Bool(true)) => Ok(ProcessClient {
                child,
                client,
                ring: ready["ring"].as_str().unwrap_or("").to_string(),
            }),
            _ => Err(format!(
                "client {client}: {}",
                ready["error"].as_str().unwrap_or("did not attach")
            )),
        }
    }

    /// The ring the client attached to, as it describes it.
    pub fn ring(&self) -> &str {
        &self.ring
    }

    /// Sends `req` (with the director's handle numbers) from this client.
    /// `Err` when the ring gave no answer or the process failed.
    pub fn send(&mut self, req: &OwnedRequest) -> Result<Remote, String> {
        let k = self.client;
        let line = self
            .child
            .request(&json!({ "request": crate::scenario::request_json(req) }))
            .map_err(|e| format!("client {k}: {e}"))?;
        if let Some(e) = line.get("error") {
            return Err(format!("client {k}: {}", e.as_str().unwrap_or("")));
        }
        // A line without these is the worker's fault, never a slot 0 or a
        // generation 0 (which would read as one that went backward).
        let number = |field: &str| {
            line[field].as_u64().ok_or_else(|| {
                format!("client {k}: an answer line without a number \"{field}\": {line}")
            })
        };
        let slot = u32::try_from(number("slot")?)
            .map_err(|_| format!("client {k}: a slot past u32 in {line}"))?;
        let (gen_before, gen_after) = (number("gen_before")?, number("gen_after")?);
        let bank = line["bank"].as_u64().map(|b| b as u32);
        let answer = if let Some(bad) = line.get("bad") {
            Answer::Bad(bad.as_str().unwrap_or("").to_string())
        } else {
            Answer::Got(
                parse_answer(req.opcode(), &line["answer"])
                    .map_err(|e| format!("client {k}: an answer that does not parse: {e}"))?,
            )
        };
        Ok(Remote {
            outcome: Outcome { answer, slot, bank },
            gen_before,
            gen_after,
        })
    }

    /// Kills the process now, whatever it holds: open handles it never
    /// closes, as a killed game process leaves them (wire.md 2.8.1).
    pub fn kill(&mut self) -> Option<String> {
        self.child.kill_now()
    }
}

/// `aether-conform client-worker --client K [--timeout SECS]`: one client
/// of the ring its own environment names, claiming from slot `K`. The exit
/// code.
pub fn client_worker(client: u32, timeout: Duration) -> i32 {
    use std::io::{BufRead, Write};
    let env: Env = std::env::vars().collect();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut say = |v: Value| {
        writeln!(stdout, "{v}")
            .and_then(|()| stdout.flush())
            .is_ok()
    };
    let mut lines = stdin.lock().lines();
    // The first line is the run's hello; the answer says whether it attached.
    if !matches!(lines.next(), Some(Ok(_))) {
        return 0;
    }
    let ring = match Attached::from_env(&env, timeout) {
        Ok(r) => r,
        Err(e) => {
            say(json!({"ok": false, "error": format!("attach failed: {e}")}));
            return 0;
        }
    };
    match call_on(&ring, client, &OwnedRequest::Heartbeat) {
        Ok(Outcome {
            answer: Answer::Got(Ok(OwnedReply::Empty)),
            ..
        }) => {}
        Ok(o) => {
            say(
                json!({"ok": false, "error": format!("the HEARTBEAT sent on attach got {:?}", o.answer)}),
            );
            return 0;
        }
        Err(e) => {
            say(json!({"ok": false, "error": format!("the HEARTBEAT sent on attach failed: {e}")}));
            return 0;
        }
    }
    if !say(json!({"ok": true, "ring": ring.describe()})) {
        return 0;
    }
    for line in lines {
        let Ok(line) = line else { return 0 };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line)
            .map_err(|e| e.to_string())
            .and_then(|v| parse_request_value(&v["request"]))
        {
            Err(e) => json!({"error": format!("not a request: {e}")}),
            Ok(req) => {
                let gen_before = ring.reg_gen();
                match call_on(&ring, client, &req) {
                    Err(e) => json!({"error": e}),
                    Ok(o) => {
                        let gen_after = ring.reg_gen();
                        let mut v = json!({
                            "slot": o.slot,
                            "gen_before": gen_before,
                            "gen_after": gen_after,
                        });
                        if let Some(b) = o.bank {
                            v["bank"] = json!(b);
                        }
                        match &o.answer {
                            Answer::Got(a) => v["answer"] = answer_json(a),
                            Answer::Bad(why) => v["bad"] = json!(why),
                        }
                        v
                    }
                }
            }
        };
        if !say(answer) {
            return 0;
        }
    }
    0
}
