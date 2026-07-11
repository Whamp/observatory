use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::Connection;
use rustix::fs::{statfs, statvfs};
use serde::Serialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::format_description;

use crate::catalogue::{APPLICATION_ID, SCHEMA_VERSION};
use crate::error::AppError;
const RESERVED_BYTES: u64 = 1_073_741_824;
const BTRFS_MAGIC: i64 = 0x9123_683e;
const EXT_MAGIC: i64 = 0xef53;
const XFS_MAGIC: i64 = 0x5846_5342;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatus {
    health: Health,
    partial: bool,
    checks: Vec<StorageCheck>,
}

impl StorageStatus {
    pub const fn health_name(&self) -> &'static str {
        match self.health {
            Health::Healthy => "healthy",
            Health::Degraded => "degraded",
            Health::Unhealthy => "unhealthy",
        }
    }

    pub const fn ready(&self) -> bool {
        !matches!(self.health, Health::Unhealthy)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Health {
    Healthy,
    Degraded,
    Unhealthy,
}

impl Health {
    fn combine(self, other: Self) -> Self {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }

    const fn severity(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Degraded => 1,
            Self::Unhealthy => 2,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StorageCheck {
    id: &'static str,
    status: CheckStatus,
    state: &'static str,
    category: &'static str,
    message: String,
    retryable: bool,
    scope: Value,
    observed_at: String,
    duration_ms: u64,
    details: Value,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

struct CheckSpec {
    id: &'static str,
    status: CheckStatus,
    state: &'static str,
    category: &'static str,
    message: &'static str,
    details: Value,
}

impl CheckSpec {
    fn pass(
        id: &'static str,
        state: &'static str,
        category: &'static str,
        message: &'static str,
        details: Value,
    ) -> Self {
        Self {
            id,
            status: CheckStatus::Pass,
            state,
            category,
            message,
            details,
        }
    }

    fn warn(
        id: &'static str,
        state: &'static str,
        category: &'static str,
        message: &'static str,
        details: Value,
    ) -> Self {
        Self {
            id,
            status: CheckStatus::Warn,
            state,
            category,
            message,
            details,
        }
    }

    fn fail(
        id: &'static str,
        state: &'static str,
        category: &'static str,
        message: &'static str,
        details: Value,
    ) -> Self {
        Self {
            id,
            status: CheckStatus::Fail,
            state,
            category,
            message,
            details,
        }
    }
}

struct Inspection<'a> {
    root: &'a Path,
    catalogue: &'a Path,
    connection: Connection,
    application_id: i64,
    user_version: i64,
    observed_at: String,
    started: Instant,
    health: Health,
    checks: Vec<StorageCheck>,
}

pub fn inspect(
    root: &Path,
    catalogue: &Path,
    connection: Connection,
) -> Result<StorageStatus, AppError> {
    let application_id = pragma_i64(&connection, "application_id")?;
    let user_version = pragma_i64(&connection, "user_version")?;
    if application_id != APPLICATION_ID || user_version != SCHEMA_VERSION {
        return Err(AppError::internal(
            "catalogue identity changed before status inspection",
        ));
    }
    Inspection {
        root,
        catalogue,
        connection,
        application_id,
        user_version,
        observed_at: observed_at()?,
        started: Instant::now(),
        health: Health::Healthy,
        checks: Vec::with_capacity(12),
    }
    .run()
}

impl Inspection<'_> {
    fn run(mut self) -> Result<StorageStatus, AppError> {
        self.inspect_sqlite()?;
        self.inspect_operations()?;
        self.inspect_storage()?;
        self.inspect_host()?;
        Ok(StorageStatus {
            health: self.health,
            partial: false,
            checks: self.checks,
        })
    }

    fn inspect_sqlite(&mut self) -> Result<(), AppError> {
        self.record(CheckSpec::pass(
            "sqlite.open",
            "open",
            "catalogue",
            "catalogue opened through the configured connection policy",
            json!({}),
        ));
        self.record(CheckSpec::pass(
            "sqlite.application",
            "matches",
            "catalogue",
            "catalogue application identity matches Observatory",
            json!({ "applicationId": self.application_id }),
        ));
        self.record(CheckSpec::pass(
            "sqlite.schema",
            "supported",
            "schema",
            "catalogue schema is supported",
            json!({ "userVersion": self.user_version }),
        ));
        let wal_bytes = metadata_len(&wal_path(self.catalogue))?;
        self.record(CheckSpec::pass(
            "sqlite.wal",
            if wal_bytes == 0 {
                "clean"
            } else {
                "frames_pending"
            },
            "wal",
            if wal_bytes == 0 {
                "write-ahead log is empty"
            } else {
                "write-ahead log has pending frames"
            },
            json!({ "bytes": wal_bytes }),
        ));
        Ok(())
    }

    fn inspect_operations(&mut self) -> Result<(), AppError> {
        let awaiting_retry = query_count(
            &self.connection,
            "SELECT count(*) FROM operation_intents WHERE state='awaiting_retry'",
        )?;
        let interrupted = query_count(
            &self.connection,
            "SELECT count(*) FROM operation_intents
             WHERE state NOT IN ('completed','cancelled','failed_terminal','awaiting_retry')",
        )?;
        self.record(if interrupted == 0 {
            CheckSpec::pass(
                "storage.intents",
                if awaiting_retry == 0 {
                    "terminal"
                } else {
                    "awaiting_retry"
                },
                "operation_interrupted",
                if awaiting_retry == 0 {
                    "operation intents were classified"
                } else {
                    "accepted Publish intents await identical caller retry"
                },
                json!({
                    "nonterminal": awaiting_retry,
                    "awaitingRetry": awaiting_retry
                }),
            )
        } else {
            CheckSpec::fail(
                "storage.intents",
                "interrupted_ambiguous",
                "operation_interrupted",
                "operation intents include unsupported interrupted work",
                json!({
                    "nonterminal": interrupted + awaiting_retry,
                    "awaitingRetry": awaiting_retry,
                    "ambiguous": interrupted
                }),
            )
        });

        let missing = missing_available_revisions(&self.connection, self.root)?;
        self.record(if missing == 0 {
            CheckSpec::pass(
                "revision.path",
                "present",
                "missing_bytes",
                "available Revision paths were checked",
                json!({ "missing": missing }),
            )
        } else {
            CheckSpec::fail(
                "revision.path",
                "missing",
                "missing_bytes",
                "an available Revision path is missing or unsafe",
                json!({ "missing": missing }),
            )
        });
        Ok(())
    }

    fn inspect_storage(&mut self) -> Result<(), AppError> {
        let staging = directory_count(&self.root.join("staging"))?;
        self.record(if staging == 0 {
            CheckSpec::pass(
                "storage.staging",
                "clear",
                "operation_interrupted",
                "staging entries were counted",
                json!({ "entries": staging }),
            )
        } else {
            CheckSpec::fail(
                "storage.staging",
                "abandoned",
                "operation_interrupted",
                "staging contains unclassified entries",
                json!({ "entries": staging }),
            )
        });

        let quarantined = directory_count(&self.root.join("quarantine"))?;
        self.record(if quarantined == 0 {
            CheckSpec::pass(
                "storage.quarantine",
                "clear",
                "quarantine",
                "quarantine evidence was counted",
                json!({ "entries": quarantined }),
            )
        } else {
            CheckSpec::warn(
                "storage.quarantine",
                "retained",
                "quarantine",
                "quarantine retains recovery evidence",
                json!({ "entries": quarantined }),
            )
        });

        let leases = query_count(&self.connection, "SELECT count(*) FROM backup_leases")?;
        self.record(CheckSpec::pass(
            "storage.backup_leases",
            if leases == 0 {
                "expired_releasable"
            } else {
                "active"
            },
            "lease",
            "backup leases were counted",
            json!({ "active": leases }),
        ));

        let cleanup = query_count(
            &self.connection,
            "SELECT count(*) FROM cleanup_runs WHERE state NOT IN ('completed','failed_terminal')",
        )?;
        self.record(if cleanup == 0 {
            CheckSpec::pass(
                "storage.cleanup",
                "ok",
                "cleanup",
                "cleanup runs were classified",
                json!({ "nonterminal": cleanup }),
            )
        } else {
            CheckSpec::fail(
                "storage.cleanup",
                "interrupted",
                "cleanup",
                "cleanup has interrupted work",
                json!({ "nonterminal": cleanup }),
            )
        });
        Ok(())
    }

    fn inspect_host(&mut self) -> Result<(), AppError> {
        let filesystem = statfs(self.root).map_err(|error| {
            AppError::internal(format!("cannot inspect storage filesystem: {error}"))
        })?;
        let filesystem_type = filesystem.f_type as i64;
        let supported = matches!(filesystem_type, BTRFS_MAGIC | EXT_MAGIC | XFS_MAGIC);
        self.record(if supported {
            CheckSpec::pass(
                "storage.filesystem",
                "supported",
                "filesystem",
                "storage filesystem type is supported",
                json!({ "type": format!("0x{filesystem_type:x}") }),
            )
        } else {
            CheckSpec::fail(
                "storage.filesystem",
                "remote_or_unsupported",
                "filesystem",
                "storage filesystem type is unsupported",
                json!({ "type": format!("0x{filesystem_type:x}") }),
            )
        });

        let capacity = statvfs(self.root).map_err(|error| {
            AppError::internal(format!("cannot inspect storage capacity: {error}"))
        })?;
        let total = capacity.f_blocks.saturating_mul(capacity.f_frsize);
        let available = capacity.f_bavail.saturating_mul(capacity.f_frsize);
        let reserve = RESERVED_BYTES.max(total / 20);
        self.record(if available >= reserve {
            CheckSpec::pass(
                "storage.capacity",
                "within_reserve",
                "capacity",
                "free-space reserve is available",
                json!({ "availableBytes": available, "reserveBytes": reserve }),
            )
        } else {
            CheckSpec::fail(
                "storage.capacity",
                "reserve_breached",
                "capacity",
                "free-space reserve is breached",
                json!({ "availableBytes": available, "reserveBytes": reserve }),
            )
        });
        Ok(())
    }

    fn record(&mut self, spec: CheckSpec) {
        self.health = self.health.combine(match spec.status {
            CheckStatus::Pass => Health::Healthy,
            CheckStatus::Warn => Health::Degraded,
            CheckStatus::Fail => Health::Unhealthy,
        });
        self.checks.push(StorageCheck {
            id: spec.id,
            status: spec.status,
            state: spec.state,
            category: spec.category,
            message: spec.message.to_owned(),
            retryable: matches!(spec.status, CheckStatus::Warn),
            scope: json!({ "kind": "system" }),
            observed_at: self.observed_at.clone(),
            duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            details: spec.details,
        });
    }
}

fn pragma_i64(connection: &Connection, name: &str) -> Result<i64, AppError> {
    connection
        .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .map_err(database_error)
}

fn query_count(connection: &Connection, statement: &str) -> Result<u64, AppError> {
    connection
        .query_row(statement, [], |row| row.get(0))
        .map_err(database_error)
}

fn missing_available_revisions(connection: &Connection, root: &Path) -> Result<u64, AppError> {
    let mut statement = connection
        .prepare("SELECT id FROM revisions WHERE state IN ('current','superseded')")
        .map_err(database_error)?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(database_error)?;
    let mut missing = 0_u64;
    for id in ids {
        let path = root.join("revisions").join(id.map_err(database_error)?);
        let present = fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
        missing += u64::from(!present);
    }
    Ok(missing)
}

fn observed_at() -> Result<String, AppError> {
    OffsetDateTime::now_utc()
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .map_err(|error| AppError::internal(format!("cannot format status time: {error}")))
}

fn directory_count(path: &Path) -> Result<u64, AppError> {
    let count = fs::read_dir(path)
        .map_err(|error| AppError::internal(format!("cannot inspect storage directory: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AppError::internal(format!("cannot inspect storage entry: {error}")))?
        .len();
    u64::try_from(count).map_err(|_| AppError::internal("storage entry count overflowed"))
}

fn metadata_len(path: &Path) -> Result<u64, AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(metadata.len())
        }
        Ok(_) => Err(AppError::internal("WAL path is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(AppError::internal(format!(
            "cannot inspect write-ahead log: {error}"
        ))),
    }
}

fn wal_path(path: &Path) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push("-wal");
    PathBuf::from(value)
}

#[allow(clippy::needless_pass_by_value)]
fn database_error(error: rusqlite::Error) -> AppError {
    AppError::internal(format!("catalogue status failure: {error}"))
}
