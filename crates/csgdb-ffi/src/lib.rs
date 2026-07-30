use csgdb_api::Result as CsgResult;
use csgdb_api::{
    Database, Error, ErrorCode, KeySource, OpenFlags, OpenOptions, SecretKey, SecretString,
};
use csgdb_core::{
    ABI_VERSION, DEFAULT_BUSY_TIMEOUT_MS, DEFAULT_CACHE_SIZE_BYTES, DEFAULT_MEMORY_BUDGET_BYTES,
    LIB_VERSION_NUMBER,
};
use libsqlite3_sys as sqlite;
use std::ffi::{c_char, c_void, CStr};
use std::os::raw::c_int;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static LIB_VERSION_C: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
static SOURCE_ID_C: &[u8] = concat!("csgdb-", env!("CARGO_PKG_VERSION"), "\0").as_bytes();

static MESSAGE_OK: &[u8] = b"not an error\0";
static MESSAGE_INVALID_ARGUMENT: &[u8] = b"invalid argument\0";
static MESSAGE_INVALID_OPEN_FLAGS: &[u8] = b"invalid open flags\0";
static MESSAGE_INVALID_KEY: &[u8] = b"invalid database key\0";
static MESSAGE_KEY_REQUIRED: &[u8] = b"database key required\0";
static MESSAGE_KEYSTORE_UNAVAILABLE: &[u8] = b"key store unavailable\0";
static MESSAGE_BUSY: &[u8] = b"database is busy or locked\0";
static MESSAGE_READONLY: &[u8] = b"database is read-only\0";
static MESSAGE_CONSTRAINT: &[u8] = b"database constraint failed\0";
static MESSAGE_CORRUPT: &[u8] = b"database is corrupt or authentication failed\0";
static MESSAGE_STORAGE: &[u8] = b"database operation failed\0";
static MESSAGE_MISUSE: &[u8] = b"statement is not in a valid state for this operation\0";
static MESSAGE_RANGE: &[u8] = b"parameter or column index is out of range\0";

pub const CSGDB_OK: i32 = 0;
pub const CSGDB_INVALID_ARGUMENT: i32 = 1;
pub const CSGDB_INVALID_OPEN_FLAGS: i32 = 2;
pub const CSGDB_INVALID_KEY: i32 = 3;
pub const CSGDB_KEY_REQUIRED: i32 = 4;
pub const CSGDB_KEYSTORE_UNAVAILABLE: i32 = 5;
pub const CSGDB_BUSY: i32 = 6;
pub const CSGDB_READONLY: i32 = 7;
pub const CSGDB_CONSTRAINT: i32 = 8;
pub const CSGDB_CORRUPT: i32 = 9;
pub const CSGDB_STORAGE: i32 = 10;
pub const CSGDB_MISUSE: i32 = 11;
pub const CSGDB_RANGE: i32 = 12;

pub const CSGDB_INTEGER: i32 = 1;
pub const CSGDB_FLOAT: i32 = 2;
pub const CSGDB_TEXT: i32 = 3;
pub const CSGDB_BLOB: i32 = 4;
pub const CSGDB_NULL: i32 = 5;
pub const CSGDB_ROW: i32 = 100;
pub const CSGDB_DONE: i32 = 101;

pub const CSGDB_PREPARE_PERSISTENT: u32 = 0x01;
const SQLITE_UTF8_ENCODING: u8 = 1;

const CSGDB_KEY_AUTO: u32 = 0;
const CSGDB_KEY_RAW: u32 = 1;
const CSGDB_KEY_PASSPHRASE: u32 = 2;
const CSGDB_KEY_PROVIDER: u32 = 3;

#[repr(C)]
pub struct csgdb_key_source {
    pub struct_size: u32,
    pub kind: u32,
    pub data: *const c_void,
    pub data_len: usize,
    pub provider_id: *const c_char,
}

impl Default for csgdb_key_source {
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(std::mem::size_of::<Self>()).unwrap_or(u32::MAX),
            kind: CSGDB_KEY_AUTO,
            data: ptr::null(),
            data_len: 0,
            provider_id: ptr::null(),
        }
    }
}

#[repr(C)]
pub struct csgdb_open_options {
    pub struct_size: u32,
    pub abi_version: u32,
    pub flags: u32,
    pub busy_timeout_ms: u32,
    pub cache_size_bytes: u64,
    pub memory_budget_bytes: u64,
    pub vfs: *const c_char,
    pub device_profile: *const c_char,
    pub key: csgdb_key_source,
}

impl Default for csgdb_open_options {
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(std::mem::size_of::<Self>()).unwrap_or(u32::MAX),
            abi_version: ABI_VERSION,
            flags: default_open_flags(),
            busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
            cache_size_bytes: DEFAULT_CACHE_SIZE_BYTES,
            memory_budget_bytes: DEFAULT_MEMORY_BUDGET_BYTES,
            vfs: ptr::null(),
            device_profile: ptr::null(),
            key: csgdb_key_source::default(),
        }
    }
}

struct SharedDatabase {
    database: Mutex<Database>,
    last_code: AtomicI32,
}

impl SharedDatabase {
    fn new(database: Database) -> Self {
        Self {
            database: Mutex::new(database),
            last_code: AtomicI32::new(CSGDB_OK),
        }
    }

    fn set_error(&self, error: &Error) -> i32 {
        let code = error_code(error);
        self.last_code.store(code, Ordering::Release);
        code
    }

    fn set_ok(&self) {
        self.last_code.store(CSGDB_OK, Ordering::Release);
    }

    fn set_code(&self, code: i32) -> i32 {
        self.last_code.store(code, Ordering::Release);
        code
    }
}

pub struct CsgdbHandle {
    database: Option<Arc<SharedDatabase>>,
    open_code: i32,
}

impl CsgdbHandle {
    fn new(database: Option<Database>, code: i32) -> Self {
        Self {
            database: database.map(|database| Arc::new(SharedDatabase::new(database))),
            open_code: code,
        }
    }

    fn current_code(&self) -> i32 {
        self.database.as_ref().map_or(self.open_code, |database| {
            database.last_code.load(Ordering::Acquire)
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatementState {
    Ready,
    Row,
    Done,
    Failed,
}

struct StatementInner {
    raw: *mut sqlite::sqlite3_stmt,
    state: StatementState,
}

// SAFETY: a statement is never accessed without both its own mutex and its
// owning connection mutex. `SharedDatabase` keeps the connection alive until
// the statement is finalized.
unsafe impl Send for StatementInner {}

pub struct CsgdbStatement {
    database: Arc<SharedDatabase>,
    inner: Mutex<StatementInner>,
}

impl Drop for CsgdbStatement {
    fn drop(&mut self) {
        let _database = self
            .database
            .database
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = self
            .inner
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !inner.raw.is_null() {
            // SAFETY: this type uniquely owns the raw statement. Drop runs
            // only after public access has ended and finalizes at most once.
            unsafe {
                sqlite::sqlite3_finalize(inner.raw);
            }
            inner.raw = ptr::null_mut();
        }
    }
}

const fn default_open_flags() -> u32 {
    OpenFlags::READWRITE.bits()
        | OpenFlags::CREATE.bits()
        | OpenFlags::ENCRYPTED.bits()
        | OpenFlags::FULLMUTEX.bits()
}

#[no_mangle]
pub extern "C" fn csgdb_libversion() -> *const c_char {
    LIB_VERSION_C.as_ptr().cast()
}

#[no_mangle]
pub const extern "C" fn csgdb_libversion_number() -> u32 {
    LIB_VERSION_NUMBER
}

#[no_mangle]
pub const extern "C" fn csgdb_abi_version() -> u32 {
    ABI_VERSION
}

#[no_mangle]
pub extern "C" fn csgdb_source_id() -> *const c_char {
    SOURCE_ID_C.as_ptr().cast()
}

#[no_mangle]
pub const extern "C" fn csgdb_default_open_flags() -> u32 {
    default_open_flags()
}

/// Initializes an options structure without reading fields from the caller.
///
/// # Safety
///
/// `out_options` must be null or point to writable, properly aligned memory
/// large enough for `csgdb_open_options`.
#[no_mangle]
pub unsafe extern "C" fn csgdb_open_options_init(out_options: *mut csgdb_open_options) -> i32 {
    if out_options.is_null() {
        return CSGDB_INVALID_ARGUMENT;
    }
    // SAFETY: the caller contract guarantees writable aligned storage.
    unsafe {
        out_options.write(csgdb_open_options::default());
    }
    CSGDB_OK
}

/// Opens a database with the default encrypted policy.
///
/// # Safety
///
/// `path` must point to a valid NUL-terminated UTF-8 string. `out_db` must
/// point to writable storage for one database handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_open(path: *const c_char, out_db: *mut *mut CsgdbHandle) -> i32 {
    // SAFETY: forwarded caller contract is unchanged.
    unsafe { open_from(path, out_db, |path| Database::open(path)) }
}

/// Opens a database using explicit flags.
///
/// # Safety
///
/// `path` must point to a valid NUL-terminated UTF-8 string. `out_db` must
/// point to writable storage for one database handle. `vfs` is reserved and
/// must currently be null.
#[no_mangle]
pub unsafe extern "C" fn csgdb_open_v2(
    path: *const c_char,
    out_db: *mut *mut CsgdbHandle,
    flags: u32,
    vfs: *const c_char,
) -> i32 {
    // SAFETY: output pointer validity is part of the caller contract.
    if !unsafe { clear_output(out_db) } {
        return CSGDB_INVALID_ARGUMENT;
    }
    if !vfs.is_null() {
        return CSGDB_INVALID_ARGUMENT;
    }
    // SAFETY: forwarded caller contract is unchanged.
    unsafe {
        open_from(path, out_db, |path| {
            let flags = OpenFlags::from_bits(flags).ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidOpenFlags,
                    "open flags contain unknown bits",
                )
            })?;
            Database::builder(path).flags(flags).open()
        })
    }
}

/// Opens an encrypted database with a 32-byte raw key.
///
/// # Safety
///
/// `path` and `out_db` follow [`csgdb_open`]. `key` must point to `key_len`
/// readable bytes for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn csgdb_open_with_key(
    path: *const c_char,
    key: *const c_void,
    key_len: usize,
    out_db: *mut *mut CsgdbHandle,
) -> i32 {
    // SAFETY: output pointer validity is part of the caller contract.
    if !unsafe { clear_output(out_db) } {
        return CSGDB_INVALID_ARGUMENT;
    }
    if key.is_null() {
        return CSGDB_INVALID_ARGUMENT;
    }
    // SAFETY: the caller guarantees `key_len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(key.cast::<u8>(), key_len) };
    let key = match SecretKey::from_slice(bytes) {
        Ok(key) => key,
        Err(error) => return error_code(&error),
    };
    // SAFETY: forwarded path and output contracts are unchanged.
    unsafe {
        open_from(path, out_db, |path| {
            Database::open_with_key(path, KeySource::Raw(key))
        })
    }
}

/// Opens a database using a versioned options structure.
///
/// # Safety
///
/// `path` and `out_db` follow [`csgdb_open`]. `options` must point to a
/// readable `csgdb_open_options` whose nested key data remains readable for
/// the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn csgdb_open_v3(
    path: *const c_char,
    out_db: *mut *mut CsgdbHandle,
    options: *const csgdb_open_options,
) -> i32 {
    // SAFETY: output pointer validity is part of the caller contract.
    if !unsafe { clear_output(out_db) } {
        return CSGDB_INVALID_ARGUMENT;
    }
    if options.is_null() {
        return CSGDB_INVALID_ARGUMENT;
    }
    // SAFETY: the caller provides a readable options structure.
    let options = unsafe { &*options };
    if usize::try_from(options.struct_size).unwrap_or(0) < std::mem::size_of::<csgdb_open_options>()
        || options.abi_version != ABI_VERSION
        || !options.vfs.is_null()
        || !options.device_profile.is_null()
    {
        return CSGDB_INVALID_ARGUMENT;
    }

    let Some(flags) = OpenFlags::from_bits(options.flags) else {
        return CSGDB_INVALID_OPEN_FLAGS;
    };
    // SAFETY: nested key storage follows the caller contract.
    let key = match unsafe { parse_key_source(&options.key) } {
        Ok(key) => key,
        Err(error) => return error_code(&error),
    };
    let open_options = OpenOptions {
        flags,
        busy_timeout: Duration::from_millis(u64::from(options.busy_timeout_ms)),
        cache_size_bytes: options.cache_size_bytes,
        memory_budget_bytes: options.memory_budget_bytes,
        key,
        auto_key_provider: None,
    };

    // SAFETY: forwarded path and output contracts are unchanged.
    unsafe {
        open_from(path, out_db, |path| {
            Database::builder(path).options(open_options).open()
        })
    }
}

/// Executes one or more SQL statements.
///
/// # Safety
///
/// `database` must be a live handle returned by this library. `sql` must
/// point to a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn csgdb_exec(database: *mut CsgdbHandle, sql: *const c_char) -> i32 {
    // SAFETY: the caller guarantees a null or live handle.
    let Some(database) = (unsafe { database.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    let sql = match c_string(sql) {
        Ok(sql) => sql,
        Err(code) => return code,
    };
    let Some(shared) = database.database.as_ref() else {
        return database.current_code();
    };
    let Ok(connection) = shared.database.lock() else {
        return CSGDB_STORAGE;
    };
    match connection.execute_batch(sql) {
        Ok(()) => {
            shared.set_ok();
            CSGDB_OK
        }
        Err(error) => shared.set_error(&error),
    }
}

/// Compiles the first SQL statement in `sql`.
///
/// # Safety
///
/// All pointers must be null or valid for their documented input/output
/// sizes. `database` must be a live handle. `sql` must contain `sql_len`
/// readable bytes, or be NUL-terminated when `sql_len` is negative.
#[no_mangle]
pub unsafe extern "C" fn csgdb_prepare_v2(
    database: *mut CsgdbHandle,
    sql: *const c_char,
    sql_len: c_int,
    out_statement: *mut *mut CsgdbStatement,
    out_tail: *mut *const c_char,
) -> i32 {
    // SAFETY: forwarded pointer contracts are unchanged.
    unsafe { prepare_statement(database, sql, sql_len, 0, out_statement, out_tail) }
}

/// Compiles the first SQL statement in `sql` with supported preparation
/// flags.
///
/// # Safety
///
/// The pointer contracts are the same as [`csgdb_prepare_v2`].
#[no_mangle]
pub unsafe extern "C" fn csgdb_prepare_v3(
    database: *mut CsgdbHandle,
    sql: *const c_char,
    sql_len: c_int,
    flags: u32,
    out_statement: *mut *mut CsgdbStatement,
    out_tail: *mut *const c_char,
) -> i32 {
    // SAFETY: forwarded pointer contracts are unchanged.
    unsafe { prepare_statement(database, sql, sql_len, flags, out_statement, out_tail) }
}

/// Returns the number of bind parameters in a statement.
///
/// # Safety
///
/// `statement` must be null or a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_bind_parameter_count(statement: *const CsgdbStatement) -> c_int {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return 0;
    };
    with_statement_metadata(statement, 0, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_bind_parameter_count(raw) }
    })
}

/// Returns the one-based index of a named parameter, or zero when absent.
///
/// # Safety
///
/// `statement` must be live and `name` must be a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn csgdb_bind_parameter_index(
    statement: *const CsgdbStatement,
    name: *const c_char,
) -> c_int {
    // SAFETY: the caller guarantees null or live inputs.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return 0;
    };
    if name.is_null() {
        statement.database.set_code(CSGDB_INVALID_ARGUMENT);
        return 0;
    }
    with_statement_metadata(statement, 0, |raw| {
        // SAFETY: the statement lock keeps `raw` live and the caller keeps
        // `name` readable for this call.
        unsafe { sqlite::sqlite3_bind_parameter_index(raw, name) }
    })
}

/// Returns the name of a one-based bind parameter, or null if unnamed.
///
/// # Safety
///
/// `statement` must be null or a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_bind_parameter_name(
    statement: *const CsgdbStatement,
    index: c_int,
) -> *const c_char {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return ptr::null();
    };
    with_statement_metadata(statement, ptr::null(), |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_bind_parameter_name(raw, index) }
    })
}

/// Binds SQL NULL to a one-based parameter index.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_bind_null(statement: *mut CsgdbStatement, index: c_int) -> i32 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    bind_value(statement, index, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_bind_null(raw, index) }
    })
}

/// Binds a 32-bit integer.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_bind_int(
    statement: *mut CsgdbStatement,
    index: c_int,
    value: c_int,
) -> i32 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    bind_value(statement, index, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_bind_int(raw, index, value) }
    })
}

/// Binds a 64-bit integer.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_bind_int64(
    statement: *mut CsgdbStatement,
    index: c_int,
    value: i64,
) -> i32 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    bind_value(statement, index, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_bind_int64(raw, index, value) }
    })
}

/// Binds a floating-point value.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_bind_double(
    statement: *mut CsgdbStatement,
    index: c_int,
    value: f64,
) -> i32 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    bind_value(statement, index, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_bind_double(raw, index, value) }
    })
}

/// Binds UTF-8 text, copying it before this call returns.
///
/// A negative `value_len` reads through the first NUL terminator. A null
/// `value` binds SQL NULL.
///
/// # Safety
///
/// `statement` must be live. Non-null `value` must contain `value_len`
/// readable bytes, or be NUL-terminated when the length is negative.
#[no_mangle]
pub unsafe extern "C" fn csgdb_bind_text(
    statement: *mut CsgdbStatement,
    index: c_int,
    value: *const c_char,
    value_len: i64,
) -> i32 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    if value.is_null() {
        return bind_value(statement, index, |raw| {
            // SAFETY: the statement lock keeps `raw` live.
            unsafe { sqlite::sqlite3_bind_null(raw, index) }
        });
    }
    let byte_len = if value_len < 0 {
        // SAFETY: the caller guarantees a NUL-terminated value.
        unsafe { CStr::from_ptr(value) }.to_bytes().len()
    } else {
        let Ok(length) = usize::try_from(value_len) else {
            return statement.database.set_code(CSGDB_INVALID_ARGUMENT);
        };
        length
    };
    let Ok(byte_len) = u64::try_from(byte_len) else {
        return statement.database.set_code(CSGDB_INVALID_ARGUMENT);
    };
    bind_value(statement, index, |raw| {
        // SAFETY: the input is readable for the call, the statement lock
        // keeps `raw` live, and SQLITE_TRANSIENT makes the engine copy bytes.
        unsafe {
            sqlite::sqlite3_bind_text64(
                raw,
                index,
                value,
                byte_len,
                sqlite::SQLITE_TRANSIENT(),
                SQLITE_UTF8_ENCODING,
            )
        }
    })
}

/// Binds a blob, copying it before this call returns. A null pointer binds
/// SQL NULL.
///
/// # Safety
///
/// `statement` must be live. Non-null `value` must contain `value_len`
/// readable bytes.
#[no_mangle]
pub unsafe extern "C" fn csgdb_bind_blob(
    statement: *mut CsgdbStatement,
    index: c_int,
    value: *const c_void,
    value_len: usize,
) -> i32 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    if value.is_null() {
        return bind_value(statement, index, |raw| {
            // SAFETY: the statement lock keeps `raw` live.
            unsafe { sqlite::sqlite3_bind_null(raw, index) }
        });
    }
    let Ok(byte_len) = u64::try_from(value_len) else {
        return statement.database.set_code(CSGDB_INVALID_ARGUMENT);
    };
    bind_value(statement, index, |raw| {
        // SAFETY: the input is readable for the call, the statement lock
        // keeps `raw` live, and SQLITE_TRANSIENT makes the engine copy bytes.
        unsafe {
            sqlite::sqlite3_bind_blob64(raw, index, value, byte_len, sqlite::SQLITE_TRANSIENT())
        }
    })
}

/// Advances a prepared statement.
///
/// Returns [`CSGDB_ROW`] for a result row, [`CSGDB_DONE`] at completion, or
/// an error code. A completed or failed statement must be reset before reuse.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_step(statement: *mut CsgdbStatement) -> i32 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    let Ok(_database) = statement.database.database.lock() else {
        return statement.database.set_code(CSGDB_STORAGE);
    };
    let Ok(mut inner) = statement.inner.lock() else {
        return statement.database.set_code(CSGDB_STORAGE);
    };
    if matches!(inner.state, StatementState::Done | StatementState::Failed) {
        return statement.database.set_code(CSGDB_MISUSE);
    }
    // SAFETY: both locks keep the statement and owning connection live and
    // serialize this raw engine call.
    let result = unsafe { sqlite::sqlite3_step(inner.raw) };
    match result {
        sqlite::SQLITE_ROW => {
            inner.state = StatementState::Row;
            statement.database.set_ok();
            CSGDB_ROW
        }
        sqlite::SQLITE_DONE => {
            inner.state = StatementState::Done;
            statement.database.set_ok();
            CSGDB_DONE
        }
        _ => {
            inner.state = StatementState::Failed;
            statement.database.set_code(map_sqlite_code(result))
        }
    }
}

/// Resets a statement to its ready state while retaining bindings.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_reset(statement: *mut CsgdbStatement) -> i32 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    let Ok(_database) = statement.database.database.lock() else {
        return statement.database.set_code(CSGDB_STORAGE);
    };
    let Ok(mut inner) = statement.inner.lock() else {
        return statement.database.set_code(CSGDB_STORAGE);
    };
    // SAFETY: both locks keep the raw statement live and serialize the call.
    let result = unsafe { sqlite::sqlite3_reset(inner.raw) };
    inner.state = StatementState::Ready;
    set_sqlite_result(&statement.database, result)
}

/// Removes all bindings from a ready statement.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_clear_bindings(statement: *mut CsgdbStatement) -> i32 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    statement_ready_operation(statement, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_clear_bindings(raw) }
    })
}

/// Returns the number of result columns.
///
/// # Safety
///
/// `statement` must be null or a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_column_count(statement: *const CsgdbStatement) -> c_int {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return 0;
    };
    with_statement_metadata(statement, 0, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_column_count(raw) }
    })
}

/// Returns a result-column name. The pointer remains valid until finalization.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_column_name(
    statement: *const CsgdbStatement,
    index: c_int,
) -> *const c_char {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return ptr::null();
    };
    with_column_metadata(statement, index, ptr::null(), |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_column_name(raw, index) }
    })
}

/// Returns the storage class of a column in the current row.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_column_type(
    statement: *const CsgdbStatement,
    index: c_int,
) -> c_int {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return CSGDB_NULL;
    };
    with_row_column(statement, index, CSGDB_NULL, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_column_type(raw, index) }
    })
}

/// Returns a column coerced to a 32-bit integer.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_column_int(statement: *const CsgdbStatement, index: c_int) -> c_int {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return 0;
    };
    with_row_column(statement, index, 0, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_column_int(raw, index) }
    })
}

/// Returns a column coerced to a 64-bit integer.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_column_int64(statement: *const CsgdbStatement, index: c_int) -> i64 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return 0;
    };
    with_row_column(statement, index, 0, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_column_int64(raw, index) }
    })
}

/// Returns a column coerced to a floating-point value.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_column_double(
    statement: *const CsgdbStatement,
    index: c_int,
) -> f64 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return 0.0;
    };
    with_row_column(statement, index, 0.0, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_column_double(raw, index) }
    })
}

/// Returns a UTF-8 text pointer for the current row. The pointer is invalidated
/// by the next step, reset, or finalization.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_column_text(
    statement: *const CsgdbStatement,
    index: c_int,
) -> *const u8 {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return ptr::null();
    };
    with_row_column(statement, index, ptr::null(), |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_column_text(raw, index) }
    })
}

/// Returns a blob pointer for the current row. The pointer is invalidated by
/// the next step, reset, or finalization.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_column_blob(
    statement: *const CsgdbStatement,
    index: c_int,
) -> *const c_void {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return ptr::null();
    };
    with_row_column(statement, index, ptr::null(), |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_column_blob(raw, index) }
    })
}

/// Returns the byte length of the current text or blob column.
///
/// # Safety
///
/// `statement` must be a live statement handle.
#[no_mangle]
pub unsafe extern "C" fn csgdb_column_bytes(
    statement: *const CsgdbStatement,
    index: c_int,
) -> c_int {
    // SAFETY: the caller guarantees a null or live statement.
    let Some(statement) = (unsafe { statement.as_ref() }) else {
        return 0;
    };
    with_row_column(statement, index, 0, |raw| {
        // SAFETY: the statement lock keeps `raw` live.
        unsafe { sqlite::sqlite3_column_bytes(raw, index) }
    })
}

/// Finalizes and releases a statement handle.
///
/// # Safety
///
/// `statement` must be null or a live handle and must not be reused.
#[no_mangle]
pub unsafe extern "C" fn csgdb_finalize(statement: *mut CsgdbStatement) -> i32 {
    if statement.is_null() {
        return CSGDB_OK;
    }
    // SAFETY: ownership of a live statement is transferred exactly once.
    let boxed = unsafe { Box::from_raw(statement) };
    let Ok(_database) = boxed.database.database.lock() else {
        return boxed.database.set_code(CSGDB_STORAGE);
    };
    let Ok(mut inner) = boxed.inner.lock() else {
        return boxed.database.set_code(CSGDB_STORAGE);
    };
    let raw = std::mem::replace(&mut inner.raw, ptr::null_mut());
    // SAFETY: `raw` was created by prepare, remains live under both locks, and
    // is finalized exactly once.
    let result = unsafe { sqlite::sqlite3_finalize(raw) };
    set_sqlite_result(&boxed.database, result)
}

/// Closes and releases a database handle.
///
/// # Safety
///
/// `database` must be null or a live handle returned by this library, and it
/// must not be used again after this call.
#[no_mangle]
pub unsafe extern "C" fn csgdb_close(database: *mut CsgdbHandle) -> i32 {
    if database.is_null() {
        return CSGDB_OK;
    }
    // SAFETY: ownership of a live handle is transferred exactly once.
    let boxed = unsafe { Box::from_raw(database) };
    let Some(shared) = boxed.database else {
        return boxed.open_code;
    };
    match Arc::try_unwrap(shared) {
        Ok(shared) => match shared.database.into_inner() {
            Ok(database) => database
                .close()
                .as_ref()
                .map_or_else(error_code, |()| CSGDB_OK),
            Err(_) => CSGDB_STORAGE,
        },
        // Active prepared statements retain the connection. It is closed
        // automatically after the final statement is finalized.
        Err(_) => CSGDB_OK,
    }
}

/// Returns the most recent error code for a handle.
///
/// # Safety
///
/// `database` must be null or a live handle returned by this library.
#[no_mangle]
pub unsafe extern "C" fn csgdb_errcode(database: *const CsgdbHandle) -> i32 {
    // SAFETY: the caller guarantees a null or live handle.
    unsafe { database.as_ref() }.map_or(CSGDB_INVALID_ARGUMENT, CsgdbHandle::current_code)
}

/// Returns a static description of the most recent handle error.
///
/// # Safety
///
/// `database` must be null or a live handle returned by this library.
#[no_mangle]
pub unsafe extern "C" fn csgdb_errmsg(database: *const CsgdbHandle) -> *const c_char {
    // SAFETY: forwarded handle contract is unchanged.
    let code = unsafe { csgdb_errcode(database) };
    csgdb_errstr(code)
}

#[no_mangle]
pub extern "C" fn csgdb_errstr(code: i32) -> *const c_char {
    error_message(code).as_ptr().cast()
}

unsafe fn prepare_statement(
    database: *mut CsgdbHandle,
    sql: *const c_char,
    sql_len: c_int,
    flags: u32,
    out_statement: *mut *mut CsgdbStatement,
    out_tail: *mut *const c_char,
) -> i32 {
    if out_statement.is_null() {
        return CSGDB_INVALID_ARGUMENT;
    }
    // SAFETY: the caller guarantees writable output storage.
    unsafe {
        out_statement.write(ptr::null_mut());
        if !out_tail.is_null() {
            out_tail.write(ptr::null());
        }
    }
    // SAFETY: the caller guarantees a null or live database handle.
    let Some(handle) = (unsafe { database.as_ref() }) else {
        return CSGDB_INVALID_ARGUMENT;
    };
    let Some(shared) = handle.database.as_ref() else {
        return handle.current_code();
    };
    if sql.is_null() || sql_len < -1 || flags & !CSGDB_PREPARE_PERSISTENT != 0 {
        return shared.set_code(CSGDB_INVALID_ARGUMENT);
    }
    let Ok(connection) = shared.database.lock() else {
        return shared.set_code(CSGDB_STORAGE);
    };

    let mut raw_statement = ptr::null_mut();
    let mut tail = ptr::null();
    // SAFETY: the connection lock keeps the engine handle live; caller input
    // and output pointers satisfy this function's contract.
    let result = unsafe {
        sqlite::sqlite3_prepare_v3(
            connection.as_raw_handle().cast(),
            sql,
            sql_len,
            flags,
            &raw mut raw_statement,
            &raw mut tail,
        )
    };
    if !out_tail.is_null() {
        // SAFETY: the caller guarantees writable output storage.
        unsafe {
            out_tail.write(tail);
        }
    }
    if result != sqlite::SQLITE_OK {
        if !raw_statement.is_null() {
            // SAFETY: defensive cleanup of an engine-created statement.
            unsafe {
                sqlite::sqlite3_finalize(raw_statement);
            }
        }
        return shared.set_code(map_sqlite_code(result));
    }

    if !raw_statement.is_null() {
        let statement = Box::new(CsgdbStatement {
            database: Arc::clone(shared),
            inner: Mutex::new(StatementInner {
                raw: raw_statement,
                state: StatementState::Ready,
            }),
        });
        // SAFETY: the caller guarantees writable output storage.
        unsafe {
            out_statement.write(Box::into_raw(statement));
        }
    }
    shared.set_ok();
    CSGDB_OK
}

fn bind_value(
    statement: &CsgdbStatement,
    index: c_int,
    operation: impl FnOnce(*mut sqlite::sqlite3_stmt) -> c_int,
) -> i32 {
    if index <= 0 {
        return statement.database.set_code(CSGDB_RANGE);
    }
    let parameter_count = with_statement_metadata(statement, 0, |raw| {
        // SAFETY: the helper locks keep `raw` live.
        unsafe { sqlite::sqlite3_bind_parameter_count(raw) }
    });
    if index > parameter_count {
        return statement.database.set_code(CSGDB_RANGE);
    }
    statement_ready_operation(statement, operation)
}

fn statement_ready_operation(
    statement: &CsgdbStatement,
    operation: impl FnOnce(*mut sqlite::sqlite3_stmt) -> c_int,
) -> i32 {
    let Ok(_database) = statement.database.database.lock() else {
        return statement.database.set_code(CSGDB_STORAGE);
    };
    let Ok(inner) = statement.inner.lock() else {
        return statement.database.set_code(CSGDB_STORAGE);
    };
    if inner.state != StatementState::Ready || inner.raw.is_null() {
        return statement.database.set_code(CSGDB_MISUSE);
    }
    set_sqlite_result(&statement.database, operation(inner.raw))
}

fn with_statement_metadata<T>(
    statement: &CsgdbStatement,
    default: T,
    operation: impl FnOnce(*mut sqlite::sqlite3_stmt) -> T,
) -> T {
    let Ok(_database) = statement.database.database.lock() else {
        statement.database.set_code(CSGDB_STORAGE);
        return default;
    };
    let Ok(inner) = statement.inner.lock() else {
        statement.database.set_code(CSGDB_STORAGE);
        return default;
    };
    if inner.raw.is_null() {
        statement.database.set_code(CSGDB_MISUSE);
        return default;
    }
    let value = operation(inner.raw);
    statement.database.set_ok();
    value
}

fn with_column_metadata<T>(
    statement: &CsgdbStatement,
    index: c_int,
    default: T,
    operation: impl FnOnce(*mut sqlite::sqlite3_stmt) -> T,
) -> T {
    let Ok(_database) = statement.database.database.lock() else {
        statement.database.set_code(CSGDB_STORAGE);
        return default;
    };
    let Ok(inner) = statement.inner.lock() else {
        statement.database.set_code(CSGDB_STORAGE);
        return default;
    };
    if inner.raw.is_null() {
        statement.database.set_code(CSGDB_MISUSE);
        return default;
    }
    // SAFETY: both locks keep `inner.raw` live.
    let count = unsafe { sqlite::sqlite3_column_count(inner.raw) };
    if index < 0 || index >= count {
        statement.database.set_code(CSGDB_RANGE);
        return default;
    }
    let value = operation(inner.raw);
    statement.database.set_ok();
    value
}

fn with_row_column<T>(
    statement: &CsgdbStatement,
    index: c_int,
    default: T,
    operation: impl FnOnce(*mut sqlite::sqlite3_stmt) -> T,
) -> T {
    let Ok(_database) = statement.database.database.lock() else {
        statement.database.set_code(CSGDB_STORAGE);
        return default;
    };
    let Ok(inner) = statement.inner.lock() else {
        statement.database.set_code(CSGDB_STORAGE);
        return default;
    };
    if inner.raw.is_null() || inner.state != StatementState::Row {
        statement.database.set_code(CSGDB_MISUSE);
        return default;
    }
    // SAFETY: both locks keep `inner.raw` live.
    let count = unsafe { sqlite::sqlite3_column_count(inner.raw) };
    if index < 0 || index >= count {
        statement.database.set_code(CSGDB_RANGE);
        return default;
    }
    let value = operation(inner.raw);
    statement.database.set_ok();
    value
}

fn set_sqlite_result(database: &SharedDatabase, result: c_int) -> i32 {
    if result == sqlite::SQLITE_OK {
        database.set_ok();
        CSGDB_OK
    } else {
        database.set_code(map_sqlite_code(result))
    }
}

fn map_sqlite_code(result: c_int) -> i32 {
    match result & 0xff {
        sqlite::SQLITE_OK => CSGDB_OK,
        sqlite::SQLITE_BUSY | sqlite::SQLITE_LOCKED => CSGDB_BUSY,
        sqlite::SQLITE_READONLY => CSGDB_READONLY,
        sqlite::SQLITE_CONSTRAINT => CSGDB_CONSTRAINT,
        sqlite::SQLITE_CORRUPT | sqlite::SQLITE_NOTADB => CSGDB_CORRUPT,
        sqlite::SQLITE_MISUSE => CSGDB_MISUSE,
        sqlite::SQLITE_RANGE => CSGDB_RANGE,
        _ => CSGDB_STORAGE,
    }
}

unsafe fn open_from(
    path: *const c_char,
    out_db: *mut *mut CsgdbHandle,
    operation: impl FnOnce(&str) -> CsgResult<Database>,
) -> i32 {
    // SAFETY: output pointer validity is part of the caller contract.
    if !unsafe { clear_output(out_db) } {
        return CSGDB_INVALID_ARGUMENT;
    }
    let path = match c_string(path) {
        Ok(path) => path,
        Err(code) => return code,
    };
    let (database, code) = match operation(path) {
        Ok(database) => (Some(database), CSGDB_OK),
        Err(error) => (None, error_code(&error)),
    };
    let handle = Box::into_raw(Box::new(CsgdbHandle::new(database, code)));
    // SAFETY: output pointer validity is part of the caller contract.
    unsafe {
        out_db.write(handle);
    }
    code
}

unsafe fn clear_output(out_db: *mut *mut CsgdbHandle) -> bool {
    if out_db.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees writable output storage.
    unsafe {
        out_db.write(ptr::null_mut());
    }
    true
}

unsafe fn parse_key_source(source: &csgdb_key_source) -> CsgResult<KeySource> {
    if usize::try_from(source.struct_size).unwrap_or(0) < std::mem::size_of::<csgdb_key_source>() {
        return Err(Error::new(
            ErrorCode::InvalidKeyLength,
            "key source structure is too small",
        ));
    }
    match source.kind {
        CSGDB_KEY_AUTO => Ok(KeySource::Auto),
        CSGDB_KEY_RAW => {
            if source.data.is_null() {
                return Err(Error::new(
                    ErrorCode::InvalidKeyLength,
                    "raw key pointer is null",
                ));
            }
            // SAFETY: nested key storage follows the caller contract.
            let bytes =
                unsafe { std::slice::from_raw_parts(source.data.cast::<u8>(), source.data_len) };
            SecretKey::from_slice(bytes).map(KeySource::Raw)
        }
        CSGDB_KEY_PASSPHRASE => {
            if source.data.is_null() {
                return Err(Error::new(
                    ErrorCode::InvalidKeyLength,
                    "passphrase pointer is null",
                ));
            }
            // SAFETY: nested passphrase storage follows the caller contract.
            let bytes =
                unsafe { std::slice::from_raw_parts(source.data.cast::<u8>(), source.data_len) };
            Ok(KeySource::Passphrase(SecretString::new(bytes)))
        }
        CSGDB_KEY_PROVIDER => Err(Error::new(
            ErrorCode::KeyStoreUnavailable,
            "named key providers are not registered",
        )),
        _ => Err(Error::new(
            ErrorCode::InvalidKeyLength,
            "unknown key source kind",
        )),
    }
}

fn c_string<'value>(value: *const c_char) -> std::result::Result<&'value str, i32> {
    if value.is_null() {
        return Err(CSGDB_INVALID_ARGUMENT);
    }
    // SAFETY: callers of this private helper carry the valid C-string
    // contract; the returned borrow cannot outlive that storage.
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|_| CSGDB_INVALID_ARGUMENT)
}

fn error_code(error: &Error) -> i32 {
    match error.code() {
        ErrorCode::InvalidOpenFlags | ErrorCode::PlaintextRequiresOptIn => CSGDB_INVALID_OPEN_FLAGS,
        ErrorCode::InvalidKeyLength | ErrorCode::InvalidDatabaseKey => CSGDB_INVALID_KEY,
        ErrorCode::KeyRequired => CSGDB_KEY_REQUIRED,
        ErrorCode::KeyStoreUnavailable => CSGDB_KEYSTORE_UNAVAILABLE,
        ErrorCode::DatabaseBusy => CSGDB_BUSY,
        ErrorCode::DatabaseReadOnly => CSGDB_READONLY,
        ErrorCode::ConstraintViolation => CSGDB_CONSTRAINT,
        ErrorCode::DatabaseCorrupt => CSGDB_CORRUPT,
        ErrorCode::InvalidParameterIndex | ErrorCode::InvalidColumnIndex => CSGDB_RANGE,
        ErrorCode::InvalidStatementState => CSGDB_MISUSE,
        ErrorCode::InvalidPath
        | ErrorCode::InvalidSql
        | ErrorCode::ParameterCountMismatch
        | ErrorCode::InvalidColumnType
        | ErrorCode::InvalidUtf8 => CSGDB_INVALID_ARGUMENT,
        _ => CSGDB_STORAGE,
    }
}

fn error_message(code: i32) -> &'static [u8] {
    match code {
        CSGDB_OK => MESSAGE_OK,
        CSGDB_INVALID_ARGUMENT => MESSAGE_INVALID_ARGUMENT,
        CSGDB_INVALID_OPEN_FLAGS => MESSAGE_INVALID_OPEN_FLAGS,
        CSGDB_INVALID_KEY => MESSAGE_INVALID_KEY,
        CSGDB_KEY_REQUIRED => MESSAGE_KEY_REQUIRED,
        CSGDB_KEYSTORE_UNAVAILABLE => MESSAGE_KEYSTORE_UNAVAILABLE,
        CSGDB_BUSY => MESSAGE_BUSY,
        CSGDB_READONLY => MESSAGE_READONLY,
        CSGDB_CONSTRAINT => MESSAGE_CONSTRAINT,
        CSGDB_CORRUPT => MESSAGE_CORRUPT,
        CSGDB_MISUSE => MESSAGE_MISUSE,
        CSGDB_RANGE => MESSAGE_RANGE,
        _ => MESSAGE_STORAGE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    fn test_path(name: &str) -> (std::path::PathBuf, CString) {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "csgdb-ffi-{name}-{}-{sequence}.db",
            std::process::id()
        ));
        let c_path = CString::new(path.to_string_lossy().as_bytes()).expect("C path");
        (path, c_path)
    }

    fn remove_database(path: &std::path::Path) {
        let _ = fs::remove_file(path);
        let base = path.to_string_lossy();
        let _ = fs::remove_file(format!("{base}-wal"));
        let _ = fs::remove_file(format!("{base}-shm"));
    }

    #[test]
    fn default_c_options_enable_encryption() {
        let options = csgdb_open_options::default();
        assert_ne!(options.flags & OpenFlags::ENCRYPTED.bits(), 0);
        assert_eq!(options.flags & OpenFlags::PLAINTEXT.bits(), 0);
        assert_eq!(options.abi_version, ABI_VERSION);
    }

    #[test]
    fn initializer_rejects_null_pointer() {
        let result = unsafe { csgdb_open_options_init(ptr::null_mut()) };
        assert_eq!(result, CSGDB_INVALID_ARGUMENT);
    }

    #[test]
    fn invalid_key_clears_output_handle() {
        let (_path, c_path) = test_path("invalid-key");
        let key = [1_u8; 31];
        let mut database = std::ptr::NonNull::<CsgdbHandle>::dangling().as_ptr();
        let result = unsafe {
            csgdb_open_with_key(
                c_path.as_ptr(),
                key.as_ptr().cast(),
                key.len(),
                &raw mut database,
            )
        };
        assert_eq!(result, CSGDB_INVALID_KEY);
        assert!(database.is_null());
    }

    #[test]
    fn raw_key_open_exec_and_close_work() {
        let (path, c_path) = test_path("encrypted");
        let key = [11_u8; 32];
        let mut database = ptr::null_mut();
        let result = unsafe {
            csgdb_open_with_key(
                c_path.as_ptr(),
                key.as_ptr().cast(),
                key.len(),
                &raw mut database,
            )
        };
        assert_eq!(result, CSGDB_OK);
        assert!(!database.is_null());

        let sql = CString::new(
            "CREATE TABLE event(id INTEGER PRIMARY KEY); INSERT INTO event DEFAULT VALUES;",
        )
        .expect("SQL");
        assert_eq!(unsafe { csgdb_exec(database, sql.as_ptr()) }, CSGDB_OK);
        assert_eq!(unsafe { csgdb_close(database) }, CSGDB_OK);
        remove_database(&path);
    }

    #[test]
    fn plaintext_v2_open_is_explicit() {
        let (path, c_path) = test_path("plaintext");
        let flags = (OpenFlags::READWRITE | OpenFlags::CREATE | OpenFlags::PLAINTEXT).bits();
        let mut database = ptr::null_mut();
        let result =
            unsafe { csgdb_open_v2(c_path.as_ptr(), &raw mut database, flags, ptr::null()) };
        assert_eq!(result, CSGDB_OK);
        assert_eq!(unsafe { csgdb_close(database) }, CSGDB_OK);
        remove_database(&path);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn prepared_statement_c_api_round_trips_values_and_state() {
        let (path, c_path) = test_path("prepared");
        let key = [17_u8; 32];
        let mut database = ptr::null_mut();
        assert_eq!(
            unsafe {
                csgdb_open_with_key(
                    c_path.as_ptr(),
                    key.as_ptr().cast(),
                    key.len(),
                    &raw mut database,
                )
            },
            CSGDB_OK
        );

        let schema = CString::new(
            "CREATE TABLE sample(
                id INTEGER PRIMARY KEY,
                score REAL,
                body TEXT,
                payload BLOB,
                optional TEXT
            );",
        )
        .expect("schema");
        assert_eq!(unsafe { csgdb_exec(database, schema.as_ptr()) }, CSGDB_OK);

        let insert = CString::new(
            "INSERT INTO sample(id, score, body, payload, optional)
             VALUES (:id, :score, :body, :payload, :optional)",
        )
        .expect("insert");
        let mut statement = ptr::null_mut();
        assert_eq!(
            unsafe {
                csgdb_prepare_v3(
                    database,
                    insert.as_ptr(),
                    -1,
                    CSGDB_PREPARE_PERSISTENT,
                    &raw mut statement,
                    ptr::null_mut(),
                )
            },
            CSGDB_OK
        );
        assert!(!statement.is_null());
        assert_eq!(unsafe { csgdb_bind_parameter_count(statement) }, 5);
        let body_name = CString::new(":body").expect("parameter name");
        assert_eq!(
            unsafe { csgdb_bind_parameter_index(statement, body_name.as_ptr()) },
            3
        );
        let expected_name = CString::new(":id").expect("parameter name");
        let actual_name = unsafe { csgdb_bind_parameter_name(statement, 1) };
        assert!(!actual_name.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(actual_name) },
            expected_name.as_c_str()
        );

        let body = b"agent row";
        let payload = [0_u8, 1, 2, 255];
        assert_eq!(unsafe { csgdb_bind_int64(statement, 1, 41) }, CSGDB_OK);
        assert_eq!(unsafe { csgdb_bind_double(statement, 2, 2.5) }, CSGDB_OK);
        assert_eq!(
            unsafe {
                csgdb_bind_text(
                    statement,
                    3,
                    body.as_ptr().cast(),
                    i64::try_from(body.len()).expect("length"),
                )
            },
            CSGDB_OK
        );
        assert_eq!(
            unsafe { csgdb_bind_blob(statement, 4, payload.as_ptr().cast(), payload.len(),) },
            CSGDB_OK
        );
        assert_eq!(unsafe { csgdb_bind_null(statement, 5) }, CSGDB_OK);
        assert_eq!(unsafe { csgdb_bind_null(statement, 6) }, CSGDB_RANGE);
        assert_eq!(unsafe { csgdb_step(statement) }, CSGDB_DONE);
        assert_eq!(unsafe { csgdb_step(statement) }, CSGDB_MISUSE);
        assert_eq!(unsafe { csgdb_reset(statement) }, CSGDB_OK);
        assert_eq!(unsafe { csgdb_clear_bindings(statement) }, CSGDB_OK);
        assert_eq!(unsafe { csgdb_finalize(statement) }, CSGDB_OK);

        let select =
            CString::new("SELECT id, score, body, payload, optional FROM sample WHERE id = ?")
                .expect("select");
        statement = ptr::null_mut();
        assert_eq!(
            unsafe {
                csgdb_prepare_v2(
                    database,
                    select.as_ptr(),
                    -1,
                    &raw mut statement,
                    ptr::null_mut(),
                )
            },
            CSGDB_OK
        );
        assert_eq!(unsafe { csgdb_column_count(statement) }, 5);
        let column_name = unsafe { csgdb_column_name(statement, 2) };
        assert_eq!(unsafe { CStr::from_ptr(column_name) }, c"body");
        assert_eq!(unsafe { csgdb_bind_int(statement, 1, 41) }, CSGDB_OK);

        assert_eq!(unsafe { csgdb_column_int64(statement, 0) }, 0);
        assert_eq!(unsafe { csgdb_errcode(database) }, CSGDB_MISUSE);
        assert_eq!(unsafe { csgdb_step(statement) }, CSGDB_ROW);
        assert_eq!(unsafe { csgdb_column_type(statement, 0) }, CSGDB_INTEGER);
        assert_eq!(unsafe { csgdb_column_int64(statement, 0) }, 41);
        assert_eq!(unsafe { csgdb_column_type(statement, 1) }, CSGDB_FLOAT);
        assert!((unsafe { csgdb_column_double(statement, 1) } - 2.5).abs() < f64::EPSILON);
        assert_eq!(unsafe { csgdb_column_type(statement, 2) }, CSGDB_TEXT);
        let text = unsafe { csgdb_column_text(statement, 2) };
        let text_len = usize::try_from(unsafe { csgdb_column_bytes(statement, 2) })
            .expect("non-negative text length");
        assert_eq!(unsafe { std::slice::from_raw_parts(text, text_len) }, body);
        assert_eq!(unsafe { csgdb_column_type(statement, 3) }, CSGDB_BLOB);
        let blob = unsafe { csgdb_column_blob(statement, 3) };
        let blob_len = usize::try_from(unsafe { csgdb_column_bytes(statement, 3) })
            .expect("non-negative blob length");
        assert_eq!(
            unsafe { std::slice::from_raw_parts(blob.cast::<u8>(), blob_len) },
            payload
        );
        assert_eq!(unsafe { csgdb_column_type(statement, 4) }, CSGDB_NULL);
        assert!(unsafe { csgdb_column_text(statement, 4) }.is_null());
        assert_eq!(unsafe { csgdb_step(statement) }, CSGDB_DONE);
        assert_eq!(unsafe { csgdb_finalize(statement) }, CSGDB_OK);
        assert_eq!(unsafe { csgdb_close(database) }, CSGDB_OK);
        remove_database(&path);
    }

    #[test]
    fn closing_a_c_handle_defers_connection_close_for_live_statements() {
        let (path, c_path) = test_path("deferred-close");
        let key = [18_u8; 32];
        let mut database = ptr::null_mut();
        assert_eq!(
            unsafe {
                csgdb_open_with_key(
                    c_path.as_ptr(),
                    key.as_ptr().cast(),
                    key.len(),
                    &raw mut database,
                )
            },
            CSGDB_OK
        );
        let sql = CString::new("SELECT 7").expect("query");
        let mut statement = ptr::null_mut();
        assert_eq!(
            unsafe {
                csgdb_prepare_v2(
                    database,
                    sql.as_ptr(),
                    -1,
                    &raw mut statement,
                    ptr::null_mut(),
                )
            },
            CSGDB_OK
        );

        assert_eq!(unsafe { csgdb_close(database) }, CSGDB_OK);
        assert_eq!(unsafe { csgdb_step(statement) }, CSGDB_ROW);
        assert_eq!(unsafe { csgdb_column_int(statement, 0) }, 7);
        assert_eq!(unsafe { csgdb_finalize(statement) }, CSGDB_OK);
        remove_database(&path);
    }
}
