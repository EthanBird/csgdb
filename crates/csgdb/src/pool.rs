use crate::{
    Database, DatabaseBuilder, Error, ErrorCode, KeyProvider, KeySource, OpenFlags, OpenOptions,
    ResolvedOpenPlan, Result, SecretString, SecurityMode, Statement, Transaction, TransactionState,
    Value,
};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Default number of read-only connections retained by a managed pool.
pub const DEFAULT_READ_CONNECTIONS: usize = 2;
/// Default number of waiting jobs accepted by the single-writer queue.
pub const DEFAULT_WRITE_QUEUE_CAPACITY: usize = 64;
/// Hard safety limit for read-only connections in one process.
pub const MAX_READ_CONNECTIONS: usize = 64;
/// Hard safety limit for waiting write jobs.
pub const MAX_WRITE_QUEUE_CAPACITY: usize = 65_536;

thread_local! {
    static IN_WRITE_WORKER: Cell<bool> = const { Cell::new(false) };
    static READ_CALLBACK_POOLS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Resource limits for [`DatabasePool`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PoolOptions {
    read_connections: usize,
    write_queue_capacity: usize,
}

impl PoolOptions {
    #[must_use]
    pub const fn new(read_connections: usize, write_queue_capacity: usize) -> Self {
        Self {
            read_connections,
            write_queue_capacity,
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
        let writer_thread = thread::Builder::new()
            .name("csgdb-writer".to_owned())
            .spawn(move || writer_main(writer, &worker_queue, &worker_metrics))
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
        if in_write_worker() || in_read_callback(&self.inner) {
            return Err(Error::new(
                ErrorCode::ReentrantWrite,
                "a database callback cannot synchronously submit a nested write",
            ));
        }

        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let metrics = Arc::clone(&self.inner.metrics);
        let job = Box::new(move |database: &mut Database| {
            let response = match catch_unwind(AssertUnwindSafe(|| operation(database))) {
                Ok(result) => result,
                Err(_) => Err(Error::new(
                    ErrorCode::WriteTaskPanicked,
                    "a database writer callback panicked",
                )),
            };
            metrics.writer_active.store(false, Ordering::Release);
            metrics.write_jobs_completed.fetch_add(1, Ordering::Relaxed);
            let _ = response_tx.send(response);
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
        let sql = sql.into();
        self.write(move |database| {
            let parameters = parameters.iter().map(Value::as_ref).collect::<Vec<_>>();
            database.execute(&sql, &parameters)
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

type WriteJob = Box<dyn FnOnce(&mut Database) + Send + 'static>;

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

    fn pop(&self) -> Option<WriteJob> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(job) = state.jobs.pop_front() {
                self.not_full.notify_one();
                return Some(job);
            }
            if !state.accepting {
                return None;
            }
            state = self
                .not_empty
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
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
}

fn writer_main(mut database: Database, queue: &WriteQueue, metrics: &PoolMetrics) -> Result<()> {
    IN_WRITE_WORKER.with(|state| state.set(true));
    while let Some(job) = queue.pop() {
        metrics.writer_active.store(true, Ordering::Release);
        job(&mut database);
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
