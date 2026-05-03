//! Typed client for the adapter's `/control/*` routes.
//!
//! The client never receives the password back: `/control/state` returns a
//! credential *state* only. Passwords travel one way, on `/control/login`.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::error::BaitongError;

/// Raw adapter view of its own state. Field names match
/// `opencode_v4_app/desktop_sidecar.py`.
#[derive(Debug, Clone, Deserialize)]
pub struct AdapterState {
    pub version: String,
    pub credentials: AdapterCredentials,
    pub adapter: AdapterRuntime,
    pub models: AdapterModels,
    pub config: AdapterConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterCredentials {
    pub state: String,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    /// Proven by upstream calls only: `unknown` | `valid` | `expired`.
    #[serde(default)]
    pub cookie_state: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterRuntime {
    pub state: String,
    pub host: String,
    pub display_host: String,
    pub port: u16,
    pub bind_mode: String,
    #[serde(default)]
    pub lan_host: String,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterVision {
    pub items: Vec<String>,
    #[serde(default)]
    pub selected: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterModels {
    pub items: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub thinking: Vec<String>,
    pub vision: AdapterVision,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterConfig {
    pub port: u16,
    pub bind_mode: String,
    #[serde(default)]
    pub lan_host: String,
    pub log_call: bool,
    pub log_interaction: bool,
    pub vision_mcp_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterStateEnvelope {
    pub state: AdapterState,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterRefreshEnvelope {
    pub status: String,
    #[serde(default)]
    pub detail: Option<String>,
    pub state: AdapterState,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterConfigEnvelope {
    pub restarted: bool,
    #[serde(default)]
    pub errors: Vec<String>,
    pub state: AdapterState,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterActionEnvelope {
    pub state: AdapterState,
    #[serde(default)]
    pub version: Option<String>,
}

pub struct AdapterClient {
    http: reqwest::Client,
    port: u16,
    token: String,
}

impl AdapterClient {
    pub fn new(port: u16, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            port,
            token: token.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, BaitongError> {
        let mut request = self
            .http
            .request(method, self.url(path))
            .header("X-Baitong-Token", &self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| BaitongError::Unreachable(format!("adapter request failed: {error}")))?;

        let status = response.status();
        let payload: Value = response.json().await.unwrap_or(Value::Null);

        if status.is_success() {
            return Ok(payload);
        }

        let detail = payload
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("adapter request rejected")
            .to_string();

        Err(match (status.as_u16(), detail.as_str()) {
            (401, "invalid_credentials") => BaitongError::InvalidCredentials,
            (401, _) => BaitongError::Adapter("adapter rejected the control token".to_string()),
            (502, "network_error") => BaitongError::Unreachable(detail),
            (409, _) => BaitongError::CredentialsRequired(detail),
            _ => BaitongError::Adapter(detail),
        })
    }

    pub async fn state(&self) -> Result<AdapterState, BaitongError> {
        let payload = self.send(reqwest::Method::GET, "/control/state", None).await?;
        serde_json::from_value(payload)
            .map_err(|error| BaitongError::Adapter(format!("unexpected adapter state: {error}")))
    }

    pub async fn login(&self, account: &str, password: &str) -> Result<AdapterState, BaitongError> {
        let body = serde_json::json!({ "account": account, "password": password });
        let payload = self.send(reqwest::Method::POST, "/control/login", Some(body)).await?;
        let envelope: AdapterStateEnvelope = serde_json::from_value(payload)
            .map_err(|error| BaitongError::Adapter(format!("unexpected login response: {error}")))?;
        Ok(envelope.state)
    }

    pub async fn refresh(&self) -> Result<AdapterRefreshEnvelope, BaitongError> {
        let payload = self
            .send(reqwest::Method::POST, "/control/session/refresh", None)
            .await?;
        serde_json::from_value(payload)
            .map_err(|error| BaitongError::Adapter(format!("unexpected refresh response: {error}")))
    }

    pub async fn clear_credentials(&self) -> Result<AdapterState, BaitongError> {
        let payload = self
            .send(reqwest::Method::POST, "/control/credentials/clear", None)
            .await?;
        let envelope: AdapterStateEnvelope = serde_json::from_value(payload)
            .map_err(|error| BaitongError::Adapter(format!("unexpected clear response: {error}")))?;
        Ok(envelope.state)
    }

    pub async fn put_config(&self, body: Value) -> Result<AdapterConfigEnvelope, BaitongError> {
        let payload = self.send(reqwest::Method::PUT, "/control/config", Some(body)).await?;
        serde_json::from_value(payload)
            .map_err(|error| BaitongError::Adapter(format!("unexpected config response: {error}")))
    }

    pub async fn adapter_action(&self, action: &str) -> Result<AdapterActionEnvelope, BaitongError> {
        let payload = self
            .send(reqwest::Method::POST, &format!("/control/adapter/{action}"), None)
            .await?;
        serde_json::from_value(payload)
            .map_err(|error| BaitongError::Adapter(format!("unexpected adapter response: {error}")))
    }

    pub async fn logs(&self) -> Result<Value, BaitongError> {
        self.send(reqwest::Method::GET, "/control/logs", None).await
    }

    pub async fn mcp_snippet(&self) -> Result<String, BaitongError> {
        let payload = self.send(reqwest::Method::GET, "/control/mcp-snippet", None).await?;
        Ok(payload
            .get("snippet")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    pub async fn write_opencode_config(&self) -> Result<String, BaitongError> {
        let payload = self
            .send(reqwest::Method::POST, "/control/write-opencode-config", None)
            .await?;
        Ok(payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// Open the opencode config directory in the desktop file manager. The
    /// adapter owns the path (`~/.config/opencode`), so AionCore only relays.
    pub async fn open_opencode_folder(&self) -> Result<String, BaitongError> {
        let payload = self
            .send(reqwest::Method::POST, "/control/open-opencode-folder", None)
            .await?;
        Ok(payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}
