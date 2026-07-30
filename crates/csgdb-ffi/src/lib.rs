use csgdb_api::Result as CsgResult;
use csgdb_api::{
    Database, Error, ErrorCode, KeySource, OpenFlags, OpenOptions, SecretKey, SecretString,
};
use csgdb_core::{
    ABI_VERSION, DEFAULT_BUSY_TIMEOUT_MS, DEFAULT_CACHE_SIZE_BYTES, DEFAULT_MEMORY_BUDGET_BYTES,
    LIB_VERSION_NUMBER,
};
use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
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

pub struct CsgdbHandle {
    database: Mutex<Option<Database>>,
    last_code: AtomicI32,
}

impl CsgdbHandle {
    fn new(database: Option<Database>, code: i32) -> Self {
        Self {
            database: Mutex::new(database),
            last_code: AtomicI32::new(code),
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
    let Ok(guard) = database.database.lock() else {
        return CSGDB_STORAGE;
    };
    let Some(connection) = guard.as_ref() else {
        return database.last_code.load(Ordering::Acquire);
    };
    match connection.execute_batch(sql) {
        Ok(()) => {
            database.set_ok();
            CSGDB_OK
        }
        Err(error) => database.set_error(&error),
    }
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
    let connection = match boxed.database.lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => return CSGDB_STORAGE,
    };
    match connection {
        Some(connection) => connection
            .close()
            .as_ref()
            .map_or_else(error_code, |()| CSGDB_OK),
        None => boxed.last_code.load(Ordering::Acquire),
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
    unsafe { database.as_ref() }.map_or(CSGDB_INVALID_ARGUMENT, |database| {
        database.last_code.load(Ordering::Acquire)
    })
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
        ErrorCode::InvalidPath => CSGDB_INVALID_ARGUMENT,
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
}
