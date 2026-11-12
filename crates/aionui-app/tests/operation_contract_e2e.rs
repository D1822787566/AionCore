//! Production catalog + real helper + SDK bridge, with isolated backend effects.
use aion_tools::validation::{check_tool_schema, validate_tool_input};
use aion_tools::{
    Tool,
    aion_operation::{AionOperationTool, OPERATION_PATHS},
    aion_operation_contract::OperationContract,
};
use aionui_api_types::aion_operation_contract;
use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Default)]
struct Backend {
    requests: Mutex<Vec<(String, String, Value)>>,
    status: Mutex<Option<u16>>,
    fail_readback: bool,
    non_json: bool,
}

async fn handle(State(state): State<Arc<Backend>>, request: Request) -> axum::response::Response {
    let method = request.method().to_string();
    let path = request.uri().to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let mut requests = state.requests.lock().unwrap();
    let written = requests.iter().any(|(m, _, _)| m != "GET");
    requests.push((method.clone(), path.clone(), body));
    if state.non_json {
        return (StatusCode::BAD_GATEWAY, "fixture gateway returned plain text").into_response();
    }
    let status = *state.status.lock().unwrap();
    if let Some(status) = status.or_else(|| (state.fail_readback && written && method == "GET").then_some(500)) {
        return (
            StatusCode::from_u16(status).unwrap(),
            axum::Json(json!({"success":false,"code":"FIXTURE_DENIED","error":"fixture rejected"})),
        )
            .into_response();
    }
    let data = if path.starts_with("/api/conversations/") {
        json!({"id":"fixture-conversation","assistant":{"id":"fixture-assistant"}})
    } else if method == "GET" {
        json!({"id":"fixture-resource","name":"fixture"})
    } else {
        json!({"id":"fixture-created"})
    };
    axum::Json(json!({"success":true,"data":data})).into_response()
}

async fn fixture(state: Arc<Backend>) -> (Vec<(String, String)>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(handle).with_state(state))
            .await
            .unwrap()
    });
    (
        vec![
            ("AIONUI_HELPER_BIN".into(), env!("CARGO_BIN_EXE_aioncore").into()),
            ("AIONUI_BASE_URL".into(), format!("http://{address}")),
            ("AIONUI_USER_ID".into(), "fixture-user".into()),
            ("AIONUI_CONVERSATION_ID".into(), "fixture-conversation".into()),
            ("AIONUI_RUNTIME_TOKEN".into(), "fixture-token".into()),
        ],
        server,
    )
}
fn tool(env: &[(String, String)]) -> AionOperationTool {
    AionOperationTool::from_runtime_env(env)
        .unwrap()
        .with_contract(Some(OperationContract::new(aion_operation_contract())))
}
fn formatted(tool: &AionOperationTool, input: &Value, result: &aion_types::tool::ToolResult, budget: usize) -> Value {
    let result = tool.format_result(input, result, budget).unwrap();
    assert!(result.content.len() <= budget, "{} > {budget}", result.content.len());
    serde_json::from_str(&result.content).unwrap()
}

#[test]
fn every_public_operation_has_supported_schema_and_valid_examples() {
    let catalog = aion_operation_contract();
    let entries = catalog["operations"].as_array().unwrap();
    let names: std::collections::BTreeSet<_> = entries.iter().map(|e| e["operation"].as_str().unwrap()).collect();
    assert_eq!(names, OPERATION_PATHS.iter().copied().collect());
    for entry in entries {
        check_tool_schema(&entry["input_schema"]).unwrap_or_else(|e| panic!("{}: {e}", entry["operation"]));
        for input in entry["examples"].as_array().unwrap() {
            validate_tool_input(&entry["input_schema"], input)
                .unwrap_or_else(|e| panic!("{}: {e:?}", entry["operation"]));
        }
    }
    let tool = tool(&[("AIONUI_HELPER_BIN".into(), "never-run".into())]);
    for name in OPERATION_PATHS {
        assert!(
            tool.preflight(&json!({"operation":"operation.describe","input":{"operation":name}}))
                .is_ok()
        );
    }
}

#[test]
fn canonical_config_examples_also_deserialize_as_the_actual_backend_request_types() {
    use aionui_api_types::*;
    fn check<T: serde::de::DeserializeOwned>(operation: &str) {
        let catalog = aion_operation_contract();
        let descriptor = catalog["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["operation"] == operation)
            .unwrap();
        serde_json::from_value::<T>(descriptor["examples"][0].clone()).unwrap_or_else(|e| panic!("{operation}: {e}"));
    }
    check::<CreateAssistantRequest>("config.assistants.create");
    check::<UpdateAssistantRequest>("config.assistants.update");
    check::<ImportAssistantsRequest>("config.assistants.import");
    check::<CreateProviderRequest>("config.providers.create");
    check::<UpdateProviderRequest>("config.providers.update");
    check::<CreateCronJobRequest>("config.cron.jobs.create");
    check::<UpdateCronJobRequest>("config.cron.jobs.update");
    check::<CreateConversationCronRequest>("config.cron.current.create");
    check::<UpdateConversationCronRequest>("config.cron.current.update");
    check::<CreateMcpServerRequest>("config.mcp.servers.create");
    check::<CustomAgentUpsertRequest>("config.agents.custom.create");
    check::<CustomAgentUpsertRequest>("config.agents.custom.update");
    check::<SetAgentOverridesRequest>("config.agents.overrides.set");
    check::<SetEnabledRequest>("config.agents.enable");
}

#[tokio::test]
async fn legitimate_selectors_free_preferences_and_legacy_imports_still_reach_the_helper() {
    let state = Arc::new(Backend::default());
    let (env, server) = fixture(state.clone()).await;
    let tool = tool(&env);
    for (operation, input) in [
        (
            "config.assistants.get",
            json!({"assistant_id":"current","locale":"zh-CN"}),
        ),
        (
            "config.settings.client.put",
            json!({"arbitrary_preference":{"nested":[true,1]},"removed_preference":null,"user_id":"untrusted-fixture-id"}),
        ),
        (
            "config.mcp.servers.import",
            json!({"mcp":{"fixture":{"type":"remote","url":"http://fixture.invalid/mcp","timeout":5000}}}),
        ),
        (
            "config.mcp.servers.import",
            json!({"servers":[{"name":"fixture","transport":{"type":"streamable-http","url":"http://fixture.invalid/mcp"}}]}),
        ),
        (
            "config.mcp.servers.import",
            json!([{"name":"fixture","command":"fixture-command","args":[]}]),
        ),
    ] {
        let result = tool
            .execute(json!({"operation":operation,"input":input,"confirm":true}))
            .await;
        assert!(!result.is_error, "{operation}: {}", result.content);
    }
    let requests = state.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|(_, path, _)| path == "/api/assistants/fixture-assistant?locale=zh-CN")
    );
    let prefs = requests
        .iter()
        .find(|(method, path, _)| method == "PUT" && path == "/api/settings/client")
        .unwrap();
    assert_eq!(prefs.2["user_id"], "fixture-user");
    assert_eq!(prefs.2["removed_preference"], Value::Null);
    assert_eq!(
        requests
            .iter()
            .filter(|(method, path, _)| method == "POST" && path == "/api/mcp/servers/import")
            .count(),
        3
    );
    server.abort();
}

#[tokio::test]
async fn wrong_fields_across_domains_are_rejected_before_any_backend_request() {
    let state = Arc::new(Backend::default());
    let (env, server) = fixture(state.clone()).await;
    let tool = tool(&env);
    for (operation, field) in [
        ("config.mcp.servers.get", "server_id"),
        ("config.providers.update", "provider_id"),
        ("config.assistants.get", "assistant_id"),
        ("config.cron.jobs.get", "job_id"),
        ("config.agents.overrides.get", "agent_id"),
        ("config.skills.delete", "skill_name"),
        ("config.conversation.rename", "conversation_id"),
        ("session.send-message", "to"),
        ("team.task.create", "subject"),
        ("conversation.create", "name"),
        ("skills.show", "name"),
    ] {
        let input = json!({"operation":operation,"input":{"id":"fixture"},"confirm":true});
        let result = tool.execute(input.clone()).await;
        assert!(result.is_error, "{operation}");
        let value = formatted(&tool, &input, &result, 10000);
        assert_eq!(value["code"], "invalid_tool_arguments", "{operation}: {value}");
        assert_eq!(value["delivery"]["execution_status"], "not_executed");
        assert!(
            value["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["path"] == format!("/input/{field}")),
            "{value}"
        );
    }
    assert!(state.requests.lock().unwrap().is_empty());
    let input = json!({"operation":"config.mcp.servers.get","input":{"server_id":42}});
    let result = tool.execute(input.clone()).await;
    let value = formatted(&tool, &input, &result, 10000);
    assert_eq!(value["errors"][0]["path"], "/input/server_id");
    assert_eq!(value["errors"][0]["expected"], "string");
    let result = tool
        .execute(json!({"operation":"operation.describe","input":{"operation":"config.mcp.servers.get"}}))
        .await;
    let value: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(value["data"]["input_schema"]["required"], json!(["server_id"]));
    assert!(state.requests.lock().unwrap().is_empty());
    let input = json!({"operation":"config.mcp.servers.get","input":{"server_id":"fixture"}});
    let result = tool.execute(input.clone()).await;
    let value = formatted(&tool, &input, &result, 10000);
    assert_eq!(value["success"], true, "{value}");
    assert_eq!(state.requests.lock().unwrap().len(), 1);
    assert_eq!(state.requests.lock().unwrap()[0].1, "/api/mcp/servers/fixture");
    server.abort();
}

#[tokio::test]
async fn real_helper_returns_json_for_local_rejection_and_keeps_stderr_and_exit_code() {
    let state = Arc::new(Backend::default());
    let (env, server) = fixture(state.clone()).await;
    for payload in [
        json!({"id":"fixture"}),
        json!({"server_id":42}),
        json!({"server_id":""}),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aioncore"));
        command
            .args(["config", "mcp", "servers", "get"])
            .envs(env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        let output = child.wait_with_output().await.unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("CONFIG_PAYLOAD_INVALID"));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "CONFIG_PAYLOAD_INVALID");
        assert_eq!(value["execution"]["status"], "not_executed");
        assert_eq!(value["error"]["details"]["errors"][0]["path"], "/server_id");
    }
    assert!(state.requests.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn backend_failures_keep_codes_but_do_not_claim_no_execution() {
    let state = Arc::new(Backend::default());
    let (env, server) = fixture(state.clone()).await;
    let tool = tool(&env);
    for status in [400, 401, 404, 500] {
        *state.status.lock().unwrap() = Some(status);
        let input = json!({"operation":"config.mcp.servers.get","input":{"server_id":"fixture"}});
        let result = tool.execute(input.clone()).await;
        let value = formatted(&tool, &input, &result, 10000);
        assert_eq!(value["code"], "CONFIG_HTTP_STATUS_ERROR");
        assert_eq!(value["error"]["details"]["backend_code"], "FIXTURE_DENIED");
        assert_eq!(value["delivery"]["execution_status"], "unknown");
        assert_eq!(value["delivery"]["complete"], true);
        assert!(value["delivery"].get("result_id").is_none());
    }
    assert_eq!(state.requests.lock().unwrap().len(), 4);
    server.abort();
}

#[tokio::test]
async fn plain_text_backend_failures_still_produce_helper_json_across_domains() {
    let state = Arc::new(Backend {
        non_json: true,
        ..Default::default()
    });
    let (env, server) = fixture(state.clone()).await;
    let tool = tool(&env);
    for (operation, input) in [
        ("config.mcp.servers.get", json!({"server_id":"fixture"})),
        ("session.list", json!({})),
        ("team.members", json!({})),
        ("conversation.create", json!({"name":"fixture"})),
        ("skills.show", json!({"name":"fixture"})),
    ] {
        let request = json!({"operation":operation,"input":input,"confirm":true});
        let result = tool.execute(request.clone()).await;
        let value = formatted(&tool, &request, &result, 10000);
        assert!(result.is_error);
        assert_ne!(value["code"], "aion_operation_invalid_output", "{value}");
        assert_eq!(value["delivery"]["execution_status"], "unknown");
        assert!(!value.to_string().contains("fixture gateway returned plain text"));
    }
    assert_eq!(state.requests.lock().unwrap().len(), 5);
    server.abort();
}

#[tokio::test]
async fn write_success_followed_by_failed_readback_is_never_reported_as_not_executed() {
    let state = Arc::new(Backend {
        fail_readback: true,
        ..Default::default()
    });
    let (env, server) = fixture(state.clone()).await;
    let tool = tool(&env);
    let input = json!({"operation":"config.mcp.servers.create","input":{"name":"fixture","transport":{"type":"stdio","command":"fixture"}},"confirm":true});
    let result = tool.execute(input.clone()).await;
    let value = formatted(&tool, &input, &result, 10000);
    assert_eq!(value["success"], false);
    assert_eq!(value["execution"]["status"], "succeeded", "{value}");
    assert_eq!(value["delivery"]["execution_status"], "succeeded");
    assert_eq!(value["execution"]["retry_write"], false);
    assert_eq!(
        state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m == "POST")
            .count(),
        1
    );
    server.abort();
}

use aion_agent::{bootstrap::AgentBootstrap, output::null_sink::NullSink, session::SessionManager};
use aion_providers::{LlmProvider, ProviderError};
use aion_types::{
    llm::{LlmEvent, LlmRequest},
    message::{ContentBlock, StopReason, TokenUsage},
};

struct RecordingProvider {
    turns: Mutex<std::collections::VecDeque<Vec<LlmEvent>>>,
    requests: Mutex<Vec<LlmRequest>>,
}
#[async_trait::async_trait]
impl LlmProvider for RecordingProvider {
    async fn stream(&self, request: &LlmRequest) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_else(|| {
            vec![LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }]
        });
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        for event in events {
            tx.try_send(event).unwrap();
        }
        Ok(rx)
    }
}
fn turn(id: &str, operation: &str, input: Value) -> Vec<LlmEvent> {
    vec![
        LlmEvent::ToolUse {
            id: id.into(),
            name: "AionOperation".into(),
            input: json!({"operation":operation,"input":input,"confirm":true}),
            extra: None,
        },
        LlmEvent::Done {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        },
    ]
}
fn provider(turns: Vec<Vec<LlmEvent>>) -> Arc<RecordingProvider> {
    Arc::new(RecordingProvider {
        turns: Mutex::new(turns.into()),
        requests: Mutex::new(Vec::new()),
    })
}
fn config(dir: &std::path::Path) -> aion_config::config::Config {
    let mut config = aion_config::config::Config::resolve(&aion_config::config::CliArgs {
        provider: Some("anthropic".into()),
        api_key: Some("fixture-key".into()),
        model: Some("fixture".into()),
        base_url: Some("http://127.0.0.1:0".into()),
        max_tokens: Some(4096),
        max_turns: Some(12),
        max_tool_call_malformed_turns: Some(3),
        max_tool_call_failure_turns: Some(3),
        thinking: None,
        thinking_budget: None,
        system_prompt: Some("fixture".into()),
        profile: None,
        auto_approve: true,
        project_dir: Some(dir.to_path_buf()),
    })
    .unwrap();
    config.mcp.servers.clear();
    config.hooks = Default::default();
    config.compact.enabled = false;
    config.session.enabled = true;
    config.session.directory = dir.join("sessions").to_string_lossy().into_owned();
    config
}

struct Approvals {
    manager: Arc<aion_protocol::ToolApprovalManager>,
    count: Mutex<usize>,
}
impl aion_protocol::writer::ProtocolEmitter for Approvals {
    fn emit(&self, event: &aion_protocol::events::ProtocolEvent) -> std::io::Result<()> {
        if let aion_protocol::events::ProtocolEvent::ToolRequest { call_id, .. } = event {
            *self.count.lock().unwrap() += 1;
            let manager = self.manager.clone();
            let call_id = call_id.clone();
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                manager.resolve(&call_id, aion_protocol::ToolApprovalResult::Approved);
            });
        }
        Ok(())
    }
}

#[tokio::test]
async fn engine_returns_field_error_to_provider_then_executes_correction_once_and_saves_it() {
    for protocol in [false, true] {
        let state = Arc::new(Backend::default());
        let (env, server) = fixture(state.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let provider = provider(vec![
            turn("wrong", "config.mcp.servers.get", json!({"id":"fixture"})),
            turn(
                "describe",
                "operation.describe",
                json!({"operation":"config.mcp.servers.get"}),
            ),
            turn("correct", "config.mcp.servers.get", json!({"server_id":"fixture"})),
        ]);
        let mut config = config(dir.path());
        config.tools.auto_approve = !protocol;
        #[cfg(windows)]
        let hook_command = "Add-Content -LiteralPath 'preflight-hook.txt' -Value 'hook'";
        #[cfg(not(windows))]
        let hook_command = "printf 'hook\\n' >> preflight-hook.txt";
        config.hooks.pre_tool_use.push(aion_config::hooks::HookDef {
            name: "preflight-count".into(),
            tool_match: vec!["AionOperation".into()],
            file_match: vec![],
            command: hook_command.into(),
            timeout_ms: 5000,
        });
        let mut built = AgentBootstrap::new(config, dir.path().to_string_lossy(), Arc::new(NullSink))
            .runtime_env(env.clone())
            .operation_contract(aion_operation_contract())
            .provider(provider.clone())
            .build()
            .await
            .unwrap();
        let manager = Arc::new(aion_protocol::ToolApprovalManager::new());
        let approvals = Arc::new(Approvals {
            manager: manager.clone(),
            count: Mutex::new(0),
        });
        if protocol {
            built.engine.set_approval_manager(manager);
            built.engine.set_protocol_writer(approvals.clone());
        }
        built
            .engine
            .init_session("fixture", &dir.path().to_string_lossy(), Some("contract-test"))
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            built.engine.run("Read the fixture MCP server", ""),
        )
        .await
        .unwrap()
        .unwrap();
        let error = {
            let requests = provider.requests.lock().unwrap();
            requests[1]
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .find_map(|b| match b {
                    ContentBlock::ToolResult {
                        tool_use_id, content, ..
                    } if tool_use_id == "wrong" => Some(content),
                    _ => None,
                })
                .cloned()
                .unwrap()
        };
        let value: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(value["errors"][0]["path"], "/input/server_id");
        assert_eq!(value["delivery"]["execution_status"], "not_executed");
        assert_eq!(state.requests.lock().unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("preflight-hook.txt"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        // Exactly describe + the valid get request; no approval for the malformed get.
        if protocol {
            assert_eq!(*approvals.count.lock().unwrap(), 2);
        }
        let saved = SessionManager::new(dir.path().join("sessions"), 5)
            .load("contract-test")
            .unwrap();
        assert!(saved.messages.iter().flat_map(|m| &m.content).any(
            |b| matches!(b,ContentBlock::ToolResult{tool_use_id,content,..} if tool_use_id=="wrong" && content==&error)
        ));
        // A reconstructed runtime receives the same host catalog when resuming history.
        let resumed_provider = provider_for_resume();
        let mut resumed = AgentBootstrap::new(
            config_for_resume(dir.path()),
            dir.path().to_string_lossy(),
            Arc::new(NullSink),
        )
        .runtime_env(env)
        .operation_contract(aion_operation_contract())
        .resume(saved)
        .provider(resumed_provider.clone())
        .build()
        .await
        .unwrap();
        resumed.engine.run("Read again", "").await.unwrap();
        assert_eq!(state.requests.lock().unwrap().len(), 2);
        server.abort();
    }
}
fn provider_for_resume() -> Arc<RecordingProvider> {
    provider(vec![turn(
        "resumed",
        "config.mcp.servers.get",
        json!({"server_id":"fixture"}),
    )])
}
fn config_for_resume(dir: &std::path::Path) -> aion_config::config::Config {
    config(dir)
}

#[tokio::test]
async fn ordinary_and_fork_children_inherit_the_production_contract() {
    use aion_agent::spawner::{AgentSpawner, ForkOverrides, Spawner, SubAgentConfig};
    use aion_agent::tool_policy::ToolPolicy;
    for fork in [false, true] {
        let state = Arc::new(Backend::default());
        let (env, server) = fixture(state.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let recording = provider(vec![
            turn("wrong", "config.mcp.servers.get", json!({"id":"fixture"})),
            turn("correct", "config.mcp.servers.get", json!({"server_id":"fixture"})),
        ]);
        let spawner = AgentSpawner::new_with_env(
            recording.clone(),
            config(dir.path()),
            dir.path().to_path_buf(),
            env,
            ToolPolicy::Unrestricted,
        )
        .with_operation_contract(Some(OperationContract::new(aion_operation_contract())));
        let task = SubAgentConfig {
            name: "fixture".into(),
            prompt: "Read fixture".into(),
            max_turns: 5,
            max_tokens: 1024,
            system_prompt: None,
        };
        let result = if fork {
            spawner.spawn_fork(task, ForkOverrides::default()).await
        } else {
            spawner.spawn_one(task).await
        };
        assert!(!result.is_error, "{}", result.text);
        assert_eq!(state.requests.lock().unwrap().len(), 1);
        assert!(recording.requests.lock().unwrap()[1].messages.iter().flat_map(|m|&m.content).any(|b|matches!(b,ContentBlock::ToolResult{content,is_error:true,..} if content.contains("/input/server_id"))));
        server.abort();
    }
}

#[tokio::test]
async fn describes_do_not_reset_repeated_bad_argument_budget() {
    let state = Arc::new(Backend::default());
    let (env, server) = fixture(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut turns = Vec::new();
    for i in 0..4 {
        turns.push(turn(
            &format!("bad-{i}"),
            "config.mcp.servers.get",
            json!({"id":format!("fixture-{i}")}),
        ));
        turns.push(turn(
            &format!("describe-{i}"),
            "operation.describe",
            json!({"operation":"config.mcp.servers.get"}),
        ));
    }
    let provider = provider(turns);
    let mut built = AgentBootstrap::new(config(dir.path()), dir.path().to_string_lossy(), Arc::new(NullSink))
        .runtime_env(env)
        .operation_contract(aion_operation_contract())
        .provider(provider.clone())
        .build()
        .await
        .unwrap();
    let result = built.engine.run("fixture", "").await.unwrap();
    assert_eq!(result.stop_reason, StopReason::MaxTurns);
    assert!(
        result.text.contains("Tool execution repeatedly failed"),
        "{}",
        result.text
    );
    assert!(result.turns <= 6);
    assert!(provider.requests.lock().unwrap().len() < 8);
    assert!(state.requests.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn mixed_calls_keep_original_ids_and_do_not_replay_the_successful_sibling() {
    let state = Arc::new(Backend::default());
    let (env, server) = fixture(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut first = turn("valid-sibling", "config.context", json!({}));
    first.pop();
    first.extend(turn("bad-sibling", "config.mcp.servers.get", json!({"id":"fixture"})));
    let recording = provider(vec![
        first,
        turn("corrected", "config.mcp.servers.get", json!({"server_id":"fixture"})),
    ]);
    let mut built = AgentBootstrap::new(config(dir.path()), dir.path().to_string_lossy(), Arc::new(NullSink))
        .runtime_env(env)
        .operation_contract(aion_operation_contract())
        .provider(recording.clone())
        .build()
        .await
        .unwrap();
    built.engine.run("fixture", "").await.unwrap();
    let requests = recording.requests.lock().unwrap();
    let results: Vec<_> = requests[1]
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((
                tool_use_id.as_str(),
                serde_json::from_str::<Value>(content).unwrap(),
                is_error,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        results.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
        vec!["valid-sibling", "bad-sibling"]
    );
    assert!(!results[0].2);
    assert_eq!(results[1].1["errors"][0]["path"], "/input/server_id");
    let backend = state.requests.lock().unwrap();
    assert_eq!(backend.len(), 2);
    assert_eq!(
        backend
            .iter()
            .filter(|(_, path, _)| path == "/api/conversations/fixture-conversation")
            .count(),
        1
    );
    assert_eq!(
        backend
            .iter()
            .filter(|(_, path, _)| path == "/api/mcp/servers/fixture")
            .count(),
        1
    );
    server.abort();
}

#[tokio::test]
async fn real_team_help_remains_markdown_in_cli_and_json_in_structured_tool() {
    let state = Arc::new(Backend::default());
    let (env, server) = fixture(state.clone()).await;
    let tool = tool(&env);
    let result = tool.execute(json!({"operation":"team.help"})).await;
    assert!(!result.is_error, "{}", result.content);
    let value: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(value["data"]["format"], "markdown");
    assert!(value["data"]["text"].as_str().unwrap().contains("team send-message"));
    assert_eq!(value["execution"]["status"], "succeeded");
    assert!(state.requests.lock().unwrap().is_empty());
    server.abort();
}
