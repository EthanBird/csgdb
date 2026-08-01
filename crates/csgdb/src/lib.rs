//! Public Rust API for CSGDB.
//!
//! The M1 foundation exposes secure database opening, SQL execution,
//! transactions, and optional bounded connection management over the bundled
//! encrypted storage kernel.

extern crate self as csgdb;

mod collection;
mod pool;
mod query;

pub use collection::{
    Collection, CollectionCrud, CollectionField, CollectionSchema, ColumnType, FieldSchema,
    FieldValue, IndexSchema, MigrationStatus, RegisteredSchema, SchemaFingerprint,
    SchemaRegistration,
};
pub use csgdb_derive::Collection;
pub use query::{
    CollectionQuery, CollectionQueryExecutor, IntoQueryValue, OrderDirection, OrderedFieldValue,
    Predicate, QueryDraft, QueryOrder, MAX_PREDICATE_DEPTH, MAX_PREDICATE_NODES, MAX_QUERY_LIMIT,
    MAX_QUERY_ORDER_FIELDS,
};

pub use pool::{
    BatchStatement, DatabasePool, DatabasePoolBuilder, GroupCommitOptions, PoolOptions, PoolStats,
    ReadConnection, ReadTransaction, WalMaintenanceOptions, WriteBackpressure,
    DEFAULT_GROUP_COMMIT_DELAY, DEFAULT_GROUP_COMMIT_MAX_JOBS,
    DEFAULT_MAINTENANCE_INTERVAL_COMMITS, DEFAULT_READ_CONNECTIONS, DEFAULT_WAL_SOFT_LIMIT_FRAMES,
    DEFAULT_WRITE_QUEUE_CAPACITY, MAX_GROUP_COMMIT_DELAY, MAX_GROUP_COMMIT_JOBS,
    MAX_MAINTENANCE_INTERVAL_COMMITS, MAX_READ_CONNECTIONS, MAX_WRITE_QUEUE_CAPACITY,
};

pub use csgdb_core::{
    prepare_open, CheckpointMode, CheckpointResult, DatabaseIdentity, Error, ErrorCode,
    KeyProvider, KeySource, OpenFlags, OpenOptions, OpenPlan, ResolvedKeyRef, ResolvedOpenPlan,
    Result, SecretKey, SecretString, SecurityMode, TransactionState, Value, ValueRef, ValueType,
    ABI_VERSION, LIB_VERSION, LIB_VERSION_NUMBER, RAW_KEY_LENGTH, SOURCE_ID,
};
pub use csgdb_storage::{
    InterruptHandle, Row, Rows, Statement, Transaction, DEFAULT_PREPARED_STATEMENT_CACHE_CAPACITY,
    MAX_WAL_AUTOCHECKPOINT_FRAMES,
};

use std::ffi::c_void;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Builder for a database connection.
pub struct DatabaseBuilder {
    path: PathBuf,
    options: OpenOptions,
}

impl DatabaseBuilder {
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            options: OpenOptions::default(),
        }
    }

    #[must_use]
    pub fn key(mut self, key: KeySource) -> Self {
        self.options.key = key;
        self
    }

    #[must_use]
    pub fn key_provider(mut self, provider: Arc<dyn KeyProvider>) -> Self {
        self.options.auto_key_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn plaintext(mut self) -> Self {
        self.options.flags =
            OpenFlags::READWRITE | OpenFlags::CREATE | OpenFlags::PLAINTEXT | OpenFlags::FULLMUTEX;
        self
    }

    #[must_use]
    pub fn flags(mut self, flags: OpenFlags) -> Self {
        self.options.flags = flags;
        self
    }

    #[must_use]
    pub fn options(mut self, options: OpenOptions) -> Self {
        self.options = options;
        self
    }

    /// Validates the open policy and resolves its key material.
    ///
    /// # Errors
    ///
    /// Returns an error when the path or flags are invalid, encryption has no
    /// usable key source, or the configured key provider fails.
    pub fn plan(self) -> Result<ResolvedOpenPlan> {
        prepare_open(self.path, self.options)?.resolve_key()
    }

    /// Opens the database after validating policy and resolving its key.
    ///
    /// # Errors
    ///
    /// Returns an error when planning, key resolution, storage opening, or
    /// connection initialization fails.
    pub fn open(self) -> Result<Database> {
        let plan = self.plan()?;
        Database::open_resolved(&plan, false)
    }
}

/// A live embedded database connection.
pub struct Database {
    connection: csgdb_storage::Connection,
}

impl fmt::Debug for Database {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Database")
            .field("security", &self.security())
            .finish_non_exhaustive()
    }
}

impl Database {
    fn open_resolved(plan: &ResolvedOpenPlan, readonly: bool) -> Result<Self> {
        let connection = if readonly {
            csgdb_storage::Connection::open_readonly(plan)?
        } else {
            csgdb_storage::Connection::open(plan)?
        };
        Ok(Self { connection })
    }

    #[must_use]
    pub fn builder(path: impl AsRef<Path>) -> DatabaseBuilder {
        DatabaseBuilder::new(path)
    }

    /// Opens a database using the default encrypted policy.
    ///
    /// This requires a configured key provider. It never falls back to
    /// plaintext.
    ///
    /// # Errors
    ///
    /// Returns an error when no key provider is configured or opening fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::builder(path).open()
    }

    /// Opens an encrypted database using explicit key material.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is invalid or opening fails.
    pub fn open_with_key(path: impl AsRef<Path>, key: KeySource) -> Result<Self> {
        Self::builder(path).key(key).open()
    }

    /// Opens an encrypted database with a passphrase.
    ///
    /// # Errors
    ///
    /// Returns an error when the passphrase is empty, incorrect, or opening
    /// fails.
    pub fn open_with_passphrase(
        path: impl AsRef<Path>,
        passphrase: impl AsRef<[u8]>,
    ) -> Result<Self> {
        Self::open_with_key(path, KeySource::Passphrase(SecretString::new(passphrase)))
    }

    /// Explicitly opens a plaintext-compatible database.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or initialized.
    pub fn open_plaintext(path: impl AsRef<Path>) -> Result<Self> {
        Self::builder(path).plaintext().open()
    }

    #[must_use]
    pub const fn security(&self) -> SecurityMode {
        self.connection.security()
    }

    /// Executes one or more SQL statements without returning rows.
    ///
    /// # Errors
    ///
    /// Returns an error when parsing or executing any statement fails.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.connection.execute_batch(sql)
    }

    /// Executes one SQL statement with positional parameters.
    ///
    /// # Errors
    ///
    /// Returns an error when the statement fails or returns rows.
    pub fn execute(&self, sql: &str, parameters: &[ValueRef<'_>]) -> Result<usize> {
        self.connection.execute(sql, parameters)
    }

    /// Compiles a SQL statement.
    ///
    /// # Errors
    ///
    /// Returns an error when the SQL cannot be compiled.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        self.connection.prepare(sql)
    }

    /// Compiles a SQL statement through the bounded connection-local LRU
    /// cache. Dropping the statement returns it to the cache.
    ///
    /// # Errors
    ///
    /// Returns an error when the SQL cannot be compiled.
    pub fn prepare_cached(&self, sql: &str) -> Result<Statement<'_>> {
        self.connection.prepare_cached(sql)
    }

    /// Changes the maximum number of idle compiled statements retained by the
    /// connection-local LRU cache.
    pub fn set_prepared_statement_cache_capacity(&self, capacity: usize) {
        self.connection
            .set_prepared_statement_cache_capacity(capacity);
    }

    /// Finalizes every idle statement currently held by this connection's
    /// cache.
    pub fn flush_prepared_statement_cache(&self) {
        self.connection.flush_prepared_statement_cache();
    }

    /// Returns the number of rows changed by the most recently completed
    /// INSERT, UPDATE, or DELETE statement.
    #[must_use]
    pub fn changes(&self) -> u64 {
        self.connection.changes()
    }

    /// Returns the cumulative number of changed rows since this connection
    /// was opened.
    #[must_use]
    pub fn total_changes(&self) -> u64 {
        self.connection.total_changes()
    }

    /// Returns the rowid produced by the most recent successful INSERT.
    #[must_use]
    pub fn last_insert_rowid(&self) -> i64 {
        self.connection.last_insert_rowid()
    }

    /// Returns whether the connection is currently in autocommit mode.
    #[must_use]
    pub fn is_autocommit(&self) -> bool {
        self.connection.is_autocommit()
    }

    /// Returns whether any statement on the connection is actively running.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.connection.is_busy()
    }

    /// Returns whether an interrupt is currently pending or being processed.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.connection.is_interrupted()
    }

    /// Returns whether a named attached database is read-only.
    ///
    /// # Errors
    ///
    /// Returns an error when `database_name` is not attached.
    pub fn is_readonly(&self, database_name: &str) -> Result<bool> {
        self.connection.is_readonly(database_name)
    }

    /// Returns current transaction activity for one database, or the highest
    /// activity across all attached databases when the name is absent.
    ///
    /// # Errors
    ///
    /// Returns an error when transaction state cannot be inspected.
    pub fn transaction_state(&self, database_name: Option<&str>) -> Result<TransactionState> {
        self.connection.transaction_state(database_name)
    }

    /// Replaces the busy timeout for this connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the timeout is outside the engine's range.
    pub fn set_busy_timeout(&self, timeout: Duration) -> Result<()> {
        self.connection.set_busy_timeout(timeout)
    }

    /// Runs a WAL checkpoint against the main database.
    ///
    /// # Errors
    ///
    /// Returns an error when the storage engine cannot run the checkpoint.
    /// Lock contention is returned as progress in [`CheckpointResult`].
    pub fn checkpoint(&self, mode: CheckpointMode) -> Result<CheckpointResult> {
        self.connection.checkpoint(mode)
    }

    /// Runs a WAL checkpoint against one attached database, or every attached
    /// database when `database_name` is absent.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid database name or when the storage
    /// engine cannot run the checkpoint. Lock contention is returned as
    /// progress in [`CheckpointResult`].
    pub fn checkpoint_database(
        &self,
        database_name: Option<&str>,
        mode: CheckpointMode,
    ) -> Result<CheckpointResult> {
        self.connection.checkpoint_database(database_name, mode)
    }

    /// Sets the passive auto-checkpoint threshold for this connection.
    ///
    /// A threshold of zero disables automatic checkpoints.
    ///
    /// # Errors
    ///
    /// Returns an error when the threshold exceeds the supported range or the
    /// storage engine rejects the setting.
    pub fn set_wal_autocheckpoint(&self, frames: u32) -> Result<()> {
        self.connection.set_wal_autocheckpoint(frames)
    }

    /// Returns a thread-safe handle that can interrupt a running operation
    /// without borrowing the database.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.connection.interrupt_handle()
    }

    /// Asks the engine to release connection-local heap caches.
    ///
    /// # Errors
    ///
    /// Returns an error when the engine cannot release its caches.
    pub fn release_memory(&self) -> Result<()> {
        self.connection.release_memory()
    }

    /// Reads a single integer value.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails or does not produce exactly one
    /// convertible value.
    pub fn query_i64(&self, sql: &str) -> Result<i64> {
        self.connection.query_i64(sql)
    }

    /// Starts a deferred transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when a transaction cannot be started.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.connection.transaction()
    }

    /// Closes the connection and reports pending close errors.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying database cannot be closed.
    pub fn close(self) -> Result<()> {
        self.connection.close()
    }

    /// Returns the underlying SQL connection pointer for the C ABI crate.
    ///
    /// # Safety
    ///
    /// This is an internal integration boundary. The pointer must not outlive
    /// the database, must not be closed, and all uses must be serialized with
    /// safe operations on this database.
    #[doc(hidden)]
    pub unsafe fn as_raw_handle(&self) -> *mut c_void {
        // SAFETY: the caller accepts the documented lifetime and
        // synchronization requirements.
        unsafe { self.connection.as_raw_handle() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    struct TestDatabasePath {
        path: PathBuf,
    }

    impl TestDatabasePath {
        fn new(name: &str) -> Self {
            let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("csgdb-{name}-{}-{sequence}.db", std::process::id()));
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDatabasePath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let base = self.path.to_string_lossy();
            let _ = fs::remove_file(format!("{base}-wal"));
            let _ = fs::remove_file(format!("{base}-shm"));
        }
    }

    fn test_key(byte: u8) -> KeySource {
        KeySource::Raw(
            SecretKey::from_slice(&[byte; RAW_KEY_LENGTH]).expect("valid fixed-size key"),
        )
    }

    #[test]
    fn encrypted_database_round_trips_and_hides_plaintext_header() {
        let path = TestDatabasePath::new("encrypted");
        let database =
            Database::open_with_key(path.path(), test_key(7)).expect("open encrypted database");
        database
            .execute_batch(
                "CREATE TABLE memory(id INTEGER PRIMARY KEY, body TEXT NOT NULL);
                 INSERT INTO memory(body) VALUES ('remember me');",
            )
            .expect("write encrypted database");
        database.close().expect("close database");

        let mut file = fs::File::open(path.path()).expect("database file");
        let mut header = [0_u8; 16];
        file.read_exact(&mut header).expect("database header");
        assert_ne!(&header, csgdb_storage::plaintext_header());
        let contents = fs::read(path.path()).expect("encrypted database bytes");
        assert!(
            !contents
                .windows(b"remember me".len())
                .any(|window| window == b"remember me"),
            "known plaintext must not appear in the database file"
        );

        let database =
            Database::open_with_key(path.path(), test_key(7)).expect("reopen with correct key");
        assert_eq!(
            database
                .query_i64("SELECT count(*) FROM memory")
                .expect("read encrypted database"),
            1
        );
    }

    #[test]
    fn incorrect_key_is_rejected() {
        let path = TestDatabasePath::new("wrong-key");
        let database =
            Database::open_with_key(path.path(), test_key(3)).expect("create encrypted database");
        database
            .execute_batch("CREATE TABLE marker(value INTEGER);")
            .expect("initialize database");
        database.close().expect("close database");

        let error =
            Database::open_with_key(path.path(), test_key(4)).expect_err("incorrect key must fail");
        assert!(matches!(
            error.code(),
            ErrorCode::InvalidDatabaseKey | ErrorCode::DatabaseCorrupt
        ));
    }

    #[test]
    fn plaintext_is_explicit_and_sqlite_compatible() {
        let path = TestDatabasePath::new("plaintext");
        let database = Database::open_plaintext(path.path()).expect("open plaintext database");
        assert_eq!(database.security(), SecurityMode::Plaintext);
        database
            .execute_batch("CREATE TABLE ordinary(value INTEGER);")
            .expect("write plaintext database");
        database.close().expect("close database");

        let mut file = fs::File::open(path.path()).expect("database file");
        let mut header = [0_u8; 16];
        file.read_exact(&mut header).expect("database header");
        assert_eq!(&header, csgdb_storage::plaintext_header());
    }

    #[test]
    fn transaction_commit_and_rollback_are_observable() {
        let path = TestDatabasePath::new("transactions");
        let mut database =
            Database::open_with_passphrase(path.path(), "agent-secret").expect("open database");
        database
            .execute_batch("CREATE TABLE event(id INTEGER PRIMARY KEY);")
            .expect("create table");

        {
            let transaction = database.transaction().expect("begin transaction");
            transaction
                .execute_batch("INSERT INTO event DEFAULT VALUES;")
                .expect("insert");
            transaction.rollback().expect("rollback");
        }
        assert_eq!(
            database
                .query_i64("SELECT count(*) FROM event")
                .expect("count after rollback"),
            0
        );

        {
            let transaction = database.transaction().expect("begin transaction");
            transaction
                .execute_batch("INSERT INTO event DEFAULT VALUES;")
                .expect("insert");
            transaction.commit().expect("commit");
        }
        assert_eq!(
            database
                .query_i64("SELECT count(*) FROM event")
                .expect("count after commit"),
            1
        );
        database.close().expect("close database");

        let reopened =
            Database::open_with_passphrase(path.path(), "agent-secret").expect("reopen database");
        assert_eq!(
            reopened
                .query_i64("SELECT count(*) FROM event")
                .expect("count after reopen"),
            1
        );
    }

    #[test]
    fn default_open_does_not_fall_back_to_plaintext() {
        let path = TestDatabasePath::new("default");
        let error = Database::open(path.path()).expect_err("missing provider must fail");
        assert_eq!(error.code(), ErrorCode::KeyStoreUnavailable);
        assert!(!path.path().exists());
    }

    #[test]
    fn prepared_statements_bind_and_stream_every_value_type() {
        let path = TestDatabasePath::new("prepared-values");
        let database =
            Database::open_with_key(path.path(), test_key(21)).expect("open encrypted database");
        database
            .execute_batch(
                "CREATE TABLE value_set(
                    id INTEGER PRIMARY KEY,
                    score REAL NOT NULL,
                    body TEXT NOT NULL,
                    payload BLOB NOT NULL,
                    optional TEXT
                );",
            )
            .expect("create table");

        let payload = [0_u8, 1, 2, 255];
        assert_eq!(
            database
                .execute(
                    "INSERT INTO value_set(id, score, body, payload, optional)
                     VALUES (?, ?, ?, ?, ?)",
                    &[
                        ValueRef::Integer(42),
                        ValueRef::Real(3.25),
                        ValueRef::Text("agent memory"),
                        ValueRef::Blob(&payload),
                        ValueRef::Null,
                    ],
                )
                .expect("insert bound values"),
            1
        );

        let mut statement = database
            .prepare_cached(
                "SELECT id, score, body, payload, optional
                 FROM value_set WHERE id = ?",
            )
            .expect("prepare cached query");
        assert_eq!(statement.parameter_count(), 1);
        assert_eq!(statement.column_count(), 5);
        assert_eq!(statement.column_name(2).expect("column name"), "body");

        let mut rows = statement
            .query(&[ValueRef::Integer(42)])
            .expect("bind query");
        {
            let row = rows.next_row().expect("advance query").expect("one row");
            assert!(database.is_busy());
            assert_eq!(row.column_count(), 5);
            assert_eq!(row.get_i64(0).expect("integer"), 42);
            assert!((row.get_f64(1).expect("real") - 3.25).abs() < f64::EPSILON);
            assert_eq!(row.get_text(2).expect("text"), "agent memory");
            assert_eq!(row.get_blob(3).expect("blob"), payload);
            assert_eq!(row.value(4).expect("null"), Value::Null);
            assert_eq!(row.value_type(4).expect("null type"), ValueType::Null);
        }
        assert!(rows.next_row().expect("finish query").is_none());
        assert!(!database.is_busy());
    }

    #[test]
    fn prepared_statements_report_binding_and_column_errors() {
        let path = TestDatabasePath::new("prepared-errors");
        let database =
            Database::open_with_key(path.path(), test_key(22)).expect("open encrypted database");

        let mut statement = database.prepare("SELECT ?, ?").expect("prepare");
        let Err(error) = statement.query(&[ValueRef::Integer(1)]) else {
            panic!("parameter mismatch must fail");
        };
        assert_eq!(error.code(), ErrorCode::ParameterCountMismatch);

        let mut rows = statement
            .query(&[ValueRef::Integer(1), ValueRef::Text("two")])
            .expect("query");
        let row = rows.next_row().expect("advance").expect("one row");
        assert_eq!(
            row.get_text(0).expect_err("strict type mismatch").code(),
            ErrorCode::InvalidColumnType
        );
        assert_eq!(
            row.value(2).expect_err("column range").code(),
            ErrorCode::InvalidColumnIndex
        );
    }

    #[test]
    fn invalid_utf8_text_is_rejected_without_affecting_blob_reads() {
        let path = TestDatabasePath::new("invalid-utf8");
        let database =
            Database::open_with_key(path.path(), test_key(23)).expect("open encrypted database");
        let mut statement = database
            .prepare("SELECT CAST(x'80ff' AS TEXT), x'80ff'")
            .expect("prepare");
        let mut rows = statement.query(&[]).expect("query");
        let row = rows.next_row().expect("advance").expect("one row");

        assert_eq!(
            row.get_text(0).expect_err("invalid UTF-8").code(),
            ErrorCode::InvalidUtf8
        );
        assert_eq!(row.get_blob(1).expect("blob remains binary"), [0x80, 0xff]);
    }

    #[test]
    fn transaction_supports_bound_statements() {
        let path = TestDatabasePath::new("transaction-bind");
        let mut database =
            Database::open_with_key(path.path(), test_key(24)).expect("open encrypted database");
        database
            .execute_batch("CREATE TABLE item(value TEXT NOT NULL);")
            .expect("create table");

        let transaction = database.transaction().expect("begin transaction");
        assert_eq!(
            transaction
                .execute(
                    "INSERT INTO item(value) VALUES (?)",
                    &[ValueRef::Text("committed")],
                )
                .expect("bound insert"),
            1
        );
        transaction.commit().expect("commit");
        assert_eq!(
            database
                .query_i64("SELECT count(*) FROM item")
                .expect("count"),
            1
        );
    }

    #[test]
    fn connection_observability_tracks_writes_and_transaction_state() {
        let path = TestDatabasePath::new("connection-state");
        let mut database =
            Database::open_with_key(path.path(), test_key(31)).expect("open encrypted database");
        database
            .execute_batch("CREATE TABLE event(id INTEGER PRIMARY KEY, body TEXT NOT NULL);")
            .expect("create table");

        assert!(database.is_autocommit());
        assert!(!database.is_busy());
        assert!(!database.is_interrupted());
        assert!(!database.is_readonly("main").expect("main database mode"));
        assert_eq!(
            database.transaction_state(None).expect("transaction state"),
            TransactionState::None
        );

        assert_eq!(
            database
                .execute(
                    "INSERT INTO event(body) VALUES (?)",
                    &[ValueRef::Text("first")],
                )
                .expect("insert"),
            1
        );
        assert_eq!(database.changes(), 1);
        assert_eq!(database.total_changes(), 1);
        assert_eq!(database.last_insert_rowid(), 1);

        {
            let transaction = database.transaction().expect("begin transaction");
            assert_eq!(
                transaction
                    .transaction_state(None)
                    .expect("initial transaction state"),
                TransactionState::None
            );
            assert_eq!(
                transaction
                    .query_i64("SELECT count(*) FROM event")
                    .expect("read in transaction"),
                1
            );
            assert_eq!(
                transaction
                    .transaction_state(None)
                    .expect("read transaction state"),
                TransactionState::Read
            );
            transaction
                .execute(
                    "INSERT INTO event(body) VALUES (?)",
                    &[ValueRef::Text("second")],
                )
                .expect("write in transaction");
            assert_eq!(
                transaction
                    .transaction_state(None)
                    .expect("write transaction state"),
                TransactionState::Write
            );
            transaction.commit().expect("commit");
        }

        assert!(database.is_autocommit());
        assert_eq!(database.total_changes(), 2);
        assert_eq!(
            database.transaction_state(None).expect("transaction state"),
            TransactionState::None
        );
        database
            .set_busy_timeout(Duration::from_millis(25))
            .expect("set busy timeout");
        database.release_memory().expect("release caches");

        let error = database
            .set_busy_timeout(Duration::from_millis(2_147_483_648))
            .expect_err("oversized timeout must fail without panicking");
        assert_eq!(error.code(), ErrorCode::InvalidBusyTimeout);

        database.close().expect("close writable database");
        let readonly = Database::builder(path.path())
            .key(test_key(31))
            .flags(OpenFlags::READONLY | OpenFlags::ENCRYPTED | OpenFlags::FULLMUTEX)
            .open()
            .expect("open read-only database");
        assert!(readonly.is_readonly("main").expect("read-only state"));
    }

    #[test]
    fn interrupt_handle_cancels_a_long_query_and_survives_close() {
        let path = TestDatabasePath::new("interrupt");
        let database =
            Database::open_with_key(path.path(), test_key(32)).expect("open encrypted database");
        let interrupt = database.interrupt_handle();
        let post_close_interrupt = database.interrupt_handle();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = Arc::clone(&started);

        let worker = std::thread::spawn(move || {
            worker_started.store(true, Ordering::Release);
            let result = database.query_i64(
                "WITH RECURSIVE counter(value) AS (
                    VALUES(0)
                    UNION ALL
                    SELECT value + 1 FROM counter WHERE value < 50000000
                 )
                 SELECT sum(value) FROM counter",
            );
            (database, result)
        });

        while !started.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        let interrupter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            interrupt.interrupt();
        });

        let (database, result) = worker.join().expect("query worker");
        interrupter.join().expect("interrupt worker");
        let error = result.expect_err("long query must be interrupted");
        assert_eq!(error.code(), ErrorCode::QueryInterrupted);
        assert_eq!(
            database.query_i64("SELECT 1").expect("connection recovers"),
            1
        );
        database.close().expect("close database");

        // The detached handle is deliberately safe after connection close.
        post_close_interrupt.interrupt();
    }
}
