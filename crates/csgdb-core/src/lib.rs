//! Core types shared by all CSGDB frontends.
//!
//! This crate intentionally has no storage dependency. It defines the stable
//! open policy, key-provider boundary, error model, and version information
//! that storage backends must obey.

mod error;
mod key;
mod open;
mod version;

pub use error::{Error, ErrorCode, Result};
pub use key::{DatabaseIdentity, KeyProvider, KeySource, SecretKey, SecretString, RAW_KEY_LENGTH};
pub use open::{
    prepare_open, OpenFlags, OpenOptions, OpenPlan, ResolvedKeyRef, ResolvedOpenPlan, SecurityMode,
    DEFAULT_BUSY_TIMEOUT_MS, DEFAULT_CACHE_SIZE_BYTES, DEFAULT_MEMORY_BUDGET_BYTES,
};
pub use version::{ABI_VERSION, LIB_VERSION, LIB_VERSION_NUMBER, SOURCE_ID};
