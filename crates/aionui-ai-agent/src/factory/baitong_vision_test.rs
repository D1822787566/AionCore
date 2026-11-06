use super::*;
use aionui_baitong::{AdapterLaunch, AdapterSupervisor, BaitongService};
use axum::{Json, Router, routing::get};
use serde_json::json;

#[tokio::test]
async fn database_only_selection_does_not_hide_saved_vision_with_empty_cache() {
    let app = Router::new()
        .route("/control/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/control/state", get(|| async { Json(json!({
            "version":"fixture", "credentials":{"state":"ready","account":"fixture","cookie_state":"valid"},
            "adapter":{"state":"stopped","host":"127.0.0.1","display_host":"localhost","port":19123,"bind_mode":"local","lan_host":"","last_error":null},
            "models":{"items":[],"default":"fixture","thinking":[],"vision":{"items":[],"selected":"fixture"}},
            "config":{"port":19123,"bind_mode":"local","lan_host":"","log_call":false,"log_interaction":false,"vision_mcp_enabled":true}
        })) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let supervisor = Arc::new(
        AdapterSupervisor::new(
            AdapterLaunch {
                program: "not-an-adapter".into(),
                args: Vec::new(),
                cwd: None,
            },
            dir.path().into(),
        )
        .with_control_port(port),
    );
    supervisor.adopt_running(port, "fixture-token").await.unwrap();
    let service = Arc::new(BaitongService::new(supervisor, dir.path().into()));
    assert!(service.vision_mcp().await.is_none());
    let database = McpServerConfig {
        transport: TransportType::StreamableHttp,
        command: None,
        args: None,
        env: None,
        url: Some("http://127.0.0.1:19124/database".into()),
        headers: None,
        deferred: Some(false),
        startup_timeout_ms: None,
    };
    let mut servers = HashMap::from([("database".into(), database)]);
    let context = prepare_baitong_vision("baitong", Some(service), &mut servers, "factory-fixture")
        .await
        .unwrap();
    assert!(context.override_config.is_none());
    assert_eq!(servers.len(), 2);
    assert_eq!(servers["vision"].url.as_deref(), Some("http://127.0.0.1:19123/mcp"));
    assert_eq!(servers["vision"].deferred, Some(false));
    assert!(servers.contains_key("database"));
    task.abort();
}

#[tokio::test]
async fn explicit_vision_wins_and_other_providers_do_not_get_builtin_vision() {
    let config = McpServerConfig {
        transport: TransportType::StreamableHttp,
        command: None,
        args: None,
        env: None,
        url: Some("http://127.0.0.1:19125/custom".into()),
        headers: None,
        deferred: Some(false),
        startup_timeout_ms: None,
    };
    let mut servers = HashMap::from([("vision".into(), config)]);
    let context = prepare_baitong_vision("baitong", None, &mut servers, "user-override")
        .await
        .unwrap();
    assert_eq!(context.override_config.unwrap().url, servers["vision"].url);
    assert_eq!(servers.len(), 1);
    let mut empty = HashMap::new();
    assert!(
        prepare_baitong_vision("openai", None, &mut empty, "other-provider")
            .await
            .is_none()
    );
    assert!(empty.is_empty());
    assert!(
        prepare_baitong_vision("baitong", None, &mut empty, "host-unavailable")
            .await
            .is_some()
    );
    assert!(
        empty.is_empty(),
        "unknown configuration must not synthesize a default endpoint"
    );
}
