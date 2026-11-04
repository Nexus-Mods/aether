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

#[test]
fn the_json_defines_every_type_and_attribute_it_uses() {
    fn used(fields: &serde_json::Value, types: &mut Vec<String>, attrs: &mut Vec<String>) {
        for f in fields.as_array().unwrap() {
            types.push(f["type"].as_str().unwrap().to_string());
            for (k, v) in f.as_object().unwrap() {
                if !["name", "type"].contains(&k.as_str()) {
                    attrs.push(k.clone());
                }
                if v.is_array() {
                    used(v, types, attrs);
                }
            }
        }
    }
    let v: serde_json::Value = serde_json::from_str(&contract_json()).unwrap();
    let (mut types, mut attrs) = (Vec::new(), Vec::new());
    for o in v["opcodes"].as_array().unwrap() {
        used(&o["request"], &mut types, &mut attrs);
        used(&o["reply"], &mut types, &mut attrs);
    }
    let defined = |section: &str| -> Vec<String> {
        v[section]
            .as_array()
            .unwrap_or_else(|| panic!("no \"{section}\" section"))
            .iter()
            .map(|t| {
                assert!(t["wire"].as_str().is_some_and(|w| !w.is_empty()));
                t["name"].as_str().unwrap().to_string()
            })
            .collect()
    };
    let (dt, da) = (defined("types"), defined("attributes"));
    for t in &types {
        assert!(dt.contains(t), "type {t} is not defined in \"types\"");
    }
    for a in &attrs {
        assert!(
            da.contains(a),
            "attribute {a} is not defined in \"attributes\""
        );
    }
    // nothing defined that no message uses
    for t in &dt {
        assert!(types.contains(t), "type {t} is defined but unused");
    }
    for a in &da {
        assert!(attrs.contains(a), "attribute {a} is defined but unused");
    }
    // flag lists hold masks
    for list in ["open_flags", "request_flags"] {
        for f in v[list].as_array().unwrap() {
            assert!(
                f.get("mask").is_some() && f.get("bit").is_none(),
                "{list}: {f}"
            );
        }
    }
}

#[test]
fn the_wire_document_lists_the_json_types() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/wire.md"))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&contract_json()).unwrap();
    for t in v["types"].as_array().unwrap() {
        let row = format!(
            "| `{}` | {} |",
            t["name"].as_str().unwrap(),
            t["wire"].as_str().unwrap()
        );
        assert!(doc.contains(&row), "docs/wire.md 3.3 lacks the row {row}");
    }
}
