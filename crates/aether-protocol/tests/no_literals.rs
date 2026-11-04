//! The numeric API is gone: `aether-protocol` exports no `OP_*`, `ST_*` or
//! `OPEN_*` constant, no `OPCODES`/`STATUSES` table and no `encode_*` or
//! `decode_*` free function for ring messages. Opcodes, statuses and flags are
//! [`Opcode`], [`Status`], [`OpenFlags`] and [`RequestFlags`]; messages are
//! [`Request`] and [`Reply`].
//!
//! Absence cannot be shown by compiling, so the crate's sources are read:
//! every `pub` item they declare is checked against the old names. The shim
//! configuration (`shimcfg.rs`) is not a ring message and keeps its
//! `encode_config`/`decode_config`.

use aether_protocol::{Opcode, OpenFlags, Reply, Request, RequestFlags, RootId, Status};

/// The old API's names, as a `pub` declaration would start them.
const RETIRED: &[&str] = &[
    "pub const OP_",
    "pub const ST_",
    "pub const OPEN_",
    "pub const FLAG_",
    "pub const OPCODES",
    "pub const STATUSES",
    "pub fn encode_",
    "pub fn decode_",
    "pub fn is_read_resp_bulk",
    "pub struct DirEntryWire",
    "pub struct ReadReq",
    "pub struct WriteReq",
    "pub struct SetattrReq",
];

#[test]
fn the_numeric_api_is_not_declared() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut read = 0;
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().is_some_and(|n| n == "shimcfg.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        read += 1;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim_start();
            for old in RETIRED {
                assert!(
                    !line.starts_with(old),
                    "{}:{}: `{line}` declares the retired `{old}`",
                    path.display(),
                    n + 1
                );
            }
        }
    }
    assert!(
        read >= 5,
        "the crate's sources were not found under {src:?}"
    );
}

/// What took the old API's place, named once so a rename shows up here.
#[test]
fn the_typed_api_names_what_the_numbers_did() {
    let req = Request::Open {
        root: RootId(0),
        flags: OpenFlags::READ | OpenFlags::WRITE,
        path: "a",
    };
    let mut bytes = Vec::new();
    req.encode(&mut bytes);
    assert_eq!(
        Request::decode(Opcode::Open, RequestFlags::empty().bits(), &bytes),
        Ok(req)
    );
    assert_eq!(
        Status::try_from(Status::NotFound.code()),
        Ok(Status::NotFound)
    );
    assert!(Reply::decode(Opcode::Close, 0, &[]).is_ok());
}
