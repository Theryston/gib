use super::ports::{RepositoryStorage, StorageError, StorageVersion};
use crate::domain::{
    FORMAT_OBJECT_KEY, HEAD_OBJECT_KEY, REPOSITORY_DESCRIPTOR_OBJECT_KEY, RepositoryDescriptor,
    RepositoryHead, RepositoryIdentity, RepositoryKey, SnapshotPublication,
};
use crate::format::{
    FormatError, decode_bootstrap, decode_descriptor, decode_head, encode_bootstrap,
    encode_descriptor, encode_head,
};
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
            Err(StorageError::ConditionNotMet) => return Err(RepositoryError::PublicationConflict),
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
        };

    Ok(HeadRead {
        head,
        version: Some(version),
    })
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
        FormatError::Serialization => RepositoryError::Storage {
            operation: "encode",
        },
    }
}
