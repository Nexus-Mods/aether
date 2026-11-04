//! The protocol table: every message of the ring protocol, defined once.
//!
//! One `protocol!` invocation lists the reply structs, every opcode with its
//! number, name and request fields, and every reply shape. From it the macro
//! emits [`Opcode`], [`Request`], [`Reply`], their codecs, and (behind the
//! `testing` feature) proptest strategies built from the same field lists.
//!
//! A field is written `name: Type` when the type is its own codec, or
//! `name: Type as Codec` when a marker type in `field.rs` says how it is
//! framed. A message is `strict` (every byte must be consumed: the registry
//! ops) or `lenient` (trailing bytes are ignored: the file ops, as
//! `docs/wire.md` 3.2 has it). `[slot_flags]` after a request's strictness
//! gives it the slot header's flag word as a [`RequestFlags`] field; only
//! READ has one.
//!
//! Three shapes are written by hand, because a table line cannot say them:
//! [`DirEntries`] (READDIR's counted list, decoded lazily), [`ReadReply`]
//! (inline or bulk, chosen by bit 31 of its first word) and the registry node
//! inside [`KeyReply`] (`field.rs`, `OptNode`).

use crate::buf::Buf;
use crate::describe::{Attr, Entry, Message};
use crate::field::{
    Bool, Bytes, Field, Flag, LenPadBytes, LowBit, OptNode, PadAfter, PadBefore, RestStr, Str,
    StrMax,
};
use crate::root::RootId;
use crate::status::{OpenFlags, RequestFlags};
use crate::wire::{DecodeError, Rd};
use aether_registry::overlay::{MAX_DATA, MAX_KEY_NAME, MAX_VALUE_NAME};
use aether_registry::{Lookup, Node};
use std::borrow::Cow;

#[cfg(feature = "testing")]
use crate::field::{arb, Arb};
#[cfg(feature = "testing")]
use proptest::prelude::*;

/// High bit of a READ reply's first word: the reply is bulk, its data in
/// the arena at `arena_offset`.
pub const READ_RESP_BULK_BIT: u32 = 0x8000_0000;

/// The codec of a table field: its own type, or the one after `as`.
macro_rules! codec {
    ($t:ty) => {
        $t
    };
    ($t:ty, $c:ty) => {
        $c
    };
}

/// The statements that end a decode: `strict` refuses trailing bytes.
macro_rules! finish {
    (strict, $r:ident) => {
        $r.done()?;
    };
    (lenient, $r:ident) => {};
}

/// Whether a message is `strict`.
macro_rules! is_strict {
    (strict) => {
        true
    };
    (lenient) => {
        false
    };
}

/// Whether a request lists a `[slot_flags]` field.
macro_rules! has_slot_flags {
    () => {
        false
    };
    ($f:ident) => {
        true
    };
}

/// A request's slot flags word: 0 unless it has a `[slot_flags]` field.
macro_rules! slot_flags_of {
    () => {
        0
    };
    ($f:ident) => {
        $f.bits()
    };
}

/// A struct type with its lifetime, if any, made `'static`.
#[cfg(feature = "testing")]
macro_rules! static_ty {
    ($n:ident) => { $n };
    ($n:ident $lt:lifetime) => { $n<'static> };
}

#[cfg(feature = "testing")]
macro_rules! slot_flags_strategy {
    ($f:ident) => {
        (0..=RequestFlags::all().bits()).prop_map(RequestFlags::from_bits_truncate)
    };
}

macro_rules! protocol {
    (
        structs {
            $(
                $(#[$smeta:meta])*
                struct $sname:ident $(<$slt:lifetime>)? {
                    $( $(#[$fmeta:meta])* $sf:ident : $sfty:ty $(as $sfc:ty)? ),* $(,)?
                }
            )*
        }
        requests {
            $(
                $(#[$qmeta:meta])*
                $op:ident = $num:literal $name:literal $qstrict:ident $([$flagf:ident])? {
                    $( $(#[$qfmeta:meta])* $qf:ident : $qfty:ty $(as $qfc:ty)? ),* $(,)?
                } => $shape:ident
            ),* $(,)?
        }
        replies {
            $(
                $(#[$pmeta:meta])*
                $pstrict:ident $pname:ident $pfield:literal ( $pty:ty $(as $pc:ty)? )
            ),* $(,)?
        }
    ) => {
        $(
            $(#[$smeta])*
            #[derive(Debug, Clone, PartialEq, Eq)]
            pub struct $sname $(<$slt>)? {
                $( $(#[$fmeta])* pub $sf: $sfty, )*
            }

            impl<'a> Field<'a> for $sname $(<$slt>)? {
                type Value = Self;
                fn encode<B: Buf + ?Sized>(v: &Self, out: &mut B) {
                    $( <codec!($sfty $(, $sfc)?) as Field<'a>>::encode(&v.$sf, out); )*
                }
                fn decode(r: &mut Rd<'a>) -> Result<Self, DecodeError> {
                    Ok($sname {
                        $( $sf: <codec!($sfty $(, $sfc)?) as Field<'a>>::decode(r)?, )*
                    })
                }
                fn describe(_name: &'static str, out: &mut Vec<Entry>) {
                    $( <codec!($sfty $(, $sfc)?) as Field<'a>>::describe(stringify!($sf), out); )*
                }
            }

            #[cfg(feature = "testing")]
            impl $(<$slt>)? Arb for $sname $(<$slt>)? {
                type Value = static_ty!($sname $($slt)?);
                fn strategy() -> BoxedStrategy<Self::Value> {
                    (Just(()), $( <codec!($sfty $(, $sfc)?) as Arb>::strategy(), )*)
                        .prop_map(|((), $($sf,)*)| $sname { $($sf,)* })
                        .boxed()
                }
            }
        )*

        /// A request's opcode: the slot header's `opcode` word. Numbers 4
        /// and 12 are reserved and are not variants; nothing sends them, and a director answers
        /// them, like any unknown number, [`Status::BadRequest`](crate::Status).
        /// Never renumber.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u32)]
        pub enum Opcode {
            $( $(#[$qmeta])* $op = $num, )*
        }

        impl Opcode {
            /// Every opcode, in number order.
            pub const ALL: &'static [Opcode] = &[$(Opcode::$op),*];

            /// The opcode's name in the protocol table (`"getattr"`,
            /// `"stored-names"`, ...).
            pub const fn name(self) -> &'static str {
                match self {
                    $( Opcode::$op => $name, )*
                }
            }

            /// This opcode's request and reply layouts, from the same field
            /// lists the codecs are built from.
            // 'a names the borrowed types in the table's field lists; clippy cannot see
            // through the macro.
            #[allow(clippy::extra_unused_lifetimes)]
            pub(crate) fn describe<'a>(self) -> Message {
                let mut request = Vec::new();
                let (reply_strict, reply) = self.shape().describe();
                match self {
                    $(
                        Opcode::$op => {
                            $( <codec!($qfty $(, $qfc)?) as Field<'a>>::describe(
                                stringify!($qf),
                                &mut request,
                            ); )*
                            Message {
                                request,
                                request_strict: is_strict!($qstrict),
                                slot_flags: has_slot_flags!($($flagf)?),
                                reply,
                                reply_strict,
                            }
                        }
                    )*
                }
            }

            const fn shape(self) -> Shape {
                match self {
                    $( Opcode::$op => Shape::$shape, )*
                }
            }
        }

        impl TryFrom<u32> for Opcode {
            type Error = DecodeError;
            fn try_from(n: u32) -> Result<Opcode, DecodeError> {
                match n {
                    $( $num => Ok(Opcode::$op), )*
                    _ => Err(DecodeError::new("unknown opcode")),
                }
            }
        }

        /// A request: one variant per opcode. Paths and data borrow from the
        /// payload it was decoded from.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Request<'a> {
            $(
                $(#[$qmeta])*
                $op {
                    $( $flagf: RequestFlags, )?
                    $( $(#[$qfmeta])* $qf: $qfty, )*
                },
            )*
        }

        impl<'a> Request<'a> {
            pub fn opcode(&self) -> Opcode {
                match self {
                    $( Request::$op { .. } => Opcode::$op, )*
                }
            }

            /// The slot header's `flags` word to send with this request.
            pub fn flags(&self) -> u32 {
                match self {
                    $( Request::$op { $($flagf,)? .. } => slot_flags_of!($($flagf)?), )*
                }
            }

            /// Appends the request payload to `out`.
            pub fn encode<B: Buf + ?Sized>(&self, out: &mut B) {
                match self {
                    $(
                        Request::$op { $($qf,)* .. } => {
                            $( <codec!($qfty $(, $qfc)?) as Field<'a>>::encode($qf, out); )*
                        }
                    )*
                }
            }

            /// Decodes the payload of a request with opcode `op` and slot
            /// flags `flags`, borrowing from `payload`.
            pub fn decode(
                op: Opcode,
                flags: u32,
                payload: &'a [u8],
            ) -> Result<Request<'a>, DecodeError> {
                let mut r = Rd(payload);
                let req = match op {
                    $(
                        Opcode::$op => {
                            $( let $flagf = RequestFlags::from_bits_truncate(flags); )?
                            $( let $qf = <codec!($qfty $(, $qfc)?) as Field<'a>>::decode(&mut r)?; )*
                            finish!($qstrict, r);
                            Request::$op { $($flagf,)? $($qf,)* }
                        }
                    )*
                };
                Ok(req)
            }

            /// A proptest strategy for requests with opcode `op`.
            #[cfg(feature = "testing")]
            pub(crate) fn strategy_for(op: Opcode) -> BoxedStrategy<Request<'static>> {
                match op {
                    $(
                        Opcode::$op => (
                            Just(()),
                            $( slot_flags_strategy!($flagf), )?
                            $( <codec!($qfty $(, $qfc)?) as Arb>::strategy(), )*
                        )
                            .prop_map(|((), $($flagf,)? $($qf,)*)| Request::$op {
                                $($flagf,)? $($qf,)*
                            })
                            .boxed(),
                    )*
                }
            }
        }

        /// What a reply payload holds, by opcode.
        #[derive(Clone, Copy)]
        enum Shape {
            $( $pname, )*
            Empty,
        }

        impl Shape {
            /// Whether the reply is strict, and its fields.
            // 'a names the borrowed types in the table's field lists; clippy cannot see
            // through the macro.
            #[allow(clippy::extra_unused_lifetimes)]
            fn describe<'a>(self) -> (bool, Vec<Entry>) {
                let mut out = Vec::new();
                match self {
                    $(
                        Shape::$pname => {
                            <codec!($pty $(, $pc)?) as Field<'a>>::describe($pfield, &mut out);
                            (is_strict!($pstrict), out)
                        }
                    )*
                    Shape::Empty => (false, out),
                }
            }
        }

        /// A reply payload on [`Status::Ok`](crate::Status). Its shape is
        /// fixed by the request's opcode; several opcodes share
        /// [`Reply::Empty`] and [`Reply::RegVersion`].
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Reply<'a> {
            $( $(#[$pmeta])* $pname($pty), )*
            /// SETATTR, RENAME, DELETE, MKDIR, CLOSE, HEARTBEAT and RENAME_X
            /// reply with an empty payload. A payload there is ignored.
            Empty,
        }

        impl<'a> Reply<'a> {
            /// Appends the reply payload to `out`.
            pub fn encode<B: Buf + ?Sized>(&self, out: &mut B) {
                match self {
                    $( Reply::$pname(v) => <codec!($pty $(, $pc)?) as Field<'a>>::encode(v, out), )*
                    Reply::Empty => {}
                }
            }

            /// Decodes the reply to a request with opcode `op`, borrowing
            /// from `payload`. `_req_flags` is the request's slot flags: a
            /// READ reply says itself whether it is bulk (bit 31 of its first
            /// word), whatever the request asked for, so no shape depends on
            /// them today.
            pub fn decode(
                op: Opcode,
                _req_flags: u32,
                payload: &'a [u8],
            ) -> Result<Reply<'a>, DecodeError> {
                let mut r = Rd(payload);
                let reply = match op.shape() {
                    $(
                        Shape::$pname => {
                            let v = <codec!($pty $(, $pc)?) as Field<'a>>::decode(&mut r)?;
                            finish!($pstrict, r);
                            Reply::$pname(v)
                        }
                    )*
                    Shape::Empty => Reply::Empty,
                };
                Ok(reply)
            }

            /// A proptest strategy for replies to opcode `op`.
            #[cfg(feature = "testing")]
            pub(crate) fn strategy_for(op: Opcode) -> BoxedStrategy<Reply<'static>> {
                match op.shape() {
                    $(
                        Shape::$pname => <codec!($pty $(, $pc)?) as Arb>::strategy()
                            .prop_map(Reply::$pname)
                            .boxed(),
                    )*
                    Shape::Empty => Just(Reply::Empty).boxed(),
                }
            }
        }
    };
}

protocol! {
    structs {
        /// GETATTR's reply, 18 bytes. A missing path is `found = false` with
        /// the other fields zero, not an error status.
        struct AttrResp {
            found: bool as Flag,
            is_dir: bool as Flag,
            size: u64,
            /// Seconds since the Unix epoch; 0 or less is "no time".
            mtime: i64,
        }

        /// OPEN's reply, 24 bytes: `fh | size | is_dir:u8 | flags:u8 |
        /// pad[2] | mount_gen:u32`. `flags` and `mount_gen` were padding
        /// (zero) until the shim's read cache needed them, so either side may
        /// be older than the other: an old director's reply decodes as
        /// mutable, generation 0, which a cache never serves from.
        #[derive(Default)]
        struct OpenResp {
            fh: u64,
            size: u64,
            is_dir: bool as Flag,
            /// The bytes behind `fh` cannot change while it is open (the
            /// director's layer that serves it says so). A client may cache
            /// what it reads through such a handle. Always `false` for a
            /// write open. Bit 0 of the flags byte.
            immutable: bool as LowBit,
            /// The director's mount generation when it opened `fh`: bumped
            /// whenever a root's provider is replaced. Two immutable opens of
            /// one path with the same generation (and size) are the same
            /// content; across a remount they need not be. A director may
            /// send `0`.
            mount_gen: u32 as PadBefore<2, u32>,
        }

        /// One READDIR entry: `name | is_dir:u8 | size:u64 | mtime:i64`.
        #[derive(Copy)]
        struct DirEntry<'a> {
            /// The stored spelling, one component.
            name: &'a str as Str,
            is_dir: bool as Flag,
            size: u64,
            mtime: i64,
        }

        /// REG_LOOKUP's reply: `version | state:u8 | below:u8`.
        #[derive(Copy)]
        struct LookupReply {
            /// The overlay's version at the lookup.
            version: u64,
            state: Lookup,
            /// The overlay has anything strictly under the key.
            below: bool as Bool,
        }

        /// REG_KEY's reply: `version | has_node:u8 | node?`.
        struct KeyReply<'a> {
            version: u64,
            /// The overlay's node for the key, if it has one. Encoded from a
            /// borrowed node; decoded owned.
            node: Option<Cow<'a, Node>> as OptNode,
        }

        /// REG_CHANGED's reply: `version | changed:u8`.
        #[derive(Copy)]
        struct ChangedReply {
            version: u64,
            changed: bool as Bool,
        }
    }

    requests {
        /// `root | vpath`.
        GetAttr = 1 "getattr" lenient {
            root: RootId,
            path: &'a str as RestStr,
        } => GetAttr,
        /// `root | vpath` (the directory; `"."` for the root).
        ReadDir = 2 "readdir" lenient {
            root: RootId,
            path: &'a str as RestStr,
        } => ReadDir,
        /// `root | oflags | vpath`.
        Open = 3 "open" lenient {
            root: RootId,
            flags: OpenFlags,
            path: &'a str as RestStr,
        } => Open,
        /// `fh | offset | len:u32 | pad:u32`; a 20-byte request (no pad)
        /// decodes. The slot flags may ask for a bulk reply.
        Read = 5 "read" lenient [slot_flags] {
            fh: u64,
            offset: u64,
            len: u32 as PadAfter<u32, 4>,
        } => Read,
        /// `fh | offset | len:u32 | pad:u32 | data[len]`; the pad must be
        /// there.
        Write = 6 "write" lenient {
            fh: u64,
            /// Ignored on an `OpenFlags::APPEND` handle.
            offset: u64,
            data: &'a [u8] as LenPadBytes,
        } => Write,
        /// `fh | size`: set the file's length on an open handle.
        SetAttr = 7 "setattr" lenient {
            fh: u64,
            size: u64,
        } => Empty,
        /// `root | from_len:u32 | from | to`. One root: both paths are under
        /// it.
        Rename = 8 "rename" lenient {
            root: RootId,
            from: &'a str as Str,
            to: &'a str as RestStr,
        } => Empty,
        /// `root | vpath`.
        Delete = 9 "delete" lenient {
            root: RootId,
            path: &'a str as RestStr,
        } => Empty,
        /// `root | mode:u32 | vpath`. The shim sends `mode = 0o755`.
        Mkdir = 10 "mkdir" lenient {
            root: RootId,
            mode: u32,
            path: &'a str as RestStr,
        } => Empty,
        /// `fh`.
        Close = 11 "close" lenient {
            fh: u64,
        } => Empty,
        /// Empty. The shim sends exactly one, right after attaching.
        Heartbeat = 13 "heartbeat" lenient {} => Empty,
        /// `root | skip:u32 | vpath`: how the components of `path` from the
        /// `skip`-th on are spelled where they are stored.
        StoredNames = 14 "stored-names" lenient {
            root: RootId,
            skip: u32,
            path: &'a str as RestStr,
        } => StoredNames,
        /// `path`. Registry requests are strict; every reply starts with the
        /// overlay version.
        RegLookup = 15 "reg-lookup" strict {
            path: &'a str as Str,
        } => RegLookup,
        /// `path`.
        RegKey = 16 "reg-key" strict {
            path: &'a str as Str,
        } => RegKey,
        /// `path | name | ty:u32 | data_len:u32 | data`.
        RegSetValue = 17 "reg-set-value" strict {
            path: &'a str as Str,
            name: &'a str as StrMax<MAX_VALUE_NAME>,
            ty: u32,
            data: &'a [u8] as Bytes<MAX_DATA>,
        } => RegVersion,
        /// `path | name`.
        RegDeleteValue = 18 "reg-delete-value" strict {
            path: &'a str as Str,
            name: &'a str as StrMax<MAX_VALUE_NAME>,
        } => RegVersion,
        /// `path | volatile:u8`.
        RegCreateKey = 19 "reg-create-key" strict {
            path: &'a str as Str,
            volatile: bool as Bool,
        } => RegVersion,
        /// `path`.
        RegDeleteKey = 20 "reg-delete-key" strict {
            path: &'a str as Str,
        } => RegVersion,
        /// `path | new_leaf`.
        RegRenameKey = 21 "reg-rename-key" strict {
            path: &'a str as Str,
            new_leaf: &'a str as StrMax<MAX_KEY_NAME>,
        } => RegVersion,
        /// `path | subtree:u8 | since:u64`.
        RegChanged = 22 "reg-changed" strict {
            path: &'a str as Str,
            subtree: bool as Bool,
            since: u64,
        } => RegChanged,
        /// `from_root | to_root | from_len:u32 | from | to`: a rename between
        /// two roots. A file is copied into `to_root` and removed from
        /// `from_root`; a directory is refused with `Status::CrossRoot`.
        RenameX = 23 "rename-x" lenient {
            from_root: RootId,
            to_root: RootId,
            from: &'a str as Str,
            to: &'a str as RestStr,
        } => Empty,
        /// `root | vpath`: stage the program at `path` onto real disk for a
        /// process the launched program starts.
        StageExec = 24 "stage-exec" lenient {
            root: RootId,
            path: &'a str as RestStr,
        } => StageExec,
    }

    replies {
        lenient GetAttr "attr" (AttrResp),
        lenient ReadDir "entries" (DirEntries<'a>),
        lenient Open "open" (OpenResp),
        lenient Read "data" (ReadReply<'a>),
        /// `written:u32 | pad:u32`; a reader needs only the first word.
        lenient Write "written" (u32 as PadAfter<u32, 4>),
        /// The stored names joined by `/` (a name has no `/` in it), no
        /// length prefix.
        lenient StoredNames "names" (&'a str as RestStr),
        /// The real, on-disk path to start the program from.
        lenient StageExec "path" (&'a str as RestStr),
        strict RegLookup "lookup" (LookupReply),
        strict RegKey "key" (KeyReply<'a>),
        /// The overlay version after a write (REG_SET_VALUE,
        /// REG_DELETE_VALUE, REG_CREATE_KEY, REG_DELETE_KEY, REG_RENAME_KEY).
        strict RegVersion "version" (u64),
        strict RegChanged "changed" (ChangedReply),
    }
}

/// READ's reply: `bytes_read:u32 | pad:u32` then the data inline, or, with
/// [`READ_RESP_BULK_BIT`] set in `bytes_read`, `arena_offset:u64`: the data
/// is in the shared arena. The reply says which, whatever the request's
/// flags asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadReply<'a> {
    Inline(&'a [u8]),
    /// `len` must be below [`READ_RESP_BULK_BIT`].
    Bulk {
        len: u32,
        arena_offset: u64,
    },
}

impl<'a> Field<'a> for ReadReply<'a> {
    type Value = Self;
    fn encode<B: Buf + ?Sized>(v: &Self, out: &mut B) {
        match v {
            ReadReply::Inline(data) => LenPadBytes::encode(data, out),
            ReadReply::Bulk { len, arena_offset } => {
                u32::encode(&(len | READ_RESP_BULK_BIT), out);
                out.put(&[0; 4]);
                u64::encode(arena_offset, out);
            }
        }
    }
    fn decode(r: &mut Rd<'a>) -> Result<Self, DecodeError> {
        let raw = r.u32()?;
        r.take(4)?; // pad
        if raw & READ_RESP_BULK_BIT != 0 {
            Ok(ReadReply::Bulk {
                len: raw & !READ_RESP_BULK_BIT,
                arena_offset: r.u64()?,
            })
        } else {
            Ok(ReadReply::Inline(r.take(raw as usize)?))
        }
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        let mut inline = Vec::new();
        LenPadBytes::describe("data", &mut inline);
        let bulk = vec![
            Entry::new("len", "u32").with("bit31", Attr::Bool(true)),
            Entry::pad(4, false),
            Entry::new("arena_offset", "u64"),
        ];
        out.push(
            Entry::new(name, "inline_or_bulk")
                .with("bulk_bit", Attr::Num(u64::from(READ_RESP_BULK_BIT)))
                .with("inline", Attr::Fields(inline))
                .with("bulk", Attr::Fields(bulk)),
        );
    }
}

#[cfg(feature = "testing")]
impl<'a> Arb for ReadReply<'a> {
    type Value = ReadReply<'static>;
    fn strategy() -> BoxedStrategy<ReadReply<'static>> {
        prop_oneof![
            arb::bytes(64).prop_map(|b| ReadReply::Inline(arb::leak_bytes(b))),
            (0..READ_RESP_BULK_BIT, any::<u64>())
                .prop_map(|(len, arena_offset)| ReadReply::Bulk { len, arena_offset }),
        ]
        .boxed()
    }
}

/// READDIR's reply: `count:u32` then `count` [`DirEntry`]s, packed. A decoded
/// listing is checked whole up front and then read lazily, so decoding
/// allocates nothing; one to encode is built from a slice.
#[derive(Clone, Copy)]
pub struct DirEntries<'a>(Entries<'a>);

#[derive(Clone, Copy)]
enum Entries<'a> {
    /// `count` entries, already checked, packed in `bytes`.
    Wire {
        count: u32,
        bytes: &'a [u8],
    },
    Slice(&'a [DirEntry<'a>]),
}

impl<'a> DirEntries<'a> {
    pub fn new(entries: &'a [DirEntry<'a>]) -> Self {
        DirEntries(Entries::Slice(entries))
    }

    pub fn len(&self) -> usize {
        match self.0 {
            Entries::Wire { count, .. } => count as usize,
            Entries::Slice(s) => s.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> DirIter<'a> {
        DirIter(match self.0 {
            Entries::Wire { count, bytes } => Iter::Wire {
                left: count,
                r: Rd(bytes),
            },
            Entries::Slice(s) => Iter::Slice(s.iter()),
        })
    }
}

impl<'a> From<&'a [DirEntry<'a>]> for DirEntries<'a> {
    fn from(entries: &'a [DirEntry<'a>]) -> Self {
        DirEntries::new(entries)
    }
}

impl<'a> IntoIterator for &DirEntries<'a> {
    type Item = DirEntry<'a>;
    type IntoIter = DirIter<'a>;
    fn into_iter(self) -> DirIter<'a> {
        self.iter()
    }
}

impl PartialEq for DirEntries<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl Eq for DirEntries<'_> {}

impl core::fmt::Debug for DirEntries<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The entries of a [`DirEntries`], in wire order.
pub struct DirIter<'a>(Iter<'a>);

enum Iter<'a> {
    Wire { left: u32, r: Rd<'a> },
    Slice(core::slice::Iter<'a, DirEntry<'a>>),
}

impl<'a> Iterator for DirIter<'a> {
    type Item = DirEntry<'a>;
    fn next(&mut self) -> Option<DirEntry<'a>> {
        match &mut self.0 {
            Iter::Wire { left, r } => {
                if *left == 0 {
                    return None;
                }
                *left -= 1;
                // Checked when the listing was decoded, so this succeeds.
                DirEntry::decode(r).ok()
            }
            Iter::Slice(s) => s.next().copied(),
        }
    }
}

impl<'a> Field<'a> for DirEntries<'a> {
    type Value = Self;
    fn encode<B: Buf + ?Sized>(v: &Self, out: &mut B) {
        u32::encode(&(v.len() as u32), out);
        for e in v.iter() {
            DirEntry::encode(&e, out);
        }
    }
    fn decode(r: &mut Rd<'a>) -> Result<Self, DecodeError> {
        let count = r.u32()?;
        let start = r.0;
        // Every entry is at least 21 bytes, so a count beyond the input
        // fails on the way rather than looping long.
        for _ in 0..count {
            DirEntry::decode(r)?;
        }
        let bytes = &start[..start.len() - r.0.len()];
        Ok(DirEntries(Entries::Wire { count, bytes }))
    }
    fn describe(name: &'static str, out: &mut Vec<Entry>) {
        let mut of = Vec::new();
        DirEntry::describe("entry", &mut of);
        out.push(Entry::new(name, "list_u32").with("of", Attr::Fields(of)));
    }
}

#[cfg(feature = "testing")]
impl<'a> Arb for DirEntries<'a> {
    type Value = DirEntries<'static>;
    fn strategy() -> BoxedStrategy<DirEntries<'static>> {
        proptest::collection::vec(<DirEntry<'static> as Arb>::strategy(), 0..4)
            .prop_map(|v| DirEntries::new(Box::leak(v.into_boxed_slice())))
            .boxed()
    }
}

/// A proptest strategy for every request, every opcode.
#[cfg(feature = "testing")]
pub fn any_request() -> BoxedStrategy<Request<'static>> {
    proptest::strategy::Union::new(Opcode::ALL.iter().map(|&op| Request::strategy_for(op))).boxed()
}

/// A proptest strategy for requests with opcode `op`.
#[cfg(feature = "testing")]
pub fn any_request_for(op: Opcode) -> BoxedStrategy<Request<'static>> {
    Request::strategy_for(op)
}

/// A proptest strategy for the replies to opcode `op`.
#[cfg(feature = "testing")]
pub fn any_reply_for(op: Opcode) -> BoxedStrategy<Reply<'static>> {
    Reply::strategy_for(op)
}
