use super::ports::{ConfigurationError, ConfigurationStorage};
use crate::domain::AuthorIdentity;
use crate::format::{FormatError, decode_identity_configuration, encode_identity_configuration};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityError {
    NotConfigured,
    Malformed,
    UnsupportedVersion { version: u16 },
    Storage { operation: &'static str },
}

pub(crate) fn get_identity(
    storage: &dyn ConfigurationStorage,
) -> Result<AuthorIdentity, IdentityError> {
    let bytes = storage
        .read_configuration()
        .map_err(|error| map_storage_error(error, "read_identity"))?;
    decode_identity_configuration(&bytes).map_err(map_format_error)
}

pub(crate) fn read_identity(
    storage: &dyn ConfigurationStorage,
) -> Result<Option<AuthorIdentity>, IdentityError> {
    match storage.read_configuration() {
        Ok(bytes) => decode_identity_configuration(&bytes)
            .map(Some)
            .map_err(map_format_error),
        Err(ConfigurationError::NotFound) => Ok(None),
        Err(error) => Err(map_storage_error(error, "read_identity")),
    }
}

pub(crate) fn set_identity(
    storage: &dyn ConfigurationStorage,
    identity: AuthorIdentity,
) -> Result<AuthorIdentity, IdentityError> {
    let bytes = encode_identity_configuration(&identity).map_err(map_format_error)?;
    storage
        .replace_atomically(&bytes)
        .map_err(|error| map_storage_error(error, "write_identity"))?;
    Ok(identity)
}

fn map_storage_error(error: ConfigurationError, operation: &'static str) -> IdentityError {
    match error {
        ConfigurationError::NotFound => IdentityError::NotConfigured,
        ConfigurationError::InvalidPath
        | ConfigurationError::TooLarge
        | ConfigurationError::Io
        | ConfigurationError::Unavailable => IdentityError::Storage { operation },
    }
}

fn map_format_error(error: FormatError) -> IdentityError {
    match error {
        FormatError::UnsupportedVersion { version } => {
            IdentityError::UnsupportedVersion { version }
        }
        FormatError::Serialization
        | FormatError::InvalidEncoding
        | FormatError::InputTooLarge
        | FormatError::TrailingBytes
        | FormatError::InvalidMagic
        | FormatError::InvalidRootReference
        | FormatError::InvalidField
        | FormatError::MissingRequiredFeature
        | FormatError::UnsupportedRequiredFeature
        | FormatError::VersionMismatch
        | FormatError::InvalidChecksum => IdentityError::Malformed,
    }
}
