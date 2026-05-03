//! Contract tests for the adapter client: the exact JSON the Python control
//! plane emits must parse, and its error envelopes must map to stable codes.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};

use aionui_baitong::AdapterClient;

const TOKEN: &str = "test-token";

fn state_body() -> Value {
    json!({
        "version": "1.0.0",
        "credentials": {
            "state": "ready",
            "account": "u@b.com",
            "detail": null,
            "cookie_state": "expired"
        },
        "adapter": {
            "state": "running",
            "host": "127.0.0.1",
            "display_host": "localhost",
            "port": 8102,
            "bind_mode": "local",
            "lan_host": "",
            "last_error": null
        },
        "models": {
            "items": ["Qwen3.5-35B-A3B"],
            "default": "Qwen3.5-35B-A3B",
            "thinking": ["DeepSeek-V4-Flash"],
            "vision": { "items": ["Qwen3.6-35B-A3B"], "selected": "Qwen3.6-35B-A3B" }
        },
        "config": {
            "port": 8102,
            "bind_mode": "local",
            "lan_host": "",
            "log_call": false,
            "log_interaction": false,
            "vision_mcp_enabled": true
        }
    })
}

async fn state_handler(headers: axum::http::HeaderMap) -> impl IntoResponse {
    if headers.get("x-baitong-token").and_then(|v| v.to_str().ok()) != Some(TOKEN) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"detail": "控制令牌无效"})));
    }
    (StatusCode::OK, Json(state_body()))
}

async fn login_handler(Json(body): Json<Value>) -> impl IntoResponse {
    if body["password"] == "wrong" {
        return (StatusCode::UNAUTHORIZED, Json(json!({"detail": "invalid_credentials"})));
    }
    (StatusCode::OK, Json(json!({ "state": state_body() })))
}

async fn refresh_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "status": "credentials_required",
            "detail": "decrypt_failed",
            "state": state_body()
        })),
    )
}

async fn config_handler(Json(body): Json<Value>) -> impl IntoResponse {
    let mut state = state_body();
    if let Some(port) = body.get("port").and_then(Value::as_u64) {
        state["config"]["port"] = json!(port);
    }
    (
        StatusCode::OK,
        Json(json!({ "restarted": false, "errors": [], "state": state })),
    )
}

async fn snippet_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "snippet": "{\"mcp\":{}}" })))
}

async fn spawn_fake_adapter() -> u16 {
    let app = Router::new()
        .route("/control/state", get(state_handler))
        .route("/control/login", post(login_handler))
        .route("/control/session/refresh", post(refresh_handler))
        .route("/control/config", put(config_handler))
        .route("/control/mcp-snippet", get(snippet_handler))
        .with_state(());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

#[tokio::test]
async fn parses_adapter_state() {
    let port = spawn_fake_adapter().await;
    let client = AdapterClient::new(port, TOKEN);
    let state = client.state().await.unwrap();

    assert_eq!(state.version, "1.0.0");
    assert_eq!(state.credentials.state, "ready");
    assert_eq!(state.credentials.account.as_deref(), Some("u@b.com"));
    // Proven-by-upstream cookie state is carried through untouched.
    assert_eq!(state.credentials.cookie_state.as_deref(), Some("expired"));
    assert_eq!(state.adapter.port, 8102);
    assert_eq!(state.models.thinking, vec!["DeepSeek-V4-Flash"]);
    assert!(state.config.vision_mcp_enabled);
}

#[tokio::test]
async fn tolerates_an_older_adapter_without_cookie_state() {
    let mut body = state_body();
    body["credentials"] = json!({ "state": "ready", "account": "u@b.com" });

    let app = Router::new()
        .route("/control/state", get(|| async move { (StatusCode::OK, Json(body)) }))
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let state = AdapterClient::new(port, TOKEN).state().await.unwrap();
    assert_eq!(state.credentials.state, "ready");
    assert!(state.credentials.cookie_state.is_none());
}

#[tokio::test]
async fn rejects_wrong_control_token() {
    let port = spawn_fake_adapter().await;
    let client = AdapterClient::new(port, "nope");
    let error = client.state().await.unwrap_err();
    assert_eq!(error.code(), "BAITONG_ADAPTER_ERROR");
}

#[tokio::test]
async fn maps_invalid_credentials() {
    let port = spawn_fake_adapter().await;
    let client = AdapterClient::new(port, TOKEN);
    let error = client.login("u@b.com", "wrong").await.unwrap_err();
    assert_eq!(error.code(), "BAITONG_INVALID_CREDENTIALS");
}

#[tokio::test]
async fn maps_refresh_credential_state() {
    let port = spawn_fake_adapter().await;
    let client = AdapterClient::new(port, TOKEN);
    let envelope = client.refresh().await.unwrap();
    assert_eq!(envelope.status, "credentials_required");
    assert_eq!(envelope.detail.as_deref(), Some("decrypt_failed"));
}

#[tokio::test]
async fn applies_config_and_reports_restart_flag() {
    let port = spawn_fake_adapter().await;
    let client = AdapterClient::new(port, TOKEN);
    let envelope = client.put_config(json!({ "port": 8300 })).await.unwrap();
    assert!(!envelope.restarted);
    assert!(envelope.errors.is_empty());
    assert_eq!(envelope.state.config.port, 8300);
}

#[tokio::test]
async fn unreachable_adapter_is_a_network_error() {
    // Bind then drop, so the port is (almost certainly) closed.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let client = AdapterClient::new(port, TOKEN);
    let error = client.state().await.unwrap_err();
    assert_eq!(error.code(), "BAITONG_NETWORK_ERROR");
}

#[tokio::test]
async fn launch_plan_defaults_to_module_entry() {
    let launch = aionui_baitong::AdapterLaunch::from_env();
    assert!(!launch.program.as_os_str().is_empty());
    assert!(launch.args.iter().any(|arg| arg.contains("desktop_sidecar")));
}

#[tokio::test]
async fn state_never_echoes_a_password_field() {
    let port = spawn_fake_adapter().await;
    let client = AdapterClient::new(port, TOKEN);
    let state = client.state().await.unwrap();
    let encoded = serde_json::to_string(&json!({
        "account": state.credentials.account,
        "detail": state.credentials.detail,
    }))
    .unwrap();
    assert!(!encoded.contains("password"));
}
