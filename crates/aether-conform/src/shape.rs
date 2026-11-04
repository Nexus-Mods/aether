//! What an OK reply's payload must look like beyond what its decoder reads:
//! the file-op decoders ignore trailing bytes (`docs/wire.md` 3.2, "Lenient
//! vs strict"), and the registry decoder reads REG_KEY's children into a
//! map, so a reply can decode and still not be the one wire.md describes.
//!
//! - **Fixed sizes** (wire.md 3.4.1: GETATTR "Reply, 18 bytes"; 3.4.3: OPEN
//!   "Reply, 24 bytes"; 3.4.5: WRITE "Reply, 8 bytes: `written:u32 |
//!   zero:u32`"; 3.4.4: an inline READ is "`8 + n` bytes", a bulk one "16
//!   bytes", each with a zero word after the first). The opcodes whose reply
//!   is "status only" or "empty" (SETATTR, RENAME, DELETE, MKDIR, CLOSE,
//!   HEARTBEAT, RENAME_X) have an empty payload. A READDIR reply is its
//!   `count` and that many entries, nothing after them (3.4.2).
//! - **An OPEN reply's reserved bits** ([`open_reply_reserved`], 3.4.3).
//! - **REG_KEY's children** (4.3: "sorted by `folded` in byte order";
//!   "send them sorted and unique"): each child's `folded` name above the
//!   one before it.

use aether_protocol::Opcode;

use crate::compare::open_reply_reserved;

/// Why `payload`, an OK reply to an `op` request, is not shaped as wire.md
/// says, if it is not. Only what its decoder does not check is looked at:
/// a payload too short for its fields is the decoder's to refuse.
pub fn reply_shape(op: Opcode, payload: &[u8]) -> Option<String> {
    let n = payload.len();
    let exact = |want: usize, cite: &str| {
        (n != want).then(|| {
            format!(
                "a {} reply of {n} bytes (wire.md {cite}: {want} bytes)",
                op.name()
            )
        })
    };
    match op {
        Opcode::GetAttr => exact(18, "3.4.1"),
        Opcode::Open => exact(24, "3.4.3").or_else(|| open_reply_reserved(payload)),
        Opcode::Write => exact(8, "3.4.5").or_else(|| zero_word(op, payload, "3.4.5")),
        Opcode::Read => {
            let first = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
            let want = if first & 0x8000_0000 != 0 {
                16
            } else {
                8 + first as usize
            };
            exact(want, "3.4.4").or_else(|| zero_word(op, payload, "3.4.4"))
        }
        Opcode::ReadDir => readdir_end(payload).and_then(|end| {
            (end != n).then(|| {
                format!(
                    "a READDIR reply of {n} bytes whose count and entries end at {end} (wire.md \
                     3.4.2: `count` and the entries, packed)"
                )
            })
        }),
        Opcode::SetAttr
        | Opcode::Rename
        | Opcode::Delete
        | Opcode::Mkdir
        | Opcode::Close
        | Opcode::Heartbeat
        | Opcode::RenameX => (n != 0).then(|| {
            format!(
                "a {} reply with a {n}-byte payload (wire.md 3.4: its reply is the status only)",
                op.name()
            )
        }),
        Opcode::RegKey => key_children_order(payload),
        _ => None,
    }
}

/// Bytes 4 to 8 of a WRITE or READ reply, which wire.md calls zero.
fn zero_word(op: Opcode, payload: &[u8], cite: &str) -> Option<String> {
    let w = payload.get(4..8)?;
    (w != [0, 0, 0, 0]).then(|| {
        format!(
            "a {} reply whose bytes 4 to 8 are {:02x}{:02x}{:02x}{:02x} (wire.md {cite}: zero)",
            op.name(),
            w[0],
            w[1],
            w[2],
            w[3]
        )
    })
}

/// A cursor over a reply payload.
struct Cur<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// Where a READDIR reply's last entry ends (wire.md 3.4.2: `count`, then
/// per entry `name_len`, the name, `is_dir`, `size`, `mtime`); `None` when
/// the payload is too short for them, which the decoder refuses.
fn readdir_end(payload: &[u8]) -> Option<usize> {
    let mut c = Cur { b: payload, at: 0 };
    for _ in 0..c.u32()? {
        c.str()?;
        c.take(1 + 8 + 8)?;
    }
    Some(c.at)
}

/// Where a REG_KEY reply's children begin and how many there are, if it
/// has a node (wire.md 4.3: `version`, `has_node`, then the node's
/// `created`, `volatile`, `last_write`, values, value tombstones and
/// children).
pub(crate) fn key_children(payload: &[u8]) -> Option<(usize, u32)> {
    let mut c = Cur { b: payload, at: 0 };
    c.take(8)?;
    if c.take(1)? != [1] {
        return None;
    }
    c.take(1 + 1 + 8)?;
    for _ in 0..c.u32()? {
        c.str()?;
        c.take(4)?;
        c.str()?;
    }
    for _ in 0..c.u32()? {
        c.str()?;
    }
    let n = c.u32()?;
    Some((c.at, n))
}

/// Why a REG_KEY reply's children are not sorted and unique by `folded`.
fn key_children_order(payload: &[u8]) -> Option<String> {
    let (at, n) = key_children(payload)?;
    let mut c = Cur { b: payload, at };
    let mut before: Option<&[u8]> = None;
    for i in 0..n {
        let folded = c.str()?;
        c.str()?;
        c.take(1)?;
        if let Some(b) = before {
            if folded <= b {
                return Some(format!(
                    "REG_KEY child {i}, {:?}, comes after {:?} (wire.md 4.3: children are \
                     \"sorted by `folded` in byte order\", and sent \"sorted and unique\")",
                    String::from_utf8_lossy(folded),
                    String::from_utf8_lossy(b)
                ));
            }
        }
        before = Some(folded);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::{AttrResp, KeyReply, Reply};
    use aether_registry::{Child, Node};

    fn encoded(r: &Reply<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        r.encode(&mut out);
        out
    }

    #[test]
    fn the_fixed_size_replies_must_be_their_size() {
        let attr = encoded(&Reply::GetAttr(AttrResp {
            found: true,
            is_dir: false,
            size: 3,
            mtime: 0,
        }));
        assert_eq!(reply_shape(Opcode::GetAttr, &attr), None);
        let mut long = attr.clone();
        long.push(0);
        assert!(reply_shape(Opcode::GetAttr, &long)
            .unwrap()
            .contains("wire.md 3.4.1"));
        let write = encoded(&Reply::Write(2));
        assert_eq!(reply_shape(Opcode::Write, &write), None);
        let mut dirty = write.clone();
        dirty[5] = 1;
        assert!(reply_shape(Opcode::Write, &dirty).unwrap().contains("zero"));
        assert!(reply_shape(Opcode::Close, &[0]).is_some());
        assert_eq!(reply_shape(Opcode::Close, &[]), None);
    }

    #[test]
    fn a_read_reply_is_its_header_and_its_bytes() {
        let mut inline = vec![3, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c'];
        assert_eq!(reply_shape(Opcode::Read, &inline), None);
        inline.push(b'd');
        assert!(reply_shape(Opcode::Read, &inline).is_some());
        let mut bulk = vec![0u8; 16];
        bulk[..4].copy_from_slice(&(5u32 | 0x8000_0000).to_le_bytes());
        assert_eq!(reply_shape(Opcode::Read, &bulk), None);
        bulk.push(0);
        assert!(reply_shape(Opcode::Read, &bulk).is_some());
    }

    #[test]
    fn a_listing_ends_with_its_last_entry() {
        let mut p = 1u32.to_le_bytes().to_vec();
        p.extend_from_slice(&1u32.to_le_bytes());
        p.push(b'a');
        p.extend_from_slice(&[0; 17]);
        assert_eq!(reply_shape(Opcode::ReadDir, &p), None);
        p.push(0);
        assert!(reply_shape(Opcode::ReadDir, &p)
            .unwrap()
            .contains("wire.md 3.4.2"));
    }

    #[test]
    fn key_children_out_of_order_or_twice_are_named() {
        let mut node = Node::default();
        for n in ["a", "b"] {
            node.children
                .insert(n.into(), (n.to_uppercase(), Child::Present));
        }
        let r = encoded(&Reply::RegKey(KeyReply {
            version: 1,
            node: Some(std::borrow::Cow::Owned(node)),
        }));
        assert_eq!(reply_shape(Opcode::RegKey, &r), None);
        let (at, n) = key_children(&r).unwrap();
        assert_eq!(n, 2);
        // The two children swapped: each is 4 + 1 + 4 + 1 + 1 bytes.
        let mut swapped = r[..at].to_vec();
        swapped.extend_from_slice(&r[at + 11..]);
        swapped.extend_from_slice(&r[at..at + 11]);
        assert!(reply_shape(Opcode::RegKey, &swapped)
            .unwrap()
            .contains("wire.md 4.3"));
        let mut twice = r[..at + 11].to_vec();
        twice.extend_from_slice(&r[at..at + 11]);
        assert!(reply_shape(Opcode::RegKey, &twice).is_some());
    }
}
