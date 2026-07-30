use csgdb_core::{
    OpenFlags, ABI_VERSION, DEFAULT_BUSY_TIMEOUT_MS, DEFAULT_CACHE_SIZE_BYTES,
    DEFAULT_MEMORY_BUDGET_BYTES, LIB_VERSION_NUMBER,
};
use std::ffi::{c_char, c_void};
use std::ptr;

static LIB_VERSION_C: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
static SOURCE_ID_C: &[u8] = concat!("csgdb-", env!("CARGO_PKG_VERSION"), "\0").as_bytes();

pub const CSGDB_OK: i32 = 0;
pub const CSGDB_INVALID_ARGUMENT: i32 = 1;

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
            kind: 0,
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

/// Initialize an options structure without reading fields from the caller.
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
    unsafe {
        out_options.write(csgdb_open_options::default());
    }
    CSGDB_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::MaybeUninit;

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
    fn initializer_writes_defaults() {
        let mut options = MaybeUninit::<csgdb_open_options>::uninit();
        let result = unsafe { csgdb_open_options_init(options.as_mut_ptr()) };
        assert_eq!(result, CSGDB_OK);
        let options = unsafe { options.assume_init() };
        assert_ne!(options.flags & OpenFlags::ENCRYPTED.bits(), 0);
    }
}
