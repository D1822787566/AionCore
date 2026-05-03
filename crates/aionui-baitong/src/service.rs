//! Business logic for the Baitong adapter bridge.
//!
//! Responsibilities:
//! - own the adapter process through [`AdapterSupervisor`];
//! - translate adapter state into the stable `BaitongStatusResponse` contract;
//! - guard config saves with a monotonic version so a stale UI cannot silently
//!   overwrite a newer save.
//!
//! No axum types appear here.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex;

use aionui_api_types::{
    BaitongAdapterAction, BaitongAdapterActionResponse, BaitongAdapterState, BaitongAdapterStatus,
    BaitongConfigResponse, BaitongConfigSnapshot, BaitongConfigUpdate, BaitongCookieState, BaitongCredentialState,
    BaitongEnterpriseState, BaitongEnterpriseStatus, BaitongLogsResponse, BaitongModelCatalog, BaitongRefreshResponse,
    BaitongRefreshStatus, BaitongSessionState, BaitongSessionStatus, BaitongStatusResponse, BaitongVisionCatalog,
};

use crate::client::{AdapterClient, AdapterState};
use crate::error::BaitongError;
use crate::managed::{ManagedSpec, VisionMcp, spec_from_status};
use crate::supervisor::AdapterSupervisor;

/// Where the built-in provider row gets published.
///
/// Narrow on purpose: this crate must not depend on `aionui-system`, and the
/// only thing it needs from the provider world is "make the managed row match
/// this spec" plus "flip its enabled flag". Implemented in `aionui-app`, which
/// owns both halves.
#[async_trait::async_trait]
pub trait ManagedProviderSink: Send + Sync {
    /// Create or update the built-in provider row to match `spec`.
    ///
    /// Implementations must skip the write when the stored row already matches,
    /// so repeated logins do not churn `updated_at`.
    async fn upsert(&self, spec: &ManagedSpec) -> Result<(), String>;

    /// Flip `enabled` on the built-in provider row without touching the rest.
    async fn set_enabled(&self, enabled: bool) -> Result<(), String>;
}

/// Version bookkeeping for the runtime config snapshot.
#[derive(Debug, Clone, Default)]
struct ConfigVersion {
    version: u64,
    last_applied_at: Option<String>,
}

pub struct BaitongService {
    supervisor: Arc<AdapterSupervisor>,
    version: Mutex<ConfigVersion>,
    /// Directory handed to the adapter as `BAITONG_CONFIG_DIR`.
    config_dir: PathBuf,
    /// One-shot launch-time restore. The first status read after startup may
    /// start the adapter so persisted credentials and cookie state become
    /// visible again; every later status read only reports. Cleared by `logout`
    /// and by an explicit `stop`, so a poll never silently undoes that choice.
    pending_restore: AtomicBool,
    /// Set once at construction. `None` in tests and in any composition that
    /// does not wire providers.
    managed_sink: Option<Arc<dyn ManagedProviderSink>>,
    /// Last observed vision-MCP endpoint, refreshed by every `compose`.
    ///
    /// In-memory only: the agent factory reads it per session build, and nothing
    /// about it belongs in a table (see `factory::aionrs` for why a built-in
    /// `mcp_server` row would never reach aionrs).
    vision_mcp: Mutex<Option<VisionMcp>>,
}

impl BaitongService {
    pub fn new(supervisor: Arc<AdapterSupervisor>, config_dir: PathBuf) -> Self {
        Self {
            supervisor,
            version: Mutex::new(ConfigVersion::default()),
            config_dir,
            pending_restore: AtomicBool::new(true),
            managed_sink: None,
            vision_mcp: Mutex::new(None),
        }
    }

    /// Publish the built-in model provider alongside the adapter lifecycle.
    pub fn with_managed_provider(mut self, sink: Arc<dyn ManagedProviderSink>) -> Self {
        self.managed_sink = Some(sink);
        self
    }

    pub fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }

    /// Vision MCP endpoint of the adapter as last observed.
    ///
    /// `None` when the adapter is not running (or not yet observed). The agent
    /// factory injects it into aionrs sessions whose model belongs to the
    /// built-in provider, so the model can call `look_at_image`.
    pub async fn vision_mcp(&self) -> Option<VisionMcp> {
        self.vision_mcp.lock().await.clone()
    }

    /// Push the row to the sink. Best effort: a provider write failure must never
    /// turn a successful login, config save, or status read into an error.
    async fn publish_managed(&self, status: &BaitongStatusResponse, enable: Option<bool>) {
        let Some(sink) = self.managed_sink.as_ref() else {
            return;
        };
        if let Some(spec) = spec_from_status(status)
            && let Err(error) = sink.upsert(&spec).await
        {
            tracing::warn!(error = %error, stage = "baitong.managed_provider.upsert", "failed to publish builtin provider");
        }
        if let Some(enable) = enable
            && let Err(error) = sink.set_enabled(enable).await
        {
            tracing::warn!(error = %error, enabled = enable, stage = "baitong.managed_provider.enabled", "failed to update builtin provider state");
        }
    }

    /// Start the adapter if needed and return a client bound to its token.
    async fn client(&self) -> Result<AdapterClient, BaitongError> {
        let token = self.supervisor.ensure_running().await?;
        Ok(AdapterClient::new(self.supervisor.control_port(), token.to_string()))
    }

    async fn client_if_running(&self) -> Option<AdapterClient> {
        let token = self.supervisor.token().await?;
        Some(AdapterClient::new(self.supervisor.control_port(), token.to_string()))
    }

    /// `GET /api/baitong/status`.
    ///
    /// This is the launch-time restore path: a status read may start the
    /// adapter so the persisted credentials and cookie state become visible
    /// again after a restart. It never fails for adapter problems — an
    /// unreachable adapter is reported as `unavailable` with `last_error`, not
    /// as an HTTP error, because the settings page must still render.
    pub async fn status(&self) -> Result<BaitongStatusResponse, BaitongError> {
        // One restore attempt only: a failed spawn must not turn every status
        // poll into another spawn attempt.
        if self.pending_restore.swap(false, Ordering::SeqCst) {
            return match self.client().await {
                Ok(client) => match client.state().await {
                    Ok(state) => {
                        let status = self.compose(state, true).await;
                        // Restore publishes content but never re-enables: the
                        // user's "off" choice survives a restart, and the row
                        // they already had comes back with their model toggles.
                        self.publish_managed(&status, None).await;
                        Ok(status)
                    }
                    Err(BaitongError::Unreachable(error)) => Ok(self.stopped_view(Some(error)).await),
                    Err(error) => Ok(self.stopped_view(Some(error.to_string())).await),
                },
                Err(error) => Ok(self.stopped_view(Some(error.to_string())).await),
            };
        }

        if let Some(client) = self.client_if_running().await {
            match client.state().await {
                Ok(state) => return Ok(self.compose(state, true).await),
                Err(BaitongError::Unreachable(error)) => return Ok(self.stopped_view(Some(error)).await),
                Err(error) => return Ok(self.stopped_view(Some(error.to_string())).await),
            }
        }
        Ok(self.stopped_view(None).await)
    }

    /// `POST /api/baitong/auth/login`.
    pub async fn login(&self, account: &str, password: &str) -> Result<BaitongStatusResponse, BaitongError> {
        let account = account.trim();
        if account.is_empty() {
            return Err(BaitongError::BadRequest("account is required".to_string()));
        }
        if password.is_empty() {
            return Err(BaitongError::BadRequest("password is required".to_string()));
        }

        let client = self.client().await?;
        let state = client.login(account, password).await?;
        let status = self.compose(state, true).await;
        // Login is the one place that turns the provider on: logging in means
        // "I want Baitong models", and logging out is how you turn them off.
        self.publish_managed(&status, Some(true)).await;
        Ok(status)
    }

    /// `POST /api/baitong/session/refresh`.
    ///
    /// Never returns an HTTP error for credential/network problems: the UI must
    /// branch on `status` while keeping the work surface mounted.
    pub async fn refresh(&self) -> Result<BaitongRefreshResponse, BaitongError> {
        // Refreshing is the recovery path for an expired cookie, so it may
        // start the adapter too — a stopped adapter is not "no credentials".
        let client = match self.client().await {
            Ok(client) => client,
            Err(error) => {
                let state = self.stopped_view(Some(error.to_string())).await;
                return Ok(BaitongRefreshResponse {
                    status: BaitongRefreshStatus::AdapterError,
                    message: Some(error.to_string()),
                    state,
                });
            }
        };

        let envelope = client.refresh().await?;
        let status = match envelope.status.as_str() {
            "refreshed" => BaitongRefreshStatus::Refreshed,
            "credentials_required" => BaitongRefreshStatus::CredentialsRequired,
            "invalid_credentials" => BaitongRefreshStatus::InvalidCredentials,
            "network_error" => BaitongRefreshStatus::NetworkError,
            _ => BaitongRefreshStatus::AdapterError,
        };

        Ok(BaitongRefreshResponse {
            status,
            message: envelope.detail,
            state: {
                let status = self.compose(envelope.state, true).await;
                // A refresh may restore a catalog the adapter lost (it restarts
                // the model service), so republish content; leave `enabled`
                // alone — refresh is not a login.
                self.publish_managed(&status, None).await;
                status
            },
        })
    }

    /// `POST /api/baitong/logout` — clear credentials and stop the adapter.
    pub async fn logout(&self) -> Result<BaitongStatusResponse, BaitongError> {
        if let Some(client) = self.client_if_running().await {
            let _ = client.clear_credentials().await;
        }
        let _ = self.supervisor.stop().await;
        self.pending_restore.store(false, Ordering::SeqCst);
        self.vision_mcp.lock().await.take();
        {
            let mut version = self.version.lock().await;
            *version = ConfigVersion::default();
        }
        let status = self.stopped_view(None).await;
        self.publish_managed(&status, Some(false)).await;
        Ok(status)
    }

    /// `PUT /api/baitong/config`.
    pub async fn update_config(&self, update: BaitongConfigUpdate) -> Result<BaitongConfigResponse, BaitongError> {
        {
            let version = self.version.lock().await;
            if let Some(expected) = update.expected_version
                && expected != version.version
            {
                return Err(BaitongError::ConfigConflict {
                    expected,
                    current: version.version,
                });
            }
        }

        if let Some(mode) = update.bind_mode.as_deref()
            && mode != "local"
            && mode != "lan"
        {
            return Err(BaitongError::BadRequest(format!(
                "bind_mode must be local or lan, got {mode}"
            )));
        }

        let client = self.client().await?;
        let body = serde_json::to_value(&update)
            .map_err(|error| BaitongError::Internal(format!("failed to encode config: {error}")))?;
        let envelope = client.put_config(body).await?;

        let mut version = self.version.lock().await;
        version.version += 1;
        version.last_applied_at = Some(now_stamp());
        let version_value = version.version;
        drop(version);

        let status = self.compose(envelope.state, true).await;
        // Config saves change the catalog; republish content, keep `enabled`.
        self.publish_managed(&status, None).await;

        Ok(BaitongConfigResponse {
            version: version_value,
            applied: true,
            restarted: envelope.restarted,
            errors: envelope.errors,
            state: status,
        })
    }

    /// `POST /api/baitong/adapter/{action}`.
    pub async fn adapter_action(
        &self,
        action: BaitongAdapterAction,
    ) -> Result<BaitongAdapterActionResponse, BaitongError> {
        match action {
            BaitongAdapterAction::Stop => {
                let _ = self.supervisor.stop().await;
                self.pending_restore.store(false, Ordering::SeqCst);
                self.vision_mcp.lock().await.take();
                return Ok(BaitongAdapterActionResponse {
                    state: self.stopped_view(None).await,
                    adapter_version: None,
                });
            }
            BaitongAdapterAction::Start | BaitongAdapterAction::Restart => {
                let client = self.client().await?;
                let envelope = client.adapter_action("restart").await?;
                let status = self.compose(envelope.state, true).await;
                // A restart can change the port and the catalog; republish
                // content. `enabled` stays as the user left it.
                self.publish_managed(&status, None).await;
                return Ok(BaitongAdapterActionResponse {
                    state: status,
                    adapter_version: envelope.version,
                });
            }
            BaitongAdapterAction::Test => {}
        }

        let client = self.client().await?;
        let envelope = client.adapter_action("test").await?;
        let status = self.compose(envelope.state, true).await;
        self.publish_managed(&status, None).await;
        Ok(BaitongAdapterActionResponse {
            state: status,
            adapter_version: envelope.version,
        })
    }

    pub async fn logs(&self) -> Result<BaitongLogsResponse, BaitongError> {
        let client = self.client().await?;
        let payload = client.logs().await?;
        serde_json::from_value(payload)
            .map_err(|error| BaitongError::Adapter(format!("unexpected logs payload: {error}")))
    }

    pub async fn mcp_snippet(&self) -> Result<String, BaitongError> {
        let client = self.client().await?;
        client.mcp_snippet().await
    }

    pub async fn write_opencode_config(&self) -> Result<String, BaitongError> {
        let client = self.client().await?;
        client.write_opencode_config().await
    }

    pub async fn open_opencode_folder(&self) -> Result<String, BaitongError> {
        let client = self.client().await?;
        client.open_opencode_folder().await
    }

    /// Stop the adapter on shutdown. Best effort.
    pub async fn shutdown(&self) {
        if let Err(error) = self.supervisor.stop().await {
            tracing::warn!(error = %error, stage = "baitong.shutdown", "failed to stop baitong adapter");
        }
    }

    async fn compose(&self, state: AdapterState, managed: bool) -> BaitongStatusResponse {
        let version = self.version.lock().await.clone();
        let credential_state = credential_state(&state.credentials.state);
        let running = state.adapter.state == "running";
        // The adapter reports what upstream calls actually proved. Wall-clock
        // expiry is never used: the machine clock is untrusted and Baitong
        // exposes no verifiable expiry instant.
        let cookie_state = cookie_state(state.credentials.cookie_state.as_deref());
        let session_state = if credential_state == BaitongCredentialState::Ready && running {
            match cookie_state {
                BaitongCookieState::Expired => BaitongSessionState::Expired,
                BaitongCookieState::Valid | BaitongCookieState::Unknown => BaitongSessionState::Authenticated,
            }
        } else {
            BaitongSessionState::Unknown
        };

        let session = BaitongSessionStatus {
            state: session_state,
            account: state.credentials.account.clone(),
            cookie_state,
            credential_state,
            last_refresh_at: version.last_applied_at.clone(),
        };

        let status = BaitongStatusResponse {
            adapter: BaitongAdapterStatus {
                state: if running {
                    BaitongAdapterState::Running
                } else {
                    BaitongAdapterState::Stopped
                },
                host: state.adapter.host.clone(),
                port: state.adapter.port,
                bind_mode: state.adapter.bind_mode.clone(),
                display_host: state.adapter.display_host.clone(),
                managed,
                last_error: state.adapter.last_error.clone(),
            },
            session,
            enterprise: BaitongEnterpriseStatus {
                state: BaitongEnterpriseState::Deferred,
            },
            models: BaitongModelCatalog {
                items: state.models.items.clone(),
                default: state.models.default.clone(),
                thinking: state.models.thinking.clone(),
                vision: BaitongVisionCatalog {
                    items: state.models.vision.items.clone(),
                    selected: state.models.vision.selected.clone(),
                },
                vision_mcp_enabled: state.config.vision_mcp_enabled,
            },
            config: BaitongConfigSnapshot {
                version: version.version,
                last_applied_at: version.last_applied_at,
                port: state.config.port,
                bind_mode: state.config.bind_mode.clone(),
                lan_host: state.config.lan_host.clone(),
                log_call: state.config.log_call,
                log_interaction: state.config.log_interaction,
            },
        };

        // Cache the vision endpoint from the same snapshot the agent factory
        // will read. `None` when the adapter is down, so a stopped adapter can
        // never leave a stale MCP URL pointing at a closed port.
        let cached = if status.adapter.state == BaitongAdapterState::Running {
            spec_from_status(&status).and_then(|spec| spec.vision_mcp)
        } else {
            None
        };
        *self.vision_mcp.lock().await = cached;

        status
    }

    /// View used when no adapter child is supervised.
    async fn stopped_view(&self, last_error: Option<String>) -> BaitongStatusResponse {
        let version = self.version.lock().await.clone();
        // No adapter, no MCP endpoint. Clearing here covers the paths that
        // report "stopped" without going through `compose` (spawn failure,
        // unreachable status read), so a later session build cannot be handed a
        // URL to a port nobody listens on.
        self.vision_mcp.lock().await.take();
        BaitongStatusResponse {
            adapter: BaitongAdapterStatus {
                // A failed spawn is reported as `unavailable` so the UI can
                // tell "not started yet" from "cannot start".
                state: if last_error.is_some() {
                    BaitongAdapterState::Unavailable
                } else {
                    BaitongAdapterState::Stopped
                },
                host: "127.0.0.1".to_string(),
                port: 8102,
                bind_mode: "local".to_string(),
                display_host: "localhost".to_string(),
                managed: true,
                last_error,
            },
            session: BaitongSessionStatus {
                state: BaitongSessionState::Unknown,
                account: None,
                cookie_state: BaitongCookieState::Unknown,
                credential_state: BaitongCredentialState::Absent,
                last_refresh_at: version.last_applied_at.clone(),
            },
            enterprise: BaitongEnterpriseStatus {
                state: BaitongEnterpriseState::Deferred,
            },
            models: BaitongModelCatalog {
                items: Vec::new(),
                default: None,
                thinking: Vec::new(),
                vision: BaitongVisionCatalog {
                    items: Vec::new(),
                    selected: None,
                },
                vision_mcp_enabled: true,
            },
            config: BaitongConfigSnapshot {
                version: version.version,
                last_applied_at: version.last_applied_at,
                port: 8102,
                bind_mode: "local".to_string(),
                lan_host: String::new(),
                log_call: false,
                log_interaction: false,
            },
        }
    }
}

fn credential_state(value: &str) -> BaitongCredentialState {
    match value {
        "ready" => BaitongCredentialState::Ready,
        "decrypt_failed" => BaitongCredentialState::DecryptFailed,
        "migration_failed" => BaitongCredentialState::MigrationFailed,
        _ => BaitongCredentialState::Absent,
    }
}

/// The adapter reports only what upstream calls proved; anything else (older
/// adapter build, no calls yet) stays `unknown` rather than guessing.
fn cookie_state(value: Option<&str>) -> BaitongCookieState {
    match value {
        Some("valid") => BaitongCookieState::Valid,
        Some("expired") => BaitongCookieState::Expired,
        _ => BaitongCookieState::Unknown,
    }
}

fn now_stamp() -> String {
    // Display-only stamp. Business expiry never depends on this value.
    chrono_lite_stamp()
}

fn chrono_lite_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix:{}", duration.as_secs()),
        Err(_) => "unix:0".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_states_map_to_stable_variants() {
        assert_eq!(credential_state("ready"), BaitongCredentialState::Ready);
        assert_eq!(
            credential_state("decrypt_failed"),
            BaitongCredentialState::DecryptFailed
        );
        assert_eq!(
            credential_state("migration_failed"),
            BaitongCredentialState::MigrationFailed
        );
        assert_eq!(credential_state("absent"), BaitongCredentialState::Absent);
        // Unknown adapter values must never be read as "ready".
        assert_eq!(credential_state("weird"), BaitongCredentialState::Absent);
    }

    #[test]
    fn cookie_states_are_only_what_upstream_proved() {
        assert_eq!(cookie_state(Some("valid")), BaitongCookieState::Valid);
        assert_eq!(cookie_state(Some("expired")), BaitongCookieState::Expired);
        // Missing (older adapter) or unrecognised values stay unknown: the UI
        // must not show "valid" without evidence.
        assert_eq!(cookie_state(None), BaitongCookieState::Unknown);
        assert_eq!(cookie_state(Some("stale")), BaitongCookieState::Unknown);
    }
}
