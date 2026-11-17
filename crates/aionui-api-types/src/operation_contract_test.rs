use super::*;
use std::collections::BTreeSet;

#[test]
fn catalog_has_unique_explicit_input_contracts() {
    let catalog = aion_operation_contract();
    assert_eq!(catalog["contract_version"], 1);
    let entries = catalog["operations"].as_array().unwrap();
    let mut names = BTreeSet::new();
    for entry in entries {
        assert!(
            names.insert(entry["operation"].as_str().unwrap()),
            "duplicate operation"
        );
        assert!(entry["input_schema"].is_object());
        assert!(entry["examples"].is_array());
    }
    assert_eq!(names.len(), 102);
    let mcp = entries
        .iter()
        .find(|d| d["operation"] == "config.mcp.servers.get")
        .unwrap();
    assert_eq!(mcp["input_schema"]["required"], json!(["server_id"]));
    assert_eq!(mcp["requires_confirmation"], false);
    let import = entries
        .iter()
        .find(|d| d["operation"] == "config.assistants.import")
        .unwrap();
    assert_eq!(import["input_schema"]["required"], json!(["assistants"]));
}
