use crate::{
    DatabaseIdentity, Error, ErrorCode, KeyProvider, KeySource, Result, SecretKey, SecretString,
};
use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_BUSY_TIMEOUT_MS: u32 = 5_000;
pub const DEFAULT_CACHE_SIZE_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_VFS_NAME_BYTES: usize = 255;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct OpenFlags(u32);

impl OpenFlags {
    const ALL_BITS: u32 = Self::READONLY.0
        | Self::READWRITE.0
        | Self::CREATE.0
        | Self::URI.0
        | Self::MEMORY.0
        | Self::ENCRYPTED.0
        | Self::PLAINTEXT.0
        | Self::FULLMUTEX.0
        | Self::NOMUTEX.0
        | Self::NOFOLLOW.0;

    pub const READONLY: Self = Self(0x0001);
    pub const READWRITE: Self = Self(0x0002);
    pub const CREATE: Self = Self(0x0004);
    pub const URI: Self = Self(0x0008);
    pub const MEMORY: Self = Self(0x0010);
    pub const ENCRYPTED: Self = Self(0x0020);
    pub const PLAINTEXT: Self = Self(0x0040);
    pub const FULLMUTEX: Self = Self(0x0080);
    pub const NOMUTEX: Self = Self(0x0100);
    pub const NOFOLLOW: Self = Self(0x0200);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    #[must_use]
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::ALL_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
}

impl BitOr for OpenFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for OpenFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SecurityMode {
    Encrypted,
    Plaintext,
}

/// Stable configuration shared by Rust and C frontends.
pub struct OpenOptions {
    pub flags: OpenFlags,
    pub busy_timeout: Duration,
    pub cache_size_bytes: u64,
    pub memory_budget_bytes: u64,
    /// Optional registered storage VFS name. `None` selects the platform default.
    pub vfs: Option<String>,
    pub key: KeySource,
    pub auto_key_provider: Option<Arc<dyn KeyProvider>>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            flags: OpenFlags::READWRITE
                | OpenFlags::CREATE
                | OpenFlags::ENCRYPTED
                | OpenFlags::FULLMUTEX,
            busy_timeout: Duration::from_millis(u64::from(DEFAULT_BUSY_TIMEOUT_MS)),
            cache_size_bytes: DEFAULT_CACHE_SIZE_BYTES,
            memory_budget_bytes: DEFAULT_MEMORY_BUDGET_BYTES,
            vfs: None,
            key: KeySource::Auto,
            auto_key_provider: None,
        }
    }
}

impl fmt::Debug for OpenOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenOptions")
            .field("flags", &self.flags)
            .field("busy_timeout", &self.busy_timeout)
            .field("cache_size_bytes", &self.cache_size_bytes)
            .field("memory_budget_bytes", &self.memory_budget_bytes)
            .field("vfs", &self.vfs)
            .field("key", &self.key)
            .field(
                "auto_key_provider",
                &self.auto_key_provider.as_ref().map(|_| "[CONFIGURED]"),
            )
            .finish()
    }
}

/// Validated request before key resolution and storage I/O.
pub struct OpenPlan {
    path: PathBuf,
    identity: DatabaseIdentity,
    flags: OpenFlags,
    security: SecurityMode,
    busy_timeout: Duration,
    cache_size_bytes: u64,
    memory_budget_bytes: u64,
    vfs: Option<String>,
    key: KeySource,
    auto_key_provider: Option<Arc<dyn KeyProvider>>,
}

impl fmt::Debug for OpenPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenPlan")
            .field("path", &self.path)
            .field("identity", &self.identity)
            .field("flags", &self.flags)
            .field("security", &self.security)
            .field("busy_timeout", &self.busy_timeout)
            .field("cache_size_bytes", &self.cache_size_bytes)
            .field("memory_budget_bytes", &self.memory_budget_bytes)
            .field("vfs", &self.vfs)
            .field("key", &self.key)
            .field(
                "auto_key_provider",
                &self.auto_key_provider.as_ref().map(|_| "[CONFIGURED]"),
            )
            .finish()
    }
}

impl OpenPlan {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn flags(&self) -> OpenFlags {
        self.flags
    }

    #[must_use]
    pub const fn security(&self) -> SecurityMode {
        self.security
    }

    #[must_use]
    pub const fn busy_timeout(&self) -> Duration {
        self.busy_timeout
    }

    #[must_use]
    pub const fn cache_size_bytes(&self) -> u64 {
        self.cache_size_bytes
    }

    #[must_use]
    pub const fn memory_budget_bytes(&self) -> u64 {
        self.memory_budget_bytes
    }

    #[must_use]
    pub fn vfs(&self) -> Option<&str> {
        self.vfs.as_deref()
    }

    /// Resolves the selected key source into an executable open plan.
    ///
    /// # Errors
    ///
    /// Returns an error when encryption is enabled but no usable key can be
    /// resolved, or when the configured key provider fails.
    pub fn resolve_key(self) -> Result<ResolvedOpenPlan> {
        let key = match self.security {
            SecurityMode::Plaintext => None,
            SecurityMode::Encrypted => Some(resolve_encrypted_key(
                &self.identity,
                self.flags,
                self.key,
                self.auto_key_provider,
            )?),
        };

        Ok(ResolvedOpenPlan {
            path: self.path,
            flags: self.flags,
            security: self.security,
            busy_timeout: self.busy_timeout,
            cache_size_bytes: self.cache_size_bytes,
            memory_budget_bytes: self.memory_budget_bytes,
            vfs: self.vfs,
            key,
        })
    }
}

/// Validated open request with key material resolved.
pub struct ResolvedOpenPlan {
    path: PathBuf,
    flags: OpenFlags,
    security: SecurityMode,
    busy_timeout: Duration,
    cache_size_bytes: u64,
    memory_budget_bytes: u64,
    vfs: Option<String>,
    key: Option<ResolvedKey>,
}

/// Borrowed key material exposed only to a storage backend.
#[derive(Clone, Copy)]
pub enum ResolvedKeyRef<'key> {
    Raw(&'key [u8; 32]),
    Passphrase(&'key [u8]),
}

impl fmt::Debug for ResolvedKeyRef<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Raw(_) => formatter.write_str("ResolvedKeyRef::Raw([REDACTED])"),
            Self::Passphrase(_) => formatter.write_str("ResolvedKeyRef::Passphrase([REDACTED])"),
        }
    }
}

enum ResolvedKey {
    Raw(SecretKey),
    Passphrase(SecretString),
}

impl ResolvedOpenPlan {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn flags(&self) -> OpenFlags {
        self.flags
    }

    #[must_use]
    pub const fn security(&self) -> SecurityMode {
        self.security
    }

    #[must_use]
    pub const fn busy_timeout(&self) -> Duration {
        self.busy_timeout
    }

    #[must_use]
    pub const fn cache_size_bytes(&self) -> u64 {
        self.cache_size_bytes
    }

    #[must_use]
    pub const fn memory_budget_bytes(&self) -> u64 {
        self.memory_budget_bytes
    }

    #[must_use]
    pub fn vfs(&self) -> Option<&str> {
        self.vfs.as_deref()
    }

    #[must_use]
    pub const fn has_key(&self) -> bool {
        self.key.is_some()
    }

    pub fn with_key<R>(&self, operation: impl FnOnce(Option<ResolvedKeyRef<'_>>) -> R) -> R {
        match &self.key {
            Some(ResolvedKey::Raw(key)) => {
                key.with_exposed(|bytes| operation(Some(ResolvedKeyRef::Raw(bytes))))
            }
            Some(ResolvedKey::Passphrase(passphrase)) => {
                passphrase.with_exposed(|bytes| operation(Some(ResolvedKeyRef::Passphrase(bytes))))
            }
            None => operation(None),
        }
    }
}

impl fmt::Debug for ResolvedOpenPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedOpenPlan")
            .field("path", &self.path)
            .field("flags", &self.flags)
            .field("security", &self.security)
            .field("busy_timeout", &self.busy_timeout)
            .field("cache_size_bytes", &self.cache_size_bytes)
            .field("memory_budget_bytes", &self.memory_budget_bytes)
            .field("vfs", &self.vfs)
            .field("key", &self.key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// Normalizes and validates an open request without touching storage.
///
/// # Errors
///
/// Returns an error when the path is empty or when the option combination
/// violates the secure-open policy.
pub fn prepare_open(path: impl AsRef<Path>, mut options: OpenOptions) -> Result<OpenPlan> {
    let path = path.as_ref();
    let identity = DatabaseIdentity::from_path(path)?;
    options.flags = normalize_and_validate_flags(options.flags)?;
    validate_vfs(options.vfs.as_deref())?;
    let security = if options.flags.contains(OpenFlags::PLAINTEXT) {
        SecurityMode::Plaintext
    } else {
        SecurityMode::Encrypted
    };

    Ok(OpenPlan {
        path: path.to_path_buf(),
        identity,
        flags: options.flags,
        security,
        busy_timeout: options.busy_timeout,
        cache_size_bytes: options.cache_size_bytes,
        memory_budget_bytes: options.memory_budget_bytes,
        vfs: options.vfs,
        key: options.key,
        auto_key_provider: options.auto_key_provider,
    })
}

fn validate_vfs(vfs: Option<&str>) -> Result<()> {
    let Some(vfs) = vfs else {
        return Ok(());
    };
    if vfs.is_empty() {
        return Err(Error::new(
            ErrorCode::InvalidVfs,
            "VFS name must not be empty",
        ));
    }
    if vfs.len() > MAX_VFS_NAME_BYTES {
        return Err(Error::new(
            ErrorCode::InvalidVfs,
            "VFS name exceeds the supported byte length",
        ));
    }
    if vfs.as_bytes().contains(&0) {
        return Err(Error::new(
            ErrorCode::InvalidVfs,
            "VFS name must not contain a NUL byte",
        ));
    }
    Ok(())
}

fn normalize_and_validate_flags(mut flags: OpenFlags) -> Result<OpenFlags> {
    if flags.contains(OpenFlags::READONLY) && flags.contains(OpenFlags::READWRITE) {
        return Err(invalid_flags(
            "READONLY and READWRITE are mutually exclusive",
        ));
    }
    if !flags.intersects(OpenFlags::READONLY | OpenFlags::READWRITE) {
        flags |= OpenFlags::READWRITE;
    }
    if flags.contains(OpenFlags::CREATE) && !flags.contains(OpenFlags::READWRITE) {
        return Err(invalid_flags("CREATE requires READWRITE"));
    }
    if flags.contains(OpenFlags::ENCRYPTED) && flags.contains(OpenFlags::PLAINTEXT) {
        return Err(invalid_flags(
            "ENCRYPTED and PLAINTEXT are mutually exclusive",
        ));
    }
    if !flags.intersects(OpenFlags::ENCRYPTED | OpenFlags::PLAINTEXT) {
        flags |= OpenFlags::ENCRYPTED;
    }
    if flags.contains(OpenFlags::FULLMUTEX) && flags.contains(OpenFlags::NOMUTEX) {
        return Err(invalid_flags(
            "FULLMUTEX and NOMUTEX are mutually exclusive",
        ));
    }
    Ok(flags)
}

fn invalid_flags(message: &'static str) -> Error {
    Error::new(ErrorCode::InvalidOpenFlags, message)
}

fn resolve_encrypted_key(
    identity: &DatabaseIdentity,
    flags: OpenFlags,
    key_source: KeySource,
    auto_provider: Option<Arc<dyn KeyProvider>>,
) -> Result<ResolvedKey> {
    match key_source {
        KeySource::Raw(key) => Ok(ResolvedKey::Raw(key)),
        KeySource::Passphrase(passphrase) => {
            if passphrase.with_exposed(<[u8]>::is_empty) {
                return Err(Error::new(
                    ErrorCode::InvalidKeyLength,
                    "database passphrase must not be empty",
                ));
            }
            Ok(ResolvedKey::Passphrase(passphrase))
        }
        KeySource::Provider { provider, .. } => {
            load_or_create_key(provider.as_ref(), identity, flags).map(ResolvedKey::Raw)
        }
        KeySource::Auto => {
            let provider = auto_provider.ok_or_else(|| {
                Error::new(
                    ErrorCode::KeyStoreUnavailable,
                    "encrypted open requires a configured key provider",
                )
            })?;
            load_or_create_key(provider.as_ref(), identity, flags).map(ResolvedKey::Raw)
        }
    }
}

fn load_or_create_key(
    provider: &dyn KeyProvider,
    identity: &DatabaseIdentity,
    flags: OpenFlags,
) -> Result<SecretKey> {
    if let Some(key) = provider.load(identity)? {
        return Ok(key);
    }
    if flags.contains(OpenFlags::CREATE) {
        return provider.create(identity);
    }
    Err(Error::new(
        ErrorCode::KeyRequired,
        "no key exists for the encrypted database",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RAW_KEY_LENGTH;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestProvider {
        loads: AtomicUsize,
        creates: AtomicUsize,
        existing: bool,
    }

    impl TestProvider {
        fn new(existing: bool) -> Self {
            Self {
                loads: AtomicUsize::new(0),
                creates: AtomicUsize::new(0),
                existing,
            }
        }
    }

    impl KeyProvider for TestProvider {
        fn load(&self, _identity: &DatabaseIdentity) -> Result<Option<SecretKey>> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.existing
                .then(|| SecretKey::from_slice(&[1_u8; RAW_KEY_LENGTH]))
                .transpose()
        }

        fn create(&self, _identity: &DatabaseIdentity) -> Result<SecretKey> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            SecretKey::from_slice(&[2_u8; RAW_KEY_LENGTH])
        }

        fn remove(&self, _identity: &DatabaseIdentity) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn default_open_is_encrypted_and_uses_db_extension_without_special_handling() {
        let plan = prepare_open("agent.db", OpenOptions::default()).expect("valid plan");
        assert_eq!(plan.path(), Path::new("agent.db"));
        assert_eq!(plan.security(), SecurityMode::Encrypted);
        assert!(plan.flags().contains(OpenFlags::ENCRYPTED));
        assert!(!plan.flags().contains(OpenFlags::PLAINTEXT));
    }

    #[test]
    fn extension_does_not_decide_security_mode() {
        let options = OpenOptions {
            flags: OpenFlags::READWRITE | OpenFlags::CREATE | OpenFlags::PLAINTEXT,
            ..OpenOptions::default()
        };
        let plan = prepare_open("also.db", options).expect("valid plaintext plan");
        assert_eq!(plan.security(), SecurityMode::Plaintext);
    }

    #[test]
    fn named_vfs_is_validated_and_preserved_in_the_resolved_plan() {
        let options = OpenOptions {
            vfs: Some("registered-test-vfs".to_owned()),
            key: KeySource::Passphrase(SecretString::new("secret")),
            ..OpenOptions::default()
        };
        let resolved = prepare_open("agent.db", options)
            .expect("valid plan")
            .resolve_key()
            .expect("resolve key");
        assert_eq!(resolved.vfs(), Some("registered-test-vfs"));
    }

    #[test]
    fn invalid_vfs_names_are_rejected_before_storage_io() {
        for name in [
            String::new(),
            "invalid\0vfs".to_owned(),
            "v".repeat(MAX_VFS_NAME_BYTES + 1),
        ] {
            let options = OpenOptions {
                vfs: Some(name),
                ..OpenOptions::default()
            };
            let error = prepare_open("agent.db", options).expect_err("invalid VFS must fail");
            assert_eq!(error.code(), ErrorCode::InvalidVfs);
        }
    }

    #[test]
    fn encrypted_open_fails_closed_without_provider() {
        let error = prepare_open("agent.db", OpenOptions::default())
            .expect("valid plan")
            .resolve_key()
            .expect_err("missing provider must fail");
        assert_eq!(error.code(), ErrorCode::KeyStoreUnavailable);
    }

    #[test]
    fn provider_creates_key_for_new_database() {
        let provider = Arc::new(TestProvider::new(false));
        let options = OpenOptions {
            auto_key_provider: Some(provider.clone()),
            ..OpenOptions::default()
        };

        let resolved = prepare_open("agent.db", options)
            .expect("valid plan")
            .resolve_key()
            .expect("provider creates a key");

        assert!(resolved.has_key());
        assert_eq!(provider.loads.load(Ordering::SeqCst), 1);
        assert_eq!(provider.creates.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_loads_existing_key_without_creating() {
        let provider = Arc::new(TestProvider::new(true));
        let options = OpenOptions {
            flags: OpenFlags::READWRITE | OpenFlags::ENCRYPTED,
            auto_key_provider: Some(provider.clone()),
            ..OpenOptions::default()
        };

        let resolved = prepare_open("agent.db", options)
            .expect("valid plan")
            .resolve_key()
            .expect("provider loads the key");

        assert!(resolved.has_key());
        assert_eq!(provider.loads.load(Ordering::SeqCst), 1);
        assert_eq!(provider.creates.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn conflicting_security_flags_are_rejected() {
        let options = OpenOptions {
            flags: OpenFlags::READWRITE | OpenFlags::ENCRYPTED | OpenFlags::PLAINTEXT,
            ..OpenOptions::default()
        };
        let error = prepare_open("agent.db", options).expect_err("conflict must fail");
        assert_eq!(error.code(), ErrorCode::InvalidOpenFlags);
    }

    #[test]
    fn plaintext_open_never_resolves_a_key() {
        let options = OpenOptions {
            flags: OpenFlags::READWRITE | OpenFlags::CREATE | OpenFlags::PLAINTEXT,
            ..OpenOptions::default()
        };
        let resolved = prepare_open("legacy.db", options)
            .expect("valid plan")
            .resolve_key()
            .expect("plaintext does not need a provider");
        assert!(!resolved.has_key());
    }

    #[test]
    fn passphrase_is_resolved_without_exposing_it() {
        let options = OpenOptions {
            key: KeySource::Passphrase(SecretString::new("correct horse battery staple")),
            ..OpenOptions::default()
        };
        let resolved = prepare_open("agent.db", options)
            .expect("valid plan")
            .resolve_key()
            .expect("passphrase resolves");
        let kind = resolved.with_key(|key| match key {
            Some(ResolvedKeyRef::Passphrase(bytes)) => {
                assert_eq!(bytes.len(), 28);
                "passphrase"
            }
            _ => "other",
        });
        assert_eq!(kind, "passphrase");
    }

    #[test]
    fn empty_passphrase_is_rejected() {
        let options = OpenOptions {
            key: KeySource::Passphrase(SecretString::new([])),
            ..OpenOptions::default()
        };
        let error = prepare_open("agent.db", options)
            .expect("valid plan")
            .resolve_key()
            .expect_err("empty passphrase must fail");
        assert_eq!(error.code(), ErrorCode::InvalidKeyLength);
    }
}
