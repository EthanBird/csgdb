//! Public Rust API for CSGDB.
//!
//! The M1 foundation exposes secure database opening, SQL execution, and
//! transactions over the bundled encrypted storage kernel.

pub use csgdb_core::{
    prepare_open, DatabaseIdentity, Error, ErrorCode, KeyProvider, KeySource, OpenFlags,
    OpenOptions, OpenPlan, ResolvedKeyRef, ResolvedOpenPlan, Result, SecretKey, SecretString,
    SecurityMode, ABI_VERSION, LIB_VERSION, LIB_VERSION_NUMBER, RAW_KEY_LENGTH, SOURCE_ID,
};
pub use csgdb_storage::Transaction;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
        let connection = csgdb_storage::Connection::open(&plan)?;
        Ok(Database { connection })
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

    /// Executes one SQL statement without bound parameters.
    ///
    /// # Errors
    ///
    /// Returns an error when the statement fails or returns rows.
    pub fn execute(&self, sql: &str) -> Result<usize> {
        self.connection.execute(sql)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

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
}
