use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use caseless::default_case_fold_str;
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::macros::format_description;
use time::{Duration, OffsetDateTime};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::catalogue::Catalogue;
use crate::crypto::random_opaque_id;
use crate::error::AppError;
use crate::safe_file::open_directory;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
pub struct ProjectService {
    catalogue: Catalogue,
    canonical_origin: String,
    in_flight_registrations: Arc<Mutex<HashMap<String, String>>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterProjectRequest {
    pub path: String,
    pub title: Option<String>,
    pub slug: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    kind: String,
    id: String,
    key: String,
    record_version: u64,
    state: ProjectState,
    title: String,
    slug: String,
    canonical_directory: String,
    created_at: String,
    updated_at: String,
    api_url: String,
    detail_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tombstoned_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ProjectState {
    Live,
    Gone,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectReference {
    id: String,
    key: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveProjectResult {
    input_path: String,
    canonical_directory: String,
    status: ResolveStatus,
    project: Option<ProjectReference>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum ResolveStatus {
    Registered,
    Unregistered,
    Gone,
}

#[derive(Clone, Debug)]
pub struct RegistrationOutcome {
    project: Project,
    replayed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistrationFingerprint<'a> {
    api_version: u8,
    method: &'static str,
    route: &'static str,
    body: &'a RegisterProjectRequest,
    canonical_directory: &'a str,
}

struct PreparedRegistration {
    canonical_directory: String,
    title: String,
    slug: String,
    title_fold: String,
    search_text: String,
    fingerprint: String,
    _directory: fs::File,
}

struct IdempotencyRecord {
    fingerprint: String,
    state: String,
    response_json: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListProjectsQuery {
    pub state: Option<String>,
    pub query: Option<String>,
    pub order: Option<String>,
    pub direction: Option<String>,
    pub limit: Option<u16>,
    pub after: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LedgerQuery {
    pub project_id: Option<String>,
    pub kind: Option<String>,
    pub query: Option<String>,
    pub order: Option<String>,
    pub direction: Option<String>,
    pub limit: Option<u16>,
    pub after: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectList {
    items: Vec<Project>,
    page: Pagination,
    #[serde(skip)]
    next_link: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    items: Vec<serde_json::Value>,
    page: Pagination,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Pagination {
    limit: u16,
    next_cursor: Option<String>,
    has_more: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ProjectFilter {
    Live,
    Gone,
    All,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ProjectOrder {
    Recent,
    Title,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Direction {
    Asc,
    Desc,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectCursor {
    endpoint: String,
    state: ProjectFilter,
    query: String,
    order: ProjectOrder,
    direction: Direction,
    last_value: String,
    last_id: String,
    expires_at_ms: i128,
}

struct RegistrationGuard {
    registrations: Arc<Mutex<HashMap<String, String>>>,
    key: String,
}

struct NormalizedProjectList {
    state: ProjectFilter,
    query: String,
    order: ProjectOrder,
    direction: Direction,
    limit: u16,
    cursor: Option<ProjectCursor>,
}

impl ProjectService {
    pub fn new(catalogue: Catalogue, canonical_origin: String) -> Self {
        Self {
            catalogue,
            canonical_origin,
            in_flight_registrations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn resolve(&self, input_path: String) -> Result<ResolveProjectResult, AppError> {
        let canonical_directory = canonical_directory(&input_path)?.0;
        let connection = self.catalogue.connection()?;
        let project = connection
            .query_row(
                &project_select("WHERE canonical_directory = ?1"),
                [&canonical_directory],
                |row| project_from_row(row, &self.canonical_origin),
            )
            .optional()
            .map_err(database_error)?;
        let (status, reference) = match project {
            Some(project) if project.state == ProjectState::Live => (
                ResolveStatus::Registered,
                Some(ProjectReference::from(&project)),
            ),
            Some(project) => (ResolveStatus::Gone, Some(ProjectReference::from(&project))),
            None => (ResolveStatus::Unregistered, None),
        };
        Ok(ResolveProjectResult {
            input_path,
            canonical_directory,
            status,
            project: reference,
        })
    }

    pub fn register(
        &self,
        request: RegisterProjectRequest,
        idempotency_key: &str,
    ) -> Result<RegistrationOutcome, AppError> {
        validate_idempotency_key(idempotency_key)?;
        let prepared = Self::prepare_registration(request)?;
        let _registration = self.begin_registration(idempotency_key, &prepared.fingerprint)?;
        let mut connection = self.catalogue.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;

        if let Some(record) = idempotency_record(&transaction, idempotency_key)? {
            return replay_registration(record, &prepared.fingerprint);
        }
        transaction
            .execute(
                "INSERT INTO idempotency_requests(key, fingerprint, state) VALUES (?1, ?2, 'in_progress')",
                params![idempotency_key, prepared.fingerprint],
            )
            .map_err(database_error)?;
        reject_registered_directory(&transaction, &prepared.canonical_directory)?;
        let project = self.insert_project(&transaction, &prepared)?;
        let response_json = serde_json::to_string(&project)
            .map_err(|error| AppError::internal(format!("cannot store Project result: {error}")))?;
        transaction
            .execute(
                "INSERT INTO audit_events(kind, details_json, at, actor, cause, resource_type, resource_id)
                 VALUES ('project_registered', ?1, ?2, 'operator', 'project_registered', 'project', ?3)",
                params![
                    serde_json::json!({ "projectId": project.id }).to_string(),
                    project.created_at,
                    project.id,
                ],
            )
            .map_err(database_error)?;
        transaction
            .execute(
                "UPDATE idempotency_requests
                 SET state='completed', status_code=201, response_json=?2, etag=?3, completed_at=?4
                 WHERE key=?1",
                params![
                    idempotency_key,
                    response_json,
                    project.etag(),
                    project.created_at,
                ],
            )
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)?;
        Ok(RegistrationOutcome {
            project,
            replayed: false,
        })
    }

    fn begin_registration(
        &self,
        idempotency_key: &str,
        fingerprint: &str,
    ) -> Result<RegistrationGuard, AppError> {
        let mut registrations = self
            .in_flight_registrations
            .lock()
            .map_err(|_| AppError::internal("registration coordination is unavailable"))?;
        if let Some(active_fingerprint) = registrations.get(idempotency_key) {
            return if active_fingerprint == fingerprint {
                Err(AppError::retryable_conflict(
                    "idempotency_in_progress",
                    "an identical request with this Idempotency-Key is still in progress",
                ))
            } else {
                Err(AppError::conflict(
                    "idempotency_conflict",
                    "Idempotency-Key is bound to a different request",
                ))
            };
        }
        registrations.insert(idempotency_key.to_owned(), fingerprint.to_owned());
        Ok(RegistrationGuard {
            registrations: Arc::clone(&self.in_flight_registrations),
            key: idempotency_key.to_owned(),
        })
    }

    fn prepare_registration(
        request: RegisterProjectRequest,
    ) -> Result<PreparedRegistration, AppError> {
        let (canonical_directory, directory) = canonical_directory(&request.path)?;
        let fingerprint = registration_fingerprint(&request, &canonical_directory)?;
        let title = project_title(request.title, &canonical_directory)?;
        let slug = project_slug(request.slug.as_deref(), &title)?;
        let title_fold = default_case_fold_str(&title);
        let search_text = default_case_fold_str(&format!("{title}\n{slug}\n{canonical_directory}"));
        Ok(PreparedRegistration {
            canonical_directory,
            title,
            slug,
            title_fold,
            search_text,
            fingerprint,
            _directory: directory,
        })
    }

    fn insert_project(
        &self,
        transaction: &Transaction<'_>,
        prepared: &PreparedRegistration,
    ) -> Result<Project, AppError> {
        let id = allocate_project_id(transaction)?;
        let key = format!("{}~{id}", prepared.slug);
        let now = observed_at()?;
        transaction
            .execute(
                "INSERT INTO projects(
                   id, record_version, canonical_directory, state, title, slug,
                   title_fold, search_text, created_at, updated_at
                 ) VALUES (?1, 1, ?2, 'live', ?3, ?4, ?5, ?6, ?7, ?7)",
                params![
                    id,
                    prepared.canonical_directory,
                    prepared.title,
                    prepared.slug,
                    prepared.title_fold,
                    prepared.search_text,
                    now,
                ],
            )
            .map_err(database_error)?;
        Ok(Project {
            kind: "project".to_owned(),
            id: id.clone(),
            key,
            record_version: 1,
            state: ProjectState::Live,
            title: prepared.title.clone(),
            slug: prepared.slug.clone(),
            canonical_directory: prepared.canonical_directory.clone(),
            created_at: now.clone(),
            updated_at: now,
            api_url: format!("{}api/v1/projects/{id}", self.canonical_origin),
            detail_url: format!(
                "{}ui/projects/{}~{id}/",
                self.canonical_origin, prepared.slug
            ),
            terminal_state: None,
            tombstoned_at: None,
            cause: None,
        })
    }
}

impl ProjectService {
    pub fn list(&self, query: &ListProjectsQuery) -> Result<ProjectList, AppError> {
        let connection = self.catalogue.connection()?;
        let selection = normalize_project_list(query, &connection)?;
        let (comparison, order) = match (selection.order, selection.direction) {
            (ProjectOrder::Title, Direction::Asc) => (
                "title_fold > ?3 OR (title_fold = ?3 AND id > ?4)",
                "title_fold ASC, id ASC",
            ),
            (ProjectOrder::Title, Direction::Desc) => (
                "title_fold < ?3 OR (title_fold = ?3 AND id > ?4)",
                "title_fold DESC, id ASC",
            ),
            (ProjectOrder::Recent, Direction::Asc) => (
                "updated_at > ?3 OR (updated_at = ?3 AND id > ?4)",
                "updated_at ASC, id ASC",
            ),
            (ProjectOrder::Recent, Direction::Desc) => (
                "updated_at < ?3 OR (updated_at = ?3 AND id > ?4)",
                "updated_at DESC, id ASC",
            ),
        };
        let sql = project_select(&format!(
            "WHERE (?1 = 'all' OR state = ?1)
             AND (?2 = '' OR instr(search_text, ?2) > 0)
             AND (?3 IS NULL OR {comparison})
             ORDER BY {order} LIMIT ?5"
        ));
        let state = selection.state.as_str();
        let last_value = selection
            .cursor
            .as_ref()
            .map(|cursor| cursor.last_value.as_str());
        let last_id = selection
            .cursor
            .as_ref()
            .map(|cursor| cursor.last_id.as_str());
        let requested = usize::from(selection.limit);
        let fetch_limit = i64::from(selection.limit) + 1;
        let mut statement = connection.prepare(&sql).map_err(database_error)?;
        let rows = statement
            .query_map(
                params![state, selection.query, last_value, last_id, fetch_limit],
                |row| project_from_row(row, &self.canonical_origin),
            )
            .map_err(database_error)?;
        let mut items = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(database_error)?;
        let has_more = items.len() > requested;
        items.truncate(requested);
        let next_cursor = if has_more {
            let last = items
                .last()
                .ok_or_else(|| AppError::internal("Project page has no cursor boundary"))?;
            Some(encode_cursor(
                &connection,
                &ProjectCursor {
                    endpoint: "projects".to_owned(),
                    state: selection.state,
                    query: selection.query.clone(),
                    order: selection.order,
                    direction: selection.direction,
                    last_value: match selection.order {
                        ProjectOrder::Title => default_case_fold_str(&last.title),
                        ProjectOrder::Recent => last.updated_at.clone(),
                    },
                    last_id: last.id.clone(),
                    expires_at_ms: (OffsetDateTime::now_utc() + Duration::minutes(15))
                        .unix_timestamp_nanos()
                        / 1_000_000,
                },
            )?)
        } else {
            None
        };
        let next_link = next_cursor
            .as_ref()
            .map(|cursor| project_next_link(&self.canonical_origin, &selection, cursor))
            .transpose()?;
        Ok(ProjectList {
            items,
            page: Pagination {
                limit: selection.limit,
                next_cursor,
                has_more,
            },
            next_link,
        })
    }

    pub fn show(&self, id: &str) -> Result<Project, AppError> {
        validate_project_id(id)?;
        let connection = self.catalogue.connection()?;
        let project = connection
            .query_row(&project_select("WHERE id = ?1"), [id], |row| {
                project_from_row(row, &self.canonical_origin)
            })
            .optional()
            .map_err(database_error)?
            .ok_or_else(|| AppError::not_found("Project does not exist"))?;
        if project.state == ProjectState::Gone {
            return Err(AppError::gone(
                "project_gone",
                "Project has a terminal identity",
            ));
        }
        Ok(project)
    }

    pub fn ledger(
        &self,
        query: LedgerQuery,
        scoped_project_id: Option<String>,
    ) -> Result<Ledger, AppError> {
        let limit = validate_limit(query.limit)?;
        if query.after.is_some() {
            return Err(AppError::invalid(
                "invalid_cursor",
                "empty ledger has no valid continuation cursor",
            ));
        }
        if !matches!(
            query.kind.as_deref(),
            None | Some("all" | "artifact" | "service")
        ) {
            return Err(AppError::invalid("invalid_filter", "invalid ledger kind"));
        }
        if !matches!(
            query.order.as_deref(),
            None | Some("recent" | "title" | "attention")
        ) {
            return Err(AppError::invalid("invalid_order", "invalid ledger order"));
        }
        validate_direction(query.direction.as_deref())?;
        let folded_query = default_case_fold_str(query.query.as_deref().unwrap_or(""));
        if folded_query.chars().count() > 500 {
            return Err(AppError::invalid(
                "invalid_filter",
                "ledger query exceeds 500 characters",
            ));
        }
        if scoped_project_id.is_some()
            && query.project_id.is_some()
            && scoped_project_id != query.project_id
        {
            return Err(AppError::invalid(
                "invalid_filter",
                "projectId conflicts with the Project-scoped ledger route",
            ));
        }
        let selected_project = scoped_project_id.or(query.project_id);
        if let Some(id) = selected_project {
            self.show(&id)?;
        }
        Ok(Ledger {
            items: Vec::new(),
            page: Pagination {
                limit,
                next_cursor: None,
                has_more: false,
            },
        })
    }
}

impl ProjectList {
    pub fn items(&self) -> &[Project] {
        &self.items
    }

    pub fn next_link(&self) -> Option<&str> {
        self.next_link.as_deref()
    }
}

impl Project {
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn detail_url(&self) -> &str {
        &self.detail_url
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn canonical_directory(&self) -> &str {
        &self.canonical_directory
    }

    pub fn etag(&self) -> String {
        format!("\"rv-{}\"", self.record_version)
    }
}

impl RegistrationOutcome {
    pub fn project(&self) -> &Project {
        &self.project
    }

    pub const fn replayed(&self) -> bool {
        self.replayed
    }
}

impl From<&Project> for ProjectReference {
    fn from(project: &Project) -> Self {
        Self {
            id: project.id.clone(),
            key: project.key.clone(),
        }
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        if let Ok(mut registrations) = self.registrations.lock() {
            registrations.remove(&self.key);
        }
    }
}

impl ProjectFilter {
    fn parse(value: Option<&str>) -> Result<Self, AppError> {
        match value.unwrap_or("live") {
            "live" => Ok(Self::Live),
            "gone" => Ok(Self::Gone),
            "all" => Ok(Self::All),
            _ => Err(AppError::invalid("invalid_filter", "invalid Project state")),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Gone => "gone",
            Self::All => "all",
        }
    }
}

impl ProjectOrder {
    fn parse(value: Option<&str>) -> Result<Self, AppError> {
        match value.unwrap_or("recent") {
            "recent" => Ok(Self::Recent),
            "title" => Ok(Self::Title),
            _ => Err(AppError::invalid("invalid_order", "invalid Project order")),
        }
    }
}

impl Direction {
    fn parse(value: Option<&str>) -> Result<Self, AppError> {
        match value.unwrap_or("desc") {
            "asc" => Ok(Self::Asc),
            "desc" => Ok(Self::Desc),
            _ => Err(AppError::invalid(
                "invalid_direction",
                "invalid Project direction",
            )),
        }
    }
}

fn normalize_project_list(
    query: &ListProjectsQuery,
    connection: &Connection,
) -> Result<NormalizedProjectList, AppError> {
    let state = ProjectFilter::parse(query.state.as_deref())?;
    let order = ProjectOrder::parse(query.order.as_deref())?;
    let direction = Direction::parse(query.direction.as_deref())?;
    let limit = validate_limit(query.limit)?;
    let folded_query = default_case_fold_str(query.query.as_deref().unwrap_or(""));
    if folded_query.chars().count() > 500 {
        return Err(AppError::invalid(
            "invalid_filter",
            "Project query exceeds 500 characters",
        ));
    }
    let cursor = query
        .after
        .as_deref()
        .map(|token| decode_cursor(connection, token))
        .transpose()?;
    if cursor.as_ref().is_some_and(|cursor| {
        cursor.endpoint != "projects"
            || cursor.state != state
            || cursor.query != folded_query
            || cursor.order != order
            || cursor.direction != direction
    }) {
        return Err(AppError::invalid(
            "invalid_cursor",
            "cursor does not match Project filters or order",
        ));
    }
    Ok(NormalizedProjectList {
        state,
        query: folded_query,
        order,
        direction,
        limit,
        cursor,
    })
}

fn validate_limit(limit: Option<u16>) -> Result<u16, AppError> {
    let limit = limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(AppError::invalid(
            "invalid_limit",
            "limit must be between 1 and 200",
        ));
    }
    Ok(limit)
}

fn validate_direction(direction: Option<&str>) -> Result<(), AppError> {
    if matches!(direction, None | Some("asc" | "desc")) {
        Ok(())
    } else {
        Err(AppError::invalid(
            "invalid_direction",
            "direction must be asc or desc",
        ))
    }
}

fn validate_project_id(id: &str) -> Result<(), AppError> {
    let valid = id.len() == 26
        && id
            .bytes()
            .all(|byte| b"0123456789abcdefghjkmnpqrstvwxyz".contains(&byte))
        && id.as_bytes()[0] <= b'7';
    if !valid {
        return Err(AppError::invalid(
            "invalid_project_id",
            "Project ID must be 26 lowercase Crockford-base32 characters",
        ));
    }
    Ok(())
}

fn encode_cursor(connection: &Connection, cursor: &ProjectCursor) -> Result<String, AppError> {
    let payload = serde_jcs::to_vec(&cursor)
        .map_err(|error| AppError::internal(format!("cannot encode cursor: {error}")))?;
    let secret = cursor_secret(connection)?;
    let mut mac = HmacSha256::new_from_slice(&secret)
        .map_err(|_| AppError::internal("cursor secret is invalid"))?;
    mac.update(&payload);
    let signature = mac.finalize().into_bytes();
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

fn decode_cursor(connection: &Connection, token: &str) -> Result<ProjectCursor, AppError> {
    let Some((payload, signature)) = token.split_once('.') else {
        return Err(AppError::invalid("invalid_cursor", "cursor is malformed"));
    };
    if signature.contains('.') {
        return Err(AppError::invalid("invalid_cursor", "cursor is malformed"));
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AppError::invalid("invalid_cursor", "cursor is malformed"))?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| AppError::invalid("invalid_cursor", "cursor is malformed"))?;
    let secret = cursor_secret(connection)?;
    let mut mac = HmacSha256::new_from_slice(&secret)
        .map_err(|_| AppError::internal("cursor secret is invalid"))?;
    mac.update(&payload);
    mac.verify_slice(&signature)
        .map_err(|_| AppError::invalid("invalid_cursor", "cursor signature is invalid"))?;
    let cursor: ProjectCursor = serde_json::from_slice(&payload)
        .map_err(|_| AppError::invalid("invalid_cursor", "cursor payload is invalid"))?;
    let now_ms = OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    if cursor.expires_at_ms <= now_ms {
        return Err(AppError::conflict("cursor_expired", "cursor has expired"));
    }
    validate_project_id(&cursor.last_id)
        .map_err(|_| AppError::invalid("invalid_cursor", "cursor boundary is invalid"))?;
    Ok(cursor)
}

fn cursor_secret(connection: &Connection) -> Result<Vec<u8>, AppError> {
    let secret = connection
        .query_row(
            "SELECT value FROM system_state WHERE key='cursor_secret'",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map_err(database_error)?;
    if secret.len() != 32 {
        return Err(AppError::internal("cursor secret has an invalid length"));
    }
    Ok(secret)
}

fn project_next_link(
    canonical_origin: &str,
    selection: &NormalizedProjectList,
    cursor: &str,
) -> Result<String, AppError> {
    let mut url = url::Url::parse(canonical_origin)
        .and_then(|origin| origin.join("api/v1/projects"))
        .map_err(|error| AppError::internal(format!("cannot build Project Link: {error}")))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("state", selection.state.as_str());
        if !selection.query.is_empty() {
            query.append_pair("query", &selection.query);
        }
        query.append_pair(
            "order",
            match selection.order {
                ProjectOrder::Recent => "recent",
                ProjectOrder::Title => "title",
            },
        );
        query.append_pair(
            "direction",
            match selection.direction {
                Direction::Asc => "asc",
                Direction::Desc => "desc",
            },
        );
        query.append_pair("limit", &selection.limit.to_string());
        query.append_pair("after", cursor);
    }
    Ok(url.into())
}

fn canonical_directory(input: &str) -> Result<(String, fs::File), AppError> {
    let path = PathBuf::from(input);
    if !path.is_absolute() {
        return Err(AppError::invalid(
            "invalid_project_directory",
            "Project directory must be absolute",
        ));
    }
    let canonical = fs::canonicalize(&path).map_err(|_| {
        AppError::invalid(
            "invalid_project_directory",
            "Project directory does not exist or is inaccessible",
        )
    })?;
    let directory = open_directory(&canonical).map_err(|_| {
        AppError::invalid(
            "invalid_project_directory",
            "Project directory cannot be opened without following links",
        )
    })?;
    let descriptor_metadata = directory.metadata().map_err(|_| {
        AppError::invalid(
            "invalid_project_directory",
            "Project directory descriptor is inaccessible",
        )
    })?;
    let path_metadata = fs::metadata(&canonical).map_err(|_| {
        AppError::invalid(
            "invalid_project_directory",
            "Project directory changed during resolution",
        )
    })?;
    if descriptor_metadata.dev() != path_metadata.dev()
        || descriptor_metadata.ino() != path_metadata.ino()
    {
        return Err(AppError::invalid(
            "invalid_project_directory",
            "Project directory changed during resolution",
        ));
    }
    let canonical = canonical
        .to_str()
        .ok_or_else(|| {
            AppError::invalid(
                "invalid_project_directory",
                "Project directory is not valid UTF-8",
            )
        })?
        .to_owned();
    Ok((canonical, directory))
}

fn project_title(title: Option<String>, canonical_directory: &str) -> Result<String, AppError> {
    let title = match title {
        Some(title) => title.trim().to_owned(),
        None => Path::new(canonical_directory)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("Project")
            .to_owned(),
    };
    if title.is_empty() || title.chars().any(char::is_control) || title.chars().count() > 200 {
        return Err(AppError::invalid(
            "invalid_project_title",
            "Project title must be 1..200 control-free characters",
        ));
    }
    Ok(title)
}

fn project_slug(slug: Option<&str>, title: &str) -> Result<String, AppError> {
    let supplied = slug.is_some();
    let normalized = normalize_slug(slug.unwrap_or(title));
    if normalized.is_empty() && supplied {
        return Err(AppError::invalid(
            "invalid_project_slug",
            "supplied Project slug normalizes to empty",
        ));
    }
    Ok(if normalized.is_empty() {
        "project".to_owned()
    } else {
        normalized
    })
}

fn normalize_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in value
        .nfkd()
        .filter(|character| character.is_ascii() && !is_combining_mark(*character))
    {
        append_slug_character(&mut slug, &mut separator, character);
    }
    slug
}

fn append_slug_character(slug: &mut String, separator: &mut bool, character: char) {
    if !character.is_ascii_alphanumeric() {
        *separator = true;
        return;
    }
    if *separator && !slug.is_empty() {
        if slug.len() >= 47 {
            return;
        }
        slug.push('-');
    }
    *separator = false;
    if slug.len() < 48 {
        slug.push(character.to_ascii_lowercase());
    }
}

fn registration_fingerprint(
    request: &RegisterProjectRequest,
    canonical_directory: &str,
) -> Result<String, AppError> {
    let canonical = serde_jcs::to_vec(&RegistrationFingerprint {
        api_version: 1,
        method: "POST",
        route: "/api/v1/projects",
        body: request,
        canonical_directory,
    })
    .map_err(|error| AppError::internal(format!("cannot fingerprint registration: {error}")))?;
    let digest = Sha256::digest(canonical);
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut fingerprint, "{byte:02x}")
            .map_err(|_| AppError::internal("cannot render Project registration fingerprint"))?;
    }
    Ok(fingerprint)
}

fn validate_idempotency_key(key: &str) -> Result<(), AppError> {
    let valid = (8..=200).contains(&key.len())
        && key
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\');
    if !valid {
        return Err(AppError::invalid(
            "invalid_idempotency_key",
            "Idempotency-Key must be 8..200 visible ASCII characters without quote or backslash",
        ));
    }
    Ok(())
}

fn idempotency_record(
    transaction: &Transaction<'_>,
    key: &str,
) -> Result<Option<IdempotencyRecord>, AppError> {
    transaction
        .query_row(
            "SELECT fingerprint, state, response_json FROM idempotency_requests WHERE key=?1",
            [key],
            |row| {
                Ok(IdempotencyRecord {
                    fingerprint: row.get(0)?,
                    state: row.get(1)?,
                    response_json: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(database_error)
}

fn replay_registration(
    record: IdempotencyRecord,
    fingerprint: &str,
) -> Result<RegistrationOutcome, AppError> {
    if record.fingerprint != fingerprint {
        return Err(AppError::conflict(
            "idempotency_conflict",
            "Idempotency-Key was already used for a different request",
        ));
    }
    if record.state != "completed" {
        return Err(AppError::retryable_conflict(
            "idempotency_in_progress",
            "an identical request is still in progress",
        ));
    }
    let response = record
        .response_json
        .ok_or_else(|| AppError::internal("completed idempotency record has no result"))?;
    let project = serde_json::from_str(&response).map_err(|error| {
        AppError::internal(format!("stored Project result is invalid: {error}"))
    })?;
    Ok(RegistrationOutcome {
        project,
        replayed: true,
    })
}

fn reject_registered_directory(
    transaction: &Transaction<'_>,
    canonical_directory: &str,
) -> Result<(), AppError> {
    let state = transaction
        .query_row(
            "SELECT state FROM projects WHERE canonical_directory=?1",
            [canonical_directory],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(database_error)?;
    match state.as_deref() {
        Some("live") => Err(AppError::conflict(
            "already_exists",
            "a live Project already owns this canonical directory",
        )),
        Some("gone") => Err(AppError::gone(
            "project_gone",
            "this canonical directory has a terminal Project identity",
        )),
        Some(_) => Err(AppError::internal("Project has an invalid state")),
        None => Ok(()),
    }
}

fn allocate_project_id(transaction: &Transaction<'_>) -> Result<String, AppError> {
    for _ in 0..16 {
        let id = random_opaque_id()?;
        let exists = transaction
            .query_row("SELECT 1 FROM projects WHERE id=?1", [&id], |_| Ok(()))
            .optional()
            .map_err(database_error)?
            .is_some();
        if !exists {
            return Ok(id);
        }
    }
    Err(AppError::internal(
        "could not allocate a collision-free Project ID",
    ))
}

fn project_select(suffix: &str) -> String {
    format!(
        "SELECT id, record_version, canonical_directory, state, title, slug,
                created_at, updated_at, terminal_state, tombstoned_at, cause
         FROM projects {suffix}"
    )
}

fn project_from_row(row: &Row<'_>, canonical_origin: &str) -> rusqlite::Result<Project> {
    let id: String = row.get(0)?;
    let record_version = row.get(1)?;
    let canonical_directory = row.get(2)?;
    let state = match row.get::<_, String>(3)?.as_str() {
        "live" => ProjectState::Live,
        "gone" => ProjectState::Gone,
        value => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                format!("invalid Project state {value}").into(),
            ));
        }
    };
    let title = row.get(4)?;
    let slug: String = row.get(5)?;
    let key = format!("{slug}~{id}");
    Ok(Project {
        kind: "project".to_owned(),
        id: id.clone(),
        key: key.clone(),
        record_version,
        state,
        title,
        slug,
        canonical_directory,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        api_url: format!("{canonical_origin}api/v1/projects/{id}"),
        detail_url: format!("{canonical_origin}ui/projects/{key}/"),
        terminal_state: row.get(8)?,
        tombstoned_at: row.get(9)?,
        cause: row.get(10)?,
    })
}

fn observed_at() -> Result<String, AppError> {
    OffsetDateTime::now_utc()
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .map_err(|error| AppError::internal(format!("cannot format Project time: {error}")))
}

#[allow(clippy::needless_pass_by_value)]
fn database_error(error: rusqlite::Error) -> AppError {
    if matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ) {
        AppError::contention("Project catalogue is busy; retry the identical request")
    } else {
        AppError::internal(format!("Project catalogue failure: {error}"))
    }
}
