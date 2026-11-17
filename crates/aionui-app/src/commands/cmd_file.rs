//! Local file commands use the host-frozen directory, not this helper's cwd.
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use aionui_office::{FileProcessingContext, execute_file_operation, resolve_hssf_home};
use serde_json::{Value, json};

use crate::cli::{FileArgs, FileCommand, FileXlsCommand};

pub(crate) async fn run_file(args: FileArgs) -> ExitCode {
    let operation = match args.command {
        FileCommand::Prepare => "file.prepare",
        FileCommand::Inspect => "file.inspect",
        FileCommand::Finish => "file.finish",
        FileCommand::Xls {
            command: FileXlsCommand::Write,
        } => "file.xls.write",
    };
    let result = run(operation).await;
    let success = result["success"] == true;
    if !success {
        // No file contents, input payloads or identity tokens are logged.
        eprintln!(
            "FILE_OPERATION_FAILED operation={operation} code={}",
            result["error"]["code"]
        );
    }
    let mut stdout = io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &result).is_err() || stdout.write_all(b"\n").is_err() {
        return ExitCode::from(1);
    }
    if success { ExitCode::SUCCESS } else { ExitCode::from(2) }
}

fn rejected(code: &str, message: &str, details: Value) -> Value {
    json!({"success":false,"error":{"code":code,"message":message,"details":details},
        "execution":{"status":"not_executed","stage":"preflight","retry_write":false}})
}

async fn run(operation: &str) -> Value {
    let mut bytes = Vec::new();
    if io::stdin().take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes).is_err() || bytes.len() > 8 * 1024 * 1024 {
        return rejected("file_input_invalid", "stdin must be JSON of at most 8 MiB", json!({}));
    }
    let input: Value = match serde_json::from_slice(&bytes) {
        Ok(input) => input,
        Err(_) => return rejected("file_input_invalid", "stdin must contain a JSON object", json!({})),
    };
    let catalog = aionui_api_types::aion_operation_contract();
    let Some(descriptor) = catalog["operations"]
        .as_array()
        .and_then(|ops| ops.iter().find(|d| d["operation"] == operation))
    else {
        return rejected("file_operation_unknown", "file operation has no contract", json!({}));
    };
    if let Err(errors) = aion_tools::validation::validate_tool_input(&descriptor["input_schema"], &input) {
        let errors: Vec<Value> = errors
            .into_iter()
            .map(|e| {
                json!({"path":e.path,"expected":e.expected,
            "actual":e.actual,"message":e.message})
            })
            .collect();
        return rejected(
            "schema_validation_failed",
            "stdin does not match the file operation contract",
            json!({"errors":errors}),
        );
    }
    let vars: Result<Vec<String>, _> = ["AIONUI_USER_ID", "AIONUI_CONVERSATION_ID", "AIONUI_TASK_WORK_DIR"]
        .iter()
        .map(std::env::var)
        .collect();
    let vars = match vars {
        Ok(vars) if vars.iter().all(|v| !v.is_empty()) => vars,
        _ => {
            return rejected(
                "file_context_missing",
                "host user, conversation and frozen task directory are required",
                json!({}),
            );
        }
    };
    let context = FileProcessingContext {
        user_id: vars[0].clone(),
        conversation_id: vars[1].clone(),
        task_directory: PathBuf::from(&vars[2]),
        hssf_home: resolve_hssf_home(),
    };
    match execute_file_operation(&context, operation, input).await {
        Ok(data) => {
            json!({"success":true,"data":data,"execution":{"status":"succeeded","stage":"file_operation","retry_write":false}})
        }
        Err(error) => json!({"success":false,"error":{"code":error.code,"message":error.message},
            "data":error.data,"execution":{"status":error.execution,"stage":"file_operation","retry_write":false}}),
    }
}
