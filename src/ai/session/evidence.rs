use super::attempt::timestamp_for_tests;
use super::error::SessionError;
use super::model::{ArtifactId, AttemptId, EvidenceId};
use super::redaction::{hash_bytes, is_safe_identifier, redact_json, redact_text, truncate_text};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const EVIDENCE_DIRECTORY_NAME: &str = "evidence";
const MAX_STATEMENT_BYTES: usize = 4 * 1024;
const MAX_LIMITATION_BYTES: usize = 512;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceKind {
    CatalogEntry,
    CatalogRevision,
    Backup,
    Timestamp,
    ContentHash,
    NormalizedEvent,
    ToolInvocation,
    RestoreVerification,
    UserProvided,
    Limitation,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceSourceKind {
    Catalog,
    Filesystem,
    Backup,
    Restore,
    Tool,
    Conversation,
    User,
    Model,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConfidenceQualifier {
    High,
    Medium,
    Low,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FactOrInference {
    Fact,
    Inference,
}

pub(crate) type EvidenceNature = FactOrInference;
pub(crate) type EvidenceFactKind = FactOrInference;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceStatus {
    Observed,
    Derived,
    Unavailable,
    Degraded,
}

impl EvidenceStatus {
    pub(crate) fn is_limitation(self) -> bool {
        matches!(self, Self::Unavailable | Self::Degraded)
    }

    pub(crate) fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unavailable, _) | (_, Self::Unavailable) => Self::Unavailable,
            (Self::Degraded, _) | (_, Self::Degraded) => Self::Degraded,
            (Self::Derived, _) | (_, Self::Derived) => Self::Derived,
            _ => Self::Observed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceSource {
    pub(crate) kind: EvidenceSourceKind,
    pub(crate) source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) revision_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) backup_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) observed_at: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) identifiers: BTreeMap<String, String>,
}

impl EvidenceSource {
    pub(crate) fn new(kind: EvidenceSourceKind, source_id: impl Into<String>) -> Self {
        Self {
            kind,
            source_id: source_id.into(),
            revision_id: None,
            backup_id: None,
            observed_at: None,
            identifiers: BTreeMap::new(),
        }
    }

    pub(crate) fn with_revision(mut self, revision_id: impl Into<String>) -> Self {
        self.revision_id = Some(revision_id.into());
        self
    }

    pub(crate) fn with_backup(mut self, backup_id: impl Into<String>) -> Self {
        self.backup_id = Some(backup_id.into());
        self
    }

    pub(crate) fn with_identifier(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.identifiers.insert(key.into(), value.into());
        self
    }

    fn validate(&self) -> Result<(), EvidenceError> {
        if self.source_id.trim().is_empty() || self.source_id.len() > 256 {
            return Err(EvidenceError::InvalidRecord);
        }
        for value in self
            .revision_id
            .iter()
            .chain(self.backup_id.iter())
            .chain(self.observed_at.iter())
        {
            if value.len() > 256 || value.contains('\0') {
                return Err(EvidenceError::InvalidRecord);
            }
        }
        for (key, value) in &self.identifiers {
            if key.is_empty()
                || key.len() > 128
                || value.len() > 256
                || key.contains('\0')
                || value.contains('\0')
            {
                return Err(EvidenceError::InvalidRecord);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceRecord {
    pub(crate) schema_version: u32,
    pub(crate) evidence_id: EvidenceId,
    pub(crate) kind: EvidenceKind,
    pub(crate) source: EvidenceSource,
    pub(crate) fact_or_inference: FactOrInference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) statement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) payload: Option<Value>,
    pub(crate) status: EvidenceStatus,
    pub(crate) confidence: ConfidenceQualifier,
    pub(crate) created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) observed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_refs: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) attempt_refs: Vec<AttemptId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) supporting_evidence_ids: Vec<EvidenceId>,
    #[serde(default)]
    pub(crate) truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) truncation_reason: Option<String>,
}

impl EvidenceRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: EvidenceKind,
        source: EvidenceSource,
        fact_or_inference: FactOrInference,
        statement: Option<String>,
        payload: Option<Value>,
        status: EvidenceStatus,
        confidence: ConfidenceQualifier,
        artifact_refs: Vec<ArtifactId>,
        attempt_refs: Vec<AttemptId>,
        supporting_evidence_ids: Vec<EvidenceId>,
    ) -> Result<Self, SessionError> {
        Ok(Self {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            evidence_id: EvidenceId::new()?,
            kind,
            source,
            fact_or_inference,
            statement,
            payload,
            status,
            confidence,
            created_at: timestamp_for_tests(),
            observed_at: None,
            artifact_refs,
            attempt_refs,
            supporting_evidence_ids,
            truncated: false,
            truncation_reason: None,
        })
    }

    pub(crate) fn fact(
        kind: EvidenceKind,
        source: EvidenceSource,
        statement: impl Into<String>,
        status: EvidenceStatus,
        confidence: ConfidenceQualifier,
    ) -> Result<Self, SessionError> {
        Self::new(
            kind,
            source,
            FactOrInference::Fact,
            Some(statement.into()),
            None,
            status,
            confidence,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    pub(crate) fn inference(
        kind: EvidenceKind,
        source: EvidenceSource,
        statement: impl Into<String>,
        confidence: ConfidenceQualifier,
        supporting_evidence_ids: Vec<EvidenceId>,
    ) -> Result<Self, SessionError> {
        Self::new(
            kind,
            source,
            FactOrInference::Inference,
            Some(statement.into()),
            None,
            EvidenceStatus::Derived,
            confidence,
            Vec::new(),
            Vec::new(),
            supporting_evidence_ids,
        )
    }

    fn validate(&self, limits: EvidenceLimits) -> Result<(), EvidenceError> {
        if self.schema_version != EVIDENCE_SCHEMA_VERSION
            || !is_safe_identifier(self.evidence_id.as_str())
            || !self.evidence_id.as_str().starts_with("evidence-")
            || self.created_at.is_empty()
            || self.statement.is_none() && self.payload.is_none()
            || self.truncated && self.truncation_reason.is_none()
            || !self.truncated && self.truncation_reason.is_some()
        {
            return Err(EvidenceError::InvalidRecord);
        }
        self.source.validate()?;
        if self
            .statement
            .as_ref()
            .is_some_and(|statement| statement.len() > limits.max_statement_bytes)
        {
            return Err(EvidenceError::RecordTooLarge {
                limit: limits.max_statement_bytes,
                actual: self.statement.as_ref().map_or(0, String::len),
            });
        }
        if self
            .truncation_reason
            .as_ref()
            .is_some_and(|reason| reason.len() > MAX_LIMITATION_BYTES)
        {
            return Err(EvidenceError::InvalidRecord);
        }
        if self.fact_or_inference == FactOrInference::Inference
            && self.supporting_evidence_ids.is_empty()
        {
            return Err(EvidenceError::InferenceNeedsSupport);
        }
        if self.status.is_limitation() && self.statement.as_deref().unwrap_or("").is_empty() {
            return Err(EvidenceError::MissingLimitation);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EvidenceLimits {
    pub(crate) max_record_bytes: usize,
    pub(crate) max_count: usize,
    pub(crate) max_statement_bytes: usize,
    pub(crate) max_file_bytes: usize,
}

impl Default for EvidenceLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: 64 * 1024,
            max_count: 4_096,
            max_statement_bytes: MAX_STATEMENT_BYTES,
            max_file_bytes: 128 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum EvidenceError {
    InvalidRecord,
    EvidenceNotFound { id: String },
    RecordTooLarge { limit: usize, actual: usize },
    EvidenceCountExceeded { limit: usize },
    InferenceNeedsSupport,
    MissingSupportingEvidence { id: String },
    MissingLimitation,
    MalformedEvidence,
    FutureSchemaVersion { version: u32 },
    UnsafePath,
    IoError { operation: String },
    SerializationError { operation: String },
    LockPoisoned,
}

impl EvidenceError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidRecord => "invalid_evidence",
            Self::EvidenceNotFound { .. } => "evidence_not_found",
            Self::RecordTooLarge { .. } => "evidence_too_large",
            Self::EvidenceCountExceeded { .. } => "evidence_count_exceeded",
            Self::InferenceNeedsSupport => "inference_needs_support",
            Self::MissingSupportingEvidence { .. } => "missing_supporting_evidence",
            Self::MissingLimitation => "missing_limitation",
            Self::MalformedEvidence => "malformed_evidence",
            Self::FutureSchemaVersion { .. } => "future_evidence_schema_version",
            Self::UnsafePath => "unsafe_path",
            Self::IoError { .. } => "io_error",
            Self::SerializationError { .. } => "serialization_error",
            Self::LockPoisoned => "evidence_ledger_unavailable",
        }
    }
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecord => formatter.write_str("the evidence record is invalid"),
            Self::EvidenceNotFound { id } => write!(formatter, "evidence '{id}' was not found"),
            Self::RecordTooLarge { limit, actual } => {
                write!(
                    formatter,
                    "evidence record is {actual} bytes; the limit is {limit} bytes"
                )
            }
            Self::EvidenceCountExceeded { limit } => {
                write!(formatter, "evidence count exceeds the limit of {limit}")
            }
            Self::InferenceNeedsSupport => {
                formatter.write_str("an inference must cite supporting evidence IDs")
            }
            Self::MissingSupportingEvidence { id } => {
                write!(formatter, "supporting evidence '{id}' is not available")
            }
            Self::MissingLimitation => {
                formatter.write_str("unavailable or degraded evidence must state its limitation")
            }
            Self::MalformedEvidence => formatter.write_str("the evidence record is malformed"),
            Self::FutureSchemaVersion { version } => write!(
                formatter,
                "evidence schema version {version} is newer than this binary"
            ),
            Self::UnsafePath => formatter.write_str("refusing to use an unsafe evidence path"),
            Self::IoError { operation } => {
                write!(formatter, "evidence storage failed to {operation}")
            }
            Self::SerializationError { operation } => {
                write!(formatter, "evidence storage failed to {operation}")
            }
            Self::LockPoisoned => formatter.write_str("evidence storage is unavailable"),
        }
    }
}

impl std::error::Error for EvidenceError {}

#[derive(Debug, Default)]
struct EvidenceState {
    records: BTreeMap<EvidenceId, EvidenceRecord>,
    persisted_count: usize,
    loaded_persisted_ids: BTreeSet<EvidenceId>,
    total_bytes: usize,
}

/// An append-only, bounded evidence ledger. A ledger may be memory-only for a
/// short turn or rooted on disk when a session needs process continuation.
#[derive(Debug, Clone)]
pub(crate) struct EvidenceLedger {
    root: Option<PathBuf>,
    limits: EvidenceLimits,
    state: Arc<Mutex<EvidenceState>>,
}

pub(crate) type EvidenceStore = EvidenceLedger;

impl EvidenceLedger {
    pub(crate) fn new(limits: EvidenceLimits) -> Self {
        Self {
            root: None,
            limits,
            state: Arc::new(Mutex::new(EvidenceState::default())),
        }
    }

    pub(crate) fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into().join(EVIDENCE_DIRECTORY_NAME);
        let persisted_count = count_evidence_files(&root);
        Self {
            root: Some(root),
            limits: EvidenceLimits::default(),
            state: Arc::new(Mutex::new(EvidenceState {
                records: BTreeMap::new(),
                persisted_count,
                loaded_persisted_ids: BTreeSet::new(),
                total_bytes: 0,
            })),
        }
    }

    pub(crate) fn with_limits(mut self, limits: EvidenceLimits) -> Self {
        self.limits = limits;
        self
    }

    pub(crate) fn limits(&self) -> EvidenceLimits {
        self.limits
    }

    pub(crate) fn append(
        &self,
        mut record: EvidenceRecord,
    ) -> Result<EvidenceRecord, EvidenceError> {
        normalize_record(&mut record, self.limits)?;
        for supporting_id in &record.supporting_evidence_ids {
            if self.get(supporting_id)?.is_none() {
                return Err(EvidenceError::MissingSupportingEvidence {
                    id: supporting_id.to_string(),
                });
            }
        }
        let should_persist = self.root.is_some();
        let id = record.evidence_id.clone();
        if self.root.is_some()
            && let Some(existing) = self.get(&id)?
        {
            if existing == record {
                return Ok(existing);
            }
            return Err(EvidenceError::InvalidRecord);
        }
        let mut state = self.state.lock().map_err(|_| EvidenceError::LockPoisoned)?;
        let mut supporting_status = EvidenceStatus::Observed;
        for supporting_id in &record.supporting_evidence_ids {
            let Some(supporting) = state.records.get(supporting_id) else {
                return Err(EvidenceError::MissingSupportingEvidence {
                    id: supporting_id.to_string(),
                });
            };
            supporting_status = supporting_status.combine(supporting.status);
        }
        record.status = record.status.combine(supporting_status);
        record.validate(self.limits)?;
        let record_size = serde_json::to_vec(&record)
            .map_err(|_| EvidenceError::SerializationError {
                operation: "encode evidence record".to_string(),
            })?
            .len();
        if record_size > self.limits.max_record_bytes {
            return Err(EvidenceError::RecordTooLarge {
                limit: self.limits.max_record_bytes,
                actual: record_size,
            });
        }

        if let Some(existing) = state.records.get(&id) {
            if existing == &record {
                return Ok(existing.clone());
            }
            return Err(EvidenceError::InvalidRecord);
        }
        let current_count = state.records.len().saturating_add(state.persisted_count);
        if current_count.saturating_sub(state.loaded_persisted_ids.len()) >= self.limits.max_count {
            return Err(EvidenceError::EvidenceCountExceeded {
                limit: self.limits.max_count,
            });
        }
        state.total_bytes = state.total_bytes.saturating_add(record_size);
        state.records.insert(id.clone(), record.clone());
        drop(state);
        if should_persist {
            if let Err(error) = self.persist(&id) {
                if let Ok(mut state) = self.state.lock() {
                    state.records.remove(&id);
                    state.total_bytes = state.total_bytes.saturating_sub(record_size);
                }
                return Err(error);
            }
        }
        Ok(self
            .get(&id)?
            .ok_or_else(|| EvidenceError::EvidenceNotFound { id: id.to_string() })?)
    }

    pub(crate) fn append_missing_source(
        &self,
        kind: EvidenceKind,
        source: EvidenceSource,
        limitation: impl Into<String>,
    ) -> Result<EvidenceRecord, SessionError> {
        let mut record = EvidenceRecord::new(
            kind,
            source,
            FactOrInference::Fact,
            Some(limitation.into()),
            None,
            EvidenceStatus::Unavailable,
            ConfidenceQualifier::Unknown,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        record.kind = EvidenceKind::Limitation;
        Ok(self.append(record)?)
    }

    pub(crate) fn get(&self, id: &EvidenceId) -> Result<Option<EvidenceRecord>, EvidenceError> {
        if let Some(record) = self
            .state
            .lock()
            .map_err(|_| EvidenceError::LockPoisoned)?
            .records
            .get(id)
            .cloned()
        {
            return Ok(Some(record));
        }
        let Some(root) = &self.root else {
            return Ok(None);
        };
        let path = root.join(format!("{}.json", id));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(EvidenceError::IoError {
                    operation: "read evidence".to_string(),
                });
            }
        };
        if bytes.len() > self.limits.max_file_bytes {
            return Err(EvidenceError::RecordTooLarge {
                limit: self.limits.max_file_bytes,
                actual: bytes.len(),
            });
        }
        let record: EvidenceRecord =
            serde_json::from_slice(&bytes).map_err(|_| EvidenceError::MalformedEvidence)?;
        if record.evidence_id != *id {
            return Err(EvidenceError::MalformedEvidence);
        }
        record.validate(self.limits)?;
        let compact_size = serde_json::to_vec(&record)
            .map_err(|_| EvidenceError::MalformedEvidence)?
            .len();
        if compact_size > self.limits.max_record_bytes {
            return Err(EvidenceError::RecordTooLarge {
                limit: self.limits.max_record_bytes,
                actual: compact_size,
            });
        }
        let mut state = self.state.lock().map_err(|_| EvidenceError::LockPoisoned)?;
        state.total_bytes = state.total_bytes.saturating_add(bytes.len());
        state.loaded_persisted_ids.insert(id.clone());
        state.records.insert(id.clone(), record.clone());
        Ok(Some(record))
    }

    pub(crate) fn load(&self, id: impl AsRef<str>) -> Result<EvidenceRecord, EvidenceError> {
        let id = EvidenceId::from_string(id.as_ref().to_string())
            .map_err(|_| EvidenceError::InvalidRecord)?;
        self.get(&id)?
            .ok_or_else(|| EvidenceError::EvidenceNotFound { id: id.to_string() })
    }

    pub(crate) fn all(&self) -> Result<Vec<EvidenceRecord>, EvidenceError> {
        let mut records = self
            .state
            .lock()
            .map_err(|_| EvidenceError::LockPoisoned)?
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
                let Ok(id) = EvidenceId::from_string(id.to_string()) else {
                    continue;
                };
                if !records.iter().any(|record| record.evidence_id == id)
                    && let Some(record) = self.get(&id)?
                {
                    records.push(record);
                }
            }
        }
        records.sort_by(|left, right| left.evidence_id.cmp(&right.evidence_id));
        Ok(records)
    }

    pub(crate) fn status_for(&self, ids: &[EvidenceId]) -> Result<EvidenceStatus, EvidenceError> {
        let mut status = EvidenceStatus::Observed;
        for id in ids {
            let Some(record) = self.get(id)? else {
                return Ok(EvidenceStatus::Unavailable);
            };
            status = status.combine(record.status);
        }
        Ok(status)
    }

    pub(crate) fn has_limitation(&self) -> Result<bool, EvidenceError> {
        Ok(self
            .all()?
            .iter()
            .any(|record| record.status.is_limitation()))
    }

    pub(crate) fn persist(&self, id: &EvidenceId) -> Result<(), EvidenceError> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        ensure_directory(root)?;
        let record = self
            .state
            .lock()
            .map_err(|_| EvidenceError::LockPoisoned)?
            .records
            .get(id)
            .cloned()
            .ok_or_else(|| EvidenceError::EvidenceNotFound { id: id.to_string() })?;
        let encoded =
            serde_json::to_vec_pretty(&record).map_err(|_| EvidenceError::SerializationError {
                operation: "encode evidence".to_string(),
            })?;
        if encoded.len() > self.limits.max_file_bytes {
            return Err(EvidenceError::RecordTooLarge {
                limit: self.limits.max_file_bytes,
                actual: encoded.len(),
            });
        }
        let compact_size = serde_json::to_vec(&record)
            .map_err(|_| EvidenceError::SerializationError {
                operation: "encode evidence record".to_string(),
            })?
            .len();
        if compact_size > self.limits.max_record_bytes {
            return Err(EvidenceError::RecordTooLarge {
                limit: self.limits.max_record_bytes,
                actual: compact_size,
            });
        }
        let path = root.join(format!("{}.json", id));
        ensure_regular_or_missing(&path)?;
        let existed = path.is_file();
        write_atomic(&path, &encoded)?;
        let mut state = self.state.lock().map_err(|_| EvidenceError::LockPoisoned)?;
        if !existed {
            state.persisted_count = state.persisted_count.saturating_add(1);
        }
        state.loaded_persisted_ids.insert(id.clone());
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceLedgerWire {
    schema_version: u32,
    records: Vec<EvidenceRecord>,
}

impl Serialize for EvidenceLedger {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let records = self.all().map_err(serde::ser::Error::custom)?;
        EvidenceLedgerWire {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            records,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EvidenceLedger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = EvidenceLedgerWire::deserialize(deserializer)?;
        if wire.schema_version != EVIDENCE_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported evidence ledger schema version",
            ));
        }
        let ledger = Self::new(EvidenceLimits::default());
        let mut records = wire.records;
        records.sort_by_key(|record| {
            (
                record.fact_or_inference == FactOrInference::Inference,
                record.evidence_id.clone(),
            )
        });
        for record in records {
            ledger.append(record).map_err(serde::de::Error::custom)?;
        }
        Ok(ledger)
    }
}

fn normalize_record(
    record: &mut EvidenceRecord,
    limits: EvidenceLimits,
) -> Result<(), EvidenceError> {
    let original_statement_bytes = record.statement.as_ref().map_or(0, String::len);
    let redacted_statement = record.statement.take().map(|value| redact_text(&value));
    let statement_was_bounded = redacted_statement.as_ref().is_some_and(|value| {
        value.len() > limits.max_statement_bytes
            || value.ends_with("…[truncated]")
            || original_statement_bytes > limits.max_statement_bytes
    });
    record.statement = redacted_statement
        .as_deref()
        .map(|value| truncate_text(value, limits.max_statement_bytes));
    if statement_was_bounded {
        record.truncated = true;
        record.truncation_reason = Some("statement_size_limit".to_string());
    }
    record.payload = record.payload.take().map(|value| redact_json(&value));
    if record.fact_or_inference == FactOrInference::Inference
        && record.supporting_evidence_ids.is_empty()
    {
        return Err(EvidenceError::InferenceNeedsSupport);
    }
    if record.status.is_limitation() && record.statement.as_deref().unwrap_or("").is_empty() {
        return Err(EvidenceError::MissingLimitation);
    }
    record.source.source_id =
        redact_source_identifier(record.source.kind, &record.source.source_id);
    record.source.revision_id = record
        .source
        .revision_id
        .take()
        .map(|value| redact_text(&value));
    record.source.backup_id = record
        .source
        .backup_id
        .take()
        .map(|value| redact_text(&value));
    record.source.observed_at = record
        .source
        .observed_at
        .take()
        .map(|value| redact_text(&value));
    record.source.identifiers = record
        .source
        .identifiers
        .iter()
        .map(|(key, value)| {
            let value = if key.eq_ignore_ascii_case("path")
                || key.eq_ignore_ascii_case("lookup_path")
                || key.eq_ignore_ascii_case("target_path")
            {
                redact_source_identifier(EvidenceSourceKind::Filesystem, value)
            } else {
                redact_text(value)
            };
            (redact_text(key), value)
        })
        .collect();
    record.validate(limits)
}

fn redact_source_identifier(kind: EvidenceSourceKind, value: &str) -> String {
    let looks_like_path = matches!(kind, EvidenceSourceKind::Filesystem)
        || value.starts_with('/')
        || value.starts_with("./")
        || value.contains('/')
        || value.contains('\\');
    if looks_like_path {
        let normalized = value
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_string();
        hash_bytes("path:sha256:", normalized.as_bytes())
    } else {
        redact_text(value)
    }
}

fn count_evidence_files(root: &Path) -> usize {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
        .count()
}

fn ensure_directory(path: &Path) -> Result<(), EvidenceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(EvidenceError::UnsafePath)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|_| EvidenceError::IoError {
                operation: "create evidence directory".to_string(),
            })?;
            set_directory_mode(path)?;
            Ok(())
        }
        Err(_) => Err(EvidenceError::IoError {
            operation: "inspect evidence directory".to_string(),
        }),
    }
}

fn ensure_regular_or_missing(path: &Path) -> Result<(), EvidenceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(EvidenceError::UnsafePath)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(EvidenceError::IoError {
            operation: "inspect evidence file".to_string(),
        }),
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), EvidenceError> {
    let parent = path.parent().ok_or(EvidenceError::UnsafePath)?;
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("evidence.json"),
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| EvidenceError::IoError {
                operation: "create temporary evidence file".to_string(),
            })?;
        set_file_mode(&file)?;
        file.write_all(data).map_err(|_| EvidenceError::IoError {
            operation: "write temporary evidence file".to_string(),
        })?;
        file.sync_all().map_err(|_| EvidenceError::IoError {
            operation: "sync temporary evidence file".to_string(),
        })?;
        fs::rename(&temporary, path).map_err(|_| EvidenceError::IoError {
            operation: "publish evidence file".to_string(),
        })?;
        #[cfg(unix)]
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|_| EvidenceError::IoError {
                operation: "sync evidence directory".to_string(),
            })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn set_file_mode(file: &File) -> Result<(), EvidenceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| EvidenceError::IoError {
                operation: "protect evidence file".to_string(),
            })?;
    }
    Ok(())
}

fn set_directory_mode(path: &Path) -> Result<(), EvidenceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| {
            EvidenceError::IoError {
                operation: "protect evidence directory".to_string(),
            }
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
