//! Public Rust API for CSGDB.
//!
//! The M0 slice exposes secure open planning and configuration. It does not
//! yet claim that a production storage or encryption backend is present.

pub use csgdb_core::{
    prepare_open, DatabaseIdentity, Error, ErrorCode, KeyProvider, KeySource, OpenFlags,
    OpenOptions, OpenPlan, ResolvedOpenPlan, Result, SecretKey, SecretString, SecurityMode,
    ABI_VERSION, LIB_VERSION, LIB_VERSION_NUMBER, RAW_KEY_LENGTH, SOURCE_ID,
};

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Builder for a future database connection.
///
/// `plan` validates policy and resolves keys without touching storage. The
/// transaction backend will consume `ResolvedOpenPlan` in M1.
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

    /// Validates the open policy and resolves its key material.
    ///
    /// # Errors
    ///
    /// Returns an error when the path or flags are invalid, encryption has no
    /// usable key source, or the configured key provider fails.
    pub fn plan(self) -> Result<ResolvedOpenPlan> {
        prepare_open(self.path, self.options)?.resolve_key()
    }
}

/// Namespace for the stable connection API planned for M1.
pub struct Database;

impl Database {
    #[must_use]
    pub fn builder(path: impl AsRef<Path>) -> DatabaseBuilder {
        DatabaseBuilder::new(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_is_explicit_even_with_db_extension() {
        let plan = Database::builder("legacy.db")
            .plaintext()
            .plan()
            .expect("plaintext plan");
        assert_eq!(plan.security(), SecurityMode::Plaintext);
    }

    #[test]
    fn default_builder_does_not_fall_back_to_plaintext() {
        let error = Database::builder("agent.db")
            .plan()
            .expect_err("missing provider must fail");
        assert_eq!(error.code(), ErrorCode::KeyStoreUnavailable);
    }
}
