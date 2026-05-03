//! Baitong (百通) local adapter contracts — phase 1.
//!
//! Phase 1 keeps the trust boundary simple: AionCore talks to the local Python
//! adapter over loopback, and AionUi talks only to AionCore. No Casdoor fields
//! are queried; `enterprise` is a fixed `deferred` marker so the UI can render
//! the second-stage placeholder without a second request.

use serde::{Deserialize, Serialize};

/// Fixed provider row id that AionCore owns for the Baitong local adapter.
///
/// Not a UUID on purpose: a stable id is what lets the frontend recognise the
/// row as built-in (hide delete, keep the edit affordances narrow) and lets the
/// service layer reject destructive writes without a second marker column.
pub const BAITONG_PROVIDER_ID: &str = "baitong-builtin";

/// `platform` value for the built-in provider row.
///
/// Deliberately not `openai`: `map_aionrs_provider` falls through to the
/// per-model protocol for unknown platforms, which defaults to `openai`, so the
/// transport is identical while `platform` stays a usable discriminator for the
/// vision-MCP injection and the update guard.
pub const BAITONG_PLATFORM: &str = "baitong";

/// Placeholder API key for the built-in provider row.
///
/// The adapter is loopback-only and ignores `Authorization` on `/v1`, but the
/// provider contract requires a non-empty key, and the column is encrypted like
/// any other. This value proves "no credential of the user's is stored here".
pub const BAITONG_API_KEY_SENTINEL: &str = "baitong-local-no-key";

/// MCP server name used for the adapter's vision tools.
///
/// Matches the name the adapter itself advertises in its `mcp-snippet`, so a
/// user who added it manually keeps the same tool namespace after this becomes
/// automatic.
pub const BAITONG_VISION_MCP_NAME: &str = "vision";

/// Adapter process state as seen by AionCore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaitongAdapterState {
    Running,
    Stopped,
    Starting,
    Unavailable,
}

/// Where the adapter-side credential material stands.
///
/// `decrypt_failed` and `migration_failed` both mean "the user must type the
/// password again"; they are distinct so the UI can explain *why*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaitongCredentialState {
    Absent,
    Ready,
    DecryptFailed,
    MigrationFailed,
}

/// Baitong session (cookie) state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaitongSessionState {
    Authenticated,
    Expired,
    Unknown,
}

/// Cookie freshness as far as the adapter can prove it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaitongCookieState {
    Valid,
    Expired,
    Unknown,
}

/// Enterprise authorization is deferred to phase 2; phase 1 only reports that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaitongEnterpriseState {
    Deferred,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongAdapterStatus {
    pub state: BaitongAdapterState,
    /// Actual listen address of the OpenAI-compatible model service.
    pub host: String,
    pub port: u16,
    /// `local` or `lan`.
    pub bind_mode: String,
    /// Address to show the user / paste into external tools.
    pub display_host: String,
    /// `true` when AionCore spawned and supervises the adapter process.
    pub managed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongSessionStatus {
    pub state: BaitongSessionState,
    /// Display account (never the password, never the raw cookie).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub cookie_state: BaitongCookieState,
    pub credential_state: BaitongCredentialState,
    /// Display-only timestamp; never used to decide expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongEnterpriseStatus {
    pub state: BaitongEnterpriseState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongVisionCatalog {
    pub items: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongModelCatalog {
    pub items: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    pub thinking: Vec<String>,
    pub vision: BaitongVisionCatalog,
    pub vision_mcp_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongConfigSnapshot {
    /// Monotonic counter bumped on every successful `PUT /api/baitong/config`.
    pub version: u64,
    /// Display-only; never used to decide expiry or ordering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_applied_at: Option<String>,
    pub port: u16,
    pub bind_mode: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub lan_host: String,
    pub log_call: bool,
    pub log_interaction: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongStatusResponse {
    pub adapter: BaitongAdapterStatus,
    pub session: BaitongSessionStatus,
    pub enterprise: BaitongEnterpriseStatus,
    pub models: BaitongModelCatalog,
    pub config: BaitongConfigSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongLoginRequest {
    pub account: String,
    /// Only forwarded to the local adapter for this one request; never logged.
    pub password: String,
    /// Phase 1 always keeps the login; accepted for forward compatibility.
    #[serde(default = "default_true")]
    pub remember: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaitongRefreshStatus {
    Refreshed,
    CredentialsRequired,
    InvalidCredentials,
    NetworkError,
    AdapterError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongRefreshResponse {
    pub status: BaitongRefreshStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub state: BaitongStatusResponse,
}

/// Full runtime snapshot pushed to the adapter. Every field is optional so a
/// partial save does not clobber unrelated settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaitongConfigUpdate {
    pub models: Option<Vec<String>>,
    pub default_model: Option<String>,
    pub thinking_models: Option<Vec<String>>,
    pub vision_models: Option<Vec<String>>,
    pub vision_model: Option<String>,
    pub vision_mcp_enabled: Option<bool>,
    pub port: Option<u16>,
    pub bind_mode: Option<String>,
    pub lan_host: Option<String>,
    pub log_call: Option<bool>,
    pub log_interaction: Option<bool>,
    /// Optimistic concurrency: when present and stale, the save is rejected.
    pub expected_version: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongConfigResponse {
    pub version: u64,
    pub applied: bool,
    pub restarted: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    pub state: BaitongStatusResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaitongAdapterAction {
    Start,
    Stop,
    Restart,
    Test,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongAdapterActionResponse {
    pub state: BaitongStatusResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongLogEntry {
    #[serde(flatten)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongLogsResponse {
    pub entries: Vec<BaitongLogEntry>,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaitongMcpSnippetResponse {
    pub snippet: String,
}
