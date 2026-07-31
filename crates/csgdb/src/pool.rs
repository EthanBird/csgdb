use crate::{
    CheckpointMode, CheckpointResult, Database, DatabaseBuilder, Error, ErrorCode, KeyProvider,
    KeySource, OpenFlags, OpenOptions, ResolvedOpenPlan, Result, SecretString, SecurityMode,
    Statement, Transaction, TransactionState, Value,
};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Default number of read-only connections retained by a managed pool.
pub const DEFAULT_READ_CONNECTIONS: usize = 2;
/// Default number of waiting jobs accepted by the single-writer queue.
pub const DEFAULT_WRITE_QUEUE_CAPACITY: usize = 64;
/// Default maximum number of logical jobs in one physical group commit.
pub const DEFAULT_GROUP_COMMIT_MAX_JOBS: usize = 8;
/// Default time spent waiting for adjacent groupable jobs.
pub const DEFAULT_GROUP_COMMIT_DELAY: Duration = Duration::from_micros(250);
/// Default number of writer commits between managed passive checkpoints.
pub const DEFAULT_MAINTENANCE_INTERVAL_COMMITS: u64 = 32;
/// Default soft limit for WAL frames that a checkpoint cannot reclaim.
pub const DEFAULT_WAL_SOFT_LIMIT_FRAMES: u32 = 4_096;
/// Hard safety limit for read-only connections in one process.
pub const MAX_READ_CONNECTIONS: usize = 64;
/// Hard safety limit for waiting write jobs.
pub const MAX_WRITE_QUEUE_CAPACITY: usize = 65_536;
/// Hard safety limit for logical jobs in one group commit.
pub const MAX_GROUP_COMMIT_JOBS: usize = 256;
/// Hard safety limit for the group-commit collection delay.
pub const MAX_GROUP_COMMIT_DELAY: Duration = Duration::from_millis(20);
/// Hard safety limit for managed-checkpoint commit intervals.
pub const MAX_MAINTENANCE_INTERVAL_COMMITS: u64 = 1_000_000;

thread_local! {
    static IN_WRITE_WORKER: Cell<bool> = const { Cell::new(false) };
    static READ_CALLBACK_POOLS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Bounds controlling how adjacent parameterized writes share a commit.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GroupCommitOptions {
    max_jobs: usize,
    max_delay: Duration,
}

impl GroupCommitOptions {
    #[must_use]
    pub const fn new(max_jobs: usize, max_delay: Duration) -> Self {
        Self {
            max_jobs,
            max_delay,
        }
    }

    #[must_use]
    pub const fn max_jobs(self) -> usize {
        self.max_jobs
    }

    #[must_use]
    pub const fn max_delay(self) -> Duration {
        self.max_delay
    }
}

impl Default for GroupCommitOptions {
    fn default() -> Self {
        Self::new(DEFAULT_GROUP_COMMIT_MAX_JOBS, DEFAULT_GROUP_COMMIT_DELAY)
    }
}

/// Bounded policy for connection-manager-owned passive WAL maintenance.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WalMaintenanceOptions {
    enabled: bool,
    checkpoint_interval_commits: u64,
    wal_soft_limit_frames: u32,
}

impl WalMaintenanceOptions {
    #[must_use]
    pub const fn new(checkpoint_interval_commits: u64, wal_soft_limit_frames: u32) -> Self {
        Self {
            enabled: true,
            checkpoint_interval_commits,
            wal_soft_limit_frames,
        }
    }

    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            checkpoint_interval_commits: DEFAULT_MAINTENANCE_INTERVAL_COMMITS,
            wal_soft_limit_frames: DEFAULT_WAL_SOFT_LIMIT_FRAMES,
        }
    }

    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn checkpoint_interval_commits(self) -> u64 {
        self.checkpoint_interval_commits
    }

    #[must_use]
    pub const fn wal_soft_limit_frames(self) -> u32 {
        self.wal_soft_limit_frames
    }
}

impl Default for WalMaintenanceOptions {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAINTENANCE_INTERVAL_COMMITS,
            DEFAULT_WAL_SOFT_LIMIT_FRAMES,
        )
    }
}

/// Resource and maintenance limits for [`DatabasePool`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PoolOptions {
    read_connections: usize,
    write_queue_capacity: usize,
    group_commit: GroupCommitOptions,
    wal_maintenance: WalMaintenanceOptions,
}

impl PoolOptions {
    #[must_use]
    pub const fn new(read_connections: usize, write_queue_capacity: usize) -> Self {
        Self {
            read_connections,
            write_queue_capacity,
            group_commit: GroupCommitOptions {
                max_jobs: DEFAULT_GROUP_COMMIT_MAX_JOBS,
                max_delay: DEFAULT_GROUP_COMMIT_DELAY,
            },
            wal_maintenance: WalMaintenanceOptions {
                enabled: true,
                checkpoint_interval_commits: DEFAULT_MAINTENANCE_INTERVAL_COMMITS,
                wal_soft_limit_frames: DEFAULT_WAL_SOFT_LIMIT_FRAMES,
            },
        }
    }

    #[must_use]
    pub const fn read_connections(self) -> usize {
        self.read_connections
    }

    #[must_use]
    pub const fn write_queue_capacity(self) -> usize {
        self.write_queue_capacity
    }

    #[must_use]
    pub const fn group_commit(self) -> GroupCommitOptions {
        self.group_commit
    }

    #[must_use]
    pub const fn wal_maintenance(self) -> WalMaintenanceOptions {
        self.wal_maintenance
    }

    #[must_use]
    pub const fn with_group_commit(mut self, options: GroupCommitOptions) -> Self {
        self.group_commit = options;
        self
    }

    #[must_use]
    pub const fn with_wal_maintenance(mut self, options: WalMaintenanceOptions) -> Self {
        self.wal_maintenance = options;
        self
    }
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self::new(DEFAULT_READ_CONNECTIONS, DEFAULT_WRITE_QUEUE_CAPACITY)
    }
}

/// Behavior when the single-writer queue has reached its configured limit.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WriteBackpressure {
    /// Wait until the queue has room.
    Wait,
    /// Return [`ErrorCode::WriteQueueFull`] immediately.
    Reject,
    /// Wait for at most the supplied duration.
    Timeout(Duration),
}

/// One owned parameterized statement in an atomic write batch.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchStatement {
    sql: String,
    parameters: Vec<Value>,
}

impl BatchStatement {
    #[must_use]
    pub fn new(sql: impl Into<String>, parameters: Vec<Value>) -> Self {
        Self {
            sql: sql.into(),
            parameters,
        }
    }

    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    #[must_use]
    pub fn parameters(&self) -> &[Value] {
        &self.parameters
    }
}

/// A point-in-time view of connection-manager activity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PoolStats {
    pub reader_capacity: usize,
    pub available_readers: usize,
    pub active_readers: usize,
    pub rejected_read_checkouts: u64,
    pub write_queue_capacity: usize,
    pub queued_writes: usize,
    pub writer_active: bool,
    pub write_jobs_submitted: u64,
    pub write_jobs_completed: u64,
    pub write_jobs_rejected: u64,
    pub group_commit_transactions: u64,
    pub grouped_write_jobs: u64,
    pub largest_group_commit: usize,
    pub checkpoint_runs: u64,
    pub automatic_checkpoint_runs: u64,
    pub incomplete_checkpoints: u64,
    pub maintenance_failures: u64,
    pub wal_pressure_events: u64,
    pub wal_pressure_active: bool,
    pub automatic_maintenance_enabled: bool,
    pub last_observed_wal_frames: u32,
    pub last_remaining_wal_frames: u32,
}

/// Builder for an encrypted or explicit-plaintext [`DatabasePool`].
pub struct DatabasePoolBuilder {
    database: DatabaseBuilder,
    pool_options: PoolOptions,
}

impl DatabasePoolBuilder {
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            database: DatabaseBuilder::new(path),
            pool_options: PoolOptions::default(),
        }
    }

    #[must_use]
    pub fn key(mut self, key: KeySource) -> Self {
        self.database = self.database.key(key);
        self
    }

    #[must_use]
    pub fn key_provider(mut self, provider: Arc<dyn KeyProvider>) -> Self {
        self.database = self.database.key_provider(provider);
        self
    }

    #[must_use]
    pub fn plaintext(mut self) -> Self {
        self.database = self.database.plaintext();
        self
    }

    #[must_use]
    pub fn flags(mut self, flags: OpenFlags) -> Self {
        self.database = self.database.flags(flags);
        self
    }

    #[must_use]
    pub fn open_options(mut self, options: OpenOptions) -> Self {
        self.database = self.database.options(options);
        self
    }

    #[must_use]
    pub const fn pool_options(mut self, options: PoolOptions) -> Self {
        self.pool_options = options;
        self
    }

    #[must_use]
    pub const fn read_connections(mut self, count: usize) -> Self {
        self.pool_options.read_connections = count;
        self
    }

    #[must_use]
    pub const fn write_queue_capacity(mut self, capacity: usize) -> Self {
        self.pool_options.write_queue_capacity = capacity;
        self
    }

    #[must_use]
    pub const fn group_commit(mut self, options: GroupCommitOptions) -> Self {
        self.pool_options.group_commit = options;
        self
    }

    #[must_use]
    pub const fn wal_maintenance(mut self, options: WalMaintenanceOptions) -> Self {
        self.pool_options.wal_maintenance = options;
        self
    }

    /// Opens the writer first, then opens the configured read-only
    /// connections against the same resolved key.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, unsupported memory databases,
    /// read-only open flags, key resolution failures, connection failures, or
    /// writer-thread startup failures.
    pub fn open(self) -> Result<DatabasePool> {
        let plan = self.database.plan()?;
        DatabasePool::open_resolved(&plan, self.pool_options)
    }
}

/// A bounded read pool paired with a single serialized writer.
///
/// Cloning this value shares the same connections, queue, and metrics.
#[derive(Clone)]
pub struct DatabasePool {
    inner: Arc<PoolInner>,
}

impl fmt::Debug for DatabasePool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabasePool")
            .field("security", &self.security())
            .field("stats", &self.stats())
            .finish()
    }
}

impl DatabasePool {
    #[must_use]
    pub fn builder(path: impl AsRef<Path>) -> DatabasePoolBuilder {
        DatabasePoolBuilder::new(path)
    }

    /// Opens a pool using the default encrypted policy.
    ///
    /// This requires a configured key provider and never falls back to
    /// plaintext.
    ///
    /// # Errors
    ///
    /// Returns an error when no key provider is configured, pool limits are
    /// invalid, or opening fails.
    pub fn open(path: impl AsRef<Path>, options: PoolOptions) -> Result<Self> {
        Self::builder(path).pool_options(options).open()
    }

    /// Opens an encrypted pool using explicit key material.
    ///
    /// # Errors
    ///
    /// Returns an error when the key, pool configuration, or database open
    /// operation is invalid.
    pub fn open_with_key(
        path: impl AsRef<Path>,
        key: KeySource,
        options: PoolOptions,
    ) -> Result<Self> {
        Self::builder(path).key(key).pool_options(options).open()
    }

    /// Opens an encrypted pool with a passphrase.
    ///
    /// # Errors
    ///
    /// Returns an error when the pool configuration, passphrase, or database
    /// open operation is invalid.
    pub fn open_with_passphrase(
        path: impl AsRef<Path>,
        passphrase: impl AsRef<[u8]>,
        options: PoolOptions,
    ) -> Result<Self> {
        Self::builder(path)
            .key(KeySource::Passphrase(SecretString::new(passphrase)))
            .pool_options(options)
            .open()
    }

    /// Explicitly opens a plaintext-compatible pool.
    ///
    /// # Errors
    ///
    /// Returns an error when the pool configuration or database open
    /// operation is invalid.
    pub fn open_plaintext(path: impl AsRef<Path>, options: PoolOptions) -> Result<Self> {
        Self::builder(path).plaintext().pool_options(options).open()
    }

    fn open_resolved(plan: &ResolvedOpenPlan, options: PoolOptions) -> Result<Self> {
        validate_pool_options(plan, options)?;

        let writer = Database::open_resolved(plan, false)?;
        if options.wal_maintenance.enabled {
            writer.set_wal_autocheckpoint(0)?;
        }
        let writer_interrupt = writer.interrupt_handle();
        let mut readers = Vec::with_capacity(options.read_connections);
        for _ in 0..options.read_connections {
            readers.push(Database::open_resolved(plan, true)?);
        }

        let readers = Arc::new(ReadPool::new(readers));
        let write_queue = Arc::new(WriteQueue::new(options.write_queue_capacity));
        let metrics = Arc::new(PoolMetrics::default());
        let worker_queue = Arc::clone(&write_queue);
        let worker_metrics = Arc::clone(&metrics);
        metrics
            .automatic_maintenance_enabled
            .store(options.wal_maintenance.enabled, Ordering::Release);
        let writer_thread = thread::Builder::new()
            .name("csgdb-writer".to_owned())
            .spawn(move || writer_main(writer, &worker_queue, &worker_metrics, options))
            .map_err(|_| {
                Error::new(
                    ErrorCode::StorageBackendUnavailable,
                    "database writer thread could not be started",
                )
            })?;

        Ok(Self {
            inner: Arc::new(PoolInner {
                readers,
                write_queue,
                metrics,
                writer_interrupt,
                writer_thread: Mutex::new(Some(writer_thread)),
                security: plan.security(),
            }),
        })
    }

    #[must_use]
    pub fn security(&self) -> SecurityMode {
        self.inner.security
    }

    /// Runs a read operation, waiting until a pooled connection is available.
    ///
    /// The callback cannot return values borrowing the read connection.
    ///
    /// # Errors
    ///
    /// Returns an error from the callback.
    pub fn read<R>(
        &self,
        operation: impl FnOnce(&mut ReadConnection<'_>) -> Result<R>,
    ) -> Result<R> {
        let _callback_guard = ReadCallbackGuard::enter(&self.inner)?;
        let mut lease = self.inner.readers.checkout();
        let mut connection = ReadConnection::new(lease.database_mut());
        operation(&mut connection)
    }

    /// Runs a read operation only when a connection is immediately available.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ReadPoolExhausted`] when every reader is checked
    /// out, or returns an error from the callback.
    pub fn try_read<R>(
        &self,
        operation: impl FnOnce(&mut ReadConnection<'_>) -> Result<R>,
    ) -> Result<R> {
        let _callback_guard = ReadCallbackGuard::enter(&self.inner)?;
        let mut lease = self.inner.readers.try_checkout()?;
        let mut connection = ReadConnection::new(lease.database_mut());
        operation(&mut connection)
    }

    /// Reads one integer using a pooled read-only connection.
    ///
    /// # Errors
    ///
    /// Returns an error when checkout or query execution fails.
    pub fn query_i64(&self, sql: &str) -> Result<i64> {
        self.read(|connection| connection.query_i64(sql))
    }

    /// Submits a write callback and waits for both queue admission and result.
    ///
    /// # Errors
    ///
    /// Returns an error from the callback, when the manager is closed, when a
    /// callback panics, or when called recursively from the writer thread.
    pub fn write<R, F>(&self, operation: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Database) -> Result<R> + Send + 'static,
    {
        self.write_with_policy(WriteBackpressure::Wait, operation)
    }

    /// Submits a write callback only when queue capacity is immediately
    /// available.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::WriteQueueFull`] when the queue is full, or any
    /// error described by [`DatabasePool::write`].
    pub fn try_write<R, F>(&self, operation: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Database) -> Result<R> + Send + 'static,
    {
        self.write_with_policy(WriteBackpressure::Reject, operation)
    }

    /// Submits a write callback with an explicit queue backpressure policy.
    ///
    /// # Errors
    ///
    /// Returns `WriteQueueFull`, `WriteQueueTimeout`,
    /// `ConnectionManagerClosed`, `WriteTaskPanicked`, `ReentrantWrite`, or an
    /// error produced by the callback.
    pub fn write_with_policy<R, F>(&self, policy: WriteBackpressure, operation: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Database) -> Result<R> + Send + 'static,
    {
        self.write_internal(policy, true, move |database, _runtime| operation(database))
    }

    fn write_internal<R, F>(
        &self,
        policy: WriteBackpressure,
        counts_as_write: bool,
        operation: F,
    ) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Database, &mut WriterRuntime) -> Result<R> + Send + 'static,
    {
        if in_write_worker() || in_read_callback(&self.inner) {
            return Err(Error::new(
                ErrorCode::ReentrantWrite,
                "a database callback cannot synchronously submit a nested write",
            ));
        }

        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let job = WriteJob::Exclusive {
            operation: Box::new(
                move |database: &mut Database, runtime: &mut WriterRuntime| {
                    let response =
                        match catch_unwind(AssertUnwindSafe(|| operation(database, runtime))) {
                            Ok(result) => result,
                            Err(_) => Err(Error::new(
                                ErrorCode::WriteTaskPanicked,
                                "a database writer callback panicked",
                            )),
                        };
                    Box::new(move || {
                        let _ = response_tx.send(response);
                    })
                },
            ),
            counts_as_write,
        };

        if let Err(error) = self.inner.write_queue.push(job, policy) {
            if matches!(
                error.code(),
                ErrorCode::WriteQueueFull | ErrorCode::WriteQueueTimeout
            ) {
                self.inner
                    .metrics
                    .write_jobs_rejected
                    .fetch_add(1, Ordering::Relaxed);
            }
            return Err(error);
        }
        self.inner
            .metrics
            .write_jobs_submitted
            .fetch_add(1, Ordering::Relaxed);
        self.inner.write_queue.notify_writer();

        response_rx.recv().map_err(|_| {
            Error::new(
                ErrorCode::ConnectionManagerClosed,
                "database writer stopped before returning a result",
            )
        })?
    }

    /// Executes one or more SQL statements on the writer.
    ///
    /// # Errors
    ///
    /// Returns an error when queueing or SQL execution fails.
    pub fn execute_batch(&self, sql: impl Into<String>) -> Result<()> {
        let sql = sql.into();
        self.write(move |database| database.execute_batch(&sql))
    }

    /// Executes one parameterized SQL statement on the writer.
    ///
    /// Owned values are required because a queued write can outlive the
    /// submitting stack frame.
    ///
    /// # Errors
    ///
    /// Returns an error when queueing, binding, or execution fails.
    pub fn execute(&self, sql: impl Into<String>, parameters: Vec<Value>) -> Result<usize> {
        let changes = self.submit_groupable(
            vec![BatchStatement::new(sql, parameters)],
            WriteBackpressure::Wait,
        )?;
        Ok(changes.into_iter().next().unwrap_or(0))
    }

    /// Executes owned parameterized statements in one atomic transaction.
    ///
    /// The returned vector contains the changed-row count for each statement
    /// in input order. Any preparation, binding, execution, or commit error
    /// rolls back the entire batch.
    ///
    /// # Errors
    ///
    /// Returns an error when queueing, transaction management, binding, or
    /// statement execution fails.
    pub fn execute_transaction(
        &self,
        statements: impl IntoIterator<Item = BatchStatement>,
    ) -> Result<Vec<usize>> {
        let statements = statements.into_iter().collect::<Vec<_>>();
        self.submit_groupable(statements, WriteBackpressure::Wait)
    }

    fn submit_groupable(
        &self,
        statements: Vec<BatchStatement>,
        policy: WriteBackpressure,
    ) -> Result<Vec<usize>> {
        if in_write_worker() || in_read_callback(&self.inner) {
            return Err(Error::new(
                ErrorCode::ReentrantWrite,
                "a database callback cannot synchronously submit a nested write",
            ));
        }

        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let job = WriteJob::Groupable(GroupableWrite {
            statements,
            response: response_tx,
        });
        if let Err(error) = self.inner.write_queue.push(job, policy) {
            if matches!(
                error.code(),
                ErrorCode::WriteQueueFull | ErrorCode::WriteQueueTimeout
            ) {
                self.inner
                    .metrics
                    .write_jobs_rejected
                    .fetch_add(1, Ordering::Relaxed);
            }
            return Err(error);
        }
        self.inner
            .metrics
            .write_jobs_submitted
            .fetch_add(1, Ordering::Relaxed);
        self.inner.write_queue.notify_writer();

        response_rx.recv().map_err(|_| {
            Error::new(
                ErrorCode::ConnectionManagerClosed,
                "database writer stopped before returning a result",
            )
        })?
    }

    /// Runs a WAL checkpoint on the writer connection.
    ///
    /// # Errors
    ///
    /// Returns an error when queueing or checkpoint execution fails.
    pub fn checkpoint(&self, mode: CheckpointMode) -> Result<CheckpointResult> {
        self.checkpoint_database(Some("main"), mode)
    }

    /// Runs a WAL checkpoint on one attached database, or every attached
    /// database when `database_name` is absent.
    ///
    /// # Errors
    ///
    /// Returns an error when queueing, database-name validation, or
    /// checkpoint execution fails.
    pub fn checkpoint_database(
        &self,
        database_name: Option<&str>,
        mode: CheckpointMode,
    ) -> Result<CheckpointResult> {
        let database_name = database_name.map(str::to_owned);
        let metrics = Arc::clone(&self.inner.metrics);
        self.write_internal(WriteBackpressure::Wait, false, move |database, runtime| {
            let result = database.checkpoint_database(database_name.as_deref(), mode)?;
            runtime.record_checkpoint(&metrics, result, false);
            Ok(result)
        })
    }

    /// Sets the writer connection's passive auto-checkpoint threshold.
    ///
    /// A threshold of zero disables automatic checkpoints.
    ///
    /// # Errors
    ///
    /// Returns an error when queueing fails or the threshold is invalid.
    pub fn set_wal_autocheckpoint(&self, frames: u32) -> Result<()> {
        let metrics = Arc::clone(&self.inner.metrics);
        self.write_internal(WriteBackpressure::Wait, false, move |database, runtime| {
            database.set_wal_autocheckpoint(frames)?;
            runtime.disable_automatic_maintenance();
            metrics
                .automatic_maintenance_enabled
                .store(false, Ordering::Release);
            metrics.wal_pressure_active.store(false, Ordering::Release);
            Ok(())
        })
    }

    /// Replaces the manager-owned automatic WAL maintenance policy.
    ///
    /// Enabling this policy disables the engine-local auto-checkpoint and lets
    /// the writer run bounded PASSIVE checkpoints. A disabled policy leaves
    /// automatic checkpointing off until explicitly configured otherwise.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, queueing failures, or when the
    /// engine cannot disable its connection-local auto-checkpoint.
    pub fn set_automatic_wal_maintenance(&self, options: WalMaintenanceOptions) -> Result<()> {
        validate_wal_maintenance_options(options)?;
        let metrics = Arc::clone(&self.inner.metrics);
        self.write_internal(WriteBackpressure::Wait, false, move |database, runtime| {
            database.set_wal_autocheckpoint(0)?;
            runtime.set_automatic_maintenance(options);
            metrics
                .automatic_maintenance_enabled
                .store(options.enabled, Ordering::Release);
            metrics.wal_pressure_active.store(false, Ordering::Release);
            Ok(())
        })
    }

    /// Interrupts the operation currently running on the writer connection.
    pub fn interrupt_writer(&self) {
        self.inner.writer_interrupt.interrupt();
    }

    /// Closes every managed connection after all other pool clones have been
    /// dropped.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ConnectionManagerInUse`] when another pool clone
    /// is still live, or returns a storage close error.
    pub fn close(self) -> Result<()> {
        match Arc::try_unwrap(self.inner) {
            Ok(mut inner) => inner.shutdown(),
            Err(_) => Err(Error::new(
                ErrorCode::ConnectionManagerInUse,
                "database pool cannot close while shared handles remain",
            )),
        }
    }

    /// Returns current pool and writer-queue activity.
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        let (available_readers, active_readers) = self.inner.readers.counts();
        PoolStats {
            reader_capacity: self.inner.readers.capacity,
            available_readers,
            active_readers,
            rejected_read_checkouts: self
                .inner
                .readers
                .rejected_checkouts
                .load(Ordering::Relaxed),
            write_queue_capacity: self.inner.write_queue.capacity,
            queued_writes: self.inner.write_queue.len(),
            writer_active: self.inner.metrics.writer_active.load(Ordering::Acquire),
            write_jobs_submitted: self
                .inner
                .metrics
                .write_jobs_submitted
                .load(Ordering::Relaxed),
            write_jobs_completed: self
                .inner
                .metrics
                .write_jobs_completed
                .load(Ordering::Relaxed),
            write_jobs_rejected: self
                .inner
                .metrics
                .write_jobs_rejected
                .load(Ordering::Relaxed),
            group_commit_transactions: self
                .inner
                .metrics
                .group_commit_transactions
                .load(Ordering::Relaxed),
            grouped_write_jobs: self
                .inner
                .metrics
                .grouped_write_jobs
                .load(Ordering::Relaxed),
            largest_group_commit: self
                .inner
                .metrics
                .largest_group_commit
                .load(Ordering::Relaxed),
            checkpoint_runs: self.inner.metrics.checkpoint_runs.load(Ordering::Relaxed),
            automatic_checkpoint_runs: self
                .inner
                .metrics
                .automatic_checkpoint_runs
                .load(Ordering::Relaxed),
            incomplete_checkpoints: self
                .inner
                .metrics
                .incomplete_checkpoints
                .load(Ordering::Relaxed),
            maintenance_failures: self
                .inner
                .metrics
                .maintenance_failures
                .load(Ordering::Relaxed),
            wal_pressure_events: self
                .inner
                .metrics
                .wal_pressure_events
                .load(Ordering::Relaxed),
            wal_pressure_active: self
                .inner
                .metrics
                .wal_pressure_active
                .load(Ordering::Acquire),
            automatic_maintenance_enabled: self
                .inner
                .metrics
                .automatic_maintenance_enabled
                .load(Ordering::Acquire),
            last_observed_wal_frames: self
                .inner
                .metrics
                .last_observed_wal_frames
                .load(Ordering::Relaxed),
            last_remaining_wal_frames: self
                .inner
                .metrics
                .last_remaining_wal_frames
                .load(Ordering::Relaxed),
        }
    }
}

/// Restricted view of a pooled connection opened in read-only mode.
pub struct ReadConnection<'connection> {
    database: &'connection mut Database,
}

impl<'connection> ReadConnection<'connection> {
    fn new(database: &'connection mut Database) -> Self {
        Self { database }
    }

    #[must_use]
    pub fn security(&self) -> SecurityMode {
        self.database.security()
    }

    /// Reads a single integer value.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    pub fn query_i64(&self, sql: &str) -> Result<i64> {
        self.database.query_i64(sql)
    }

    /// Compiles a statement on the read-only connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the SQL cannot be compiled.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        self.database.prepare(sql)
    }

    /// Compiles a statement through the connection-local LRU cache.
    ///
    /// # Errors
    ///
    /// Returns an error when the SQL cannot be compiled.
    pub fn prepare_cached(&self, sql: &str) -> Result<Statement<'_>> {
        self.database.prepare_cached(sql)
    }

    /// Starts a read transaction that holds one snapshot across statements.
    ///
    /// # Errors
    ///
    /// Returns an error when the transaction cannot start.
    pub fn transaction(&mut self) -> Result<ReadTransaction<'_>> {
        self.database
            .transaction()
            .map(|inner| ReadTransaction { inner })
    }

    /// Returns whether the named database is read-only.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown database name.
    pub fn is_readonly(&self, database_name: &str) -> Result<bool> {
        self.database.is_readonly(database_name)
    }

    /// Returns current transaction activity.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be inspected.
    pub fn transaction_state(&self, database_name: Option<&str>) -> Result<TransactionState> {
        self.database.transaction_state(database_name)
    }

    /// Releases connection-local recyclable caches.
    ///
    /// # Errors
    ///
    /// Returns an error when the engine cannot release memory.
    pub fn release_memory(&self) -> Result<()> {
        self.database.release_memory()
    }
}

/// A read-only snapshot transaction borrowed from a pooled reader.
pub struct ReadTransaction<'connection> {
    inner: Transaction<'connection>,
}

impl ReadTransaction<'_> {
    /// Reads one integer from the transaction snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when query execution fails.
    pub fn query_i64(&self, sql: &str) -> Result<i64> {
        self.inner.query_i64(sql)
    }

    /// Compiles a statement in the transaction snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the SQL cannot be compiled.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        self.inner.prepare(sql)
    }

    /// Returns current transaction activity.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be inspected.
    pub fn transaction_state(&self, database_name: Option<&str>) -> Result<TransactionState> {
        self.inner.transaction_state(database_name)
    }

    /// Commits the read transaction and releases its snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the transaction cannot be completed.
    pub fn commit(self) -> Result<()> {
        self.inner.commit()
    }
}

struct PoolInner {
    readers: Arc<ReadPool>,
    write_queue: Arc<WriteQueue>,
    metrics: Arc<PoolMetrics>,
    writer_interrupt: crate::InterruptHandle,
    writer_thread: Mutex<Option<JoinHandle<Result<()>>>>,
    security: SecurityMode,
}

struct ReadCallbackGuard {
    pool_id: usize,
}

impl ReadCallbackGuard {
    fn enter(pool: &Arc<PoolInner>) -> Result<Self> {
        let pool_id = Arc::as_ptr(pool) as usize;
        let inserted = READ_CALLBACK_POOLS.with(|pools| {
            let mut pools = pools.borrow_mut();
            if pools.contains(&pool_id) {
                false
            } else {
                pools.push(pool_id);
                true
            }
        });
        if inserted {
            Ok(Self { pool_id })
        } else {
            Err(Error::new(
                ErrorCode::ReentrantRead,
                "a read callback cannot synchronously check out from the same pool",
            ))
        }
    }
}

impl Drop for ReadCallbackGuard {
    fn drop(&mut self) {
        READ_CALLBACK_POOLS.with(|pools| {
            let mut pools = pools.borrow_mut();
            if let Some(index) = pools.iter().rposition(|pool_id| *pool_id == self.pool_id) {
                pools.remove(index);
            }
        });
    }
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl PoolInner {
    fn shutdown(&mut self) -> Result<()> {
        self.write_queue.close();
        let writer_thread = self
            .writer_thread
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let writer_result = if let Some(writer_thread) = writer_thread {
            if writer_thread.thread().id() == thread::current().id() {
                Ok(())
            } else {
                writer_thread.join().map_err(|_| {
                    Error::new(
                        ErrorCode::Storage,
                        "database writer thread stopped unexpectedly",
                    )
                })?
            }
        } else {
            Ok(())
        };
        let reader_result = self.readers.close_connections();
        writer_result.and(reader_result)
    }
}

struct ReadPool {
    capacity: usize,
    connections: Mutex<Vec<Database>>,
    available: Condvar,
    active: AtomicUsize,
    rejected_checkouts: AtomicU64,
}

impl ReadPool {
    fn new(connections: Vec<Database>) -> Self {
        Self {
            capacity: connections.len(),
            connections: Mutex::new(connections),
            available: Condvar::new(),
            active: AtomicUsize::new(0),
            rejected_checkouts: AtomicU64::new(0),
        }
    }

    fn checkout(self: &Arc<Self>) -> ReadLease {
        let mut connections = self
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(database) = connections.pop() {
                self.active.fetch_add(1, Ordering::Relaxed);
                return ReadLease {
                    pool: Arc::clone(self),
                    database: Some(database),
                };
            }
            connections = self
                .available
                .wait(connections)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn try_checkout(self: &Arc<Self>) -> Result<ReadLease> {
        let mut connections = self
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(database) = connections.pop() else {
            self.rejected_checkouts.fetch_add(1, Ordering::Relaxed);
            return Err(Error::new(
                ErrorCode::ReadPoolExhausted,
                "all read connections are currently checked out",
            ));
        };
        self.active.fetch_add(1, Ordering::Relaxed);
        Ok(ReadLease {
            pool: Arc::clone(self),
            database: Some(database),
        })
    }

    fn counts(&self) -> (usize, usize) {
        let available = self
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        (available, self.active.load(Ordering::Relaxed))
    }

    fn close_connections(&self) -> Result<()> {
        let mut connections = self
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut result = Ok(());
        for database in connections.drain(..) {
            if let Err(error) = database.close() {
                if result.is_ok() {
                    result = Err(error);
                }
            }
        }
        result
    }
}

struct ReadLease {
    pool: Arc<ReadPool>,
    database: Option<Database>,
}

impl ReadLease {
    fn database_mut(&mut self) -> &mut Database {
        self.database
            .as_mut()
            .expect("read lease owns a connection")
    }
}

impl Drop for ReadLease {
    fn drop(&mut self) {
        if let Some(database) = self.database.take() {
            let mut connections = self
                .pool
                .connections
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            connections.push(database);
            self.pool.active.fetch_sub(1, Ordering::Relaxed);
            self.pool.available.notify_one();
        }
    }
}

type WriteCompletion = Box<dyn FnOnce() + Send + 'static>;
type ExclusiveWrite =
    Box<dyn FnOnce(&mut Database, &mut WriterRuntime) -> WriteCompletion + Send + 'static>;
type GroupWriteResponse = (mpsc::SyncSender<Result<Vec<usize>>>, Result<Vec<usize>>);

struct GroupableWrite {
    statements: Vec<BatchStatement>,
    response: mpsc::SyncSender<Result<Vec<usize>>>,
}

struct GroupCommitCompletion {
    committed: bool,
    responses: Vec<GroupWriteResponse>,
}

impl GroupCommitCompletion {
    fn finish(self) {
        for (response, result) in self.responses {
            let _ = response.send(result);
        }
    }
}

enum WriteJob {
    Exclusive {
        operation: ExclusiveWrite,
        counts_as_write: bool,
    },
    Groupable(GroupableWrite),
}

enum DequeuedWrite {
    Exclusive {
        operation: ExclusiveWrite,
        counts_as_write: bool,
    },
    Group(Vec<GroupableWrite>),
}

struct WriteQueueState {
    jobs: VecDeque<WriteJob>,
    accepting: bool,
}

struct WriteQueue {
    capacity: usize,
    state: Mutex<WriteQueueState>,
    not_empty: Condvar,
    not_full: Condvar,
}

impl WriteQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(WriteQueueState {
                jobs: VecDeque::with_capacity(capacity),
                accepting: true,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
        }
    }

    fn push(&self, job: WriteJob, policy: WriteBackpressure) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match policy {
            WriteBackpressure::Wait => {
                while state.accepting && state.jobs.len() >= self.capacity {
                    state = self
                        .not_full
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
            WriteBackpressure::Reject => {
                if state.jobs.len() >= self.capacity {
                    return Err(write_queue_full());
                }
            }
            WriteBackpressure::Timeout(timeout) => {
                let started = Instant::now();
                let mut remaining = timeout;
                while state.accepting && state.jobs.len() >= self.capacity {
                    let (next_state, wait) = self
                        .not_full
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(PoisonError::into_inner);
                    state = next_state;
                    if state.jobs.len() < self.capacity {
                        break;
                    }
                    if wait.timed_out() || started.elapsed() >= timeout {
                        return Err(Error::new(
                            ErrorCode::WriteQueueTimeout,
                            "timed out waiting for write queue capacity",
                        ));
                    }
                    remaining = timeout.saturating_sub(started.elapsed());
                }
            }
        }

        if !state.accepting {
            return Err(connection_manager_closed());
        }
        state.jobs.push_back(job);
        Ok(())
    }

    fn notify_writer(&self) {
        self.not_empty.notify_one();
    }

    fn pop(&self, group_commit: GroupCommitOptions) -> Option<DequeuedWrite> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let first = loop {
            if let Some(job) = state.jobs.pop_front() {
                self.not_full.notify_all();
                break job;
            }
            if !state.accepting {
                return None;
            }
            state = self
                .not_empty
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        };

        match first {
            WriteJob::Exclusive {
                operation,
                counts_as_write,
            } => Some(DequeuedWrite::Exclusive {
                operation,
                counts_as_write,
            }),
            WriteJob::Groupable(first) => {
                let mut jobs = Vec::with_capacity(group_commit.max_jobs);
                jobs.push(first);
                let deadline = Instant::now()
                    .checked_add(group_commit.max_delay)
                    .unwrap_or_else(Instant::now);

                while jobs.len() < group_commit.max_jobs {
                    if matches!(state.jobs.front(), Some(WriteJob::Groupable(_))) {
                        let Some(WriteJob::Groupable(job)) = state.jobs.pop_front() else {
                            unreachable!("front was verified as groupable");
                        };
                        jobs.push(job);
                        self.not_full.notify_all();
                        continue;
                    }
                    if !state.jobs.is_empty() || !state.accepting {
                        break;
                    }

                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let (next_state, wait) = self
                        .not_empty
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(PoisonError::into_inner);
                    state = next_state;
                    if wait.timed_out() && state.jobs.is_empty() {
                        break;
                    }
                }
                Some(DequeuedWrite::Group(jobs))
            }
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.accepting = false;
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .jobs
            .len()
    }
}

#[derive(Default)]
struct PoolMetrics {
    writer_active: AtomicBool,
    write_jobs_submitted: AtomicU64,
    write_jobs_completed: AtomicU64,
    write_jobs_rejected: AtomicU64,
    group_commit_transactions: AtomicU64,
    grouped_write_jobs: AtomicU64,
    largest_group_commit: AtomicUsize,
    checkpoint_runs: AtomicU64,
    automatic_checkpoint_runs: AtomicU64,
    incomplete_checkpoints: AtomicU64,
    maintenance_failures: AtomicU64,
    wal_pressure_events: AtomicU64,
    wal_pressure_active: AtomicBool,
    automatic_maintenance_enabled: AtomicBool,
    last_observed_wal_frames: AtomicU32,
    last_remaining_wal_frames: AtomicU32,
}

struct WriterRuntime {
    maintenance: WalMaintenanceOptions,
    commits_since_maintenance: u64,
    wal_pressure: bool,
}

impl WriterRuntime {
    const fn new(maintenance: WalMaintenanceOptions) -> Self {
        Self {
            maintenance,
            commits_since_maintenance: 0,
            wal_pressure: false,
        }
    }

    fn disable_automatic_maintenance(&mut self) {
        self.maintenance.enabled = false;
        self.commits_since_maintenance = 0;
        self.wal_pressure = false;
    }

    fn set_automatic_maintenance(&mut self, options: WalMaintenanceOptions) {
        self.maintenance = options;
        self.commits_since_maintenance = 0;
        self.wal_pressure = false;
    }

    fn after_write(&mut self, database: &Database, metrics: &PoolMetrics) {
        if !self.maintenance.enabled {
            return;
        }
        self.commits_since_maintenance = self.commits_since_maintenance.saturating_add(1);
        if !self.wal_pressure
            && self.commits_since_maintenance < self.maintenance.checkpoint_interval_commits
        {
            return;
        }
        self.commits_since_maintenance = 0;

        match database.checkpoint(CheckpointMode::Passive) {
            Ok(result) => self.record_checkpoint(metrics, result, true),
            Err(_) => {
                metrics.maintenance_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn record_checkpoint(
        &mut self,
        metrics: &PoolMetrics,
        result: CheckpointResult,
        automatic: bool,
    ) {
        metrics.checkpoint_runs.fetch_add(1, Ordering::Relaxed);
        if automatic {
            metrics
                .automatic_checkpoint_runs
                .fetch_add(1, Ordering::Relaxed);
        }
        if !result.is_complete() {
            metrics
                .incomplete_checkpoints
                .fetch_add(1, Ordering::Relaxed);
        }

        let wal_frames = result.wal_frames().unwrap_or(0);
        let remaining_frames = result.remaining_frames().unwrap_or(0);
        metrics
            .last_observed_wal_frames
            .store(wal_frames, Ordering::Relaxed);
        metrics
            .last_remaining_wal_frames
            .store(remaining_frames, Ordering::Relaxed);

        let pressure = remaining_frames > self.maintenance.wal_soft_limit_frames;
        if pressure && !self.wal_pressure {
            metrics.wal_pressure_events.fetch_add(1, Ordering::Relaxed);
        }
        self.wal_pressure = pressure;
        metrics
            .wal_pressure_active
            .store(pressure, Ordering::Release);
    }
}

fn execute_group_commit(
    database: &mut Database,
    jobs: Vec<GroupableWrite>,
    metrics: &PoolMetrics,
) -> GroupCommitCompletion {
    let job_count = jobs.len();
    let job_count_u64 = u64::try_from(job_count).unwrap_or(u64::MAX);
    metrics
        .group_commit_transactions
        .fetch_add(1, Ordering::Relaxed);
    metrics
        .largest_group_commit
        .fetch_max(job_count, Ordering::Relaxed);
    if job_count > 1 {
        metrics
            .grouped_write_jobs
            .fetch_add(job_count_u64, Ordering::Relaxed);
    }

    let transaction = match database.transaction() {
        Ok(transaction) => transaction,
        Err(error) => {
            metrics
                .write_jobs_completed
                .fetch_add(job_count_u64, Ordering::Relaxed);
            return GroupCommitCompletion {
                committed: false,
                responses: jobs
                    .into_iter()
                    .map(|job| (job.response, Err(error.clone())))
                    .collect(),
            };
        }
    };

    let mut responses = Vec::with_capacity(job_count);
    let mut fatal_error: Option<Error> = None;
    for job in jobs {
        let result = if let Some(error) = &fatal_error {
            Err(error.clone())
        } else {
            match execute_group_job(&transaction, &job.statements) {
                Ok(result) => result,
                Err(error) => {
                    fatal_error = Some(error.clone());
                    Err(error)
                }
            }
        };
        responses.push((job.response, result));
    }

    let committed = if let Some(error) = fatal_error {
        drop(transaction);
        for (_, result) in &mut responses {
            *result = Err(error.clone());
        }
        false
    } else {
        match transaction.commit() {
            Ok(()) => responses.iter().any(|(_, result)| result.is_ok()),
            Err(error) => {
                for (_, result) in &mut responses {
                    if result.is_ok() {
                        *result = Err(error.clone());
                    }
                }
                false
            }
        }
    };

    metrics
        .write_jobs_completed
        .fetch_add(job_count_u64, Ordering::Relaxed);
    GroupCommitCompletion {
        committed,
        responses,
    }
}

fn execute_group_job(
    transaction: &Transaction<'_>,
    statements: &[BatchStatement],
) -> std::result::Result<Result<Vec<usize>>, Error> {
    const SAVEPOINT: &str = "SAVEPOINT csgdb_group_commit_job";
    const RELEASE: &str = "RELEASE csgdb_group_commit_job";
    const ROLLBACK: &str = "ROLLBACK TO csgdb_group_commit_job; RELEASE csgdb_group_commit_job";

    transaction.execute_batch(SAVEPOINT)?;
    let mut changes = Vec::with_capacity(statements.len());
    for statement in statements {
        let parameters = statement
            .parameters
            .iter()
            .map(Value::as_ref)
            .collect::<Vec<_>>();
        match transaction.execute(&statement.sql, &parameters) {
            Ok(statement_changes) => changes.push(statement_changes),
            Err(error) => {
                transaction.execute_batch(ROLLBACK)?;
                return Ok(Err(error));
            }
        }
    }
    transaction.execute_batch(RELEASE)?;
    Ok(Ok(changes))
}

fn writer_main(
    mut database: Database,
    queue: &WriteQueue,
    metrics: &PoolMetrics,
    options: PoolOptions,
) -> Result<()> {
    IN_WRITE_WORKER.with(|state| state.set(true));
    let mut runtime = WriterRuntime::new(options.wal_maintenance);
    while let Some(job) = queue.pop(options.group_commit) {
        metrics.writer_active.store(true, Ordering::Release);
        match job {
            DequeuedWrite::Exclusive {
                operation,
                counts_as_write,
            } => {
                let completion = operation(&mut database, &mut runtime);
                if counts_as_write {
                    runtime.after_write(&database, metrics);
                }
                metrics.write_jobs_completed.fetch_add(1, Ordering::Relaxed);
                metrics.writer_active.store(false, Ordering::Release);
                completion();
            }
            DequeuedWrite::Group(jobs) => {
                let completion = execute_group_commit(&mut database, jobs, metrics);
                if completion.committed {
                    runtime.after_write(&database, metrics);
                }
                metrics.writer_active.store(false, Ordering::Release);
                completion.finish();
            }
        }
    }
    IN_WRITE_WORKER.with(|state| state.set(false));
    database.close()
}

fn in_write_worker() -> bool {
    IN_WRITE_WORKER.with(Cell::get)
}

fn in_read_callback(pool: &Arc<PoolInner>) -> bool {
    let pool_id = Arc::as_ptr(pool) as usize;
    READ_CALLBACK_POOLS.with(|pools| pools.borrow().contains(&pool_id))
}

fn validate_pool_options(plan: &ResolvedOpenPlan, options: PoolOptions) -> Result<()> {
    if options.read_connections == 0 || options.read_connections > MAX_READ_CONNECTIONS {
        return Err(Error::new(
            ErrorCode::InvalidPoolConfiguration,
            "read connection count is outside the supported range",
        ));
    }
    if options.write_queue_capacity == 0 || options.write_queue_capacity > MAX_WRITE_QUEUE_CAPACITY
    {
        return Err(Error::new(
            ErrorCode::InvalidPoolConfiguration,
            "write queue capacity is outside the supported range",
        ));
    }
    validate_group_commit_options(options.group_commit)?;
    validate_wal_maintenance_options(options.wal_maintenance)?;
    if plan.flags().contains(OpenFlags::READONLY) {
        return Err(Error::new(
            ErrorCode::InvalidPoolConfiguration,
            "a managed pool requires a writable primary connection",
        ));
    }
    if plan.flags().contains(OpenFlags::MEMORY) {
        return Err(Error::new(
            ErrorCode::InvalidPoolConfiguration,
            "managed pools do not support isolated memory databases",
        ));
    }
    Ok(())
}

fn validate_group_commit_options(options: GroupCommitOptions) -> Result<()> {
    if options.max_jobs == 0 || options.max_jobs > MAX_GROUP_COMMIT_JOBS {
        return Err(Error::new(
            ErrorCode::InvalidPoolConfiguration,
            "group commit job limit is outside the supported range",
        ));
    }
    if options.max_delay > MAX_GROUP_COMMIT_DELAY {
        return Err(Error::new(
            ErrorCode::InvalidPoolConfiguration,
            "group commit delay exceeds the supported range",
        ));
    }
    Ok(())
}

fn validate_wal_maintenance_options(options: WalMaintenanceOptions) -> Result<()> {
    if options.checkpoint_interval_commits == 0
        || options.checkpoint_interval_commits > MAX_MAINTENANCE_INTERVAL_COMMITS
    {
        return Err(Error::new(
            ErrorCode::InvalidPoolConfiguration,
            "WAL maintenance commit interval is outside the supported range",
        ));
    }
    if options.wal_soft_limit_frames == 0
        || options.wal_soft_limit_frames > crate::MAX_WAL_AUTOCHECKPOINT_FRAMES
    {
        return Err(Error::new(
            ErrorCode::InvalidPoolConfiguration,
            "WAL soft frame limit is outside the supported range",
        ));
    }
    Ok(())
}

fn write_queue_full() -> Error {
    Error::new(
        ErrorCode::WriteQueueFull,
        "write queue has reached its configured capacity",
    )
}

fn connection_manager_closed() -> Error {
    Error::new(
        ErrorCode::ConnectionManagerClosed,
        "database connection manager is closed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SecretKey, ValueRef, RAW_KEY_LENGTH};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
    use std::sync::{Barrier, OnceLock};

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    struct TestDatabasePath {
        path: PathBuf,
    }

    impl TestDatabasePath {
        fn new(name: &str) -> Self {
            let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "csgdb-pool-{name}-{}-{sequence}.db",
                std::process::id()
            ));
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

    fn test_pool(path: &Path, key: u8, readers: usize, write_capacity: usize) -> DatabasePool {
        DatabasePool::builder(path)
            .key(test_key(key))
            .read_connections(readers)
            .write_queue_capacity(write_capacity)
            .open()
            .expect("open database pool")
    }

    #[test]
    fn managed_pool_serializes_writes_and_reports_activity() {
        let path = TestDatabasePath::new("serialized-writes");
        let pool = test_pool(path.path(), 41, 2, 8);
        pool.execute_batch(
            "CREATE TABLE event(
                sequence INTEGER PRIMARY KEY,
                worker INTEGER NOT NULL
            );",
        )
        .expect("create table");

        let active_writers = Arc::new(AtomicUsize::new(0));
        let maximum_writers = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for worker_id in 0_i64..8 {
            let worker_pool = pool.clone();
            let active_writers = Arc::clone(&active_writers);
            let maximum_writers = Arc::clone(&maximum_writers);
            workers.push(thread::spawn(move || {
                worker_pool.write(move |database| {
                    let active = active_writers.fetch_add(1, Ordering::AcqRel) + 1;
                    maximum_writers.fetch_max(active, Ordering::AcqRel);
                    thread::sleep(Duration::from_millis(2));
                    let result = database.execute(
                        "INSERT INTO event(worker) VALUES (?)",
                        &[ValueRef::Integer(worker_id)],
                    );
                    active_writers.fetch_sub(1, Ordering::AcqRel);
                    result
                })
            }));
        }

        for worker in workers {
            assert_eq!(worker.join().expect("writer thread").expect("write"), 1);
        }
        assert_eq!(maximum_writers.load(Ordering::Acquire), 1);
        assert_eq!(
            pool.query_i64("SELECT count(*) FROM event")
                .expect("count rows"),
            8
        );

        let stats = pool.stats();
        assert_eq!(stats.reader_capacity, 2);
        assert_eq!(stats.available_readers, 2);
        assert_eq!(stats.active_readers, 0);
        assert_eq!(stats.queued_writes, 0);
        assert!(!stats.writer_active);
        assert_eq!(stats.write_jobs_submitted, 9);
        assert_eq!(stats.write_jobs_completed, 9);
        assert_eq!(stats.write_jobs_rejected, 0);
    }

    #[test]
    fn read_pool_is_bounded_and_returns_connections_after_use() {
        let path = TestDatabasePath::new("bounded-readers");
        let pool = test_pool(path.path(), 42, 1, 4);
        pool.execute_batch("CREATE TABLE marker(value INTEGER NOT NULL);")
            .expect("create table");

        let phases = Arc::new(Barrier::new(2));
        let worker_phases = Arc::clone(&phases);
        let worker_pool = pool.clone();
        let worker = thread::spawn(move || {
            worker_pool.read(|connection| {
                assert!(connection.is_readonly("main")?);
                worker_phases.wait();
                worker_phases.wait();
                connection.query_i64("SELECT count(*) FROM marker")
            })
        });

        phases.wait();
        let stats = pool.stats();
        assert_eq!(stats.available_readers, 0);
        assert_eq!(stats.active_readers, 1);

        let error = pool
            .try_read(|connection| connection.query_i64("SELECT 1"))
            .expect_err("second checkout must be rejected");
        assert_eq!(error.code(), ErrorCode::ReadPoolExhausted);
        phases.wait();
        assert_eq!(worker.join().expect("reader thread").expect("read"), 0);

        let stats = pool.stats();
        assert_eq!(stats.available_readers, 1);
        assert_eq!(stats.active_readers, 0);
        assert_eq!(stats.rejected_read_checkouts, 1);

        let error = pool
            .read(|connection| {
                let mut statement = connection.prepare("INSERT INTO marker(value) VALUES (1)")?;
                statement.execute(&[])
            })
            .expect_err("pooled readers must reject writes at the engine");
        assert_eq!(error.code(), ErrorCode::DatabaseReadOnly);
    }

    #[test]
    fn read_transaction_keeps_a_snapshot_across_a_write() {
        let path = TestDatabasePath::new("snapshot");
        let pool = test_pool(path.path(), 43, 1, 4);
        pool.execute_batch(
            "CREATE TABLE state(value INTEGER NOT NULL);
             INSERT INTO state(value) VALUES (1);",
        )
        .expect("initialize state");

        let (snapshot_ready_tx, snapshot_ready_rx) = mpsc::sync_channel(0);
        let (continue_tx, continue_rx) = mpsc::sync_channel(0);
        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            reader_pool.read(|connection| {
                let transaction = connection.transaction()?;
                let before = transaction.query_i64("SELECT value FROM state")?;
                snapshot_ready_tx.send(()).expect("signal snapshot");
                continue_rx.recv().expect("continue snapshot");
                let after = transaction.query_i64("SELECT value FROM state")?;
                transaction.commit()?;
                Ok((before, after))
            })
        });

        snapshot_ready_rx.recv().expect("snapshot ready");
        pool.execute_batch("UPDATE state SET value = 2;")
            .expect("update state");
        continue_tx.send(()).expect("continue reader");

        assert_eq!(
            reader.join().expect("reader thread").expect("snapshot"),
            (1, 1)
        );
        assert_eq!(
            pool.query_i64("SELECT value FROM state")
                .expect("latest value"),
            2
        );
    }

    #[test]
    fn parameterized_transaction_batches_commit_or_rollback_atomically() {
        let path = TestDatabasePath::new("transaction-batch");
        let pool = test_pool(path.path(), 51, 1, 4);
        pool.execute_batch(
            "CREATE TABLE memory(
                id INTEGER PRIMARY KEY,
                body TEXT NOT NULL UNIQUE
            );",
        )
        .expect("create table");

        let changes = pool
            .execute_transaction([
                BatchStatement::new(
                    "INSERT INTO memory(id, body) VALUES (?, ?)",
                    vec![Value::Integer(1), Value::from("first")],
                ),
                BatchStatement::new(
                    "INSERT INTO memory(id, body) VALUES (?, ?)",
                    vec![Value::Integer(2), Value::from("second")],
                ),
            ])
            .expect("commit batch");
        assert_eq!(changes, [1, 1]);

        let error = pool
            .execute_transaction([
                BatchStatement::new(
                    "INSERT INTO memory(id, body) VALUES (?, ?)",
                    vec![Value::Integer(3), Value::from("third")],
                ),
                BatchStatement::new(
                    "INSERT INTO memory(id, body) VALUES (?, ?)",
                    vec![Value::Integer(4), Value::from("second")],
                ),
            ])
            .expect_err("constraint failure must reject the batch");
        assert_eq!(error.code(), ErrorCode::ConstraintViolation);
        assert_eq!(
            pool.query_i64("SELECT count(*) FROM memory")
                .expect("count committed rows"),
            2
        );
        assert_eq!(
            pool.query_i64("SELECT count(*) FROM memory WHERE id = 3")
                .expect("verify rollback"),
            0
        );
    }

    #[test]
    fn adjacent_parameterized_writes_share_one_bounded_commit() {
        let path = TestDatabasePath::new("group-commit");
        let pool = DatabasePool::builder(path.path())
            .key(test_key(53))
            .read_connections(1)
            .write_queue_capacity(8)
            .group_commit(GroupCommitOptions::new(2, Duration::from_millis(20)))
            .wal_maintenance(WalMaintenanceOptions::disabled())
            .open()
            .expect("open group commit pool");
        pool.execute_batch("CREATE TABLE event(value INTEGER NOT NULL UNIQUE);")
            .expect("create table");

        let (blocker_started_tx, blocker_started_rx) = mpsc::sync_channel(0);
        let (release_blocker_tx, release_blocker_rx) = mpsc::sync_channel(0);
        let blocker_pool = pool.clone();
        let blocker = thread::spawn(move || {
            blocker_pool.write(move |_| {
                blocker_started_tx.send(()).expect("signal blocker");
                release_blocker_rx.recv().expect("release blocker");
                Ok(())
            })
        });
        blocker_started_rx.recv().expect("blocker started");

        let mut writers = Vec::new();
        for value in 0_i64..6 {
            let writer_pool = pool.clone();
            writers.push(thread::spawn(move || {
                writer_pool.execute(
                    "INSERT INTO event(value) VALUES (?)",
                    vec![Value::Integer(value)],
                )
            }));
        }
        wait_until(|| pool.stats().queued_writes == 6);
        release_blocker_tx.send(()).expect("release blocker");
        blocker.join().expect("blocker thread").expect("blocker");
        for writer in writers {
            assert_eq!(writer.join().expect("writer thread").expect("insert"), 1);
        }
        wait_until(|| !pool.stats().writer_active);

        assert_eq!(
            pool.query_i64("SELECT count(*) FROM event")
                .expect("count rows"),
            6
        );
        let stats = pool.stats();
        assert_eq!(stats.group_commit_transactions, 3);
        assert_eq!(stats.grouped_write_jobs, 6);
        assert_eq!(stats.largest_group_commit, 2);
    }

    #[test]
    fn group_commit_savepoints_isolate_one_logical_failure() {
        let path = TestDatabasePath::new("group-commit-isolation");
        let pool = DatabasePool::builder(path.path())
            .key(test_key(54))
            .read_connections(1)
            .write_queue_capacity(4)
            .group_commit(GroupCommitOptions::new(2, Duration::from_millis(20)))
            .wal_maintenance(WalMaintenanceOptions::disabled())
            .open()
            .expect("open group commit pool");
        pool.execute_batch(
            "CREATE TABLE memory(body TEXT NOT NULL UNIQUE);
             INSERT INTO memory(body) VALUES ('existing');",
        )
        .expect("initialize table");

        let (blocker_started_tx, blocker_started_rx) = mpsc::sync_channel(0);
        let (release_blocker_tx, release_blocker_rx) = mpsc::sync_channel(0);
        let blocker_pool = pool.clone();
        let blocker = thread::spawn(move || {
            blocker_pool.write(move |_| {
                blocker_started_tx.send(()).expect("signal blocker");
                release_blocker_rx.recv().expect("release blocker");
                Ok(())
            })
        });
        blocker_started_rx.recv().expect("blocker started");

        let successful_pool = pool.clone();
        let successful = thread::spawn(move || {
            successful_pool.execute(
                "INSERT INTO memory(body) VALUES (?)",
                vec![Value::from("committed")],
            )
        });
        let failing_pool = pool.clone();
        let failing = thread::spawn(move || {
            failing_pool.execute(
                "INSERT INTO memory(body) VALUES (?)",
                vec![Value::from("existing")],
            )
        });
        wait_until(|| pool.stats().queued_writes == 2);
        release_blocker_tx.send(()).expect("release blocker");
        blocker.join().expect("blocker thread").expect("blocker");

        assert_eq!(
            successful
                .join()
                .expect("successful thread")
                .expect("successful insert"),
            1
        );
        let error = failing
            .join()
            .expect("failing thread")
            .expect_err("duplicate insert must fail");
        assert_eq!(error.code(), ErrorCode::ConstraintViolation);
        assert_eq!(
            pool.query_i64("SELECT count(*) FROM memory")
                .expect("count isolated results"),
            2
        );

        let stats = pool.stats();
        assert_eq!(stats.group_commit_transactions, 1);
        assert_eq!(stats.grouped_write_jobs, 2);
        assert_eq!(stats.largest_group_commit, 2);
    }

    #[test]
    fn managed_passive_checkpoints_observe_and_clear_wal_pressure() {
        let path = TestDatabasePath::new("managed-wal");
        let pool = DatabasePool::builder(path.path())
            .key(test_key(55))
            .read_connections(1)
            .write_queue_capacity(4)
            .group_commit(GroupCommitOptions::new(1, Duration::ZERO))
            .wal_maintenance(WalMaintenanceOptions::new(1, 1))
            .open()
            .expect("open managed WAL pool");
        pool.execute_batch(
            "CREATE TABLE event(
                id INTEGER PRIMARY KEY,
                body TEXT NOT NULL
            );
            INSERT INTO event(body) VALUES ('initial');",
        )
        .expect("initialize database");
        wait_until(|| pool.stats().automatic_checkpoint_runs >= 1);
        let baseline_checkpoints = pool.stats().automatic_checkpoint_runs;

        let (snapshot_ready_tx, snapshot_ready_rx) = mpsc::sync_channel(0);
        let (release_snapshot_tx, release_snapshot_rx) = mpsc::sync_channel(0);
        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            reader_pool.read(|connection| {
                let transaction = connection.transaction()?;
                let visible_rows = transaction.query_i64("SELECT count(*) FROM event")?;
                snapshot_ready_tx.send(()).expect("signal snapshot");
                release_snapshot_rx.recv().expect("release snapshot");
                transaction.commit()?;
                Ok(visible_rows)
            })
        });
        snapshot_ready_rx.recv().expect("snapshot ready");

        let statements = (0_i64..128).map(|id| {
            BatchStatement::new(
                "INSERT INTO event(body) VALUES (?)",
                vec![Value::Text(format!("{id:04}-{}", "x".repeat(1_024)))],
            )
        });
        pool.execute_transaction(statements)
            .expect("write behind snapshot");
        wait_until(|| {
            let stats = pool.stats();
            stats.automatic_checkpoint_runs > baseline_checkpoints
                && stats.wal_pressure_active
                && stats.last_remaining_wal_frames > 1
        });
        let pressured = pool.stats();
        assert!(pressured.wal_pressure_events >= 1);

        release_snapshot_tx.send(()).expect("release snapshot");
        assert_eq!(reader.join().expect("reader thread").expect("snapshot"), 1);
        pool.execute(
            "INSERT INTO event(body) VALUES (?)",
            vec![Value::from("after-snapshot")],
        )
        .expect("write after snapshot");
        wait_until(|| {
            let stats = pool.stats();
            stats.automatic_checkpoint_runs > pressured.automatic_checkpoint_runs
                && !stats.wal_pressure_active
                && stats.last_remaining_wal_frames == 0
        });

        pool.set_wal_autocheckpoint(0)
            .expect("switch off managed and engine checkpoints");
        let disabled_at = pool.stats().automatic_checkpoint_runs;
        pool.execute(
            "INSERT INTO event(body) VALUES (?)",
            vec![Value::from("maintenance-disabled")],
        )
        .expect("write with maintenance disabled");
        assert!(!pool.stats().automatic_maintenance_enabled);
        assert_eq!(pool.stats().automatic_checkpoint_runs, disabled_at);
    }

    #[test]
    fn checkpoint_reports_long_snapshot_interference_and_truncates_after_release() {
        let path = TestDatabasePath::new("checkpoint-snapshot");
        let pool = test_pool(path.path(), 52, 1, 8);
        pool.set_wal_autocheckpoint(0)
            .expect("disable auto-checkpoint");
        pool.execute_batch(
            "CREATE TABLE event(
                id INTEGER PRIMARY KEY,
                body TEXT NOT NULL
            );
            INSERT INTO event(body) VALUES ('initial');",
        )
        .expect("initialize database");
        for mode in [
            CheckpointMode::Passive,
            CheckpointMode::Full,
            CheckpointMode::Restart,
            CheckpointMode::Truncate,
        ] {
            assert!(
                pool.checkpoint(mode)
                    .expect("initial checkpoint")
                    .is_complete(),
                "{mode:?} checkpoint must complete without a reader"
            );
        }

        let (snapshot_ready_tx, snapshot_ready_rx) = mpsc::sync_channel(0);
        let (release_snapshot_tx, release_snapshot_rx) = mpsc::sync_channel(0);
        let reader_pool = pool.clone();
        let reader = thread::spawn(move || {
            reader_pool.read(|connection| {
                let transaction = connection.transaction()?;
                let visible_rows = transaction.query_i64("SELECT count(*) FROM event")?;
                snapshot_ready_tx.send(()).expect("signal snapshot");
                release_snapshot_rx.recv().expect("release snapshot");
                transaction.commit()?;
                Ok(visible_rows)
            })
        });
        snapshot_ready_rx.recv().expect("snapshot ready");

        let statements = (0_i64..128).map(|id| {
            BatchStatement::new(
                "INSERT INTO event(id, body) VALUES (?, ?)",
                vec![Value::Integer(id + 2), Value::Text(format!("event-{id}"))],
            )
        });
        let changes = pool
            .execute_transaction(statements)
            .expect("write behind snapshot");
        assert_eq!(changes.len(), 128);

        let passive = pool
            .checkpoint(CheckpointMode::Passive)
            .expect("passive checkpoint");
        assert!(!passive.is_complete());
        assert!(
            passive.remaining_frames().is_some_and(|frames| frames > 0),
            "long snapshot must leave observable WAL frames"
        );

        release_snapshot_tx.send(()).expect("release snapshot");
        assert_eq!(reader.join().expect("reader thread").expect("snapshot"), 1);

        let truncate = pool
            .checkpoint(CheckpointMode::Truncate)
            .expect("truncate checkpoint");
        assert!(truncate.is_complete());
        assert_eq!(truncate.remaining_frames(), Some(0));

        let stats = pool.stats();
        assert_eq!(stats.checkpoint_runs, 6);
        assert_eq!(stats.incomplete_checkpoints, 1);
    }

    #[test]
    fn write_queue_applies_reject_and_timeout_backpressure() {
        let path = TestDatabasePath::new("backpressure");
        let pool = test_pool(path.path(), 44, 1, 1);
        pool.execute_batch("CREATE TABLE event(value INTEGER NOT NULL);")
            .expect("create table");

        let (first_started_tx, first_started_rx) = mpsc::sync_channel(0);
        let (release_first_tx, release_first_rx) = mpsc::sync_channel(0);
        let first_pool = pool.clone();
        let first = thread::spawn(move || {
            first_pool.write(move |database| {
                first_started_tx.send(()).expect("signal first write");
                release_first_rx.recv().expect("release first write");
                database.execute(
                    "INSERT INTO event(value) VALUES (?)",
                    &[ValueRef::Integer(1)],
                )
            })
        });
        first_started_rx.recv().expect("first write started");

        let second_pool = pool.clone();
        let second = thread::spawn(move || {
            second_pool.write(|database| {
                database.execute(
                    "INSERT INTO event(value) VALUES (?)",
                    &[ValueRef::Integer(2)],
                )
            })
        });
        wait_until(|| pool.stats().queued_writes == 1);

        let error = pool
            .try_write(|_| Ok(()))
            .expect_err("full queue must reject");
        assert_eq!(error.code(), ErrorCode::WriteQueueFull);

        let error = pool
            .write_with_policy(
                WriteBackpressure::Timeout(Duration::from_millis(20)),
                |_| Ok(()),
            )
            .expect_err("full queue must time out");
        assert_eq!(error.code(), ErrorCode::WriteQueueTimeout);

        release_first_tx.send(()).expect("release first write");
        assert_eq!(first.join().expect("first thread").expect("first write"), 1);
        assert_eq!(
            second.join().expect("second thread").expect("second write"),
            1
        );
        assert_eq!(
            pool.query_i64("SELECT count(*) FROM event")
                .expect("count writes"),
            2
        );

        let stats = pool.stats();
        assert_eq!(stats.write_jobs_submitted, 3);
        assert_eq!(stats.write_jobs_completed, 3);
        assert_eq!(stats.write_jobs_rejected, 2);
    }

    #[test]
    fn writer_recovers_from_panics_and_rejects_recursive_submission() {
        let path = TestDatabasePath::new("writer-recovery");
        let pool = test_pool(path.path(), 45, 1, 4);

        let error = pool
            .write::<(), _>(|_| panic!("intentional writer callback panic"))
            .expect_err("panic must become a stable error");
        assert_eq!(error.code(), ErrorCode::WriteTaskPanicked);

        let recursive_pool = pool.clone();
        let error = pool
            .write(move |_| recursive_pool.write(|_| Ok(())))
            .expect_err("recursive write must fail instead of deadlocking");
        assert_eq!(error.code(), ErrorCode::ReentrantWrite);

        let recursive_pool = pool.clone();
        let error = pool
            .read(move |_| recursive_pool.read(|_| Ok(())))
            .expect_err("recursive read must fail instead of deadlocking");
        assert_eq!(error.code(), ErrorCode::ReentrantRead);

        let recursive_pool = pool.clone();
        let error = pool
            .read(move |_| recursive_pool.write(|_| Ok(())))
            .expect_err("a read callback must not wait for its own writer");
        assert_eq!(error.code(), ErrorCode::ReentrantWrite);

        pool.execute_batch(
            "CREATE TABLE recovered(value INTEGER);
             INSERT INTO recovered DEFAULT VALUES;",
        )
        .expect("writer remains usable");
        assert_eq!(
            pool.query_i64("SELECT count(*) FROM recovered")
                .expect("read recovered writer"),
            1
        );
    }

    #[test]
    fn writer_can_be_interrupted_without_stopping_the_manager() {
        let path = TestDatabasePath::new("writer-interrupt");
        let pool = test_pool(path.path(), 46, 1, 4);
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = Arc::clone(&started);
        let worker_pool = pool.clone();
        let worker = thread::spawn(move || {
            worker_pool.write(move |database| {
                worker_started.store(true, Ordering::Release);
                database.query_i64(
                    "WITH RECURSIVE counter(value) AS (
                        VALUES(0)
                        UNION ALL
                        SELECT value + 1 FROM counter WHERE value < 50000000
                     )
                     SELECT sum(value) FROM counter",
                )
            })
        });

        wait_until(|| started.load(Ordering::Acquire));
        thread::sleep(Duration::from_millis(25));
        pool.interrupt_writer();

        let error = worker
            .join()
            .expect("writer caller")
            .expect_err("long writer operation must be interrupted");
        assert_eq!(error.code(), ErrorCode::QueryInterrupted);
        assert_eq!(pool.write(|database| database.query_i64("SELECT 1")), Ok(1));
    }

    #[test]
    fn invalid_pool_limits_fail_before_opening_connections() {
        let options = PoolOptions::new(3, 12)
            .with_group_commit(GroupCommitOptions::new(5, Duration::from_micros(400)))
            .with_wal_maintenance(WalMaintenanceOptions::new(7, 512));
        assert_eq!(options.read_connections(), 3);
        assert_eq!(options.write_queue_capacity(), 12);
        assert_eq!(options.group_commit().max_jobs(), 5);
        assert_eq!(
            options.group_commit().max_delay(),
            Duration::from_micros(400)
        );
        assert_eq!(options.wal_maintenance().checkpoint_interval_commits(), 7);
        assert_eq!(options.wal_maintenance().wal_soft_limit_frames(), 512);

        let path = TestDatabasePath::new("invalid-limits");
        let error = DatabasePool::builder(path.path())
            .key(test_key(47))
            .read_connections(0)
            .open()
            .expect_err("zero readers must fail");
        assert_eq!(error.code(), ErrorCode::InvalidPoolConfiguration);
        assert!(!path.path().exists());

        let error = DatabasePool::builder(path.path())
            .key(test_key(47))
            .write_queue_capacity(0)
            .open()
            .expect_err("zero queue capacity must fail");
        assert_eq!(error.code(), ErrorCode::InvalidPoolConfiguration);
        assert!(!path.path().exists());

        let error = DatabasePool::builder(path.path())
            .key(test_key(47))
            .group_commit(GroupCommitOptions::new(0, Duration::ZERO))
            .open()
            .expect_err("zero group size must fail");
        assert_eq!(error.code(), ErrorCode::InvalidPoolConfiguration);
        assert!(!path.path().exists());

        let error = DatabasePool::builder(path.path())
            .key(test_key(47))
            .group_commit(GroupCommitOptions::new(
                2,
                MAX_GROUP_COMMIT_DELAY + Duration::from_nanos(1),
            ))
            .open()
            .expect_err("oversized group delay must fail");
        assert_eq!(error.code(), ErrorCode::InvalidPoolConfiguration);
        assert!(!path.path().exists());

        let error = DatabasePool::builder(path.path())
            .key(test_key(47))
            .wal_maintenance(WalMaintenanceOptions::new(0, 1))
            .open()
            .expect_err("zero maintenance interval must fail");
        assert_eq!(error.code(), ErrorCode::InvalidPoolConfiguration);
        assert!(!path.path().exists());

        let error = DatabasePool::builder(path.path())
            .key(test_key(47))
            .wal_maintenance(WalMaintenanceOptions::new(1, 0))
            .open()
            .expect_err("zero WAL soft limit must fail");
        assert_eq!(error.code(), ErrorCode::InvalidPoolConfiguration);
        assert!(!path.path().exists());
    }

    #[test]
    fn explicit_close_requires_unique_ownership() {
        let path = TestDatabasePath::new("explicit-close");
        let pool = test_pool(path.path(), 48, 1, 4);
        let shared = pool.clone();

        let error = shared
            .close()
            .expect_err("shared pool must not close active handles");
        assert_eq!(error.code(), ErrorCode::ConnectionManagerInUse);
        assert_eq!(pool.write(|database| database.query_i64("SELECT 1")), Ok(1));
        pool.close().expect("close unique pool");
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        static TIMEOUT: OnceLock<Duration> = OnceLock::new();
        let timeout = *TIMEOUT.get_or_init(|| Duration::from_secs(2));
        let started = Instant::now();
        while !condition() {
            assert!(started.elapsed() < timeout, "condition timed out");
            thread::yield_now();
        }
    }
}
