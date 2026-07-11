use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Form, Path as AxumPath, Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_TYPE, ETAG, HOST, IF_NONE_MATCH, LOCATION, ORIGIN, REFERER,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use crate::catalogue::{Catalogue, CatalogueCounts, CataloguePolicy};
use crate::config::{EffectiveConfiguration, validate_proposal};
use crate::crypto::random_opaque_id;
use crate::csrf::CsrfStore;
use crate::error::{AppError, Success};
use crate::project::{
    LedgerQuery, ListProjectsQuery, ProjectList, ProjectService, RegisterProjectRequest,
    RegistrationOutcome,
};
use crate::storage_status::StorageStatus;
use crate::ui;

const BUILD_ID: &str = "project-ledger-v2";
const CSS: &str = include_str!("assets/app.css");
const JAVASCRIPT: &str = include_str!("assets/app.js");
const CSS_ETAG: &str =
    "\"sha256-4b0877d152aee07d74e9d400193509b77a04055c7a34b37bfccae64ce75e2ccb\"";
const JS_ETAG: &str = "\"sha256-5eca0221a848554e02a81d0f2184acd7928a8593763b8e2ad8ee0e517bf2d131\"";

#[derive(Clone)]
pub struct ApplicationState {
    configuration: Arc<EffectiveConfiguration>,
    catalogue: Catalogue,
    projects: ProjectService,
    csrf: CsrfStore,
}

impl ApplicationState {
    pub fn new(configuration: EffectiveConfiguration, catalogue: Catalogue) -> Self {
        let projects = ProjectService::new(
            catalogue.clone(),
            configuration.server.canonical_origin.clone(),
        );
        Self {
            configuration: Arc::new(configuration),
            catalogue,
            projects,
            csrf: CsrfStore::default(),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveProjectQuery {
    path: String,
}

#[derive(Default, Deserialize)]
struct UiIndexQuery {
    query: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectRegistrationForm {
    path: String,
    title: Option<String>,
    slug: Option<String>,
    csrf_token: String,
    idempotency_key: String,
}

pub fn router(state: ApplicationState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/ui/", get(ui_index))
        .route("/ui/projects/new/", get(ui_register_project))
        .route("/ui/projects/", post(ui_submit_project))
        .route("/ui/projects/{project_key}/", get(ui_project_detail))
        .route(&format!("/_static/{BUILD_ID}/app.css"), get(static_css))
        .route(
            &format!("/_static/{BUILD_ID}/app.js"),
            get(static_javascript),
        )
        .route(
            "/api/v1/projects",
            get(list_projects).post(register_project),
        )
        .route("/api/v1/projects/resolve", get(resolve_project))
        .route("/api/v1/projects/ledger", get(all_projects_ledger))
        .route("/api/v1/projects/{project_id}", get(show_project))
        .route("/api/v1/projects/{project_id}/ledger", get(project_ledger))
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

async fn ui_index(
    State(state): State<ApplicationState>,
    query: Result<Query<UiIndexQuery>, QueryRejection>,
) -> Response {
    let Query(query) = query.unwrap_or_default();
    let search = query.query.unwrap_or_default();
    let projects = state.projects.clone();
    let list_query = ListProjectsQuery {
        query: (!search.is_empty()).then(|| search.clone()),
        ..ListProjectsQuery::default()
    };
    match tokio::task::spawn_blocking(move || projects.list(&list_query)).await {
        Ok(Ok(projects)) => no_store(Html(ui::index(&projects, BUILD_ID, &search)).into_response()),
        Ok(Err(error)) => ui_error(&error),
        Err(error) => ui_error(&AppError::internal(format!(
            "Project index worker failed: {error}"
        ))),
    }
}

async fn ui_register_project(State(state): State<ApplicationState>) -> Response {
    let csrf_token = match state.csrf.issue("project.register") {
        Ok(token) => token,
        Err(error) => return ui_error(&error),
    };
    let idempotency_key = match random_opaque_id() {
        Ok(id) => format!("browser-project-register-{id}"),
        Err(error) => return ui_error(&error),
    };
    no_store(Html(ui::register_form(&csrf_token, &idempotency_key, BUILD_ID)).into_response())
}

async fn ui_project_detail(
    State(state): State<ApplicationState>,
    AxumPath(project_key): AxumPath<String>,
) -> Response {
    let Some((_, project_id)) = project_key.rsplit_once('~') else {
        return ui_error_status(
            StatusCode::NOT_FOUND,
            "Project not found",
            "The Project route is malformed or unknown.",
        );
    };
    let project_id = project_id.to_ascii_lowercase();
    let projects = state.projects.clone();
    match tokio::task::spawn_blocking(move || projects.show(&project_id)).await {
        Ok(Ok(project)) if project.key() == project_key => {
            no_store(Html(ui::project_detail(&project, BUILD_ID)).into_response())
        }
        Ok(Ok(project)) => {
            let Ok(location) = HeaderValue::from_str(project.detail_url()) else {
                return ui_error(&AppError::internal("Project detail URL is invalid"));
            };
            let mut response = StatusCode::PERMANENT_REDIRECT.into_response();
            response.headers_mut().insert(LOCATION, location);
            no_store(response)
        }
        Ok(Err(error)) => ui_error(&error),
        Err(error) => ui_error(&AppError::internal(format!(
            "Project detail worker failed: {error}"
        ))),
    }
}

async fn ui_submit_project(
    State(state): State<ApplicationState>,
    headers: HeaderMap,
    form: Result<Form<ProjectRegistrationForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    let form = match accepted_project_form(&state, &headers, form) {
        Ok(form) => form,
        Err(response) => return *response,
    };
    dispatch_browser_project_registration(state.projects, form).await
}

fn accepted_project_form(
    state: &ApplicationState,
    headers: &HeaderMap,
    form: Result<Form<ProjectRegistrationForm>, axum::extract::rejection::FormRejection>,
) -> Result<ProjectRegistrationForm, Box<Response>> {
    verify_browser_mutation(headers, &state.configuration.server.canonical_origin)
        .map_err(|error| Box::new(ui_error(&error)))?;
    let Form(form) = form.map_err(|rejection| {
        Box::new(ui_error_status(
            rejection.status(),
            "Invalid registration",
            &rejection.body_text(),
        ))
    })?;
    let csrf_token = browser_csrf_token(headers, &form.csrf_token)
        .map_err(|error| Box::new(ui_error(&error)))?;
    state
        .csrf
        .consume(csrf_token, "project.register")
        .map_err(|error| Box::new(ui_error(&error)))?;
    Ok(form)
}

async fn dispatch_browser_project_registration(
    projects: ProjectService,
    form: ProjectRegistrationForm,
) -> Response {
    let request = RegisterProjectRequest {
        path: form.path,
        title: optional_form_value(form.title),
        slug: optional_form_value(form.slug),
    };
    let idempotency_key = form.idempotency_key;
    match tokio::task::spawn_blocking(move || projects.register(request, &idempotency_key)).await {
        Ok(Ok(outcome)) => project_registration_redirect(&outcome),
        Ok(Err(error)) => ui_error(&error),
        Err(error) => ui_error(&AppError::internal(format!(
            "Project registration worker failed: {error}"
        ))),
    }
}

fn project_registration_redirect(outcome: &RegistrationOutcome) -> Response {
    let Ok(location) = HeaderValue::from_str(outcome.project().detail_url()) else {
        return ui_error(&AppError::internal("Project detail URL is invalid"));
    };
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(LOCATION, location);
    no_store(response)
}

fn browser_csrf_token<'a>(
    headers: &'a HeaderMap,
    form_token: &'a str,
) -> Result<&'a str, AppError> {
    let Some(header) = headers.get("x-observatory-csrf") else {
        return Ok(form_token);
    };
    let header = header.to_str().map_err(|_| {
        AppError::forbidden("csrf_rejected", "browser mutation CSRF header is malformed")
    })?;
    if header != form_token {
        return Err(AppError::forbidden(
            "csrf_rejected",
            "browser mutation CSRF header does not match the form capability",
        ));
    }
    Ok(header)
}

fn optional_form_value(value: Option<String>) -> Option<String> {
    value.and_then(|value| (!value.trim().is_empty()).then(|| value.trim().to_owned()))
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

async fn list_projects(
    State(state): State<ApplicationState>,
    query: Result<Query<ListProjectsQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return api_failure(rejection.status(), &AppError::usage(rejection.body_text()));
        }
    };
    let projects = state.projects.clone();
    match tokio::task::spawn_blocking(move || projects.list(&query)).await {
        Ok(Ok(result)) => project_list_response(&result),
        Ok(Err(error)) => api_error(&error),
        Err(error) => api_error(&AppError::internal(format!(
            "Project list worker failed: {error}"
        ))),
    }
}

fn project_list_response(result: &ProjectList) -> Response {
    let mut response = api_success(result);
    if let Some(link) = result.next_link() {
        let Ok(link) = HeaderValue::from_str(&format!("<{link}>; rel=\"next\"")) else {
            return api_error(&AppError::internal("Project next Link is invalid"));
        };
        response
            .headers_mut()
            .insert(axum::http::header::LINK, link);
    }
    response
}

async fn resolve_project(
    State(state): State<ApplicationState>,
    query: Result<Query<ResolveProjectQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return api_failure(rejection.status(), &AppError::usage(rejection.body_text()));
        }
    };
    let projects = state.projects.clone();
    match tokio::task::spawn_blocking(move || projects.resolve(query.path)).await {
        Ok(Ok(result)) => api_success(result),
        Ok(Err(error)) => api_error(&error),
        Err(error) => api_error(&AppError::internal(format!(
            "Project resolve worker failed: {error}"
        ))),
    }
}

async fn show_project(
    State(state): State<ApplicationState>,
    AxumPath(project_id): AxumPath<String>,
) -> Response {
    let projects = state.projects.clone();
    match tokio::task::spawn_blocking(move || projects.show(&project_id)).await {
        Ok(Ok(project)) => {
            let etag = project.etag();
            let mut response = api_success(project);
            let Ok(etag) = HeaderValue::from_str(&etag) else {
                return api_error(&AppError::internal("Project ETag is invalid"));
            };
            response.headers_mut().insert(ETAG, etag);
            response
        }
        Ok(Err(error)) => api_error(&error),
        Err(error) => api_error(&AppError::internal(format!(
            "Project show worker failed: {error}"
        ))),
    }
}

async fn all_projects_ledger(
    State(state): State<ApplicationState>,
    query: Result<Query<LedgerQuery>, QueryRejection>,
) -> Response {
    ledger_response(state, query, None).await
}

async fn project_ledger(
    State(state): State<ApplicationState>,
    AxumPath(project_id): AxumPath<String>,
    query: Result<Query<LedgerQuery>, QueryRejection>,
) -> Response {
    ledger_response(state, query, Some(project_id)).await
}

async fn ledger_response(
    state: ApplicationState,
    query: Result<Query<LedgerQuery>, QueryRejection>,
    project_id: Option<String>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return api_failure(rejection.status(), &AppError::usage(rejection.body_text()));
        }
    };
    let projects = state.projects.clone();
    match tokio::task::spawn_blocking(move || projects.ledger(query, project_id)).await {
        Ok(Ok(result)) => api_success(result),
        Ok(Err(error)) => api_error(&error),
        Err(error) => api_error(&AppError::internal(format!(
            "Project ledger worker failed: {error}"
        ))),
    }
}

async fn register_project(
    State(state): State<ApplicationState>,
    headers: HeaderMap,
    request: Result<Json<RegisterProjectRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => {
            return api_failure(rejection.status(), &AppError::usage(rejection.body_text()));
        }
    };
    if let Err(error) = authorize_api_browser_mutation(&state, &headers, "project.register") {
        return api_error(&error);
    }
    let Some(idempotency_key) = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        return api_error(&AppError::invalid(
            "invalid_idempotency_key",
            "Idempotency-Key is required",
        ));
    };
    let projects = state.projects.clone();
    match tokio::task::spawn_blocking(move || projects.register(request, &idempotency_key)).await {
        Ok(Ok(outcome)) => project_registration_response(&outcome),
        Ok(Err(error)) => api_error(&error),
        Err(error) => api_error(&AppError::internal(format!(
            "Project registration worker failed: {error}"
        ))),
    }
}

fn project_registration_response(outcome: &RegistrationOutcome) -> Response {
    let project = outcome.project();
    let mut response = (StatusCode::CREATED, Json(Success::new(project))).into_response();
    for (name, value) in [
        ("etag", project.etag()),
        ("location", project.api_url().to_owned()),
    ] {
        let Ok(value) = HeaderValue::from_str(&value) else {
            return api_error(&AppError::internal(format!(
                "Project response {name} is not a valid header"
            )));
        };
        let Ok(name) = axum::http::header::HeaderName::from_bytes(name.as_bytes()) else {
            return api_error(&AppError::internal("Project response header is invalid"));
        };
        response.headers_mut().insert(name, value);
    }
    if outcome.replayed() {
        response.headers_mut().insert(
            axum::http::header::HeaderName::from_static("idempotency-replayed"),
            HeaderValue::from_static("true"),
        );
    }
    no_store(response)
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

fn authorize_api_browser_mutation(
    state: &ApplicationState,
    headers: &HeaderMap,
    action: &str,
) -> Result<(), AppError> {
    let browser_request = [
        ORIGIN.as_str(),
        REFERER.as_str(),
        "sec-fetch-site",
        "sec-fetch-mode",
    ]
    .iter()
    .any(|header| headers.contains_key(*header));
    if !browser_request {
        return Ok(());
    }
    verify_browser_mutation(headers, &state.configuration.server.canonical_origin)?;
    let token = headers
        .get("x-observatory-csrf")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            AppError::forbidden(
                "csrf_rejected",
                "browser API mutation requires X-Observatory-CSRF",
            )
        })?;
    state.csrf.consume(token, action)
}

fn verify_browser_mutation(headers: &HeaderMap, canonical_origin: &str) -> Result<(), AppError> {
    let canonical = url::Url::parse(canonical_origin)
        .map_err(|error| AppError::internal(format!("canonical origin is invalid: {error}")))?;
    let expected_host = &canonical[url::Position::BeforeHost..url::Position::AfterPort];
    let supplied_host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !supplied_host.eq_ignore_ascii_case(expected_host) {
        return Err(AppError::forbidden(
            "browser_origin_rejected",
            "browser mutation Host does not match the canonical Observatory origin",
        ));
    }
    let expected_origin = canonical.origin();
    let expected_origin_header = expected_origin.ascii_serialization();
    let source_matches = match headers.get(ORIGIN) {
        Some(origin) => origin
            .to_str()
            .is_ok_and(|origin| origin == expected_origin_header),
        None => headers
            .get(REFERER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| url::Url::parse(value).ok())
            .is_some_and(|referer| referer.origin() == expected_origin),
    };
    if !source_matches {
        return Err(AppError::forbidden(
            "browser_origin_rejected",
            "browser mutation Origin or Referer does not match the canonical Observatory origin",
        ));
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| value != "same-origin")
    {
        return Err(AppError::forbidden(
            "browser_origin_rejected",
            "browser mutation Fetch Metadata is not same-origin",
        ));
    }
    Ok(())
}

fn ui_error(error: &AppError) -> Response {
    let status =
        StatusCode::from_u16(error.api_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let message = format!("{} [{}]", error.message, error.code());
    ui_error_status(status, "Request failed", &message)
}

fn ui_error_status(status: StatusCode, title: &str, message: &str) -> Response {
    no_store((status, Html(ui::error(title, message, BUILD_ID))).into_response())
}

fn api_success<T: Serialize>(result: T) -> Response {
    no_store(Json(Success::new(result)).into_response())
}

fn api_error(error: &AppError) -> Response {
    let status =
        StatusCode::from_u16(error.api_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = no_store((status, Json(error.envelope())).into_response());
    if error.code() == "idempotency_in_progress" {
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            HeaderValue::from_static("1"),
        );
    }
    response
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
