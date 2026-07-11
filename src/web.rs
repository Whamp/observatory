use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH, LOCATION};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use crate::catalogue::{Catalogue, CatalogueCounts, CataloguePolicy};
use crate::config::{EffectiveConfiguration, validate_proposal};
use crate::error::{AppError, Success};
use crate::storage_status::StorageStatus;

const BUILD_ID: &str = "empty-ledger-v1";
const HTML: &str = include_str!("assets/index.html");
const CSS: &str = include_str!("assets/app.css");
const JAVASCRIPT: &str = include_str!("assets/app.js");
const CSS_ETAG: &str =
    "\"sha256-ac9f519a179fdaf4521c3c7d79737f8192685e17fe2ba817076443c8689b4daf\"";
const JS_ETAG: &str = "\"sha256-dd74f8ff234697b9e2fb9d5f94f99c6ef24be892f30a58d6ed753487e0760982\"";

#[derive(Clone)]
pub struct ApplicationState {
    configuration: Arc<EffectiveConfiguration>,
    catalogue: Catalogue,
}

impl ApplicationState {
    pub fn new(configuration: EffectiveConfiguration, catalogue: Catalogue) -> Self {
        Self {
            configuration: Arc::new(configuration),
            catalogue,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Health {
    ready: bool,
    build_id: &'static str,
    api_version: u8,
    #[serde(rename = "storageHealth")]
    storage: &'static str,
    migration: &'static str,
    startup_reconciliation: &'static str,
    background_workers: &'static str,
    tailscale: &'static str,
}

#[derive(Serialize)]
struct Status {
    #[serde(flatten)]
    storage: StorageStatus,
    catalogue: CatalogueCounts,
    policy: CataloguePolicy,
}

#[derive(Deserialize)]
struct ValidationProposal {
    content: String,
}

pub fn router(state: ApplicationState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/ui/", get(ui))
        .route(&format!("/_static/{BUILD_ID}/app.css"), get(static_css))
        .route(
            &format!("/_static/{BUILD_ID}/app.js"),
            get(static_javascript),
        )
        .route("/api/v1/system/health", get(health))
        .route("/api/v1/system/status", get(status))
        .route("/api/v1/system/configuration", get(configuration))
        .route(
            "/api/v1/system/configuration/validate",
            post(validate_configuration),
        )
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state)
}

async fn root(State(state): State<ApplicationState>) -> Response {
    let location = format!("{}ui/", state.configuration.server.canonical_origin);
    let Ok(location) = HeaderValue::from_str(&location) else {
        return api_error(&AppError::internal(
            "canonical UI URL is not a valid header",
        ));
    };
    let mut response = StatusCode::PERMANENT_REDIRECT.into_response();
    response.headers_mut().insert(LOCATION, location);
    no_store(response)
}

async fn ui() -> Response {
    no_store(Html(HTML).into_response())
}

async fn static_css(headers: HeaderMap) -> Response {
    immutable_asset("text/css; charset=utf-8", CSS, CSS_ETAG, &headers)
}

async fn static_javascript(headers: HeaderMap) -> Response {
    immutable_asset(
        "text/javascript; charset=utf-8",
        JAVASCRIPT,
        JS_ETAG,
        &headers,
    )
}

async fn health(State(state): State<ApplicationState>) -> Response {
    let catalogue = state.catalogue.clone();
    match tokio::task::spawn_blocking(move || catalogue.status()).await {
        Ok(Ok(storage)) => readiness(storage.ready(), storage.health_name()),
        Ok(Err(_)) | Err(_) => readiness(false, "offline"),
    }
}

fn readiness(ready: bool, storage: &'static str) -> Response {
    api_success(Health {
        ready,
        build_id: BUILD_ID,
        api_version: 1,
        storage,
        migration: "complete",
        startup_reconciliation: "complete",
        background_workers: "idle",
        tailscale: "unconfigured",
    })
}

async fn status(State(state): State<ApplicationState>) -> Response {
    let catalogue = state.catalogue.clone();
    match tokio::task::spawn_blocking(move || {
        Ok::<_, AppError>(Status {
            storage: catalogue.status()?,
            catalogue: catalogue.counts()?,
            policy: catalogue.policy()?,
        })
    })
    .await
    {
        Ok(Ok(status)) => api_success(status),
        Ok(Err(error)) => api_error(&error),
        Err(error) => api_error(&AppError::internal(format!(
            "status worker failed: {error}"
        ))),
    }
}

async fn configuration(State(state): State<ApplicationState>) -> Response {
    api_success(state.configuration.redacted())
}

async fn validate_configuration(
    proposal: Result<Json<ValidationProposal>, JsonRejection>,
) -> Response {
    match proposal {
        Ok(Json(proposal)) => api_success(validate_proposal(&proposal.content)),
        Err(rejection) => api_failure(rejection.status(), &AppError::usage(rejection.body_text())),
    }
}

async fn not_found() -> Response {
    api_failure(
        StatusCode::NOT_FOUND,
        &AppError::not_found("the requested route does not exist"),
    )
}

fn api_success<T: Serialize>(result: T) -> Response {
    no_store(Json(Success::new(result)).into_response())
}

fn api_error(error: &AppError) -> Response {
    no_store((StatusCode::INTERNAL_SERVER_ERROR, Json(error.envelope())).into_response())
}

fn api_failure(status: StatusCode, error: &AppError) -> Response {
    no_store((status, Json(error.envelope())).into_response())
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn immutable_asset(
    content_type: &'static str,
    content: &'static str,
    etag: &'static str,
    request_headers: &HeaderMap,
) -> Response {
    let not_modified = request_headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag));
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Response::new(Body::from(content))
    };
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response
        .headers_mut()
        .insert(ETAG, HeaderValue::from_static(etag));
    response
}
