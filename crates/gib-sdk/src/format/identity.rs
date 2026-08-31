use super::repository::{FormatError, decode_messagepack};
use crate::domain::{AuthorIdentity, MAX_AUTHOR_IDENTITY_LENGTH};
use serde::{Deserialize, Serialize};

/// The current version of the global identity configuration format.
pub(crate) const CURRENT_IDENTITY_CONFIGURATION_VERSION: u16 = 1;

pub(crate) const MAX_IDENTITY_CONFIGURATION_BYTES: usize = 4 * 1_024;

#[derive(Serialize)]
struct IdentityConfigurationWire<'a> {
    config_version: u16,
    author: &'a str,
}

#[derive(Deserialize)]
struct IdentityConfigurationWireOwned {
    #[serde(default)]
    config_version: Option<u16>,
    author: String,
}

pub(crate) fn encode_identity_configuration(
    identity: &AuthorIdentity,
) -> Result<Vec<u8>, FormatError> {
    rmp_serde::to_vec_named(&IdentityConfigurationWire {
        config_version: CURRENT_IDENTITY_CONFIGURATION_VERSION,
        author: identity.as_str(),
    })
    .map_err(|_| FormatError::Serialization)
}

pub(crate) fn decode_identity_configuration(bytes: &[u8]) -> Result<AuthorIdentity, FormatError> {
    let wire: IdentityConfigurationWireOwned =
        decode_messagepack(bytes, MAX_IDENTITY_CONFIGURATION_BYTES)?;
    if wire
        .config_version
        .is_some_and(|version| version != CURRENT_IDENTITY_CONFIGURATION_VERSION)
    {
        return Err(FormatError::UnsupportedVersion {
            version: wire.config_version.unwrap_or_default(),
        });
    }
    if wire.author.len() > MAX_AUTHOR_IDENTITY_LENGTH {
        return Err(FormatError::InvalidField);
    }
    AuthorIdentity::new(wire.author).map_err(|_| FormatError::InvalidField)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct LegacyIdentityConfiguration<'a> {
        author: &'a str,
    }

    #[derive(Serialize)]
    struct FutureIdentityConfiguration<'a> {
        config_version: u16,
        author: &'a str,
    }

    #[test]
    fn current_and_legacy_identity_records_decode_without_normalization() {
        let identity = AuthorIdentity::new("Jane Doe <jane@example.com>")
            .expect("test identity should be valid");
        let current = encode_identity_configuration(&identity).expect("encoding should succeed");
        assert_eq!(
            decode_identity_configuration(&current).expect("current record should decode"),
            identity
        );

        let legacy = rmp_serde::to_vec_named(&LegacyIdentityConfiguration {
            author: identity.as_str(),
        })
        .expect("legacy record should encode");
        assert_eq!(
            decode_identity_configuration(&legacy).expect("legacy record should decode"),
            identity
        );
    }

    #[test]
    fn future_identity_versions_are_rejected_explicitly() {
        let bytes = rmp_serde::to_vec_named(&FutureIdentityConfiguration {
            config_version: CURRENT_IDENTITY_CONFIGURATION_VERSION + 1,
            author: "Jane Doe <jane@example.com>",
        })
        .expect("future record should encode");
        assert_eq!(
            decode_identity_configuration(&bytes),
            Err(FormatError::UnsupportedVersion {
                version: CURRENT_IDENTITY_CONFIGURATION_VERSION + 1,
            })
        );
    }
}
