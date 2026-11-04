//! Wire freeze: fixed inputs must encode to exactly the bytes in
//! `golden/vectors.txt` (one `name hex` line per vector). Every ring message
//! among them is built as a typed `Request` or `Reply` (`common/mod.rs`) and
//! encoded by the protocol table.
//!
//! The file is the contract with the injected shim. If an encoder changes on
//! purpose, bump the ring `VERSION` where the layout changed, then regenerate
//! with `GOLDEN_UPDATE=1 cargo test -p aether-protocol --test golden` and review
//! the diff (the update run itself fails, by design, so it cannot pass in CI). A changed existing line is a wire break.

mod common;

use aether_protocol as P;
use aether_protocol::shimcfg::{decode_config, StaticImport};
use aether_protocol::{AttrResp, OpenResp, ReadReq, Reply, Request};
use common::{sample_node, Msg};
use std::fmt::Write as _;

fn vectors() -> Vec<(&'static str, Vec<u8>)> {
    common::vectors()
        .into_iter()
        .map(|(name, m)| (name, m.bytes()))
        .collect()
}

/// Every typed vector decodes back to the value it was encoded from.
#[test]
fn golden_vectors_decode_to_their_typed_values() {
    for (name, m) in common::vectors() {
        let bytes = m.bytes();
        match m {
            Msg::Req(r) => assert_eq!(
                Request::decode(r.opcode(), r.flags(), &bytes),
                Ok(r),
                "{name}"
            ),
            Msg::Rep(op, r) => assert_eq!(Reply::decode(op, 0, &bytes), Ok(r), "{name}"),
            Msg::Raw(_) => {}
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn render() -> String {
    let mut s = String::new();
    for (name, bytes) in vectors() {
        let _ = writeln!(s, "{name} {}", hex(&bytes));
    }
    s
}

#[test]
fn encoders_match_committed_golden() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/vectors.txt");
    if std::env::var_os("GOLDEN_UPDATE").is_some() {
        std::fs::write(path, render()).unwrap();
        panic!("rewrote tests/golden/vectors.txt; review the diff and rerun without GOLDEN_UPDATE");
    }
    let committed = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
    let rendered = render();
    for (got, want) in rendered.lines().zip(committed.lines()) {
        assert_eq!(got, want, "golden vector drifted: this is a wire break");
    }
    assert_eq!(
        rendered.lines().count(),
        committed.lines().count(),
        "vector count differs from tests/golden/vectors.txt"
    );
}

/// Decoders accept what the golden bytes say, not just what today's encoder
/// emits: a pinned vector decodes back to its inputs.
#[test]
fn golden_bytes_decode_to_their_inputs() {
    let committed = include_str!("golden/vectors.txt");
    let line = |name: &str| -> Vec<u8> {
        let l = committed
            .lines()
            .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(' ')))
            .unwrap();
        (0..l.len() / 2)
            .map(|i| u8::from_str_radix(&l[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    };
    assert_eq!(
        P::decode_open_req(&line("open-req-read-skyrim")),
        Some((0, P::OPEN_READ, "Data/Skyrim.esm".to_string()))
    );
    assert_eq!(
        P::decode_getattr_resp(&line("getattr-resp-file-123")),
        Some(AttrResp {
            found: true,
            is_dir: false,
            size: 123,
            mtime: -7
        })
    );
    assert_eq!(
        P::decode_read_req(&line("read-req-fh7-off10-len4")),
        Some(ReadReq {
            fh: 7,
            offset: 10,
            len: 4
        })
    );
    assert_eq!(
        P::decode_read_resp(&line("read-resp-abcd")),
        Some(b"abcd".to_vec())
    );
    assert_eq!(
        P::decode_readdir_resp(&line("readdir-resp-two")).map(|v| v.len()),
        Some(2)
    );
    assert_eq!(P::decode_close_req(&line("close-req-99")), Some(99));
    assert_eq!(P::decode_write_resp(&line("write-resp-3")), Some(3));
    assert_eq!(
        P::decode_open_resp(&line("open-resp-immutable-gen-0xa1b2c3d4")),
        Some(OpenResp {
            fh: 42,
            size: 1000,
            is_dir: false,
            immutable: true,
            mount_gen: 0xA1B2_C3D4
        })
    );
    let cfg = decode_config(&line("shim-config-v2-one-static-import")).unwrap();
    assert_eq!(cfg.root, r"C:\GameLayers\runtime");
    assert_eq!(
        cfg.static_imports,
        vec![StaticImport {
            dll_name: "d3d11.dll".into(),
            backing_path: r"C:\GameLayers\d3d11.dll".into(),
        }]
    );
    assert_eq!(
        P::decode_names_req(&line("names-req-root2-skip1")),
        Some((2, 1, "Data/Textures".to_string()))
    );
    assert_eq!(
        P::decode_reg_set_value(&line("reg-set-value")),
        Some((r"\Registry\Machine\Software", "Name", 3, &[9u8, 8, 7][..]))
    );
    assert_eq!(
        P::decode_reg_key_reply(&line("reg-key-reply-sample-node-12")),
        Some((Some(sample_node()), 12))
    );
    assert_eq!(
        P::decode_reg_lookup_reply(&line("reg-lookup-reply-state1-below-42")),
        Some((1, true, 42))
    );
    assert_eq!(
        P::decode_reg_changed(&line("reg-changed-subtree-since77")),
        Some((r"\Registry\Machine", true, 77))
    );
}
