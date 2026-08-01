//! Deterministic storage fault injection for recovery testing.
//!
//! This module is feature-gated and is not part of normal database builds.
//! It registers a transparent VFS layered over the platform default and can
//! fail one selected write, synchronization, or truncation operation.

use csgdb_core::{Error, ErrorCode, Result};
use rusqlite::ffi;
use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

const VFS_NAME: &str = "csgdb-fault-v1";
const VFS_NAME_C: &[u8] = b"csgdb-fault-v1\0";
const FILE_OFFSET: usize = std::mem::size_of::<FaultFile>();

static PARENT_VFS: AtomicPtr<ffi::sqlite3_vfs> = AtomicPtr::new(ptr::null_mut());
static REGISTRATION: OnceLock<Result<()>> = OnceLock::new();
static SESSION: Mutex<()> = Mutex::new(());
static CONTROLLER: Mutex<Controller> = Mutex::new(Controller::new());

/// Selects which storage file is eligible for a fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultTarget {
    Any,
    MainDatabase,
    WriteAheadLog,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultOperation {
    Write,
    Sync,
    Truncate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultAction {
    Error,
    PartialWrite(usize),
}

/// A one-shot deterministic fault rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultRule {
    operation: FaultOperation,
    target: FaultTarget,
    occurrence: u64,
    action: FaultAction,
}

impl FaultRule {
    /// Fails the selected write with `SQLITE_IOERR_WRITE`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidFaultRule`] when `occurrence` is zero.
    pub fn write_error(target: FaultTarget, occurrence: u64) -> Result<Self> {
        Self::new(
            FaultOperation::Write,
            target,
            occurrence,
            FaultAction::Error,
        )
    }

    /// Persists at most `prefix_bytes` and then returns `SQLITE_IOERR_WRITE`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidFaultRule`] when `occurrence` or
    /// `prefix_bytes` is zero.
    pub fn partial_write(
        target: FaultTarget,
        occurrence: u64,
        prefix_bytes: usize,
    ) -> Result<Self> {
        if prefix_bytes == 0 {
            return Err(invalid_rule(
                "partial-write prefix must be greater than zero",
            ));
        }
        Self::new(
            FaultOperation::Write,
            target,
            occurrence,
            FaultAction::PartialWrite(prefix_bytes),
        )
    }

    /// Fails the selected synchronization with `SQLITE_IOERR_FSYNC`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidFaultRule`] when `occurrence` is zero.
    pub fn sync_error(target: FaultTarget, occurrence: u64) -> Result<Self> {
        Self::new(FaultOperation::Sync, target, occurrence, FaultAction::Error)
    }

    /// Fails the selected truncation with `SQLITE_IOERR_TRUNCATE`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidFaultRule`] when `occurrence` is zero.
    pub fn truncate_error(target: FaultTarget, occurrence: u64) -> Result<Self> {
        Self::new(
            FaultOperation::Truncate,
            target,
            occurrence,
            FaultAction::Error,
        )
    }

    fn new(
        operation: FaultOperation,
        target: FaultTarget,
        occurrence: u64,
        action: FaultAction,
    ) -> Result<Self> {
        if occurrence == 0 {
            return Err(invalid_rule("fault occurrence must be greater than zero"));
        }
        Ok(Self {
            operation,
            target,
            occurrence,
            action,
        })
    }
}

/// Counters captured since the current fault session started.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FaultStats {
    pub writes: u64,
    pub syncs: u64,
    pub truncates: u64,
    pub matching_operations: u64,
    pub injections: u64,
}

/// Exclusive controller for the process-global fault-injection VFS.
///
/// Only one session may exist at a time. Dropping the session disarms any
/// pending rule. The VFS registration itself remains valid for the process.
pub struct FaultSession {
    _exclusive: MutexGuard<'static, ()>,
}

impl FaultSession {
    /// Registers the fault VFS and starts an exclusive, initially disarmed session.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` initialization or VFS registration fails.
    pub fn start() -> Result<Self> {
        register_vfs()?;
        let exclusive = SESSION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_controller();
        Ok(Self {
            _exclusive: exclusive,
        })
    }

    /// Returns the name to pass to [`csgdb_core::OpenOptions::vfs`].
    #[must_use]
    pub const fn vfs_name(&self) -> &'static str {
        VFS_NAME
    }

    /// Arms one rule and resets its match counter without resetting I/O stats.
    pub fn arm(&self, rule: FaultRule) {
        let mut controller = CONTROLLER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        controller.rule = Some(rule);
        controller.match_count = 0;
    }

    /// Disarms the current rule while preserving counters.
    pub fn disarm(&self) {
        let mut controller = CONTROLLER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        controller.rule = None;
        controller.match_count = 0;
    }

    /// Returns a consistent snapshot of the session counters.
    #[must_use]
    pub fn stats(&self) -> FaultStats {
        CONTROLLER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stats
    }
}

impl Drop for FaultSession {
    fn drop(&mut self) {
        reset_controller();
    }
}

#[derive(Clone, Copy)]
struct Controller {
    rule: Option<FaultRule>,
    match_count: u64,
    stats: FaultStats,
}

impl Controller {
    const fn new() -> Self {
        Self {
            rule: None,
            match_count: 0,
            stats: FaultStats {
                writes: 0,
                syncs: 0,
                truncates: 0,
                matching_operations: 0,
                injections: 0,
            },
        }
    }

    fn intercept(&mut self, operation: FaultOperation, target: FaultTarget) -> Option<FaultAction> {
        match operation {
            FaultOperation::Write => self.stats.writes = self.stats.writes.saturating_add(1),
            FaultOperation::Sync => self.stats.syncs = self.stats.syncs.saturating_add(1),
            FaultOperation::Truncate => {
                self.stats.truncates = self.stats.truncates.saturating_add(1);
            }
        }
        let rule = self.rule?;
        if rule.operation != operation || !target_matches(rule.target, target) {
            return None;
        }
        self.match_count = self.match_count.saturating_add(1);
        self.stats.matching_operations = self.stats.matching_operations.saturating_add(1);
        if self.match_count != rule.occurrence {
            return None;
        }
        self.rule = None;
        self.stats.injections = self.stats.injections.saturating_add(1);
        Some(rule.action)
    }
}

fn target_matches(expected: FaultTarget, actual: FaultTarget) -> bool {
    expected == FaultTarget::Any || expected == actual
}

fn invalid_rule(message: &'static str) -> Error {
    Error::new(ErrorCode::InvalidFaultRule, message)
}

fn reset_controller() {
    let mut controller = CONTROLLER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *controller = Controller::new();
}

fn register_vfs() -> Result<()> {
    match REGISTRATION.get_or_init(register_vfs_once) {
        Ok(()) => Ok(()),
        Err(error) => Err(error.clone()),
    }
}

fn register_vfs_once() -> Result<()> {
    // SAFETY: SQLite process initialization is idempotent and required before
    // inspecting the process-global VFS registry.
    let initialized = unsafe { ffi::sqlite3_initialize() };
    if initialized != ffi::SQLITE_OK {
        return Err(Error::new(
            ErrorCode::StorageBackendUnavailable,
            "storage runtime initialization failed",
        ));
    }
    // SAFETY: a null name requests SQLite's current default VFS.
    let parent = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
    if parent.is_null() {
        return Err(Error::new(
            ErrorCode::StorageBackendUnavailable,
            "default storage VFS is unavailable",
        ));
    }

    // SAFETY: the registered default VFS remains process-global. Copying its
    // callback table creates a transparent wrapper whose only changed callback
    // is xOpen.
    let mut wrapper = unsafe { *parent };
    let size = FILE_OFFSET
        .checked_add(usize::try_from(wrapper.szOsFile).unwrap_or(usize::MAX))
        .and_then(|size| c_int::try_from(size).ok())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::StorageBackendUnavailable,
                "storage VFS file structure is too large",
            )
        })?;
    wrapper.szOsFile = size;
    wrapper.pNext = ptr::null_mut();
    wrapper.zName = VFS_NAME_C.as_ptr().cast::<c_char>();
    wrapper.xOpen = Some(fault_open);
    PARENT_VFS.store(parent, Ordering::Release);

    let wrapper = Box::into_raw(Box::new(wrapper));
    // SAFETY: `wrapper` is intentionally process-lived and points to a complete
    // VFS table. SQLite owns only the registry link, not the allocation.
    let result = unsafe { ffi::sqlite3_vfs_register(wrapper, 0) };
    if result != ffi::SQLITE_OK {
        // SAFETY: registration failed, so SQLite retained no ownership/link.
        unsafe {
            drop(Box::from_raw(wrapper));
        }
        PARENT_VFS.store(ptr::null_mut(), Ordering::Release);
        return Err(Error::new(
            ErrorCode::StorageBackendUnavailable,
            "fault-injection VFS registration failed",
        ));
    }
    Ok(())
}

#[repr(C)]
struct FaultFile {
    base: ffi::sqlite3_file,
    real: *mut ffi::sqlite3_file,
    target: FaultTarget,
}

unsafe extern "C" fn fault_open(
    _vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    let parent = PARENT_VFS.load(Ordering::Acquire);
    if parent.is_null() || file.is_null() {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: SQLite allocated at least the wrapper VFS's szOsFile bytes.
    let wrapped = file.cast::<FaultFile>();
    #[allow(clippy::cast_ptr_alignment)]
    let real = unsafe {
        file.cast::<u8>()
            .add(FILE_OFFSET)
            .cast::<ffi::sqlite3_file>()
    };
    unsafe {
        ptr::write(
            wrapped,
            FaultFile {
                base: ffi::sqlite3_file {
                    pMethods: ptr::null(),
                },
                real,
                target: target_from_flags(flags),
            },
        );
    }
    // SAFETY: parent is the registered default VFS and xOpen follows SQLite's
    // callback contract.
    let Some(open) = (unsafe { (*parent).xOpen }) else {
        return ffi::SQLITE_CANTOPEN;
    };
    let result = unsafe { open(parent, name, real, flags, out_flags) };
    if result != ffi::SQLITE_OK {
        return result;
    }
    // SAFETY: a successful parent open installs a valid method table.
    let version = unsafe { (*(*real).pMethods).iVersion };
    unsafe {
        (*wrapped).base.pMethods = match version {
            i if i >= 3 => &raw const IO_METHODS_V3,
            2 => &raw const IO_METHODS_V2,
            _ => &raw const IO_METHODS_V1,
        };
    }
    ffi::SQLITE_OK
}

const fn target_from_flags(flags: c_int) -> FaultTarget {
    if flags & ffi::SQLITE_OPEN_WAL != 0 {
        FaultTarget::WriteAheadLog
    } else if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
        FaultTarget::MainDatabase
    } else {
        FaultTarget::Any
    }
}

unsafe fn wrapped_file(file: *mut ffi::sqlite3_file) -> *mut FaultFile {
    file.cast::<FaultFile>()
}

unsafe fn real_file(file: *mut ffi::sqlite3_file) -> *mut ffi::sqlite3_file {
    // SAFETY: every callback is installed only on a successfully opened FaultFile.
    unsafe { (*wrapped_file(file)).real }
}

unsafe fn real_methods(file: *mut ffi::sqlite3_file) -> *const ffi::sqlite3_io_methods {
    // SAFETY: the real file remains open for the lifetime of the wrapper.
    unsafe { (*real_file(file)).pMethods }
}

unsafe fn file_target(file: *mut ffi::sqlite3_file) -> FaultTarget {
    // SAFETY: every callback is installed only on a successfully opened FaultFile.
    unsafe { (*wrapped_file(file)).target }
}

fn intercept(operation: FaultOperation, target: FaultTarget) -> Option<FaultAction> {
    CONTROLLER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .intercept(operation, target)
}

unsafe extern "C" fn io_close(file: *mut ffi::sqlite3_file) -> c_int {
    let methods = unsafe { real_methods(file) };
    let real = unsafe { real_file(file) };
    let result = match unsafe { (*methods).xClose } {
        Some(close) => unsafe { close(real) },
        None => ffi::SQLITE_IOERR,
    };
    unsafe {
        (*wrapped_file(file)).base.pMethods = ptr::null();
    }
    result
}

unsafe extern "C" fn io_read(
    file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    let methods = unsafe { real_methods(file) };
    match unsafe { (*methods).xRead } {
        Some(read) => unsafe { read(real_file(file), buffer, amount, offset) },
        None => ffi::SQLITE_IOERR,
    }
}

unsafe extern "C" fn io_write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    let methods = unsafe { real_methods(file) };
    let Some(write) = (unsafe { (*methods).xWrite }) else {
        return ffi::SQLITE_IOERR_WRITE;
    };
    match intercept(FaultOperation::Write, unsafe { file_target(file) }) {
        Some(FaultAction::Error) => ffi::SQLITE_IOERR_WRITE,
        Some(FaultAction::PartialWrite(prefix)) => {
            let prefix = usize::try_from(amount)
                .ok()
                .map_or(0, |amount| amount.min(prefix));
            if prefix > 0 {
                let result = unsafe {
                    write(
                        real_file(file),
                        buffer,
                        c_int::try_from(prefix).unwrap_or(amount),
                        offset,
                    )
                };
                if result != ffi::SQLITE_OK {
                    return result;
                }
            }
            ffi::SQLITE_IOERR_WRITE
        }
        None => unsafe { write(real_file(file), buffer, amount, offset) },
    }
}

unsafe extern "C" fn io_truncate(file: *mut ffi::sqlite3_file, size: ffi::sqlite3_int64) -> c_int {
    let methods = unsafe { real_methods(file) };
    let Some(truncate) = (unsafe { (*methods).xTruncate }) else {
        return ffi::SQLITE_IOERR_TRUNCATE;
    };
    if intercept(FaultOperation::Truncate, unsafe { file_target(file) }).is_some() {
        ffi::SQLITE_IOERR_TRUNCATE
    } else {
        unsafe { truncate(real_file(file), size) }
    }
}

unsafe extern "C" fn io_sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    let methods = unsafe { real_methods(file) };
    let Some(sync) = (unsafe { (*methods).xSync }) else {
        return ffi::SQLITE_IOERR_FSYNC;
    };
    if intercept(FaultOperation::Sync, unsafe { file_target(file) }).is_some() {
        ffi::SQLITE_IOERR_FSYNC
    } else {
        unsafe { sync(real_file(file), flags) }
    }
}

unsafe extern "C" fn io_file_size(
    file: *mut ffi::sqlite3_file,
    size: *mut ffi::sqlite3_int64,
) -> c_int {
    let methods = unsafe { real_methods(file) };
    match unsafe { (*methods).xFileSize } {
        Some(file_size) => unsafe { file_size(real_file(file), size) },
        None => ffi::SQLITE_IOERR,
    }
}

macro_rules! forward_int_method {
    ($name:ident, $field:ident, ($($arg:ident : $kind:ty),*)) => {
        unsafe extern "C" fn $name(file: *mut ffi::sqlite3_file, $($arg: $kind),*) -> c_int {
            let methods = unsafe { real_methods(file) };
            match unsafe { (*methods).$field } {
                Some(method) => unsafe { method(real_file(file), $($arg),*) },
                None => ffi::SQLITE_IOERR,
            }
        }
    };
}

forward_int_method!(io_lock, xLock, (level: c_int));
forward_int_method!(io_unlock, xUnlock, (level: c_int));
forward_int_method!(io_check_reserved_lock, xCheckReservedLock, (result: *mut c_int));
forward_int_method!(io_file_control, xFileControl, (operation: c_int, argument: *mut c_void));
forward_int_method!(io_shm_map, xShmMap, (page: c_int, page_size: c_int, extend: c_int, output: *mut *mut c_void));
forward_int_method!(io_shm_lock, xShmLock, (offset: c_int, count: c_int, flags: c_int));
forward_int_method!(io_shm_unmap, xShmUnmap, (delete: c_int));

unsafe extern "C" fn io_fetch(
    file: *mut ffi::sqlite3_file,
    offset: ffi::sqlite3_int64,
    amount: c_int,
    output: *mut *mut c_void,
) -> c_int {
    let methods = unsafe { real_methods(file) };
    if let Some(method) = unsafe { (*methods).xFetch } {
        unsafe { method(real_file(file), offset, amount, output) }
    } else {
        if !output.is_null() {
            unsafe { output.write(ptr::null_mut()) };
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn io_unfetch(
    file: *mut ffi::sqlite3_file,
    offset: ffi::sqlite3_int64,
    buffer: *mut c_void,
) -> c_int {
    let methods = unsafe { real_methods(file) };
    match unsafe { (*methods).xUnfetch } {
        Some(method) => unsafe { method(real_file(file), offset, buffer) },
        None => ffi::SQLITE_OK,
    }
}

unsafe extern "C" fn io_sector_size(file: *mut ffi::sqlite3_file) -> c_int {
    let methods = unsafe { real_methods(file) };
    match unsafe { (*methods).xSectorSize } {
        Some(method) => unsafe { method(real_file(file)) },
        None => 0,
    }
}

unsafe extern "C" fn io_device_characteristics(file: *mut ffi::sqlite3_file) -> c_int {
    let methods = unsafe { real_methods(file) };
    match unsafe { (*methods).xDeviceCharacteristics } {
        Some(method) => unsafe { method(real_file(file)) },
        None => 0,
    }
}

unsafe extern "C" fn io_shm_barrier(file: *mut ffi::sqlite3_file) {
    let methods = unsafe { real_methods(file) };
    if let Some(method) = unsafe { (*methods).xShmBarrier } {
        unsafe { method(real_file(file)) };
    }
}

const fn io_methods(version: c_int) -> ffi::sqlite3_io_methods {
    ffi::sqlite3_io_methods {
        iVersion: version,
        xClose: Some(io_close),
        xRead: Some(io_read),
        xWrite: Some(io_write),
        xTruncate: Some(io_truncate),
        xSync: Some(io_sync),
        xFileSize: Some(io_file_size),
        xLock: Some(io_lock),
        xUnlock: Some(io_unlock),
        xCheckReservedLock: Some(io_check_reserved_lock),
        xFileControl: Some(io_file_control),
        xSectorSize: Some(io_sector_size),
        xDeviceCharacteristics: Some(io_device_characteristics),
        xShmMap: Some(io_shm_map),
        xShmLock: Some(io_shm_lock),
        xShmBarrier: Some(io_shm_barrier),
        xShmUnmap: Some(io_shm_unmap),
        xFetch: Some(io_fetch),
        xUnfetch: Some(io_unfetch),
    }
}

static IO_METHODS_V1: ffi::sqlite3_io_methods = io_methods(1);
static IO_METHODS_V2: ffi::sqlite3_io_methods = io_methods(2);
static IO_METHODS_V3: ffi::sqlite3_io_methods = io_methods(3);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_rules_are_rejected_without_panicking() {
        assert_eq!(
            FaultRule::write_error(FaultTarget::Any, 0)
                .expect_err("zero occurrence must fail")
                .code(),
            ErrorCode::InvalidFaultRule
        );
        assert_eq!(
            FaultRule::partial_write(FaultTarget::Any, 1, 0)
                .expect_err("zero prefix must fail")
                .code(),
            ErrorCode::InvalidFaultRule
        );
    }
}
