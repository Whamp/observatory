use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
struct ErrorCode(String);

impl ErrorCode {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Retryability {
    Retryable,
    Terminal,
}

impl Retryability {
    pub const fn from_bool(retryable: bool) -> Self {
        if retryable {
            Self::Retryable
        } else {
            Self::Terminal
        }
    }

    const fn as_bool(self) -> bool {
        matches!(self, Self::Retryable)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CliExit {
    Usage,
    NotFound,
    Conflict,
    Unavailable,
    Contention,
    SourceChanged,
    Internal,
}

impl CliExit {
    pub const fn code(self) -> u8 {
        match self {
            Self::Usage => 2,
            Self::NotFound => 3,
            Self::Conflict => 4,
            Self::Unavailable => 5,
            Self::Contention => 6,
            Self::SourceChanged => 7,
            Self::Internal => 10,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ApiStatus {
    Forbidden,
    NotFound,
    Conflict,
    PreconditionFailed,
    PreconditionRequired,
    Gone,
    UnsupportedMedia,
    Unprocessable,
    Locked,
    Internal,
    Unavailable,
    InsufficientStorage,
}

impl ApiStatus {
    const fn code(self) -> u16 {
        match self {
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::Conflict => 409,
            Self::Gone => 410,
            _ => self.server_or_extended_code(),
        }
    }

    const fn server_or_extended_code(self) -> u16 {
        match self {
            Self::UnsupportedMedia => 415,
            Self::Internal => 500,
            Self::Unavailable => 503,
            Self::InsufficientStorage => 507,
            _ => self.extended_code(),
        }
    }

    const fn extended_code(self) -> u16 {
        match self {
            Self::PreconditionFailed => 412,
            Self::Unprocessable => 422,
            Self::Locked => 423,
            Self::PreconditionRequired => 428,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredError {
    code: String,
    message: String,
    retryability: Retryability,
    details: Value,
    exit: CliExit,
    api_status: ApiStatus,
}

#[derive(Debug)]
pub struct AppError {
    code: ErrorCode,
    pub message: String,
    retryability: Retryability,
    details: Value,
    exit: CliExit,
    api_status: ApiStatus,
    replayed: bool,
}

impl AppError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(
            "invalid_input",
            message,
            Retryability::Terminal,
            CliExit::Usage,
            ApiStatus::Unprocessable,
        )
    }

    pub fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            code,
            message,
            Retryability::Terminal,
            CliExit::Usage,
            ApiStatus::Unprocessable,
        )
    }

    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            code,
            message,
            Retryability::Terminal,
            CliExit::Usage,
            ApiStatus::Forbidden,
        )
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            code,
            message,
            Retryability::Terminal,
            CliExit::Conflict,
            ApiStatus::Conflict,
        )
    }

    pub fn changed_record() -> Self {
        Self::new(
            "changed_record",
            "the resource changed",
            Retryability::Terminal,
            CliExit::Conflict,
            ApiStatus::PreconditionFailed,
        )
    }

    pub fn precondition_required() -> Self {
        Self::new(
            "precondition_required",
            "If-Match is required",
            Retryability::Terminal,
            CliExit::Conflict,
            ApiStatus::PreconditionRequired,
        )
    }

    pub fn retryable_conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            code,
            message,
            Retryability::Retryable,
            CliExit::Conflict,
            ApiStatus::Conflict,
        )
    }

    pub fn contention(message: impl Into<String>) -> Self {
        Self::new(
            "contention",
            message,
            Retryability::Retryable,
            CliExit::Contention,
            ApiStatus::Locked,
        )
    }

    pub fn source_changed() -> Self {
        Self::new(
            "source_changed",
            "source changed while it was copied",
            Retryability::Retryable,
            CliExit::SourceChanged,
            ApiStatus::Unprocessable,
        )
    }

    pub fn unsupported_media(message: impl Into<String>) -> Self {
        Self::new(
            "unsupported_entry_media",
            message,
            Retryability::Terminal,
            CliExit::Usage,
            ApiStatus::UnsupportedMedia,
        )
    }

    pub fn capacity(details: Value) -> Self {
        let mut error = Self::new(
            "capacity",
            "Artifact Publish is blocked by storage capacity",
            Retryability::Terminal,
            CliExit::Internal,
            ApiStatus::InsufficientStorage,
        );
        error.details = details;
        error
    }

    pub fn gone(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            code,
            message,
            Retryability::Terminal,
            CliExit::NotFound,
            ApiStatus::Gone,
        )
    }

    pub fn client_timeout(idempotency_key: Option<&str>) -> Self {
        let mut error = Self::new(
            "client_timeout",
            "client wait deadline expired; commit state is unknown; retry the identical request with the same Idempotency-Key",
            Retryability::Retryable,
            CliExit::Unavailable,
            ApiStatus::Unavailable,
        );
        error.details = match idempotency_key {
            Some(key) => json!({
                "idempotencyKey": key,
                "retry": "repeat the identical request with the same key"
            }),
            None => json!({
                "retry": "repeat the identical read request"
            }),
        };
        error
    }

    pub fn unavailable() -> Self {
        Self::new(
            "daemon_unavailable",
            "the Observatory daemon is unavailable",
            Retryability::Retryable,
            CliExit::Unavailable,
            ApiStatus::Unavailable,
        )
    }

    pub fn unavailable_with(message: impl Into<String>) -> Self {
        Self::new(
            "daemon_unavailable",
            message,
            Retryability::Retryable,
            CliExit::Unavailable,
            ApiStatus::Unavailable,
        )
    }

    pub fn already_running() -> Self {
        Self::new(
            "daemon_already_running",
            "another Observatory daemon holds the authority lock",
            Retryability::Retryable,
            CliExit::Contention,
            ApiStatus::Locked,
        )
    }

    pub fn not_found_code(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            code,
            message,
            Retryability::Terminal,
            CliExit::NotFound,
            ApiStatus::NotFound,
        )
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(
            "not_found",
            message,
            Retryability::Terminal,
            CliExit::NotFound,
            ApiStatus::NotFound,
        )
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(
            "internal",
            message,
            Retryability::Terminal,
            CliExit::Internal,
            ApiStatus::Internal,
        )
    }

    pub fn remote(
        code: impl Into<String>,
        message: impl Into<String>,
        retryability: Retryability,
        details: Value,
        exit: CliExit,
    ) -> Self {
        Self {
            code: ErrorCode::new(code),
            message: message.into(),
            retryability,
            details,
            exit,
            api_status: ApiStatus::Internal,
            replayed: false,
        }
    }

    pub(crate) fn stored(&self) -> StoredError {
        StoredError {
            code: self.code.as_str().to_owned(),
            message: self.message.clone(),
            retryability: self.retryability,
            details: self.details.clone(),
            exit: self.exit,
            api_status: self.api_status,
        }
    }

    pub(crate) fn from_stored(stored: StoredError) -> Self {
        Self {
            code: ErrorCode::new(stored.code),
            message: stored.message,
            retryability: stored.retryability,
            details: stored.details,
            exit: stored.exit,
            api_status: stored.api_status,
            replayed: true,
        }
    }

    pub const fn replayed(&self) -> bool {
        self.replayed
    }

    pub fn envelope(&self) -> Value {
        json!({
            "schemaVersion": 1,
            "ok": false,
            "error": {
                "code": self.code.as_str(),
                "message": self.message,
                "retryable": self.retryability.as_bool(),
                "details": self.details
            }
        })
    }

    pub fn code(&self) -> &str {
        self.code.as_str()
    }

    pub const fn exit_code(&self) -> u8 {
        self.exit.code()
    }

    pub const fn api_status(&self) -> u16 {
        self.api_status.code()
    }

    fn new(
        code: &'static str,
        message: impl Into<String>,
        retryability: Retryability,
        exit: CliExit,
        api_status: ApiStatus,
    ) -> Self {
        Self {
            code: ErrorCode::new(code),
            message: message.into(),
            retryability,
            details: json!({}),
            exit,
            api_status,
            replayed: false,
        }
    }
}

impl Display for AppError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Success<T: Serialize> {
    schema_version: u8,
    ok: bool,
    result: T,
}

impl<T: Serialize> Success<T> {
    pub fn new(result: T) -> Self {
        Self {
            schema_version: 1,
            ok: true,
            result,
        }
    }
}
