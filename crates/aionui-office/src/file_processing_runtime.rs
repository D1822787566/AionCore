use aionui_runtime::Builder as CmdBuilder;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use super::file_processing_types::{FileProcessingError, OfficeCheck};
use super::officecli_runtime::resolve_officecli_path;

pub(super) fn failure(code: &'static str, message: impl Into<String>) -> FileProcessingError {
    FileProcessingError {
        code,
        message: message.into(),
        execution: "not_executed",
        data: Box::new(json!({})),
    }
}

pub(super) async fn hssf(home: Option<&Path>, input: Value) -> Result<Value, FileProcessingError> {
    let home = home.ok_or_else(|| failure("hssf_runtime_missing", "bundled HSSF runtime is not configured"))?;
    let java = home
        .join("runtime")
        .join("bin")
        .join(if cfg!(windows) { "java.exe" } else { "java" });
    if !java.is_file() || !home.join("classes/HssfBridge.class").is_file() {
        return Err(failure(
            "hssf_runtime_missing",
            "bundled Java or HSSF bridge is missing",
        ));
    }
    let classpath = std::env::join_paths([home.join("classes"), home.join("lib/*")])
        .map_err(|_| failure("hssf_runtime_invalid", "invalid HSSF resource path"))?;
    let mut command = CmdBuilder::clean_cli(java);
    command.args([
        "-Xmx512m",
        "-Djava.awt.headless=true",
        "-Dfile.encoding=UTF-8",
        "-Dlog4j2.loggerContextFactory=org.apache.logging.log4j.simple.SimpleLoggerContextFactory",
    ]);
    command
        .arg("-cp")
        .arg(classpath)
        .arg("HssfBridge")
        .stdin(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| failure("hssf_start_failed", "HSSF helper did not start"))?;
    let payload = serde_json::to_vec(&input).map_err(|_| failure("hssf_input_invalid", "invalid helper input"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| failure("hssf_stdin_failed", "HSSF stdin missing"))?;
    stdin
        .write_all(&payload)
        .await
        .map_err(|_| failure("hssf_stdin_failed", "HSSF input write failed"))?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
        .await
        .map_err(|_| failure("hssf_timeout", "HSSF timed out; inspect state before continuing"))?
        .map_err(|_| failure("hssf_execution_failed", "HSSF process failed"))?;
    let result: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| failure("hssf_output_invalid", "HSSF returned invalid JSON"))?;
    if !output.status.success() || result["success"] != true {
        return Err(failure(
            "hssf_operation_failed",
            result["error"]["message"].as_str().unwrap_or("HSSF operation failed"),
        ));
    }
    Ok(result["data"].clone())
}

async fn officecli(args: &[String]) -> Result<Value, FileProcessingError> {
    let path = resolve_officecli_path().ok_or_else(|| failure("officecli_missing", "OfficeCLI is not available"))?;
    let mut command = CmdBuilder::clean_cli(path);
    command
        .args(args)
        .env("OFFICECLI_NO_AUTO_RESIDENT", "1")
        .env("OFFICECLI_SKIP_UPDATE", "1");
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| failure("officecli_timeout", "OfficeCLI timed out"))?
        .map_err(|_| failure("officecli_execution_failed", "OfficeCLI did not run"))?;
    if !output.status.success() {
        return Err(failure(
            "officecli_verification_failed",
            "OfficeCLI could not close or reopen the file",
        ));
    }
    let result: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| failure("officecli_output_invalid", "OfficeCLI returned invalid JSON"))?;
    if result.get("success") == Some(&Value::Bool(false)) || result.get("error").is_some_and(|v| !v.is_null()) {
        return Err(failure(
            "officecli_verification_failed",
            "OfficeCLI reported a document error",
        ));
    }
    Ok(result)
}

pub(super) async fn close_officecli(path: &Path) -> Result<(), FileProcessingError> {
    officecli(&["close".into(), path.to_string_lossy().into_owned(), "--json".into()]).await?;
    Ok(())
}

pub(super) async fn verify_officecli(path: &Path, checks: &[OfficeCheck]) -> Result<Value, FileProcessingError> {
    let structure = officecli(&[
        "get".into(),
        path.to_string_lossy().into_owned(),
        "/".into(),
        "--depth".into(),
        "0".into(),
        "--json".into(),
    ])
    .await?;
    for check in checks {
        if !check.selector.starts_with('/') || check.expected.as_object().is_none_or(|v| v.is_empty()) {
            return Err(failure(
                "file_checks_invalid",
                "each check requires a DOM selector and a nonempty expected JSON subset",
            ));
        }
        let actual = officecli(&[
            "get".into(),
            path.to_string_lossy().into_owned(),
            check.selector.clone(),
            "--json".into(),
        ])
        .await?;
        if !subset(&actual, &check.expected) {
            return Err(failure(
                "file_readback_mismatch",
                format!("OfficeCLI readback did not match selector {}", check.selector),
            ));
        }
    }
    Ok(json!({"readback":"passed","checked_nodes":checks.len(),"structure_opened":!structure.is_null()}))
}

fn subset(actual: &Value, expected: &Value) -> bool {
    match expected {
        Value::Object(fields) => fields
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|v| subset(v, value))),
        _ => actual == expected,
    }
}

pub fn resolve_hssf_home() -> Option<PathBuf> {
    std::env::var_os("AIONUI_HSSF_HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readback_compares_requested_fields_and_does_not_accept_missing_nodes() {
        assert!(subset(&json!({"text":"new","other":1}), &json!({"text":"new"})));
        assert!(!subset(&json!({"text":"old"}), &json!({"text":"new"})));
        assert!(!subset(&json!({}), &json!({"text":"new"})));
    }
}
