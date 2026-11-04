//! The two shapes whose descriptions are written by hand (`ReadReply` in
//! `table.rs`, `OptNode` in `field.rs`) lay their bytes out as
//! `contract/protocol.json` says: a small reader that follows only the
//! described fields, sizes and attributes consumes exactly what the encoder
//! wrote.

mod common;

use aether_protocol::{contract_json, KeyReply, Opcode, ReadReply, Reply};
use common::sample_node;
use serde_json::Value;
use std::borrow::Cow;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Bytes the described `fields` take at the start of `b`.
fn walk(fields: &Value, b: &[u8]) -> usize {
    let mut at = 0;
    for f in fields.as_array().unwrap() {
        let name = f["name"].as_str().unwrap();
        at += match f["type"].as_str().unwrap() {
            "u32" | "root_id" | "open_flags" => 4,
            "u64" | "i64" => 8,
            "bool" | "bool_lenient" | "bool_bit0" | "lookup_state" | "child_state" => 1,
            "pad" => f["size"].as_u64().unwrap() as usize,
            "str_u32" => 4 + u32_at(b, at) as usize,
            "bytes_u32" => {
                4 + f["pad_after_len"].as_u64().unwrap_or(0) as usize + u32_at(b, at) as usize
            }
            "list_u32" => {
                let mut n = 4;
                for _ in 0..u32_at(b, at) {
                    n += walk(&f["of"], &b[at + n..]);
                }
                n
            }
            "optional_node" => match b[at] {
                0 => 1,
                1 => 1 + walk(&f["node"], &b[at + 1..]),
                v => panic!("{name}: has_node {v}"),
            },
            "inline_or_bulk" => {
                let bit = f["bulk_bit"].as_u64().unwrap() as u32;
                let branch = if u32_at(b, at) & bit != 0 {
                    "bulk"
                } else {
                    "inline"
                };
                walk(&f[branch], &b[at..])
            }
            other => panic!("{name}: unknown type {other}"),
        };
    }
    at
}

fn reply_fields(op: Opcode) -> Value {
    let v: Value = serde_json::from_str(&contract_json()).unwrap();
    let o = v["opcodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["number"].as_u64() == Some(u64::from(op as u32)))
        .unwrap();
    o["reply"].clone()
}

fn encode(r: &Reply) -> Vec<u8> {
    let mut b = Vec::new();
    r.encode(&mut b);
    b
}

#[test]
fn a_read_reply_has_the_described_layout() {
    let fields = reply_fields(Opcode::Read);
    let inline = encode(&Reply::Read(ReadReply::Inline(&[1, 2, 3, 4, 5])));
    // len:u32, 4 pad bytes, the data
    assert_eq!(inline.len(), 4 + 4 + 5);
    assert_eq!(walk(&fields, &inline), inline.len());

    let bulk = encode(&Reply::Read(ReadReply::Bulk {
        len: 4096,
        arena_offset: 0x1234,
    }));
    // len|bit31:u32, 4 pad bytes, arena_offset:u64
    assert_eq!(bulk.len(), 4 + 4 + 8);
    assert_eq!(u32_at(&bulk, 0), 4096 | 0x8000_0000);
    assert_eq!(walk(&fields, &bulk), bulk.len());
}

#[test]
fn a_reg_key_reply_has_the_described_layout() {
    let fields = reply_fields(Opcode::RegKey);
    let none = encode(&Reply::RegKey(KeyReply {
        version: 7,
        node: None,
    }));
    assert_eq!(none.len(), 8 + 1);
    assert_eq!(walk(&fields, &none), none.len());

    let node = sample_node();
    let some = encode(&Reply::RegKey(KeyReply {
        version: 12,
        node: Some(Cow::Borrowed(&node)),
    }));
    // version, has_node, created, volatile, last_write, then three counted lists
    assert!(some.len() > 8 + 1 + 2 + 8 + 12);
    assert_eq!(walk(&fields, &some), some.len());
}
