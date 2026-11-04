//! The calling end: a typed request in, a typed reply out.

use aether_ipc::{IpcError, Notifier, RingClient};
use aether_protocol::{Reply, Request, SliceBuf, Status};

/// Why [`Client::call`] has no reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The director answered with this status. Its payload, if any, was not
    /// looked at.
    Status(Status),
    /// The director answered with a status the table does not have (the
    /// ring's own overflow sentinel, `i32::MIN`, among them).
    UnknownStatus(i32),
    /// The director answered `Status::Ok` with a payload that is not a reply
    /// to this opcode. The shim treats it as `Status::BadRequest`
    /// (`docs/wire.md` 3.1).
    Undecodable,
    /// The ring gave no answer.
    Ipc(IpcError),
}

impl core::fmt::Display for CallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CallError::Status(s) => write!(f, "director answered {}", s.name()),
            CallError::UnknownStatus(n) => write!(f, "director answered unknown status {n}"),
            CallError::Undecodable => f.write_str("director's OK reply does not decode"),
            CallError::Ipc(e) => write!(f, "ring error: {e:?}"),
        }
    }
}

impl std::error::Error for CallError {}

/// A [`RingClient`] that speaks [`Request`] and [`Reply`].
pub struct Client<'a, N: Notifier> {
    ring: RingClient<'a, N>,
}

impl<'a, N: Notifier> Client<'a, N> {
    pub fn new(ring: RingClient<'a, N>) -> Self {
        Client { ring }
    }

    /// The ring underneath, for what the typed calls do not cover (held
    /// slots for bulk reads, raw submits).
    pub fn ring(&self) -> &RingClient<'a, N> {
        &self.ring
    }

    pub fn into_ring(self) -> RingClient<'a, N> {
        self.ring
    }

    /// Sends `req` and waits for its reply, which borrows from `scratch`.
    ///
    /// The request is encoded straight into the slot and the reply payload is
    /// copied into `scratch`, so a call allocates only when `scratch` has to
    /// grow. A non-OK status is returned without decoding the payload.
    ///
    /// The slot is freed before this returns, so a bulk READ reply's arena
    /// bank may already belong to another request: bulk reads go through the
    /// ring's held-slot calls instead.
    pub fn call<'r>(
        &self,
        req: &Request<'_>,
        scratch: &'r mut Vec<u8>,
    ) -> Result<Reply<'r>, CallError> {
        let op = req.opcode();
        let flags = req.flags();
        let status = self
            .ring
            .submit_in_place(
                op as u32,
                flags,
                |slot| {
                    let mut out = SliceBuf::new(slot);
                    req.encode(&mut out);
                    out.finish().ok()
                },
                scratch,
            )
            .map_err(CallError::Ipc)?;
        if status != Status::Ok.code() {
            return Err(match Status::try_from(status) {
                Ok(s) => CallError::Status(s),
                Err(_) => CallError::UnknownStatus(status),
            });
        }
        Reply::decode(op, flags, scratch).map_err(|_| CallError::Undecodable)
    }
}
