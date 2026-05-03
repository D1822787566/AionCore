// `ApiError` is the intended error type at this HTTP boundary (routes map the
// crate-owned `BaitongError` to it here), so the disallowed_types lint that
// steers service code away from `ApiError` does not apply to this module.
#![allow(clippy::disallowed_types)]

//! `/api/baitong/*` HTTP surface.
//!
//! All routes require authentication (applied by the caller in `aionui-app`).

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};

use aionui_api_types::{
    ApiResponse, BaitongAdapterAction, BaitongAdapterActionResponse, BaitongConfigResponse, BaitongConfigUpdate,
    BaitongLoginRequest, BaitongLogsResponse, BaitongMcpSnippetResponse, BaitongRefreshResponse, BaitongStatusResponse,
};
use aionui_common::ApiError;

use crate::error::BaitongError;
use crate::service::BaitongService;

#[derive(Clone)]
pub struct BaitongRouterState {
    pub service: Arc<BaitongService>,
}

pub fn baitong_routes(state: BaitongRouterState) -> Router {
    Router::new()
        .route("/api/baitong/status", get(get_status))
        .route("/api/baitong/auth/login", post(login))
        .route("/api/baitong/session/refresh", post(refresh))
        .route("/api/baitong/session/logout", post(logout))
        .route("/api/baitong/config", put(update_config))
        .route("/api/baitong/adapter/{action}", post(adapter_action))
        .route("/api/baitong/logs", get(logs))
        .route("/api/baitong/mcp-snippet", get(mcp_snippet))
        .route("/api/baitong/write-opencode-config", post(write_opencode_config))
        .route("/api/baitong/open-opencode-folder", post(open_opencode_folder))
        .with_state(state)
}

impl From<BaitongError> for ApiError {
    fn from(error: BaitongError) -> Self {
        let code = error.code();
        let message = error.to_string();
        match &error {
            BaitongError::BadRequest(_) => ApiError::coded(StatusCode::BAD_REQUEST, code, message, None),
            BaitongError::InvalidCredentials => ApiError::coded(StatusCode::UNAUTHORIZED, code, message, None),
            BaitongError::CredentialsRequired(_) => ApiError::coded(StatusCode::CONFLICT, code, message, None),
            BaitongError::ConfigConflict { expected, current } => ApiError::coded(
                StatusCode::CONFLICT,
                code,
                message,
                Some(serde_json::json!({ "expected": expected, "current": current })),
            ),
            BaitongError::Unreachable(_) => ApiError::coded(StatusCode::BAD_GATEWAY, code, message, None),
            BaitongError::Adapter(_) => ApiError::coded(StatusCode::BAD_GATEWAY, code, message, None),
            BaitongError::Internal(_) => ApiError::coded(StatusCode::INTERNAL_SERVER_ERROR, code, message, None),
        }
    }
}

async fn get_status(
    State(state): State<BaitongRouterState>,
) -> Result<Json<ApiResponse<BaitongStatusResponse>>, ApiError> {
    let status = state.service.status().await?;
    Ok(Json(ApiResponse::ok(status)))
}

async fn login(
    State(state): State<BaitongRouterState>,
    body: Result<Json<BaitongLoginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<BaitongStatusResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let status = state.service.login(&request.account, &request.password).await?;
    Ok(Json(ApiResponse::ok(status)))
}

async fn refresh(
    State(state): State<BaitongRouterState>,
) -> Result<Json<ApiResponse<BaitongRefreshResponse>>, ApiError> {
    let response = state.service.refresh().await?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn logout(State(state): State<BaitongRouterState>) -> Result<Json<ApiResponse<BaitongStatusResponse>>, ApiError> {
    let status = state.service.logout().await?;
    Ok(Json(ApiResponse::ok(status)))
}

async fn update_config(
    State(state): State<BaitongRouterState>,
    body: Result<Json<BaitongConfigUpdate>, JsonRejection>,
) -> Result<Json<ApiResponse<BaitongConfigResponse>>, ApiError> {
    let Json(update) = body.map_err(ApiError::from)?;
    let response = state.service.update_config(update).await?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn adapter_action(
    State(state): State<BaitongRouterState>,
    axum::extract::Path(action): axum::extract::Path<String>,
) -> Result<Json<ApiResponse<BaitongAdapterActionResponse>>, ApiError> {
    let action = match action.as_str() {
        "start" => BaitongAdapterAction::Start,
        "stop" => BaitongAdapterAction::Stop,
        "restart" => BaitongAdapterAction::Restart,
        "test" => BaitongAdapterAction::Test,
        other => {
            return Err(ApiError::coded(
                StatusCode::BAD_REQUEST,
                "BAITONG_BAD_REQUEST",
                format!("unknown adapter action: {other}"),
                None,
            ));
        }
    };
    let response = state.service.adapter_action(action).await?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn logs(State(state): State<BaitongRouterState>) -> Result<Json<ApiResponse<BaitongLogsResponse>>, ApiError> {
    let logs = state.service.logs().await?;
    Ok(Json(ApiResponse::ok(logs)))
}

async fn mcp_snippet(
    State(state): State<BaitongRouterState>,
) -> Result<Json<ApiResponse<BaitongMcpSnippetResponse>>, ApiError> {
    let snippet = state.service.mcp_snippet().await?;
    Ok(Json(ApiResponse::ok(BaitongMcpSnippetResponse { snippet })))
}

async fn write_opencode_config(
    State(state): State<BaitongRouterState>,
) -> Result<Json<ApiResponse<BaitongMcpSnippetResponse>>, ApiError> {
    let message = state.service.write_opencode_config().await?;
    Ok(Json(ApiResponse::ok(BaitongMcpSnippetResponse { snippet: message })))
}

async fn open_opencode_folder(
    State(state): State<BaitongRouterState>,
) -> Result<Json<ApiResponse<BaitongMcpSnippetResponse>>, ApiError> {
    let message = state.service.open_opencode_folder().await?;
    Ok(Json(ApiResponse::ok(BaitongMcpSnippetResponse { snippet: message })))
}
