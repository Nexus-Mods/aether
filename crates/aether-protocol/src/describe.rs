//! The protocol table as data. Every codec in `field.rs` describes the bytes
//! it puts on the wire as [`Entry`]s, and the `protocol!` macro in `table.rs`
//! strings those together per message, so [`contract_json`] is made from the
//! same definitions the encoders and decoders run on, never from a second
//! list. Padding a codec folds into a neighbouring field (OPEN's reply has two
//! bytes before `mount_gen`) still appears, as an entry named `"pad"`, so a
//! reader can count offsets from the file alone.
//!
//! A field is `{"name", "type"}` plus attributes for its type. [`TYPES`] and
//! [`ATTRIBUTES`] say what each one means on the wire; the JSON carries both,
//! so the published file is self-contained.

use crate::status::{OpenFlags, RequestFlags, Status};
use crate::table::Opcode;

/// The field types and what each is on the wire, in the order the JSON lists
/// them. A test checks that every type a message uses is here and that every
/// one here is used.
pub(crate) const TYPES: &[(&str, &str)] = &[
    ("u32", "little-endian unsigned 32-bit integer"),
    ("u64", "little-endian unsigned 64-bit integer"),
    ("i64", "little-endian signed 64-bit integer"),
    ("root_id", "u32, a root's id"),
    ("open_flags", "u32 of the open_flags masks"),
    ("bool", "one byte, exactly 0 or 1 (a decoder refuses others)"),
    ("bool_lenient", "one byte, written 0 or 1, any non-zero reads as true"),
    ("bool_bit0", "one byte, bit 0 is the value, the rest written 0 and ignored"),
    ("lookup_state", "one byte, the values in the values attribute"),
    ("child_state", "one byte, the values in the values attribute"),
    ("pad", "size bytes, written 0, skipped on read; optional means a reader may find them missing"),
    ("str_u32", "len:u32 then len bytes of UTF-8; max_utf16 is a limit in UTF-16 units"),
    ("str_rest", "the rest of the payload as UTF-8"),
    ("bytes_u32", "len:u32, then pad_after_len zero bytes if given, then len bytes; max is a limit on len"),
    ("list_u32", "count:u32 then count items, each laid out as the fields in of"),
    ("optional_node", "has_node:u8 (a bool) then, if 1, the fields in node"),
    ("inline_or_bulk", "a READ reply: if the bulk_bit mask is set in its first u32 the reply is the fields in bulk, else the fields in inline"),
];

/// The attributes a field may carry besides `name` and `type`.
pub(crate) const ATTRIBUTES: &[(&str, &str)] = &[
    ("size", "pad: the number of bytes"),
    ("optional", "pad: true if a reader may find the bytes missing"),
    ("max", "bytes_u32: the largest len a decoder accepts"),
    ("max_utf16", "str_u32: the longest string a decoder accepts, in UTF-16 units"),
    ("pad_after_len", "bytes_u32: the number of zero bytes between len and the data"),
    ("values", "lookup_state, child_state: the byte's values"),
    ("of", "list_u32: the fields of each item"),
    ("node", "optional_node: the fields of the node"),
    ("bulk_bit", "inline_or_bulk: the mask of the bit in the first u32 that selects bulk"),
    ("inline", "inline_or_bulk: the fields when that bit is clear"),
    ("bulk", "inline_or_bulk: the fields when that bit is set"),
    ("bit31", "in a bulk field's len: this u32 has bit 31 (the bulk_bit) set on the wire, and the value is the other 31 bits"),
];

/// One field of a message as the table describes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub name: &'static str,
    pub ty: &'static str,
    pub attrs: Vec<(&'static str, Attr)>,
}

/// An attribute of a field's type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Attr {
    Num(u64),
    Bool(bool),
    Text(&'static str),
    Fields(Vec<Entry>),
}

impl Entry {
    pub(crate) fn new(name: &'static str, ty: &'static str) -> Entry {
        Entry {
            name,
            ty,
            attrs: Vec::new(),
        }
    }

    pub(crate) fn with(mut self, key: &'static str, attr: Attr) -> Entry {
        self.attrs.push((key, attr));
        self
    }

    pub(crate) fn pad(size: u64, optional: bool) -> Entry {
        let e = Entry::new("pad", "pad").with("size", Attr::Num(size));
        if optional {
            e.with("optional", Attr::Bool(true))
        } else {
            e
        }
    }
}

/// One message's description: the payload of a request and of its reply.
pub(crate) struct Message {
    pub request: Vec<Entry>,
    /// Trailing bytes after the last field are refused.
    pub request_strict: bool,
    /// The request has a slot-header flags word (`RequestFlags`).
    pub slot_flags: bool,
    pub reply: Vec<Entry>,
    pub reply_strict: bool,
}

/// A JSON value, just enough to write the contract.
enum J {
    Num(i64),
    Bool(bool),
    Str(&'static str),
    Owned(String),
    Arr(Vec<J>),
    Obj(Vec<(&'static str, J)>),
}

impl J {
    fn is_scalar(&self) -> bool {
        !matches!(self, J::Arr(_) | J::Obj(_))
    }

    /// Objects and arrays of scalars go on one line; the rest, one member to
    /// a line, two-space indented.
    fn write(&self, out: &mut String, depth: usize) {
        match self {
            J::Num(n) => out.push_str(&n.to_string()),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Str(s) => quote(s, out),
            J::Owned(s) => quote(s, out),
            J::Arr(items) if items.is_empty() => out.push_str("[]"),
            J::Arr(items) if items.iter().all(J::is_scalar) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.write(out, depth);
                }
                out.push(']');
            }
            J::Arr(items) => {
                out.push_str("[\n");
                for (i, v) in items.iter().enumerate() {
                    indent(out, depth + 1);
                    v.write(out, depth + 1);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                indent(out, depth);
                out.push(']');
            }
            J::Obj(members) if members.iter().all(|(_, v)| v.is_scalar()) => {
                out.push('{');
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    quote(k, out);
                    out.push_str(": ");
                    v.write(out, depth);
                }
                out.push('}');
            }
            J::Obj(members) => {
                out.push_str("{\n");
                for (i, (k, v)) in members.iter().enumerate() {
                    indent(out, depth + 1);
                    quote(k, out);
                    out.push_str(": ");
                    v.write(out, depth + 1);
                    out.push_str(if i + 1 < members.len() { ",\n" } else { "\n" });
                }
                indent(out, depth);
                out.push('}');
            }
        }
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn entry_json(e: &Entry) -> J {
    let mut members = vec![("name", J::Str(e.name)), ("type", J::Str(e.ty))];
    for (k, a) in &e.attrs {
        members.push((
            k,
            match a {
                Attr::Num(n) => J::Num(*n as i64),
                Attr::Bool(b) => J::Bool(*b),
                Attr::Text(s) => J::Str(s),
                Attr::Fields(f) => fields_json(f),
            },
        ));
    }
    J::Obj(members)
}

fn fields_json(fields: &[Entry]) -> J {
    J::Arr(fields.iter().map(entry_json).collect())
}

/// A flag set as `[{"name", "mask"}]`; `mask` is the flag's value as a
/// mask (append is 32), not a bit index.
fn flags_json<'a>(flags: impl Iterator<Item = (&'a str, u32)>) -> J {
    J::Arr(
        flags
            .map(|(name, bits)| {
                J::Obj(vec![
                    ("name", J::Owned(name.to_ascii_lowercase())),
                    ("mask", J::Num(i64::from(bits))),
                ])
            })
            .collect(),
    )
}

/// The ring protocol as `contract/protocol.json`, the file a release ships so
/// a director in any language can build its own tables from it:
///
/// ```text
/// {"wire_version": 4,
///  "opcodes": [{"name", "number", "request_strict", "slot_flags", "request": [fields],
///               "reply_strict", "reply": [fields]}],
///  "reserved_opcodes": [4, 12],
///  "statuses": [{"name", "number"}],
///  "open_flags": [{"name", "mask"}],
///  "request_flags": [{"name", "mask"}],
///  "types": [{"name", "wire"}],
///  "attributes": [{"name", "wire"}]}
/// ```
///
/// Fields are in wire order; "types" and "attributes" say what each field
/// type and attribute means.
/// Two-space indented, ending in a newline, so the checked-in file is
/// byte-stable. Written by hand, as `aether_env::contract_json` is: the crate
/// needs no JSON library.
pub fn contract_json() -> String {
    let opcodes = Opcode::ALL
        .iter()
        .map(|op| {
            let m = op.describe();
            J::Obj(vec![
                ("name", J::Str(op.name())),
                ("number", J::Num(i64::from(*op as u32))),
                ("request_strict", J::Bool(m.request_strict)),
                ("slot_flags", J::Bool(m.slot_flags)),
                ("request", fields_json(&m.request)),
                ("reply_strict", J::Bool(m.reply_strict)),
                ("reply", fields_json(&m.reply)),
            ])
        })
        .collect();
    // The numbers below the highest opcode that no opcode has are reserved.
    let top = Opcode::ALL.iter().map(|&op| op as u32).max().unwrap_or(0);
    let reserved = (1..top)
        .filter(|&n| Opcode::try_from(n).is_err())
        .map(|n| J::Num(i64::from(n)))
        .collect();
    let statuses = Status::ALL
        .iter()
        .map(|s| {
            J::Obj(vec![
                ("name", J::Str(s.name())),
                ("number", J::Num(i64::from(s.code()))),
            ])
        })
        .collect();
    let defs = |table: &'static [(&'static str, &'static str)]| {
        J::Arr(
            table
                .iter()
                .map(|&(name, wire)| J::Obj(vec![("name", J::Str(name)), ("wire", J::Str(wire))]))
                .collect(),
        )
    };
    let doc = J::Obj(vec![
        ("wire_version", J::Num(i64::from(crate::WIRE_VERSION))),
        ("opcodes", J::Arr(opcodes)),
        ("reserved_opcodes", J::Arr(reserved)),
        ("statuses", J::Arr(statuses)),
        (
            "open_flags",
            flags_json(OpenFlags::all().iter_names().map(|(n, f)| (n, f.bits()))),
        ),
        (
            "request_flags",
            flags_json(RequestFlags::all().iter_names().map(|(n, f)| (n, f.bits()))),
        ),
        ("types", defs(TYPES)),
        ("attributes", defs(ATTRIBUTES)),
    ]);
    let mut out = String::new();
    doc.write(&mut out, 0);
    out.push('\n');
    out
}
