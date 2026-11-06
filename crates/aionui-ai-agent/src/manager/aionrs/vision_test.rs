use super::*;

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::output::null_sink::NullSink;
use aion_agent::tool_policy::ToolPolicy;
use aion_config::config::{CliArgs, Config};
use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::json;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

struct Fixture {
    config: McpServerConfig,
    status: Arc<AtomicU16>,
    attempts: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(tool: &str, failures: usize) -> Fixture {
    let status = Arc::new(AtomicU16::new(200));
    let attempts = Arc::new(AtomicUsize::new(0));
    let status_copy = status.clone();
    let attempts_copy = attempts.clone();
    let tool = tool.to_owned();
    let app = Router::new().route("/mcp", post(move |Json(body): Json<Value>| {
        let status = status_copy.clone();
        let attempts = attempts_copy.clone();
        let tool = tool.clone();
        async move {
            let attempt = if body["method"] == "initialize" { attempts.fetch_add(1, Ordering::SeqCst) } else { usize::MAX };
            let code = if attempt < failures { 503 } else { status.load(Ordering::SeqCst) };
            if code != 200 { return (StatusCode::from_u16(code).unwrap(), Json(json!({}))); }
            let result = match body["method"].as_str().unwrap_or("") {
                "initialize" => json!({"protocolVersion":"2025-03-26", "capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
                "tools/list" => json!({"tools":[{"name":tool,"description":"fixture tool","inputSchema":{"type":"object","properties":{}}}]}),
                "tools/call" => json!({"content":[{"type":"text","text":"fixture result"}],"isError":false}),
                _ => json!({}),
            };
            (StatusCode::OK, Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result})))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = server_config(format!("http://{}/mcp", listener.local_addr().unwrap()));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Fixture {
        config,
        status,
        attempts,
        task,
    }
}

async fn engine(policy: ToolPolicy) -> AgentEngine {
    let config = Config::resolve(&CliArgs {
        provider: Some("anthropic".into()),
        api_key: Some("fixture".into()),
        model: Some("fixture".into()),
        base_url: None,
        max_tokens: None,
        thinking: None,
        thinking_budget: None,
        max_turns: None,
        max_tool_call_malformed_turns: None,
        max_tool_call_failure_turns: None,
        system_prompt: None,
        profile: None,
        auto_approve: false,
        project_dir: None,
    })
    .unwrap();
    AgentBootstrap::new(config, std::env::temp_dir().to_string_lossy(), Arc::new(NullSink))
        .tool_policy(policy)
        .build()
        .await
        .unwrap()
        .engine
}

fn runtime(config: McpServerConfig) -> VisionRuntime {
    VisionRuntime::new(
        BaitongVisionConfig {
            service: None,
            override_config: Some(config),
        },
        &[],
        &HashMap::new(),
    )
}

#[tokio::test]
async fn empty_outer_success_is_recovered_and_text_paths_prepare_too() {
    let empty = Arc::new(McpManager::connect_all(&HashMap::new()).await.unwrap());
    let server = fixture(TOOL, 0).await;
    let mut engine = engine(ToolPolicy::default()).await;
    let mut runtime = VisionRuntime::new(
        BaitongVisionConfig {
            service: None,
            override_config: Some(server.config.clone()),
        },
        &[empty],
        &HashMap::new(),
    );
    runtime.prepare(&mut engine, "recover-text", false).await.unwrap();
    assert!(engine.is_tool_advertised(TOOL));
    assert_eq!(
        runtime
            .manager
            .as_ref()
            .unwrap()
            .call_tool(SERVER, TOOL, json!({}))
            .await
            .unwrap(),
        "fixture result"
    );
    for _ in 0..3 {
        runtime.prepare(&mut engine, "recover-repeat", true).await.unwrap();
    }
    assert_eq!(
        engine.tool_names().iter().filter(|name| name.as_str() == TOOL).count(),
        1
    );
    engine.registry_mut().remove_mcp_server(SERVER);
    runtime
        .prepare(&mut engine, "recover-registration", true)
        .await
        .unwrap();
    assert!(engine.is_tool_advertised(TOOL));
    assert_eq!(
        server.attempts.load(Ordering::SeqCst),
        1,
        "lost proxies should not reconnect a healthy session"
    );
}

#[tokio::test]
async fn aliases_and_other_servers_survive_connection_replacement() {
    let server = fixture(TOOL, 0).await;
    let database = fixture(TOOL, 0).await;
    let mut engine = engine(ToolPolicy::default()).await;
    let mut other = McpManager::connect_all(&HashMap::new()).await.unwrap();
    other.connect_one("database".into(), &database.config).await.unwrap();
    let other = Arc::new(other);
    let names = engine.tool_names();
    register_single_server_tools(engine.registry_mut(), &other, "database", &names, true);
    engine.registry_mut().mark_deferred_loaded([TOOL]);
    let mut runtime = runtime(server.config.clone());
    runtime.prepare(&mut engine, "aliases", true).await.unwrap();
    let exposed = engine.registry_mut().mcp_tool_name(SERVER, TOOL).unwrap();
    assert_ne!(exposed, TOOL);
    assert!(engine.is_tool_advertised(&exposed));
    // Force a missing runtime connection while preserving the old proxies.
    runtime.manager = None;
    runtime.connected_config = None;
    runtime.prepare(&mut engine, "aliases", true).await.unwrap();
    assert_eq!(
        engine.registry_mut().mcp_tool_name(SERVER, TOOL).as_deref(),
        Some(exposed.as_str())
    );
    assert_eq!(engine.tool_names().iter().filter(|name| *name == &exposed).count(), 1);
    assert_eq!(
        engine.registry_mut().mcp_tool_name("database", TOOL).as_deref(),
        Some(TOOL)
    );
    assert!(
        engine.is_tool_advertised(TOOL),
        "other server's loaded deferred tool must stay loaded"
    );
}

#[tokio::test]
async fn missing_tool_and_denied_policy_do_not_report_ready_or_retry() {
    let missing = fixture("unrelated_tool", 0).await;
    let mut engine = engine(ToolPolicy::default()).await;
    let issue = runtime(missing.config.clone())
        .prepare(&mut engine, "missing", true)
        .await
        .unwrap_err();
    assert_eq!(
        issue,
        VisionIssue {
            stage: "discovery",
            reason: "missing_tool"
        }
    );
    assert_eq!(missing.attempts.load(Ordering::SeqCst), 1);
    assert!(!engine.is_tool_advertised("unrelated_tool"));
    let server = fixture(TOOL, 0).await;
    let mut engine = self::engine(ToolPolicy::allow_only(["Read"])).await;
    let issue = runtime(server.config.clone())
        .prepare(&mut engine, "denied", true)
        .await
        .unwrap_err();
    assert_eq!(
        issue,
        VisionIssue {
            stage: "registration",
            reason: "tool_filtered"
        }
    );
    assert!(!engine.is_tool_advertised(TOOL));
    assert_eq!(server.attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn delayed_server_recovers_but_auth_and_bad_endpoint_fail_fast() {
    let delayed = fixture(TOOL, 2).await;
    let mut engine = engine(ToolPolicy::default()).await;
    runtime(delayed.config.clone())
        .prepare_until(&mut engine, "delay", true, Instant::now() + Duration::from_secs(4))
        .await
        .unwrap();
    assert_eq!(delayed.attempts.load(Ordering::SeqCst), 3);
    for status in [401, 403, 404] {
        let server = fixture(TOOL, 0).await;
        server.status.store(status, Ordering::SeqCst);
        let issue = timeout_at(
            Instant::now() + Duration::from_secs(1),
            runtime(server.config.clone()).prepare(&mut engine, "reject", true),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(issue.stage, "connection");
        assert_eq!(server.attempts.load(Ordering::SeqCst), 1);
        assert!(!engine.is_tool_advertised(TOOL));
    }
}

#[tokio::test]
async fn expired_budget_does_not_trust_cached_tools() {
    let server = fixture(TOOL, 0).await;
    let mut engine = engine(ToolPolicy::default()).await;
    let mut runtime = runtime(server.config.clone());
    runtime.prepare(&mut engine, "budget", true).await.unwrap();
    let issue = runtime
        .prepare_until(&mut engine, "budget", true, Instant::now())
        .await
        .unwrap_err();
    assert_eq!(issue.stage, "connection");
    assert!(!engine.is_tool_advertised(TOOL));
    // Idle time or an earlier exhausted budget does not poison the next message.
    runtime.prepare(&mut engine, "budget-next", true).await.unwrap();
    assert!(engine.is_tool_advertised(TOOL));
}

#[test]
fn image_attachment_detection_and_configuration_error_are_explicit() {
    assert!(has_image_attachment(&["C:/test/a.PNG".into()]));
    assert!(!has_image_attachment(&["C:/test/a.docx".into()]));
    assert!(
        VisionIssue {
            stage: "configuration",
            reason: "disabled"
        }
        .detail()
        .contains("关闭")
    );
}
