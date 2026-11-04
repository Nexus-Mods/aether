//! [`Field`]: how one field of a message goes on the wire. The protocol
//! table (`table.rs`) names a codec for every field it lists; a codec is
//! either the field's own type (`u32`, `u64`, `i64`, [`RootId`],
//! [`OpenFlags`], a table struct) or one of the marker types here, which say
//! how a plain Rust type is framed (`&str` as a length-prefixed string or as
//! the rest of the payload, `bool` as a lenient or a strict byte, ...).
//!
//! Decoding borrows from the payload and never allocates, except for a
//! registry node, which decodes owned. A length or count read from the wire
//! is never trusted for an allocation: every element is read from the input,
//! so a count beyond it fails on the way.

use crate::buf::Buf;
use crate::describe::{Attr, Entry};
use crate::root::RootId;
use crate::status::OpenFlags;
use crate::wire::{DecodeError, Rd};
use aether_registry::overlay::{MAX_DATA, MAX_KEY_NAME, MAX_VALUE_NAME};
use aether_registry::{Child, Lookup, Node, Value};
use std::borrow::Cow;
use std::marker::PhantomData;

/// One field's wire form. `Value` is the Rust type the message holds;
/// `'a` is the payload a decoded value borrows from.
pub(crate) trait Field<'a> {
    type Value;
    fn encode<B: Buf + ?Sized>(v: &Self::Value, out: &mut B);
    fn decode(r: &mut Rd<'a>) -> Result<Self::Value, DecodeError>;
    /// Appends what this field puts on the wire, named `name`, to `out`: one
    /// entry, or several where the codec folds padding or a struct's fields.
    fn describe(name: &'static str, out: &mut Vec<Entry>);
}

/// A proptest strategy for a codec's values, borrowing nothing (strings and
/// byte slices are leaked, which is fine in a test).
#[cfg(feature = "testing")]
pub(crate) trait Arb {
    type Value: core::fmt::Debug;
    fn strategy() -> proptest::strategy::BoxedStrategy<Self::Value>;
}

#[cfg(feature = "testing")]
pub(crate) mod arb {
    use proptest::prelude::*;

    pub(crate) fn leak_str(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    pub(crate) fn leak_bytes(b: Vec<u8>) -> &'static [u8] {
        Box::leak(b.into_boxed_slice())
    }

    /// Strings of at most `max_chars` characters, any of Unicode: each is
    /// at most two UTF-16 units, so `max_chars` bounds a UTF-16 limit too.
    pub(crate) fn string(max_chars: usize) -> BoxedStrategy<String> {
        proptest::collection::vec(any::<char>(), 0..=max_chars)
            .prop_map(|cs| cs.into_iter().collect())
            .boxed()
    }

    pub(crate) fn bytes(max: usize) -> BoxedStrategy<Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 0..=max).boxed()
    }
}

#[cfg(feature = "testing")]
use proptest::prelude::*;

macro_rules! le_int {
    ($t:ty) => {
        impl<'a> Field<'a> for $t {
            type Value = $t;
            fn encode<B: Buf + ?Sized>(v: &$t, out: &mut B) {
                out.put(&v.to_le_bytes());
            }
            fn decode(r: &mut Rd<'a>) -> Result<$t, DecodeError> {
                Ok(<$t>::from_le_bytes(r.array()?))
            }
            fn describe(name: &'static str, out: &mut Vec<Entry>) {
                out.push(Entry::new(name, stringify!($t)));
            }
        }

        #[cfg(feature = "testing")]
        impl Arb for $t {
            type Value = $t;
            fn strategy() -> BoxedStrategy<$t> {
                any::<$t>().boxed()
            }
        }
    };
}

le_int!(u32);
le_int!(u64);
le_int!(i64);

impl<'a> Field<'a> for RootId {
    type Value = RootId;
    fn encode<B: Buf + ?Sized>(v: &RootId, out: &mut B) {
        out.put(&v.0.to_le_bytes());
    }
    fn decode(r: &mut Rd<'a>) -> Result<RootId, DecodeError> {
        Ok(RootId(r.u32()?))
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "root_id"));
    }
}

#[cfg(feature = "testing")]
impl Arb for RootId {
    type Value = RootId;
    fn strategy() -> BoxedStrategy<RootId> {
        any::<u32>().prop_map(RootId).boxed()
    }
}

impl<'a> Field<'a> for OpenFlags {
    type Value = OpenFlags;
    fn encode<B: Buf + ?Sized>(v: &OpenFlags, out: &mut B) {
        out.put(&v.bits().to_le_bytes());
    }
    fn decode(r: &mut Rd<'a>) -> Result<OpenFlags, DecodeError> {
        Ok(OpenFlags::from_bits_retain(r.u32()?))
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "open_flags"));
    }
}

#[cfg(feature = "testing")]
impl Arb for OpenFlags {
    type Value = OpenFlags;
    fn strategy() -> BoxedStrategy<OpenFlags> {
        any::<u32>().prop_map(OpenFlags::from_bits_retain).boxed()
    }
}

/// A file-op boolean: one byte, written 0 or 1, read as `!= 0`.
pub(crate) struct Flag;

impl<'a> Field<'a> for Flag {
    type Value = bool;
    fn encode<B: Buf + ?Sized>(v: &bool, out: &mut B) {
        out.put(&[*v as u8]);
    }
    fn decode(r: &mut Rd<'a>) -> Result<bool, DecodeError> {
        Ok(r.u8()? != 0)
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "bool_lenient"));
    }
}

/// A registry boolean: one byte, exactly 0 or 1.
pub(crate) struct Bool;

impl<'a> Field<'a> for Bool {
    type Value = bool;
    fn encode<B: Buf + ?Sized>(v: &bool, out: &mut B) {
        out.put(&[*v as u8]);
    }
    fn decode(r: &mut Rd<'a>) -> Result<bool, DecodeError> {
        match r.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::new("boolean not 0 or 1")),
        }
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "bool"));
    }
}

/// A byte whose bit 0 is the value; the other bits are written zero and
/// ignored on read (OPEN's reply flags, bit 0 `OpenResp::immutable`).
pub(crate) struct LowBit;

impl<'a> Field<'a> for LowBit {
    type Value = bool;
    fn encode<B: Buf + ?Sized>(v: &bool, out: &mut B) {
        out.put(&[*v as u8]);
    }
    fn decode(r: &mut Rd<'a>) -> Result<bool, DecodeError> {
        Ok(r.u8()? & 1 != 0)
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "bool_bit0"));
    }
}

#[cfg(feature = "testing")]
macro_rules! arb_bool {
    ($($t:ty),*) => {$(
        impl Arb for $t {
            type Value = bool;
            fn strategy() -> BoxedStrategy<bool> {
                any::<bool>().boxed()
            }
        }
    )*};
}

#[cfg(feature = "testing")]
arb_bool!(Flag, Bool, LowBit);

/// `len:u32 | utf8[len]`, no limit.
pub(crate) struct Str;

impl<'a> Field<'a> for Str {
    type Value = &'a str;
    fn encode<B: Buf + ?Sized>(v: &&'a str, out: &mut B) {
        out.put(&(v.len() as u32).to_le_bytes());
        out.put(v.as_bytes());
    }
    fn decode(r: &mut Rd<'a>) -> Result<&'a str, DecodeError> {
        Rd::utf8(r.bytes(usize::MAX)?)
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "str_u32"));
    }
}

#[cfg(feature = "testing")]
impl Arb for Str {
    type Value = &'static str;
    fn strategy() -> BoxedStrategy<&'static str> {
        arb::string(24).prop_map(arb::leak_str).boxed()
    }
}

/// [`Str`] of at most `MAX` UTF-16 units: a registry name.
pub(crate) struct StrMax<const MAX: usize>;

impl<'a, const MAX: usize> Field<'a> for StrMax<MAX> {
    type Value = &'a str;
    fn encode<B: Buf + ?Sized>(v: &&'a str, out: &mut B) {
        Str::encode(v, out);
    }
    fn decode(r: &mut Rd<'a>) -> Result<&'a str, DecodeError> {
        let s = Str::decode(r)?;
        Rd::check_utf16(s, MAX)?;
        Ok(s)
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "str_u32").with("max_utf16", Attr::Num(MAX as u64)));
    }
}

#[cfg(feature = "testing")]
impl<const MAX: usize> Arb for StrMax<MAX> {
    type Value = &'static str;
    fn strategy() -> BoxedStrategy<&'static str> {
        arb::string((MAX / 2).min(24))
            .prop_map(arb::leak_str)
            .boxed()
    }
}

/// The rest of the payload as UTF-8: the path at the tail of a file request.
pub(crate) struct RestStr;

impl<'a> Field<'a> for RestStr {
    type Value = &'a str;
    fn encode<B: Buf + ?Sized>(v: &&'a str, out: &mut B) {
        out.put(v.as_bytes());
    }
    fn decode(r: &mut Rd<'a>) -> Result<&'a str, DecodeError> {
        Rd::utf8(r.rest())
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "str_rest"));
    }
}

#[cfg(feature = "testing")]
impl Arb for RestStr {
    type Value = &'static str;
    fn strategy() -> BoxedStrategy<&'static str> {
        arb::string(24).prop_map(arb::leak_str).boxed()
    }
}

/// `len:u32 | bytes[len]`, refused above `MAX` bytes.
pub(crate) struct Bytes<const MAX: usize>;

impl<'a, const MAX: usize> Field<'a> for Bytes<MAX> {
    type Value = &'a [u8];
    fn encode<B: Buf + ?Sized>(v: &&'a [u8], out: &mut B) {
        out.put(&(v.len() as u32).to_le_bytes());
        out.put(v);
    }
    fn decode(r: &mut Rd<'a>) -> Result<&'a [u8], DecodeError> {
        r.bytes(MAX)
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "bytes_u32").with("max", Attr::Num(MAX as u64)));
    }
}

#[cfg(feature = "testing")]
impl<const MAX: usize> Arb for Bytes<MAX> {
    type Value = &'static [u8];
    fn strategy() -> BoxedStrategy<&'static [u8]> {
        arb::bytes(MAX.min(64)).prop_map(arb::leak_bytes).boxed()
    }
}

/// `len:u32 | pad:u32 | bytes[len]`: WRITE's data and an inline READ reply's.
/// The pad is written zero and must be present on read.
pub(crate) struct LenPadBytes;

impl<'a> Field<'a> for LenPadBytes {
    type Value = &'a [u8];
    fn encode<B: Buf + ?Sized>(v: &&'a [u8], out: &mut B) {
        out.put(&(v.len() as u32).to_le_bytes());
        out.put(&[0; 4]);
        out.put(v);
    }
    fn decode(r: &mut Rd<'a>) -> Result<&'a [u8], DecodeError> {
        let n = r.u32()? as usize;
        r.take(4)?;
        r.take(n)
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "bytes_u32").with("pad_after_len", Attr::Num(4)));
    }
}

#[cfg(feature = "testing")]
impl Arb for LenPadBytes {
    type Value = &'static [u8];
    fn strategy() -> BoxedStrategy<&'static [u8]> {
        arb::bytes(64).prop_map(arb::leak_bytes).boxed()
    }
}

/// `N` bytes, written zero and skipped (whatever they hold) on read, then `C`.
pub(crate) struct PadBefore<const N: usize, C>(PhantomData<C>);

impl<'a, const N: usize, C: Field<'a>> Field<'a> for PadBefore<N, C> {
    type Value = C::Value;
    fn encode<B: Buf + ?Sized>(v: &C::Value, out: &mut B) {
        out.put(&[0; N]);
        C::encode(v, out);
    }
    fn decode(r: &mut Rd<'a>) -> Result<C::Value, DecodeError> {
        r.take(N)?;
        C::decode(r)
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::pad(N as u64, false));
        C::describe(name, out);
    }
}

/// `C`, then `N` bytes written zero that a reader may find missing: READ's
/// request ends `len | pad`, and a 20-byte one decodes.
pub(crate) struct PadAfter<C, const N: usize>(PhantomData<C>);

impl<'a, C: Field<'a>, const N: usize> Field<'a> for PadAfter<C, N> {
    type Value = C::Value;
    fn encode<B: Buf + ?Sized>(v: &C::Value, out: &mut B) {
        C::encode(v, out);
        out.put(&[0; N]);
    }
    fn decode(r: &mut Rd<'a>) -> Result<C::Value, DecodeError> {
        let v = C::decode(r)?;
        let pad = r.0.len().min(N);
        r.take(pad)?;
        Ok(v)
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        C::describe(name, out);
        out.push(Entry::pad(N as u64, true));
    }
}

#[cfg(feature = "testing")]
impl<const N: usize, C: Arb> Arb for PadBefore<N, C> {
    type Value = C::Value;
    fn strategy() -> BoxedStrategy<C::Value> {
        C::strategy()
    }
}

#[cfg(feature = "testing")]
impl<C: Arb, const N: usize> Arb for PadAfter<C, N> {
    type Value = C::Value;
    fn strategy() -> BoxedStrategy<C::Value> {
        C::strategy()
    }
}

/// REG_LOOKUP's `state:u8`: 0 absent, 1 present, 2 present and created in
/// the overlay, 3 tombstoned; anything else is refused.
impl<'a> Field<'a> for Lookup {
    type Value = Lookup;
    fn encode<B: Buf + ?Sized>(v: &Lookup, out: &mut B) {
        out.put(&[match v {
            Lookup::Absent => 0,
            Lookup::Present { created: false } => 1,
            Lookup::Present { created: true } => 2,
            Lookup::Tombstoned => 3,
        }]);
    }
    fn decode(r: &mut Rd<'a>) -> Result<Lookup, DecodeError> {
        match r.u8()? {
            0 => Ok(Lookup::Absent),
            1 => Ok(Lookup::Present { created: false }),
            2 => Ok(Lookup::Present { created: true }),
            3 => Ok(Lookup::Tombstoned),
            _ => Err(DecodeError::new("lookup state out of range")),
        }
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        out.push(Entry::new(name, "lookup_state").with(
            "values",
            Attr::Text("0 absent, 1 present, 2 present and created in the overlay, 3 tombstoned"),
        ));
    }
}

#[cfg(feature = "testing")]
impl Arb for Lookup {
    type Value = Lookup;
    fn strategy() -> BoxedStrategy<Lookup> {
        prop_oneof![
            Just(Lookup::Absent),
            Just(Lookup::Present { created: false }),
            Just(Lookup::Present { created: true }),
            Just(Lookup::Tombstoned),
        ]
        .boxed()
    }
}

/// REG_KEY's `has_node:u8 | node?`, where a node is `created:u8 |
/// volatile:u8 | last_write:u64 | nvalues:u32 | values | ntombstones:u32 |
/// names | nchildren:u32 | children`; a value is `name | ty:u32 |
/// data_len:u32 | data`, a child `folded | spelling | state:u8` (0 present,
/// 1 tombstone). Written by hand: the node nests three lists. It decodes to
/// an owned [`Node`], the form the shim keeps it in.
pub(crate) struct OptNode;

impl<'a> Field<'a> for OptNode {
    type Value = Option<Cow<'a, Node>>;

    fn encode<B: Buf + ?Sized>(v: &Option<Cow<'a, Node>>, out: &mut B) {
        let Some(n) = v else {
            Bool::encode(&false, out);
            return;
        };
        Bool::encode(&true, out);
        Bool::encode(&n.created, out);
        Bool::encode(&n.volatile, out);
        u64::encode(&n.last_write, out);
        u32::encode(&(n.values.len() as u32), out);
        for v in &n.values {
            Str::encode(&v.name.as_str(), out);
            u32::encode(&v.ty, out);
            Bytes::<MAX_DATA>::encode(&v.data.as_slice(), out);
        }
        u32::encode(&(n.value_tombstones.len() as u32), out);
        for t in &n.value_tombstones {
            Str::encode(&t.as_str(), out);
        }
        u32::encode(&(n.children.len() as u32), out);
        for (folded, (spelling, state)) in &n.children {
            Str::encode(&folded.as_str(), out);
            Str::encode(&spelling.as_str(), out);
            out.put(&[match state {
                Child::Present => 0,
                Child::Tombstone => 1,
            }]);
        }
    }

    fn decode(r: &mut Rd<'a>) -> Result<Option<Cow<'a, Node>>, DecodeError> {
        if !Bool::decode(r)? {
            return Ok(None);
        }
        let mut n = Node {
            created: Bool::decode(r)?,
            volatile: Bool::decode(r)?,
            last_write: r.u64()?,
            ..Node::default()
        };
        // Counts are not trusted for allocation: every element takes at least
        // 4 bytes, so a count beyond the remaining input fails on the way.
        for _ in 0..r.u32()? {
            let name = StrMax::<MAX_VALUE_NAME>::decode(r)?.to_string();
            let ty = r.u32()?;
            let data = Bytes::<MAX_DATA>::decode(r)?.to_vec();
            n.values.push(Value { name, ty, data });
        }
        for _ in 0..r.u32()? {
            n.value_tombstones
                .push(StrMax::<MAX_VALUE_NAME>::decode(r)?.to_string());
        }
        for _ in 0..r.u32()? {
            let folded = StrMax::<MAX_KEY_NAME>::decode(r)?.to_string();
            let spelling = StrMax::<MAX_KEY_NAME>::decode(r)?.to_string();
            let state = match r.u8()? {
                0 => Child::Present,
                1 => Child::Tombstone,
                _ => return Err(DecodeError::new("child state out of range")),
            };
            n.children.insert(folded, (spelling, state));
        }
        Ok(Some(Cow::Owned(n)))
    }

    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        let mut value = Vec::new();
        StrMax::<MAX_VALUE_NAME>::describe("name", &mut value);
        u32::describe("ty", &mut value);
        Bytes::<MAX_DATA>::describe("data", &mut value);
        let mut tombstone = Vec::new();
        StrMax::<MAX_VALUE_NAME>::describe("name", &mut tombstone);
        let mut child = Vec::new();
        StrMax::<MAX_KEY_NAME>::describe("folded", &mut child);
        StrMax::<MAX_KEY_NAME>::describe("spelling", &mut child);
        child.push(
            Entry::new("state", "child_state").with("values", Attr::Text("0 present, 1 tombstone")),
        );
        let list = |name, of| Entry::new(name, "list_u32").with("of", Attr::Fields(of));
        let mut node = Vec::new();
        Bool::describe("created", &mut node);
        Bool::describe("volatile", &mut node);
        u64::describe("last_write", &mut node);
        node.push(list("values", value));
        node.push(list("value_tombstones", tombstone));
        node.push(list("children", child));
        out.push(Entry::new(name, "optional_node").with("node", Attr::Fields(node)));
    }
}

#[cfg(feature = "testing")]
impl Arb for OptNode {
    type Value = Option<Cow<'static, Node>>;
    fn strategy() -> BoxedStrategy<Option<Cow<'static, Node>>> {
        let value = (arb::string(8), any::<u32>(), arb::bytes(16))
            .prop_map(|(name, ty, data)| Value { name, ty, data });
        let child = (
            arb::string(8),
            prop_oneof![Just(Child::Present), Just(Child::Tombstone)],
        );
        let node = (
            proptest::collection::vec(value, 0..3),
            proptest::collection::vec(arb::string(8), 0..3),
            proptest::collection::btree_map(arb::string(8), child, 0..3),
            any::<bool>(),
            any::<bool>(),
            any::<u64>(),
        )
            .prop_map(
                |(values, value_tombstones, children, created, volatile, last_write)| Node {
                    values,
                    value_tombstones,
                    children,
                    created,
                    volatile,
                    last_write,
                },
            );
        proptest::option::of(node.prop_map(Cow::Owned)).boxed()
    }
}
