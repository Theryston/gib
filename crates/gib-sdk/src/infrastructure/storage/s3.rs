use crate::api::CancellationToken;
use crate::application::ports::{
    ObjectCursor, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectRange,
    ObjectRead, ObjectWriteOptions, RepositoryStorage, StorageCapabilities, StorageError,
    StorageResult, StorageVersion, StorageWriteCondition,
};
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Cursor, Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::runtime::{Builder as RuntimeBuilder, Handle};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Semaphore, mpsc::Sender as TokioSender};
use tokio::task::{AbortHandle, JoinSet};
use url::Url;

/// The smallest non-final S3 multipart part accepted by the AWS API.
pub const MIN_S3_MULTIPART_PART_SIZE: u64 = 5 * 1024 * 1024;

/// The largest multipart part accepted by this adapter.
///
/// AWS permits larger parts, but this SDK bound keeps the configured in-flight
/// memory budget predictable on all supported platforms.
pub const MAX_S3_MULTIPART_PART_SIZE: u64 = 64 * 1024 * 1024;

/// The default size at which an upload changes from `PutObject` to multipart.
pub const DEFAULT_S3_MULTIPART_THRESHOLD: u64 = 8 * 1024 * 1024;

/// The default S3 multipart part size.
pub const DEFAULT_S3_MULTIPART_PART_SIZE: u64 = 8 * 1024 * 1024;

/// The largest threshold accepted by this adapter.
pub const MAX_S3_MULTIPART_THRESHOLD: u64 = MAX_S3_MULTIPART_PART_SIZE;

/// The maximum number of parts accepted by S3 for one multipart upload.
pub const MAX_S3_MULTIPART_UPLOAD_PARTS: u32 = 10_000;

/// The default number of concurrent multipart part requests.
pub const DEFAULT_S3_MAX_CONCURRENCY: usize = 4;

/// The name used by the default S3 conditional-write capability cache.
pub const DEFAULT_S3_CAPABILITY_CACHE_FILE_NAME: &str = "s3-capabilities.msgpack";

/// The number of seconds for which a positive or negative capability result is
/// trusted before the endpoint is probed again.
pub const DEFAULT_S3_CAPABILITY_CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;

/// The result of one S3 conditional-write capability probe.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum S3ConditionalWriteStatus {
    /// The provider enforced the requested native precondition.
    Supported,
    /// The provider explicitly rejected or ignored the requested precondition.
    Unsupported,
    /// The probe could not establish a result, commonly because of access or
    /// transient network failure.
    Inconclusive,
}

impl fmt::Display for S3ConditionalWriteStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Inconclusive => "inconclusive",
        };
        formatter.write_str(value)
    }
}

/// Independently detected native conditional-write capabilities for one S3
/// endpoint and bucket.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct S3ConditionalWriteCapabilities {
    create_if_absent: S3ConditionalWriteStatus,
    replace_if_version: S3ConditionalWriteStatus,
}

impl S3ConditionalWriteCapabilities {
    /// Returns the status of native create-if-absent writes.
    pub const fn create_if_absent(self) -> S3ConditionalWriteStatus {
        self.create_if_absent
    }

    /// Returns the status of native replace-if-version writes.
    pub const fn replace_if_version(self) -> S3ConditionalWriteStatus {
        self.replace_if_version
    }

    /// Returns whether native create-if-absent writes are available.
    pub const fn supports_create_if_absent(self) -> bool {
        matches!(self.create_if_absent, S3ConditionalWriteStatus::Supported)
    }

    /// Returns whether native replace-if-version writes are available.
    pub const fn supports_replace_if_version(self) -> bool {
        matches!(self.replace_if_version, S3ConditionalWriteStatus::Supported)
    }

    /// Returns whether both native conditional-write forms are available.
    pub const fn supports_conditional_writes(self) -> bool {
        self.supports_create_if_absent() && self.supports_replace_if_version()
    }

    const fn inconclusive() -> Self {
        Self {
            create_if_absent: S3ConditionalWriteStatus::Inconclusive,
            replace_if_version: S3ConditionalWriteStatus::Inconclusive,
        }
    }

    const fn is_complete(self) -> bool {
        !matches!(
            self.create_if_absent,
            S3ConditionalWriteStatus::Inconclusive
        ) && !matches!(
            self.replace_if_version,
            S3ConditionalWriteStatus::Inconclusive
        )
    }

    const fn has_definitive_result(self) -> bool {
        !matches!(
            self.create_if_absent,
            S3ConditionalWriteStatus::Inconclusive
        ) || !matches!(
            self.replace_if_version,
            S3ConditionalWriteStatus::Inconclusive
        )
    }

    const fn with_create_if_absent(self, status: S3ConditionalWriteStatus) -> Self {
        Self {
            create_if_absent: status,
            ..self
        }
    }

    const fn with_replace_if_version(self, status: S3ConditionalWriteStatus) -> Self {
        Self {
            replace_if_version: status,
            ..self
        }
    }
}

impl Default for S3ConditionalWriteCapabilities {
    fn default() -> Self {
        Self::inconclusive()
    }
}

const MAX_S3_MAX_CONCURRENCY: usize = 64;
const STREAM_CHANNEL_CAPACITY: usize = 2;
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_REGION_LENGTH: usize = 128;
const MAX_BUCKET_LENGTH: usize = 63;
const MAX_CREDENTIAL_LENGTH: usize = 4096;
const MAX_ENDPOINT_LENGTH: usize = 2048;
const S3_CAPABILITY_CACHE_SCHEMA_VERSION: u16 = 1;
const MAX_S3_CAPABILITY_CACHE_BYTES: u64 = 1024 * 1024;
const MAX_S3_CAPABILITY_CACHE_ENTRIES: usize = 1_024;
const S3_CAPABILITY_PROBE_PREFIX: &str = "gib-capability-probe";

static NEXT_S3_CAPABILITY_PROBE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_S3_CAPABILITY_CACHE_TEMP_ID: AtomicU64 = AtomicU64::new(1);
static S3_CAPABILITY_CACHE_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> =
    OnceLock::new();

/// Validated, provider-neutral configuration for [`S3Storage`].
///
/// Credentials are retained only by the adapter and are deliberately omitted
/// from `Debug`. Custom endpoints are also omitted from `Debug` because a
/// signed URL or an endpoint containing user information must never become a
/// log or event value. The `s3` Cargo feature is required.
#[derive(Clone, Eq, PartialEq)]
pub struct S3StorageConfig {
    region: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    endpoint: Option<String>,
    force_path_style: bool,
    force_path_style_explicit: bool,
    multipart_threshold: u64,
    multipart_part_size: u64,
    max_concurrency: usize,
    capability_cache_path: Option<PathBuf>,
}

impl S3StorageConfig {
    /// Creates a configuration using explicit long-lived or temporary
    /// credentials and the standard AWS S3 endpoint.
    ///
    /// When the host exposes a platform cache directory, conditional-write
    /// results are persisted there by default. Use [`Self::with_capability_cache_path`]
    /// to select an explicit file or [`Self::without_capability_cache`] for an
    /// ephemeral adapter.
    ///
    /// The input is validated before it is returned. Builder methods may
    /// change optional values; [`S3Storage::new`] validates the complete
    /// configuration again before constructing a client.
    pub fn new(
        region: impl Into<String>,
        bucket: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> StorageResult<Self> {
        let config = Self {
            region: region.into(),
            bucket: bucket.into(),
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            session_token: None,
            endpoint: None,
            force_path_style: false,
            force_path_style_explicit: false,
            multipart_threshold: DEFAULT_S3_MULTIPART_THRESHOLD,
            multipart_part_size: DEFAULT_S3_MULTIPART_PART_SIZE,
            max_concurrency: DEFAULT_S3_MAX_CONCURRENCY,
            capability_cache_path: default_s3_capability_cache_path(),
        };
        config.validate()?;
        Ok(config)
    }

    /// Adds an optional temporary-session token.
    pub fn with_session_token(mut self, session_token: impl Into<String>) -> Self {
        self.session_token = Some(session_token.into());
        self
    }

    /// Uses a custom S3-compatible endpoint such as MinIO or LocalStack.
    ///
    /// Custom endpoints default to path-style addressing. Call
    /// [`Self::with_force_path_style`] afterwards when a compatible service
    /// requires virtual-hosted-style addressing instead.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        if !self.force_path_style_explicit {
            self.force_path_style = true;
        }
        self
    }

    /// Selects path-style or virtual-hosted-style object addressing.
    pub const fn with_force_path_style(mut self, force_path_style: bool) -> Self {
        self.force_path_style = force_path_style;
        self.force_path_style_explicit = true;
        self
    }

    /// Sets the bounded-object threshold for multipart uploads.
    pub const fn with_multipart_threshold(mut self, threshold: u64) -> Self {
        self.multipart_threshold = threshold;
        self
    }

    /// Sets the bounded multipart part size.
    pub const fn with_multipart_part_size(mut self, part_size: u64) -> Self {
        self.multipart_part_size = part_size;
        self
    }

    /// Sets the maximum number of concurrent multipart part requests.
    pub const fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    /// Stores capability results in the supplied local cache file.
    ///
    /// The file contains only the endpoint, region, bucket, capability
    /// statuses, and cache timestamps. Credentials are never persisted.
    pub fn with_capability_cache_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.capability_cache_path = Some(path.into());
        self
    }

    /// Disables persistence of capability results while retaining in-process
    /// caching. This is useful for ephemeral applications and tests.
    pub fn without_capability_cache(mut self) -> Self {
        self.capability_cache_path = None;
        self
    }

    /// Returns the configured AWS region.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// Returns the configured bucket name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Returns whether a custom endpoint was configured without exposing its
    /// value.
    pub const fn has_custom_endpoint(&self) -> bool {
        self.endpoint.is_some()
    }

    /// Returns the configured multipart threshold.
    pub const fn multipart_threshold(&self) -> u64 {
        self.multipart_threshold
    }

    /// Returns the configured multipart part size.
    pub const fn multipart_part_size(&self) -> u64 {
        self.multipart_part_size
    }

    /// Returns the configured multipart request concurrency.
    pub const fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    /// Returns the configured capability-cache file, if persistence is enabled.
    pub fn capability_cache_path(&self) -> Option<&Path> {
        self.capability_cache_path.as_deref()
    }

    fn validate(&self) -> StorageResult<()> {
        validate_text(&self.region, MAX_REGION_LENGTH, true)?;
        validate_bucket(&self.bucket)?;
        validate_text(&self.access_key, MAX_CREDENTIAL_LENGTH, true)?;
        validate_text(&self.secret_key, MAX_CREDENTIAL_LENGTH, true)?;
        if let Some(session_token) = &self.session_token {
            validate_text(session_token, MAX_CREDENTIAL_LENGTH, true)?;
        }
        if let Some(endpoint) = &self.endpoint {
            validate_endpoint(endpoint)?;
        }
        if self.multipart_threshold == 0 || self.multipart_threshold > MAX_S3_MULTIPART_THRESHOLD {
            return Err(StorageError::InvalidRequest);
        }
        if !(MIN_S3_MULTIPART_PART_SIZE..=MAX_S3_MULTIPART_PART_SIZE)
            .contains(&self.multipart_part_size)
        {
            return Err(StorageError::InvalidRequest);
        }
        if !(1..=MAX_S3_MAX_CONCURRENCY).contains(&self.max_concurrency) {
            return Err(StorageError::InvalidRequest);
        }
        if self
            .capability_cache_path
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty() || path.file_name().is_none())
        {
            return Err(StorageError::InvalidRequest);
        }
        Ok(())
    }
}

impl fmt::Debug for S3StorageConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3StorageConfig")
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .field("endpoint", &self.endpoint.as_ref().map(|_| "<redacted>"))
            .field("force_path_style", &self.force_path_style)
            .field("multipart_threshold", &self.multipart_threshold)
            .field("multipart_part_size", &self.multipart_part_size)
            .field("max_concurrency", &self.max_concurrency)
            .field(
                "capability_cache_path",
                &self.capability_cache_path.as_ref().map(|_| "<configured>"),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct S3CapabilityCacheKey {
    endpoint: Option<String>,
    region: String,
    bucket: String,
}

impl S3CapabilityCacheKey {
    fn from_config(config: &S3StorageConfig) -> Self {
        Self {
            endpoint: config.endpoint.clone(),
            region: config.region.clone(),
            bucket: config.bucket.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct S3CapabilityCacheFile {
    version: u16,
    entries: Vec<S3CapabilityCacheEntry>,
}

impl S3CapabilityCacheFile {
    fn empty() -> Self {
        Self {
            version: S3_CAPABILITY_CACHE_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct S3CapabilityCacheEntry {
    endpoint: Option<String>,
    region: String,
    bucket: String,
    observed_at_unix_seconds: u64,
    create_if_absent: Option<bool>,
    replace_if_version: Option<bool>,
}

impl S3CapabilityCacheEntry {
    fn for_capabilities(
        key: &S3CapabilityCacheKey,
        capabilities: S3ConditionalWriteCapabilities,
    ) -> Self {
        Self {
            endpoint: key.endpoint.clone(),
            region: key.region.clone(),
            bucket: key.bucket.clone(),
            observed_at_unix_seconds: current_unix_seconds(),
            create_if_absent: status_as_cache_value(capabilities.create_if_absent()),
            replace_if_version: status_as_cache_value(capabilities.replace_if_version()),
        }
    }

    fn matches(&self, key: &S3CapabilityCacheKey) -> bool {
        self.endpoint.as_deref() == key.endpoint.as_deref()
            && self.region == key.region
            && self.bucket == key.bucket
    }

    fn capabilities_if_fresh(
        &self,
        key: &S3CapabilityCacheKey,
        now: u64,
    ) -> Option<S3ConditionalWriteCapabilities> {
        if !self.matches(key)
            || now < self.observed_at_unix_seconds
            || now - self.observed_at_unix_seconds > DEFAULT_S3_CAPABILITY_CACHE_TTL_SECONDS
        {
            return None;
        }
        Some(S3ConditionalWriteCapabilities {
            create_if_absent: cache_value_as_status(self.create_if_absent),
            replace_if_version: cache_value_as_status(self.replace_if_version),
        })
    }
}

struct S3CapabilityState {
    key: S3CapabilityCacheKey,
    cache_path: Option<PathBuf>,
    capabilities: Mutex<S3ConditionalWriteCapabilities>,
    probe_lock: Mutex<()>,
}

impl S3CapabilityState {
    fn new(config: &S3StorageConfig) -> Self {
        let key = S3CapabilityCacheKey::from_config(config);
        let capabilities = config
            .capability_cache_path
            .as_deref()
            .and_then(|path| load_cached_capabilities(path, &key))
            .unwrap_or_default();
        Self {
            key,
            cache_path: config.capability_cache_path.clone(),
            capabilities: Mutex::new(capabilities),
            probe_lock: Mutex::new(()),
        }
    }

    fn snapshot(&self) -> S3ConditionalWriteCapabilities {
        self.capabilities
            .lock()
            .map(|capabilities| *capabilities)
            .unwrap_or_default()
    }

    fn set(&self, capabilities: S3ConditionalWriteCapabilities) -> StorageResult<()> {
        let mut current = self
            .capabilities
            .lock()
            .map_err(|_| StorageError::Unavailable)?;
        *current = capabilities;
        Ok(())
    }

    fn cache_result(&self, capabilities: S3ConditionalWriteCapabilities) {
        if !capabilities.has_definitive_result() {
            return;
        }
        let Some(path) = self.cache_path.as_deref() else {
            return;
        };
        let _ = persist_cached_capabilities(path, &self.key, capabilities);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeOutcome {
    Supported,
    Unsupported,
    Inconclusive(StorageError),
}

fn status_as_cache_value(status: S3ConditionalWriteStatus) -> Option<bool> {
    match status {
        S3ConditionalWriteStatus::Supported => Some(true),
        S3ConditionalWriteStatus::Unsupported => Some(false),
        S3ConditionalWriteStatus::Inconclusive => None,
    }
}

fn cache_value_as_status(value: Option<bool>) -> S3ConditionalWriteStatus {
    match value {
        Some(true) => S3ConditionalWriteStatus::Supported,
        Some(false) => S3ConditionalWriteStatus::Unsupported,
        None => S3ConditionalWriteStatus::Inconclusive,
    }
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn load_cached_capabilities(
    path: &Path,
    key: &S3CapabilityCacheKey,
) -> Option<S3ConditionalWriteCapabilities> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() == 0 || metadata.len() > MAX_S3_CAPABILITY_CACHE_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let cache = decode_capability_cache(&bytes)?;
    if cache.version != S3_CAPABILITY_CACHE_SCHEMA_VERSION
        || cache.entries.len() > MAX_S3_CAPABILITY_CACHE_ENTRIES
    {
        return None;
    }
    let now = current_unix_seconds();
    cache
        .entries
        .iter()
        .find_map(|entry| entry.capabilities_if_fresh(key, now))
}

fn read_capability_cache(path: &Path) -> S3CapabilityCacheFile {
    let Ok(metadata) = fs::metadata(path) else {
        return S3CapabilityCacheFile::empty();
    };
    if metadata.len() == 0 || metadata.len() > MAX_S3_CAPABILITY_CACHE_BYTES {
        return S3CapabilityCacheFile::empty();
    }
    let Ok(bytes) = fs::read(path) else {
        return S3CapabilityCacheFile::empty();
    };
    match decode_capability_cache(&bytes) {
        Some(cache)
            if cache.version == S3_CAPABILITY_CACHE_SCHEMA_VERSION
                && cache.entries.len() <= MAX_S3_CAPABILITY_CACHE_ENTRIES =>
        {
            cache
        }
        _ => S3CapabilityCacheFile::empty(),
    }
}

fn decode_capability_cache(bytes: &[u8]) -> Option<S3CapabilityCacheFile> {
    let mut decoder = rmp_serde::Deserializer::new(Cursor::new(bytes));
    let cache = S3CapabilityCacheFile::deserialize(&mut decoder).ok()?;
    if decoder.position() != bytes.len() as u64 {
        return None;
    }
    Some(cache)
}

fn cache_lock(path: &Path) -> StorageResult<Arc<Mutex<()>>> {
    let locks = S3_CAPABILITY_CACHE_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks.lock().map_err(|_| StorageError::Unavailable)?;
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_owned(), Arc::downgrade(&lock));
    Ok(lock)
}

fn with_capability_cache_lock<T>(
    path: &Path,
    operation: impl FnOnce() -> StorageResult<T>,
) -> StorageResult<T> {
    let process_lock = cache_lock(path)?;
    let _process_guard = process_lock.lock().map_err(|_| StorageError::Unavailable)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| StorageError::from_io_error(&error))?;
    let file_name = path
        .file_name()
        .ok_or(StorageError::InvalidRequest)?
        .to_string_lossy();
    let lock_path = parent.join(format!(".{file_name}.lock"));
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|error| StorageError::from_io_error(&error))?;
    lock_file
        .lock()
        .map_err(|error| StorageError::from_io_error(&error))?;
    operation()
}

fn persist_cached_capabilities(
    path: &Path,
    key: &S3CapabilityCacheKey,
    capabilities: S3ConditionalWriteCapabilities,
) -> StorageResult<()> {
    with_capability_cache_lock(path, || {
        let mut cache = read_capability_cache(path);
        cache.entries.retain(|entry| !entry.matches(key));
        if cache.entries.len() >= MAX_S3_CAPABILITY_CACHE_ENTRIES {
            cache
                .entries
                .sort_unstable_by_key(|entry| entry.observed_at_unix_seconds);
            let remove_count = cache.entries.len() - MAX_S3_CAPABILITY_CACHE_ENTRIES + 1;
            cache.entries.drain(..remove_count);
        }
        cache
            .entries
            .push(S3CapabilityCacheEntry::for_capabilities(key, capabilities));
        write_capability_cache(path, &cache)
    })
}

fn invalidate_cached_capabilities(path: &Path, key: &S3CapabilityCacheKey) -> StorageResult<()> {
    with_capability_cache_lock(path, || {
        match fs::metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(StorageError::from_io_error(&error)),
        }
        let mut cache = read_capability_cache(path);
        cache.entries.retain(|entry| !entry.matches(key));
        write_capability_cache(path, &cache)
    })
}

fn write_capability_cache(path: &Path, cache: &S3CapabilityCacheFile) -> StorageResult<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| StorageError::from_io_error(&error))?;
    let contents = rmp_serde::to_vec_named(cache).map_err(|_| StorageError::Io)?;
    let (temporary_path, mut file) = create_cache_temp_file(path)?;
    if let Err(error) = file
        .write_all(&contents)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = fs::remove_file(&temporary_path);
        return Err(StorageError::from_io_error(&error));
    }
    drop(file);
    if let Err(error) = replace_cache_file(&temporary_path, path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(StorageError::from_io_error(&error));
    }
    sync_cache_parent(parent)
}

fn create_cache_temp_file(path: &Path) -> StorageResult<(PathBuf, File)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or(StorageError::InvalidRequest)?
        .to_string_lossy();
    for _ in 0..8 {
        let id = NEXT_S3_CAPABILITY_CACHE_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(".{file_name}.tmp-{}-{id}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.mode(0o600);
        }
        match options.open(&temporary_path) {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(StorageError::from_io_error(&error)),
        }
    }
    Err(StorageError::Unavailable)
}

#[cfg(not(windows))]
fn replace_cache_file(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_cache_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let from = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both vectors are NUL-terminated UTF-16 paths that remain alive
    // for this synchronous call. MoveFileExW does not retain either pointer.
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;

#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

fn sync_cache_parent(parent: &Path) -> StorageResult<()> {
    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| StorageError::from_io_error(&error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
    }
    Ok(())
}

fn default_s3_capability_cache_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))?;
        Some(
            PathBuf::from(base)
                .join("gib")
                .join(DEFAULT_S3_CAPABILITY_CACHE_FILE_NAME),
        )
    }
    #[cfg(target_os = "macos")]
    {
        let base = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
        Some(
            PathBuf::from(base)
                .join("Library")
                .join("Caches")
                .join("gib")
                .join(DEFAULT_S3_CAPABILITY_CACHE_FILE_NAME),
        )
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = match std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
            Some(base) => PathBuf::from(base),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())?
                    .as_os_str(),
            )
            .join(".cache"),
        };
        Some(base.join("gib").join(DEFAULT_S3_CAPABILITY_CACHE_FILE_NAME))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// A streamed S3 or S3-compatible object-storage adapter.
///
/// The public API uses only Gib storage types. AWS SDK configuration, clients,
/// request builders, and provider errors remain private implementation details.
/// Synchronous storage-port calls are dispatched to a dedicated Tokio runtime
/// thread, and multipart uploads hold at most `max_concurrency` parts in
/// memory at once.
#[derive(Clone)]
pub struct S3Storage {
    config: Arc<S3StorageConfig>,
    runtime: Arc<S3Runtime>,
    capability_state: Arc<S3CapabilityState>,
}

impl S3Storage {
    /// Constructs an S3 adapter from a fully validated configuration.
    pub fn new(config: S3StorageConfig) -> StorageResult<Self> {
        config.validate()?;
        let capability_state = Arc::new(S3CapabilityState::new(&config));
        let credentials = Credentials::new(
            config.access_key.clone(),
            config.secret_key.clone(),
            config.session_token.clone(),
            None,
            "gib-sdk",
        );
        let mut builder = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(Region::new(config.region.clone()))
            .credentials_provider(credentials)
            .force_path_style(config.force_path_style);
        if let Some(endpoint) = &config.endpoint {
            builder = builder.endpoint_url(endpoint.clone());
        }
        let client = Client::from_conf(builder.build());
        let runtime = Arc::new(S3Runtime::new(client, config.max_concurrency)?);
        Ok(Self {
            config: Arc::new(config),
            runtime,
            capability_state,
        })
    }

    /// Returns a redacted view of the adapter configuration.
    pub fn config(&self) -> &S3StorageConfig {
        &self.config
    }

    /// Returns the cached conditional-write status without contacting the
    /// provider.
    pub fn conditional_write_capabilities(&self) -> S3ConditionalWriteCapabilities {
        self.capability_state.snapshot()
    }

    /// Probes and caches create-if-absent and replace-if-version independently.
    ///
    /// A provider or network failure leaves the affected status inconclusive
    /// and is returned as a typed storage error. Definitive results from the
    /// other probe are retained and persisted when possible.
    pub fn probe_conditional_write_capabilities(
        &self,
    ) -> StorageResult<S3ConditionalWriteCapabilities> {
        let _probe_guard = self
            .capability_state
            .probe_lock
            .lock()
            .map_err(|_| StorageError::Unavailable)?;
        let mut capabilities = self.capability_state.snapshot();
        if capabilities.is_complete() {
            return Ok(capabilities);
        }

        let mut first_error = None;
        if matches!(
            capabilities.create_if_absent(),
            S3ConditionalWriteStatus::Inconclusive
        ) {
            match self.probe_create_if_absent() {
                ProbeOutcome::Supported => {
                    capabilities =
                        capabilities.with_create_if_absent(S3ConditionalWriteStatus::Supported);
                }
                ProbeOutcome::Unsupported => {
                    capabilities =
                        capabilities.with_create_if_absent(S3ConditionalWriteStatus::Unsupported);
                }
                ProbeOutcome::Inconclusive(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if matches!(
            capabilities.replace_if_version(),
            S3ConditionalWriteStatus::Inconclusive
        ) {
            match self.probe_replace_if_version() {
                ProbeOutcome::Supported => {
                    capabilities =
                        capabilities.with_replace_if_version(S3ConditionalWriteStatus::Supported);
                }
                ProbeOutcome::Unsupported => {
                    capabilities =
                        capabilities.with_replace_if_version(S3ConditionalWriteStatus::Unsupported);
                }
                ProbeOutcome::Inconclusive(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        self.capability_state.set(capabilities)?;
        self.capability_state.cache_result(capabilities);
        match first_error {
            Some(error) => Err(error),
            None => Ok(capabilities),
        }
    }

    /// Clears the cached result for this endpoint, region, and bucket.
    ///
    /// The next conditional operation or explicit probe will test the
    /// provider again. Cache invalidation is local-file-only and never sends a
    /// request to the bucket.
    pub fn invalidate_conditional_write_cache(&self) -> StorageResult<()> {
        let _probe_guard = self
            .capability_state
            .probe_lock
            .lock()
            .map_err(|_| StorageError::Unavailable)?;
        self.capability_state
            .set(S3ConditionalWriteCapabilities::default())?;
        if let Some(path) = self.capability_state.cache_path.as_deref() {
            invalidate_cached_capabilities(path, &self.capability_state.key)?;
        }
        Ok(())
    }

    /// Invalidates the cached result and immediately probes the provider again.
    pub fn reprobe_conditional_write_capabilities(
        &self,
    ) -> StorageResult<S3ConditionalWriteCapabilities> {
        self.invalidate_conditional_write_cache()?;
        self.probe_conditional_write_capabilities()
    }

    fn ensure_conditional_write_capability(
        &self,
        condition: &WriteConditionHeader,
    ) -> StorageResult<()> {
        let status = match condition {
            WriteConditionHeader::Any => return Ok(()),
            WriteConditionHeader::IfAbsent => self.capability_state.snapshot().create_if_absent(),
            WriteConditionHeader::IfVersion(_) => {
                self.capability_state.snapshot().replace_if_version()
            }
        };
        match status {
            S3ConditionalWriteStatus::Supported => Ok(()),
            S3ConditionalWriteStatus::Unsupported => Err(StorageError::UnsupportedCapability),
            S3ConditionalWriteStatus::Inconclusive => {
                let probe_result = self.probe_conditional_write_capabilities();
                let capabilities = self.capability_state.snapshot();
                let status = match condition {
                    WriteConditionHeader::IfAbsent => capabilities.create_if_absent(),
                    WriteConditionHeader::IfVersion(_) => capabilities.replace_if_version(),
                    WriteConditionHeader::Any => S3ConditionalWriteStatus::Supported,
                };
                match status {
                    S3ConditionalWriteStatus::Supported => Ok(()),
                    S3ConditionalWriteStatus::Unsupported => {
                        Err(StorageError::UnsupportedCapability)
                    }
                    S3ConditionalWriteStatus::Inconclusive => match probe_result {
                        Err(error) => Err(error),
                        Ok(_) => Err(StorageError::Unavailable),
                    },
                }
            }
        }
    }

    fn probe_create_if_absent(&self) -> ProbeOutcome {
        let key = match capability_probe_key("create") {
            Ok(key) => key,
            Err(error) => return ProbeOutcome::Inconclusive(error),
        };
        let outcome = match self.put_small(
            &key,
            b"gib-create-probe-one".to_vec(),
            WriteConditionHeader::IfAbsent,
        ) {
            Ok(_) => match self.put_small(
                &key,
                b"gib-create-probe-two".to_vec(),
                WriteConditionHeader::IfAbsent,
            ) {
                Ok(_) => ProbeOutcome::Unsupported,
                Err(error) if error.is_conflict() => ProbeOutcome::Supported,
                Err(error) => classify_probe_error(error),
            },
            Err(StorageError::AlreadyExists) => {
                ProbeOutcome::Inconclusive(StorageError::AlreadyExists)
            }
            Err(error) => classify_probe_error(error),
        };
        self.finish_probe(&key, outcome)
    }

    fn probe_replace_if_version(&self) -> ProbeOutcome {
        let key = match capability_probe_key("replace") {
            Ok(key) => key,
            Err(error) => return ProbeOutcome::Inconclusive(error),
        };
        let outcome = match self.put_small(
            &key,
            b"gib-replace-probe-one".to_vec(),
            WriteConditionHeader::Any,
        ) {
            Ok(metadata) => {
                let Some(version) = metadata.version().cloned() else {
                    return self.finish_probe(
                        &key,
                        ProbeOutcome::Inconclusive(StorageError::InvalidVersion),
                    );
                };
                let version = match version_as_etag(&version) {
                    Ok(version) => version,
                    Err(error) => {
                        return self.finish_probe(&key, ProbeOutcome::Inconclusive(error));
                    }
                };
                match self.put_small(
                    &key,
                    b"gib-replace-probe-two".to_vec(),
                    WriteConditionHeader::IfVersion(version.clone()),
                ) {
                    Ok(_) => match self.put_small(
                        &key,
                        b"gib-replace-probe-three".to_vec(),
                        WriteConditionHeader::IfVersion(version),
                    ) {
                        Ok(_) => ProbeOutcome::Unsupported,
                        Err(error) if error.is_conflict() => ProbeOutcome::Supported,
                        Err(error) => classify_probe_error(error),
                    },
                    Err(error) => classify_probe_error(error),
                }
            }
            Err(error) => classify_probe_error(error),
        };
        self.finish_probe(&key, outcome)
    }

    fn finish_probe(&self, key: &ObjectKey, outcome: ProbeOutcome) -> ProbeOutcome {
        match self.delete_unconditionally(key) {
            Ok(()) | Err(StorageError::NotFound) => outcome,
            Err(error) => ProbeOutcome::Inconclusive(error),
        }
    }

    /// Writes an object while observing a cooperative cancellation request.
    ///
    /// Cancellation is checked while reading the caller's source and between
    /// bounded multipart batches. Once a single-object PUT or multipart
    /// completion begins, the operation is allowed to finish because those
    /// provider operations are atomic publications and cannot be rolled back
    /// safely after an ambiguous network result. A cancelled multipart upload
    /// is aborted before this method returns.
    pub fn write_stream_with_cancellation(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
        cancellation: Option<&CancellationToken>,
    ) -> StorageResult<ObjectMetadata> {
        self.write_stream_inner(object_key, source, options, cancellation)
    }

    fn write_stream_inner(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
        cancellation: Option<&CancellationToken>,
    ) -> StorageResult<ObjectMetadata> {
        check_cancelled(cancellation)?;
        let condition = WriteConditionHeader::from_storage_condition(options.condition())?;
        self.ensure_conditional_write_capability(&condition)?;
        self.write_stream_unchecked(
            object_key,
            source,
            options.expected_size(),
            condition,
            cancellation,
        )
    }

    fn write_stream_unchecked(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        expected_size: Option<u64>,
        condition: WriteConditionHeader,
        cancellation: Option<&CancellationToken>,
    ) -> StorageResult<ObjectMetadata> {
        check_cancelled(cancellation)?;
        let threshold = self.config.multipart_threshold;

        if let Some(expected_size) = expected_size {
            if expected_size <= threshold {
                let contents = read_small_source(source, expected_size, cancellation)?;
                check_cancelled(cancellation)?;
                return self.put_small(object_key, contents, condition);
            }
            validate_multipart_size(expected_size, self.config.multipart_part_size)?;
            return self.put_multipart(
                object_key,
                source,
                Vec::new(),
                Some(expected_size),
                condition,
                cancellation,
            );
        }

        let probe_limit = threshold
            .checked_add(1)
            .ok_or(StorageError::InvalidRequest)?;
        let probe = read_up_to(source, probe_limit, cancellation)?;
        check_cancelled(cancellation)?;
        if (probe.len() as u64) <= threshold {
            return self.put_small(object_key, probe, condition);
        }
        self.put_multipart(object_key, source, probe, None, condition, cancellation)
    }

    fn put_small(
        &self,
        object_key: &ObjectKey,
        contents: Vec<u8>,
        condition: WriteConditionHeader,
    ) -> StorageResult<ObjectMetadata> {
        let size = contents.len() as u64;
        let bucket = self.config.bucket.clone();
        let key = object_key.as_str().to_owned();
        let request_condition = condition.clone();
        let version = self
            .runtime
            .run_without_cancellation(move |client| async move {
                let content_length =
                    i64::try_from(contents.len()).map_err(|_| StorageError::InvalidRequest)?;
                let mut request = client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .content_length(content_length)
                    .body(ByteStream::from(contents));
                request = match request_condition {
                    WriteConditionHeader::Any => request,
                    WriteConditionHeader::IfAbsent => request.if_none_match("*"),
                    WriteConditionHeader::IfVersion(version) => request.if_match(version),
                };
                let output = request.send().await.map_err(map_sdk_error)?;
                output
                    .e_tag()
                    .map(storage_version_from_etag)
                    .transpose()?
                    .ok_or(StorageError::InvalidVersion)
            })
            .map_err(|error| map_condition_error(error, &condition))?;
        Ok(ObjectMetadata::new(object_key.clone(), size, Some(version)))
    }

    fn put_multipart(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        prefix: Vec<u8>,
        expected_size: Option<u64>,
        condition: WriteConditionHeader,
        cancellation: Option<&CancellationToken>,
    ) -> StorageResult<ObjectMetadata> {
        if let Some(expected_size) = expected_size {
            validate_multipart_size(expected_size, self.config.multipart_part_size)?;
        }

        let upload_id = self.create_multipart_upload(object_key)?;
        let parameters = MultipartUploadParameters {
            object_key,
            expected_size,
            condition,
            upload_id: &upload_id,
            cancellation,
        };
        let result = self.put_multipart_parts(source, prefix, parameters);
        match result {
            Ok(metadata) => Ok(metadata),
            Err(error) => {
                self.abort_multipart_upload(object_key, upload_id);
                Err(error)
            }
        }
    }

    fn create_multipart_upload(&self, object_key: &ObjectKey) -> StorageResult<String> {
        let bucket = self.config.bucket.clone();
        let key = object_key.as_str().to_owned();
        self.runtime
            .run_without_cancellation(move |client| async move {
                let output = client
                    .create_multipart_upload()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(map_sdk_error)?;
                output
                    .upload_id()
                    .map(str::to_owned)
                    .ok_or(StorageError::InvalidRequest)
            })
    }

    fn put_multipart_parts(
        &self,
        source: &mut dyn Read,
        prefix: Vec<u8>,
        parameters: MultipartUploadParameters<'_>,
    ) -> StorageResult<ObjectMetadata> {
        let object_key = parameters.object_key;
        let expected_size = parameters.expected_size;
        let condition = parameters.condition;
        let upload_id = parameters.upload_id;
        let cancellation = parameters.cancellation;
        let part_size = usize::try_from(self.config.multipart_part_size)
            .map_err(|_| StorageError::InvalidRequest)?;
        let mut part_source = MultipartPartSource::new(source, prefix);
        let mut remaining = expected_size;
        let mut next_part_number: u32 = 1;
        let mut total_size = 0_u64;
        let mut completed_parts = Vec::new();

        loop {
            check_cancelled(cancellation)?;
            if next_part_number > MAX_S3_MULTIPART_UPLOAD_PARTS {
                return Err(StorageError::InvalidRequest);
            }
            let mut batch = Vec::with_capacity(self.config.max_concurrency);
            while batch.len() < self.config.max_concurrency
                && next_part_number <= MAX_S3_MULTIPART_UPLOAD_PARTS
            {
                let Some(part) = part_source.read_part(part_size, &mut remaining, cancellation)?
                else {
                    break;
                };
                if part.is_empty() {
                    return Err(StorageError::InvalidRequest);
                }
                total_size = total_size
                    .checked_add(part.len() as u64)
                    .ok_or(StorageError::InvalidRequest)?;
                let part_number =
                    i32::try_from(next_part_number).map_err(|_| StorageError::InvalidRequest)?;
                batch.push((part_number, part));
                next_part_number = next_part_number
                    .checked_add(1)
                    .ok_or(StorageError::InvalidRequest)?;
                if remaining == Some(0) {
                    break;
                }
            }

            if batch.is_empty() {
                if remaining.is_some_and(|value| value != 0) {
                    return Err(StorageError::InvalidRequest);
                }
                break;
            }

            let uploaded = self.upload_part_batch(upload_id, object_key, batch, cancellation)?;
            completed_parts.extend(uploaded);
            check_cancelled(cancellation)?;
            if remaining == Some(0) {
                part_source.ensure_no_extra(cancellation)?;
                break;
            }
        }

        if completed_parts.is_empty() {
            return Err(StorageError::InvalidRequest);
        }
        completed_parts.sort_by_key(|part| part.part_number());
        let version =
            self.complete_multipart_upload(object_key, upload_id, completed_parts, condition)?;
        Ok(ObjectMetadata::new(
            object_key.clone(),
            total_size,
            Some(version),
        ))
    }

    fn upload_part_batch(
        &self,
        upload_id: &str,
        object_key: &ObjectKey,
        parts: Vec<(i32, Vec<u8>)>,
        cancellation: Option<&CancellationToken>,
    ) -> StorageResult<Vec<CompletedPart>> {
        let bucket = self.config.bucket.clone();
        let key = object_key.as_str().to_owned();
        let upload_id = upload_id.to_owned();
        let max_concurrency = self.config.max_concurrency;
        self.runtime
            .run_with_cancellation(cancellation, move |client, semaphore| async move {
                upload_part_batch_async(
                    client,
                    semaphore,
                    bucket,
                    key,
                    upload_id,
                    parts,
                    max_concurrency,
                )
                .await
            })
    }

    fn complete_multipart_upload(
        &self,
        object_key: &ObjectKey,
        upload_id: &str,
        mut completed_parts: Vec<CompletedPart>,
        condition: WriteConditionHeader,
    ) -> StorageResult<StorageVersion> {
        let bucket = self.config.bucket.clone();
        let key = object_key.as_str().to_owned();
        let upload_id = upload_id.to_owned();
        let request_condition = condition.clone();
        self.runtime
            .run_without_cancellation(move |client| async move {
                completed_parts.sort_by_key(|part| part.part_number());
                let multipart_upload = CompletedMultipartUpload::builder()
                    .set_parts(Some(completed_parts))
                    .build();
                let mut request = client
                    .complete_multipart_upload()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .multipart_upload(multipart_upload);
                request = match request_condition {
                    WriteConditionHeader::Any => request,
                    WriteConditionHeader::IfAbsent => request.if_none_match("*"),
                    WriteConditionHeader::IfVersion(version) => request.if_match(version),
                };
                let output = request.send().await.map_err(map_sdk_error)?;
                output
                    .e_tag()
                    .map(storage_version_from_etag)
                    .transpose()?
                    .ok_or(StorageError::InvalidVersion)
            })
            .map_err(|error| map_condition_error(error, &condition))
    }

    fn abort_multipart_upload(&self, object_key: &ObjectKey, upload_id: String) {
        let bucket = self.config.bucket.clone();
        let key = object_key.as_str().to_owned();
        let _ = self
            .runtime
            .run_without_cancellation(move |client| async move {
                client
                    .abort_multipart_upload()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .send()
                    .await
                    .map_err(map_sdk_error)
                    .map(|_| ())
            });
    }

    fn metadata_for_key(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        let bucket = self.config.bucket.clone();
        let key = object_key.as_str().to_owned();
        let object_key = object_key.clone();
        self.runtime
            .run_without_cancellation(move |client| async move {
                let output = client
                    .head_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(map_sdk_error)?;
                metadata_from_head(object_key, output)
            })
    }

    fn delete_unconditionally(&self, object_key: &ObjectKey) -> StorageResult<()> {
        let bucket = self.config.bucket.clone();
        let key = object_key.as_str().to_owned();
        self.runtime
            .run_without_cancellation(move |client| async move {
                client
                    .delete_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(map_sdk_error)
                    .map(|_| ())
            })
    }

    fn open_stream(
        &self,
        object_key: &ObjectKey,
        range: Option<ObjectRange>,
        metadata: ObjectMetadata,
    ) -> StorageResult<ObjectRead> {
        if metadata.size() == 0 || range.is_some_and(ObjectRange::is_empty) {
            return Ok(ObjectRead::new(metadata, io::Cursor::new(Vec::new())));
        }
        let version = metadata
            .version()
            .ok_or(StorageError::InvalidVersion)
            .and_then(version_as_etag)?;
        let range_header =
            range.map(|range| format!("bytes={}-{}", range.start(), range.end().saturating_sub(1)));
        let (receiver, abort_handle) = self.runtime.spawn_get_stream(
            self.config.bucket.clone(),
            object_key.as_str().to_owned(),
            range_header,
            version,
        )?;
        let remaining = range.map_or(metadata.size(), ObjectRange::length);
        let reader = S3ObjectReader {
            receiver: Some(receiver),
            abort_handle: Some(abort_handle),
            current: Vec::new(),
            current_offset: 0,
            remaining,
            done: false,
            _runtime: self.runtime.clone(),
        };
        Ok(ObjectRead::new(metadata, reader))
    }
}

impl fmt::Debug for S3Storage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3Storage")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl RepositoryStorage for S3Storage {
    fn capabilities(&self) -> StorageCapabilities {
        let capabilities = self.capability_state.snapshot();
        // Inconclusive results stay advertised for compatibility; the guarded
        // conditional-write path probes before it can publish anything.
        if capabilities.create_if_absent() == S3ConditionalWriteStatus::Unsupported
            || capabilities.replace_if_version() == S3ConditionalWriteStatus::Unsupported
        {
            StorageCapabilities::ALL & !StorageCapabilities::CONDITIONAL_WRITE
        } else {
            StorageCapabilities::ALL
        }
    }

    fn read_stream(&self, object_key: &ObjectKey) -> StorageResult<ObjectRead> {
        let metadata = self.metadata_for_key(object_key)?;
        self.open_stream(object_key, None, metadata)
    }

    fn read_range(&self, object_key: &ObjectKey, range: ObjectRange) -> StorageResult<ObjectRead> {
        let metadata = self.metadata_for_key(object_key)?;
        if range.end() > metadata.size() {
            return Err(StorageError::InvalidRange);
        }
        self.open_stream(object_key, Some(range), metadata)
    }

    fn metadata(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        self.metadata_for_key(object_key)
    }

    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> StorageResult<ObjectMetadata> {
        self.write_stream_inner(object_key, source, options, None)
    }

    fn delete(&self, object_key: &ObjectKey) -> StorageResult<()> {
        let metadata = self.metadata_for_key(object_key)?;
        let etag = metadata
            .version()
            .ok_or(StorageError::InvalidVersion)
            .and_then(version_as_etag)?;
        let bucket = self.config.bucket.clone();
        let key = object_key.as_str().to_owned();
        self.runtime
            .run_without_cancellation(move |client| async move {
                client
                    .delete_object()
                    .bucket(bucket)
                    .key(key)
                    .if_match(etag)
                    .send()
                    .await
                    .map_err(map_sdk_error)
                    .map(|_| ())
            })
    }

    fn list_page(&self, request: &ObjectListRequest) -> StorageResult<ObjectListPage> {
        request.validate()?;
        let bucket = self.config.bucket.clone();
        let prefix = request.prefix().as_str().to_owned();
        let cursor = request.cursor().map(|cursor| cursor.as_str().to_owned());
        let max_keys = i32::try_from(request.limit()).map_err(|_| StorageError::InvalidRequest)?;
        let output = self
            .runtime
            .run_without_cancellation(move |client| async move {
                let mut request = client
                    .list_objects_v2()
                    .bucket(bucket)
                    .prefix(prefix)
                    .max_keys(max_keys);
                if let Some(cursor) = cursor {
                    request = request.continuation_token(cursor);
                }
                request.send().await.map_err(map_sdk_error)
            })?;

        let prefix = request.prefix().as_str();
        if output.contents().len() > request.limit() {
            return Err(StorageError::InvalidRequest);
        }
        let mut objects = Vec::with_capacity(request.limit());
        let mut last_key: Option<ObjectKey> = None;
        for object in output.contents() {
            let Some(key_value) = object.key() else {
                return Err(StorageError::InvalidRequest);
            };
            if !matches_object_prefix(key_value, prefix) {
                continue;
            }
            let Ok(key) = ObjectKey::new(key_value) else {
                continue;
            };
            if last_key.as_ref().is_some_and(|last| last >= &key) {
                return Err(StorageError::Unavailable);
            }
            let size = object
                .size()
                .ok_or(StorageError::InvalidRequest)
                .and_then(|size| u64::try_from(size).map_err(|_| StorageError::InvalidRequest))?;
            let version = object.e_tag().map(storage_version_from_etag).transpose()?;
            objects.push(ObjectMetadata::new(key.clone(), size, version));
            last_key = Some(key);
            if objects.len() > request.limit() {
                return Err(StorageError::InvalidRequest);
            }
        }

        let next_cursor = if output.is_truncated().ok_or(StorageError::InvalidRequest)? {
            let token = output
                .next_continuation_token()
                .ok_or(StorageError::InvalidCursor)?;
            if request
                .cursor()
                .is_some_and(|cursor| cursor.as_str() == token)
            {
                return Err(StorageError::InvalidCursor);
            }
            Some(ObjectCursor::new(token.to_owned())?)
        } else {
            None
        };
        Ok(ObjectListPage::new(objects, next_cursor))
    }
}

struct S3Runtime {
    tasks: TokioSender<RuntimeTask>,
}

type RuntimeTask = Box<dyn FnOnce(&Client, &Handle, Arc<Semaphore>) + Send + 'static>;

impl S3Runtime {
    fn new(client: Client, max_concurrency: usize) -> StorageResult<Self> {
        let (task_sender, mut task_receiver) =
            tokio::sync::mpsc::channel::<RuntimeTask>(max_concurrency);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("gib-s3-runtime".to_owned())
            .spawn(move || {
                let runtime = RuntimeBuilder::new_current_thread().enable_all().build();
                let Ok(runtime) = runtime else {
                    let _ = ready_sender.send(false);
                    return;
                };
                let semaphore = Arc::new(Semaphore::new(max_concurrency));
                let handle = runtime.handle().clone();
                if ready_sender.send(true).is_err() {
                    return;
                }
                runtime.block_on(async move {
                    while let Some(task) = task_receiver.recv().await {
                        task(&client, &handle, semaphore.clone());
                    }
                });
            })
            .map_err(|_| StorageError::Unavailable)?;
        match ready_receiver.recv() {
            Ok(true) => Ok(Self { tasks: task_sender }),
            Ok(false) | Err(_) => Err(StorageError::Unavailable),
        }
    }

    fn run_without_cancellation<T, F, Fut>(&self, operation: F) -> StorageResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = StorageResult<T>> + Send + 'static,
    {
        self.run_internal(None, true, move |client, _semaphore| operation(client))
    }

    fn run_with_cancellation<T, F, Fut>(
        &self,
        cancellation: Option<&CancellationToken>,
        operation: F,
    ) -> StorageResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Client, Arc<Semaphore>) -> Fut + Send + 'static,
        Fut: Future<Output = StorageResult<T>> + Send + 'static,
    {
        self.run_internal(cancellation.cloned(), false, operation)
    }

    fn run_internal<T, F, Fut>(
        &self,
        cancellation: Option<CancellationToken>,
        acquire_permit: bool,
        operation: F,
    ) -> StorageResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Client, Arc<Semaphore>) -> Fut + Send + 'static,
        Fut: Future<Output = StorageResult<T>> + Send + 'static,
    {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        let task_cancellation = cancellation.clone();
        let task = Box::new(
            move |client: &Client, handle: &Handle, semaphore: Arc<Semaphore>| {
                let client = client.clone();
                handle.spawn(async move {
                    let result = async move {
                        if acquire_permit {
                            let acquire_permit = async {
                                semaphore
                                    .clone()
                                    .acquire_owned()
                                    .await
                                    .map_err(|_| StorageError::Unavailable)
                            };
                            let permit =
                                cancelable(acquire_permit, task_cancellation.clone()).await?;
                            let result =
                                cancelable(operation(client, semaphore), task_cancellation.clone())
                                    .await;
                            drop(permit);
                            result
                        } else {
                            cancelable(operation(client, semaphore), task_cancellation).await
                        }
                    }
                    .await;
                    let _ = reply_sender.send(result);
                });
            },
        ) as RuntimeTask;
        self.send_task(task, cancellation.as_ref())?;
        reply_receiver
            .recv()
            .map_err(|_| StorageError::Unavailable)?
    }

    fn spawn_get_stream(
        &self,
        bucket: String,
        key: String,
        range: Option<String>,
        if_match: String,
    ) -> StorageResult<(Receiver<StreamMessage>, AbortHandle)> {
        let (sender, receiver) = mpsc::sync_channel(STREAM_CHANNEL_CAPACITY);
        let (abort_sender, abort_receiver) = mpsc::sync_channel(1);
        let task = Box::new(
            move |client: &Client, handle: &Handle, semaphore: Arc<Semaphore>| {
                let client = client.clone();
                let task = handle.spawn(async move {
                    let permit = match semaphore.acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => {
                            let _ = send_stream_message(
                                &sender,
                                StreamMessage::Error(StorageError::Unavailable),
                            )
                            .await;
                            return;
                        }
                    };
                    let mut request = client
                        .get_object()
                        .bucket(bucket)
                        .key(key)
                        .if_match(if_match);
                    if let Some(range) = range {
                        request = request.range(range);
                    }
                    let output = match request.send().await {
                        Ok(output) => output,
                        Err(error) => {
                            let _ = send_stream_message(
                                &sender,
                                StreamMessage::Error(map_sdk_error(error)),
                            )
                            .await;
                            drop(permit);
                            return;
                        }
                    };
                    let mut body = output.body;
                    loop {
                        match body.try_next().await {
                            Ok(Some(bytes)) => {
                                let mut disconnected = false;
                                for chunk in bytes
                                    .chunks(crate::application::ports::STORAGE_TRANSFER_BUFFER_SIZE)
                                {
                                    if !send_stream_message(
                                        &sender,
                                        StreamMessage::Data(chunk.to_vec()),
                                    )
                                    .await
                                    {
                                        disconnected = true;
                                        break;
                                    }
                                }
                                if disconnected {
                                    break;
                                }
                            }
                            Ok(None) => {
                                let _ = send_stream_message(&sender, StreamMessage::End).await;
                                break;
                            }
                            Err(_) => {
                                let _ = send_stream_message(
                                    &sender,
                                    StreamMessage::Error(StorageError::Transient),
                                )
                                .await;
                                break;
                            }
                        }
                    }
                    drop(permit);
                });
                let _ = abort_sender.send(task.abort_handle());
            },
        ) as RuntimeTask;
        self.send_task(task, None)?;
        let abort_handle = abort_receiver
            .recv()
            .map_err(|_| StorageError::Unavailable)?;
        Ok((receiver, abort_handle))
    }

    fn send_task(
        &self,
        task: RuntimeTask,
        cancellation: Option<&CancellationToken>,
    ) -> StorageResult<()> {
        let mut task = task;
        loop {
            check_cancelled(cancellation)?;
            match self.tasks.try_send(task) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(_)) => return Err(StorageError::Unavailable),
                Err(TrySendError::Full(returned_task)) => {
                    task = returned_task;
                    thread::sleep(CANCELLATION_POLL_INTERVAL);
                }
            }
        }
    }
}

enum StreamMessage {
    Data(Vec<u8>),
    End,
    Error(StorageError),
}

async fn send_stream_message(sender: &SyncSender<StreamMessage>, message: StreamMessage) -> bool {
    let sender = sender.clone();
    tokio::task::spawn_blocking(move || sender.send(message).is_ok())
        .await
        .unwrap_or(false)
}

struct S3ObjectReader {
    receiver: Option<Receiver<StreamMessage>>,
    abort_handle: Option<AbortHandle>,
    current: Vec<u8>,
    current_offset: usize,
    remaining: u64,
    done: bool,
    _runtime: Arc<S3Runtime>,
}

impl Read for S3ObjectReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            if self.current_offset < self.current.len() {
                let available = self.current.len() - self.current_offset;
                let amount = available.min(buffer.len());
                if (amount as u64) > self.remaining {
                    self.abort();
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        StorageError::InvalidRange.to_string(),
                    ));
                }
                buffer[..amount].copy_from_slice(
                    &self.current[self.current_offset..self.current_offset + amount],
                );
                self.current_offset += amount;
                self.remaining -= amount as u64;
                return Ok(amount);
            }

            if self.done {
                return Ok(0);
            }
            self.current.clear();
            self.current_offset = 0;
            let Some(receiver) = self.receiver.as_ref() else {
                self.done = true;
                return Ok(0);
            };
            match receiver.recv() {
                Ok(StreamMessage::Data(data)) => {
                    if data.is_empty() {
                        continue;
                    }
                    if (data.len() as u64) > self.remaining {
                        self.abort();
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            StorageError::InvalidRange.to_string(),
                        ));
                    }
                    self.current = data;
                }
                Ok(StreamMessage::End) => {
                    self.done = true;
                    self.abort_handle = None;
                    if self.remaining != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "S3 object stream ended before the advertised length",
                        ));
                    }
                    return Ok(0);
                }
                Ok(StreamMessage::Error(error)) => {
                    self.done = true;
                    self.abort_handle = None;
                    return Err(io::Error::other(error.to_string()));
                }
                Err(_) => {
                    self.done = true;
                    self.abort_handle = None;
                    if self.remaining == 0 {
                        return Ok(0);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "S3 object stream was disconnected",
                    ));
                }
            }
        }
    }
}

impl S3ObjectReader {
    fn abort(&mut self) {
        self.receiver.take();
        if let Some(abort_handle) = self.abort_handle.take() {
            abort_handle.abort();
        }
        self.done = true;
    }
}

impl Drop for S3ObjectReader {
    fn drop(&mut self) {
        self.abort();
    }
}

struct MultipartUploadParameters<'a> {
    object_key: &'a ObjectKey,
    expected_size: Option<u64>,
    condition: WriteConditionHeader,
    upload_id: &'a str,
    cancellation: Option<&'a CancellationToken>,
}

struct MultipartPartSource<'a> {
    source: &'a mut dyn Read,
    prefix: Vec<u8>,
    prefix_offset: usize,
}

impl<'a> MultipartPartSource<'a> {
    fn new(source: &'a mut dyn Read, prefix: Vec<u8>) -> Self {
        Self {
            source,
            prefix,
            prefix_offset: 0,
        }
    }

    fn read_part(
        &mut self,
        part_size: usize,
        remaining: &mut Option<u64>,
        cancellation: Option<&CancellationToken>,
    ) -> StorageResult<Option<Vec<u8>>> {
        let target = remaining.map_or(part_size, |remaining| {
            remaining.min(part_size as u64) as usize
        });
        let mut part = Vec::with_capacity(target);
        let mut buffer = [0_u8; crate::application::ports::STORAGE_TRANSFER_BUFFER_SIZE];
        while part.len() < target {
            check_cancelled(cancellation)?;
            if self.prefix_offset < self.prefix.len() {
                let available = self.prefix.len() - self.prefix_offset;
                let amount = available.min(target - part.len());
                part.extend_from_slice(
                    &self.prefix[self.prefix_offset..self.prefix_offset + amount],
                );
                self.prefix_offset += amount;
                continue;
            }
            let amount = (target - part.len()).min(buffer.len());
            let read = self
                .source
                .read(&mut buffer[..amount])
                .map_err(|error| StorageError::from_io_error(&error))?;
            if read > amount {
                return Err(StorageError::InvalidRequest);
            }
            if read == 0 {
                break;
            }
            part.extend_from_slice(&buffer[..read]);
        }
        if let Some(remaining) = remaining {
            if part.len() < target {
                return Err(StorageError::InvalidRequest);
            }
            *remaining = remaining
                .checked_sub(part.len() as u64)
                .ok_or(StorageError::InvalidRequest)?;
        }
        if part.is_empty() {
            Ok(None)
        } else {
            Ok(Some(part))
        }
    }

    fn ensure_no_extra(&mut self, cancellation: Option<&CancellationToken>) -> StorageResult<()> {
        check_cancelled(cancellation)?;
        if self.prefix_offset < self.prefix.len() {
            return Err(StorageError::InvalidRequest);
        }
        let mut byte = [0_u8; 1];
        let read = self
            .source
            .read(&mut byte)
            .map_err(|error| StorageError::from_io_error(&error))?;
        if read != 0 {
            return Err(StorageError::InvalidRequest);
        }
        Ok(())
    }
}

async fn upload_part_batch_async(
    client: Client,
    semaphore: Arc<Semaphore>,
    bucket: String,
    key: String,
    upload_id: String,
    parts: Vec<(i32, Vec<u8>)>,
    max_concurrency: usize,
) -> StorageResult<Vec<CompletedPart>> {
    if parts.is_empty() || parts.len() > max_concurrency {
        return Err(StorageError::InvalidRequest);
    }
    let mut tasks = JoinSet::new();
    for (part_number, contents) in parts {
        let client = client.clone();
        let semaphore = semaphore.clone();
        let bucket = bucket.clone();
        let key = key.clone();
        let upload_id = upload_id.clone();
        tasks.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .map_err(|_| StorageError::Unavailable)?;
            let content_length =
                i64::try_from(contents.len()).map_err(|_| StorageError::InvalidRequest)?;
            let output = client
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .content_length(content_length)
                .body(ByteStream::from(contents))
                .send()
                .await
                .map_err(map_sdk_error)?;
            let etag = output.e_tag().ok_or(StorageError::InvalidVersion)?;
            storage_version_from_etag(etag)?;
            let completed = CompletedPart::builder()
                .part_number(part_number)
                .e_tag(etag)
                .build();
            Ok::<(i32, CompletedPart), StorageError>((part_number, completed))
        });
    }

    let mut completed = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok((part_number, part))) => completed.push((part_number, part)),
            Ok(Err(error)) => {
                tasks.abort_all();
                return Err(error);
            }
            Err(error) => {
                tasks.abort_all();
                return Err(if error.is_cancelled() {
                    StorageError::Cancelled
                } else {
                    StorageError::Unavailable
                });
            }
        }
    }
    completed.sort_by_key(|(part_number, _)| *part_number);
    Ok(completed.into_iter().map(|(_, part)| part).collect())
}

async fn cancelable<T, F>(future: F, cancellation: Option<CancellationToken>) -> StorageResult<T>
where
    F: Future<Output = StorageResult<T>>,
{
    let Some(cancellation) = cancellation else {
        return future.await;
    };
    tokio::pin!(future);
    loop {
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {}
        }
    }
}

fn metadata_from_head(
    object_key: ObjectKey,
    output: aws_sdk_s3::operation::head_object::HeadObjectOutput,
) -> StorageResult<ObjectMetadata> {
    let size = output
        .content_length()
        .ok_or(StorageError::InvalidRequest)
        .and_then(|size| u64::try_from(size).map_err(|_| StorageError::InvalidRequest))?;
    let etag = output.e_tag().ok_or(StorageError::InvalidVersion)?;
    let version = storage_version_from_etag(etag)?;
    Ok(ObjectMetadata::new(object_key, size, Some(version)))
}

fn validate_text(value: &str, max_length: usize, reject_whitespace: bool) -> StorageResult<()> {
    if value.is_empty()
        || value.len() > max_length
        || value.chars().any(|character| {
            character.is_control() || (reject_whitespace && character.is_whitespace())
        })
    {
        return Err(StorageError::InvalidRequest);
    }
    Ok(())
}

fn validate_bucket(bucket: &str) -> StorageResult<()> {
    if bucket.len() < 3
        || bucket.len() > MAX_BUCKET_LENGTH
        || bucket.parse::<IpAddr>().is_ok()
        || !bucket.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-'
        })
        || !bucket
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !bucket
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        || bucket.contains("..")
        || bucket.contains(".-")
        || bucket.contains("-.")
    {
        return Err(StorageError::InvalidRequest);
    }
    Ok(())
}

fn validate_endpoint(endpoint: &str) -> StorageResult<()> {
    if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT_LENGTH {
        return Err(StorageError::InvalidRequest);
    }
    let url = Url::parse(endpoint).map_err(|_| StorageError::InvalidRequest)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(StorageError::InvalidRequest);
    }
    Ok(())
}

fn check_cancelled(cancellation: Option<&CancellationToken>) -> StorageResult<()> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        Err(StorageError::Cancelled)
    } else {
        Ok(())
    }
}

fn read_small_source(
    source: &mut dyn Read,
    expected_size: u64,
    cancellation: Option<&CancellationToken>,
) -> StorageResult<Vec<u8>> {
    let limit = expected_size
        .checked_add(1)
        .ok_or(StorageError::InvalidRequest)?;
    let contents = read_up_to(source, limit, cancellation)?;
    if contents.len() as u64 != expected_size {
        return Err(StorageError::InvalidRequest);
    }
    Ok(contents)
}

fn read_up_to(
    source: &mut dyn Read,
    limit: u64,
    cancellation: Option<&CancellationToken>,
) -> StorageResult<Vec<u8>> {
    let capacity = usize::try_from(limit).map_err(|_| StorageError::InvalidRequest)?;
    let mut contents = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; crate::application::ports::STORAGE_TRANSFER_BUFFER_SIZE];
    while (contents.len() as u64) < limit {
        check_cancelled(cancellation)?;
        let remaining = limit - contents.len() as u64;
        let amount = remaining.min(buffer.len() as u64) as usize;
        let read = source
            .read(&mut buffer[..amount])
            .map_err(|error| StorageError::from_io_error(&error))?;
        if read > amount {
            return Err(StorageError::InvalidRequest);
        }
        if read == 0 {
            break;
        }
        contents.extend_from_slice(&buffer[..read]);
    }
    Ok(contents)
}

fn validate_multipart_size(size: u64, part_size: u64) -> StorageResult<()> {
    let maximum = part_size
        .checked_mul(MAX_S3_MULTIPART_UPLOAD_PARTS as u64)
        .ok_or(StorageError::InvalidRequest)?;
    if size == 0 || size > maximum {
        Err(StorageError::InvalidRequest)
    } else {
        Ok(())
    }
}

#[derive(Clone)]
enum WriteConditionHeader {
    Any,
    IfAbsent,
    IfVersion(String),
}

impl WriteConditionHeader {
    fn from_storage_condition(condition: &StorageWriteCondition) -> StorageResult<Self> {
        match condition {
            StorageWriteCondition::Any => Ok(Self::Any),
            StorageWriteCondition::IfAbsent => Ok(Self::IfAbsent),
            StorageWriteCondition::IfVersion(version) => {
                Ok(Self::IfVersion(version_as_etag(version)?))
            }
        }
    }
}

fn map_condition_error(error: StorageError, condition: &WriteConditionHeader) -> StorageError {
    match condition {
        WriteConditionHeader::IfAbsent
            if matches!(
                error,
                StorageError::AlreadyExists
                    | StorageError::Conflict
                    | StorageError::ConditionNotMet
            ) =>
        {
            StorageError::AlreadyExists
        }
        WriteConditionHeader::IfAbsent | WriteConditionHeader::IfVersion(_)
            if error == StorageError::InvalidRequest =>
        {
            StorageError::UnsupportedCapability
        }
        _ => error,
    }
}

fn version_as_etag(version: &StorageVersion) -> StorageResult<String> {
    let value =
        String::from_utf8(version.as_bytes().to_vec()).map_err(|_| StorageError::InvalidVersion)?;
    if value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(StorageError::InvalidVersion);
    }
    Ok(value)
}

fn storage_version_from_etag(etag: &str) -> StorageResult<StorageVersion> {
    if etag.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(StorageError::InvalidVersion);
    }
    StorageVersion::from_bytes(etag.as_bytes().to_vec())
}

fn matches_object_prefix(key: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || key == prefix
        || key
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn capability_probe_key(purpose: &str) -> StorageResult<ObjectKey> {
    let sequence = NEXT_S3_CAPABILITY_PROBE_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    ObjectKey::new(format!(
        "{S3_CAPABILITY_PROBE_PREFIX}/{}-{timestamp}-{sequence}/{purpose}",
        std::process::id(),
    ))
}

fn classify_probe_error(error: StorageError) -> ProbeOutcome {
    if error == StorageError::UnsupportedCapability {
        ProbeOutcome::Unsupported
    } else {
        ProbeOutcome::Inconclusive(error)
    }
}

fn map_sdk_error<E>(error: SdkError<E>) -> StorageError
where
    E: ProvideErrorMetadata,
{
    let code = error
        .as_service_error()
        .and_then(ProvideErrorMetadata::code);
    if let Some(error) = code.and_then(map_provider_code) {
        return error;
    }
    if let Some(status) = error
        .raw_response()
        .map(|response| response.status().as_u16())
    {
        return StorageError::from_http_status(status);
    }
    match error {
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) | SdkError::ResponseError(_) => {
            StorageError::Transient
        }
        SdkError::ConstructionFailure(_) => StorageError::Io,
        SdkError::ServiceError(_) => StorageError::Io,
        _ => StorageError::Io,
    }
}

fn map_provider_code(code: &str) -> Option<StorageError> {
    Some(match code {
        "NoSuchKey" | "NoSuchBucket" | "NoSuchUpload" | "NotFound" => StorageError::NotFound,
        "PreconditionFailed" | "ConditionalRequestConflict" => StorageError::Conflict,
        "AccessDenied" | "AllAccessDisabled" => StorageError::PermissionDenied,
        "InvalidAccessKeyId"
        | "SignatureDoesNotMatch"
        | "ExpiredToken"
        | "InvalidToken"
        | "TokenRefreshRequired"
        | "AuthorizationHeaderMalformed" => StorageError::Authentication,
        "SlowDown"
        | "Throttling"
        | "ThrottlingException"
        | "RequestLimitExceeded"
        | "TooManyRequests" => StorageError::RateLimited,
        "ServiceUnavailable" => StorageError::Unavailable,
        "InternalError" | "RequestTimeout" | "RequestTimeTooSkewed" => StorageError::Transient,
        "InvalidRange" => StorageError::InvalidRange,
        "NotImplemented" | "MethodNotAllowed" => StorageError::UnsupportedCapability,
        "BadDigest" | "EntityTooLarge" | "InvalidDigest" | "XAmzContentSHA256Mismatch" => {
            StorageError::InvalidRequest
        }
        "EntityTooSmall" | "InvalidPart" | "InvalidPartOrder" | "InvalidRequest"
        | "MalformedXML" | "InvalidBucketName" => StorageError::InvalidRequest,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::StorageCapability;

    struct TestCacheDirectory {
        path: PathBuf,
    }

    impl TestCacheDirectory {
        fn new() -> StorageResult<Self> {
            for _ in 0..8 {
                let id = NEXT_S3_CAPABILITY_CACHE_TEMP_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "gib-s3-capability-cache-test-{}-{id}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self { path }),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(StorageError::from_io_error(&error)),
                }
            }
            Err(StorageError::Unavailable)
        }

        fn file(&self) -> PathBuf {
            self.path.join(DEFAULT_S3_CAPABILITY_CACHE_FILE_NAME)
        }
    }

    impl Drop for TestCacheDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_cache_key(sequence: u16) -> S3CapabilityCacheKey {
        S3CapabilityCacheKey {
            endpoint: Some(format!("http://127.0.0.1:{sequence}")),
            region: "us-east-1".to_owned(),
            bucket: format!("gib-cache-{sequence}"),
        }
    }

    #[test]
    fn capability_cache_round_trips_without_credentials() -> StorageResult<()> {
        let directory = TestCacheDirectory::new()?;
        let path = directory.file();
        let config = S3StorageConfig::new(
            "us-east-1",
            "gib-cache-9001",
            "ACCESS-SECRET",
            "SECRET-VALUE",
        )?
        .with_endpoint("http://127.0.0.1:9000");
        let key = S3CapabilityCacheKey::from_config(&config);
        let capabilities = S3ConditionalWriteCapabilities {
            create_if_absent: S3ConditionalWriteStatus::Supported,
            replace_if_version: S3ConditionalWriteStatus::Unsupported,
        };
        persist_cached_capabilities(&path, &key, capabilities)?;
        assert_eq!(load_cached_capabilities(&path, &key), Some(capabilities));
        assert_eq!(
            load_cached_capabilities(&path, &test_cache_key(9_002)),
            None
        );
        let mut different_region = key.clone();
        different_region.region = "eu-west-1".to_owned();
        assert_eq!(load_cached_capabilities(&path, &different_region), None);
        let mut different_bucket = key.clone();
        different_bucket.bucket = "gib-cache-other".to_owned();
        assert_eq!(load_cached_capabilities(&path, &different_bucket), None);
        let bytes = fs::read(&path).map_err(|error| StorageError::from_io_error(&error))?;
        assert!(
            !bytes
                .windows(b"ACCESS-SECRET".len())
                .any(|window| window == b"ACCESS-SECRET")
        );
        assert!(
            !bytes
                .windows(b"SECRET-VALUE".len())
                .any(|window| window == b"SECRET-VALUE")
        );
        Ok(())
    }

    #[test]
    fn stale_and_corrupt_capability_cache_entries_are_ignored() -> StorageResult<()> {
        let directory = TestCacheDirectory::new()?;
        let path = directory.file();
        let key = test_cache_key(9_003);
        let stale = S3CapabilityCacheFile {
            version: S3_CAPABILITY_CACHE_SCHEMA_VERSION,
            entries: vec![S3CapabilityCacheEntry {
                endpoint: key.endpoint.clone(),
                region: key.region.clone(),
                bucket: key.bucket.clone(),
                observed_at_unix_seconds: u64::MAX,
                create_if_absent: Some(true),
                replace_if_version: Some(true),
            }],
        };
        let stale_bytes = rmp_serde::to_vec_named(&stale).map_err(|_| StorageError::Io)?;
        fs::write(&path, stale_bytes).map_err(|error| StorageError::from_io_error(&error))?;
        assert_eq!(load_cached_capabilities(&path, &key), None);

        let future = S3CapabilityCacheFile {
            version: S3_CAPABILITY_CACHE_SCHEMA_VERSION + 1,
            entries: Vec::new(),
        };
        let future_bytes = rmp_serde::to_vec_named(&future).map_err(|_| StorageError::Io)?;
        fs::write(&path, future_bytes).map_err(|error| StorageError::from_io_error(&error))?;
        assert_eq!(load_cached_capabilities(&path, &key), None);

        fs::write(&path, [0_u8, 1, 2, 3]).map_err(|error| StorageError::from_io_error(&error))?;
        assert_eq!(load_cached_capabilities(&path, &key), None);
        Ok(())
    }

    #[test]
    fn concurrent_capability_cache_writers_keep_all_entries_valid() -> StorageResult<()> {
        let directory = TestCacheDirectory::new()?;
        let path = Arc::new(directory.file());
        let mut workers = Vec::new();
        for sequence in 0..8_u16 {
            let path = path.clone();
            workers.push(thread::spawn(move || {
                let key = test_cache_key(9_100 + sequence);
                let capabilities = S3ConditionalWriteCapabilities {
                    create_if_absent: S3ConditionalWriteStatus::Supported,
                    replace_if_version: if sequence % 2 == 0 {
                        S3ConditionalWriteStatus::Supported
                    } else {
                        S3ConditionalWriteStatus::Unsupported
                    },
                };
                persist_cached_capabilities(&path, &key, capabilities)
            }));
        }
        for worker in workers {
            worker.join().map_err(|_| StorageError::Unavailable)??;
        }
        let cache = read_capability_cache(&path);
        assert_eq!(cache.entries.len(), 8);
        assert_eq!(cache.version, S3_CAPABILITY_CACHE_SCHEMA_VERSION);
        Ok(())
    }

    #[test]
    fn storage_loads_scoped_capabilities_and_invalidates_them() -> StorageResult<()> {
        let directory = TestCacheDirectory::new()?;
        let path = directory.file();
        let config =
            S3StorageConfig::new("us-east-1", "gib-cache-9004", "access-key", "secret-key")?
                .with_endpoint("http://127.0.0.1:9000")
                .with_capability_cache_path(&path);
        let key = S3CapabilityCacheKey::from_config(&config);
        let capabilities = S3ConditionalWriteCapabilities {
            create_if_absent: S3ConditionalWriteStatus::Supported,
            replace_if_version: S3ConditionalWriteStatus::Unsupported,
        };
        persist_cached_capabilities(&path, &key, capabilities)?;

        let storage = S3Storage::new(config)?;
        assert_eq!(storage.conditional_write_capabilities(), capabilities);
        assert!(
            !storage
                .capabilities()
                .supports(StorageCapability::ConditionalWrite)
        );
        storage.invalidate_conditional_write_cache()?;
        assert_eq!(
            storage.conditional_write_capabilities(),
            S3ConditionalWriteCapabilities::default()
        );
        assert_eq!(load_cached_capabilities(&path, &key), None);
        Ok(())
    }

    #[test]
    fn probe_failures_remain_inconclusive() {
        assert_eq!(
            classify_probe_error(StorageError::PermissionDenied),
            ProbeOutcome::Inconclusive(StorageError::PermissionDenied)
        );
        assert_eq!(
            classify_probe_error(StorageError::Transient),
            ProbeOutcome::Inconclusive(StorageError::Transient)
        );
        assert_eq!(
            classify_probe_error(StorageError::UnsupportedCapability),
            ProbeOutcome::Unsupported
        );
    }

    #[test]
    fn invalid_conditional_requests_are_classified_as_unsupported() {
        assert_eq!(
            map_condition_error(
                StorageError::InvalidRequest,
                &WriteConditionHeader::IfAbsent
            ),
            StorageError::UnsupportedCapability
        );
        assert_eq!(
            map_condition_error(
                StorageError::InvalidRequest,
                &WriteConditionHeader::IfVersion("etag".to_owned())
            ),
            StorageError::UnsupportedCapability
        );
    }

    #[test]
    fn configuration_debug_redacts_secrets_and_endpoint() -> StorageResult<()> {
        let config =
            S3StorageConfig::new("us-east-1", "gib-test", "ACCESS-SECRET", "SECRET-VALUE")?
                .with_session_token("SESSION-VALUE")
                .with_endpoint("https://example.test/s3?X-Amz-Signature=do-not-log");
        let debug = format!("{config:?}");
        assert!(!debug.contains("ACCESS-SECRET"));
        assert!(!debug.contains("SECRET-VALUE"));
        assert!(!debug.contains("SESSION-VALUE"));
        assert!(!debug.contains("X-Amz-Signature"));
        assert!(debug.contains("<redacted>"));
        Ok(())
    }

    #[test]
    fn configuration_rejects_invalid_provider_values() -> StorageResult<()> {
        assert!(matches!(
            S3StorageConfig::new("", "gib-test", "access", "secret"),
            Err(StorageError::InvalidRequest)
        ));
        assert!(matches!(
            S3StorageConfig::new("us east 1", "gib-test", "access", "secret"),
            Err(StorageError::InvalidRequest)
        ));
        assert!(matches!(
            S3StorageConfig::new("us-east-1", "Gib-Test", "access", "secret"),
            Err(StorageError::InvalidRequest)
        ));
        let config = S3StorageConfig::new("us-east-1", "gib-test", "access", "secret")?
            .with_endpoint("https://user:password@example.test");
        assert!(matches!(
            S3Storage::new(config),
            Err(StorageError::InvalidRequest)
        ));
        Ok(())
    }

    #[test]
    fn provider_codes_are_mapped_without_exposing_provider_errors() {
        assert_eq!(map_provider_code("NoSuchKey"), Some(StorageError::NotFound));
        assert_eq!(
            map_provider_code("PreconditionFailed"),
            Some(StorageError::Conflict)
        );
        assert_eq!(
            map_provider_code("InvalidAccessKeyId"),
            Some(StorageError::Authentication)
        );
        assert_eq!(
            map_provider_code("SlowDown"),
            Some(StorageError::RateLimited)
        );
        assert_eq!(
            map_provider_code("NotImplemented"),
            Some(StorageError::UnsupportedCapability)
        );
        assert_eq!(map_provider_code("unknown-provider-code"), None);
    }

    #[test]
    fn multipart_part_source_preserves_prefetched_bytes() -> StorageResult<()> {
        let mut source = io::Cursor::new(b"tail".to_vec());
        let mut part_source = MultipartPartSource::new(&mut source, b"prefix".to_vec());
        let mut remaining = None;
        let first = part_source
            .read_part(4, &mut remaining, None)?
            .ok_or(StorageError::Unavailable)?;
        let second = part_source
            .read_part(4, &mut remaining, None)?
            .ok_or(StorageError::Unavailable)?;
        assert_eq!(first, b"pref");
        assert_eq!(second, b"ixta");
        Ok(())
    }

    #[test]
    fn prefix_matching_requires_a_component_boundary() {
        assert!(matches_object_prefix("objects/list", "objects/list"));
        assert!(matches_object_prefix("objects/list/item", "objects/list"));
        assert!(!matches_object_prefix("objects/listing", "objects/list"));
    }

    #[test]
    fn etags_are_rejected_before_they_can_be_used_as_headers() -> StorageResult<()> {
        assert_eq!(
            storage_version_from_etag("etag\nvalue"),
            Err(StorageError::InvalidVersion)
        );
        let malicious = StorageVersion::from_bytes(b"etag\rvalue".to_vec())?;
        assert_eq!(
            version_as_etag(&malicious),
            Err(StorageError::InvalidVersion)
        );
        Ok(())
    }

    #[test]
    fn runtime_drives_bounded_concurrent_tasks() -> StorageResult<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let client = Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .region(Region::new("us-east-1"))
                .build(),
        );
        let runtime = Arc::new(S3Runtime::new(client, 2)?);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let runtime = runtime.clone();
            let active = active.clone();
            let peak = peak.clone();
            workers.push(thread::spawn(move || {
                runtime.run_without_cancellation(move |_client| async move {
                    let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                    peak.fetch_max(current, Ordering::AcqRel);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    active.fetch_sub(1, Ordering::AcqRel);
                    Ok(())
                })
            }));
        }
        for worker in workers {
            worker.join().map_err(|_| StorageError::Unavailable)??;
        }
        assert_eq!(peak.load(Ordering::Acquire), 2);
        Ok(())
    }

    #[test]
    fn runtime_cancellation_stops_a_pending_operation() -> StorageResult<()> {
        let client = Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .region(Region::new("us-east-1"))
                .build(),
        );
        let runtime = Arc::new(S3Runtime::new(client, 1)?);
        let cancellation = CancellationToken::new();
        let writer_runtime = runtime.clone();
        let writer_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            writer_runtime.run_with_cancellation(Some(&writer_cancellation), |_client, _| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(())
            })
        });
        thread::sleep(Duration::from_millis(50));
        cancellation.cancel();
        let result = worker.join().map_err(|_| StorageError::Unavailable)?;
        assert_eq!(result, Err(StorageError::Cancelled));
        Ok(())
    }
}
