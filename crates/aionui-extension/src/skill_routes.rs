#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path as AxumPath, State};
use axum::routing::{delete, get, post};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use aionui_api_types::{
    AddExternalPathRequest, ApiResponse, CapabilityCatalogSnapshotResponse, CapabilityFactoryStatusResponse,
    CapabilityInstallationResponse, CapabilityPlatformCatalogItem, CapabilityPlatformCatalogRequest,
    CapabilityPlatformCatalogResponse, CapabilityPlatformHealthResponse, CapabilityPlatformSkillFile,
    CapabilityPlatformSkillFilePreview, ExportSkillRequest, ExternalSkillSourceResponse,
    ImportCapabilityPlatformSkillRequest, ImportSkillFailureResponse, ImportSkillRequest, ImportSkillResponse,
    InstallCapabilityRequest, ListCapabilityPlatformSkillFilesRequest, MaterializeSkillsRequest,
    MaterializeSkillsResponse, MaterializedSkillRef, NamedPathResponse, ReadAssistantRuleRequest,
    ReadBuiltinResourceRequest, ReadCapabilityPlatformSkillFileRequest, ReadCapabilityPlatformSkillRequest,
    ReadSkillInfoRequest, ReadSkillInfoResponse, RemoveExternalPathRequest, ScanForSkillsRequest,
    ScanForSkillsResponse, ScannedSkillResponse, SkillImportLimitsResponse, SkillImportRecordResponse,
    SkillListItemResponse, SkillPathsResponse, SkillSourceResponse, UpdateCapabilityRequest, WriteAssistantRuleRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::{
    CreateMcpServerParams, IClientPreferenceRepository, IMcpServerRepository, ISkillRepository, IUserRepository,
    UpdateMcpServerParams,
};

use crate::classifier::AssistantRuleDispatcher;
use crate::error::ExtensionError;
use crate::external_paths::ExternalPathsManager;
use crate::skill_service::{self, SkillPaths, SkillSource};

const CAPABILITY_PLATFORM_TIMEOUT: Duration = Duration::from_secs(10);
const CAPABILITY_PLATFORM_RESPONSE_LIMIT: usize = 75 * 1024 * 1024;
const CAPABILITY_PLATFORM_PREVIEW_RESPONSE_LIMIT: usize = 1024 * 1024;
const CAPABILITY_FACTORY_URL_KEY: &str = "capabilityFactory.url";
const CAPABILITY_FACTORY_STATE_KEY: &str = "capabilityFactory.state";
const CAPABILITY_FACTORY_CATALOG_KEY: &str = "capabilityFactory.catalog";
const CAPABILITY_FACTORY_INSTALLATIONS_KEY: &str = "capabilityFactory.installations";
const CAPABILITY_FACTORY_MCP_BACKUPS_KEY: &str = "capabilityFactory.mcpBackups";
const CAPABILITY_FACTORY_SYNC_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const CAPABILITY_FACTORY_STARTUP_SYNC_DELAY: Duration = Duration::from_secs(10);

#[derive(serde::Deserialize)]
struct CapabilityFactoryTestRequest {
    url: String,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct CapabilityFactoryState {
    url: Option<String>,
    last_attempted_at: Option<i64>,
    last_successful_sync_at: Option<i64>,
    catalog_version: Option<String>,
    etag: Option<String>,
    last_error_code: Option<String>,
    last_error_message: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CapabilityCatalogCache {
    catalog: CapabilityPlatformCatalogResponse,
    synced_at: i64,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct CapabilityPlatformSkillPackageResponse {
    files: Vec<CapabilityPlatformSkillFile>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CapabilityMcpBackup {
    version: String,
    artifact_sha256: Option<String>,
    description: Option<String>,
    enabled: bool,
    transport_type: String,
    transport_config: String,
    tools: Option<String>,
    original_json: Option<String>,
    builtin: bool,
}

fn to_source_response(source: SkillSource) -> SkillSourceResponse {
    match source {
        SkillSource::Builtin => SkillSourceResponse::Builtin,
        SkillSource::Custom => SkillSourceResponse::Custom,
        SkillSource::Cron => SkillSourceResponse::Cron,
        SkillSource::Extension => SkillSourceResponse::Extension,
    }
}

fn is_auto_inject_builtin_skill(source: SkillSource, relative_location: Option<&str>) -> bool {
    source == SkillSource::Builtin && relative_location.is_some_and(|location| location.starts_with("auto-inject/"))
}

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for skill/rule route handlers.
#[derive(Clone)]
pub struct SkillRouterState {
    pub skill_paths: SkillPaths,
    pub skill_repo: Arc<dyn ISkillRepository>,
    pub mcp_repo: Arc<dyn IMcpServerRepository>,
    pub client_preference_repo: Arc<dyn IClientPreferenceRepository>,
    pub external_paths_manager: Arc<ExternalPathsManager>,
    /// Optional dispatcher that routes assistant-rule / assistant-skill
    /// read/write/delete by source (builtin / extension / user). When
    /// `None`, the legacy user-directory-only behavior is preserved.
    #[allow(clippy::type_complexity)]
    pub assistant_dispatcher: Option<Arc<dyn AssistantRuleDispatcher>>,
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the skill router with all `/api/skills/*` routes.
///
/// All routes require authentication (applied by the caller).
pub fn skill_routes(state: SkillRouterState) -> Router {
    Router::new()
        // Skill listing & info
        .route("/api/skills", get(list_skills))
        .route("/api/skills/import-history", get(list_import_history))
        .route("/api/skills/import-limits", get(get_import_limits))
        .route("/api/skills/info", post(read_skill_info))
        .route("/api/skills/paths", get(get_skill_paths))
        .route("/api/capability-factory/status", get(get_capability_factory_status))
        .route("/api/capability-factory/test", post(test_capability_factory))
        .route("/api/capability-factory/sync", post(sync_capability_factory))
        .route("/api/capabilities/catalog", get(get_capability_catalog))
        .route("/api/capabilities/installations", get(get_capability_installations))
        .route("/api/capabilities/install", post(install_capability))
        .route(
            "/api/capabilities/{installation_id}/check",
            post(check_capability_installation),
        )
        .route(
            "/api/capabilities/{installation_id}/update",
            post(update_capability_installation),
        )
        .route(
            "/api/capabilities/{installation_id}/rollback",
            post(rollback_capability_installation),
        )
        .route(
            "/api/skills/capability-platform/catalog",
            post(list_capability_platform_catalog),
        )
        .route(
            "/api/skills/capability-platform/health",
            post(get_capability_platform_health),
        )
        // Import / export / delete
        .route("/api/skills/import", post(import_skill))
        .route(
            "/api/skills/capability-platform/import",
            post(import_capability_platform_skill),
        )
        .route(
            "/api/skills/capability-platform/skill",
            post(read_capability_platform_skill),
        )
        .route(
            "/api/skills/capability-platform/skill-files",
            post(list_capability_platform_skill_files),
        )
        .route(
            "/api/skills/capability-platform/skill-file",
            post(read_capability_platform_skill_file),
        )
        .route("/api/skills/export-symlink", post(export_skill_symlink))
        .route("/api/skills/{name}", delete(delete_skill))
        // Scanning & discovery
        .route("/api/skills/scan", post(scan_for_skills))
        .route("/api/skills/detect-paths", get(detect_paths))
        .route("/api/skills/detect-external", get(detect_external))
        // Built-in resources
        .route("/api/skills/builtin-rule", post(read_builtin_rule))
        .route("/api/skills/builtin-skill", post(read_builtin_skill))
        // Per-agent skill resolution (for agent CLI symlink layout).
        .route("/api/skills/materialize-for-agent", post(materialize_for_agent))
        // Assistant rules CRUD
        .route("/api/skills/assistant-rule/read", post(read_assistant_rule))
        .route("/api/skills/assistant-rule/write", post(write_assistant_rule))
        .route("/api/skills/assistant-rule/{id}", delete(delete_assistant_rule))
        // Assistant skills CRUD
        .route("/api/skills/assistant-skill/read", post(read_assistant_skill))
        .route("/api/skills/assistant-skill/write", post(write_assistant_skill))
        .route("/api/skills/assistant-skill/{id}", delete(delete_assistant_skill))
        // External path management
        .route(
            "/api/skills/external-paths",
            get(get_external_paths)
                .post(add_external_path)
                .delete(remove_external_path),
        )
        // Skills market
        .route("/api/skills/market/enable", post(enable_skills_market))
        .route("/api/skills/market/disable", post(disable_skills_market))
        .with_state(state)
}

/// Start the delayed and periodic directory refresh for every configured user.
/// Synchronization only updates cached catalog state; it never changes a local
/// capability. The runtime owns cancellation when the process exits.
pub fn spawn_capability_factory_sync(state: SkillRouterState, user_repo: Arc<dyn IUserRepository>) {
    tokio::spawn(async move {
        tokio::time::sleep(CAPABILITY_FACTORY_STARTUP_SYNC_DELAY).await;
        loop {
            match user_repo.list_users().await {
                Ok(users) => {
                    for user in users {
                        if synchronize_capability_factory_for_user(&state, &user.id, false)
                            .await
                            .is_err()
                        {
                            warn!(user_id = %user.id, "scheduled capability factory synchronization failed");
                        }
                    }
                }
                Err(_) => warn!("scheduled capability factory synchronization could not list users"),
            }
            tokio::time::sleep(CAPABILITY_FACTORY_SYNC_INTERVAL).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Skill listing & info
// ---------------------------------------------------------------------------

/// `GET /api/skills` — list all available skills.
async fn list_skills(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<SkillListItemResponse>>>, ApiError> {
    let items = skill_service::list_available_skills_with_repo_for_user(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &current_user.id,
    )
    .await?;
    let resp: Vec<SkillListItemResponse> = items
        .into_iter()
        .map(|s| SkillListItemResponse {
            is_auto_inject: is_auto_inject_builtin_skill(s.source, s.relative_location.as_deref()),
            name: s.name,
            description: s.description,
            location: s.location,
            relative_location: s.relative_location,
            is_custom: s.is_custom,
            source: to_source_response(s.source),
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `POST /api/skills/info` — read skill info without importing.
async fn read_skill_info(
    body: Result<Json<ReadSkillInfoRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ReadSkillInfoResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let (name, description) = skill_service::read_skill_info(Path::new(&req.skill_path)).await?;
    Ok(Json(ApiResponse::ok(ReadSkillInfoResponse { name, description })))
}

/// `GET /api/skills/paths` — get user and built-in skill directories.
async fn get_skill_paths(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<SkillPathsResponse>>, ApiError> {
    let (user_dir, builtin_dir) = skill_service::get_skill_paths(&state.skill_paths);
    Ok(Json(ApiResponse::ok(SkillPathsResponse {
        user_skills_dir: user_dir,
        builtin_skills_dir: builtin_dir,
    })))
}

/// `GET /api/skills/import-limits` — get server-side skill import limits.
async fn get_import_limits() -> Result<Json<ApiResponse<SkillImportLimitsResponse>>, ApiError> {
    let limits = skill_service::skill_import_limits();
    Ok(Json(ApiResponse::ok(SkillImportLimitsResponse {
        max_file_bytes: limits.max_file_bytes,
        max_total_bytes: limits.max_total_bytes,
    })))
}

async fn get_capability_factory_status(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<CapabilityFactoryStatusResponse>>, ApiError> {
    let factory = load_factory_state(&state, &current_user.id).await?;
    Ok(Json(ApiResponse::ok(CapabilityFactoryStatusResponse {
        url: factory.url,
        sync_enabled: true,
        sync_interval_minutes: 360,
        last_attempted_at: factory.last_attempted_at,
        last_successful_sync_at: factory.last_successful_sync_at,
        catalog_version: factory.catalog_version,
        etag: factory.etag,
        last_error_code: factory.last_error_code,
        last_error_message: factory.last_error_message,
    })))
}

async fn test_capability_factory(
    body: Result<Json<CapabilityFactoryTestRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CapabilityPlatformHealthResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let health = fetch_capability_platform_json::<CapabilityPlatformHealthResponse>(capability_platform_url(
        &request.url,
        "/health",
    )?)
    .await?;
    if health.status != "ok" {
        return Err(ExtensionError::InvalidRequest("capability factory health check failed".to_string()).into());
    }
    Ok(Json(ApiResponse::ok(health)))
}

/// Synchronize the only factory into the per-user local cache. A failed read
/// never replaces the last known good cache or local installation records.
async fn sync_capability_factory(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<CapabilityCatalogSnapshotResponse>>, ApiError> {
    synchronize_capability_factory_for_user(&state, &current_user.id, true)
        .await?
        .map(|snapshot| Json(ApiResponse::ok(snapshot)))
        .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_FACTORY_NOT_CONFIGURED".to_string()).into())
}

async fn synchronize_capability_factory_for_user(
    state: &SkillRouterState,
    user_id: &str,
    require_configuration: bool,
) -> Result<Option<CapabilityCatalogSnapshotResponse>, ApiError> {
    let mut factory = load_factory_state(state, user_id).await?;
    let Some(url) = factory.url.clone() else {
        return if require_configuration {
            Err(ExtensionError::InvalidRequest("CAPABILITY_FACTORY_NOT_CONFIGURED".to_string()).into())
        } else {
            Ok(None)
        };
    };
    factory.last_attempted_at = Some(aionui_common::now_ms());
    match fetch_capability_platform_catalog_with_etag(
        capability_platform_url(&url, "/api/catalog")?,
        factory.etag.as_deref(),
    )
    .await
    {
        Ok(CapabilityPlatformCatalogFetch::Modified { catalog, etag }) => {
            let synced_at = aionui_common::now_ms();
            factory.last_successful_sync_at = Some(synced_at);
            factory.catalog_version = catalog.catalog_version.clone();
            factory.etag = etag;
            factory.last_error_code = None;
            factory.last_error_message = None;
            save_preference(
                state,
                user_id,
                CAPABILITY_FACTORY_CATALOG_KEY,
                &CapabilityCatalogCache { catalog, synced_at },
            )
            .await?;
            save_preference(state, user_id, CAPABILITY_FACTORY_STATE_KEY, &factory).await?;
        }
        Ok(CapabilityPlatformCatalogFetch::NotModified { etag }) => {
            if load_preference::<CapabilityCatalogCache>(state, user_id, CAPABILITY_FACTORY_CATALOG_KEY)
                .await?
                .is_none()
            {
                return Err(ExtensionError::InvalidRequest("CAPABILITY_CATALOG_CACHE_MISSING".to_string()).into());
            }
            factory.last_successful_sync_at = Some(aionui_common::now_ms());
            factory.etag = etag.or(factory.etag);
            factory.last_error_code = None;
            factory.last_error_message = None;
            save_preference(state, user_id, CAPABILITY_FACTORY_STATE_KEY, &factory).await?;
        }
        Err(error) => {
            factory.last_error_code = Some("CAPABILITY_FACTORY_UNAVAILABLE".to_string());
            factory.last_error_message = Some(error.to_string());
            save_preference(state, user_id, CAPABILITY_FACTORY_STATE_KEY, &factory).await?;
            return Err(error.into());
        }
    }
    capability_catalog_snapshot(state, user_id).await.map(Some)
}

async fn get_capability_catalog(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<CapabilityCatalogSnapshotResponse>>, ApiError> {
    capability_catalog_snapshot(&state, &current_user.id)
        .await
        .map(|snapshot| Json(ApiResponse::ok(snapshot)))
}

async fn get_capability_installations(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<CapabilityInstallationResponse>>>, ApiError> {
    let cache =
        load_preference::<CapabilityCatalogCache>(&state, &current_user.id, CAPABILITY_FACTORY_CATALOG_KEY).await?;
    let installations = load_preference::<Vec<CapabilityInstallationResponse>>(
        &state,
        &current_user.id,
        CAPABILITY_FACTORY_INSTALLATIONS_KEY,
    )
    .await?
    .unwrap_or_default();
    let installations = compute_installation_states(
        installations,
        cache.as_ref(),
        load_factory_state(&state, &current_user.id)
            .await?
            .last_error_code
            .is_some(),
    );
    Ok(Json(ApiResponse::ok(
        annotate_local_installation_states(&state, &current_user.id, installations).await?,
    )))
}

/// Install a cached published release and persist its stable factory identity.
async fn install_capability(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<InstallCapabilityRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CapabilityInstallationResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    if !matches!(request.kind.as_str(), "skill" | "mcp") || !is_capability_platform_id(&request.capability_id) {
        return Err(ExtensionError::InvalidRequest("invalid capability identity".to_string()).into());
    }
    let factory = load_factory_state(&state, &current_user.id).await?;
    let url = factory
        .url
        .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_FACTORY_NOT_CONFIGURED".to_string()))?;
    let cache = load_preference::<CapabilityCatalogCache>(&state, &current_user.id, CAPABILITY_FACTORY_CATALOG_KEY)
        .await?
        .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_CATALOG_NOT_SYNCED".to_string()))?;
    let item = cache
        .catalog
        .items
        .iter()
        .find(|item| item.kind == request.kind && item.id == request.capability_id)
        .cloned()
        .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_NOT_FOUND".to_string()))?;
    let mut installations = load_preference::<Vec<CapabilityInstallationResponse>>(
        &state,
        &current_user.id,
        CAPABILITY_FACTORY_INSTALLATIONS_KEY,
    )
    .await?
    .unwrap_or_default();
    if installations
        .iter()
        .any(|entry| entry.kind == item.kind && entry.capability_id == item.id)
    {
        return Err(ExtensionError::InvalidRequest("CAPABILITY_ALREADY_INSTALLED".to_string()).into());
    }
    let local_resource_id = if item.kind == "skill" {
        let package_url = capability_platform_url(
            &url,
            &format!("/api/skills/{}/versions/{}/package", item.id, item.version),
        )?;
        let package = fetch_capability_platform_json::<CapabilityPlatformSkillPackageResponse>(package_url).await?;
        let expected_hash = item
            .artifact_sha256
            .as_deref()
            .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_ARTIFACT_HASH_MISSING".to_string()))?;
        let actual_hash = capability_package_sha256(&package.files)?;
        if !actual_hash.eq_ignore_ascii_case(expected_hash) {
            return Err(ExtensionError::InvalidRequest("CAPABILITY_ARTIFACT_HASH_MISMATCH".to_string()).into());
        }
        let skill_name = capability_package_skill_name(&package.files)?;
        if state
            .skill_repo
            .find_by_name_for_user(&current_user.id, &skill_name)
            .await
            .map_err(ExtensionError::from)?
            .is_some()
        {
            return Err(ExtensionError::InvalidRequest("CAPABILITY_LOCAL_NAME_CONFLICT".to_string()).into());
        }
        let outcome = skill_service::import_capability_platform_skill_with_repo_for_user(
            &state.skill_paths,
            state.skill_repo.as_ref(),
            &current_user.id,
            &package.files,
        )
        .await?;
        if !outcome.failed.is_empty() || outcome.imported.len() != 1 {
            return Err(ExtensionError::InvalidRequest("CAPABILITY_INSTALL_FAILED".to_string()).into());
        }
        outcome.imported.into_iter().next().unwrap_or_default()
    } else {
        let config = item
            .config
            .as_ref()
            .filter(|config| config.transport == "streamable-http" && !config.url.trim().is_empty())
            .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_MCP_TRANSPORT_UNSUPPORTED".to_string()))?;
        if state
            .mcp_repo
            .find_by_name(&current_user.id, &item.name)
            .await
            .map_err(ExtensionError::from)?
            .is_some()
        {
            return Err(ExtensionError::InvalidRequest("CAPABILITY_LOCAL_NAME_CONFLICT".to_string()).into());
        }
        let transport_config = serde_json::json!({ "url": config.url }).to_string();
        let original_json = serde_json::json!({
            "capability_factory": { "id": item.id, "version": item.version },
            "transport": { "transport": config.transport, "url": config.url },
        })
        .to_string();
        state
            .mcp_repo
            .create(CreateMcpServerParams {
                user_id: &current_user.id,
                name: &item.name,
                description: Some(&item.description),
                enabled: true,
                transport_type: "http",
                transport_config: &transport_config,
                tools: None,
                original_json: Some(&original_json),
                builtin: false,
            })
            .await
            .map_err(ExtensionError::from)?
            .id
    };
    let now = aionui_common::now_ms();
    let installation = CapabilityInstallationResponse {
        installation_id: format!("{}:{}", item.kind, item.id),
        kind: item.kind,
        capability_id: item.id,
        local_resource_id,
        installed_version: item.version,
        artifact_sha256: item.artifact_sha256,
        factory_url_snapshot: url,
        installed_at: now,
        updated_at: now,
        last_checked_at: Some(now),
        update_policy: "notify".to_string(),
        ignored_version: None,
        backup_path: None,
        backup_version: None,
        backup_artifact_sha256: None,
        last_error_code: None,
        last_error_message: None,
        state: "installed".to_string(),
    };
    installations.push(installation.clone());
    save_preference(
        &state,
        &current_user.id,
        CAPABILITY_FACTORY_INSTALLATIONS_KEY,
        &installations,
    )
    .await?;
    Ok(Json(ApiResponse::ok(installation)))
}

async fn check_capability_installation(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    AxumPath(installation_id): AxumPath<String>,
) -> Result<Json<ApiResponse<CapabilityInstallationResponse>>, ApiError> {
    let snapshot = capability_catalog_snapshot(&state, &current_user.id).await?;
    let checked = snapshot
        .installations
        .into_iter()
        .find(|installation| installation.installation_id == installation_id)
        .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_INSTALLATION_NOT_FOUND".to_string()))?;
    let mut installations = load_preference::<Vec<CapabilityInstallationResponse>>(
        &state,
        &current_user.id,
        CAPABILITY_FACTORY_INSTALLATIONS_KEY,
    )
    .await?
    .unwrap_or_default();
    let stored = installations
        .iter_mut()
        .find(|installation| installation.installation_id == installation_id)
        .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_INSTALLATION_NOT_FOUND".to_string()))?;
    stored.last_checked_at = Some(aionui_common::now_ms());
    stored.state = checked.state;
    let response = stored.clone();
    save_preference(
        &state,
        &current_user.id,
        CAPABILITY_FACTORY_INSTALLATIONS_KEY,
        &installations,
    )
    .await?;
    Ok(Json(ApiResponse::ok(response)))
}

/// Replace an installed release with the newer cached release. Skill updates
/// reject unrecorded local edits unless the caller explicitly confirms them.
async fn update_capability_installation(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    AxumPath(installation_id): AxumPath<String>,
    body: Result<Json<UpdateCapabilityRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CapabilityInstallationResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let mut installations = load_capability_installations(&state, &current_user.id).await?;
    let index = installations
        .iter()
        .position(|installation| installation.installation_id == installation_id)
        .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_INSTALLATION_NOT_FOUND".to_string()))?;
    let previous = installations[index].clone();
    let item = cached_capability_item(&state, &current_user.id, &previous).await?;
    let local_version = semver::Version::parse(&previous.installed_version)
        .map_err(|_| ExtensionError::InvalidRequest("CAPABILITY_LOCAL_VERSION_INVALID".to_string()))?;
    let remote_version = semver::Version::parse(&item.version)
        .map_err(|_| ExtensionError::InvalidRequest("CAPABILITY_REMOTE_VERSION_INVALID".to_string()))?;
    if remote_version <= local_version {
        return Err(ExtensionError::InvalidRequest("CAPABILITY_UPDATE_NOT_AVAILABLE".to_string()).into());
    }

    let now = aionui_common::now_ms();
    if previous.kind == "skill" {
        let row = state
            .skill_repo
            .find_by_name_for_user(&current_user.id, &previous.local_resource_id)
            .await
            .map_err(ExtensionError::from)?
            .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_LOCAL_RESOURCE_MISSING".to_string()))?;
        let expected = previous
            .artifact_sha256
            .as_deref()
            .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_LOCAL_INTEGRITY_UNKNOWN".to_string()))?;
        let actual = capability_directory_sha256(Path::new(&row.path)).await?;
        if !actual.eq_ignore_ascii_case(expected) && !request.replace_locally_modified {
            return Err(ExtensionError::InvalidRequest("CAPABILITY_LOCALLY_MODIFIED".to_string()).into());
        }
        let factory = load_factory_state(&state, &current_user.id).await?;
        let url = factory
            .url
            .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_FACTORY_NOT_CONFIGURED".to_string()))?;
        let files = fetch_verified_skill_package(&url, &item).await?;
        if capability_package_skill_name(&files)? != previous.local_resource_id {
            return Err(ExtensionError::InvalidRequest("CAPABILITY_LOCAL_NAME_CONFLICT".to_string()).into());
        }
        let backup = skill_service::replace_capability_platform_skill_with_repo_for_user(
            &state.skill_paths,
            state.skill_repo.as_ref(),
            &current_user.id,
            &previous.local_resource_id,
            &files,
        )
        .await?;
        let updated = &mut installations[index];
        updated.installed_version = item.version;
        updated.artifact_sha256 = item.artifact_sha256;
        updated.updated_at = now;
        updated.last_checked_at = Some(now);
        updated.backup_path = Some(backup.to_string_lossy().to_string());
        updated.backup_version = Some(previous.installed_version);
        updated.backup_artifact_sha256 = previous.artifact_sha256;
        updated.last_error_code = None;
        updated.last_error_message = None;
        updated.state = "installed".to_string();
    } else {
        let config = item
            .config
            .as_ref()
            .filter(|config| config.transport == "streamable-http" && !config.url.trim().is_empty())
            .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_MCP_TRANSPORT_UNSUPPORTED".to_string()))?;
        let row = state
            .mcp_repo
            .find_by_id(&current_user.id, &previous.local_resource_id)
            .await
            .map_err(ExtensionError::from)?
            .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_LOCAL_RESOURCE_MISSING".to_string()))?;
        let mut backups = load_preference::<HashMap<String, CapabilityMcpBackup>>(
            &state,
            &current_user.id,
            CAPABILITY_FACTORY_MCP_BACKUPS_KEY,
        )
        .await?
        .unwrap_or_default();
        backups.insert(
            previous.installation_id.clone(),
            CapabilityMcpBackup {
                version: previous.installed_version.clone(),
                artifact_sha256: previous.artifact_sha256.clone(),
                description: row.description.clone(),
                enabled: row.enabled,
                transport_type: row.transport_type.clone(),
                transport_config: row.transport_config.clone(),
                tools: row.tools.clone(),
                original_json: row.original_json.clone(),
                builtin: row.builtin,
            },
        );
        save_preference(&state, &current_user.id, CAPABILITY_FACTORY_MCP_BACKUPS_KEY, &backups).await?;
        let transport_config = serde_json::json!({ "url": config.url }).to_string();
        let original_json = serde_json::json!({
            "capability_factory": { "id": item.id, "version": item.version },
            "transport": { "transport": config.transport, "url": config.url },
        })
        .to_string();
        state
            .mcp_repo
            .update(
                &current_user.id,
                &previous.local_resource_id,
                UpdateMcpServerParams {
                    description: Some(Some(&item.description)),
                    enabled: Some(true),
                    transport_type: Some("http"),
                    transport_config: Some(&transport_config),
                    original_json: Some(Some(&original_json)),
                    ..Default::default()
                },
            )
            .await
            .map_err(ExtensionError::from)?;
        let updated = &mut installations[index];
        updated.installed_version = item.version;
        updated.artifact_sha256 = item.artifact_sha256;
        updated.updated_at = now;
        updated.last_checked_at = Some(now);
        updated.backup_path = None;
        updated.backup_version = Some(previous.installed_version);
        updated.backup_artifact_sha256 = previous.artifact_sha256;
        updated.last_error_code = None;
        updated.last_error_message = None;
        updated.state = "installed".to_string();
    }
    let response = installations[index].clone();
    save_capability_installations(&state, &current_user.id, &installations).await?;
    info!(
        capability_kind = %response.kind,
        capability_id = %response.capability_id,
        version = %response.installed_version,
        "capability installation updated"
    );
    Ok(Json(ApiResponse::ok(response)))
}

/// Restore the one retained Skill directory or MCP configuration snapshot.
async fn rollback_capability_installation(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    AxumPath(installation_id): AxumPath<String>,
) -> Result<Json<ApiResponse<CapabilityInstallationResponse>>, ApiError> {
    let mut installations = load_capability_installations(&state, &current_user.id).await?;
    let index = installations
        .iter()
        .position(|installation| installation.installation_id == installation_id)
        .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_INSTALLATION_NOT_FOUND".to_string()))?;
    let previous = installations[index].clone();
    let backup_version = previous
        .backup_version
        .clone()
        .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_ROLLBACK_BACKUP_MISSING".to_string()))?;
    let backup_hash = previous.backup_artifact_sha256.clone();
    let now = aionui_common::now_ms();
    if previous.kind == "skill" {
        let row = state
            .skill_repo
            .find_by_name_for_user(&current_user.id, &previous.local_resource_id)
            .await
            .map_err(ExtensionError::from)?
            .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_LOCAL_RESOURCE_MISSING".to_string()))?;
        let expected = previous
            .artifact_sha256
            .as_deref()
            .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_LOCAL_INTEGRITY_UNKNOWN".to_string()))?;
        if !capability_directory_sha256(Path::new(&row.path))
            .await?
            .eq_ignore_ascii_case(expected)
        {
            return Err(ExtensionError::InvalidRequest("CAPABILITY_LOCALLY_MODIFIED".to_string()).into());
        }
        let backup = skill_service::rollback_capability_platform_skill_for_user(
            &state.skill_paths,
            &current_user.id,
            &previous.local_resource_id,
        )
        .await?;
        let updated = &mut installations[index];
        updated.installed_version = backup_version;
        updated.artifact_sha256 = backup_hash;
        updated.updated_at = now;
        updated.last_checked_at = Some(now);
        updated.backup_path = Some(backup.to_string_lossy().to_string());
        updated.backup_version = Some(previous.installed_version);
        updated.backup_artifact_sha256 = previous.artifact_sha256;
        updated.last_error_code = None;
        updated.last_error_message = None;
        updated.state = "installed".to_string();
    } else {
        let mut backups = load_preference::<HashMap<String, CapabilityMcpBackup>>(
            &state,
            &current_user.id,
            CAPABILITY_FACTORY_MCP_BACKUPS_KEY,
        )
        .await?
        .unwrap_or_default();
        let backup = backups
            .get(&previous.installation_id)
            .cloned()
            .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_ROLLBACK_BACKUP_MISSING".to_string()))?;
        let row = state
            .mcp_repo
            .find_by_id(&current_user.id, &previous.local_resource_id)
            .await
            .map_err(ExtensionError::from)?
            .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_LOCAL_RESOURCE_MISSING".to_string()))?;
        state
            .mcp_repo
            .update(
                &current_user.id,
                &previous.local_resource_id,
                UpdateMcpServerParams {
                    description: Some(backup.description.as_deref()),
                    enabled: Some(backup.enabled),
                    transport_type: Some(&backup.transport_type),
                    transport_config: Some(&backup.transport_config),
                    tools: Some(backup.tools.as_deref()),
                    original_json: Some(backup.original_json.as_deref()),
                    builtin: Some(backup.builtin),
                    ..Default::default()
                },
            )
            .await
            .map_err(ExtensionError::from)?;
        backups.insert(
            previous.installation_id.clone(),
            CapabilityMcpBackup {
                version: previous.installed_version.clone(),
                artifact_sha256: previous.artifact_sha256.clone(),
                description: row.description,
                enabled: row.enabled,
                transport_type: row.transport_type,
                transport_config: row.transport_config,
                tools: row.tools,
                original_json: row.original_json,
                builtin: row.builtin,
            },
        );
        save_preference(&state, &current_user.id, CAPABILITY_FACTORY_MCP_BACKUPS_KEY, &backups).await?;
        let updated = &mut installations[index];
        updated.installed_version = backup.version;
        updated.artifact_sha256 = backup.artifact_sha256;
        updated.updated_at = now;
        updated.last_checked_at = Some(now);
        updated.backup_path = None;
        updated.backup_version = Some(previous.installed_version);
        updated.backup_artifact_sha256 = previous.artifact_sha256;
        updated.last_error_code = None;
        updated.last_error_message = None;
        updated.state = "installed".to_string();
    }
    let response = installations[index].clone();
    save_capability_installations(&state, &current_user.id, &installations).await?;
    info!(
        capability_kind = %response.kind,
        capability_id = %response.capability_id,
        version = %response.installed_version,
        "capability installation rolled back"
    );
    Ok(Json(ApiResponse::ok(response)))
}

/// `POST /api/skills/capability-platform/catalog` — fetch the published
/// capability catalog through AionCore so desktop and WebUI use the same
/// internal-network path without browser CORS requirements.
async fn list_capability_platform_catalog(
    body: Result<Json<CapabilityPlatformCatalogRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CapabilityPlatformCatalogResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let url = capability_platform_url(&req.platform_url, "/api/catalog")?;
    Ok(Json(ApiResponse::ok(fetch_capability_platform_catalog(url).await?)))
}

/// `POST /api/skills/capability-platform/health` — verify a configured internal
/// endpoint through AionCore, avoiding browser CORS requirements.
async fn get_capability_platform_health(
    body: Result<Json<CapabilityPlatformCatalogRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CapabilityPlatformHealthResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let url = capability_platform_url(&req.platform_url, "/health")?;
    let health = fetch_capability_platform_json::<CapabilityPlatformHealthResponse>(url).await?;
    if health.status != "ok" {
        return Err(ExtensionError::InvalidRequest("capability platform health check failed".to_string()).into());
    }
    Ok(Json(ApiResponse::ok(health)))
}

async fn load_preference<T: serde::de::DeserializeOwned>(
    state: &SkillRouterState,
    user_id: &str,
    key: &str,
) -> Result<Option<T>, ApiError> {
    let entries = state
        .client_preference_repo
        .get_by_keys(user_id, &[key])
        .await
        .map_err(ExtensionError::from)?;
    entries
        .first()
        .map(|entry| {
            serde_json::from_str(&entry.value)
                .map_err(|_| ExtensionError::InvalidRequest("CAPABILITY_FACTORY_STATE_INVALID".to_string()).into())
        })
        .transpose()
}

async fn save_preference<T: serde::Serialize>(
    state: &SkillRouterState,
    user_id: &str,
    key: &str,
    value: &T,
) -> Result<(), ApiError> {
    let json = serde_json::to_string(value)
        .map_err(|error| ExtensionError::Internal(format!("failed to serialize capability factory state: {error}")))?;
    state
        .client_preference_repo
        .upsert_batch(user_id, &[(key, json.as_str())])
        .await
        .map_err(ExtensionError::from)?;
    Ok(())
}

async fn load_factory_state(state: &SkillRouterState, user_id: &str) -> Result<CapabilityFactoryState, ApiError> {
    if let Some(factory) = load_preference(state, user_id, CAPABILITY_FACTORY_STATE_KEY).await? {
        return Ok(factory);
    }
    let legacy_url = load_preference::<String>(state, user_id, CAPABILITY_FACTORY_URL_KEY).await?;
    Ok(CapabilityFactoryState {
        url: legacy_url,
        ..Default::default()
    })
}

async fn capability_catalog_snapshot(
    state: &SkillRouterState,
    user_id: &str,
) -> Result<CapabilityCatalogSnapshotResponse, ApiError> {
    let factory = load_factory_state(state, user_id).await?;
    let cache = load_preference::<CapabilityCatalogCache>(state, user_id, CAPABILITY_FACTORY_CATALOG_KEY).await?;
    let installations =
        load_preference::<Vec<CapabilityInstallationResponse>>(state, user_id, CAPABILITY_FACTORY_INSTALLATIONS_KEY)
            .await?
            .unwrap_or_default();
    let stale = factory.last_error_code.is_some();
    let catalog = cache
        .as_ref()
        .map(|cache| cache.catalog.clone())
        .unwrap_or(CapabilityPlatformCatalogResponse {
            schema_version: None,
            catalog_version: None,
            generated_at: None,
            items: Vec::new(),
        });
    Ok(CapabilityCatalogSnapshotResponse {
        installations: annotate_local_installation_states(
            state,
            user_id,
            compute_installation_states(installations, cache.as_ref(), stale),
        )
        .await?,
        items: catalog.items,
        stale,
        last_successful_sync_at: factory.last_successful_sync_at,
        catalog_version: factory.catalog_version,
    })
}

fn compute_installation_states(
    installations: Vec<CapabilityInstallationResponse>,
    cache: Option<&CapabilityCatalogCache>,
    factory_unavailable: bool,
) -> Vec<CapabilityInstallationResponse> {
    installations
        .into_iter()
        .map(|mut installation| {
            installation.state = match cache.and_then(|cache| {
                cache
                    .catalog
                    .items
                    .iter()
                    .find(|item| item.kind == installation.kind && item.id == installation.capability_id)
            }) {
                None if factory_unavailable => "factory_unavailable".to_string(),
                None => "remote_withdrawn".to_string(),
                Some(item) => match (
                    semver::Version::parse(&installation.installed_version),
                    semver::Version::parse(&item.version),
                ) {
                    (Ok(local), Ok(remote)) if remote > local => "update_available".to_string(),
                    (Ok(local), Ok(remote)) if local > remote => "local_newer".to_string(),
                    _ => "installed".to_string(),
                },
            };
            installation
        })
        .collect()
}

async fn annotate_local_installation_states(
    state: &SkillRouterState,
    user_id: &str,
    installations: Vec<CapabilityInstallationResponse>,
) -> Result<Vec<CapabilityInstallationResponse>, ApiError> {
    let mut annotated = Vec::with_capacity(installations.len());
    for mut installation in installations {
        if matches!(
            installation.state.as_str(),
            "installed" | "update_available" | "local_newer"
        ) && let Some(local_state) = local_installation_state(state, user_id, &installation).await?
        {
            installation.state = local_state;
        }
        annotated.push(installation);
    }
    Ok(annotated)
}

async fn local_installation_state(
    state: &SkillRouterState,
    user_id: &str,
    installation: &CapabilityInstallationResponse,
) -> Result<Option<String>, ApiError> {
    if installation.kind == "skill" {
        let Some(row) = state
            .skill_repo
            .find_by_name_for_user(user_id, &installation.local_resource_id)
            .await
            .map_err(ExtensionError::from)?
        else {
            return Ok(Some("integrity_error".to_string()));
        };
        let Some(expected) = installation.artifact_sha256.as_deref() else {
            return Ok(Some("integrity_error".to_string()));
        };
        return match capability_directory_sha256(Path::new(&row.path)).await {
            Ok(actual) if actual.eq_ignore_ascii_case(expected) => Ok(None),
            Ok(_) => Ok(Some("locally_modified".to_string())),
            Err(_) => Ok(Some("integrity_error".to_string())),
        };
    }
    if installation.kind == "mcp"
        && state
            .mcp_repo
            .find_by_id(user_id, &installation.local_resource_id)
            .await
            .map_err(ExtensionError::from)?
            .is_none()
    {
        return Ok(Some("integrity_error".to_string()));
    }
    Ok(None)
}

async fn load_capability_installations(
    state: &SkillRouterState,
    user_id: &str,
) -> Result<Vec<CapabilityInstallationResponse>, ApiError> {
    Ok(load_preference(state, user_id, CAPABILITY_FACTORY_INSTALLATIONS_KEY)
        .await?
        .unwrap_or_default())
}

async fn save_capability_installations(
    state: &SkillRouterState,
    user_id: &str,
    installations: &[CapabilityInstallationResponse],
) -> Result<(), ApiError> {
    save_preference(state, user_id, CAPABILITY_FACTORY_INSTALLATIONS_KEY, &installations).await
}

async fn cached_capability_item(
    state: &SkillRouterState,
    user_id: &str,
    installation: &CapabilityInstallationResponse,
) -> Result<CapabilityPlatformCatalogItem, ApiError> {
    let cache = load_preference::<CapabilityCatalogCache>(state, user_id, CAPABILITY_FACTORY_CATALOG_KEY)
        .await?
        .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_CATALOG_NOT_SYNCED".to_string()))?;
    cache
        .catalog
        .items
        .into_iter()
        .find(|item| item.kind == installation.kind && item.id == installation.capability_id)
        .ok_or_else(|| ExtensionError::NotFound("CAPABILITY_NOT_FOUND".to_string()).into())
}

async fn fetch_verified_skill_package(
    factory_url: &str,
    item: &CapabilityPlatformCatalogItem,
) -> Result<Vec<CapabilityPlatformSkillFile>, ExtensionError> {
    let package_url = capability_platform_url(
        factory_url,
        &format!("/api/skills/{}/versions/{}/package", item.id, item.version),
    )?;
    let package = fetch_capability_platform_json::<CapabilityPlatformSkillPackageResponse>(package_url).await?;
    let expected_hash = item
        .artifact_sha256
        .as_deref()
        .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_ARTIFACT_HASH_MISSING".to_string()))?;
    let actual_hash = capability_package_sha256(&package.files)?;
    if !actual_hash.eq_ignore_ascii_case(expected_hash) {
        return Err(ExtensionError::InvalidRequest(
            "CAPABILITY_ARTIFACT_HASH_MISMATCH".to_string(),
        ));
    }
    Ok(package.files)
}

async fn capability_directory_sha256(root: &Path) -> Result<String, ExtensionError> {
    let metadata = tokio::fs::symlink_metadata(root).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ExtensionError::InvalidRequest(
            "CAPABILITY_LOCAL_INTEGRITY_INVALID".to_string(),
        ));
    }
    let mut files = Vec::new();
    collect_capability_directory_files(root, root, &mut files).await?;
    if files.is_empty() || files.len() > 500 {
        return Err(ExtensionError::InvalidRequest(
            "CAPABILITY_LOCAL_INTEGRITY_INVALID".to_string(),
        ));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    for (path, content) in files {
        let length = u32::try_from(path.len())
            .map_err(|_| ExtensionError::InvalidRequest("CAPABILITY_LOCAL_INTEGRITY_INVALID".to_string()))?;
        digest.update(length.to_be_bytes());
        digest.update(path.as_bytes());
        digest.update((content.len() as u64).to_be_bytes());
        digest.update(content);
    }
    Ok(hex::encode(digest.finalize()))
}

async fn collect_capability_directory_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), ExtensionError> {
    let mut entries = tokio::fs::read_dir(directory).await?;
    let mut total_bytes = files.iter().map(|(_, content)| content.len() as u64).sum::<u64>();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let metadata = tokio::fs::symlink_metadata(&path).await?;
        if metadata.file_type().is_symlink() {
            return Err(ExtensionError::InvalidRequest(
                "CAPABILITY_LOCAL_INTEGRITY_INVALID".to_string(),
            ));
        }
        if metadata.is_dir() {
            Box::pin(collect_capability_directory_files(root, &path, files)).await?;
            total_bytes = files.iter().map(|(_, content)| content.len() as u64).sum();
            continue;
        }
        if !metadata.is_file() || metadata.len() > 10 * 1024 * 1024 {
            return Err(ExtensionError::InvalidRequest(
                "CAPABILITY_LOCAL_INTEGRITY_INVALID".to_string(),
            ));
        }
        total_bytes = total_bytes.saturating_add(metadata.len());
        if files.len() >= 500 || total_bytes > 50 * 1024 * 1024 {
            return Err(ExtensionError::InvalidRequest(
                "CAPABILITY_LOCAL_INTEGRITY_INVALID".to_string(),
            ));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| ExtensionError::InvalidRequest("CAPABILITY_LOCAL_INTEGRITY_INVALID".to_string()))?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        files.push((relative, tokio::fs::read(path).await?));
    }
    Ok(())
}

fn capability_package_skill_name(files: &[CapabilityPlatformSkillFile]) -> Result<String, ExtensionError> {
    let skill_file = files
        .iter()
        .find(|file| file.path == "SKILL.md")
        .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_PACKAGE_SKILL_MISSING".to_string()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&skill_file.content_base64)
        .map_err(|_| ExtensionError::InvalidRequest("CAPABILITY_PACKAGE_SKILL_INVALID".to_string()))?;
    let content = std::str::from_utf8(&bytes)
        .map_err(|_| ExtensionError::InvalidRequest("CAPABILITY_PACKAGE_SKILL_INVALID".to_string()))?;
    skill_service::parse_frontmatter_fields(content)
        .map(|(name, _)| name)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| ExtensionError::InvalidRequest("CAPABILITY_PACKAGE_SKILL_INVALID".to_string()))
}

fn capability_package_sha256(files: &[CapabilityPlatformSkillFile]) -> Result<String, ExtensionError> {
    let mut files = files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let mut digest = Sha256::new();
    for file in files {
        let path = file.path.as_bytes();
        let content = base64::engine::general_purpose::STANDARD
            .decode(&file.content_base64)
            .map_err(|_| ExtensionError::InvalidRequest("CAPABILITY_PACKAGE_INVALID".to_string()))?;
        let path_length = u32::try_from(path.len())
            .map_err(|_| ExtensionError::InvalidRequest("CAPABILITY_PACKAGE_INVALID".to_string()))?;
        digest.update(path_length.to_be_bytes());
        digest.update(path);
        digest.update((content.len() as u64).to_be_bytes());
        digest.update(content);
    }
    Ok(hex::encode(digest.finalize()))
}

// ---------------------------------------------------------------------------
// Import / export / delete
// ---------------------------------------------------------------------------

/// `POST /api/skills/import` — import skill directories or zip packages by copying.
async fn import_skill(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ImportSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportSkillResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let outcome = match skill_service::import_skills_with_repo_for_user(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &current_user.id,
        Path::new(&req.skill_path),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            warn!(
                source_path = %req.skill_path,
                error = %err,
                "skill import failed"
            );
            return Err(err.into());
        }
    };
    if !outcome.failed.is_empty() {
        warn!(
            source_path = %req.skill_path,
            imported_count = outcome.imported.len(),
            failed_count = outcome.failed.len(),
            failures = ?outcome.failed,
            "skill batch import completed with failures"
        );
    }
    let names = outcome.imported;
    let first_name = names.first().cloned().unwrap_or_default();
    let failed = outcome
        .failed
        .into_iter()
        .map(|failure| ImportSkillFailureResponse {
            source_name: failure.source_name,
            code: failure.code,
            error_path: failure.error_path,
            actual_bytes: failure.actual_bytes,
            limit_bytes: failure.limit_bytes,
            line: failure.line,
            column: failure.column,
        })
        .collect();
    Ok(Json(ApiResponse::ok(ImportSkillResponse {
        skill_name: first_name,
        skill_names: names,
        failed,
    })))
}

/// `POST /api/skills/capability-platform/import` — fetch one published Skill
/// package, validate it in the local importer, and persist the copied result.
async fn import_capability_platform_skill(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ImportCapabilityPlatformSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportSkillResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if !is_capability_platform_id(&req.skill_id) {
        return Err(ExtensionError::InvalidRequest("invalid capability platform Skill id".to_string()).into());
    }
    let url = capability_platform_url(&req.platform_url, &format!("/api/skills/{}/package", req.skill_id))?;
    let package = fetch_capability_platform_json::<CapabilityPlatformSkillPackageResponse>(url).await?;
    let outcome = skill_service::import_capability_platform_skill_with_repo_for_user(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &current_user.id,
        &package.files,
    )
    .await?;
    let skill_name = outcome.imported.first().cloned().unwrap_or_default();
    let failed = outcome
        .failed
        .into_iter()
        .map(|failure| ImportSkillFailureResponse {
            source_name: failure.source_name,
            code: failure.code,
            error_path: failure.error_path,
            actual_bytes: failure.actual_bytes,
            limit_bytes: failure.limit_bytes,
            line: failure.line,
            column: failure.column,
        })
        .collect();
    Ok(Json(ApiResponse::ok(ImportSkillResponse {
        skill_name,
        skill_names: outcome.imported,
        failed,
    })))
}

async fn read_capability_platform_skill(
    body: Result<Json<ReadCapabilityPlatformSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CapabilityPlatformSkillPackageResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if !is_capability_platform_id(&req.skill_id) || req.version.trim().is_empty() {
        return Err(ExtensionError::InvalidRequest("invalid capability platform Skill reference".to_string()).into());
    }
    let url = capability_platform_url(
        &req.platform_url,
        &format!("/api/skills/{}/versions/{}/package", req.skill_id, req.version),
    )?;
    Ok(Json(ApiResponse::ok(fetch_capability_platform_json(url).await?)))
}

/// `POST /api/skills/capability-platform/skill-files` — retrieve only the
/// browseable file metadata for a published Skill. This deliberately avoids
/// the package endpoint used by installation.
async fn list_capability_platform_skill_files(
    body: Result<Json<ListCapabilityPlatformSkillFilesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<CapabilityPlatformSkillFilePreview>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if !is_capability_platform_id(&req.skill_id) || req.version.trim().is_empty() {
        return Err(ExtensionError::InvalidRequest("invalid capability platform Skill reference".to_string()).into());
    }
    let url = capability_platform_url(
        &req.platform_url,
        &format!("/api/skills/{}/versions/{}/files", req.skill_id, req.version),
    )?;
    Ok(Json(ApiResponse::ok(fetch_capability_platform_json(url).await?)))
}

/// `POST /api/skills/capability-platform/skill-file` — retrieve a single
/// safe Skill preview. The local byte limit mirrors the platform contract.
async fn read_capability_platform_skill_file(
    body: Result<Json<ReadCapabilityPlatformSkillFileRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CapabilityPlatformSkillFilePreview>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if !is_capability_platform_id(&req.skill_id) || req.version.trim().is_empty() || req.path.trim().is_empty() {
        return Err(
            ExtensionError::InvalidRequest("invalid capability platform Skill file reference".to_string()).into(),
        );
    }
    let mut url = capability_platform_url(
        &req.platform_url,
        &format!("/api/skills/{}/versions/{}/files/content", req.skill_id, req.version),
    )?;
    url.query_pairs_mut().append_pair("path", &req.path);
    let bytes =
        fetch_capability_platform_bytes_with_limit(url, None, CAPABILITY_PLATFORM_PREVIEW_RESPONSE_LIMIT).await?;
    let preview = serde_json::from_slice(&bytes)
        .map_err(|error| ExtensionError::InvalidRequest(format!("capability platform response is invalid: {error}")))?;
    Ok(Json(ApiResponse::ok(preview)))
}

fn capability_platform_url(platform_url: &str, suffix: &str) -> Result<reqwest::Url, ExtensionError> {
    let mut url = reqwest::Url::parse(platform_url.trim())
        .map_err(|_| ExtensionError::InvalidRequest("capability platform URL is invalid".to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ExtensionError::InvalidRequest(
            "capability platform URL must use HTTP or HTTPS".to_string(),
        ));
    }
    let base_path = url.path().trim_end_matches('/');
    url.set_path(&format!("{base_path}{suffix}"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

async fn fetch_capability_platform_json<T: serde::de::DeserializeOwned>(
    url: reqwest::Url,
) -> Result<T, ExtensionError> {
    let bytes = fetch_capability_platform_bytes(url, None).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ExtensionError::InvalidRequest(format!("capability platform response is invalid: {error}")))
}

async fn fetch_capability_platform_catalog(
    url: reqwest::Url,
) -> Result<CapabilityPlatformCatalogResponse, ExtensionError> {
    let bytes = fetch_capability_platform_bytes(url, Some("application/vnd.aion.capability-catalog.v2+json")).await?;
    if let Ok(catalog) = serde_json::from_slice::<CapabilityPlatformCatalogResponse>(&bytes) {
        return Ok(catalog);
    }
    let items = serde_json::from_slice::<Vec<CapabilityPlatformCatalogItem>>(&bytes)
        .map_err(|error| ExtensionError::InvalidRequest(format!("capability platform catalog is invalid: {error}")))?;
    Ok(CapabilityPlatformCatalogResponse {
        schema_version: None,
        catalog_version: None,
        generated_at: None,
        items,
    })
}

enum CapabilityPlatformCatalogFetch {
    Modified {
        catalog: CapabilityPlatformCatalogResponse,
        etag: Option<String>,
    },
    NotModified {
        etag: Option<String>,
    },
}

async fn fetch_capability_platform_catalog_with_etag(
    url: reqwest::Url,
    etag: Option<&str>,
) -> Result<CapabilityPlatformCatalogFetch, ExtensionError> {
    let client = reqwest::Client::builder()
        .timeout(CAPABILITY_PLATFORM_TIMEOUT)
        .build()
        .map_err(|error| {
            ExtensionError::Internal(format!("failed to configure capability platform client: {error}"))
        })?;
    let mut request = client.get(url).header(
        reqwest::header::ACCEPT,
        "application/vnd.aion.capability-catalog.v2+json",
    );
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| ExtensionError::InvalidRequest(format!("capability platform request failed: {error}")))?;
    let response_etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(CapabilityPlatformCatalogFetch::NotModified { etag: response_etag });
    }
    if !response.status().is_success() {
        return Err(ExtensionError::InvalidRequest(format!(
            "capability platform returned HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > CAPABILITY_PLATFORM_RESPONSE_LIMIT as u64)
    {
        return Err(ExtensionError::SkillImportTotalTooLarge {
            total_bytes: response.content_length().unwrap_or_default(),
            limit_bytes: CAPABILITY_PLATFORM_RESPONSE_LIMIT as u64,
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ExtensionError::InvalidRequest(format!("failed to read capability platform response: {error}"))
    })? {
        let next_len = bytes.len().saturating_add(chunk.len());
        if next_len > CAPABILITY_PLATFORM_RESPONSE_LIMIT {
            return Err(ExtensionError::SkillImportTotalTooLarge {
                total_bytes: next_len as u64,
                limit_bytes: CAPABILITY_PLATFORM_RESPONSE_LIMIT as u64,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    let catalog = if let Ok(catalog) = serde_json::from_slice::<CapabilityPlatformCatalogResponse>(&bytes) {
        catalog
    } else {
        CapabilityPlatformCatalogResponse {
            schema_version: None,
            catalog_version: None,
            generated_at: None,
            items: serde_json::from_slice(&bytes).map_err(|error| {
                ExtensionError::InvalidRequest(format!("capability platform catalog is invalid: {error}"))
            })?,
        }
    };
    Ok(CapabilityPlatformCatalogFetch::Modified {
        catalog,
        etag: response_etag,
    })
}

async fn fetch_capability_platform_bytes(url: reqwest::Url, accept: Option<&str>) -> Result<Vec<u8>, ExtensionError> {
    fetch_capability_platform_bytes_with_limit(url, accept, CAPABILITY_PLATFORM_RESPONSE_LIMIT).await
}

async fn fetch_capability_platform_bytes_with_limit(
    url: reqwest::Url,
    accept: Option<&str>,
    response_limit: usize,
) -> Result<Vec<u8>, ExtensionError> {
    let client = reqwest::Client::builder()
        .timeout(CAPABILITY_PLATFORM_TIMEOUT)
        .build()
        .map_err(|error| {
            ExtensionError::Internal(format!("failed to configure capability platform client: {error}"))
        })?;
    let mut request = client.get(url);
    if let Some(accept) = accept {
        request = request.header(reqwest::header::ACCEPT, accept);
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| ExtensionError::InvalidRequest(format!("capability platform request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(ExtensionError::InvalidRequest(format!(
            "capability platform returned HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > response_limit as u64)
    {
        return Err(ExtensionError::SkillImportTotalTooLarge {
            total_bytes: response.content_length().unwrap_or_default(),
            limit_bytes: response_limit as u64,
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ExtensionError::InvalidRequest(format!("failed to read capability platform response: {error}"))
    })? {
        let next_len = bytes.len().saturating_add(chunk.len());
        if next_len > response_limit {
            return Err(ExtensionError::SkillImportTotalTooLarge {
                total_bytes: next_len as u64,
                limit_bytes: response_limit as u64,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn is_capability_platform_id(value: &str) -> bool {
    (2..=63).contains(&value.len())
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-'))
}

/// `POST /api/skills/export-symlink` — export a skill symlink.
async fn export_skill_symlink(
    body: Result<Json<ExportSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    skill_service::export_skill_with_symlink(Path::new(&req.skill_path), Path::new(&req.target_dir)).await?;
    Ok(Json(ApiResponse::success()))
}

/// `DELETE /api/skills/:name` — delete a user-custom skill.
async fn delete_skill(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    skill_service::delete_skill_with_repo_for_user(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &current_user.id,
        &name,
    )
    .await?;
    Ok(Json(ApiResponse::success()))
}

/// `GET /api/skills/import-history` — list recent skill import records.
async fn list_import_history(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<SkillImportRecordResponse>>>, ApiError> {
    let records = state
        .skill_repo
        .list_import_records_for_user(&current_user.id, 100)
        .await
        .map_err(ExtensionError::from)?;
    let resp = records
        .into_iter()
        .map(|row| SkillImportRecordResponse {
            id: row.id,
            operation_id: row.operation_id,
            source_label: row.source_label,
            source_path: row.source_path,
            source_name: row.source_name,
            skill_id: row.skill_id,
            skill_name: row.skill_name,
            status: row.status,
            error_code: row.error_code,
            error_path: row.error_path,
            actual_bytes: row.actual_bytes,
            limit_bytes: row.limit_bytes,
            line: row.line,
            column: row.column,
            created_at: row.created_at,
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

// ---------------------------------------------------------------------------
// Scanning & discovery
// ---------------------------------------------------------------------------

/// `POST /api/skills/scan` — scan a directory for skills.
async fn scan_for_skills(
    body: Result<Json<ScanForSkillsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ScanForSkillsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let skills = skill_service::scan_for_skills(Path::new(&req.folder_path)).await?;
    let resp = ScanForSkillsResponse {
        skills: skills
            .into_iter()
            .map(|s| ScannedSkillResponse {
                name: s.name,
                description: s.description,
                path: s.path,
            })
            .collect(),
    };
    Ok(Json(ApiResponse::ok(resp)))
}

/// `GET /api/skills/detect-paths` — detect common skill paths.
async fn detect_paths() -> Result<Json<ApiResponse<Vec<NamedPathResponse>>>, ApiError> {
    let paths = skill_service::detect_common_skill_paths().await;
    let resp: Vec<NamedPathResponse> = paths
        .into_iter()
        .map(|p| NamedPathResponse {
            name: p.name,
            path: p.path,
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `GET /api/skills/detect-external` — discover external skills from all sources.
async fn detect_external(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<ExternalSkillSourceResponse>>>, ApiError> {
    let custom = state.external_paths_manager.get_custom_external_paths().await;
    let sources = skill_service::detect_and_count_external_skills(&custom).await;
    let resp: Vec<ExternalSkillSourceResponse> = sources
        .into_iter()
        .map(|s| ExternalSkillSourceResponse {
            name: s.name,
            path: s.path,
            source: s.source,
            skill_count: s.skill_count,
            skills: s
                .skills
                .into_iter()
                .map(|sk| ScannedSkillResponse {
                    name: sk.name,
                    description: sk.description,
                    path: sk.path,
                })
                .collect(),
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

// ---------------------------------------------------------------------------
// Built-in resources
// ---------------------------------------------------------------------------

/// `POST /api/skills/builtin-rule` — read a built-in rule file.
async fn read_builtin_rule(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadBuiltinResourceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let content = skill_service::read_builtin_rule(&state.skill_paths, &req.file_name).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/builtin-skill` — read a built-in skill file.
async fn read_builtin_skill(
    State(state): State<SkillRouterState>,
    body: Result<Json<ReadBuiltinResourceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let content = skill_service::read_builtin_skill(&state.skill_paths, &req.file_name).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/materialize-for-agent` — resolve each requested skill
/// name to its on-disk source directory. The frontend symlinks each
/// returned `source_path` into the agent CLI's native skills dir. The
/// backend no longer copies any files per-conversation.
async fn materialize_for_agent(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<MaterializeSkillsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<MaterializeSkillsResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if req.conversation_id.trim().is_empty() {
        return Err(ApiError::BadRequest("conversationId must not be empty".into()));
    }
    let resolved = skill_service::materialize_skills_for_agent_with_repo_for_user(
        &state.skill_paths,
        state.skill_repo.as_ref(),
        &current_user.id,
        &req.conversation_id,
        &req.skills,
    )
    .await?;
    let skills: Vec<MaterializedSkillRef> = resolved
        .into_iter()
        .map(|s| MaterializedSkillRef {
            name: s.name,
            source_path: s.source_path.to_string_lossy().into_owned(),
        })
        .collect();
    Ok(Json(ApiResponse::ok(MaterializeSkillsResponse { skills })))
}

// ---------------------------------------------------------------------------
// Assistant rules CRUD
// ---------------------------------------------------------------------------

/// `POST /api/skills/assistant-rule/read` — read an assistant rule.
///
/// Dispatches by source via [`AssistantRuleDispatcher`] when wired; falls
/// back to user-directory-only legacy behavior otherwise.
async fn read_assistant_rule(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ReadAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let content = dispatcher
            .read_rule(&current_user.id, &req.assistant_id, req.locale.as_deref())
            .await?;
        return Ok(Json(ApiResponse::ok(content)));
    }
    tracing::warn!(
        assistant_id = %req.assistant_id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-rule read fallback (must not happen in production)"
    );
    let content =
        skill_service::read_assistant_rule(&state.skill_paths, &req.assistant_id, req.locale.as_deref()).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/assistant-rule/write` — write an assistant rule.
///
/// Dispatches by source: builtin / extension ids reject with 400.
async fn write_assistant_rule(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<WriteAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        dispatcher
            .write_rule(&current_user.id, &req.assistant_id, req.locale.as_deref(), &req.content)
            .await?;
        return Ok(Json(ApiResponse::ok(true)));
    }
    tracing::warn!(
        assistant_id = %req.assistant_id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-rule write fallback (must not happen in production)"
    );
    let ok = skill_service::write_assistant_rule(
        &state.skill_paths,
        &req.assistant_id,
        &req.content,
        req.locale.as_deref(),
    )
    .await?;
    Ok(Json(ApiResponse::ok(ok)))
}

/// `DELETE /api/skills/assistant-rule/:id` — delete all locale versions.
async fn delete_assistant_rule(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let ok = dispatcher.delete_rule(&current_user.id, &id).await?;
        return Ok(Json(ApiResponse::ok(ok)));
    }
    tracing::warn!(
        assistant_id = %id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-rule delete fallback (must not happen in production)"
    );
    let ok = skill_service::delete_assistant_rule(&state.skill_paths, &id).await?;
    Ok(Json(ApiResponse::ok(ok)))
}

// ---------------------------------------------------------------------------
// Assistant skills CRUD
// ---------------------------------------------------------------------------

/// `POST /api/skills/assistant-skill/read` — read an assistant skill.
///
/// Dispatches by source via [`AssistantRuleDispatcher`] when wired.
async fn read_assistant_skill(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ReadAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let content = dispatcher
            .read_skill(&current_user.id, &req.assistant_id, req.locale.as_deref())
            .await?;
        return Ok(Json(ApiResponse::ok(content)));
    }
    tracing::warn!(
        assistant_id = %req.assistant_id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-skill read fallback (must not happen in production)"
    );
    let content =
        skill_service::read_assistant_skill(&state.skill_paths, &req.assistant_id, req.locale.as_deref()).await?;
    Ok(Json(ApiResponse::ok(content)))
}

/// `POST /api/skills/assistant-skill/write` — write an assistant skill.
///
/// Dispatches by source: builtin / extension ids reject with 400.
async fn write_assistant_skill(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<WriteAssistantRuleRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(dispatcher) = &state.assistant_dispatcher {
        dispatcher
            .write_skill(&current_user.id, &req.assistant_id, req.locale.as_deref(), &req.content)
            .await?;
        return Ok(Json(ApiResponse::ok(true)));
    }
    tracing::warn!(
        assistant_id = %req.assistant_id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-skill write fallback (must not happen in production)"
    );
    let ok = skill_service::write_assistant_skill(
        &state.skill_paths,
        &req.assistant_id,
        &req.content,
        req.locale.as_deref(),
    )
    .await?;
    Ok(Json(ApiResponse::ok(ok)))
}

/// `DELETE /api/skills/assistant-skill/:id` — delete all locale versions.
async fn delete_assistant_skill(
    State(state): State<SkillRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ApiResponse<bool>>, ApiError> {
    if let Some(dispatcher) = &state.assistant_dispatcher {
        let ok = dispatcher.delete_skill(&current_user.id, &id).await?;
        return Ok(Json(ApiResponse::ok(ok)));
    }
    tracing::warn!(
        assistant_id = %id,
        "assistant_dispatcher not configured; using unscoped legacy assistant-skill delete fallback (must not happen in production)"
    );
    let ok = skill_service::delete_assistant_skill(&state.skill_paths, &id).await?;
    Ok(Json(ApiResponse::ok(ok)))
}

// ---------------------------------------------------------------------------
// External path management
// ---------------------------------------------------------------------------

/// `GET /api/skills/external-paths` — list custom external paths.
async fn get_external_paths(
    State(state): State<SkillRouterState>,
) -> Result<Json<ApiResponse<Vec<NamedPathResponse>>>, ApiError> {
    let paths = state.external_paths_manager.get_custom_external_paths().await;
    let resp: Vec<NamedPathResponse> = paths
        .into_iter()
        .map(|p| NamedPathResponse {
            name: p.name,
            path: p.path,
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `POST /api/skills/external-paths` — add a custom external path.
async fn add_external_path(
    State(state): State<SkillRouterState>,
    body: Result<Json<AddExternalPathRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .external_paths_manager
        .add_custom_external_path(&req.name, &req.path)
        .await?;
    Ok(Json(ApiResponse::success()))
}

/// `DELETE /api/skills/external-paths` — remove a custom external path.
async fn remove_external_path(
    State(state): State<SkillRouterState>,
    body: Result<Json<RemoveExternalPathRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .external_paths_manager
        .remove_custom_external_path(&req.path)
        .await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Skills market
// ---------------------------------------------------------------------------

/// `POST /api/skills/market/enable` — enable the aionui skills market.
async fn enable_skills_market(State(state): State<SkillRouterState>) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.external_paths_manager.enable_skills_market().await?;
    Ok(Json(ApiResponse::success()))
}

/// `POST /api/skills/market/disable` — disable the aionui skills market.
async fn disable_skills_market(State(state): State<SkillRouterState>) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.external_paths_manager.disable_skills_market().await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use tower::ServiceExt;

    async fn make_state() -> SkillRouterState {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join("skills"),
            cron_skills_dir: tmp.path().join("cron").join("skills"),
            builtin_skills_dir: tmp.path().join("builtin-skills"),
            builtin_rules_dir: tmp.path().join("builtin-rules"),
            assistant_rules_dir: tmp.path().join("assistant-rules"),
            assistant_skills_dir: tmp.path().join("assistant-skills"),
        };
        let ext_mgr = Arc::new(ExternalPathsManager::with_file(tmp.path().join("paths.json")).await);
        let db = aionui_db::init_database_memory().await.unwrap();
        let skill_repo = Arc::new(aionui_db::SqliteSkillRepository::new(db.pool().clone()));
        std::mem::forget(tmp);
        SkillRouterState {
            skill_paths: paths,
            skill_repo,
            mcp_repo: Arc::new(aionui_db::SqliteMcpServerRepository::new(db.pool().clone())),
            client_preference_repo: Arc::new(aionui_db::SqliteClientPreferenceRepository::new(db.pool().clone())),
            external_paths_manager: ext_mgr,
            assistant_dispatcher: None,
        }
    }

    #[tokio::test]
    async fn skill_routes_builds_router() {
        let state = make_state().await;
        let _router = skill_routes(state);
    }

    #[test]
    fn capability_platform_url_keeps_a_configured_path_and_rejects_non_http_schemes() {
        let url = capability_platform_url("http://factory.intra:8787/capabilities/", "/api/catalog").unwrap();
        assert_eq!(url.as_str(), "http://factory.intra:8787/capabilities/api/catalog");
        assert!(capability_platform_url("file:///factory", "/api/catalog").is_err());
    }

    #[tokio::test]
    async fn capability_platform_catalog_fetches_from_an_internal_http_endpoint() {
        let app = Router::new().route(
            "/api/catalog",
            get(|| async {
                Json(vec![CapabilityPlatformCatalogItem {
                    id: "internal-review".to_string(),
                    kind: "skill".to_string(),
                    name: "Internal review".to_string(),
                    description: "Review internal changes".to_string(),
                    version: "1.0.0".to_string(),
                    status: Some("published".to_string()),
                    revision: Some(1),
                    artifact_sha256: Some("a".repeat(64)),
                    published_at: Some("2026-05-03T10:00:00.000Z".to_string()),
                    updated_at: Some("2026-05-03T10:00:00.000Z".to_string()),
                    release_notes: None,
                    compatibility: None,
                    file_count: Some(2),
                    total_bytes: Some(128),
                    example_count: Some(1),
                    config: None,
                }])
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let url = capability_platform_url(&format!("http://{address}"), "/api/catalog").unwrap();
        let items = fetch_capability_platform_json::<Vec<CapabilityPlatformCatalogItem>>(url)
            .await
            .unwrap();

        server.abort();
        assert_eq!(items[0].id, "internal-review");
        assert_eq!(items[0].revision, Some(1));
    }

    #[tokio::test]
    async fn capability_platform_catalog_prefers_schema_v2_and_keeps_legacy_fallback() {
        let app = Router::new().route(
            "/api/catalog",
            get(|| async {
                Json(serde_json::json!({
                    "schemaVersion": 2,
                    "catalogVersion": "17",
                    "generatedAt": "2026-05-03T10:00:00.000Z",
                    "items": []
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let url = capability_platform_url(&format!("http://{address}"), "/api/catalog").unwrap();
        let catalog = fetch_capability_platform_catalog(url).await.unwrap();

        server.abort();
        assert_eq!(catalog.schema_version, Some(2));
        assert_eq!(catalog.catalog_version.as_deref(), Some("17"));
    }

    #[tokio::test]
    async fn capability_factory_sync_keeps_a_per_user_catalog_cache() {
        let app = Router::new().route(
            "/api/catalog",
            get(|| async {
                Json(serde_json::json!({
                    "schemaVersion": 2,
                    "catalogVersion": "18",
                    "generatedAt": "2026-05-04T10:00:00.000Z",
                    "items": [{
                        "id": "internal-review",
                        "kind": "skill",
                        "name": "Internal review",
                        "description": "Review internal changes",
                        "version": "1.0.0"
                    }]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = make_state().await;
        let user = CurrentUser::local_default();
        save_preference(
            &state,
            &user.id,
            CAPABILITY_FACTORY_STATE_KEY,
            &CapabilityFactoryState {
                url: Some(format!("http://{address}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let Json(ApiResponse { data, .. }) = sync_capability_factory(State(state.clone()), Extension(user.clone()))
            .await
            .unwrap();
        let data = data.unwrap();
        assert_eq!(data.items.len(), 1);
        assert_eq!(data.catalog_version.as_deref(), Some("18"));
        let Json(ApiResponse { data, .. }) = get_capability_catalog(State(state), Extension(user)).await.unwrap();
        let data = data.unwrap();
        assert_eq!(data.items[0].id, "internal-review");
        assert!(!data.stale);
        server.abort();
    }

    #[tokio::test]
    async fn scheduled_capability_sync_skips_users_without_a_factory_configuration() {
        let state = make_state().await;
        let result = synchronize_capability_factory_for_user(&state, "unconfigured-user", false)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn capability_factory_catalog_fetch_uses_etag_for_unchanged_catalogs() {
        let app = Router::new().route(
            "/api/catalog",
            get(|headers: axum::http::HeaderMap| async move {
                if headers.get(reqwest::header::IF_NONE_MATCH).is_some() {
                    return StatusCode::NOT_MODIFIED.into_response();
                }
                (
                    [(reqwest::header::ETAG, "\"catalog-20\"")],
                    Json(serde_json::json!({ "schemaVersion": 2, "catalogVersion": "20", "items": [] })),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = capability_platform_url(&format!("http://{address}"), "/api/catalog").unwrap();
        let first = fetch_capability_platform_catalog_with_etag(url.clone(), None)
            .await
            .unwrap();
        let etag = match first {
            CapabilityPlatformCatalogFetch::Modified { etag, .. } => etag.unwrap(),
            CapabilityPlatformCatalogFetch::NotModified { .. } => panic!("first read must return a catalog"),
        };
        let second = fetch_capability_platform_catalog_with_etag(url, Some(&etag))
            .await
            .unwrap();
        assert!(matches!(second, CapabilityPlatformCatalogFetch::NotModified { .. }));
        server.abort();
    }

    #[tokio::test]
    async fn capability_factory_install_records_the_frontmatter_skill_identity() {
        let files = vec![CapabilityPlatformSkillFile {
            path: "SKILL.md".to_string(),
            content_base64: base64::engine::general_purpose::STANDARD
                .encode("---\nname: factory-review\ndescription: Review code from the factory\n---\n# Review"),
        }];
        let artifact_sha256 = capability_package_sha256(&files).unwrap();
        let app = Router::new()
            .route(
                "/api/catalog",
                get(move || {
                    let artifact_sha256 = artifact_sha256.clone();
                    async move {
                        Json(serde_json::json!({
                            "schemaVersion": 2,
                            "catalogVersion": "19",
                            "items": [{
                                "id": "factory-review",
                                "kind": "skill",
                                "name": "Different display name",
                                "description": "Review code from the factory",
                                "version": "1.0.0",
                                "artifactSha256": artifact_sha256
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/api/skills/factory-review/versions/1.0.0/package",
                get(move || {
                    let files = files.clone();
                    async move { Json(serde_json::json!({ "files": files })) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = make_state().await;
        let user = CurrentUser::local_default();
        save_preference(
            &state,
            &user.id,
            CAPABILITY_FACTORY_STATE_KEY,
            &CapabilityFactoryState {
                url: Some(format!("http://{address}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let _ = sync_capability_factory(State(state.clone()), Extension(user.clone()))
            .await
            .unwrap();

        let Json(ApiResponse { data, .. }) = install_capability(
            State(state.clone()),
            Extension(user.clone()),
            Ok(Json(InstallCapabilityRequest {
                kind: "skill".to_string(),
                capability_id: "factory-review".to_string(),
            })),
        )
        .await
        .unwrap();
        let installation = data.unwrap();
        assert_eq!(installation.local_resource_id, "factory-review");
        assert_eq!(installation.installed_version, "1.0.0");
        let Json(ApiResponse { data, .. }) =
            get_capability_installations(State(state.clone()), Extension(user.clone()))
                .await
                .unwrap();
        assert_eq!(data.unwrap()[0].capability_id, "factory-review");
        let Json(ApiResponse { data, .. }) = check_capability_installation(
            State(state),
            Extension(user),
            AxumPath("skill:factory-review".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(data.unwrap().state, "installed");
        server.abort();
    }

    #[tokio::test]
    async fn capability_factory_skill_update_rejects_local_edits_and_can_rollback() {
        let v1_files = vec![CapabilityPlatformSkillFile {
            path: "SKILL.md".to_string(),
            content_base64: base64::engine::general_purpose::STANDARD
                .encode("---\nname: factory-review\ndescription: First release\n---\n# v1"),
        }];
        let v2_files = vec![CapabilityPlatformSkillFile {
            path: "SKILL.md".to_string(),
            content_base64: base64::engine::general_purpose::STANDARD
                .encode("---\nname: factory-review\ndescription: Second release\n---\n# v2"),
        }];
        let v1_hash = capability_package_sha256(&v1_files).unwrap();
        let v2_hash = capability_package_sha256(&v2_files).unwrap();
        let app = Router::new().route(
            "/api/skills/factory-review/versions/2.0.0/package",
            get(move || {
                let files = v2_files.clone();
                async move { Json(serde_json::json!({ "files": files })) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = make_state().await;
        let user = CurrentUser::local_default();
        let imported = skill_service::import_capability_platform_skill_with_repo_for_user(
            &state.skill_paths,
            state.skill_repo.as_ref(),
            &user.id,
            &v1_files,
        )
        .await
        .unwrap();
        assert_eq!(imported.imported, ["factory-review"]);
        save_preference(
            &state,
            &user.id,
            CAPABILITY_FACTORY_STATE_KEY,
            &CapabilityFactoryState {
                url: Some(format!("http://{address}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        save_preference(
            &state,
            &user.id,
            CAPABILITY_FACTORY_CATALOG_KEY,
            &CapabilityCatalogCache {
                catalog: CapabilityPlatformCatalogResponse {
                    schema_version: Some(2),
                    catalog_version: Some("2".to_string()),
                    generated_at: None,
                    items: vec![CapabilityPlatformCatalogItem {
                        id: "factory-review".to_string(),
                        kind: "skill".to_string(),
                        name: "Factory Review".to_string(),
                        description: "Second release".to_string(),
                        version: "2.0.0".to_string(),
                        status: Some("published".to_string()),
                        revision: Some(2),
                        artifact_sha256: Some(v2_hash.clone()),
                        published_at: None,
                        updated_at: None,
                        release_notes: Some("Upgrade".to_string()),
                        compatibility: None,
                        file_count: Some(1),
                        total_bytes: None,
                        example_count: None,
                        config: None,
                    }],
                },
                synced_at: aionui_common::now_ms(),
            },
        )
        .await
        .unwrap();
        save_capability_installations(
            &state,
            &user.id,
            &[CapabilityInstallationResponse {
                installation_id: "skill:factory-review".to_string(),
                kind: "skill".to_string(),
                capability_id: "factory-review".to_string(),
                local_resource_id: "factory-review".to_string(),
                installed_version: "1.0.0".to_string(),
                artifact_sha256: Some(v1_hash),
                factory_url_snapshot: format!("http://{address}"),
                installed_at: 1,
                updated_at: 1,
                last_checked_at: None,
                update_policy: "notify".to_string(),
                ignored_version: None,
                backup_path: None,
                backup_version: None,
                backup_artifact_sha256: None,
                last_error_code: None,
                last_error_message: None,
                state: "installed".to_string(),
            }],
        )
        .await
        .unwrap();

        let Json(ApiResponse { data, .. }) = update_capability_installation(
            State(state.clone()),
            Extension(user.clone()),
            AxumPath("skill:factory-review".to_string()),
            Ok(Json(UpdateCapabilityRequest {
                replace_locally_modified: false,
            })),
        )
        .await
        .unwrap();
        let updated = data.unwrap();
        assert_eq!(updated.installed_version, "2.0.0");
        assert_eq!(
            std::fs::read_to_string(
                state
                    .skill_paths
                    .user_skills_dir
                    .join("users/system_default_user/factory-review/SKILL.md")
            )
            .unwrap(),
            "---\nname: factory-review\ndescription: Second release\n---\n# v2"
        );

        let Json(ApiResponse { data, .. }) = rollback_capability_installation(
            State(state.clone()),
            Extension(user.clone()),
            AxumPath("skill:factory-review".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(data.unwrap().installed_version, "1.0.0");
        let skill_file = state
            .skill_paths
            .user_skills_dir
            .join("users/system_default_user/factory-review/SKILL.md");
        assert!(std::fs::read_to_string(&skill_file).unwrap().contains("# v1"));
        std::fs::write(&skill_file, "locally modified").unwrap();
        let error = update_capability_installation(
            State(state),
            Extension(user),
            AxumPath("skill:factory-review".to_string()),
            Ok(Json(UpdateCapabilityRequest {
                replace_locally_modified: false,
            })),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("CAPABILITY_LOCALLY_MODIFIED"));
        server.abort();
    }

    #[tokio::test]
    async fn capability_platform_preview_response_respects_its_smaller_limit() {
        let app = Router::new().route("/preview", get(|| async { "x".repeat(32) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let url = capability_platform_url(&format!("http://{address}"), "/preview").unwrap();
        let result = fetch_capability_platform_bytes_with_limit(url, None, 16).await;

        server.abort();
        assert!(matches!(
            result,
            Err(ExtensionError::SkillImportTotalTooLarge { limit_bytes: 16, .. })
        ));
    }

    #[tokio::test]
    async fn builtin_auto_skill_list_get_is_not_registered() {
        let state = make_state().await;
        let response = skill_routes(state)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/skills/builtin-auto")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
