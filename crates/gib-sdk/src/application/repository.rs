use super::ports::{RepositoryStorage, StorageError, StorageVersion};
use crate::domain::{
    FORMAT_OBJECT_KEY, HEAD_OBJECT_KEY, MAX_SNAPSHOT_PAGE_SIZE, REPOSITORY_DESCRIPTOR_OBJECT_KEY,
    RepositoryDescriptor, RepositoryHead, RepositoryIdentity, RepositoryKey,
    SNAPSHOT_HISTORY_OBJECT_PREFIX, SNAPSHOT_OBJECT_PREFIX, Snapshot, SnapshotCursor,
    SnapshotListRequest, SnapshotPublication, SnapshotReference, SnapshotSelector, SnapshotSummary,
    SnapshotSummaryPage,
};
use crate::format::{
    FormatError, decode_bootstrap, decode_descriptor, decode_head, decode_history_record,
    decode_snapshot, encode_bootstrap, encode_descriptor, encode_head, encode_history_record,
};
use std::cmp::Ordering;
use std::collections::HashSet;

const MAX_PUBLICATION_OBJECTS: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepositoryError {
    AlreadyExists,
    Missing,
    Malformed { reason: &'static str },
    UnsupportedVersion { version: u16 },
    Incompatible { reason: &'static str },
    PublicationConflict,
    SnapshotMissing,
    RequiredObjectMissing,
    InvalidPublication { reason: &'static str },
    GenerationExhausted,
    UnsupportedCapability,
    Cancelled,
    NoSnapshots,
    SnapshotReferenceEmpty,
    SnapshotReferenceMalformed,
    SnapshotReferenceNotFound,
    SnapshotReferenceAmbiguous,
    SnapshotHistoryRequestInvalid,
    SnapshotHistoryCursorInvalid,
    Storage { operation: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RepositoryOpenExpectations<'a> {
    pub(crate) identity: Option<&'a RepositoryIdentity>,
    pub(crate) repository_key: Option<&'a RepositoryKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HeadRead {
    pub(crate) head: RepositoryHead,
    pub(crate) version: Option<StorageVersion>,
}

pub(crate) fn initialize_repository(
    storage: &dyn RepositoryStorage,
    identity: RepositoryIdentity,
    repository_key: RepositoryKey,
) -> Result<RepositoryDescriptor, RepositoryError> {
    let descriptor = RepositoryDescriptor::new(identity, repository_key);
    let format_bytes = encode_bootstrap().map_err(map_format_error)?;
    let descriptor_bytes = encode_descriptor(&descriptor).map_err(map_format_error)?;

    ensure_absent(storage, FORMAT_OBJECT_KEY)?;
    ensure_absent(storage, REPOSITORY_DESCRIPTOR_OBJECT_KEY)?;

    storage
        .create_if_absent(FORMAT_OBJECT_KEY, &format_bytes)
        .map_err(|error| map_create_error(error, "format"))?;
    storage
        .create_if_absent(REPOSITORY_DESCRIPTOR_OBJECT_KEY, &descriptor_bytes)
        .map_err(|error| map_create_error(error, "descriptor"))?;

    Ok(descriptor)
}

pub(crate) fn open_repository(
    storage: &dyn RepositoryStorage,
    expectations: RepositoryOpenExpectations<'_>,
) -> Result<RepositoryDescriptor, RepositoryError> {
    let format_bytes = storage
        .read(FORMAT_OBJECT_KEY)
        .map_err(|error| map_read_error(error, "format"))?;
    let bootstrap = decode_bootstrap(&format_bytes).map_err(map_format_error)?;

    let descriptor_bytes = storage
        .read(REPOSITORY_DESCRIPTOR_OBJECT_KEY)
        .map_err(|error| map_read_error(error, "descriptor"))?;
    let descriptor =
        decode_descriptor(&descriptor_bytes, bootstrap.format_version).map_err(map_format_error)?;

    if expectations
        .identity
        .is_some_and(|expected| expected != descriptor.identity())
    {
        return Err(RepositoryError::Incompatible {
            reason: "repository identity does not match the requested identity",
        });
    }
    if expectations
        .repository_key
        .is_some_and(|expected| expected != descriptor.repository_key())
    {
        return Err(RepositoryError::Incompatible {
            reason: "repository key does not match the requested key",
        });
    }

    validate_existing_head(storage)?;

    Ok(descriptor)
}

fn validate_existing_head(storage: &dyn RepositoryStorage) -> Result<(), RepositoryError> {
    match storage.read(HEAD_OBJECT_KEY) {
        Ok(_) => read_head(storage).map(|_| ()),
        Err(StorageError::NotFound) => Ok(()),
        Err(StorageError::InvalidObjectKey) => Err(RepositoryError::Malformed {
            reason: "repository HEAD has an invalid storage representation",
        }),
        Err(
            StorageError::AlreadyExists
            | StorageError::Io
            | StorageError::Unavailable
            | StorageError::UnsupportedCapability
            | StorageError::ConditionNotMet
            | StorageError::InvalidVersion,
        ) => Err(RepositoryError::Storage {
            operation: "read_head",
        }),
        _ => Err(RepositoryError::Storage {
            operation: "read_head",
        }),
    }
}

pub(crate) fn read_head(storage: &dyn RepositoryStorage) -> Result<HeadRead, RepositoryError> {
    match storage.read_versioned(HEAD_OBJECT_KEY) {
        Ok(object) => {
            let head = decode_head(object.contents()).map_err(map_format_error)?;
            Ok(HeadRead {
                head,
                version: Some(object.version().clone()),
            })
        }
        Err(StorageError::NotFound) => Ok(HeadRead {
            head: RepositoryHead::empty(),
            version: None,
        }),
        Err(StorageError::UnsupportedCapability) => Err(RepositoryError::UnsupportedCapability),
        Err(StorageError::InvalidObjectKey | StorageError::InvalidVersion) => {
            Err(RepositoryError::Malformed {
                reason: "repository HEAD has an invalid storage representation",
            })
        }
        Err(StorageError::AlreadyExists | StorageError::Io | StorageError::Unavailable) => {
            Err(RepositoryError::Storage {
                operation: "read_head",
            })
        }
        Err(StorageError::ConditionNotMet) => Err(RepositoryError::Storage {
            operation: "read_head",
        }),
        _ => Err(RepositoryError::Storage {
            operation: "read_head",
        }),
    }
}

pub(crate) fn publish_head(
    storage: &dyn RepositoryStorage,
    expected: &HeadRead,
    publication: &SnapshotPublication,
    is_cancelled: Option<&dyn Fn() -> bool>,
) -> Result<HeadRead, RepositoryError> {
    check_cancelled(is_cancelled)?;
    validate_publication(publication)?;

    let mut seen = HashSet::with_capacity(publication.required_objects().len() + 1);
    let snapshot_key = publication.snapshot().as_str();
    seen.insert(snapshot_key);
    match storage.read(snapshot_key) {
        Ok(contents) if !contents.is_empty() => {}
        Ok(_) => {
            return Err(RepositoryError::InvalidPublication {
                reason: "the target snapshot object is empty",
            });
        }
        Err(StorageError::NotFound) => return Err(RepositoryError::SnapshotMissing),
        Err(StorageError::InvalidObjectKey) => {
            return Err(RepositoryError::InvalidPublication {
                reason: "the target snapshot reference is not a valid storage key",
            });
        }
        Err(
            StorageError::AlreadyExists
            | StorageError::Io
            | StorageError::Unavailable
            | StorageError::UnsupportedCapability
            | StorageError::ConditionNotMet
            | StorageError::InvalidVersion,
        ) => {
            return Err(RepositoryError::Storage {
                operation: "validate_snapshot",
            });
        }
        _ => {
            return Err(RepositoryError::Storage {
                operation: "validate_snapshot",
            });
        }
    }
    for object in publication.required_objects() {
        if !seen.insert(object.as_str()) {
            continue;
        }
        match storage.read(object.as_str()) {
            Ok(_) => {}
            Err(StorageError::NotFound) => return Err(RepositoryError::RequiredObjectMissing),
            Err(StorageError::InvalidObjectKey) => {
                return Err(RepositoryError::InvalidPublication {
                    reason: "required object reference is not a valid storage key",
                });
            }
            Err(
                StorageError::AlreadyExists
                | StorageError::Io
                | StorageError::Unavailable
                | StorageError::UnsupportedCapability
                | StorageError::ConditionNotMet
                | StorageError::InvalidVersion,
            ) => {
                return Err(RepositoryError::Storage {
                    operation: "validate_publication",
                });
            }
            _ => {
                return Err(RepositoryError::Storage {
                    operation: "validate_publication",
                });
            }
        }
    }

    check_cancelled(is_cancelled)?;
    let head = expected
        .head
        .advance_to(publication.snapshot().clone())
        .map_err(|_| RepositoryError::GenerationExhausted)?;
    let head_bytes = encode_head(&head).map_err(map_format_error)?;
    check_cancelled(is_cancelled)?;
    let version =
        match storage.conditional_write(HEAD_OBJECT_KEY, expected.version.as_ref(), &head_bytes) {
            Ok(version) => version,
            Err(StorageError::ConditionNotMet | StorageError::Conflict) => {
                return Err(RepositoryError::PublicationConflict);
            }
            Err(StorageError::UnsupportedCapability) => {
                return Err(RepositoryError::UnsupportedCapability);
            }
            Err(StorageError::InvalidObjectKey | StorageError::InvalidVersion) => {
                return Err(RepositoryError::Malformed {
                    reason: "repository HEAD has an invalid storage representation",
                });
            }
            Err(
                StorageError::NotFound
                | StorageError::AlreadyExists
                | StorageError::Io
                | StorageError::Unavailable,
            ) => {
                return Err(RepositoryError::Storage {
                    operation: "publish_head",
                });
            }
            _ => {
                return Err(RepositoryError::Storage {
                    operation: "publish_head",
                });
            }
        };

    persist_history_record(storage, head.generation(), publication);

    Ok(HeadRead {
        head,
        version: Some(version),
    })
}

pub(crate) fn list_snapshot_summaries(
    storage: &dyn RepositoryStorage,
    request: &SnapshotListRequest,
) -> Result<SnapshotSummaryPage, RepositoryError> {
    let limit = request.requested_limit();
    if !(1..=MAX_SNAPSHOT_PAGE_SIZE).contains(&limit) {
        return Err(RepositoryError::SnapshotHistoryRequestInvalid);
    }

    let summaries = load_snapshot_summaries(storage)?;
    let start = match request.cursor() {
        Some(cursor) => summaries
            .iter()
            .position(|summary| summary.cursor_token() == cursor.as_str())
            .map(|position| position + 1)
            .ok_or(RepositoryError::SnapshotHistoryCursorInvalid)?,
        None => 0,
    };
    let end = start.saturating_add(limit).min(summaries.len());
    let page = summaries.get(start..end).unwrap_or_default().to_vec();
    let next_cursor = if end < summaries.len() {
        page.last()
            .map(SnapshotSummary::cursor_token)
            .and_then(|value| SnapshotCursor::new(value).ok())
    } else {
        None
    };
    Ok(SnapshotSummaryPage::new(page, next_cursor))
}

pub(crate) fn rebuild_snapshot_summaries(
    storage: &dyn RepositoryStorage,
) -> Result<Vec<SnapshotSummary>, RepositoryError> {
    let mut summaries = Vec::new();
    for object_key in list_objects(storage, SNAPSHOT_OBJECT_PREFIX, "list_snapshots")? {
        let reference =
            SnapshotReference::new(object_key).map_err(|_| RepositoryError::Malformed {
                reason: "snapshot listing contains an invalid object reference",
            })?;
        let bytes = storage
            .read(reference.as_str())
            .map_err(|error| match error {
                StorageError::NotFound => RepositoryError::SnapshotMissing,
                other => map_storage_error(other, "read_snapshot"),
            })?;
        match decode_snapshot(&bytes) {
            Ok(snapshot) => {
                validate_snapshot_identity(&reference, &snapshot)?;
                summaries.push(SnapshotSummary::from_snapshot_at(
                    &snapshot, reference, None,
                ));
            }
            Err(_error) if is_legacy_snapshot_bytes(&bytes) => {
                summaries.push(SnapshotSummary::legacy(reference).map_err(|_| {
                    RepositoryError::Malformed {
                        reason: "legacy snapshot reference cannot produce a summary",
                    }
                })?);
            }
            Err(error) => return Err(map_format_error(error)),
        }
    }
    sort_summaries(&mut summaries);
    Ok(summaries)
}

pub(crate) fn resolve_snapshot_reference(
    storage: &dyn RepositoryStorage,
    reference: &str,
) -> Result<SnapshotReference, RepositoryError> {
    if reference.is_empty() {
        return Err(RepositoryError::SnapshotReferenceEmpty);
    }
    let selector = SnapshotSelector::parse(reference)
        .map_err(|_| RepositoryError::SnapshotReferenceMalformed)?;
    if selector.is_latest() {
        let head = read_head(storage)?;
        let Some(snapshot) = head.head.snapshot().cloned() else {
            return Err(RepositoryError::NoSnapshots);
        };
        match storage.read(snapshot.as_str()) {
            Ok(_) => return Ok(snapshot),
            Err(StorageError::NotFound) => return Err(RepositoryError::SnapshotMissing),
            Err(error) => return Err(map_storage_error(error, "resolve_latest_snapshot")),
        }
    }

    let query = selector
        .id()
        .ok_or(RepositoryError::SnapshotReferenceMalformed)?;
    let summaries = load_snapshot_summaries(storage)?;
    let exact = summaries
        .iter()
        .filter(|summary| summary.id().as_str() == query)
        .map(SnapshotSummary::reference)
        .collect::<HashSet<_>>();
    if exact.len() == 1 {
        return exact
            .into_iter()
            .next()
            .cloned()
            .ok_or(RepositoryError::SnapshotReferenceAmbiguous);
    }
    if exact.len() > 1 {
        return Err(RepositoryError::SnapshotReferenceAmbiguous);
    }

    let matches = summaries
        .iter()
        .filter(|summary| summary.id().as_str().starts_with(query))
        .map(SnapshotSummary::reference)
        .collect::<HashSet<_>>();
    match matches.len() {
        0 => Err(RepositoryError::SnapshotReferenceNotFound),
        1 => matches
            .into_iter()
            .next()
            .cloned()
            .ok_or(RepositoryError::SnapshotReferenceAmbiguous),
        _ => Err(RepositoryError::SnapshotReferenceAmbiguous),
    }
}

pub(crate) fn read_snapshot_summary(
    storage: &dyn RepositoryStorage,
    reference: &str,
) -> Result<SnapshotSummary, RepositoryError> {
    let resolved = resolve_snapshot_reference(storage, reference)?;
    let bytes = storage
        .read(resolved.as_str())
        .map_err(|error| match error {
            StorageError::NotFound => RepositoryError::SnapshotMissing,
            other => map_storage_error(other, "read_snapshot"),
        })?;
    match decode_snapshot(&bytes) {
        Ok(snapshot) => {
            validate_snapshot_identity(&resolved, &snapshot)?;
            Ok(SnapshotSummary::from_snapshot_at(&snapshot, resolved, None))
        }
        Err(_error) if is_legacy_snapshot_bytes(&bytes) => SnapshotSummary::legacy(resolved)
            .map_err(|_| RepositoryError::Malformed {
                reason: "legacy snapshot reference cannot produce a summary",
            }),
        Err(error) => Err(map_format_error(error)),
    }
}

fn load_snapshot_summaries(
    storage: &dyn RepositoryStorage,
) -> Result<Vec<SnapshotSummary>, RepositoryError> {
    let history = load_history_summaries(storage)?;
    if let Some(mut history) = history {
        let head = read_head(storage)?;
        let generation = head.head.generation();
        let complete_history = usize::try_from(generation).is_ok_and(|expected| {
            expected == history.len()
                && history
                    .iter()
                    .filter_map(SnapshotSummary::publication_generation)
                    .collect::<HashSet<_>>()
                    .len()
                    == expected
                && history.iter().all(|summary| {
                    summary
                        .publication_generation()
                        .is_some_and(|value| value > 0 && value <= generation)
                })
        });
        if complete_history {
            history.retain(|summary| {
                summary
                    .publication_generation()
                    .is_none_or(|value| value <= generation)
            });
            sort_summaries(&mut history);
            return Ok(history);
        }
    }
    rebuild_snapshot_summaries(storage)
}

fn load_history_summaries(
    storage: &dyn RepositoryStorage,
) -> Result<Option<Vec<SnapshotSummary>>, RepositoryError> {
    let keys = list_objects(storage, SNAPSHOT_HISTORY_OBJECT_PREFIX, "list_history")?;
    if keys.is_empty() {
        return Ok(None);
    }
    let mut summaries = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(key_generation) = parse_history_generation(&key) else {
            return Err(RepositoryError::Malformed {
                reason: "snapshot history contains an invalid record key",
            });
        };
        let bytes = storage.read(&key).map_err(|error| match error {
            StorageError::NotFound => RepositoryError::SnapshotMissing,
            other => map_storage_error(other, "read_history"),
        })?;
        let record = decode_history_record(&bytes).map_err(map_format_error)?;
        if record.generation != key_generation {
            return Err(RepositoryError::Malformed {
                reason: "snapshot history record generation does not match its key",
            });
        }
        summaries.push(record.summary);
    }
    sort_summaries(&mut summaries);
    Ok(Some(summaries))
}

fn persist_history_record(
    storage: &dyn RepositoryStorage,
    generation: u64,
    publication: &SnapshotPublication,
) {
    let summary = snapshot_summary_for_publication(storage, publication)
        .ok()
        .or_else(|| publication.summary().cloned())
        .or_else(|| SnapshotSummary::legacy(publication.snapshot().clone()).ok());
    let Some(summary) = summary else {
        return;
    };
    let Ok(bytes) = encode_history_record(generation, &summary) else {
        return;
    };
    let key = history_object_key(generation);
    let _ = storage.create_if_absent(&key, &bytes);
}

fn snapshot_summary_for_publication(
    storage: &dyn RepositoryStorage,
    publication: &SnapshotPublication,
) -> Result<SnapshotSummary, RepositoryError> {
    let reference = publication.snapshot().clone();
    let bytes = storage
        .read(reference.as_str())
        .map_err(|error| match error {
            StorageError::NotFound => RepositoryError::SnapshotMissing,
            other => map_storage_error(other, "read_snapshot"),
        })?;
    let snapshot = decode_snapshot(&bytes).map_err(map_format_error)?;
    validate_snapshot_identity(&reference, &snapshot)?;
    Ok(SnapshotSummary::from_snapshot_at(
        &snapshot, reference, None,
    ))
}

fn validate_snapshot_identity(
    reference: &SnapshotReference,
    snapshot: &Snapshot,
) -> Result<(), RepositoryError> {
    let reference_id = reference
        .snapshot_id()
        .map_err(|_| RepositoryError::Malformed {
            reason: "snapshot object reference has no valid immutable ID",
        })?;
    if reference_id != *snapshot.id() {
        return Err(RepositoryError::Malformed {
            reason: "snapshot ID does not match its object reference",
        });
    }
    Ok(())
}

fn sort_summaries(summaries: &mut [SnapshotSummary]) {
    summaries.sort_by(compare_summaries);
}

fn compare_summaries(left: &SnapshotSummary, right: &SnapshotSummary) -> Ordering {
    match (
        left.publication_generation(),
        right.publication_generation(),
    ) {
        (Some(left_generation), Some(right_generation)) => right_generation
            .cmp(&left_generation)
            .then_with(|| right.id().cmp(left.id())),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => right
            .timestamp()
            .unwrap_or_default()
            .cmp(&left.timestamp().unwrap_or_default())
            .then_with(|| right.id().cmp(left.id())),
    }
}

fn list_objects(
    storage: &dyn RepositoryStorage,
    prefix: &str,
    operation: &'static str,
) -> Result<Vec<String>, RepositoryError> {
    storage.list_objects(prefix).map_err(|error| match error {
        StorageError::UnsupportedCapability => RepositoryError::UnsupportedCapability,
        other => map_storage_error(other, operation),
    })
}

fn history_object_key(generation: u64) -> String {
    format!("{SNAPSHOT_HISTORY_OBJECT_PREFIX}/{generation:020}")
}

fn parse_history_generation(key: &str) -> Option<u64> {
    let prefix = format!("{SNAPSHOT_HISTORY_OBJECT_PREFIX}/");
    let value = key.strip_prefix(&prefix)?;
    if value.len() != 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn is_legacy_snapshot_bytes(bytes: &[u8]) -> bool {
    !bytes
        .first()
        .is_some_and(|byte| matches!(byte, 0x80..=0x8f | 0xde | 0xdf))
}

fn map_storage_error(error: StorageError, operation: &'static str) -> RepositoryError {
    match error {
        StorageError::InvalidObjectKey | StorageError::InvalidVersion => {
            RepositoryError::Malformed {
                reason: "repository storage returned an invalid object representation",
            }
        }
        StorageError::UnsupportedCapability => RepositoryError::UnsupportedCapability,
        StorageError::NotFound
        | StorageError::AlreadyExists
        | StorageError::Io
        | StorageError::Unavailable
        | StorageError::ConditionNotMet => RepositoryError::Storage { operation },
        _ => RepositoryError::Storage { operation },
    }
}

fn validate_publication(publication: &SnapshotPublication) -> Result<(), RepositoryError> {
    if publication.required_objects().len() > MAX_PUBLICATION_OBJECTS {
        return Err(RepositoryError::InvalidPublication {
            reason: "publication references too many required objects",
        });
    }
    if publication.snapshot().as_str() == HEAD_OBJECT_KEY {
        return Err(RepositoryError::InvalidPublication {
            reason: "a publication cannot use HEAD as its snapshot",
        });
    }
    for object in publication.required_objects() {
        if object.as_str() == HEAD_OBJECT_KEY {
            return Err(RepositoryError::InvalidPublication {
                reason: "a required object cannot reference HEAD",
            });
        }
    }
    Ok(())
}

fn check_cancelled(is_cancelled: Option<&dyn Fn() -> bool>) -> Result<(), RepositoryError> {
    if is_cancelled.is_some_and(|is_cancelled| is_cancelled()) {
        Err(RepositoryError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_create_error(error: StorageError, _object: &'static str) -> RepositoryError {
    match error {
        StorageError::AlreadyExists => RepositoryError::AlreadyExists,
        StorageError::NotFound
        | StorageError::InvalidObjectKey
        | StorageError::Io
        | StorageError::Unavailable
        | StorageError::UnsupportedCapability
        | StorageError::ConditionNotMet
        | StorageError::InvalidVersion => RepositoryError::Storage {
            operation: "create",
        },
        _ => RepositoryError::Storage {
            operation: "create",
        },
    }
}

fn map_read_error(error: StorageError, _object: &'static str) -> RepositoryError {
    match error {
        StorageError::NotFound => RepositoryError::Missing,
        StorageError::InvalidObjectKey => RepositoryError::Malformed {
            reason: "required repository object has an invalid storage type",
        },
        StorageError::AlreadyExists
        | StorageError::Io
        | StorageError::Unavailable
        | StorageError::UnsupportedCapability
        | StorageError::ConditionNotMet
        | StorageError::InvalidVersion => RepositoryError::Storage { operation: "read" },
        _ => RepositoryError::Storage { operation: "read" },
    }
}

fn ensure_absent(
    storage: &dyn RepositoryStorage,
    object_key: &'static str,
) -> Result<(), RepositoryError> {
    match storage.read(object_key) {
        Ok(_) | Err(StorageError::InvalidObjectKey) => Err(RepositoryError::AlreadyExists),
        Err(StorageError::NotFound) => Ok(()),
        Err(StorageError::AlreadyExists | StorageError::Io | StorageError::Unavailable) => {
            Err(RepositoryError::Storage { operation: "read" })
        }
        Err(
            StorageError::UnsupportedCapability
            | StorageError::ConditionNotMet
            | StorageError::InvalidVersion,
        ) => Err(RepositoryError::Storage { operation: "read" }),
        _ => Err(RepositoryError::Storage { operation: "read" }),
    }
}

fn map_format_error(error: FormatError) -> RepositoryError {
    match error {
        FormatError::UnsupportedVersion { version } => {
            RepositoryError::UnsupportedVersion { version }
        }
        FormatError::MissingRequiredFeature | FormatError::UnsupportedRequiredFeature => {
            RepositoryError::Incompatible {
                reason: "required repository feature flags are not supported",
            }
        }
        FormatError::InvalidEncoding => RepositoryError::Malformed {
            reason: "repository object is not valid MessagePack",
        },
        FormatError::InputTooLarge => RepositoryError::Malformed {
            reason: "repository object exceeds the MessagePack size limit",
        },
        FormatError::TrailingBytes => RepositoryError::Malformed {
            reason: "repository object contains trailing MessagePack bytes",
        },
        FormatError::InvalidMagic => RepositoryError::Malformed {
            reason: "repository magic is invalid",
        },
        FormatError::InvalidRootReference => RepositoryError::Malformed {
            reason: "repository root object reference is invalid",
        },
        FormatError::InvalidField => RepositoryError::Malformed {
            reason: "repository descriptor field is invalid",
        },
        FormatError::VersionMismatch => RepositoryError::Malformed {
            reason: "repository descriptor and bootstrap versions differ",
        },
        FormatError::InvalidChecksum => RepositoryError::Malformed {
            reason: "repository object integrity check failed",
        },
        FormatError::InvalidObjectKind => RepositoryError::Malformed {
            reason: "repository object kind is invalid",
        },
        FormatError::UnsupportedObjectVersion { version } => {
            RepositoryError::UnsupportedVersion { version }
        }
        FormatError::InvalidCodec
        | FormatError::InvalidEncryption
        | FormatError::InvalidLength
        | FormatError::InvalidDigestLength
        | FormatError::InvalidObjectId
        | FormatError::InvalidPayloadChecksum
        | FormatError::InvalidEnvelopeChecksum => RepositoryError::Malformed {
            reason: "repository immutable object integrity or metadata is invalid",
        },
        FormatError::UnsupportedCodec | FormatError::UnsupportedEncryption => {
            RepositoryError::Incompatible {
                reason: "repository immutable object transport metadata is not supported",
            }
        }
        FormatError::Serialization => RepositoryError::Storage {
            operation: "encode",
        },
    }
}
