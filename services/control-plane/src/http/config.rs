//! Deployment/config read endpoint for the web console. Exposes non-sensitive deployment settings
//! the frontend needs to render platform-correct links.

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::AppState;
use crate::jwt::Caller;

/// Non-sensitive deployment settings for the web console.
#[derive(Debug, Serialize)]
pub struct DeploymentConfig {
    /// Web (non-API) GitLab base URL, e.g. `https://gitlab.com` or a self-hosted host. Derived from
    /// the GitLab config's `default_api_url` and used only as a fallback when a project-specific host
    /// is unavailable.
    pub gitlab_base_url: String,
    /// Project-specific GitLab web base URLs keyed by GitLab `project.id`. This preserves
    /// multi-project, multi-host deployments.
    pub gitlab_project_base_urls: BTreeMap<String, String>,
}

/// `GET /config` — deployment settings for the console. Authenticated, like the rest of the read API.
pub async fn deployment_config(
    _caller: Caller,
    State(state): State<AppState>,
) -> Json<DeploymentConfig> {
    let gitlab_base_url = state
        .gitlab
        .as_ref()
        .map(|registry| registry.web_base_url())
        .unwrap_or_else(|| "https://gitlab.com".to_string());
    let gitlab_project_base_urls = state
        .gitlab
        .as_ref()
        .map(|registry| registry.project_web_base_urls())
        .unwrap_or_default();
    Json(DeploymentConfig {
        gitlab_base_url,
        gitlab_project_base_urls,
    })
}
