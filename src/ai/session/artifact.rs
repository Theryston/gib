use super::attempt::timestamp_for_tests;
use super::error::SessionError;
use super::model::{AgentPhase, ArtifactId, AttemptId};
use super::redaction::{
    canonical_bytes, hash_bytes, is_safe_identifier, redact_json, truncate_text,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) const ARTIFACT_SCHEMA_VERSION: u32 = 1;
const ARTIFACT_DIRECTORY_NAME: &str = "artifacts";
const MAX_TRUNCATION_REASON_BYTES: usize = 128;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactKind {
    CatalogPage,
    CatalogSummary,
    NormalizedTimeline,
    CandidateSet,
    RestorePreview,
    ExplanationContext,
    Context,
    ToolResult,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactStorageStatus {
    InMemory,
    Persisted,
    Missing,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactSensitivity {
    Public,
    Internal,
    Sensitive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetentionClass {
    Turn,
    Session,
    Durable,
}

impl RetentionClass {
    pub(crate) fn requires_persistence(self) -> bool {
        matches!(self, Self::Session | Self::Durable)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactTruncation {
    pub(crate) truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

impl Default for ArtifactTruncation {
    fn default() -> Self {
        Self {
            truncated: false,
            reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactHeader {
    pub(crate) schema_version: u32,
    pub(crate) artifact_id: ArtifactId,
    pub(crate) kind: ArtifactKind,
    pub(crate) producing_phase: AgentPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) producing_attempt_id: Option<AttemptId>,
    pub(crate) content_hash: String,
    pub(crate) byte_size: u64,
    pub(crate) created_at: String,
    pub(crate) sensitivity: ArtifactSensitivity,
    pub(crate) storage_status: ArtifactStorageStatus,
    pub(crate) retention: RetentionClass,
    pub(crate) truncation: ArtifactTruncation,
}

impl ArtifactHeader {
    pub(crate) fn new(
        kind: ArtifactKind,
        producing_phase: AgentPhase,
        producing_attempt_id: Option<AttemptId>,
        sensitivity: ArtifactSensitivity,
        retention: RetentionClass,
    ) -> Result<Self, SessionError> {
        Ok(Self {
            schema_version: ARTIFACT_SCHEMA_VERSION,
            artifact_id: ArtifactId::new()?,
            kind,
            producing_phase,
            producing_attempt_id,
            content_hash: String::new(),
            byte_size: 0,
            created_at: timestamp_for_tests(),
            sensitivity,
            storage_status: ArtifactStorageStatus::InMemory,
            retention,
            truncation: ArtifactTruncation::default(),
        })
    }

    pub(crate) fn validate(&self, limits: ArtifactLimits) -> Result<(), ArtifactError> {
        if self.schema_version != ARTIFACT_SCHEMA_VERSION
            || !is_safe_identifier(self.artifact_id.as_str())
            || !self.artifact_id.as_str().starts_with("artifact-")
            || self.artifact_id.as_str().len() > 128
            || !is_sha256_hash(&self.content_hash)
            || self.byte_size > limits.max_bytes as u64
            || self.created_at.is_empty()
        {
            return Err(ArtifactError::InvalidArtifact);
        }
        if self
            .truncation
            .reason
            .as_ref()
            .is_some_and(|reason| reason.is_empty() || reason.len() > MAX_TRUNCATION_REASON_BYTES)
        {
            return Err(ArtifactError::InvalidArtifact);
        }
        if !self.truncation.truncated && self.truncation.reason.is_some() {
            return Err(ArtifactError::InvalidArtifact);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactRecord {
    pub(crate) header: ArtifactHeader,
    pub(crate) payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArtifactLimits {
    pub(crate) max_bytes: usize,
    pub(crate) max_count: usize,
    pub(crate) max_file_bytes: usize,
}

impl Default for ArtifactLimits {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            max_count: 1_024,
            max_file_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArtifactWriteOptions {
    pub(crate) sensitivity: ArtifactSensitivity,
    pub(crate) retention: RetentionClass,
}

impl Default for ArtifactWriteOptions {
    fn default() -> Self {
        Self {
            sensitivity: ArtifactSensitivity::Internal,
            retention: RetentionClass::Turn,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum ArtifactError {
    InvalidArtifact,
    ArtifactNotFound { id: String },
    ArtifactTooLarge { limit: usize, actual: usize },
    ArtifactCountExceeded { limit: usize },
    MalformedArtifact,
    FutureSchemaVersion { version: u32 },
    UnsafePath,
    IoError { operation: String },
    SerializationError { operation: String },
    LockPoisoned,
}

impl ArtifactError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidArtifact => "invalid_artifact",
            Self::ArtifactNotFound { .. } => "artifact_not_found",
            Self::ArtifactTooLarge { .. } => "artifact_too_large",
            Self::ArtifactCountExceeded { .. } => "artifact_count_exceeded",
            Self::MalformedArtifact => "malformed_artifact",
            Self::FutureSchemaVersion { .. } => "future_artifact_schema_version",
            Self::UnsafePath => "unsafe_path",
            Self::IoError { .. } => "io_error",
            Self::SerializationError { .. } => "serialization_error",
            Self::LockPoisoned => "artifact_store_unavailable",
        }
    }
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArtifact => formatter.write_str("the artifact record is invalid"),
            Self::ArtifactNotFound { id } => write!(formatter, "artifact '{id}' was not found"),
            Self::ArtifactTooLarge { limit, actual } => {
                write!(
                    formatter,
                    "artifact is {actual} bytes; the limit is {limit} bytes"
                )
            }
            Self::ArtifactCountExceeded { limit } => {
                write!(formatter, "artifact count exceeds the limit of {limit}")
            }
            Self::MalformedArtifact => formatter.write_str("the artifact record is malformed"),
            Self::FutureSchemaVersion { version } => write!(
                formatter,
                "artifact schema version {version} is newer than this binary"
            ),
            Self::UnsafePath => formatter.write_str("refusing to use an unsafe artifact path"),
            Self::IoError { operation } => {
                write!(formatter, "artifact storage failed to {operation}")
            }
            Self::SerializationError { operation } => {
                write!(formatter, "artifact storage failed to {operation}")
            }
            Self::LockPoisoned => formatter.write_str("artifact storage is unavailable"),
        }
    }
}

impl std::error::Error for ArtifactError {}

#[derive(Debug, Default)]
struct ArtifactState {
    records: BTreeMap<ArtifactId, ArtifactRecord>,
    persisted_count: usize,
}

/// Bounded in-memory artifact storage with optional durable records.
#[derive(Debug, Clone)]
pub(crate) struct ArtifactStore {
    root: Option<PathBuf>,
    limits: ArtifactLimits,
    state: Arc<Mutex<ArtifactState>>,
}

impl ArtifactStore {
    pub(crate) fn new(limits: ArtifactLimits) -> Self {
        Self {
            root: None,
            limits,
            state: Arc::new(Mutex::new(ArtifactState::default())),
        }
    }

    pub(crate) fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into().join(ARTIFACT_DIRECTORY_NAME);
        let persisted_count = count_artifact_files(&root);
        Self {
            root: Some(root),
            limits: ArtifactLimits::default(),
            state: Arc::new(Mutex::new(ArtifactState {
                records: BTreeMap::new(),
                persisted_count,
            })),
        }
    }

    pub(crate) fn with_limits(mut self, limits: ArtifactLimits) -> Self {
        self.limits = limits;
        self
    }

    pub(crate) fn limits(&self) -> ArtifactLimits {
        self.limits
    }

    pub(crate) fn create<T: Serialize>(
        &self,
        kind: ArtifactKind,
        phase: AgentPhase,
        attempt_id: Option<AttemptId>,
        payload: &T,
        options: ArtifactWriteOptions,
    ) -> Result<ArtifactRecord, ArtifactError> {
        let value =
            serde_json::to_value(payload).map_err(|_| ArtifactError::SerializationError {
                operation: "encode artifact payload".to_string(),
            })?;
        let header = ArtifactHeader::new(
            kind,
            phase,
            attempt_id,
            options.sensitivity,
            options.retention,
        )
        .map_err(|_| ArtifactError::InvalidArtifact)?;
        self.put_json(header, value)
    }

    pub(crate) fn put_json(
        &self,
        mut header: ArtifactHeader,
        payload: Value,
    ) -> Result<ArtifactRecord, ArtifactError> {
        let mut payload = redact_json(&payload);
        let mut truncation = ArtifactTruncation::default();
        let mut encoded = canonical_bytes(&payload);
        if encoded.len() > self.limits.max_bytes {
            let (bounded, bounded_bytes) = bounded_payload(&payload, self.limits.max_bytes)?;
            payload = bounded;
            encoded = bounded_bytes;
            truncation = ArtifactTruncation {
                truncated: true,
                reason: Some("artifact_size_limit".to_string()),
            };
        }
        if encoded.len() > self.limits.max_bytes {
            return Err(ArtifactError::ArtifactTooLarge {
                limit: self.limits.max_bytes,
                actual: encoded.len(),
            });
        }

        header.content_hash = hash_bytes("sha256:", &encoded);
        header.byte_size = encoded.len() as u64;
        header.truncation = truncation;
        header.storage_status = ArtifactStorageStatus::InMemory;
        header.validate(self.limits)?;
        let id = header.artifact_id.clone();
        let record = ArtifactRecord { header, payload };

        let should_persist = record.header.retention.requires_persistence();
        if self.root.is_some()
            && let Some(existing) = self.get(&id)?
        {
            if same_artifact_identity(&existing, &record) {
                return Ok(existing);
            }
            return Err(ArtifactError::InvalidArtifact);
        }
        {
            let mut state = self.state.lock().map_err(|_| ArtifactError::LockPoisoned)?;
            if let Some(existing) = state.records.get(&id) {
                if same_artifact_identity(existing, &record) {
                    return Ok(existing.clone());
                }
                return Err(ArtifactError::InvalidArtifact);
            } else if state.records.len() + state.persisted_count >= self.limits.max_count {
                return Err(ArtifactError::ArtifactCountExceeded {
                    limit: self.limits.max_count,
                });
            }
            state.records.insert(id.clone(), record.clone());
        }

        if should_persist {
            if let Err(error) = self.persist(&id) {
                if let Ok(mut state) = self.state.lock() {
                    state.records.remove(&id);
                }
                return Err(error);
            }
        }
        Ok(self
            .get(&id)?
            .ok_or_else(|| ArtifactError::ArtifactNotFound { id: id.to_string() })?)
    }

    pub(crate) fn get(&self, id: &ArtifactId) -> Result<Option<ArtifactRecord>, ArtifactError> {
        if let Some(record) = self
            .state
            .lock()
            .map_err(|_| ArtifactError::LockPoisoned)?
            .records
            .get(id)
            .cloned()
        {
            return Ok(Some(record));
        }
        let Some(path) = self.path_for(id)? else {
            return Ok(None);
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(ArtifactError::IoError {
                    operation: "read artifact".to_string(),
                });
            }
        };
        if bytes.len() > self.limits.max_file_bytes {
            return Err(ArtifactError::ArtifactTooLarge {
                limit: self.limits.max_file_bytes,
                actual: bytes.len(),
            });
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| ArtifactError::MalformedArtifact)?;
        let version = value
            .get("header")
            .and_then(|header| header.get("schema_version"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        if version > ARTIFACT_SCHEMA_VERSION {
            return Err(ArtifactError::FutureSchemaVersion { version });
        }
        let record: ArtifactRecord =
            serde_json::from_value(value).map_err(|_| ArtifactError::MalformedArtifact)?;
        if record.header.artifact_id != *id {
            return Err(ArtifactError::MalformedArtifact);
        }
        record.header.validate(self.limits)?;
        validate_payload_integrity(&record)?;
        self.state
            .lock()
            .map_err(|_| ArtifactError::LockPoisoned)?
            .records
            .insert(id.clone(), record.clone());
        Ok(Some(record))
    }

    pub(crate) fn load(&self, id: impl AsRef<str>) -> Result<ArtifactRecord, ArtifactError> {
        let id = ArtifactId::from_string(id.as_ref().to_string())
            .map_err(|_| ArtifactError::InvalidArtifact)?;
        self.get(&id)?
            .ok_or_else(|| ArtifactError::ArtifactNotFound { id: id.to_string() })
    }

    pub(crate) fn persist(&self, id: &ArtifactId) -> Result<(), ArtifactError> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        ensure_directory(root)?;
        let mut record = self
            .state
            .lock()
            .map_err(|_| ArtifactError::LockPoisoned)?
            .records
            .get(id)
            .cloned()
            .ok_or_else(|| ArtifactError::ArtifactNotFound { id: id.to_string() })?;
        record.header.storage_status = ArtifactStorageStatus::Persisted;
        let encoded =
            serde_json::to_vec_pretty(&record).map_err(|_| ArtifactError::SerializationError {
                operation: "encode artifact".to_string(),
            })?;
        validate_payload_integrity(&record)?;
        if encoded.len() > self.limits.max_file_bytes {
            return Err(ArtifactError::ArtifactTooLarge {
                limit: self.limits.max_file_bytes,
                actual: encoded.len(),
            });
        }
        let path = root.join(format!("{}.json", id));
        ensure_regular_or_missing(&path)?;
        write_atomic(&path, &encoded)?;
        let mut state = self.state.lock().map_err(|_| ArtifactError::LockPoisoned)?;
        let was_persisted = state
            .records
            .get(id)
            .is_some_and(|record| record.header.storage_status == ArtifactStorageStatus::Persisted);
        state.records.insert(id.clone(), record);
        if !was_persisted {
            state.persisted_count = state.persisted_count.saturating_add(1);
        }
        Ok(())
    }

    pub(crate) fn list(&self) -> Result<Vec<ArtifactRecord>, ArtifactError> {
        let mut records = self
            .state
            .lock()
            .map_err(|_| ArtifactError::LockPoisoned)?
            .records
            .values()
            .cloned()
            .collect::<Vec<_>>();
        if let Some(root) = &self.root
            && let Ok(entries) = fs::read_dir(root)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                let Ok(id) = ArtifactId::from_string(id.to_string()) else {
                    continue;
                };
                if !records.iter().any(|record| record.header.artifact_id == id)
                    && let Some(record) = self.get(&id)?
                {
                    records.push(record);
                }
            }
        }
        records.sort_by(|left, right| left.header.artifact_id.cmp(&right.header.artifact_id));
        Ok(records)
    }

    pub(crate) fn count(&self) -> Result<usize, ArtifactError> {
        Ok(self.list()?.len())
    }

    fn path_for(&self, id: &ArtifactId) -> Result<Option<PathBuf>, ArtifactError> {
        if !id.as_str().starts_with("artifact-") || id.as_str().contains("..") {
            return Err(ArtifactError::UnsafePath);
        }
        Ok(self
            .root
            .as_ref()
            .map(|root| root.join(format!("{}.json", id))))
    }
}

fn bounded_payload(value: &Value, limit: usize) -> Result<(Value, Vec<u8>), ArtifactError> {
    let marker = |preview: String| {
        let mut object = Map::new();
        object.insert("truncated".to_string(), Value::Bool(true));
        object.insert(
            "reason".to_string(),
            Value::String("artifact_size_limit".to_string()),
        );
        if !preview.is_empty() {
            object.insert("preview".to_string(), Value::String(preview));
        }
        Value::Object(object)
    };
    let full_preview = match value {
        Value::String(value) => value.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    };
    let mut low = 0_usize;
    let mut high = full_preview.chars().count();
    let mut best = marker(String::new());
    let mut best_bytes = canonical_bytes(&best);
    if best_bytes.len() > limit {
        return Err(ArtifactError::ArtifactTooLarge {
            limit,
            actual: best_bytes.len(),
        });
    }
    while low <= high {
        let middle = low + (high - low) / 2;
        let preview = full_preview.chars().take(middle).collect::<String>();
        let candidate = marker(truncate_text(&preview, middle));
        let bytes = canonical_bytes(&candidate);
        if bytes.len() <= limit {
            best = candidate;
            best_bytes = bytes;
            low = middle.saturating_add(1);
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    Ok((best, best_bytes))
}

fn validate_payload_integrity(record: &ArtifactRecord) -> Result<(), ArtifactError> {
    let encoded = canonical_bytes(&record.payload);
    if encoded.len() as u64 != record.header.byte_size
        || hash_bytes("sha256:", &encoded) != record.header.content_hash
    {
        return Err(ArtifactError::MalformedArtifact);
    }
    Ok(())
}

fn same_artifact_identity(left: &ArtifactRecord, right: &ArtifactRecord) -> bool {
    left.header.artifact_id == right.header.artifact_id
        && left.header.content_hash == right.header.content_hash
        && left.header.kind == right.header.kind
        && left.header.producing_phase == right.header.producing_phase
        && left.header.producing_attempt_id == right.header.producing_attempt_id
        && left.header.sensitivity == right.header.sensitivity
        && left.header.retention == right.header.retention
        && left.header.truncation == right.header.truncation
}

fn is_sha256_hash(value: &str) -> bool {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return false;
    };
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn count_artifact_files(root: &Path) -> usize {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
        .count()
}

fn ensure_directory(path: &Path) -> Result<(), ArtifactError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(ArtifactError::UnsafePath)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|_| ArtifactError::IoError {
                operation: "create artifact directory".to_string(),
            })?;
            set_directory_mode(path)?;
            Ok(())
        }
        Err(_) => Err(ArtifactError::IoError {
            operation: "inspect artifact directory".to_string(),
        }),
    }
}

fn ensure_regular_or_missing(path: &Path) -> Result<(), ArtifactError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ArtifactError::UnsafePath)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ArtifactError::IoError {
            operation: "inspect artifact file".to_string(),
        }),
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), ArtifactError> {
    let parent = path.parent().ok_or(ArtifactError::UnsafePath)?;
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact.json"),
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| ArtifactError::IoError {
                operation: "create temporary artifact file".to_string(),
            })?;
        set_file_mode(&file)?;
        file.write_all(data).map_err(|_| ArtifactError::IoError {
            operation: "write temporary artifact file".to_string(),
        })?;
        file.sync_all().map_err(|_| ArtifactError::IoError {
            operation: "sync temporary artifact file".to_string(),
        })?;
        fs::rename(&temporary, path).map_err(|_| ArtifactError::IoError {
            operation: "publish artifact file".to_string(),
        })?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sync_directory(path: &Path) -> Result<(), ArtifactError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| ArtifactError::IoError {
                operation: "sync artifact directory".to_string(),
            })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn set_file_mode(file: &File) -> Result<(), ArtifactError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| ArtifactError::IoError {
                operation: "protect artifact file".to_string(),
            })?;
    }
    Ok(())
}

fn set_directory_mode(path: &Path) -> Result<(), ArtifactError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| {
            ArtifactError::IoError {
                operation: "protect artifact directory".to_string(),
            }
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
