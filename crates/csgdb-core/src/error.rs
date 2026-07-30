use std::error;
use std::fmt;

/// Stable, machine-readable error categories.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ErrorCode {
    InvalidOpenFlags,
    InvalidKeyLength,
    InvalidPath,
    InvalidSql,
    ParameterCountMismatch,
    InvalidParameterIndex,
    InvalidColumnIndex,
    InvalidColumnType,
    InvalidUtf8,
    InvalidStatementState,
    InvalidBusyTimeout,
    KeyRequired,
    KeyStoreUnavailable,
    PlaintextRequiresOptIn,
    EncryptionBackendUnavailable,
    StorageBackendUnavailable,
    DatabaseBusy,
    DatabaseReadOnly,
    ConstraintViolation,
    InvalidDatabaseKey,
    DatabaseCorrupt,
    QueryInterrupted,
    Storage,
}

/// CSGDB error with a stable category and a non-sensitive diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    code: ErrorCode,
    message: &'static str,
}

impl Error {
    #[must_use]
    pub const fn new(code: ErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
