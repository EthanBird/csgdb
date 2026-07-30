//! SQLCipher-backed transaction kernel adapter.

use csgdb_core::{
    Error, ErrorCode, OpenFlags as CoreOpenFlags, ResolvedKeyRef, ResolvedOpenPlan, Result,
    SecurityMode,
};
use rusqlite::{ffi, Connection as SqlConnection, OpenFlags as SqlOpenFlags};
use std::ffi::c_int;

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// A live storage-kernel connection.
pub struct Connection {
    inner: SqlConnection,
    security: SecurityMode,
}

impl Connection {
    /// Opens a validated database plan.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened, keying fails, the
    /// key is incorrect, or connection initialization cannot complete.
    pub fn open(plan: &ResolvedOpenPlan) -> Result<Self> {
        let flags = sqlite_open_flags(plan.flags());
        let inner = SqlConnection::open_with_flags(plan.path(), flags)
            .map_err(|error| map_open_error(&error))?;

        if plan.security() == SecurityMode::Encrypted {
            plan.with_key(|key| apply_key(&inner, key))?;
        }

        verify_database(&inner)?;
        inner
            .busy_timeout(plan.busy_timeout())
            .map_err(|error| map_storage_error(&error))?;
        configure_connection(&inner, plan)?;

        Ok(Self {
            inner,
            security: plan.security(),
        })
    }

    #[must_use]
    pub const fn security(&self) -> SecurityMode {
        self.security
    }

    /// Executes one or more SQL statements without returning rows.
    ///
    /// # Errors
    ///
    /// Returns an error when parsing or executing any statement fails.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.inner
            .execute_batch(sql)
            .map_err(|error| map_storage_error(&error))
    }

    /// Executes one SQL statement without bound parameters.
    ///
    /// # Errors
    ///
    /// Returns an error when the statement fails or returns rows.
    pub fn execute(&self, sql: &str) -> Result<usize> {
        self.inner
            .execute(sql, [])
            .map_err(|error| map_storage_error(&error))
    }

    /// Reads a single integer value.
    ///
    /// # Errors
    ///
    /// Returns an error when preparation, execution, conversion, or row
    /// cardinality validation fails.
    pub fn query_i64(&self, sql: &str) -> Result<i64> {
        self.inner
            .query_row(sql, [], |row| row.get(0))
            .map_err(|error| map_storage_error(&error))
    }

    /// Starts a deferred transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when a transaction is already active or storage is
    /// unavailable.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        let inner = self
            .inner
            .transaction()
            .map_err(|error| map_storage_error(&error))?;
        Ok(Transaction { inner })
    }

    /// Closes the database and reports pending close errors.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying connection cannot be closed.
    pub fn close(self) -> Result<()> {
        self.inner
            .close()
            .map_err(|(_connection, error)| map_storage_error(&error))
    }
}

/// A transaction that rolls back automatically unless committed.
pub struct Transaction<'connection> {
    inner: rusqlite::Transaction<'connection>,
}

impl Transaction<'_> {
    /// Executes one or more SQL statements inside the transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when parsing or executing any statement fails.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.inner
            .execute_batch(sql)
            .map_err(|error| map_storage_error(&error))
    }

    /// Reads a single integer value inside the transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when preparation, execution, conversion, or row
    /// cardinality validation fails.
    pub fn query_i64(&self, sql: &str) -> Result<i64> {
        self.inner
            .query_row(sql, [], |row| row.get(0))
            .map_err(|error| map_storage_error(&error))
    }

    /// Commits all transaction changes.
    ///
    /// # Errors
    ///
    /// Returns an error when the commit cannot be persisted.
    pub fn commit(self) -> Result<()> {
        self.inner
            .commit()
            .map_err(|error| map_storage_error(&error))
    }

    /// Rolls back all transaction changes.
    ///
    /// # Errors
    ///
    /// Returns an error when rollback fails.
    pub fn rollback(self) -> Result<()> {
        self.inner
            .rollback()
            .map_err(|error| map_storage_error(&error))
    }
}

fn sqlite_open_flags(flags: CoreOpenFlags) -> SqlOpenFlags {
    let mut mapped = if flags.contains(CoreOpenFlags::READONLY) {
        SqlOpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        SqlOpenFlags::SQLITE_OPEN_READ_WRITE
    };

    if flags.contains(CoreOpenFlags::CREATE) {
        mapped |= SqlOpenFlags::SQLITE_OPEN_CREATE;
    }
    if flags.contains(CoreOpenFlags::URI) {
        mapped |= SqlOpenFlags::SQLITE_OPEN_URI;
    }
    if flags.contains(CoreOpenFlags::MEMORY) {
        mapped |= SqlOpenFlags::SQLITE_OPEN_MEMORY;
    }
    if flags.contains(CoreOpenFlags::FULLMUTEX) {
        mapped |= SqlOpenFlags::SQLITE_OPEN_FULL_MUTEX;
    }
    if flags.contains(CoreOpenFlags::NOMUTEX) {
        mapped |= SqlOpenFlags::SQLITE_OPEN_NO_MUTEX;
    }
    if flags.contains(CoreOpenFlags::NOFOLLOW) {
        mapped |= SqlOpenFlags::SQLITE_OPEN_NOFOLLOW;
    }
    mapped
}

fn apply_key(connection: &SqlConnection, key: Option<ResolvedKeyRef<'_>>) -> Result<()> {
    let key = key.ok_or_else(|| {
        Error::new(
            ErrorCode::KeyRequired,
            "encrypted database open requires key material",
        )
    })?;

    match key {
        ResolvedKeyRef::Raw(bytes) => apply_raw_key(connection, bytes),
        ResolvedKeyRef::Passphrase(bytes) => apply_key_bytes(connection, bytes),
    }
}

fn apply_raw_key(connection: &SqlConnection, raw: &[u8; 32]) -> Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut literal = [0_u8; 67];
    literal[0] = b'x';
    literal[1] = b'\'';
    for (index, byte) in raw.iter().copied().enumerate() {
        literal[2 + index * 2] = HEX[usize::from(byte >> 4)];
        literal[3 + index * 2] = HEX[usize::from(byte & 0x0f)];
    }
    literal[66] = b'\'';
    let result = apply_key_bytes(connection, &literal);
    literal.fill(0);
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    result
}

fn apply_key_bytes(connection: &SqlConnection, bytes: &[u8]) -> Result<()> {
    let length = c_int::try_from(bytes.len()).map_err(|_| {
        Error::new(
            ErrorCode::InvalidKeyLength,
            "database key material is too large",
        )
    })?;

    // SAFETY: `connection.handle()` is valid for the life of `connection`;
    // `bytes` remains readable for the duration of the call. Keying is the
    // first database operation after open.
    let result = unsafe { ffi::sqlite3_key(connection.handle(), bytes.as_ptr().cast(), length) };
    if result == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::EncryptionBackendUnavailable,
            "database encryption key setup failed",
        ))
    }
}

fn verify_database(connection: &SqlConnection) -> Result<()> {
    connection
        .query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
        .map_err(|error| map_open_error(&error))
}

fn configure_connection(connection: &SqlConnection, plan: &ResolvedOpenPlan) -> Result<()> {
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(|error| map_storage_error(&error))?;

    let cache_kib = plan.cache_size_bytes().div_ceil(1024);
    let cache_kib = i64::try_from(cache_kib)
        .unwrap_or(i64::MAX)
        .saturating_neg();
    connection
        .pragma_update(None, "cache_size", cache_kib)
        .map_err(|error| map_storage_error(&error))?;

    if !plan.flags().contains(CoreOpenFlags::READONLY)
        && !plan.flags().contains(CoreOpenFlags::MEMORY)
    {
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| map_storage_error(&error))?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| map_storage_error(&error))?;
    }
    Ok(())
}

fn map_open_error(error: &rusqlite::Error) -> Error {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => match failure.code {
            rusqlite::ErrorCode::NotADatabase => Error::new(
                ErrorCode::InvalidDatabaseKey,
                "database key is incorrect or the file is not a supported database",
            ),
            rusqlite::ErrorCode::DatabaseCorrupt => Error::new(
                ErrorCode::DatabaseCorrupt,
                "database is corrupt or authentication failed",
            ),
            _ => map_storage_error(error),
        },
        _ => map_storage_error(error),
    }
}

fn map_storage_error(error: &rusqlite::Error) -> Error {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => match failure.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                Error::new(ErrorCode::DatabaseBusy, "database is busy or locked")
            }
            rusqlite::ErrorCode::ReadOnly => {
                Error::new(ErrorCode::DatabaseReadOnly, "database is read-only")
            }
            rusqlite::ErrorCode::ConstraintViolation => {
                Error::new(ErrorCode::ConstraintViolation, "database constraint failed")
            }
            rusqlite::ErrorCode::DatabaseCorrupt => {
                Error::new(ErrorCode::DatabaseCorrupt, "database is corrupt")
            }
            _ => Error::new(ErrorCode::Storage, "database operation failed"),
        },
        _ => Error::new(ErrorCode::Storage, "database operation failed"),
    }
}

#[must_use]
pub const fn plaintext_header() -> &'static [u8; 16] {
    SQLITE_HEADER
}
