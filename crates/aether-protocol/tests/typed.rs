//! The typed codec: round trips over generated messages, the opcode and
//! status tables against the historical numbers, and malformed input (every
//! cut and every extra byte of every golden vector, garbage of any length).

mod common;

use aether_protocol::{
    any_reply_for, any_request, any_request_for, Opcode, Reply, Request, RequestFlags, RootId,
    Status, OPCODES, STATUSES,
};
use common::Msg;
use proptest::prelude::*;

/// The wire numbers as literals (the same as `wire_numbers_are_the_historical_ones`).
const HISTORICAL_OPCODES: &[(&str, u32)] = &[
    ("getattr", 1),
    ("readdir", 2),
    ("open", 3),
    ("read", 5),
    ("write", 6),
    ("setattr", 7),
    ("rename", 8),
    ("delete", 9),
    ("mkdir", 10),
    ("close", 11),
    ("heartbeat", 13),
    ("stored-names", 14),
    ("reg-lookup", 15),
    ("reg-key", 16),
    ("reg-set-value", 17),
    ("reg-delete-value", 18),
    ("reg-create-key", 19),
    ("reg-delete-key", 20),
    ("reg-rename-key", 21),
    ("reg-changed", 22),
    ("rename-x", 23),
    ("stage-exec", 24),
];

const HISTORICAL_STATUSES: &[(&str, i32)] = &[
    ("ok", 0),
    ("not-found", -1),
    ("not-a-directory", -2),
    ("bad-request", -3),
    ("io-error", -4),
    ("is-dir", -5),
    ("bad-fh", -6),
    ("no-space", -7),
    ("not-supported", -8),
    ("read-only", -9),
    ("exists", -10),
    ("reply-too-large", -11),
    ("cross-root", -12),
];

#[test]
fn opcodes_are_the_historical_table() {
    let typed: Vec<(&str, u32)> = Opcode::ALL
        .iter()
        .map(|&op| (op.name(), op as u32))
        .collect();
    assert_eq!(typed, HISTORICAL_OPCODES);
    assert_eq!(OPCODES, HISTORICAL_OPCODES);
}

#[test]
fn statuses_are_the_historical_table() {
    let typed: Vec<(&str, i32)> = Status::ALL
        .iter()
        .map(|&st| (st.name(), st.code()))
        .collect();
    assert_eq!(typed, HISTORICAL_STATUSES);
    assert_eq!(STATUSES, HISTORICAL_STATUSES);
}

#[test]
fn only_the_live_opcode_numbers_convert() {
    for n in 0..=64u32 {
        let live = HISTORICAL_OPCODES.iter().find(|(_, num)| *num == n);
        match (Opcode::try_from(n), live) {
            (Ok(op), Some((name, _))) => {
                assert_eq!(op as u32, n);
                assert_eq!(op.name(), *name);
            }
            (Err(_), None) => {}
            (got, want) => panic!("opcode {n}: got {got:?}, live entry {want:?}"),
        }
    }
    for n in [0, 4, 12, 25, u32::MAX] {
        assert!(Opcode::try_from(n).is_err(), "{n}");
    }
}

#[test]
fn only_the_known_status_numbers_convert() {
    for n in -64..=64i32 {
        let known = HISTORICAL_STATUSES.iter().any(|(_, num)| *num == n);
        assert_eq!(Status::try_from(n).is_ok(), known, "status {n}");
    }
    assert!(
        Status::try_from(i32::MIN).is_err(),
        "overflow is not a status"
    );
}

/// How a golden vector may be cut or extended and still decode.
struct Slack {
    /// The shortest proper prefix that decodes (`None`: none does).
    prefix_from: Option<usize>,
    /// One extra byte at the end still decodes.
    trailing: bool,
}

/// The exceptions to "every proper prefix and every vector plus one byte is
/// an error". A file-op message (lenient, `docs/wire.md` 3.2) ignores
/// trailing bytes, or, when it ends in a rest-of-payload path or text, reads
/// them as more of it; such a path also decodes cut anywhere after the fixed
/// fields, since every golden path is ASCII. Registry messages are strict:
/// every one is absent here, so each cut and each extra byte must fail.
fn slack(name: &str) -> Slack {
    let (prefix_from, trailing) = match name {
        // root | flags | path: the path may be cut anywhere.
        "open-req-read-skyrim" => (Some(8), true),
        // Fixed 18 bytes, trailing bytes ignored.
        "getattr-resp-file-123" => (None, true),
        // The final pad is optional: 20 bytes decode.
        "read-req-fh7-off10-len4" => (Some(20), true),
        // Counted data; trailing bytes ignored.
        "read-resp-abcd" => (None, true),
        "readdir-resp-two" => (None, true),
        "close-req-99" => (None, true),
        "open-resp-fh42-size1000" => (None, true),
        "open-resp-immutable-gen-0xa1b2c3d4" => (None, true),
        "read-resp-bulk-len5-off65536" => (None, true),
        "write-req-fh7-off10-abc" => (None, true),
        // A reader needs only the first word.
        "write-resp-3" => (Some(4), true),
        // root | mode | path.
        "mkdir-req-mode493-dir" => (Some(8), true),
        // root | from_len | "old.txt" | to: `to` may be cut anywhere.
        "rename-req-a-b" => (Some(15), true),
        "setattr-req-fh5-size100" => (None, true),
        // root | path.
        "path-req-root1-a" => (Some(4), true),
        // root | skip | path.
        "names-req-root2-skip1" => (Some(8), true),
        // The whole payload is the text; empty is one empty name.
        "names-resp-two" => (Some(0), true),
        _ => (None, false),
    };
    Slack {
        prefix_from,
        trailing,
    }
}

fn decodes(m: &Msg, bytes: &[u8]) -> bool {
    match m {
        Msg::Req(r) => Request::decode(r.opcode(), r.flags(), bytes).is_ok(),
        Msg::Rep(op, _) => Reply::decode(*op, 0, bytes).is_ok(),
        Msg::Raw(_) => unreachable!(),
    }
}

#[test]
fn cut_or_extended_golden_vectors_fail_except_where_the_format_allows() {
    for (name, m) in common::vectors() {
        if let Msg::Raw(_) = m {
            continue;
        }
        let bytes = m.bytes();
        let s = slack(name);
        for n in 0..bytes.len() {
            let want = s.prefix_from.is_some_and(|from| n >= from);
            assert_eq!(decodes(&m, &bytes[..n]), want, "{name} cut to {n} bytes");
        }
        // ASCII bytes: a 0xff after a rest-of-payload path is bad UTF-8, which
        // out_of_range_bytes_fail covers.
        for extra in [0u8, 1, b'x'] {
            let mut long = bytes.clone();
            long.push(extra);
            assert_eq!(
                decodes(&m, &long),
                s.trailing,
                "{name} plus byte {extra:#x}"
            );
        }
    }
}

#[test]
fn out_of_range_bytes_fail() {
    // A registry boolean is 0 or 1.
    let mut b = Vec::new();
    Request::RegCreateKey {
        path: "p",
        volatile: true,
    }
    .encode(&mut b);
    *b.last_mut().unwrap() = 2;
    assert!(Request::decode(Opcode::RegCreateKey, 0, &b).is_err());
    // A lookup state is 0 to 3.
    let mut b = 1u64.to_le_bytes().to_vec();
    b.extend_from_slice(&[4, 0]);
    assert!(Reply::decode(Opcode::RegLookup, 0, &b).is_err());
    // Bad UTF-8 in a path.
    let mut b = 0u32.to_le_bytes().to_vec();
    b.push(0xff);
    assert!(Request::decode(Opcode::GetAttr, 0, &b).is_err());
    // A count larger than the input.
    let b = u32::MAX.to_le_bytes();
    assert!(Reply::decode(Opcode::ReadDir, 0, &b).is_err());
}

#[test]
fn read_carries_its_slot_flags_and_drops_unknown_bits() {
    let r = Request::Read {
        slot_flags: RequestFlags::READ_BULK,
        fh: 1,
        offset: 2,
        len: 3,
    };
    assert_eq!(r.flags(), 1);
    let mut b = Vec::new();
    r.encode(&mut b);
    assert_eq!(Request::decode(Opcode::Read, u32::MAX, &b), Ok(r));
    let plain = Request::Close { fh: 1 };
    assert_eq!(plain.flags(), 0);
}

/// RENAME's `from_len` framing with names whose bytes are multi-byte UTF-8:
/// the split falls where the length says, not at a character boundary guess.
#[test]
fn rename_framing_keeps_multibyte_names_apart() {
    let r = Request::Rename {
        root: RootId(3),
        from: "Données/été.txt",
        to: "Ünïcødé/日本.txt",
    };
    let mut b = Vec::new();
    r.encode(&mut b);
    assert_eq!(Request::decode(Opcode::Rename, 0, &b), Ok(r));
    let x = Request::RenameX {
        from_root: RootId(1),
        to_root: RootId(2),
        from: "日本",
        to: "",
    };
    let mut b = Vec::new();
    x.encode(&mut b);
    assert_eq!(Request::decode(Opcode::RenameX, 0, &b), Ok(x));
}

fn is_registry(op: Opcode) -> bool {
    (Opcode::RegLookup as u32..=Opcode::RegChanged as u32).contains(&(op as u32))
}

proptest! {
    #[test]
    fn every_request_round_trips(r in any_request()) {
        let mut b = Vec::new();
        r.encode(&mut b);
        prop_assert_eq!(Request::decode(r.opcode(), r.flags(), &b), Ok(r));
    }

    #[test]
    fn every_reply_round_trips(
        (op, rep) in proptest::sample::select(Opcode::ALL)
            .prop_flat_map(|op| (Just(op), any_reply_for(op)))
    ) {
        let mut b = Vec::new();
        rep.encode(&mut b);
        prop_assert_eq!(Reply::decode(op, 0, &b), Ok(rep));
    }

    /// Registry messages are strict: any cut and any extra byte fails.
    #[test]
    fn registry_messages_refuse_cuts_and_extra_bytes(
        (op, req, rep) in proptest::sample::select(Opcode::ALL)
            .prop_filter("registry", |&op| is_registry(op))
            .prop_flat_map(|op| (Just(op), any_request_for(op), any_reply_for(op))),
        extra in any::<u8>(),
    ) {
        let mut q = Vec::new();
        req.encode(&mut q);
        let mut p = Vec::new();
        rep.encode(&mut p);
        for n in 0..q.len() {
            prop_assert!(Request::decode(op, 0, &q[..n]).is_err(), "request cut to {}", n);
        }
        for n in 0..p.len() {
            prop_assert!(Reply::decode(op, 0, &p[..n]).is_err(), "reply cut to {}", n);
        }
        q.push(extra);
        p.push(extra);
        prop_assert!(Request::decode(op, 0, &q).is_err());
        prop_assert!(Reply::decode(op, 0, &p).is_err());
    }

    /// Garbage never panics, whatever the opcode and flags.
    #[test]
    fn garbage_never_panics(
        op in proptest::sample::select(Opcode::ALL),
        flags in any::<u32>(),
        bytes in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let _ = Request::decode(op, flags, &bytes);
        if let Ok(Reply::ReadDir(list)) = Reply::decode(op, flags, &bytes) {
            prop_assert_eq!(list.iter().count(), list.len());
        }
    }
}
