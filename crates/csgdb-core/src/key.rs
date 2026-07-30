use crate::{Error, ErrorCode, Result};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const RAW_KEY_LENGTH: usize = 32;

/// A fixed-size database key.
///
/// `Debug` never exposes bytes. Memory clearing in `Drop` is best effort until
/// the audited crypto backend introduces a dedicated secret-memory primitive.
pub struct SecretKey {
    bytes: [u8; RAW_KEY_LENGTH],
}

impl SecretKey {
    /// Creates a fixed-length key from raw bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidKeyLength`] when `bytes` is not exactly
    /// [`RAW_KEY_LENGTH`] bytes long.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let bytes: [u8; RAW_KEY_LENGTH] = bytes.try_into().map_err(|_| {
            Error::new(
                ErrorCode::InvalidKeyLength,
                "raw database keys must contain exactly 32 bytes",
            )
        })?;
        Ok(Self { bytes })
    }

    pub fn with_exposed<R>(&self, operation: impl FnOnce(&[u8; RAW_KEY_LENGTH]) -> R) -> R {
        operation(&self.bytes)
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretKey([REDACTED])")
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.bytes.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// A passphrase kept out of diagnostics.
pub struct SecretString {
    bytes: Vec<u8>,
}

impl SecretString {
    #[must_use]
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: value.into(),
        }
    }

    pub fn with_exposed<R>(&self, operation: impl FnOnce(&[u8]) -> R) -> R {
        operation(&self.bytes)
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.bytes.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// Identity passed to platform key providers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DatabaseIdentity {
    path: PathBuf,
}

impl DatabaseIdentity {
    /// Creates a database identity from a path.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidPath`] when the path is empty.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidPath,
                "database path must not be empty",
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Host integration for a system keystore, secure element, TEE, or TPM.
pub trait KeyProvider: Send + Sync {
    /// Loads an existing key for `identity`.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing key store cannot be accessed or its
    /// stored value is invalid.
    fn load(&self, identity: &DatabaseIdentity) -> Result<Option<SecretKey>>;

    /// Creates and persists a new key for `identity`.
    ///
    /// # Errors
    ///
    /// Returns an error when a key cannot be generated or persisted.
    fn create(&self, identity: &DatabaseIdentity) -> Result<SecretKey>;

    /// Removes the key associated with `identity`.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing key store cannot remove the key.
    fn remove(&self, identity: &DatabaseIdentity) -> Result<()>;
}

/// How an encrypted open operation obtains its key material.
#[derive(Default)]
pub enum KeySource {
    #[default]
    Auto,
    Raw(SecretKey),
    Passphrase(SecretString),
    Provider {
        id: String,
        provider: Arc<dyn KeyProvider>,
    },
}

impl KeySource {
    #[must_use]
    pub fn provider(id: impl Into<String>, provider: Arc<dyn KeyProvider>) -> Self {
        Self::Provider {
            id: id.into(),
            provider,
        }
    }

    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Raw(_) => "raw",
            Self::Passphrase(_) => "passphrase",
            Self::Provider { .. } => "provider",
        }
    }
}

impl fmt::Debug for KeySource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => formatter.write_str("KeySource::Auto"),
            Self::Raw(_) => formatter.write_str("KeySource::Raw([REDACTED])"),
            Self::Passphrase(_) => formatter.write_str("KeySource::Passphrase([REDACTED])"),
            Self::Provider { id, .. } => formatter
                .debug_struct("KeySource::Provider")
                .field("id", id)
                .finish_non_exhaustive(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_key_requires_exact_length() {
        let short = [0_u8; RAW_KEY_LENGTH - 1];
        let error = SecretKey::from_slice(&short).expect_err("short key must fail");
        assert_eq!(error.code(), ErrorCode::InvalidKeyLength);
    }

    #[test]
    fn secrets_are_redacted_from_debug() {
        let key = SecretKey::from_slice(&[7_u8; RAW_KEY_LENGTH]).expect("valid key");
        let passphrase = SecretString::new(b"do not print me".to_vec());

        assert_eq!(format!("{key:?}"), "SecretKey([REDACTED])");
        assert_eq!(format!("{passphrase:?}"), "SecretString([REDACTED])");
    }
}
