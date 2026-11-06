//! The lifecycle hooks that keep the built-in provider row in sync.
//!
//! Runs against a real (fake) adapter over HTTP rather than a mocked client, so
//! the assertions cover the request path, the state mapping, and the sink calls
//! together.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};

use aionui_api_types::{
    BAITONG_API_KEY_SENTINEL, BAITONG_PLATFORM, BAITONG_PROVIDER_ID, BAITONG_VISION_MCP_NAME, BaitongAdapterState,
    BaitongConfigUpdate, ModelImageInputCapability,
};
use aionui_baitong::{
    AdapterLaunch, AdapterSupervisor, BaitongService, ManagedProviderSink, ManagedSpec, spec_from_status,
};

const TOKEN: &str = "test-token";

fn state_body(vision_mcp_enabled: bool) -> Value {
    json!({
        "version": "1.0.0",
        "credentials": { "state": "ready", "account": "u@b.com", "detail": null, "cookie_state": "valid" },
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
            "items": ["DeepSeek-V4-Flash", "Qwen3.5-35B-A3B"],
            "default": "DeepSeek-V4-Flash",
            "thinking": ["DeepSeek-V4-Flash"],
            "vision": { "items": ["Qwen3.6-35B-A3B"], "selected": "Qwen3.6-35B-A3B" }
        },
        "config": {
            "port": 8102,
            "bind_mode": "local",
            "lan_host": "",
            "log_call": false,
            "log_interaction": false,
            "vision_mcp_enabled": vision_mcp_enabled
        }
    })
}

/// One recorded call to the sink.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Upsert(ManagedSpec),
    Enabled(bool),
}

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<Call>>,
}

impl Recorder {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn upserts(&self) -> Vec<ManagedSpec> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Upsert(spec) => Some(spec),
                Call::Enabled(_) => None,
            })
            .collect()
    }

    fn enabled_flips(&self) -> Vec<bool> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Enabled(enabled) => Some(enabled),
                Call::Upsert(_) => None,
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl ManagedProviderSink for Recorder {
    async fn upsert(&self, spec: &ManagedSpec) -> Result<(), String> {
        self.calls.lock().unwrap().push(Call::Upsert(spec.clone()));
        Ok(())
    }

    async fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        self.calls.lock().unwrap().push(Call::Enabled(enabled));
        Ok(())
    }
}

/// Sink that always fails, to prove a provider-write failure is not fatal.
struct FailingSink;

#[async_trait::async_trait]
impl ManagedProviderSink for FailingSink {
    async fn upsert(&self, _spec: &ManagedSpec) -> Result<(), String> {
        Err("database is on fire".to_string())
    }

    async fn set_enabled(&self, _enabled: bool) -> Result<(), String> {
        Err("database is on fire".to_string())
    }
}

/// Spawn the fake adapter and return the control port it listens on.
///
/// The vision switch is fixed per server so parallel tests cannot observe each
/// other's setting.
async fn spawn_fake_adapter(vision_mcp_enabled: bool) -> u16 {
    let health = || async { (StatusCode::OK, Json(json!({ "status": "ok" }))) };
    let state = move |headers: axum::http::HeaderMap| async move {
        if headers.get("x-baitong-token").and_then(|v| v.to_str().ok()) != Some(TOKEN) {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "detail": "bad token" })));
        }
        (StatusCode::OK, Json(state_body(vision_mcp_enabled)))
    };
    let login = move |Json(body): Json<Value>| async move {
        if body["password"] == "wrong" {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "detail": "invalid_credentials" })),
            );
        }
        (StatusCode::OK, Json(json!({ "state": state_body(vision_mcp_enabled) })))
    };
    let clear = move || async move { (StatusCode::OK, Json(json!({ "state": state_body(vision_mcp_enabled) }))) };
    let config = move || async move {
        (
            StatusCode::OK,
            Json(json!({ "restarted": false, "errors": [], "state": state_body(vision_mcp_enabled) })),
        )
    };

    let app = Router::new()
        .route("/control/health", get(health))
        .route("/control/state", get(state))
        .route("/control/login", post(login))
        .route("/control/credentials/clear", post(clear))
        .route("/control/config", put(config))
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

/// A service wired to an already-running adapter, plus its temp dir.
async fn service_on(control_port: u16) -> (BaitongService, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let launch = AdapterLaunch {
        program: PathBuf::from("definitely-not-a-real-adapter-binary"),
        args: Vec::new(),
        cwd: None,
    };
    let supervisor = Arc::new(AdapterSupervisor::new(launch, dir.path().to_path_buf()).with_control_port(control_port));
    supervisor.adopt_running(control_port, TOKEN).await.unwrap();
    (BaitongService::new(supervisor, dir.path().to_path_buf()), dir)
}

fn service_without_adapter(dir: &std::path::Path) -> BaitongService {
    let launch = AdapterLaunch {
        program: PathBuf::from("definitely-not-a-real-adapter-binary"),
        args: Vec::new(),
        cwd: None,
    };
    BaitongService::new(
        Arc::new(AdapterSupervisor::new(launch, dir.to_path_buf())),
        dir.to_path_buf(),
    )
}

// ---------------------------------------------------------------------------
// Provenance of the constants the frontend depends on
// ---------------------------------------------------------------------------

#[test]
fn published_identity_is_stable() {
    // A rename would silently break the frontend's "built-in" detection and the
    // backend's delete/update guards, so pin the literal values.
    assert_eq!(BAITONG_PROVIDER_ID, "baitong-builtin");
    assert_eq!(BAITONG_PLATFORM, "baitong");
    assert_eq!(BAITONG_API_KEY_SENTINEL, "baitong-local-no-key");
    assert_eq!(BAITONG_VISION_MCP_NAME, "vision");
}

// ---------------------------------------------------------------------------
// Pure mapping
// ---------------------------------------------------------------------------

fn status_from(value: Value) -> aionui_api_types::BaitongStatusResponse {
    serde_json::from_value(value).unwrap()
}

#[test]
fn every_catalog_model_is_marked_image_capable() {
    let status = status_from(json!({
        "adapter": {
            "state": "running", "host": "127.0.0.1", "port": 8102, "bind_mode": "local",
            "display_host": "localhost", "managed": true
        },
        "session": {
            "state": "authenticated", "account": "u@b.com", "cookie_state": "valid",
            "credential_state": "ready"
        },
        "enterprise": { "state": "deferred" },
        "models": {
            "items": ["a", "b", "c"],
            "thinking": [],
            "vision": { "items": [] },
            "vision_mcp_enabled": true
        },
        "config": {
            "version": 1, "port": 8102, "bind_mode": "local",
            "lan_host": "",
            "log_call": false, "log_interaction": false
        }
    }));

    let spec = spec_from_status(&status).expect("spec");

    assert_eq!(spec.models.len(), 3);
    for model in &spec.models {
        assert_eq!(
            spec.model_settings[model].image_input,
            Some(ModelImageInputCapability::Supported)
        );
    }
    assert_eq!(spec.base_url, "http://127.0.0.1:8102/v1");
    assert_eq!(
        spec.vision_mcp.as_ref().map(|v| v.url.as_str()),
        Some("http://127.0.0.1:8102/mcp")
    );
}

#[test]
fn stopped_status_never_publishes() {
    let mut status = status_from(json!({
        "adapter": {
            "state": "stopped", "host": "127.0.0.1", "port": 8102, "bind_mode": "local",
            "display_host": "localhost", "managed": true
        },
        "session": {
            "state": "unknown", "cookie_state": "unknown", "credential_state": "absent"
        },
        "enterprise": { "state": "deferred" },
        "models": { "items": ["a"], "thinking": [], "vision": { "items": [] }, "vision_mcp_enabled": true },
        "config": {
            "version": 0, "port": 8102, "bind_mode": "local",
            "lan_host": "",
            "log_call": false, "log_interaction": false
        }
    }));

    assert!(spec_from_status(&status).is_none());
    status.adapter.state = BaitongAdapterState::Running;
    assert!(spec_from_status(&status).is_some());
}

// ---------------------------------------------------------------------------
// Lifecycle hooks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_publishes_the_catalog_and_enables_the_row() {
    let port = spawn_fake_adapter(true).await;
    let (service, _dir) = service_on(port).await;
    let recorder = Arc::new(Recorder::default());
    let service = service.with_managed_provider(recorder.clone());

    let status = service.login("u@b.com", "right").await.unwrap();
    assert_eq!(status.adapter.state, BaitongAdapterState::Running);

    let upserts = recorder.upserts();
    assert_eq!(upserts.len(), 1, "login publishes once");
    assert_eq!(upserts[0].models, vec!["DeepSeek-V4-Flash", "Qwen3.5-35B-A3B"]);
    assert_eq!(recorder.enabled_flips(), vec![true]);
}

#[tokio::test]
async fn failed_login_publishes_nothing() {
    let port = spawn_fake_adapter(true).await;
    let (service, _dir) = service_on(port).await;
    let recorder = Arc::new(Recorder::default());
    let service = service.with_managed_provider(recorder.clone());

    let error = service.login("u@b.com", "wrong").await.unwrap_err();
    assert_eq!(error.code(), "BAITONG_INVALID_CREDENTIALS");
    assert!(recorder.calls().is_empty());
}

#[tokio::test]
async fn config_save_republishes_content_but_never_toggles_enabled() {
    let port = spawn_fake_adapter(true).await;
    let (service, _dir) = service_on(port).await;
    let recorder = Arc::new(Recorder::default());
    let service = service.with_managed_provider(recorder.clone());

    service
        .update_config(BaitongConfigUpdate {
            models: Some(vec!["only".into()]),
            ..BaitongConfigUpdate::default()
        })
        .await
        .unwrap();

    assert_eq!(recorder.upserts().len(), 1);
    assert!(
        recorder.enabled_flips().is_empty(),
        "a config save must not turn the provider on or off"
    );
}

#[tokio::test]
async fn status_restore_publishes_content_but_never_toggles_enabled() {
    let port = spawn_fake_adapter(true).await;
    let (service, _dir) = service_on(port).await;
    let recorder = Arc::new(Recorder::default());
    let service = service.with_managed_provider(recorder.clone());

    // First status read is the launch-time restore.
    service.status().await.unwrap();
    // Later polls only report; they must not publish a second time.
    service.status().await.unwrap();

    assert_eq!(recorder.upserts().len(), 1);
    assert!(recorder.enabled_flips().is_empty());
}

#[tokio::test]
async fn logout_disables_without_publishing_content() {
    let port = spawn_fake_adapter(true).await;
    let (service, _dir) = service_on(port).await;
    let recorder = Arc::new(Recorder::default());
    let service = service.with_managed_provider(recorder.clone());

    service.logout().await.expect("logout must not fail");

    assert_eq!(recorder.enabled_flips(), vec![false]);
    assert!(recorder.upserts().is_empty(), "logout must not rewrite the catalog");
}

#[tokio::test]
async fn logout_disables_even_when_the_adapter_is_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let service = service_without_adapter(dir.path()).with_managed_provider(recorder.clone());

    service.logout().await.expect("logout must not fail");

    assert_eq!(recorder.enabled_flips(), vec![false]);
    assert!(recorder.upserts().is_empty());
}

#[tokio::test]
async fn a_sink_failure_does_not_break_the_lifecycle_call() {
    let port = spawn_fake_adapter(true).await;
    let (service, _dir) = service_on(port).await;
    let service = service.with_managed_provider(Arc::new(FailingSink));

    // Both the upsert and the enable fail; login still succeeds, because the
    // adapter — not the provider table — decides whether the user is logged in.
    let status = service.login("u@b.com", "right").await.unwrap();
    assert_eq!(status.adapter.state, BaitongAdapterState::Running);

    let status = service.logout().await.expect("logout must not fail");
    assert_eq!(status.adapter.state, BaitongAdapterState::Stopped);
}

#[tokio::test]
async fn vision_endpoint_tracks_the_adapter_and_clears_on_logout() {
    let port = spawn_fake_adapter(true).await;
    let (service, _dir) = service_on(port).await;

    assert!(service.vision_mcp().await.is_none(), "nothing observed yet");

    service.login("u@b.com", "right").await.unwrap();
    let vision = service.vision_mcp().await.expect("vision endpoint");
    assert_eq!(vision.url, "http://127.0.0.1:8102/mcp");
    assert!(vision.enabled);

    service.logout().await.unwrap();
    assert!(
        service.vision_mcp().await.is_none(),
        "a stopped adapter must not leave a dialable endpoint behind"
    );
}

#[tokio::test]
async fn vision_endpoint_carries_the_adapter_switch() {
    let port = spawn_fake_adapter(false).await;
    let (service, _dir) = service_on(port).await;
    service.login("u@b.com", "right").await.unwrap();
    let vision = service.vision_mcp().await.expect("endpoint is still advertised");
    assert!(!vision.enabled, "the switch is carried through, not dropped");
}

#[tokio::test]
async fn status_on_a_dead_adapter_clears_the_vision_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let service = service_without_adapter(dir.path());

    let status = service.status().await.unwrap();
    assert_eq!(status.adapter.state, BaitongAdapterState::Unavailable);

    assert!(service.vision_mcp().await.is_none());
}

#[tokio::test]
async fn vision_configuration_reads_saved_settings_before_any_status_poll() {
    let port = spawn_fake_adapter(true).await;
    let (service, _dir) = service_on(port).await;
    assert!(service.vision_mcp().await.is_none(), "baseline cache starts empty");
    let saved = service.vision_configuration().await.unwrap().unwrap();
    assert_eq!(saved.url, "http://127.0.0.1:8102/mcp");
    assert!(saved.enabled);
}

#[tokio::test]
async fn vision_configuration_honors_disable_and_explicit_stop() {
    let port = spawn_fake_adapter(false).await;
    let (service, _dir) = service_on(port).await;
    assert!(service.vision_configuration().await.unwrap().is_none());
    service
        .adapter_action(aionui_api_types::BaitongAdapterAction::Stop)
        .await
        .unwrap();
    let error = service.vision_configuration().await.unwrap_err();
    assert_eq!(
        error.code(),
        "BAITONG_NETWORK_ERROR",
        "stop must not relaunch the invalid executable"
    );
}

#[tokio::test]
async fn vision_configuration_uses_saved_port_without_running_models() {
    let mut body = state_body(true);
    body["adapter"]["state"] = json!("stopped");
    body["models"]["items"] = json!([]);
    body["config"]["port"] = json!(19123);
    let app = Router::new()
        .route("/control/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route(
            "/control/state",
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (service, _dir) = service_on(port).await;
    assert_eq!(
        service.vision_configuration().await.unwrap().unwrap().url,
        "http://127.0.0.1:19123/mcp"
    );
    task.abort();
    assert_eq!(
        service.vision_configuration().await.unwrap_err().code(),
        "BAITONG_NETWORK_ERROR"
    );
}
