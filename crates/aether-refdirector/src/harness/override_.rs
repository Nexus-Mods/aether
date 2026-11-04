//! [`Override`]: OPEN replies with fields a test chose, for the answers
//! another director may give.

use super::lock;
use crate::key;
use aether_casefold::fold;
use aether_ipc::RequestView;
use aether_protocol::{Opcode, Reply, Request, Status};
use aether_rpc::Tap;
use std::sync::{Arc, Mutex};

/// The OPEN reply fields an [`Override`] sets; `None` leaves the director's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenEdit {
    pub immutable: Option<bool>,
    pub mount_gen: Option<u32>,
    pub size: Option<u64>,
}

/// A [`Tap`] that rewrites the OPEN replies under a path prefix: decoded,
/// edited and encoded again through the typed codec, so the reply is one a
/// director could send (its reserved bits zero, no trailing bytes). A refused
/// OPEN, or a reply that does not decode, is left alone. A clone is another
/// handle on the same rules.
///
/// Prefixes are compared folded and spelled as [`key`] spells a table key
/// (bare for root 0, `@<root>/` before it for another root). Every rule
/// whose prefix matches applies, in the order added, so a later one wins
/// where both set a field.
#[derive(Clone, Default)]
pub struct Override {
    rules: Arc<Mutex<Vec<(String, OpenEdit)>>>,
}

impl Override {
    pub fn new() -> Override {
        Override::default()
    }

    /// Applies `f` to every OPEN reply for a path under `prefix` from the
    /// next request on.
    pub fn open(&self, prefix: &str, f: OpenEdit) {
        lock(&self.rules).push((fold(prefix), f));
    }

    /// Drops every rule.
    pub fn clear(&self) {
        lock(&self.rules).clear();
    }
}

impl Tap for Override {
    fn on_reply(&mut self, view: &RequestView<'_>, status: &mut i32, payload: &mut Vec<u8>) {
        if view.opcode != Opcode::Open as u32 || *status != Status::Ok.code() {
            return;
        }
        let rules = lock(&self.rules);
        if rules.is_empty() {
            return;
        }
        let Ok(Request::Open { root, path, .. }) =
            Request::decode(Opcode::Open, view.flags, view.payload)
        else {
            return;
        };
        let Ok(Reply::Open(mut o)) = Reply::decode(Opcode::Open, view.flags, payload) else {
            return;
        };
        let k = key(root.0, path);
        let mut hit = false;
        for (_, e) in rules.iter().filter(|(p, _)| k.starts_with(p.as_str())) {
            hit = true;
            o.immutable = e.immutable.unwrap_or(o.immutable);
            o.mount_gen = e.mount_gen.unwrap_or(o.mount_gen);
            o.size = e.size.unwrap_or(o.size);
        }
        if hit {
            payload.clear();
            Reply::Open(o).encode(payload);
        }
    }
}
