use std::fmt::{self, Display, Formatter};

use serde::Serialize;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliExit {
    Usage,
    NotFound,
    Conflict,
    Unavailable,
    Contention,
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
            Self::Internal => 10,
        }
    }
}

#[derive(Debug)]
pub struct AppError {
    code: ErrorCode,
    pub message: String,
    retryability: Retryability,
    details: Value,
    exit: CliExit,
}

impl AppError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(
            "invalid_input",
            message,
            Retryability::Terminal,
            CliExit::Usage,
        )
    }

    pub fn unavailable() -> Self {
        Self::new(
            "daemon_unavailable",
            "the Observatory daemon is unavailable",
            Retryability::Retryable,
            CliExit::Unavailable,
        )
    }

    pub fn unavailable_with(message: impl Into<String>) -> Self {
        Self::new(
            "daemon_unavailable",
            message,
            Retryability::Retryable,
            CliExit::Unavailable,
        )
    }

    pub fn already_running() -> Self {
        Self::new(
            "daemon_already_running",
            "another Observatory daemon holds the authority lock",
            Retryability::Retryable,
            CliExit::Contention,
        )
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(
            "not_found",
            message,
            Retryability::Terminal,
            CliExit::NotFound,
        )
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(
            "internal",
            message,
            Retryability::Terminal,
            CliExit::Internal,
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
        }
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

    fn new(
        code: &'static str,
        message: impl Into<String>,
        retryability: Retryability,
        exit: CliExit,
    ) -> Self {
        Self {
            code: ErrorCode::new(code),
            message: message.into(),
            retryability,
            details: json!({}),
            exit,
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
