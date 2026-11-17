use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use aion_mcp::manager::McpManager;
use aion_mcp::transport::McpError;
use aion_protocol::events::ToolCategory;
use aion_tools::{Tool, view_image::ViewImageTool};
use aion_types::tool::{JsonSchema, ToolResult};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Captured only from the host's resolved attachment list, never from chat text.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Attachment {
    canonical: PathBuf,
}

#[derive(Default)]
pub(super) struct ImageScope {
    pub id: String,
    pub attachments: HashMap<String, Attachment>,
}

impl ImageScope {
    pub fn register(&mut self, files: &[String]) {
        for file in files {
            let path = Path::new(file);
            if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
                continue;
            }
            if let Ok(canonical) = path.canonicalize() {
                self.attachments.insert(file.clone(), Attachment { canonical });
            }
        }
    }
}

pub(super) struct VisionTool {
    pub name: String,
    pub manager: Arc<McpManager>,
    pub scope: Arc<RwLock<ImageScope>>,
}

impl VisionTool {
    async fn recognize(&self, input: &Value) -> Result<String, &'static str> {
        let path = input["file_path"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or("image_argument_required")?;
        let question = input["question"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or("image_argument_required")?;
        let requested = Path::new(path);
        if !requested.is_absolute() || requested.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err("image_not_in_scope");
        }
        let (scope_id, attachment) = {
            let scope = self.scope.read().map_err(|_| "image_not_in_scope")?;
            (
                scope.id.clone(),
                scope.attachments.get(path).cloned().ok_or("image_not_in_scope")?,
            )
        };
        let canonical = tokio::fs::canonicalize(requested)
            .await
            .map_err(|_| "image_not_found")?;
        if canonical != attachment.canonical {
            return Err("image_not_in_scope");
        }
        // Reuse ViewImage's size, file-signature and extension checks, without
        // appending the binary payload to provider history.
        let image = ViewImageTool::new()
            .load_image(&json!({"file_path": canonical}))
            .await
            .map_err(|error| {
                if error.contains("size limit") {
                    "image_too_large"
                } else {
                    "image_invalid"
                }
            })?;
        if tokio::fs::canonicalize(requested)
            .await
            .map_err(|_| "image_not_found")?
            != attachment.canonical
        {
            return Err("image_not_in_scope");
        }
        let (_, encoded) = image.url.split_once(',').ok_or("image_invalid")?;
        let bytes = STANDARD.decode(encoded).map_err(|_| "image_invalid")?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let response = self
            .manager
            .call_tool_cancellable(
                "vision",
                "look_at_image",
                json!({
                    "image_data_url": image.url, "question": question, "scope_id": scope_id
                }),
            )
            .await
            .map_err(|error| match error {
                McpError::ToolExecution(text) => [
                    "image_upload_failed",
                    "image_auth_failed",
                    "image_too_large",
                    "image_invalid",
                    "image_argument_required",
                    "image_cancelled",
                ]
                .into_iter()
                .find(|code| text.contains(code))
                .unwrap_or("image_recognition_failed"),
                _ => "image_recognition_failed",
            })?;
        let result: Value = serde_json::from_str(&response).map_err(|_| "vision_contract_mismatch")?;
        if result["sha256"].as_str() != Some(digest.as_str())
            || result["answer"].as_str().is_none_or(|s| s.trim().is_empty())
        {
            return Err("vision_contract_mismatch");
        }
        tracing::info!(image_sha256 = %digest, "Baitong image recognition completed");
        Ok(json!({"attachment": path, "sha256": digest, "answer": result["answer"]}).to_string())
    }
}

#[cfg(test)]
#[path = "vision_tool_test.rs"]
mod vision_tool_test;

#[async_trait]
impl Tool for VisionTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Analyze a verified image attachment in this conversation. Supply its absolute file_path and question. For 'this image', use the current turn's attachment, unless the user explicitly refers to an older image. Compare multiple images with one call per image. Clarify ambiguous references. Reads, uploads if necessary, and recognizes the specified bytes in one call; never selects an image from cache. No ViewImage prerequisite."
    }
    fn input_schema(&self) -> JsonSchema {
        json!({"type":"object","properties":{"file_path":{"type":"string","minLength":1},"question":{"type":"string","minLength":1}},"required":["file_path","question"],"additionalProperties":false})
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    async fn execute(&self, input: Value) -> ToolResult {
        match tokio::time::timeout(Duration::from_secs(300), self.recognize(&input)).await {
            Ok(Ok(content)) => ToolResult {
                content,
                is_error: false,
            },
            Ok(Err(code)) => ToolResult {
                content: code.into(),
                is_error: true,
            },
            Err(_) => ToolResult {
                content: "image_recognition_failed: timeout".into(),
                is_error: true,
            },
        }
    }
    fn mcp_origin(&self) -> Option<(&str, &str)> {
        Some(("vision", "look_at_image"))
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Mcp
    }
    fn describe(&self, _: &Value) -> String {
        "Recognize the specified conversation image".into()
    }
}
