use super::ports::{RepositoryStorage, StorageError};
use crate::domain::{
    FORMAT_OBJECT_KEY, REPOSITORY_DESCRIPTOR_OBJECT_KEY, RepositoryDescriptor, RepositoryIdentity,
    RepositoryKey,
};
use crate::format::{
    FormatError, decode_descriptor, decode_format_marker, encode_descriptor, encode_format_marker,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepositoryError {
    AlreadyExists,
    Missing,
    Malformed { reason: &'static str },
    UnsupportedVersion { version: u16 },
    Incompatible { reason: &'static str },
    Storage { operation: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RepositoryOpenExpectations<'a> {
    pub(crate) identity: Option<&'a RepositoryIdentity>,
    pub(crate) repository_key: Option<&'a RepositoryKey>,
}

pub(crate) fn initialize_repository(
    storage: &dyn RepositoryStorage,
    identity: RepositoryIdentity,
    repository_key: RepositoryKey,
) -> Result<RepositoryDescriptor, RepositoryError> {
    let descriptor = RepositoryDescriptor::new(identity, repository_key);
    let format_bytes = encode_format_marker().map_err(map_format_error)?;
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
    let marker = decode_format_marker(&format_bytes).map_err(map_format_error)?;

    let descriptor_bytes = storage
        .read(REPOSITORY_DESCRIPTOR_OBJECT_KEY)
        .map_err(|error| map_read_error(error, "descriptor"))?;
    let descriptor =
        decode_descriptor(&descriptor_bytes, marker.version).map_err(map_format_error)?;

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

    Ok(descriptor)
}

fn map_create_error(error: StorageError, _object: &'static str) -> RepositoryError {
    match error {
        StorageError::AlreadyExists => RepositoryError::AlreadyExists,
        StorageError::NotFound
        | StorageError::InvalidObjectKey
        | StorageError::Io
        | StorageError::Unavailable => RepositoryError::Storage {
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
        StorageError::AlreadyExists | StorageError::Io | StorageError::Unavailable => {
            RepositoryError::Storage { operation: "read" }
        }
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
            reason: "repository object is not valid JSON",
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
            reason: "repository descriptor and format marker versions differ",
        },
        FormatError::Serialization => RepositoryError::Storage {
            operation: "encode",
        },
    }
}
