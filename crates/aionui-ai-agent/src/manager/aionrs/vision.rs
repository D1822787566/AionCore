use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aion_agent::engine::AgentEngine;
use aion_config::config::{McpServerConfig, TransportType};
use aion_mcp::manager::McpManager;
use aion_mcp::tool_proxy::register_single_server_tools;
use aion_mcp::transport::McpError;
use aion_types::message::extension_to_image_media_type;
use aionui_baitong::BaitongError;
use serde_json::Value;
use tokio::time::{Instant, sleep, timeout_at};
use tracing::{info, warn};

use crate::types::BaitongVisionConfig;

const SERVER: &str = "vision";
const TOOL: &str = "look_at_image";
const BUDGET: Duration = Duration::from_secs(60);
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VisionIssue {
    pub stage: &'static str,
    pub reason: &'static str,
}

impl VisionIssue {
    pub(super) fn detail(self) -> String {
        let reason = if self.reason == "disabled" {
            "百通视觉开关已关闭。请在百通模型服务设置中开启后重试。"
        } else {
            match self.stage {
                "configuration" => "读取百通视觉配置失败。请检查百通服务状态后重试。",
                "discovery" => "视觉服务未提供图片识别工具。请检查视觉 MCP 配置。",
                "registration" => "当前会话模式或策略未提供图片识别工具。",
                _ => "百通视觉服务连接失败或尚未就绪。请稍后重试。",
            }
        };
        format!("图片识别未完成：{reason}")
    }
}

pub(super) struct VisionRuntime {
    config: BaitongVisionConfig,
    manager: Option<Arc<McpManager>>,
    connected_config: Option<Value>,
}

pub(super) fn has_image_attachment(files: &[String]) -> bool {
    files.iter().any(|file| {
        Path::new(file)
            .extension()
            .and_then(|s| s.to_str())
            .and_then(extension_to_image_media_type)
            .is_some()
    })
}

pub(super) fn server_config(url: String) -> McpServerConfig {
    McpServerConfig {
        transport: TransportType::StreamableHttp,
        command: None,
        args: None,
        env: None,
        url: Some(url),
        headers: Some(HashMap::new()),
        deferred: Some(false),
        startup_timeout_ms: Some(BUDGET.as_millis() as u64),
    }
}

impl VisionRuntime {
    pub(super) fn new(
        config: BaitongVisionConfig,
        managers: &[Arc<McpManager>],
        servers: &HashMap<String, McpServerConfig>,
    ) -> Self {
        let manager = managers
            .iter()
            .find(|manager| manager.server_names().iter().any(|name| name == SERVER))
            .cloned();
        let connected_config = manager.as_ref().and_then(|_| servers.get(SERVER)).map(config_key);
        Self {
            config,
            manager,
            connected_config,
        }
    }

    pub(super) async fn prepare(
        &mut self,
        engine: &mut AgentEngine,
        conversation_id: &str,
        require_image: bool,
    ) -> Result<(), VisionIssue> {
        // Every message gets its own preparation budget, independent of idle time.
        // Text turns make one short attempt so image paths and ViewImage can recover too.
        let budget = if require_image { BUDGET } else { Duration::from_secs(2) };
        self.prepare_until(engine, conversation_id, require_image, Instant::now() + budget)
            .await
    }

    async fn prepare_until(
        &mut self,
        engine: &mut AgentEngine,
        conversation_id: &str,
        require_image: bool,
        deadline: Instant,
    ) -> Result<(), VisionIssue> {
        loop {
            let desired = if let Some(config) = self.config.override_config.as_ref() {
                Ok(Some(config.clone()))
            } else {
                let Some(service) = self.config.service.as_ref() else {
                    return self.unavailable(
                        engine,
                        conversation_id,
                        VisionIssue {
                            stage: "configuration",
                            reason: "host_service_missing",
                        },
                    );
                };
                match timeout_at(deadline, service.vision_configuration()).await {
                    Ok(Ok(vision)) => Ok(vision.map(|vision| server_config(vision.url))),
                    Ok(Err(error)) => {
                        let retryable = matches!(error, BaitongError::Unreachable(_));
                        Err((
                            VisionIssue {
                                stage: "configuration",
                                reason: error.code(),
                            },
                            retryable,
                        ))
                    }
                    Err(_) => Err((
                        VisionIssue {
                            stage: "configuration",
                            reason: "timeout",
                        },
                        false,
                    )),
                }
            };
            let config = match desired {
                Ok(Some(config)) => config,
                Ok(None) => {
                    return self.unavailable(
                        engine,
                        conversation_id,
                        VisionIssue {
                            stage: "configuration",
                            reason: "disabled",
                        },
                    );
                }
                Err((issue, retryable)) => {
                    if !require_image || !retryable || !wait_for_retry(deadline).await {
                        return self.unavailable(engine, conversation_id, issue);
                    }
                    continue;
                }
            };
            let key = config_key(&config);
            if self.connected_config.as_ref() == Some(&key)
                && let Some(manager) = self.manager.clone()
            {
                let live = if Instant::now() < deadline {
                    match timeout_at(deadline, manager.list_server_tools(SERVER)).await {
                        Ok(Ok(tools)) => Some(tools.iter().any(|tool| tool.name == TOOL)),
                        Ok(Err(error))
                            if !retryable_mcp_error(&error) && !matches!(error, McpError::HttpStatus(404)) =>
                        {
                            return self.unavailable(
                                engine,
                                conversation_id,
                                VisionIssue {
                                    stage: "connection",
                                    reason: "connection_rejected",
                                },
                            );
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                if live == Some(true) {
                    if engine.registry_mut().mcp_tool_name(SERVER, TOOL).is_none() {
                        engine.registry_mut().remove_mcp_server(SERVER);
                        let names = engine.tool_names();
                        register_single_server_tools(engine.registry_mut(), &manager, SERVER, &names, false);
                    }
                    return self.validate(engine, conversation_id);
                }
                if live == Some(false) {
                    return self.unavailable(
                        engine,
                        conversation_id,
                        VisionIssue {
                            stage: "discovery",
                            reason: "missing_tool",
                        },
                    );
                }
            }
            if Instant::now() >= deadline {
                return self.unavailable(
                    engine,
                    conversation_id,
                    VisionIssue {
                        stage: "connection",
                        reason: "timeout",
                    },
                );
            }
            let configs = HashMap::new();
            let mut manager = McpManager::connect_all(&configs).await.map_err(|_| VisionIssue {
                stage: "connection",
                reason: "initialization",
            })?;
            let last_issue = match timeout_at(deadline, manager.connect_one(SERVER.to_owned(), &config)).await {
                Ok(Ok(_)) => {
                    if !manager
                        .all_tools()
                        .iter()
                        .any(|(server, tool)| *server == SERVER && tool.name == TOOL)
                    {
                        return self.unavailable(
                            engine,
                            conversation_id,
                            VisionIssue {
                                stage: "discovery",
                                reason: "missing_tool",
                            },
                        );
                    }
                    let manager = Arc::new(manager);
                    engine.registry_mut().remove_mcp_server(SERVER);
                    let names = engine.tool_names();
                    register_single_server_tools(engine.registry_mut(), &manager, SERVER, &names, false);
                    self.manager = Some(manager);
                    self.connected_config = Some(key);
                    return self.validate(engine, conversation_id);
                }
                Ok(Err(error)) => {
                    let issue = VisionIssue {
                        stage: "connection",
                        reason: match &error {
                            McpError::HttpStatus(401 | 403) => "authorization",
                            McpError::HttpStatus(404) => "invalid_endpoint",
                            McpError::InitFailed(_) => "invalid_configuration",
                            _ => "connection_failed",
                        },
                    };
                    if !retryable_mcp_error(&error) {
                        return self.unavailable(engine, conversation_id, issue);
                    }
                    issue
                }
                Err(_) => {
                    return self.unavailable(
                        engine,
                        conversation_id,
                        VisionIssue {
                            stage: "connection",
                            reason: "timeout",
                        },
                    );
                }
            };
            if !require_image || !wait_for_retry(deadline).await {
                return self.unavailable(engine, conversation_id, last_issue);
            }
        }
    }

    fn validate(&self, engine: &mut AgentEngine, conversation_id: &str) -> Result<(), VisionIssue> {
        let exposed = engine.registry_mut().mcp_tool_name(SERVER, TOOL);
        let available = exposed.as_ref().is_some_and(|name| engine.is_tool_advertised(name));
        info!(conversation_id, desired_enabled = true, configuration_source = if self.config.override_config.is_some() { "user_override" } else { "saved_settings" },
            connection_state = "connected", discovered_tool = TOOL, exposed_tool = ?exposed, effective_available = available,
            "Baitong vision tool availability");
        if available {
            Ok(())
        } else {
            warn!(
                conversation_id,
                effective_available = false,
                failure_stage = "registration",
                failure_reason = "tool_filtered",
                "Baitong vision preparation unavailable"
            );
            Err(VisionIssue {
                stage: "registration",
                reason: "tool_filtered",
            })
        }
    }

    fn unavailable(
        &mut self,
        engine: &mut AgentEngine,
        conversation_id: &str,
        issue: VisionIssue,
    ) -> Result<(), VisionIssue> {
        engine.registry_mut().remove_mcp_server(SERVER);
        self.manager = None;
        self.connected_config = None;
        warn!(
            conversation_id,
            desired_enabled = if issue.reason == "disabled" {
                Some(false)
            } else if issue.stage != "configuration" {
                Some(true)
            } else {
                None
            },
            configuration_source = if self.config.override_config.is_some() {
                "user_override"
            } else {
                "saved_settings"
            },
            effective_available = false,
            failure_stage = issue.stage,
            failure_reason = issue.reason,
            "Baitong vision preparation unavailable"
        );
        Err(issue)
    }
}

fn config_key(config: &McpServerConfig) -> Value {
    // Runtime identity is never logged; user overrides may contain HTTP headers.
    serde_json::json!({"transport":config.transport,"url":config.url,"headers":config.headers,"command":config.command,"args":config.args,"env":config.env})
}

fn retryable_mcp_error(error: &McpError) -> bool {
    matches!(
        error,
        McpError::Transport(_) | McpError::Io(_) | McpError::HttpStatus(408 | 429 | 500..=599)
    )
}

async fn wait_for_retry(deadline: Instant) -> bool {
    if deadline.saturating_duration_since(Instant::now()) <= RETRY_INTERVAL {
        return false;
    }
    sleep(RETRY_INTERVAL).await;
    true
}

#[cfg(test)]
#[path = "vision_test.rs"]
mod vision_test;
