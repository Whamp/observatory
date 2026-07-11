use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use serde_json::json;

use crate::error::AppError;
use crate::storage_status::{self, StorageStatus};

pub const APPLICATION_ID: i64 = 0x4f42_5356;
pub const SCHEMA_VERSION: i64 = 1;
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
        let connection = bootstrap_connection(&path)?;
        if is_new {
            initialize(&connection)?;
        }
        verify_identity(&connection)?;
        reconcile(root, &connection)?;
        Ok(Self {
            root: root.to_path_buf(),
            path,
        })
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
                "SELECT count(*) = 8 FROM pragma_table_list WHERE name IN ('projects','artifacts','services','revisions','operation_intents','backup_leases','cleanup_runs','audit_events') AND strict = 1",
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
    let connection = bootstrap_connection(path)?;
    verify_connection_identity(&connection)?;
    Ok(connection)
}

fn bootstrap_connection(path: &Path) -> Result<Connection, AppError> {
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
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .map_err(database_error)?;
    Ok(connection)
}

fn initialize(connection: &Connection) -> Result<(), AppError> {
    connection
        .execute_batch(&format!(
            "BEGIN IMMEDIATE;
             PRAGMA application_id={APPLICATION_ID};
             PRAGMA user_version={SCHEMA_VERSION};
             CREATE TABLE projects (id TEXT PRIMARY KEY, record_version INTEGER NOT NULL CHECK(record_version > 0)) STRICT;
             CREATE TABLE artifacts (id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects(id), record_version INTEGER NOT NULL CHECK(record_version > 0)) STRICT;
             CREATE TABLE services (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), record_version INTEGER NOT NULL CHECK(record_version > 0)) STRICT;
             CREATE TABLE revisions (id TEXT PRIMARY KEY, artifact_id TEXT NOT NULL REFERENCES artifacts(id), state TEXT NOT NULL CHECK(state IN ('available','unavailable'))) STRICT;
             CREATE TABLE operation_intents (id TEXT PRIMARY KEY, kind TEXT NOT NULL, state TEXT NOT NULL, details_json TEXT NOT NULL) STRICT;
             CREATE TABLE backup_leases (id TEXT PRIMARY KEY, state TEXT NOT NULL) STRICT;
             CREATE TABLE cleanup_runs (id TEXT PRIMARY KEY, state TEXT NOT NULL) STRICT;
             CREATE TABLE audit_events (sequence INTEGER PRIMARY KEY, kind TEXT NOT NULL, details_json TEXT NOT NULL) STRICT;
             COMMIT;"
        ))
        .map_err(database_error)
}

fn verify_identity(connection: &Connection) -> Result<(), AppError> {
    verify_connection_identity(connection)?;
    let quick_check = pragma_string(connection, "quick_check")?;
    if quick_check != "ok" {
        return Err(AppError::internal("catalogue quick check failed"));
    }
    let required_tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('projects','artifacts','services','revisions','operation_intents','backup_leases','cleanup_runs','audit_events')",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if required_tables != 8 {
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
    let interrupted: u64 = connection
        .query_row(
            "SELECT count(*) FROM operation_intents WHERE state NOT IN ('completed','cancelled','failed_terminal')",
            [],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    if interrupted != 0 {
        return Err(AppError::internal(
            "startup reconciliation found an unsupported interrupted operation",
        ));
    }

    quarantine_all(root, connection, "staging", "unreferenced_staging")?;
    let referenced = connection
        .prepare("SELECT id FROM revisions")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<HashSet<_>, _>>()
        })
        .map_err(database_error)?;
    quarantine_unreferenced_revisions(root, connection, &referenced)?;
    Ok(())
}

fn quarantine_all(
    root: &Path,
    connection: &Connection,
    directory: &str,
    cause: &str,
) -> Result<(), AppError> {
    let entries = fs::read_dir(root.join(directory)).map_err(|error| {
        AppError::internal(format!(
            "startup reconciliation could not inspect {directory}: {error}"
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            AppError::internal(format!("startup reconciliation entry failed: {error}"))
        })?;
        quarantine_entry(root, connection, &entry.path(), cause)?;
    }
    Ok(())
}

fn quarantine_unreferenced_revisions(
    root: &Path,
    connection: &Connection,
    referenced: &HashSet<String>,
) -> Result<(), AppError> {
    let entries = fs::read_dir(root.join("revisions")).map_err(|error| {
        AppError::internal(format!(
            "startup reconciliation could not inspect revisions: {error}"
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            AppError::internal(format!("startup reconciliation entry failed: {error}"))
        })?;
        reconcile_revision_entry(root, connection, referenced, &entry)?;
    }
    Ok(())
}

fn reconcile_revision_entry(
    root: &Path,
    connection: &Connection,
    referenced: &HashSet<String>,
    entry: &fs::DirEntry,
) -> Result<(), AppError> {
    let name = entry.file_name();
    let name = name.to_str();
    let metadata = entry
        .file_type()
        .map_err(|error| AppError::internal(format!("cannot classify Revision entry: {error}")))?;
    let is_valid_reference = name.is_some_and(|id| referenced.contains(id))
        && metadata.is_dir()
        && !metadata.is_symlink();
    if is_valid_reference {
        return Ok(());
    }
    if let Some(id) = name.filter(|id| referenced.contains(*id)) {
        connection
            .execute("UPDATE revisions SET state='unavailable' WHERE id=?1", [id])
            .map_err(database_error)?;
    }
    quarantine_entry(
        root,
        connection,
        &entry.path(),
        "unreferenced_or_malformed_revision",
    )
}

fn quarantine_entry(
    root: &Path,
    connection: &Connection,
    source: &Path,
    cause: &str,
) -> Result<(), AppError> {
    let quarantine = root.join("quarantine");
    let mut ordinal = 1_u64;
    let destination = loop {
        let candidate = quarantine.join(format!("startup-{ordinal:016x}"));
        if !candidate.exists() {
            break candidate;
        }
        ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| AppError::internal("cannot allocate quarantine path"))?;
    };
    connection
        .execute(
            "INSERT INTO audit_events(kind, details_json) VALUES ('startup_quarantine_planned', ?1)",
            [json!({ "cause": cause }).to_string()],
        )
        .map_err(database_error)?;
    fs::rename(source, &destination).map_err(|error| {
        AppError::internal(format!("cannot quarantine startup evidence: {error}"))
    })?;
    connection
        .execute(
            "INSERT INTO audit_events(kind, details_json) VALUES ('startup_quarantine_completed', ?1)",
            [json!({ "cause": cause }).to_string()],
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
