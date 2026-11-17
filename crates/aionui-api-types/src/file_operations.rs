//! File-operation input contracts. The host supplies identity and the frozen task directory.
use serde_json::{Value, json};

pub(super) fn file_operation_entries() -> Vec<Value> {
    let text = || json!({"type":"string","minLength":1});
    let changes: Vec<Value> = ["string", "number", "boolean", "date", "blank", "formula"]
        .into_iter()
        .map(|kind| {
            let mut fields = json!({"sheet":text(),"cell":text(),"type":{"const":kind}});
            let mut required = vec!["sheet", "cell", "type"];
            if kind != "blank" {
                fields["value"] = json!({"type":match kind {"number"=>"number","boolean"=>"boolean",_=>"string"}});
                required.push("value");
            }
            json!({"type":"object","additionalProperties":false,"required":required,"properties":fields})
        })
        .collect();
    let mut result = Vec::new();
    let definitions = [
        (
            "file.prepare",
            "Prepare a complete local working file in this task's frozen session directory. Preserve the original.",
            json!({"source_path":text()}),
            vec!["source_path"],
        ),
        (
            "file.inspect",
            "Inspect processing state and hashes. For XLS, optionally read a sheet and A1 range (at most 10000 cells).",
            json!({"processing_id":text(),"sheet":text(),"range":text()}),
            vec!["processing_id"],
        ),
        (
            "file.xls.write",
            "Write explicitly typed XLS cells, preserve formatting and other cells, recalculate, save and reopen. Inspect after timeout; never replay blindly.",
            json!({
                "processing_id":text(),"expected_sha256":text(),
                "changes":{"type":"array","minItems":1,"items":{"oneOf":changes}}
            }),
            vec!["processing_id", "expected_sha256", "changes"],
        ),
        (
            "file.finish",
            "Verify and deliver a final file outside the temporary directory, then clean only this processing record's files. Modern formats require checks from OfficeCLI get output; expected is a JSON subset. accept_uncalculated requires the user's explicit choice for this file.",
            json!({
                "processing_id":text(),"expected_sha256":text(),"output_name":text(),
                "accept_uncalculated":{"type":"boolean"},
                "checks":{"type":"array","items":{
                    "type":"object","additionalProperties":false,"required":["selector","expected"],
                    "properties":{"selector":text(),"expected":{"type":"object"}}
                }}
            }),
            vec!["processing_id", "expected_sha256", "output_name"],
        ),
    ];
    for (name, description, properties, required) in definitions {
        let schema = json!({"type":"object","additionalProperties":false,"properties":properties,"required":required});
        result.push(super::operation_contract::entry(name, description, schema));
    }
    result
}
