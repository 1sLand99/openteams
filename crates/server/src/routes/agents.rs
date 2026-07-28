use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{Path, Query},
    response::Json as ResponseJson,
    routing::{get, patch, post},
};
use executors::executors::BaseCodingAgent;
use serde::Deserialize;
use services::services::agent_runtime::{
    AgentRuntimeDiagnostics, AgentRuntimeError, AgentRuntimeListResponse,
    AgentRuntimeRefreshResponse, AgentRuntimeStatus, UpdateAgentRuntimeConfig,
    list_runtime_statuses, refresh_runtime_discovery, runtime_diagnostics, update_runtime_config,
};
use utils::response::ApiResponse;

use crate::{DeploymentImpl, error::ApiError};

pub fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route("/agents/runtime", get(get_runtime))
        .route("/agents/runtime/refresh", post(refresh_runtime))
        .route("/agents/runtime/{runner}", patch(patch_runtime_config))
        .route(
            "/agents/runtime/{runner}/diagnostics",
            get(get_runtime_diagnostics),
        )
}

#[derive(Debug, Default, Deserialize)]
struct RuntimeDiagnosticsQuery {
    workspace_path: Option<PathBuf>,
    auth_method_id: Option<String>,
}

async fn get_runtime() -> Result<ResponseJson<ApiResponse<AgentRuntimeListResponse>>, ApiError> {
    let response = list_runtime_statuses().map_err(api_error_from_runtime)?;
    Ok(ResponseJson(ApiResponse::success(response)))
}

async fn refresh_runtime()
-> Result<ResponseJson<ApiResponse<AgentRuntimeRefreshResponse>>, ApiError> {
    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let response = refresh_runtime_discovery(&current_dir)
        .await
        .map_err(api_error_from_runtime)?;
    Ok(ResponseJson(ApiResponse::success(response)))
}

async fn patch_runtime_config(
    Path(runner): Path<BaseCodingAgent>,
    Json(payload): Json<UpdateAgentRuntimeConfig>,
) -> Result<ResponseJson<ApiResponse<AgentRuntimeStatus>>, ApiError> {
    let response = update_runtime_config(runner, payload).map_err(api_error_from_runtime)?;
    Ok(ResponseJson(ApiResponse::success(response)))
}

async fn get_runtime_diagnostics(
    Path(runner): Path<BaseCodingAgent>,
    Query(query): Query<RuntimeDiagnosticsQuery>,
) -> Result<ResponseJson<ApiResponse<AgentRuntimeDiagnostics>>, ApiError> {
    let probe_dir = query
        .workspace_path
        .unwrap_or(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let response = runtime_diagnostics(runner, &probe_dir, query.auth_method_id.as_deref())
        .await
        .map_err(api_error_from_runtime)?;
    Ok(ResponseJson(ApiResponse::success(response)))
}

fn api_error_from_runtime(err: AgentRuntimeError) -> ApiError {
    match err {
        AgentRuntimeError::InvalidEnvKey(_) | AgentRuntimeError::UnknownRunner(_) => {
            ApiError::BadRequest(err.to_string())
        }
        AgentRuntimeError::Io(err) => ApiError::Io(err),
        AgentRuntimeError::Json(err) => ApiError::BadRequest(err.to_string()),
        AgentRuntimeError::Profile(executors::profile::ProfileError::Io(err)) => ApiError::Io(err),
        AgentRuntimeError::Profile(err) => ApiError::BadRequest(err.to_string()),
    }
}
