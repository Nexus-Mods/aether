//! `contract/protocol.json`, the protocol table as data for directors in other
//! languages, is what the code generates, and agrees with the table.

use aether_protocol::{contract_json, Opcode, Status};

const PROTOCOL_JSON: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contract/protocol.json");

#[test]
fn checked_in_protocol_json_is_current() {
    // UPDATE_CONTRACT=1 writes it, after a deliberate change to the table
    if std::env::var_os("UPDATE_CONTRACT").is_some() {
        std::fs::write(PROTOCOL_JSON, contract_json()).unwrap();
    }
    let on_disk = std::fs::read_to_string(PROTOCOL_JSON).unwrap_or_default();
    assert_eq!(
        on_disk,
        contract_json(),
        "contract/protocol.json is not the protocol table; regenerate it with \
         UPDATE_CONTRACT=1 cargo test -p aether-protocol --test contract"
    );
}

#[test]
fn wire_version_is_the_rings() {
    let v: serde_json::Value = serde_json::from_str(&contract_json()).unwrap();
    assert_eq!(
        v["wire_version"].as_u64(),
        Some(u64::from(aether_ipc::layout::VERSION))
    );
}

#[test]
fn the_json_lists_the_table() {
    let v: serde_json::Value = serde_json::from_str(&contract_json()).unwrap();
    let ops = v["opcodes"].as_array().unwrap();
    assert_eq!(ops.len(), Opcode::ALL.len());
    for (o, op) in ops.iter().zip(Opcode::ALL) {
        assert_eq!(o["name"].as_str(), Some(op.name()));
        assert_eq!(o["number"].as_u64(), Some(u64::from(*op as u32)));
        assert!(o["request"].is_array() && o["reply"].is_array());
    }
    assert_eq!(v["reserved_opcodes"], serde_json::json!([4, 12]));
    let sts = v["statuses"].as_array().unwrap();
    assert_eq!(sts.len(), Status::ALL.len());
    for (s, st) in sts.iter().zip(Status::ALL) {
        assert_eq!(s["name"].as_str(), Some(st.name()));
        assert_eq!(s["number"].as_i64(), Some(i64::from(st.code())));
    }
}

#[test]
fn every_field_has_a_name_and_a_type() {
    fn check(fields: &serde_json::Value, at: &str) {
        for f in fields
            .as_array()
            .unwrap_or_else(|| panic!("{at}: not a list"))
        {
            assert!(f["name"].is_string(), "{at}: field without a name: {f}");
            assert!(f["type"].is_string(), "{at}: field without a type: {f}");
            for key in ["of", "node", "inline", "bulk"] {
                if f.get(key).is_some() {
                    check(&f[key], &format!("{at}.{}", f["name"]));
                }
            }
        }
    }
    let v: serde_json::Value = serde_json::from_str(&contract_json()).unwrap();
    for o in v["opcodes"].as_array().unwrap() {
        check(&o["request"], o["name"].as_str().unwrap());
        check(&o["reply"], o["name"].as_str().unwrap());
    }
}
