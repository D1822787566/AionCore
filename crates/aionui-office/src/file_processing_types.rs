use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Context supplied by the host, never accepted as model-controlled input fields.
#[derive(Clone)]
pub struct FileProcessingContext {
    pub user_id: String,
    pub conversation_id: String,
    pub task_directory: PathBuf,
    pub hssf_home: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct FileProcessingError {
    pub code: &'static str,
    pub message: String,
    pub execution: &'static str,
    pub data: Box<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PrepareInput {
    pub source_path: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InspectInput {
    pub processing_id: String,
    pub sheet: Option<String>,
    pub range: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WriteInput {
    pub processing_id: String,
    pub expected_sha256: String,
    pub changes: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FinishInput {
    pub processing_id: String,
    pub expected_sha256: String,
    pub output_name: String,
    #[serde(default)]
    pub accept_uncalculated: bool,
    #[serde(default)]
    pub checks: Vec<OfficeCheck>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OfficeCheck {
    pub selector: String,
    pub expected: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ProcessingRecord {
    pub version: u32,
    pub user_id: String,
    pub conversation_id: String,
    pub task_directory: PathBuf,
    pub source_path: PathBuf,
    pub source_sha256: String,
    pub working_sha256: String,
    pub format: String,
    pub recalculation: String,
    pub changes: Vec<Value>,
    pub final_path: Option<PathBuf>,
    pub final_sha256: Option<String>,
    #[serde(default)]
    pub accepted_uncalculated: bool,
}
