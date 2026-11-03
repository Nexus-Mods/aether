//! The contract files at the repository root (`contract/`), which a
//! release ships for directors to check themselves against, agree with the
//! code: `env.json` is the handshake tables, `artifacts.json`'s wire version
//! is the ring's.

const ENV_JSON: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contract/env.json");
const ARTIFACTS_JSON: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contract/artifacts.json");

#[test]
fn env_json_is_the_tables() {
    // UPDATE_CONTRACT=1 writes it, after a deliberate change to the tables
    if std::env::var_os("UPDATE_CONTRACT").is_some() {
        std::fs::write(ENV_JSON, aether_env::contract_json()).unwrap();
    }
    let on_disk = std::fs::read_to_string(ENV_JSON).unwrap_or_default();
    assert_eq!(
        on_disk,
        aether_env::contract_json(),
        "contract/env.json is not the handshake tables (UPDATE_CONTRACT=1 rewrites it)"
    );
}

#[test]
fn env_json_lists_each_table_in_order() {
    let v: serde_json::Value = serde_json::from_str(&aether_env::contract_json()).unwrap();
    let names = |k: &str| -> Vec<String> {
        v[k].as_array()
            .unwrap_or_else(|| panic!("no {k} array"))
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names("transport"), aether_env::handshake::TRANSPORT);
    assert_eq!(names("inject"), aether_env::handshake::INJECT);
    assert_eq!(names("config"), aether_env::handshake::CONFIG);
}

#[test]
fn artifacts_json_wire_version_is_the_rings() {
    let text = std::fs::read_to_string(ARTIFACTS_JSON).expect("contract/artifacts.json");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["wire_version"].as_u64(),
        Some(u64::from(aether_ipc::layout::VERSION))
    );
    for arch in ["x64", "x86"] {
        for role in ["injector", "shim"] {
            assert!(
                v["natives"][arch][role].is_string(),
                "artifacts.json has no natives.{arch}.{role}"
            );
        }
    }
}
