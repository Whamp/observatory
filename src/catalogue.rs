use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use serde_json::json;

use crate::crypto::random_bytes;
use crate::error::AppError;
use crate::storage_boundary::StorageBoundary;
use crate::storage_status::{self, StorageStatus};

pub const APPLICATION_ID: i64 = 0x4f42_5356;
pub const SCHEMA_VERSION: i64 = 5;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct Catalogue {
    root: PathBuf,
    path: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueCounts {
    pub projects: u64,
    pub artifacts: u64,
    pub services: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CataloguePolicy {
    pub application_id: i64,
    pub user_version: i64,
    pub foreign_keys: bool,
    pub journal_mode: String,
    pub synchronous: String,
    pub busy_timeout_ms: u64,
    pub strict_tables: bool,
}

impl Catalogue {
    pub fn open_data_root(root: &Path) -> Result<Self, AppError> {
        create_private_layout(root)?;
        let path = root.join("catalogue.sqlite");
        let is_new = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(AppError::internal(
                        "catalogue path must be one regular non-symlink file",
                    ));
                }
                metadata.len() == 0
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .map_err(|error| {
                        AppError::internal(format!("cannot create catalogue: {error}"))
                    })?;
                true
            }
            Err(error) => {
                return Err(AppError::internal(format!(
                    "cannot inspect catalogue path: {error}"
                )));
            }
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|error| AppError::internal(format!("cannot protect catalogue: {error}")))?;
        let connection = raw_connection(&path)?;
        if is_new {
            configure_journal(&connection)?;
            initialize(&connection)?;
        } else {
            migrate(root, &connection)?;
            configure_journal(&connection)?;
        }
        verify_identity(&connection)?;
        reconcile(root, &connection)?;
        Ok(Self {
            root: root.to_path_buf(),
            path,
        })
    }

    pub(crate) fn connection(&self) -> Result<Connection, AppError> {
        configured_connection(&self.path)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub fn counts(&self) -> Result<CatalogueCounts, AppError> {
        let connection = configured_connection(&self.path)?;
        Ok(CatalogueCounts {
            projects: count(&connection, "projects")?,
            artifacts: count(&connection, "artifacts")?,
            services: count(&connection, "services")?,
        })
    }

    pub fn policy(&self) -> Result<CataloguePolicy, AppError> {
        let connection = configured_connection(&self.path)?;
        let application_id = pragma_i64(&connection, "application_id")?;
        let user_version = pragma_i64(&connection, "user_version")?;
        let foreign_keys = pragma_i64(&connection, "foreign_keys")? == 1;
        let journal_mode = pragma_string(&connection, "journal_mode")?;
        let synchronous_value = pragma_i64(&connection, "synchronous")?;
        let strict_tables = connection
            .query_row(
                "SELECT count(*) = 10 FROM pragma_table_list WHERE name IN ('projects','artifacts','services','revisions','operation_intents','backup_leases','cleanup_runs','audit_events','idempotency_requests','system_state') AND strict = 1",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(database_error)?;
        Ok(CataloguePolicy {
            application_id,
            user_version,
            foreign_keys,
            journal_mode,
            synchronous: if synchronous_value == 2 {
                "FULL"
            } else {
                "UNKNOWN"
            }
            .into(),
            busy_timeout_ms: 5_000,
            strict_tables,
        })
    }

    pub fn status(&self) -> Result<StorageStatus, AppError> {
        let connection = configured_connection(&self.path)?;
        storage_status::inspect(&self.root, &self.path, connection)
    }

    pub fn checkpoint(&self) -> Result<(), AppError> {
        let connection = configured_connection(&self.path)?;
        connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(database_error)
    }
}

fn create_private_layout(root: &Path) -> Result<(), AppError> {
    create_private_directory(root)?;
    for child in [
        "staging",
        "revisions",
        "quarantine",
        "backups",
        "candidates",
    ] {
        create_private_directory(&root.join(child))?;
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), AppError> {
    fs::create_dir_all(path).map_err(|error| {
        AppError::internal(format!("cannot create private storage layout: {error}"))
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        AppError::internal(format!("cannot inspect private storage layout: {error}"))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(AppError::internal(
            "private storage path must be a non-symlink directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        AppError::internal(format!("cannot protect private storage layout: {error}"))
    })
}

fn configured_connection(path: &Path) -> Result<Connection, AppError> {
    let connection = raw_connection(path)?;
    verify_connection_identity(&connection)?;
    configure_journal(&connection)?;
    Ok(connection)
}

fn raw_connection(path: &Path) -> Result<Connection, AppError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(database_error)?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(database_error)?;
    connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
        .map_err(database_error)?;
    Ok(connection)
}

fn configure_journal(connection: &Connection) -> Result<(), AppError> {
    let journal_mode = pragma_string(connection, "journal_mode=WAL")?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(AppError::internal(
            "catalogue cannot enter WAL journal mode",
        ));
    }
    Ok(())
}

fn initialize(connection: &Connection) -> Result<(), AppError> {
    connection
        .execute_batch(&format!(
            "BEGIN IMMEDIATE;
             PRAGMA application_id={APPLICATION_ID};
             PRAGMA user_version={SCHEMA_VERSION};
             CREATE TABLE projects (
               id TEXT PRIMARY KEY,
               record_version INTEGER NOT NULL CHECK(record_version > 0),
               canonical_directory TEXT NOT NULL,
               state TEXT NOT NULL CHECK(state IN ('live','gone')),
               title TEXT NOT NULL,
               slug TEXT NOT NULL,
               title_fold TEXT NOT NULL,
               search_text TEXT NOT NULL,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               terminal_state TEXT,
               tombstoned_at TEXT,
               cause TEXT
             ) STRICT;
             CREATE UNIQUE INDEX projects_canonical_directory ON projects(canonical_directory);
             CREATE TABLE artifacts (
               id TEXT PRIMARY KEY,
               project_id TEXT NOT NULL REFERENCES projects(id),
               record_version INTEGER NOT NULL CHECK(record_version > 0),
               state TEXT NOT NULL CHECK(state IN ('live','recoverable','gone')),
               title TEXT NOT NULL,
               description TEXT NOT NULL,
               slug TEXT NOT NULL,
               title_fold TEXT NOT NULL,
               search_text TEXT NOT NULL,
               current_revision_id TEXT REFERENCES revisions(id),
               retention_mode TEXT NOT NULL CHECK(retention_mode IN ('default','ttl','pinned')),
               ttl_ms INTEGER CHECK(ttl_ms IS NULL OR ttl_ms > 0),
               expires_at TEXT,
               pin_reason TEXT,
               recovery_until TEXT,
               files INTEGER NOT NULL CHECK(files > 0),
               logical_bytes INTEGER NOT NULL CHECK(logical_bytes >= 0),
               revision_count INTEGER NOT NULL CHECK(revision_count > 0),
               published_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               terminal_state TEXT,
               tombstoned_at TEXT,
               cause TEXT
             ) STRICT;
             CREATE TABLE services (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), record_version INTEGER NOT NULL CHECK(record_version > 0), state TEXT NOT NULL CHECK(state IN ('live','gone'))) STRICT;
             CREATE TABLE revisions (
               id TEXT PRIMARY KEY,
               artifact_id TEXT NOT NULL REFERENCES artifacts(id),
               state TEXT NOT NULL CHECK(state IN ('current','superseded','unavailable','gone')),
               entry_path TEXT NOT NULL,
               entry_media_type TEXT NOT NULL,
               files INTEGER NOT NULL CHECK(files > 0),
               logical_bytes INTEGER NOT NULL CHECK(logical_bytes >= 0),
               manifest_digest TEXT NOT NULL,
               published_at TEXT NOT NULL,
               superseded_at TEXT
             ) STRICT;
             CREATE INDEX artifacts_project_state ON artifacts(project_id, state);
             CREATE INDEX revisions_artifact_published ON revisions(artifact_id, published_at DESC);
             CREATE INDEX revisions_artifact_superseded ON revisions(artifact_id, superseded_at DESC);
             CREATE TABLE operation_intents (id TEXT PRIMARY KEY, kind TEXT NOT NULL, state TEXT NOT NULL, details_json TEXT NOT NULL, project_id TEXT REFERENCES projects(id)) STRICT;
             CREATE INDEX operation_intents_kind_state ON operation_intents(kind, state);
             CREATE TABLE backup_leases (id TEXT PRIMARY KEY, state TEXT NOT NULL) STRICT;
             CREATE TABLE cleanup_runs (id TEXT PRIMARY KEY, state TEXT NOT NULL) STRICT;
             CREATE TABLE audit_events (
               sequence INTEGER PRIMARY KEY,
               kind TEXT NOT NULL,
               details_json TEXT NOT NULL,
               at TEXT NOT NULL,
               actor TEXT NOT NULL,
               cause TEXT NOT NULL,
               resource_type TEXT NOT NULL,
               resource_id TEXT NOT NULL
             ) STRICT;
             CREATE TABLE idempotency_requests (
               key TEXT PRIMARY KEY,
               fingerprint TEXT NOT NULL,
               state TEXT NOT NULL CHECK(state IN ('in_progress','completed','failed_terminal')),
               status_code INTEGER,
               response_json TEXT,
               etag TEXT,
               completed_at TEXT
             ) STRICT;
             CREATE TABLE system_state (key TEXT PRIMARY KEY, value BLOB NOT NULL) STRICT;"
        ))
        .map_err(database_error)?;
    let cursor_secret = random_bytes::<32>()?;
    connection
        .execute(
            "INSERT INTO system_state(key, value) VALUES ('cursor_secret', ?1)",
            [cursor_secret.as_slice()],
        )
        .map_err(database_error)?;
    connection.execute_batch("COMMIT;").map_err(database_error)
}

fn migrate(root: &Path, connection: &Connection) -> Result<(), AppError> {
    verify_pre_migration_integrity(connection)?;
    let application_id = pragma_i64(connection, "application_id")?;
    if application_id != APPLICATION_ID {
        return Err(AppError::internal(
            "catalogue has the wrong application identity",
        ));
    }
    match pragma_i64(connection, "user_version")? {
        SCHEMA_VERSION => Ok(()),
        1 => {
            migrate_v1_to_v2(root, connection)?;
            migrate_v2_to_v3(root, connection)?;
            migrate_v3_to_v4(root, connection)?;
            migrate_v4_to_v5(root, connection)
        }
        2 => {
            migrate_v2_to_v3(root, connection)?;
            migrate_v3_to_v4(root, connection)?;
            migrate_v4_to_v5(root, connection)
        }
        3 => {
            migrate_v3_to_v4(root, connection)?;
            migrate_v4_to_v5(root, connection)
        }
        4 => migrate_v4_to_v5(root, connection),
        _ => Err(AppError::internal("catalogue schema is unsupported")),
    }
}

fn verify_pre_migration_integrity(connection: &Connection) -> Result<(), AppError> {
    if pragma_string(connection, "quick_check")? != "ok" {
        return Err(AppError::internal(
            "catalogue quick check failed before migration",
        ));
    }
    let foreign_key_failures: i64 = connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(database_error)?;
    if foreign_key_failures != 0 {
        return Err(AppError::internal(
            "catalogue foreign key check failed before migration",
        ));
    }
    Ok(())
}

fn migrate_v1_to_v2(root: &Path, connection: &Connection) -> Result<(), AppError> {
    if count(connection, "projects")? != 0 || count(connection, "audit_events")? != 0 {
        return Err(AppError::internal(
            "schema v1 contains state that its public contract could not create",
        ));
    }
    backup_before_migration(root, connection, 1)?;
    let cursor_secret = random_bytes::<32>()?;
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE projects ADD COLUMN canonical_directory TEXT NOT NULL DEFAULT '';
             ALTER TABLE projects ADD COLUMN state TEXT NOT NULL DEFAULT 'live' CHECK(state IN ('live','gone'));
             ALTER TABLE projects ADD COLUMN title TEXT NOT NULL DEFAULT '';
             ALTER TABLE projects ADD COLUMN slug TEXT NOT NULL DEFAULT '';
             ALTER TABLE projects ADD COLUMN title_fold TEXT NOT NULL DEFAULT '';
             ALTER TABLE projects ADD COLUMN search_text TEXT NOT NULL DEFAULT '';
             ALTER TABLE projects ADD COLUMN created_at TEXT NOT NULL DEFAULT '';
             ALTER TABLE projects ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
             ALTER TABLE projects ADD COLUMN terminal_state TEXT;
             ALTER TABLE projects ADD COLUMN tombstoned_at TEXT;
             ALTER TABLE projects ADD COLUMN cause TEXT;
             CREATE UNIQUE INDEX projects_canonical_directory ON projects(canonical_directory);
             DROP TABLE audit_events;
             CREATE TABLE audit_events (
               sequence INTEGER PRIMARY KEY,
               kind TEXT NOT NULL,
               details_json TEXT NOT NULL,
               at TEXT NOT NULL,
               actor TEXT NOT NULL,
               cause TEXT NOT NULL,
               resource_type TEXT NOT NULL,
               resource_id TEXT NOT NULL
             ) STRICT;
             CREATE TABLE idempotency_requests (
               key TEXT PRIMARY KEY,
               fingerprint TEXT NOT NULL,
               state TEXT NOT NULL CHECK(state IN ('in_progress','completed','failed_terminal')),
               status_code INTEGER,
               response_json TEXT,
               etag TEXT,
               completed_at TEXT
             ) STRICT;
             CREATE TABLE system_state (key TEXT PRIMARY KEY, value BLOB NOT NULL) STRICT;
             PRAGMA user_version=2;",
        )
        .map_err(database_error)?;
    connection
        .execute(
            "INSERT INTO system_state(key, value) VALUES ('cursor_secret', ?1)",
            [cursor_secret.as_slice()],
        )
        .map_err(database_error)?;
    connection.execute_batch("COMMIT;").map_err(database_error)
}

fn migrate_v2_to_v3(root: &Path, connection: &Connection) -> Result<(), AppError> {
    backup_before_migration(root, connection, 2)?;
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE services ADD COLUMN state TEXT NOT NULL DEFAULT 'live' CHECK(state IN ('live','gone'));
             ALTER TABLE operation_intents ADD COLUMN project_id TEXT REFERENCES projects(id);
             CREATE INDEX services_project_state ON services(project_id, state);
             CREATE INDEX operation_intents_project_state ON operation_intents(project_id, state);
             PRAGMA user_version=3;
             COMMIT;",
        )
        .map_err(database_error)
}

fn migrate_v3_to_v4(root: &Path, connection: &Connection) -> Result<(), AppError> {
    if count(connection, "artifacts")? != 0 || count(connection, "revisions")? != 0 {
        return Err(AppError::internal(
            "schema v3 contains Artifact state that its public contract could not create",
        ));
    }
    backup_before_migration(root, connection, 3)?;
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE idempotency_requests RENAME TO idempotency_requests_v3;
             CREATE TABLE idempotency_requests (
               key TEXT PRIMARY KEY,
               fingerprint TEXT NOT NULL,
               state TEXT NOT NULL CHECK(state IN ('in_progress','completed','failed_terminal')),
               status_code INTEGER,
               response_json TEXT,
               etag TEXT,
               completed_at TEXT
             ) STRICT;
             INSERT INTO idempotency_requests(
               key,fingerprint,state,status_code,response_json,etag,completed_at
             )
             SELECT key,fingerprint,state,status_code,response_json,etag,completed_at
             FROM idempotency_requests_v3;
             DROP TABLE idempotency_requests_v3;
             DROP TABLE revisions;
             DROP TABLE artifacts;
             CREATE TABLE artifacts (
               id TEXT PRIMARY KEY,
               project_id TEXT NOT NULL REFERENCES projects(id),
               record_version INTEGER NOT NULL CHECK(record_version > 0),
               state TEXT NOT NULL CHECK(state IN ('live','recoverable','gone')),
               title TEXT NOT NULL,
               description TEXT NOT NULL,
               slug TEXT NOT NULL,
               title_fold TEXT NOT NULL,
               search_text TEXT NOT NULL,
               current_revision_id TEXT REFERENCES revisions(id),
               retention_mode TEXT NOT NULL CHECK(retention_mode IN ('default','ttl','pinned')),
               ttl_ms INTEGER CHECK(ttl_ms IS NULL OR ttl_ms > 0),
               expires_at TEXT,
               pin_reason TEXT,
               recovery_until TEXT,
               files INTEGER NOT NULL CHECK(files > 0),
               logical_bytes INTEGER NOT NULL CHECK(logical_bytes >= 0),
               revision_count INTEGER NOT NULL CHECK(revision_count > 0),
               published_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               terminal_state TEXT,
               tombstoned_at TEXT,
               cause TEXT
             ) STRICT;
             CREATE TABLE revisions (
               id TEXT PRIMARY KEY,
               artifact_id TEXT NOT NULL REFERENCES artifacts(id),
               state TEXT NOT NULL CHECK(state IN ('current','superseded','unavailable','gone')),
               entry_path TEXT NOT NULL,
               entry_media_type TEXT NOT NULL,
               files INTEGER NOT NULL CHECK(files > 0),
               logical_bytes INTEGER NOT NULL CHECK(logical_bytes >= 0),
               manifest_digest TEXT NOT NULL,
               published_at TEXT NOT NULL
             ) STRICT;
             CREATE INDEX artifacts_project_state ON artifacts(project_id, state);
             CREATE INDEX revisions_artifact_published ON revisions(artifact_id, published_at DESC);
             CREATE INDEX operation_intents_kind_state ON operation_intents(kind, state);
             PRAGMA user_version=4;
             COMMIT;",
        )
        .map_err(database_error)
}

fn migrate_v4_to_v5(root: &Path, connection: &Connection) -> Result<(), AppError> {
    backup_before_migration(root, connection, 4)?;
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE revisions ADD COLUMN superseded_at TEXT;
             CREATE INDEX revisions_artifact_superseded
               ON revisions(artifact_id, superseded_at DESC);
             PRAGMA user_version=5;
             COMMIT;",
        )
        .map_err(database_error)
}

fn backup_before_migration(
    root: &Path,
    connection: &Connection,
    schema_version: i64,
) -> Result<(), AppError> {
    let backups = root.join("backups");
    let final_path = backups.join(format!("schema-v{schema_version}-pre-migration.sqlite"));
    if valid_existing_migration_backup(&final_path, schema_version)? {
        return Ok(());
    }
    let staging = backups.join(format!(
        ".schema-v{schema_version}-pre-migration.sqlite.staging"
    ));
    reset_migration_backup_staging(&staging)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
        .map_err(|error| AppError::internal(format!("cannot stage migration backup: {error}")))?;
    let mut destination = Connection::open_with_flags(
        &staging,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(database_error)?;
    {
        let backup = Backup::new(connection, &mut destination).map_err(database_error)?;
        backup
            .run_to_completion(64, Duration::from_millis(10), None)
            .map_err(database_error)?;
    }
    drop(destination);
    OpenOptions::new()
        .read(true)
        .open(&staging)
        .and_then(|file| file.sync_all())
        .map_err(|error| AppError::internal(format!("cannot sync migration backup: {error}")))?;
    fs::rename(&staging, &final_path).map_err(|error| {
        AppError::internal(format!("cannot finalize migration backup: {error}"))
    })?;
    fs::File::open(&backups)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            AppError::internal(format!("cannot sync migration backup directory: {error}"))
        })
}

fn valid_existing_migration_backup(path: &Path, schema_version: i64) -> Result<bool, AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(AppError::internal(format!(
                "cannot inspect migration backup: {error}"
            )));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::internal(
            "migration backup path is not a regular file",
        ));
    }
    let backup = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(database_error)?;
    if pragma_i64(&backup, "application_id")? != APPLICATION_ID
        || pragma_i64(&backup, "user_version")? != schema_version
        || pragma_string(&backup, "quick_check")? != "ok"
    {
        return Err(AppError::internal(
            "existing migration backup does not match catalogue authority",
        ));
    }
    Ok(true)
}

fn reset_migration_backup_staging(path: &Path) -> Result<(), AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(AppError::internal(format!(
                "cannot inspect migration backup staging: {error}"
            )));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::internal(
            "migration backup staging path is not a regular file",
        ));
    }
    fs::remove_file(path).map_err(|error| {
        AppError::internal(format!("cannot reset migration backup staging: {error}"))
    })
}

fn verify_identity(connection: &Connection) -> Result<(), AppError> {
    verify_connection_identity(connection)?;
    let quick_check = pragma_string(connection, "quick_check")?;
    if quick_check != "ok" {
        return Err(AppError::internal("catalogue quick check failed"));
    }
    let required_tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('projects','artifacts','services','revisions','operation_intents','backup_leases','cleanup_runs','audit_events','idempotency_requests','system_state')",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if required_tables != 10 {
        return Err(AppError::internal("catalogue schema is incomplete"));
    }
    Ok(())
}

fn verify_connection_identity(connection: &Connection) -> Result<(), AppError> {
    let application_id = pragma_i64(connection, "application_id")?;
    let user_version = pragma_i64(connection, "user_version")?;
    if application_id != APPLICATION_ID {
        return Err(AppError::internal(
            "catalogue has the wrong application identity",
        ));
    }
    if user_version != SCHEMA_VERSION {
        return Err(AppError::internal("catalogue schema is unsupported"));
    }
    Ok(())
}

fn reconcile(root: &Path, connection: &Connection) -> Result<(), AppError> {
    let foreign_key_failures: i64 = connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(database_error)?;
    if foreign_key_failures != 0 {
        return Err(AppError::internal("catalogue foreign key check failed"));
    }
    let mut protected_staging = HashSet::new();
    let mut protected_revisions = HashSet::new();
    let interrupted = connection
        .prepare(
            "SELECT kind,details_json FROM operation_intents
             WHERE state NOT IN ('completed','cancelled','failed_terminal')",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(database_error)?;
    for (kind, details) in interrupted {
        if kind != "artifact_publish" {
            return Err(AppError::internal(
                "startup reconciliation found an unsupported interrupted operation",
            ));
        }
        let details: serde_json::Value = serde_json::from_str(&details)
            .map_err(|error| AppError::internal(format!("Publish intent is invalid: {error}")))?;
        let operation_id = details
            .get("operationId")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AppError::internal("Publish intent has no operation ID"))?;
        let revision_id = details
            .pointer("/revision/id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AppError::internal("Publish intent has no Revision ID"))?;
        protected_staging.insert(operation_id.to_owned());
        protected_revisions.insert(revision_id.to_owned());
    }

    let storage = StorageBoundary::open(root)?;
    quarantine_all_except(
        &storage,
        connection,
        "staging",
        "unreferenced_staging",
        &protected_staging,
    )?;
    let mut referenced = connection
        .prepare("SELECT id FROM revisions")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<HashSet<_>, _>>()
        })
        .map_err(database_error)?;
    referenced.extend(protected_revisions);
    quarantine_unreferenced_revisions(&storage, connection, &referenced)?;
    Ok(())
}

fn quarantine_all_except(
    storage: &StorageBoundary,
    connection: &Connection,
    directory: &str,
    cause: &str,
    protected: &HashSet<String>,
) -> Result<(), AppError> {
    for name in storage.entry_names(directory)? {
        if !name.to_str().is_some_and(|name| protected.contains(name)) {
            quarantine_entry(storage, connection, directory, &name, cause)?;
        }
    }
    Ok(())
}

fn quarantine_unreferenced_revisions(
    storage: &StorageBoundary,
    connection: &Connection,
    referenced: &HashSet<String>,
) -> Result<(), AppError> {
    for name in storage.entry_names("revisions")? {
        reconcile_revision_entry(storage, connection, referenced, &name)?;
    }
    Ok(())
}

fn reconcile_revision_entry(
    storage: &StorageBoundary,
    connection: &Connection,
    referenced: &HashSet<String>,
    name: &std::ffi::OsStr,
) -> Result<(), AppError> {
    let text_name = name.to_str();
    let is_valid_reference = text_name.is_some_and(|id| referenced.contains(id))
        && storage.is_directory("revisions", name)?;
    if is_valid_reference {
        return Ok(());
    }
    if let Some(id) = text_name.filter(|id| referenced.contains(*id)) {
        connection
            .execute("UPDATE revisions SET state='unavailable' WHERE id=?1", [id])
            .map_err(database_error)?;
    }
    quarantine_entry(
        storage,
        connection,
        "revisions",
        name,
        "unreferenced_or_malformed_revision",
    )
}

fn quarantine_entry(
    storage: &StorageBoundary,
    connection: &Connection,
    source_directory: &str,
    source_name: &std::ffi::OsStr,
    cause: &str,
) -> Result<(), AppError> {
    connection
        .execute(
            "INSERT INTO audit_events(
                 kind, details_json, at, actor, cause, resource_type, resource_id
             ) VALUES (
                 'startup_quarantine_planned', ?1,
                 strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'system', ?2, 'storage', 'startup'
             )",
            params![json!({ "cause": cause }).to_string(), cause],
        )
        .map_err(database_error)?;
    storage.quarantine_startup(source_directory, source_name)?;
    connection
        .execute(
            "INSERT INTO audit_events(
                 kind, details_json, at, actor, cause, resource_type, resource_id
             ) VALUES (
                 'startup_quarantine_completed', ?1,
                 strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'system', ?2, 'storage', 'startup'
             )",
            params![json!({ "cause": cause }).to_string(), cause],
        )
        .map_err(database_error)?;
    Ok(())
}

fn count(connection: &Connection, table: &str) -> Result<u64, AppError> {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), params![], |row| {
            row.get(0)
        })
        .map_err(database_error)
}

fn pragma_i64(connection: &Connection, name: &str) -> Result<i64, AppError> {
    connection
        .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .map_err(database_error)
}

fn pragma_string(connection: &Connection, name: &str) -> Result<String, AppError> {
    connection
        .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .map_err(database_error)
}

#[allow(clippy::needless_pass_by_value)]
fn database_error(error: rusqlite::Error) -> AppError {
    AppError::internal(format!("catalogue failure: {error}"))
}
