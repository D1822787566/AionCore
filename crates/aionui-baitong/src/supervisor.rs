//! Adapter process supervision: spawn, health-gate, stop.
//!
//! Only this crate spawns the adapter. AionUi never launches Python and never
//! calls the model service management routes directly.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::error::BaitongError;
use crate::launch::AdapterLaunch;

/// Port the adapter's own control plane listens on.
pub const DEFAULT_CONTROL_PORT: u16 = 8103;

const SPAWN_ATTEMPTS: usize = 40;
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(250);

struct Supervised {
    /// `None` for an adapter this process did not spawn (test harness, or a
    /// development adapter started by hand). Such an entry is supervised for its
    /// token and port only: stopping it releases the entry without signalling a
    /// process we do not own.
    child: Option<Child>,
    control_port: u16,
    token: Arc<str>,
}

/// Owns the adapter child process and the shared control token.
pub struct AdapterSupervisor {
    launch: AdapterLaunch,
    config_dir: PathBuf,
    /// Port the adapter's control plane listens on. Fixed in production; a
    /// builder override exists so tests can drive a real HTTP adapter on an
    /// ephemeral port instead of racing for the well-known one.
    control_port: u16,
    inner: Mutex<Option<Supervised>>,
    client: reqwest::Client,
}

impl AdapterSupervisor {
    pub fn new(launch: AdapterLaunch, config_dir: PathBuf) -> Self {
        Self {
            launch,
            config_dir,
            control_port: DEFAULT_CONTROL_PORT,
            inner: Mutex::new(None),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Override the control port. Only for tests and development harnesses.
    pub fn with_control_port(mut self, control_port: u16) -> Self {
        self.control_port = control_port;
        self
    }

    pub fn control_port(&self) -> u16 {
        self.control_port
    }

    /// Ensure the adapter is running. Returns the control token.
    ///
    /// Idempotent: a live child whose health check still answers is reused.
    pub async fn ensure_running(&self) -> Result<Arc<str>, BaitongError> {
        let mut guard = self.inner.lock().await;

        if let Some(supervised) = guard.as_mut() {
            let healthy = self.probe(&supervised.token, supervised.control_port).await;
            if healthy {
                return Ok(supervised.token.clone());
            }
            // Dead or wedged: reap before respawning so the port is free.
            if let Some(child) = supervised.child.as_mut() {
                let _ = child.kill().await;
            }
            *guard = None;
        }

        let token = generate_token()?;
        let mut child = spawn_child(&self.launch, &self.config_dir, &token, self.control_port)?;

        for _ in 0..SPAWN_ATTEMPTS {
            if self.probe(&token, self.control_port).await {
                *guard = Some(Supervised {
                    child: Some(child),
                    control_port: self.control_port,
                    token: token.clone(),
                });
                tracing::info!(stage = "baitong.adapter.spawn", "baitong adapter is ready");
                return Ok(token);
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|error| BaitongError::Adapter(format!("failed to poll adapter process: {error}")))?
            {
                return Err(BaitongError::Adapter(format!(
                    "adapter exited during startup with {status}"
                )));
            }
            sleep(SPAWN_POLL_INTERVAL).await;
        }

        let _ = child.kill().await;
        Err(BaitongError::Unreachable(
            "adapter did not answer /control/health in time".to_string(),
        ))
    }

    /// Current token, if a child is supervised.
    pub async fn token(&self) -> Option<Arc<str>> {
        self.inner.lock().await.as_ref().map(|s| s.token.clone())
    }

    /// Adopt an adapter that is already answering on `control_port`.
    ///
    /// Used by test harnesses and by development setups that start the adapter
    /// themselves. Refuses a port that does not answer, so a stale entry can
    /// never make `ensure_running` skip the spawn it should have done.
    #[doc(hidden)]
    pub async fn adopt_running(&self, control_port: u16, token: impl Into<Arc<str>>) -> Result<(), BaitongError> {
        let token = token.into();
        if !self.probe(&token, control_port).await {
            return Err(BaitongError::Unreachable(format!(
                "no adapter answering on control port {control_port}"
            )));
        }
        *self.inner.lock().await = Some(Supervised {
            child: None,
            control_port,
            token,
        });
        Ok(())
    }

    /// Stop the adapter. Safe when nothing is running.
    pub async fn stop(&self) -> Result<(), BaitongError> {
        let mut guard = self.inner.lock().await;
        if let Some(mut supervised) = guard.take() {
            if let Some(child) = supervised.child.as_mut() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            tracing::info!(stage = "baitong.adapter.stop", "baitong adapter stopped");
        }
        Ok(())
    }

    /// Restart, keeping the same plan.
    pub async fn restart(&self) -> Result<Arc<str>, BaitongError> {
        self.stop().await?;
        self.ensure_running().await
    }

    async fn probe(&self, token: &str, port: u16) -> bool {
        let url = format!("http://127.0.0.1:{port}/control/health");
        match self.client.get(url).header("X-Baitong-Token", token).send().await {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }
}

fn spawn_child(
    launch: &AdapterLaunch,
    config_dir: &std::path::Path,
    token: &str,
    control_port: u16,
) -> Result<Child, BaitongError> {
    let mut builder = aionui_runtime::Builder::new(&launch.program);
    builder.args(launch.args.iter().map(String::as_str));
    builder.env("BAITONG_CONFIG_DIR", config_dir.as_os_str());
    builder.env("BAITONG_CONTROL_TOKEN", token);
    builder.env("BAITONG_CONTROL_PORT", control_port.to_string());
    if let Some(cwd) = &launch.cwd {
        builder.current_dir(cwd);
    }
    builder.stdout(std::process::Stdio::null());
    builder.stderr(std::process::Stdio::null());

    builder.spawn().map_err(|error| {
        BaitongError::Adapter(format!("failed to spawn adapter {}: {error}", launch.program.display()))
    })
}

fn generate_token() -> Result<Arc<str>, BaitongError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| BaitongError::Internal(format!("failed to generate control token: {error}")))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(Arc::from(hex))
}
