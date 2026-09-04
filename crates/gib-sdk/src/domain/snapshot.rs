use super::{DomainError, RepositoryObject, SnapshotReference};
use std::fmt;

/// The version of the authoritative compact snapshot object.
pub const CURRENT_SNAPSHOT_VERSION: u16 = 1;

/// The version of persisted snapshot summary records.
pub const CURRENT_SNAPSHOT_SUMMARY_VERSION: u16 = 1;

/// The version of the persisted history index records.
pub const CURRENT_SNAPSHOT_HISTORY_VERSION: u16 = 1;

/// The logical prefix containing immutable snapshot objects.
pub const SNAPSHOT_OBJECT_PREFIX: &str = "snapshots";

const SNAPSHOT_OBJECT_REFERENCE_PREFIX: &str = "snapshots/";

/// The logical prefix containing derived, reconstructable history records.
pub const SNAPSHOT_HISTORY_OBJECT_PREFIX: &str = "refs/history";

/// The reserved user-facing alias for the repository HEAD snapshot.
pub const LATEST_SNAPSHOT_ALIAS: &str = "latest";

/// The default number of summaries returned by one history page.
pub const DEFAULT_SNAPSHOT_PAGE_SIZE: usize = 100;

/// The largest page accepted by the history API.
pub const MAX_SNAPSHOT_PAGE_SIZE: usize = 1_024;

/// The largest accepted snapshot identifier or prefix in UTF-8 bytes.
pub const MAX_SNAPSHOT_ID_LENGTH: usize = 128;

/// The largest accepted snapshot message in UTF-8 bytes.
pub const MAX_SNAPSHOT_MESSAGE_LENGTH: usize = 512;

/// The largest accepted snapshot author in UTF-8 bytes.
pub const MAX_SNAPSHOT_AUTHOR_LENGTH: usize = 512;

/// The largest accepted opaque history cursor in UTF-8 bytes.
pub const MAX_SNAPSHOT_CURSOR_LENGTH: usize = 256;

/// A validated immutable snapshot identifier.
///
/// Gib normally uses a lowercase content hash here. The domain permits other
/// safe identifiers as well so repositories can preserve historical object
/// names while migrating to content-addressed snapshot IDs. Identity is always
/// the complete value; a prefix is only a query selector.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// Creates an immutable snapshot identifier after validating its syntax.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_SNAPSHOT_ID_LENGTH {
            return Err(DomainError::InvalidSnapshotId {
                reason: "must contain 1 to 128 ASCII identifier bytes",
            });
        }
        let mut bytes = value.bytes();
        let Some(first) = bytes.next() else {
            return Err(DomainError::InvalidSnapshotId {
                reason: "must contain 1 to 128 ASCII identifier bytes",
            });
        };
        if !first.is_ascii_alphanumeric()
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(DomainError::InvalidSnapshotId {
                reason: "must start with an ASCII letter or digit and contain only letters, digits, dot, underscore, or hyphen",
            });
        }
        if value.eq_ignore_ascii_case(LATEST_SNAPSHOT_ALIAS) {
            return Err(DomainError::InvalidSnapshotId {
                reason: "latest is reserved for the HEAD alias",
            });
        }
        Ok(Self(value))
    }

    /// Creates an identifier from a snapshot object reference.
    pub fn from_reference(reference: &SnapshotReference) -> Result<Self, DomainError> {
        let Some(snapshot_path) = reference
            .as_str()
            .strip_prefix(SNAPSHOT_OBJECT_REFERENCE_PREFIX)
        else {
            return Err(DomainError::InvalidSnapshotId {
                reason: "snapshot objects must be below the snapshots prefix",
            });
        };
        let Some(identifier) = snapshot_path.rsplit('/').next() else {
            return Err(DomainError::InvalidSnapshotId {
                reason: "snapshot object reference must contain an identifier",
            });
        };
        Self::new(identifier)
    }

    /// Returns the canonical identifier representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns whether this identifier has the length and syntax of a SHA-256
    /// hexadecimal content hash.
    pub fn is_sha256_hex(&self) -> bool {
        self.0.len() == 64 && self.0.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    /// Returns the conventional unsharded snapshot object reference.
    pub fn object_reference(&self) -> Result<SnapshotReference, DomainError> {
        SnapshotReference::new(format!("{SNAPSHOT_OBJECT_PREFIX}/{}", self.as_str()))
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for SnapshotId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for SnapshotId {
    type Error = DomainError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for SnapshotId {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A validated user-facing snapshot selector.
///
/// The selector contains either a complete ID or a validated prefix. The SDK
/// resolves it against repository state; constructing a selector never
/// chooses among multiple matches.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SnapshotSelector {
    id: Option<String>,
}

impl SnapshotSelector {
    /// Parses `latest`, a complete ID, or a safe ID prefix.
    pub fn parse(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.is_empty() {
            return Err(DomainError::InvalidSnapshotSelector {
                reason: "must not be empty",
            });
        }
        if value.eq_ignore_ascii_case(LATEST_SNAPSHOT_ALIAS) {
            return Ok(Self::latest());
        }
        validate_selector_id(&value)?;
        Ok(Self { id: Some(value) })
    }

    /// Returns the selector for repository HEAD.
    pub const fn latest() -> Self {
        Self { id: None }
    }

    /// Returns the original selector text in canonical display form.
    pub fn as_str(&self) -> &str {
        match self.id.as_deref() {
            Some(value) => value,
            None => LATEST_SNAPSHOT_ALIAS,
        }
    }

    /// Returns the ID or prefix, or `None` for the `latest` alias.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Returns whether this selector is the `latest` alias.
    pub const fn is_latest(&self) -> bool {
        self.id.is_none()
    }
}

impl fmt::Display for SnapshotSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for SnapshotSelector {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for SnapshotSelector {
    type Error = DomainError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for SnapshotSelector {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<SnapshotId> for SnapshotSelector {
    fn from(value: SnapshotId) -> Self {
        Self { id: Some(value.0) }
    }
}

impl From<&SnapshotId> for SnapshotSelector {
    fn from(value: &SnapshotId) -> Self {
        Self {
            id: Some(value.as_str().to_owned()),
        }
    }
}

impl From<SnapshotSelector> for String {
    fn from(value: SnapshotSelector) -> Self {
        value.as_str().to_owned()
    }
}

/// Compatibility name for [`SnapshotSelector`] used by request-oriented APIs.
pub type SnapshotReferenceInput = SnapshotSelector;

/// Short compatibility name for [`SnapshotSelector`].
pub type SnapshotRef = SnapshotSelector;

/// Compatibility name for [`SnapshotSelector`] used by reference resolvers.
pub type SnapshotReferenceSelector = SnapshotSelector;

/// Compatibility name for [`SnapshotSelector`] used by backup-oriented APIs.
pub type BackupReference = SnapshotSelector;

/// A compact authoritative snapshot header.
///
/// The full snapshot tree is referenced by object IDs and is deliberately not
/// part of this value. Listing history can therefore read and decode this
/// header without loading any tree objects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    id: SnapshotId,
    parent: Option<SnapshotId>,
    created_at: u64,
    message: String,
    author: Option<String>,
    root_tree: Option<RepositoryObject>,
    path_delta: Option<RepositoryObject>,
    file_count: u64,
    directory_count: u64,
    total_size: u64,
}

impl Snapshot {
    /// Creates a compact snapshot header with no parent or tree statistics.
    pub fn new(
        id: SnapshotId,
        message: impl Into<String>,
        created_at: u64,
    ) -> Result<Self, DomainError> {
        let message = message.into();
        validate_metadata_string(&message, MAX_SNAPSHOT_MESSAGE_LENGTH, "snapshot message")?;
        Ok(Self {
            id,
            parent: None,
            created_at,
            message,
            author: None,
            root_tree: None,
            path_delta: None,
            file_count: 0,
            directory_count: 0,
            total_size: 0,
        })
    }

    /// Sets the parent snapshot ID.
    pub fn with_parent(mut self, parent: Option<SnapshotId>) -> Self {
        self.parent = parent;
        self
    }

    /// Sets the snapshot author after validating its bounded metadata value.
    pub fn with_author(mut self, author: impl Into<String>) -> Result<Self, DomainError> {
        let author = author.into();
        validate_metadata_string(&author, MAX_SNAPSHOT_AUTHOR_LENGTH, "snapshot author")?;
        self.author = Some(author);
        Ok(self)
    }

    /// Sets the root-tree object reference.
    pub fn with_root_tree(mut self, root_tree: RepositoryObject) -> Self {
        self.root_tree = Some(root_tree);
        self
    }

    /// Sets the immutable path-delta object reference.
    pub fn with_path_delta(mut self, path_delta: RepositoryObject) -> Self {
        self.path_delta = Some(path_delta);
        self
    }

    /// Sets summary statistics without loading the referenced tree.
    pub fn with_statistics(
        mut self,
        file_count: u64,
        directory_count: u64,
        total_size: u64,
    ) -> Self {
        self.file_count = file_count;
        self.directory_count = directory_count;
        self.total_size = total_size;
        self
    }

    /// Returns the immutable snapshot ID.
    pub fn id(&self) -> &SnapshotId {
        &self.id
    }

    /// Alias for [`Self::id`] using backup terminology.
    pub fn snapshot_id(&self) -> &SnapshotId {
        self.id()
    }

    /// Returns the parent snapshot ID, when this is an incremental snapshot.
    pub fn parent(&self) -> Option<&SnapshotId> {
        self.parent.as_ref()
    }

    /// Returns the creation timestamp supplied by the backup producer.
    pub const fn created_at(&self) -> u64 {
        self.created_at
    }

    /// Alias for [`Self::created_at`] using history terminology.
    pub const fn timestamp(&self) -> u64 {
        self.created_at()
    }

    /// Returns the user-visible snapshot message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the author, when one was recorded.
    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }

    /// Returns the root-tree object reference, when one was recorded.
    pub fn root_tree(&self) -> Option<&RepositoryObject> {
        self.root_tree.as_ref()
    }

    /// Returns the path-delta object reference, when one was recorded.
    pub fn path_delta(&self) -> Option<&RepositoryObject> {
        self.path_delta.as_ref()
    }

    /// Returns the regular-file count in the snapshot.
    pub const fn file_count(&self) -> u64 {
        self.file_count
    }

    /// Returns the directory count in the snapshot.
    pub const fn directory_count(&self) -> u64 {
        self.directory_count
    }

    /// Returns the total logical size represented by the snapshot.
    pub const fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Returns the conventional immutable object reference for this snapshot.
    pub fn reference(&self) -> Result<SnapshotReference, DomainError> {
        self.id.object_reference()
    }
}

/// An immutable summary suitable for history, restore selectors, and CLI
/// presentation.
///
/// Summary values contain metadata and object references only. They never own
/// a snapshot tree. `publication_generation` is present for records written by
/// the history index and is used before timestamp tie-breakers when ordering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotSummary {
    id: SnapshotId,
    reference: SnapshotReference,
    parent: Option<SnapshotId>,
    message: String,
    author: Option<String>,
    timestamp: Option<u64>,
    size: Option<u64>,
    file_count: Option<u64>,
    directory_count: Option<u64>,
    total_size: Option<u64>,
    root_tree: Option<RepositoryObject>,
    path_delta: Option<RepositoryObject>,
    publication_generation: Option<u64>,
}

impl SnapshotSummary {
    /// Creates a summary from a snapshot ID or object reference and CLI
    /// history fields. A bare ID is converted to the conventional
    /// `snapshots/<id>` object reference.
    pub fn new(
        reference: impl AsRef<str>,
        message: impl Into<String>,
        timestamp: Option<u64>,
        size: Option<u64>,
    ) -> Result<Self, DomainError> {
        let value = reference.as_ref();
        let reference = if value.starts_with(SNAPSHOT_OBJECT_REFERENCE_PREFIX) {
            SnapshotReference::new(value.to_owned())?
        } else {
            SnapshotId::new(value.to_owned())?.object_reference()?
        };
        let id = SnapshotId::from_reference(&reference)?;
        let message = message.into();
        validate_metadata_string(&message, MAX_SNAPSHOT_MESSAGE_LENGTH, "snapshot message")?;
        Ok(Self {
            id,
            reference,
            parent: None,
            message,
            author: None,
            timestamp,
            size,
            file_count: None,
            directory_count: None,
            total_size: size,
            root_tree: None,
            path_delta: None,
            publication_generation: None,
        })
    }

    /// Builds a summary from an authoritative snapshot header.
    pub fn from_snapshot(snapshot: &Snapshot) -> Result<Self, DomainError> {
        let reference = snapshot.reference()?;
        Ok(Self::from_snapshot_at(snapshot, reference, None))
    }

    /// Creates a summary from a validated snapshot object reference and CLI
    /// history fields.
    pub fn from_reference(
        reference: SnapshotReference,
        message: impl Into<String>,
        timestamp: Option<u64>,
        size: Option<u64>,
    ) -> Result<Self, DomainError> {
        let id = SnapshotId::from_reference(&reference)?;
        let message = message.into();
        validate_metadata_string(&message, MAX_SNAPSHOT_MESSAGE_LENGTH, "snapshot message")?;
        Ok(Self {
            id,
            reference,
            parent: None,
            message,
            author: None,
            timestamp,
            size,
            file_count: None,
            directory_count: None,
            total_size: size,
            root_tree: None,
            path_delta: None,
            publication_generation: None,
        })
    }

    /// Returns the immutable snapshot ID.
    pub fn id(&self) -> &SnapshotId {
        &self.id
    }

    /// Alias for [`Self::id`] using backup terminology.
    pub fn snapshot_id(&self) -> &SnapshotId {
        self.id()
    }

    /// Alias for [`Self::id`] using the historical hash terminology.
    pub fn hash(&self) -> &str {
        self.id.as_str()
    }

    /// Returns the immutable snapshot object reference.
    pub fn reference(&self) -> &SnapshotReference {
        &self.reference
    }

    /// Alias for [`Self::reference`].
    pub fn snapshot_reference(&self) -> &SnapshotReference {
        self.reference()
    }

    /// Returns the parent snapshot ID, when present.
    pub fn parent(&self) -> Option<&SnapshotId> {
        self.parent.as_ref()
    }

    /// Returns the user-visible message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the author, when recorded.
    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }

    /// Returns the producer timestamp, when recorded.
    pub const fn timestamp(&self) -> Option<u64> {
        self.timestamp
    }

    /// Alias for [`Self::timestamp`] using snapshot terminology.
    pub const fn created_at(&self) -> Option<u64> {
        self.timestamp()
    }

    /// Returns the logical size, when recorded.
    pub const fn size(&self) -> Option<u64> {
        self.size
    }

    /// Alias for [`Self::size`] using snapshot statistics terminology.
    pub const fn total_size(&self) -> Option<u64> {
        self.total_size
    }

    /// Returns the regular-file count, when recorded.
    pub const fn file_count(&self) -> Option<u64> {
        self.file_count
    }

    /// Returns the directory count, when recorded.
    pub const fn directory_count(&self) -> Option<u64> {
        self.directory_count
    }

    /// Returns the referenced root tree, when recorded.
    pub fn root_tree(&self) -> Option<&RepositoryObject> {
        self.root_tree.as_ref()
    }

    /// Returns the referenced path delta, when recorded.
    pub fn path_delta(&self) -> Option<&RepositoryObject> {
        self.path_delta.as_ref()
    }

    /// Returns the publication generation used to order this history record.
    pub const fn publication_generation(&self) -> Option<u64> {
        self.publication_generation
    }

    /// Alias for [`Self::publication_generation`].
    pub const fn generation(&self) -> Option<u64> {
        self.publication_generation()
    }

    /// Adds an author to a summary after validating its bounded metadata value.
    pub fn with_author(mut self, author: impl Into<String>) -> Result<Self, DomainError> {
        let author = author.into();
        validate_metadata_string(&author, MAX_SNAPSHOT_AUTHOR_LENGTH, "snapshot author")?;
        self.author = Some(author);
        Ok(self)
    }

    pub(crate) fn with_parent(mut self, parent: Option<SnapshotId>) -> Self {
        self.parent = parent;
        self
    }

    pub(crate) fn with_root_tree(mut self, root_tree: RepositoryObject) -> Self {
        self.root_tree = Some(root_tree);
        self
    }

    pub(crate) fn with_path_delta(mut self, path_delta: RepositoryObject) -> Self {
        self.path_delta = Some(path_delta);
        self
    }

    pub(crate) fn with_statistics(
        mut self,
        file_count: Option<u64>,
        directory_count: Option<u64>,
        total_size: Option<u64>,
    ) -> Self {
        self.file_count = file_count;
        self.directory_count = directory_count;
        self.total_size = total_size;
        self
    }

    /// Returns a copy with an explicit publication generation for ordering.
    pub fn with_publication_generation(mut self, generation: u64) -> Self {
        self.publication_generation = Some(generation);
        self
    }

    pub(crate) fn from_snapshot_at(
        snapshot: &Snapshot,
        reference: SnapshotReference,
        publication_generation: Option<u64>,
    ) -> Self {
        Self {
            id: snapshot.id.clone(),
            reference,
            parent: snapshot.parent.clone(),
            message: snapshot.message.clone(),
            author: snapshot.author.clone(),
            timestamp: Some(snapshot.created_at),
            size: Some(snapshot.total_size),
            file_count: Some(snapshot.file_count),
            directory_count: Some(snapshot.directory_count),
            total_size: Some(snapshot.total_size),
            root_tree: snapshot.root_tree.clone(),
            path_delta: snapshot.path_delta.clone(),
            publication_generation,
        }
    }

    pub(crate) fn legacy(reference: SnapshotReference) -> Result<Self, DomainError> {
        Self::from_reference(reference, String::new(), None, None)
    }

    pub(crate) fn cursor_token(&self) -> String {
        match self.publication_generation {
            Some(generation) => format!("g:{generation:020}:{}", self.id()),
            None => format!("t:{:020}:{}", self.timestamp.unwrap_or_default(), self.id()),
        }
    }
}

/// An opaque cursor for deterministic snapshot-summary pagination.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SnapshotCursor(String);

impl SnapshotCursor {
    /// Creates a cursor after validating its bounded opaque representation.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_SNAPSHOT_CURSOR_LENGTH
            || !value.is_ascii()
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(DomainError::InvalidSnapshotSelector {
                reason: "history cursor must contain 1 to 256 printable ASCII bytes",
            });
        }
        Ok(Self(value))
    }

    /// Returns the opaque cursor representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SnapshotCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for SnapshotCursor {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&SnapshotCursor> for SnapshotCursor {
    fn from(value: &SnapshotCursor) -> Self {
        value.clone()
    }
}

impl TryFrom<&str> for SnapshotCursor {
    type Error = DomainError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for SnapshotCursor {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Input for one paginated snapshot-summary request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotListRequest {
    limit: usize,
    cursor: Option<SnapshotCursor>,
}

impl SnapshotListRequest {
    /// Creates a request with the SDK page-size default.
    pub const fn new() -> Self {
        Self {
            limit: DEFAULT_SNAPSHOT_PAGE_SIZE,
            cursor: None,
        }
    }

    /// Sets the requested page size. Bounds are enforced by the SDK operation.
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Alias for [`Self::with_limit`].
    pub const fn limit(self, limit: usize) -> Self {
        self.with_limit(limit)
    }

    /// Sets the cursor returned by the previous page.
    pub fn after(mut self, cursor: impl Into<SnapshotCursor>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// Alias for [`Self::after`] using cursor terminology.
    pub fn with_cursor(self, cursor: impl Into<SnapshotCursor>) -> Self {
        self.after(cursor)
    }

    /// Returns the requested page size before operation-level validation.
    pub const fn requested_limit(&self) -> usize {
        self.limit
    }

    /// Returns the continuation cursor, when present.
    pub fn cursor(&self) -> Option<&SnapshotCursor> {
        self.cursor.as_ref()
    }
}

impl Default for SnapshotListRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl From<()> for SnapshotListRequest {
    fn from(_: ()) -> Self {
        Self::new()
    }
}

/// One page of snapshot summaries in deterministic newest-first order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotSummaryPage {
    summaries: Vec<SnapshotSummary>,
    next_cursor: Option<SnapshotCursor>,
}

impl SnapshotSummaryPage {
    pub(crate) fn new(
        summaries: Vec<SnapshotSummary>,
        next_cursor: Option<SnapshotCursor>,
    ) -> Self {
        Self {
            summaries,
            next_cursor,
        }
    }

    /// Returns summaries in newest-first order.
    pub fn summaries(&self) -> &[SnapshotSummary] {
        &self.summaries
    }

    /// Alias for [`Self::summaries`] using page terminology.
    pub fn items(&self) -> &[SnapshotSummary] {
        self.summaries()
    }

    /// Returns the continuation cursor, if another page exists.
    pub fn next_cursor(&self) -> Option<&SnapshotCursor> {
        self.next_cursor.as_ref()
    }

    /// Returns whether another page exists.
    pub fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }

    /// Returns the number of summaries in this page.
    pub const fn len(&self) -> usize {
        self.summaries.len()
    }

    /// Returns whether this page contains no summaries.
    pub const fn is_empty(&self) -> bool {
        self.summaries.is_empty()
    }

    /// Consumes the page into its summaries and continuation cursor.
    pub fn into_parts(self) -> (Vec<SnapshotSummary>, Option<SnapshotCursor>) {
        (self.summaries, self.next_cursor)
    }
}

/// Compatibility name for [`SnapshotSummaryPage`] used by history APIs.
pub type SnapshotHistoryPage = SnapshotSummaryPage;

/// Compatibility name for [`SnapshotListRequest`] used by history APIs.
pub type SnapshotHistoryRequest = SnapshotListRequest;

/// Compatibility name for [`SnapshotListRequest`] used by summary APIs.
pub type SnapshotSummaryListRequest = SnapshotListRequest;

/// Compatibility name for [`SnapshotSummaryPage`] used by generic pagers.
pub type SnapshotPage = SnapshotSummaryPage;

fn validate_selector_id(value: &str) -> Result<(), DomainError> {
    if value.len() > MAX_SNAPSHOT_ID_LENGTH {
        return Err(DomainError::InvalidSnapshotSelector {
            reason: "must contain at most 128 ASCII identifier bytes",
        });
    }
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err(DomainError::InvalidSnapshotSelector {
            reason: "must not be empty",
        });
    };
    if !first.is_ascii_alphanumeric()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(DomainError::InvalidSnapshotSelector {
            reason: "must be an ASCII snapshot ID or prefix",
        });
    }
    Ok(())
}

fn validate_metadata_string(
    value: &str,
    max_length: usize,
    field: &'static str,
) -> Result<(), DomainError> {
    if value.len() > max_length || value.contains('\0') {
        return Err(DomainError::InvalidSnapshotMetadata {
            reason: match field {
                "snapshot message" => "message must contain at most 512 bytes and no NUL",
                "snapshot author" => "author must contain at most 512 bytes and no NUL",
                _ => "metadata is too large or contains NUL",
            },
        });
    }
    Ok(())
}

impl SnapshotReference {
    /// Creates the conventional object reference for an immutable snapshot ID.
    pub fn from_id(id: SnapshotId) -> Result<Self, DomainError> {
        id.object_reference()
    }

    /// Extracts the immutable snapshot ID from this object reference.
    pub fn snapshot_id(&self) -> Result<SnapshotId, DomainError> {
        SnapshotId::from_reference(self)
    }

    /// Parses a user-facing ID, prefix, or `latest` selector.
    pub fn parse_selector(value: impl Into<String>) -> Result<SnapshotSelector, DomainError> {
        SnapshotSelector::parse(value)
    }

    /// Alias for [`Self::parse_selector`].
    pub fn parse(value: impl Into<String>) -> Result<SnapshotSelector, DomainError> {
        Self::parse_selector(value)
    }
}

impl TryFrom<SnapshotId> for SnapshotReference {
    type Error = DomainError;

    fn try_from(value: SnapshotId) -> Result<Self, Self::Error> {
        Self::from_id(value)
    }
}
