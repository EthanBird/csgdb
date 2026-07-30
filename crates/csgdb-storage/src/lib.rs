//! SQLCipher-backed transaction kernel adapter.

use csgdb_core::{
    CheckpointMode, CheckpointResult, Error, ErrorCode, OpenFlags as CoreOpenFlags, ResolvedKeyRef,
    ResolvedOpenPlan, Result, SecurityMode, TransactionState, Value, ValueRef, ValueType,
};
use rusqlite::{
    ffi, types::ValueRef as SqlValueRef, CachedStatement, Connection as SqlConnection,
    OpenFlags as SqlOpenFlags,
};
use std::ffi::{c_int, c_void, CString};
use std::ptr;
use std::time::Duration;

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
const MAX_BUSY_TIMEOUT_MILLIS: u128 = 2_147_483_647;
pub const DEFAULT_PREPARED_STATEMENT_CACHE_CAPACITY: usize = 16;
pub const MAX_WAL_AUTOCHECKPOINT_FRAMES: u32 = i32::MAX as u32;

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
        Self::open_with_access(plan, ConnectionAccess::ReadWrite)
    }

    /// Opens a validated plan as a read-only connection.
    ///
    /// The resolved key is reused only while opening the connection; no key
    /// material is cloned.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened read-only, keying
    /// fails, the key is incorrect, or initialization cannot complete.
    pub fn open_readonly(plan: &ResolvedOpenPlan) -> Result<Self> {
        Self::open_with_access(plan, ConnectionAccess::ReadOnly)
    }

    fn open_with_access(plan: &ResolvedOpenPlan, access: ConnectionAccess) -> Result<Self> {
        let flags = match access {
            ConnectionAccess::ReadWrite => plan.flags(),
            ConnectionAccess::ReadOnly => readonly_open_flags(plan.flags()),
        };
        let flags = sqlite_open_flags(flags);
        let inner = SqlConnection::open_with_flags(plan.path(), flags)
            .map_err(|error| map_open_error(&error))?;

        if plan.security() == SecurityMode::Encrypted {
            plan.with_key(|key| apply_key(&inner, key))?;
        }

        verify_database(&inner)?;
        inner
            .busy_timeout(plan.busy_timeout())
            .map_err(|error| map_storage_error(&error))?;
        configure_connection(&inner, plan, access)?;
        inner.set_prepared_statement_cache_capacity(DEFAULT_PREPARED_STATEMENT_CACHE_CAPACITY);

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

    /// Executes one SQL statement with positional parameters.
    ///
    /// # Errors
    ///
    /// Returns an error when the statement fails or returns rows.
    pub fn execute(&self, sql: &str, parameters: &[ValueRef<'_>]) -> Result<usize> {
        self.prepare_cached(sql)?.execute(parameters)
    }

    /// Compiles a SQL statement.
    ///
    /// # Errors
    ///
    /// Returns an error when the SQL cannot be compiled.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        let inner = self
            .inner
            .prepare(sql)
            .map_err(|error| map_storage_error(&error))?;
        Ok(Statement::direct(inner))
    }

    /// Compiles a SQL statement using the connection's bounded LRU cache.
    ///
    /// The statement is returned to the cache when it is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when the SQL cannot be compiled.
    pub fn prepare_cached(&self, sql: &str) -> Result<Statement<'_>> {
        let inner = self
            .inner
            .prepare_cached(sql)
            .map_err(|error| map_storage_error(&error))?;
        Ok(Statement::cached(inner))
    }

    /// Changes the maximum number of idle compiled statements retained by
    /// the connection's LRU cache.
    pub fn set_prepared_statement_cache_capacity(&self, capacity: usize) {
        self.inner.set_prepared_statement_cache_capacity(capacity);
    }

    /// Finalizes every idle statement currently held by the connection cache.
    pub fn flush_prepared_statement_cache(&self) {
        self.inner.flush_prepared_statement_cache();
    }

    /// Returns the number of rows changed by the most recently completed
    /// INSERT, UPDATE, or DELETE statement.
    #[must_use]
    pub fn changes(&self) -> u64 {
        self.inner.changes()
    }

    /// Returns the cumulative number of changed rows since this connection
    /// was opened.
    #[must_use]
    pub fn total_changes(&self) -> u64 {
        self.inner.total_changes()
    }

    /// Returns the rowid produced by the most recent successful INSERT on
    /// this connection.
    #[must_use]
    pub fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }

    /// Returns whether the connection is currently in autocommit mode.
    #[must_use]
    pub fn is_autocommit(&self) -> bool {
        self.inner.is_autocommit()
    }

    /// Returns whether any statement on the connection is actively running.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.inner.is_busy()
    }

    /// Returns whether an interrupt is currently pending or being processed.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.inner.is_interrupted()
    }

    /// Returns whether a named attached database is read-only.
    ///
    /// # Errors
    ///
    /// Returns an error when `database_name` is not attached.
    pub fn is_readonly(&self, database_name: &str) -> Result<bool> {
        self.inner
            .is_readonly(database_name)
            .map_err(|error| map_storage_error(&error))
    }

    /// Returns the current transaction activity for one database, or the
    /// highest activity across all attached databases when the name is absent.
    ///
    /// # Errors
    ///
    /// Returns an error when the engine cannot inspect transaction state.
    pub fn transaction_state(&self, database_name: Option<&str>) -> Result<TransactionState> {
        let state = self
            .inner
            .transaction_state(database_name)
            .map_err(|error| map_storage_error(&error))?;
        map_transaction_state(state)
    }

    /// Replaces the connection's busy timeout.
    ///
    /// # Errors
    ///
    /// Returns an error rather than panicking when the timeout exceeds the
    /// storage engine's signed 32-bit millisecond range.
    pub fn set_busy_timeout(&self, timeout: Duration) -> Result<()> {
        validate_busy_timeout(timeout)?;
        self.inner
            .busy_timeout(timeout)
            .map_err(|error| map_storage_error(&error))
    }

    /// Runs a WAL checkpoint against the main database.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint cannot be started or the storage
    /// engine reports a failure other than lock contention.
    pub fn checkpoint(&self, mode: CheckpointMode) -> Result<CheckpointResult> {
        self.checkpoint_database(Some("main"), mode)
    }

    /// Runs a WAL checkpoint against one attached database, or every attached
    /// database when `database_name` is absent.
    ///
    /// Lock contention is reported in [`CheckpointResult`] so callers retain
    /// the frame counts produced by the engine.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid database name or a storage failure
    /// other than lock contention.
    pub fn checkpoint_database(
        &self,
        database_name: Option<&str>,
        mode: CheckpointMode,
    ) -> Result<CheckpointResult> {
        let database_name = database_name.map(CString::new).transpose().map_err(|_| {
            Error::new(
                ErrorCode::InvalidDatabaseName,
                "database name contains an embedded NUL byte",
            )
        })?;
        let database_name = database_name
            .as_ref()
            .map_or(ptr::null(), |name| name.as_ptr());
        let mut wal_frames = -1;
        let mut checkpointed_frames = -1;
        // SAFETY: the connection owns a live handle, the optional database
        // name remains valid for the call, and both output pointers are live.
        let result = unsafe {
            ffi::sqlite3_wal_checkpoint_v2(
                self.inner.handle(),
                database_name,
                mode as c_int,
                &raw mut wal_frames,
                &raw mut checkpointed_frames,
            )
        };
        if result != ffi::SQLITE_OK && result != ffi::SQLITE_BUSY {
            return Err(map_raw_storage_error(result));
        }

        Ok(CheckpointResult::new(
            non_negative_frame_count(wal_frames),
            non_negative_frame_count(checkpointed_frames),
            result == ffi::SQLITE_BUSY,
        ))
    }

    /// Sets the connection-local passive auto-checkpoint threshold.
    ///
    /// A threshold of zero disables automatic checkpoints.
    ///
    /// # Errors
    ///
    /// Returns an error when the threshold exceeds the engine's signed
    /// 32-bit range or the engine rejects the setting.
    pub fn set_wal_autocheckpoint(&self, frames: u32) -> Result<()> {
        let frames = c_int::try_from(frames).map_err(|_| {
            Error::new(
                ErrorCode::InvalidCheckpointThreshold,
                "WAL auto-checkpoint threshold exceeds the supported range",
            )
        })?;
        // SAFETY: the connection owns a live handle and the threshold was
        // validated for the engine's C integer API.
        let result = unsafe { ffi::sqlite3_wal_autocheckpoint(self.inner.handle(), frames) };
        if result == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(map_raw_storage_error(result))
        }
    }

    /// Creates a thread-safe handle for interrupting a long-running operation.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle {
            inner: self.inner.get_interrupt_handle(),
        }
    }

    /// Asks the storage engine to release connection-local heap caches.
    ///
    /// # Errors
    ///
    /// Returns an error when the engine cannot release its caches.
    pub fn release_memory(&self) -> Result<()> {
        self.inner
            .release_memory()
            .map_err(|error| map_storage_error(&error))
    }

    /// Returns the underlying SQL connection pointer for the C ABI layer.
    ///
    /// # Safety
    ///
    /// The pointer is valid only while this `Connection` remains alive. The
    /// caller must serialize all uses with safe connection operations, must
    /// not close it, and must not retain references derived from it.
    #[doc(hidden)]
    pub unsafe fn as_raw_handle(&self) -> *mut c_void {
        // SAFETY: the caller accepts the lifetime and synchronization
        // requirements documented above.
        unsafe { self.inner.handle().cast() }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionAccess {
    ReadOnly,
    ReadWrite,
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

    /// Executes one SQL statement with positional parameters.
    ///
    /// # Errors
    ///
    /// Returns an error when compilation, binding, or execution fails.
    pub fn execute(&self, sql: &str, parameters: &[ValueRef<'_>]) -> Result<usize> {
        self.prepare(sql)?.execute(parameters)
    }

    /// Compiles a SQL statement scoped to this transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when the SQL cannot be compiled.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        let inner = self
            .inner
            .prepare(sql)
            .map_err(|error| map_storage_error(&error))?;
        Ok(Statement::direct(inner))
    }

    /// Returns the transaction activity for one database, or the highest
    /// activity across all attached databases when the name is absent.
    ///
    /// # Errors
    ///
    /// Returns an error when the engine cannot inspect transaction state.
    pub fn transaction_state(&self, database_name: Option<&str>) -> Result<TransactionState> {
        let state = self
            .inner
            .transaction_state(database_name)
            .map_err(|error| map_storage_error(&error))?;
        map_transaction_state(state)
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

/// A thread-safe cancellation handle detached from a database borrow.
///
/// Calling [`InterruptHandle::interrupt`] after the database has closed is a
/// safe no-op.
pub struct InterruptHandle {
    inner: rusqlite::InterruptHandle,
}

impl InterruptHandle {
    /// Interrupts the operation currently executing on the connection.
    pub fn interrupt(&self) {
        self.inner.interrupt();
    }
}

enum StatementInner<'connection> {
    Direct(rusqlite::Statement<'connection>),
    Cached(CachedStatement<'connection>),
}

/// A compiled SQL statement.
///
/// A statement borrows its connection, so it cannot outlive the database that
/// compiled it. Bound text and blob values are copied by the storage engine.
pub struct Statement<'connection> {
    inner: StatementInner<'connection>,
}

impl<'connection> Statement<'connection> {
    fn direct(inner: rusqlite::Statement<'connection>) -> Self {
        Self {
            inner: StatementInner::Direct(inner),
        }
    }

    fn cached(inner: CachedStatement<'connection>) -> Self {
        Self {
            inner: StatementInner::Cached(inner),
        }
    }

    fn inner(&self) -> &rusqlite::Statement<'connection> {
        match &self.inner {
            StatementInner::Direct(inner) => inner,
            StatementInner::Cached(inner) => inner,
        }
    }

    fn inner_mut(&mut self) -> &mut rusqlite::Statement<'connection> {
        match &mut self.inner {
            StatementInner::Direct(inner) => inner,
            StatementInner::Cached(inner) => inner,
        }
    }

    #[must_use]
    pub fn parameter_count(&self) -> usize {
        self.inner().parameter_count()
    }

    #[must_use]
    pub fn column_count(&self) -> usize {
        self.inner().column_count()
    }

    /// Returns a result-column name.
    ///
    /// # Errors
    ///
    /// Returns an error when `index` is outside the result column range.
    pub fn column_name(&self, index: usize) -> Result<&str> {
        self.inner().column_name(index).map_err(|_| {
            Error::new(
                ErrorCode::InvalidColumnIndex,
                "result column index is out of range",
            )
        })
    }

    /// Executes the statement with an exact positional parameter list.
    ///
    /// # Errors
    ///
    /// Returns an error on a parameter count mismatch or execution failure.
    pub fn execute(&mut self, parameters: &[ValueRef<'_>]) -> Result<usize> {
        self.bind_all(parameters)?;
        self.inner_mut()
            .raw_execute()
            .map_err(|error| map_storage_error(&error))
    }

    /// Starts a streaming query with an exact positional parameter list.
    ///
    /// Only the current row is borrowed from the engine at a time. Calling
    /// [`Rows::next_row`] invalidates the previously returned row.
    ///
    /// # Errors
    ///
    /// Returns an error on a parameter count mismatch or binding failure.
    pub fn query<'statement>(
        &'statement mut self,
        parameters: &[ValueRef<'_>],
    ) -> Result<Rows<'statement>> {
        self.bind_all(parameters)?;
        Ok(Rows {
            inner: self.inner_mut().raw_query(),
        })
    }

    /// Removes all parameter bindings.
    pub fn clear_bindings(&mut self) {
        self.inner_mut().clear_bindings();
    }

    fn bind_all(&mut self, parameters: &[ValueRef<'_>]) -> Result<()> {
        let expected = self.parameter_count();
        if parameters.len() != expected {
            return Err(Error::new(
                ErrorCode::ParameterCountMismatch,
                "SQL parameter count does not match the supplied values",
            ));
        }

        self.clear_bindings();
        for (offset, value) in parameters.iter().copied().enumerate() {
            let index = offset + 1;
            let result = match value {
                ValueRef::Null => self
                    .inner_mut()
                    .raw_bind_parameter(index, rusqlite::types::Null),
                ValueRef::Integer(value) => self.inner_mut().raw_bind_parameter(index, value),
                ValueRef::Real(value) => self.inner_mut().raw_bind_parameter(index, value),
                ValueRef::Text(value) => self.inner_mut().raw_bind_parameter(index, value),
                ValueRef::Blob(value) => self.inner_mut().raw_bind_parameter(index, value),
            };
            result.map_err(|error| map_storage_error(&error))?;
        }
        Ok(())
    }
}

/// A forward-only stream of query rows.
pub struct Rows<'statement> {
    inner: rusqlite::Rows<'statement>,
}

impl Rows<'_> {
    /// Advances to the next result row.
    ///
    /// # Errors
    ///
    /// Returns an error when the storage engine cannot continue the query.
    pub fn next_row(&mut self) -> Result<Option<Row<'_>>> {
        self.inner
            .next()
            .map(|row| row.map(|inner| Row { inner }))
            .map_err(|error| map_storage_error(&error))
    }
}

/// A borrowed query result row.
///
/// The row is valid only until its parent [`Rows`] stream advances.
pub struct Row<'row> {
    inner: &'row rusqlite::Row<'row>,
}

impl Row<'_> {
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.inner.as_ref().column_count()
    }

    /// Returns a column name.
    ///
    /// # Errors
    ///
    /// Returns an error when `index` is outside the result column range.
    pub fn column_name(&self, index: usize) -> Result<&str> {
        self.inner.as_ref().column_name(index).map_err(|_| {
            Error::new(
                ErrorCode::InvalidColumnIndex,
                "result column index is out of range",
            )
        })
    }

    /// Returns a borrowed dynamically typed column value.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range column or invalid UTF-8 text.
    pub fn value_ref(&self, index: usize) -> Result<ValueRef<'_>> {
        if index >= self.column_count() {
            return Err(Error::new(
                ErrorCode::InvalidColumnIndex,
                "result column index is out of range",
            ));
        }
        match self
            .inner
            .get_ref(index)
            .map_err(|error| map_storage_error(&error))?
        {
            SqlValueRef::Null => Ok(ValueRef::Null),
            SqlValueRef::Integer(value) => Ok(ValueRef::Integer(value)),
            SqlValueRef::Real(value) => Ok(ValueRef::Real(value)),
            SqlValueRef::Text(value) => std::str::from_utf8(value)
                .map(ValueRef::Text)
                .map_err(|_| Error::new(ErrorCode::InvalidUtf8, "text value is not valid UTF-8")),
            SqlValueRef::Blob(value) => Ok(ValueRef::Blob(value)),
        }
    }

    /// Copies a dynamically typed column value.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range column or invalid UTF-8 text.
    pub fn value(&self, index: usize) -> Result<Value> {
        self.value_ref(index).map(ValueRef::to_owned)
    }

    /// Returns the storage class of a column value.
    ///
    /// # Errors
    ///
    /// Returns an error when `index` is outside the result column range.
    pub fn value_type(&self, index: usize) -> Result<ValueType> {
        self.value_ref(index).map(ValueRef::value_type)
    }

    /// Returns an integer without applying cross-type coercion.
    ///
    /// # Errors
    ///
    /// Returns an error when the column is not an integer.
    pub fn get_i64(&self, index: usize) -> Result<i64> {
        match self.value_ref(index)? {
            ValueRef::Integer(value) => Ok(value),
            _ => Err(invalid_column_type()),
        }
    }

    /// Returns a floating-point value without applying cross-type coercion.
    ///
    /// # Errors
    ///
    /// Returns an error when the column is not a real value.
    pub fn get_f64(&self, index: usize) -> Result<f64> {
        match self.value_ref(index)? {
            ValueRef::Real(value) => Ok(value),
            _ => Err(invalid_column_type()),
        }
    }

    /// Returns text without applying cross-type coercion.
    ///
    /// # Errors
    ///
    /// Returns an error when the column is not valid UTF-8 text.
    pub fn get_text(&self, index: usize) -> Result<&str> {
        match self.value_ref(index)? {
            ValueRef::Text(value) => Ok(value),
            _ => Err(invalid_column_type()),
        }
    }

    /// Returns a blob without applying cross-type coercion.
    ///
    /// # Errors
    ///
    /// Returns an error when the column is not a blob.
    pub fn get_blob(&self, index: usize) -> Result<&[u8]> {
        match self.value_ref(index)? {
            ValueRef::Blob(value) => Ok(value),
            _ => Err(invalid_column_type()),
        }
    }
}

fn invalid_column_type() -> Error {
    Error::new(
        ErrorCode::InvalidColumnType,
        "result column has an incompatible storage class",
    )
}

fn map_transaction_state(state: rusqlite::TransactionState) -> Result<TransactionState> {
    match state {
        rusqlite::TransactionState::None => Ok(TransactionState::None),
        rusqlite::TransactionState::Read => Ok(TransactionState::Read),
        rusqlite::TransactionState::Write => Ok(TransactionState::Write),
        _ => Err(Error::new(
            ErrorCode::Storage,
            "database returned an unknown transaction state",
        )),
    }
}

fn validate_busy_timeout(timeout: Duration) -> Result<()> {
    if timeout.as_millis() > MAX_BUSY_TIMEOUT_MILLIS {
        Err(Error::new(
            ErrorCode::InvalidBusyTimeout,
            "busy timeout exceeds the supported millisecond range",
        ))
    } else {
        Ok(())
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

fn readonly_open_flags(flags: CoreOpenFlags) -> CoreOpenFlags {
    let mut readonly = CoreOpenFlags::READONLY;
    for retained in [
        CoreOpenFlags::URI,
        CoreOpenFlags::MEMORY,
        CoreOpenFlags::FULLMUTEX,
        CoreOpenFlags::NOMUTEX,
        CoreOpenFlags::NOFOLLOW,
    ] {
        if flags.contains(retained) {
            readonly |= retained;
        }
    }
    readonly
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

fn configure_connection(
    connection: &SqlConnection,
    plan: &ResolvedOpenPlan,
    access: ConnectionAccess,
) -> Result<()> {
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

    if access == ConnectionAccess::ReadWrite
        && !plan.flags().contains(CoreOpenFlags::READONLY)
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
            rusqlite::ErrorCode::OperationInterrupted => Error::new(
                ErrorCode::QueryInterrupted,
                "database operation was interrupted",
            ),
            _ => Error::new(ErrorCode::Storage, "database operation failed"),
        },
        rusqlite::Error::SqlInputError { .. } => {
            Error::new(ErrorCode::InvalidSql, "SQL statement is invalid")
        }
        rusqlite::Error::InvalidParameterCount(_, _) => Error::new(
            ErrorCode::ParameterCountMismatch,
            "SQL parameter count does not match the supplied values",
        ),
        rusqlite::Error::InvalidParameterName(_) => Error::new(
            ErrorCode::InvalidParameterIndex,
            "SQL parameter index or name is invalid",
        ),
        rusqlite::Error::InvalidColumnIndex(_) => Error::new(
            ErrorCode::InvalidColumnIndex,
            "result column index is out of range",
        ),
        rusqlite::Error::InvalidColumnType(_, _, _) => Error::new(
            ErrorCode::InvalidColumnType,
            "result column has an incompatible storage class",
        ),
        rusqlite::Error::Utf8Error(..) => {
            Error::new(ErrorCode::InvalidUtf8, "text value is not valid UTF-8")
        }
        _ => Error::new(ErrorCode::Storage, "database operation failed"),
    }
}

fn map_raw_storage_error(result: c_int) -> Error {
    map_storage_error(&rusqlite::Error::SqliteFailure(
        ffi::Error::new(result),
        None,
    ))
}

fn non_negative_frame_count(value: c_int) -> Option<u32> {
    u32::try_from(value).ok()
}

#[must_use]
pub const fn plaintext_header() -> &'static [u8; 16] {
    SQLITE_HEADER
}
