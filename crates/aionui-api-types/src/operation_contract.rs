//! Immutable public helper-input catalog, shared by capabilities and host injection.
use crate::{conversation_tool_descriptors, session_tool_descriptors, team_tool_descriptors};
use serde_json::{Value, json};

pub const AION_OPERATION_CONTRACT_VERSION: u32 = 1;

pub fn aion_operation_contract() -> Value {
    let mut operations = Vec::new();
    let config = crate::config_operation_capabilities();
    for domain in config["domains"].as_array().expect("static domains") {
        for descriptor in domain["commands"].as_array().expect("static commands") {
            let name = descriptor["command"]
                .as_str()
                .expect("static command")
                .replace(' ', ".");
            let mut entry = descriptor.clone();
            entry["operation"] = json!(name);
            entry["contract_version"] = json!(AION_OPERATION_CONTRACT_VERSION);
            entry["input_schema"] = descriptor["stdin_json_schema"].clone();
            // Keep the legacy config-capabilities shape at its CLI boundary,
            // without duplicating the schema or a partial field list in describe.
            entry
                .as_object_mut()
                .expect("static descriptor")
                .remove("stdin_json_schema");
            entry.as_object_mut().expect("static descriptor").remove("stdin_fields");
            entry["requires_confirmation"] = json!(requires_confirmation(&name));
            operations.push(entry);
        }
    }
    for d in session_tool_descriptors() {
        operations.push(entry(
            &format!("session.{}", d.cli_command.join(".")),
            &d.description,
            d.input_schema,
        ));
    }
    for d in team_tool_descriptors() {
        operations.push(entry(
            &format!("team.{}", d.cli_command.join(".")),
            &d.description,
            d.input_schema,
        ));
    }
    for d in conversation_tool_descriptors() {
        operations.push(entry(
            &format!("conversation.{}", d.cli_command.join(".")),
            &d.description,
            d.input_schema,
        ));
    }
    for name in [
        "capabilities",
        "session.capabilities",
        "team.capabilities",
        "team.help",
        "team.context",
        "conversation.capabilities",
        "skills.capabilities",
        "skills.list",
    ] {
        operations.push(entry(
            name,
            "Read helper capabilities or current context.",
            json!({"type":"object","properties":{}}),
        ));
    }
    for (name, field) in [("skills.show", "name"), ("skills.cat", "path")] {
        operations.push(entry(
            name,
            "Read a runtime skill or a file within it.",
            json!({"type":"object","required":[field],"properties":{field:{"type":"string","minLength":1}}}),
        ));
    }
    json!({"contract_version":AION_OPERATION_CONTRACT_VERSION,"operations":operations})
}

fn entry(name: &str, description: &str, schema: Value) -> Value {
    json!({"operation":name,"contract_version":AION_OPERATION_CONTRACT_VERSION,"description":description,"examples":[example_input(&schema)],"input_schema":schema,"selectors":[],"requires_confirmation":requires_confirmation(name)})
}

fn requires_confirmation(name: &str) -> bool {
    matches!(
        name.rsplit('.').next().unwrap_or(name),
        "create"
            | "update"
            | "delete"
            | "import"
            | "write"
            | "state"
            | "toggle"
            | "login"
            | "logout"
            | "enable"
            | "disable"
            | "add"
            | "remove"
            | "patch"
            | "put"
            | "save"
            | "run"
            | "send-message"
            | "rename"
            | "spawn-agent"
            | "interrupt-agent"
            | "shutdown-agent"
            | "clear-agent-context"
    )
}

pub(super) fn example_input(schema: &Value) -> Value {
    if let Some(value) = schema.get("const") {
        return value.clone();
    }
    if let Some(value) = schema.get("enum").and_then(|v| v.get(0)) {
        return value.clone();
    }
    if let Some(branch) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(|v| v.get(0))
    {
        return example_input(branch);
    }
    match schema["type"].as_str().unwrap_or("object") {
        "string" => json!("example"),
        "integer" | "number" => schema.get("minimum").cloned().unwrap_or(json!(1)),
        "boolean" => json!(false),
        "null" => Value::Null,
        "array" => json!([example_input(&schema["items"])]),
        _ => {
            let mut value = serde_json::Map::new();
            for key in schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                value.insert(key.into(), example_input(&schema["properties"][key]));
            }
            Value::Object(value)
        }
    }
}

#[cfg(test)]
#[path = "operation_contract_test.rs"]
mod tests;
